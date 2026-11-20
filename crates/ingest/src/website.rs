//! The `website` source: HTML pages found through the sitemap and a breadth-first crawl from the start page,
//! within robots.txt and the configured path prefixes.

use std::collections::{BTreeSet, VecDeque};

use url::Url;
use zenodotus_core::ids::DocKey;
use zenodotus_core::model::{DocError, VersionChanges};

use crate::error::IngestError;
use crate::fetch::{FetchError, FetchRequest, FetchResponse, HostSet};
use crate::robots::Robots;
use crate::sitemap::{SitemapEntries, parse_sitemap};
use crate::source::{Context, Redirect, Visit, Visiting};

/// The most sitemap files read for one site, indexes included.
pub const MAX_SITEMAPS: usize = 50;

/// A website source as configured.
#[derive(Debug, Clone, Copy)]
pub struct Website<'a> {
    /// The first page and the root of the crawl.
    pub start_url: &'a Url,
    /// The configured sitemap, if any.
    pub sitemap: Option<&'a Url>,
    /// URL path prefixes a page must start with; empty means the path of `start_url`.
    pub allow_prefixes: &'a [String],
    /// The most pages to fetch.
    pub max_pages: u32,
}

/// Which URLs belong to the crawl.
struct CrawlScope<'a> {
    start: &'a Url,
    prefixes: Vec<&'a str>,
    robots: Robots,
}

impl CrawlScope<'_> {
    /// Same scheme family, host and port as the start page, under a prefix or the start page itself, and allowed by
    /// robots.txt. The start page is always visited, so a start page outside the prefixes would otherwise be indexed
    /// by one sync and deleted by the next.
    fn contains(&self, url: &Url) -> bool {
        matches!(url.scheme(), "http" | "https")
            && url.host_str() == self.start.host_str()
            && url.port_or_known_default() == self.start.port_or_known_default()
            && (url == self.start || self.prefixes.iter().any(|prefix| url.path().starts_with(prefix)))
            && self.robots.allowed(url)
    }
}

/// Crawls the site from the start page, then every stored page, the start page's links and the sitemap's pages,
/// in that order, so that a page whose only link is on an unchanged (304) page is still revisited, and stored pages
/// come before new ones when `max_pages` stops the crawl. A page that redirects is indexed under the URL it
/// redirects to, in the same visit. A page is deleted only when it answers 404 or 410, when it leaves the crawl
/// scope, or when it redirects permanently (301 or 308 on every hop) to a page that left the scope or is indexed
/// already; absence from a sitemap or from the pages reached is not evidence. A temporary redirect, or one to a
/// page whose fetch failed in this crawl, keeps the stored copy.
///
/// # Errors
/// [`IngestError::ListingUnavailable`] when robots.txt, the configured sitemap or the start page cannot be fetched.
pub async fn collect_website(ctx: &Context<'_>, site: &Website<'_>) -> Result<VersionChanges, IngestError> {
    let own: Vec<&Url> = std::iter::once(site.start_url).chain(site.sitemap).collect();
    let hosts = HostSet::of(&own, ctx.extra_hosts);
    let robots = fetch_robots(ctx, site.start_url, &hosts).await?;
    if let (Some(delay), Some(host)) = (robots.crawl_delay(), site.start_url.host_str()) {
        ctx.fetcher.slow_down(host, delay);
    }
    let mut sitemap_errors = Vec::new();
    let sitemap_pages = sitemap_pages(ctx, site, &robots, &hosts, &mut sitemap_errors).await?;
    let prefixes = if site.allow_prefixes.is_empty() {
        vec![site.start_url.path()]
    } else {
        site.allow_prefixes.iter().map(String::as_str).collect()
    };
    let scope = CrawlScope {
        start: site.start_url,
        prefixes,
        robots,
    };
    let mut crawl = Crawl {
        ctx,
        hosts: &hosts,
        scope: &scope,
        changes: VersionChanges {
            errors: sitemap_errors,
            ..VersionChanges::default()
        },
        queue: VecDeque::new(),
        seen: BTreeSet::new(),
        reached: BTreeSet::new(),
        failed: BTreeSet::new(),
        visits: 0,
    };
    let start_links = crawl.start(site.start_url).await?;
    for key in ctx.previous.keys() {
        if let Ok(page) = Url::parse(key.doc_path()) {
            crawl.enqueue(page);
        }
    }
    for page in start_links.into_iter().chain(sitemap_pages) {
        crawl.enqueue(page);
    }
    crawl.run(site.max_pages).await;
    Ok(crawl.finish())
}

/// The state of one crawl.
struct Crawl<'a, 'c> {
    ctx: &'a Context<'c>,
    hosts: &'a HostSet,
    scope: &'a CrawlScope<'a>,
    changes: VersionChanges,
    queue: VecDeque<Url>,
    seen: BTreeSet<Url>,
    /// Keys visited or indexed in this crawl.
    reached: BTreeSet<DocKey>,
    /// Keys whose visit failed in this crawl.
    failed: BTreeSet<DocKey>,
    /// Fetches made, which `max_pages` caps; a redirect indexed under its target is one.
    visits: usize,
}

impl Crawl<'_, '_> {
    /// Fetches the start page and returns its links, which are crawled after the stored pages. When robots.txt
    /// disallows it, that is recorded and the crawl goes on without it.
    ///
    /// # Errors
    /// [`IngestError::ListingUnavailable`] when the start page cannot be fetched, is gone, or redirects outside the
    /// crawl scope.
    async fn start(&mut self, start: &Url) -> Result<Vec<Url>, IngestError> {
        self.seen.insert(start.clone());
        if !self.scope.robots.allowed(start) {
            let error = FetchError::RobotsDisallowed { url: start.to_string() };
            self.changes.errors.push(error.doc_error(start.as_str()));
            return Ok(Vec::new());
        }
        let visit = self.visit(start, true).await;
        match &visit {
            Some(
                Visit::Fetched { .. }
                | Visit::NotModified
                | Visit::Moved {
                    indexed_as: Some(_), ..
                },
            ) => Ok(self.links(visit)),
            Some(Visit::Moved { target, .. }) => Err(IngestError::listing(
                start,
                &format!("the start page redirects to {target}, outside the crawl scope"),
            )),
            Some(Visit::Gone) => Err(IngestError::listing(start, &"the start page answers 404 or 410")),
            _ => {
                let reason = self.changes.errors.pop().map(|error| error.message).unwrap_or_default();
                Err(IngestError::listing(start, &reason))
            }
        }
    }

    /// Queues `page` once, if it is in scope.
    fn enqueue(&mut self, page: Url) {
        if self.scope.contains(&page) && self.seen.insert(page.clone()) {
            self.queue.push_back(page);
        }
    }

    /// Visits queued pages until the queue is empty or `max_pages` pages were fetched.
    async fn run(&mut self, max_pages: u32) {
        let max_pages = usize::try_from(max_pages).unwrap_or(usize::MAX);
        while self.visits < max_pages {
            let Some(page) = self.queue.pop_front() else {
                return;
            };
            let visit = self.visit(&page, false).await;
            for link in self.links(visit) {
                self.enqueue(link);
            }
        }
    }

    /// The links a visit found. A redirect target indexed in that visit is recorded as reached, so that it is not
    /// fetched again.
    fn links(&mut self, visit: Option<Visit>) -> Vec<Url> {
        match visit {
            Some(Visit::Fetched { links, .. }) => links,
            Some(Visit::Moved {
                target,
                indexed_as: Some(key),
                links,
            }) => {
                self.reached.insert(key);
                self.seen.insert(target);
                links
            }
            _ => Vec::new(),
        }
    }

    /// Fetches one page and records it; `None` when its URL cannot be a key or it was already indexed as the
    /// target of a redirect.
    async fn visit(&mut self, page: &Url, report_missing: bool) -> Option<Visit> {
        let doc_key = self.ctx.doc_key(page, &mut self.changes)?;
        if !self.reached.insert(doc_key.clone()) {
            return None;
        }
        self.visits += 1;
        let (scope, reached, failed, library) = (self.scope, &self.reached, &self.failed, self.ctx.scope);
        let decide = |target: &Url| {
            if !scope.contains(target) {
                return Redirect::Drop;
            }
            match DocKey::new(library, target.as_str()) {
                Ok(key) if failed.contains(&key) => Redirect::Keep,
                Ok(key) if reached.contains(&key) => Redirect::Drop,
                _ => Redirect::Follow,
            }
        };
        let visiting = Visiting {
            url: page,
            doc_key: &doc_key,
            hosts: self.hosts,
            max_bytes: self.ctx.settings.page_max_bytes,
            conditional: true,
            report_missing,
            moves: Some(&decide),
        };
        let visit = self.ctx.visit(&visiting, &mut self.changes).await;
        if matches!(visit, Visit::Failed) {
            self.failed.insert(doc_key);
        }
        Some(visit)
    }

    /// Adds the deletions of stored pages that left the scope.
    fn finish(mut self) -> VersionChanges {
        let deleted: BTreeSet<DocKey> = self.changes.deletions.iter().cloned().collect();
        for key in self.ctx.previous.keys() {
            let in_scope = Url::parse(key.doc_path()).is_ok_and(|page| self.scope.contains(&page));
            if !in_scope && !deleted.contains(key) {
                self.changes.deletions.push(key.clone());
            }
        }
        self.changes
    }
}

/// The site's robots.txt. A 4xx answer, 404 included, allows everything; any other failure stops the crawl.
async fn fetch_robots(ctx: &Context<'_>, start: &Url, hosts: &HostSet) -> Result<Robots, IngestError> {
    let robots_url = at_root(start, "/robots.txt");
    let request = FetchRequest {
        url: &robots_url,
        allowed_hosts: hosts,
        max_bytes: ctx.settings.page_max_bytes,
        validators: None,
        timeout: ctx.timeout(),
        headers: &[],
    };
    match ctx.fetcher.get(&request).await {
        Ok(FetchResponse::Body(body)) => Ok(Robots::parse(ctx.user_agent, &body.bytes)),
        Ok(_) | Err(FetchError::Status { status: 400..=499, .. }) => Ok(Robots::allow_all()),
        Err(error) => Err(IngestError::listing(&robots_url, &error)),
    }
}

/// The URL of `path` on the site of `start`.
fn at_root(start: &Url, path: &str) -> Url {
    let mut url = start.clone();
    url.set_path(path);
    url.set_query(None);
    url.set_fragment(None);
    url
}

/// Where the sitemaps being read come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SitemapOrigin {
    /// The source's `sitemap`: failing to read it fails the version.
    Configured,
    /// robots.txt's `Sitemap:` lines: a failure is reported, and the next sitemap is read.
    Listed,
    /// The guess `/sitemap.xml`: a failure is ignored, and the pages read so far are kept.
    Guessed,
}

/// The page URLs of the site's sitemaps, following indexes: the configured sitemap, else those robots.txt lists,
/// else `/sitemap.xml`. Only a configured sitemap that cannot be read fails; any other failure skips that sitemap.
/// Failures of listed sitemaps, and reading stopped after [`MAX_SITEMAPS`] files, are added to `errors`.
async fn sitemap_pages(
    ctx: &Context<'_>,
    site: &Website<'_>,
    robots: &Robots,
    hosts: &HostSet,
    errors: &mut Vec<DocError>,
) -> Result<Vec<Url>, IngestError> {
    let listed = robots.sitemaps(site.start_url);
    let (mut queue, origin) = match site.sitemap {
        Some(sitemap) => (VecDeque::from([sitemap.clone()]), SitemapOrigin::Configured),
        None if !listed.is_empty() => (listed.into(), SitemapOrigin::Listed),
        None => (
            VecDeque::from([at_root(site.start_url, "/sitemap.xml")]),
            SitemapOrigin::Guessed,
        ),
    };
    let mut pages = Vec::new();
    let mut read = 0;
    while let Some(sitemap) = queue.pop_front() {
        if read == MAX_SITEMAPS {
            let reason = format!("more than {MAX_SITEMAPS} sitemaps; later sitemaps were not read");
            errors.push(sitemap_error(&sitemap, &reason));
            return Ok(pages);
        }
        read += 1;
        let entries = match read_sitemap(ctx, &sitemap, hosts).await {
            Ok(entries) => entries,
            Err(reason) => {
                match origin {
                    SitemapOrigin::Configured => return Err(IngestError::listing(&sitemap, &reason)),
                    SitemapOrigin::Listed => errors.push(sitemap_error(&sitemap, &reason)),
                    SitemapOrigin::Guessed => tracing::debug!(%sitemap, reason, "no usable sitemap"),
                }
                continue;
            }
        };
        pages.extend(entries.pages.iter().filter_map(|page| sitemap.join(page).ok()));
        queue.extend(entries.sitemaps.iter().filter_map(|nested| sitemap.join(nested).ok()));
    }
    Ok(pages)
}

/// Fetches and parses one sitemap; the reason it cannot be read otherwise.
async fn read_sitemap(ctx: &Context<'_>, sitemap: &Url, hosts: &HostSet) -> Result<SitemapEntries, String> {
    let request = FetchRequest {
        url: sitemap,
        allowed_hosts: hosts,
        max_bytes: ctx.settings.document_max_bytes,
        validators: None,
        timeout: ctx.timeout(),
        headers: &[],
    };
    match ctx.fetcher.get(&request).await {
        Ok(FetchResponse::Body(body)) => parse_sitemap(&body.bytes, ctx.settings.document_max_bytes),
        Ok(_) => Err("not found (HTTP 404 or 410)".to_owned()),
        Err(error) => Err(error.to_string()),
    }
}

/// The error recorded for a sitemap that could not be used.
fn sitemap_error(sitemap: &Url, reason: &str) -> DocError {
    DocError {
        doc_path: sitemap.to_string(),
        kind: "sitemap".to_owned(),
        message: format!("{sitemap}: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::fetch::tests::url;
    use crate::source::tests::{Harness, errors, paths};

    async fn serve(server: &MockServer, route: &str, status: u16, mime: &str, body: &str) {
        Mock::given(path(route))
            .respond_with(ResponseTemplate::new(status).set_body_raw(body.to_owned(), mime))
            .mount(server)
            .await;
    }

    async fn html(server: &MockServer, route: &str, body: &str) {
        let page = format!("<html><head><title>{route}</title></head><body><main>{body}</main></body></html>");
        serve(server, route, 200, "text/html", &page).await;
    }

    fn sitemap(locs: &[String]) -> String {
        let urls: Vec<String> = locs.iter().map(|loc| format!("<url><loc>{loc}</loc></url>")).collect();
        format!("<urlset>{}</urlset>", urls.concat())
    }

    fn site<'a>(start_url: &'a Url, prefixes: &'a [String]) -> Website<'a> {
        Website {
            start_url,
            sitemap: None,
            allow_prefixes: prefixes,
            max_pages: 100,
        }
    }

    fn upserted(changes: &VersionChanges) -> Vec<&str> {
        changes.upserts.iter().map(|doc| doc.state.doc_key.doc_path()).collect()
    }

    #[tokio::test]
    async fn a_crawl_follows_the_sitemap_index_and_links_and_deletes_gone_or_out_of_scope_pages() {
        let server = MockServer::start().await;
        let uri = server.uri();
        let at = |p: &str| format!("{uri}{p}");
        serve(
            &server,
            "/robots.txt",
            200,
            "text/plain",
            &format!("User-agent: *\nDisallow: /docs/private\n\nSitemap: {uri}/index.xml\n"),
        )
        .await;
        let index = format!("<sitemapindex><sitemap><loc>{uri}/pages.xml</loc></sitemap></sitemapindex>");
        serve(&server, "/index.xml", 200, "application/xml", &index).await;
        let pages = sitemap(&[at("/docs/a"), at("/docs/gone"), at("/blog/x"), at("/docs/private/p")]);
        serve(&server, "/pages.xml", 200, "application/xml", &pages).await;
        html(
            &server,
            "/docs/",
            r#"<a href="/docs/b">b</a> <a href="https://other.example/docs/">x</a>"#,
        )
        .await;
        html(
            &server,
            "/docs/a",
            r#"<a href="/docs/">home</a> <a href="/docs/private/q">secret</a>"#,
        )
        .await;
        html(&server, "/docs/b", "B").await;
        serve(&server, "/docs/gone", 404, "text/html", "").await;
        serve(&server, "/docs/unreached", 410, "text/html", "").await;
        let start = url(&at("/docs/"));
        let mut harness = Harness::new();
        harness.remember(&at("/docs/gone"), "old", None);
        harness.remember(&at("/docs/unreached"), "old", None);
        harness.remember(&at("/blog/old"), "old", None);
        harness.remember("not a url", "old", None);
        let changes = collect_website(&harness.context(), &site(&start, &[])).await.unwrap();
        assert_eq!(upserted(&changes), [at("/docs/"), at("/docs/b"), at("/docs/a")]);
        assert_eq!(
            paths(&changes.deletions),
            [
                at("/docs/gone").as_str(),
                &at("/docs/unreached"),
                &at("/blog/old"),
                "not a url"
            ]
        );
        assert!(changes.errors.is_empty(), "{:?}", errors(&changes));
    }

    #[tokio::test]
    async fn a_stored_page_linked_only_from_an_unchanged_page_survives_a_complete_sitemap() {
        let server = MockServer::start().await;
        let uri = server.uri();
        let at = |p: &str| format!("{uri}{p}");
        serve(&server, "/robots.txt", 404, "text/plain", "").await;
        serve(
            &server,
            "/sitemap.xml",
            200,
            "application/xml",
            &sitemap(&[at("/docs/")]),
        )
        .await;
        serve(&server, "/docs/", 304, "text/html", "").await;
        serve(&server, "/docs/linked", 304, "text/html", "").await;
        let start = url(&at("/docs/"));
        let mut harness = Harness::new();
        harness.remember(&at("/docs/"), "home", Some("\"v1\""));
        harness.remember(&at("/docs/linked"), "linked", Some("\"v1\""));
        let changes = collect_website(&harness.context(), &site(&start, &[])).await.unwrap();
        assert_eq!(paths(&changes.deletions), [] as [&str; 0]);
        assert_eq!(paths(&changes.unchanged), [at("/docs/").as_str(), &at("/docs/linked")]);
    }

    #[tokio::test]
    async fn a_page_that_redirects_is_indexed_under_its_target_and_its_old_key_deleted() {
        let server = MockServer::start().await;
        let uri = server.uri();
        let at = |p: &str| format!("{uri}{p}");
        serve(&server, "/robots.txt", 404, "text/plain", "").await;
        serve(&server, "/sitemap.xml", 404, "text/plain", "").await;
        let long = format!("/docs/{}", "x".repeat(1100));
        let links = r#"<a href="/docs/short">s</a> <a href="/docs/new">n</a> <a href="/docs/again">a</a>
            <a href="/docs/long">l</a>"#;
        html(&server, "/docs/", links).await;
        for (from, to) in [
            ("/docs/old", "/docs/new"),
            ("/docs/short", "/docs/new"),
            ("/docs/again", "/docs/new"),
            ("/docs/long", long.as_str()),
        ] {
            Mock::given(path(from))
                .respond_with(ResponseTemplate::new(301).insert_header("location", to))
                .mount(&server)
                .await;
        }
        html(&server, "/docs/new", r#"New <a href="/docs/">home</a>"#).await;
        html(&server, &long, "Long").await;
        let start = url(&at("/docs/"));
        let mut harness = Harness::new();
        harness.remember(&at("/docs/old"), "old", None);
        harness.remember(&at("/docs/long"), "old", None);
        let changes = collect_website(&harness.context(), &site(&start, &[])).await.unwrap();
        assert_eq!(upserted(&changes), [at("/docs/"), at("/docs/new")]);
        assert_eq!(paths(&changes.deletions), [at("/docs/old").as_str()]);
        assert_eq!(errors(&changes).len(), 1, "{:?}", errors(&changes));
        assert!(
            errors(&changes)[0].ends_with("(invalid_path)"),
            "{:?}",
            errors(&changes)
        );
    }

    #[tokio::test]
    async fn a_temporary_redirect_indexes_its_target_and_deletes_nothing() {
        let server = MockServer::start().await;
        let uri = server.uri();
        let at = |p: &str| format!("{uri}{p}");
        serve(&server, "/robots.txt", 404, "text/plain", "").await;
        serve(&server, "/sitemap.xml", 404, "text/plain", "").await;
        html(&server, "/docs/", "Home").await;
        for (from, status, to) in [
            ("/docs/old", 302, "/docs/new"),
            ("/docs/away", 307, "/blog/x"),
            ("/docs/down", 302, "/docs/maintenance"),
        ] {
            Mock::given(path(from))
                .respond_with(ResponseTemplate::new(status).insert_header("location", to))
                .mount(&server)
                .await;
        }
        html(&server, "/docs/new", "New").await;
        html(&server, "/blog/x", "Blog").await;
        let start = url(&at("/docs/"));
        let mut harness = Harness::new();
        harness.remember(&at("/docs/old"), "old", None);
        harness.remember(&at("/docs/away"), "away", None);
        harness.remember(&at("/docs/down"), "down", None);
        let changes = collect_website(&harness.context(), &site(&start, &[])).await.unwrap();
        assert_eq!(upserted(&changes), [at("/docs/"), at("/docs/new")]);
        assert_eq!(paths(&changes.deletions), [] as [&str; 0]);
        assert_eq!(errors(&changes), [format!("{} (status)", at("/docs/down"))]);
    }

    #[tokio::test]
    async fn a_redirect_to_a_host_the_source_may_not_reach_leaves_the_scope_without_an_error() {
        let server = MockServer::start().await;
        let uri = server.uri();
        let at = |p: &str| format!("{uri}{p}");
        serve(&server, "/robots.txt", 404, "text/plain", "").await;
        serve(&server, "/sitemap.xml", 404, "text/plain", "").await;
        html(&server, "/docs/", "Home").await;
        for (from, status) in [("/docs/moved", 301), ("/docs/down", 302)] {
            Mock::given(path(from))
                .respond_with(ResponseTemplate::new(status).insert_header("location", "https://elsewhere.example/x"))
                .mount(&server)
                .await;
        }
        let start = url(&at("/docs/"));
        let mut harness = Harness::new();
        harness.remember(&at("/docs/moved"), "moved", None);
        harness.remember(&at("/docs/down"), "down", None);
        let changes = collect_website(&harness.context(), &site(&start, &[])).await.unwrap();
        assert_eq!(paths(&changes.deletions), [at("/docs/moved")]);
        assert_eq!(errors(&changes), [] as [&str; 0]);
    }

    #[tokio::test]
    async fn a_stored_page_whose_redirect_target_fails_is_kept() {
        let server = MockServer::start().await;
        let uri = server.uri();
        let at = |p: &str| format!("{uri}{p}");
        serve(&server, "/robots.txt", 404, "text/plain", "").await;
        serve(&server, "/sitemap.xml", 404, "text/plain", "").await;
        html(&server, "/docs/", "Home").await;
        Mock::given(path("/docs/old"))
            .respond_with(ResponseTemplate::new(301).insert_header("location", "/docs/new"))
            .mount(&server)
            .await;
        serve(&server, "/docs/new", 500, "text/html", "").await;
        let start = url(&at("/docs/"));
        let mut harness = Harness::new();
        harness.remember(&at("/docs/old"), "old", None);
        let changes = collect_website(&harness.context(), &site(&start, &[])).await.unwrap();
        assert_eq!(paths(&changes.deletions), [] as [&str; 0]);
        assert_eq!(errors(&changes), [format!("{} (status)", at("/docs/old"))]);
    }

    #[tokio::test]
    async fn a_stored_page_redirecting_to_a_target_that_failed_in_this_crawl_is_kept() {
        let server = MockServer::start().await;
        let uri = server.uri();
        let at = |p: &str| format!("{uri}{p}");
        serve(&server, "/robots.txt", 404, "text/plain", "").await;
        serve(&server, "/sitemap.xml", 404, "text/plain", "").await;
        html(&server, "/docs/", "Home").await;
        Mock::given(path("/docs/new"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        html(&server, "/docs/new", "New").await;
        Mock::given(path("/docs/z-old"))
            .respond_with(ResponseTemplate::new(301).insert_header("location", "/docs/new"))
            .mount(&server)
            .await;
        let start = url(&at("/docs/"));
        let mut harness = Harness::new();
        harness.remember(&at("/docs/new"), "new", None);
        harness.remember(&at("/docs/z-old"), "old", None);
        let changes = collect_website(&harness.context(), &site(&start, &[])).await.unwrap();
        assert_eq!(paths(&changes.deletions), [] as [&str; 0]);
        assert_eq!(errors(&changes), [format!("{} (status)", at("/docs/new"))]);
    }

    #[tokio::test]
    async fn stored_pages_are_visited_before_the_links_of_the_start_page() {
        let server = MockServer::start().await;
        let uri = server.uri();
        let at = |p: &str| format!("{uri}{p}");
        serve(&server, "/robots.txt", 404, "text/plain", "").await;
        serve(&server, "/sitemap.xml", 404, "text/plain", "").await;
        html(&server, "/", r#"<a href="/new">n</a>"#).await;
        html(&server, "/new", "New").await;
        html(&server, "/stored", "Stored").await;
        let start = url(&at("/"));
        let mut harness = Harness::new();
        harness.remember(&at("/stored"), "old", None);
        let website = Website {
            max_pages: 2,
            ..site(&start, &[])
        };
        let changes = collect_website(&harness.context(), &website).await.unwrap();
        assert_eq!(upserted(&changes), [at("/"), at("/stored")]);
    }

    #[tokio::test]
    async fn a_redirect_target_is_indexed_in_the_visit_that_found_it_even_at_max_pages() {
        let server = MockServer::start().await;
        let uri = server.uri();
        let at = |p: &str| format!("{uri}{p}");
        serve(&server, "/robots.txt", 404, "text/plain", "").await;
        serve(&server, "/sitemap.xml", 404, "text/plain", "").await;
        html(&server, "/docs/", "Home").await;
        Mock::given(path("/docs/old"))
            .respond_with(ResponseTemplate::new(301).insert_header("location", "/docs/new#top"))
            .mount(&server)
            .await;
        html(&server, "/docs/new", "New").await;
        let start = url(&at("/docs/"));
        let mut harness = Harness::new();
        harness.remember(&at("/docs/old"), "old", None);
        harness.remember(&at("/docs/z"), "old", None);
        html(&server, "/docs/z", "Z").await;
        let website = Website {
            max_pages: 2,
            ..site(&start, &[])
        };
        let changes = collect_website(&harness.context(), &website).await.unwrap();
        assert_eq!(upserted(&changes), [at("/docs/"), at("/docs/new")]);
        assert_eq!(paths(&changes.deletions), [at("/docs/old").as_str()]);
        let sources: Vec<&str> = changes.upserts[1]
            .sections
            .iter()
            .map(|section| section.source_ref.as_str())
            .collect();
        assert!(
            sources
                .iter()
                .all(|source| source.starts_with(&at("/docs/new")) && !source.contains("#top")),
            "{sources:?}"
        );
        let website = Website {
            max_pages: 3,
            ..site(&start, &[])
        };
        let changes = collect_website(&harness.context(), &website).await.unwrap();
        assert_eq!(
            upserted(&changes),
            [at("/docs/"), at("/docs/new"), at("/docs/z")],
            "a redirect is one visit of the max_pages budget"
        );
    }

    #[tokio::test]
    async fn a_start_page_that_redirects_is_crawled_from_its_target() {
        let server = MockServer::start().await;
        let uri = server.uri();
        let at = |p: &str| format!("{uri}{p}");
        serve(&server, "/robots.txt", 404, "text/plain", "").await;
        serve(&server, "/sitemap.xml", 404, "text/plain", "").await;
        Mock::given(path("/docs"))
            .respond_with(ResponseTemplate::new(301).insert_header("location", "/docs/"))
            .mount(&server)
            .await;
        html(&server, "/docs/", r#"Home <a href="/docs/next">next</a>"#).await;
        html(&server, "/docs/next", "Next").await;
        let start = url(&at("/docs"));
        let changes = collect_website(&Harness::new().context(), &site(&start, &[]))
            .await
            .unwrap();
        assert_eq!(upserted(&changes), [at("/docs/"), at("/docs/next")]);

        Mock::given(path("/latest/"))
            .respond_with(ResponseTemplate::new(301).insert_header("location", "/en/latest/"))
            .mount(&server)
            .await;
        html(&server, "/en/latest/", "Elsewhere").await;
        let start = url(&at("/latest/"));
        let mut harness = Harness::new();
        harness.remember(start.as_str(), "old", None);
        let error = collect_website(&harness.context(), &site(&start, &[]))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "cannot list {start}: the start page redirects to {}, outside the crawl scope",
                at("/en/latest/")
            )
        );
    }

    #[tokio::test]
    async fn without_a_sitemap_stored_pages_are_revisited_and_never_deleted_for_being_unreached() {
        let server = MockServer::start().await;
        let uri = server.uri();
        let at = |p: &str| format!("{uri}{p}");
        serve(&server, "/robots.txt", 404, "text/plain", "").await;
        serve(&server, "/sitemap.xml", 404, "text/plain", "").await;
        html(&server, "/", r#"<a href="/guide">g</a>"#).await;
        html(&server, "/guide", "G").await;
        serve(&server, "/flaky", 500, "text/html", "").await;
        let start = url(&at("/"));
        let mut harness = Harness::new();
        harness.remember(&at("/flaky"), "old", None);
        harness.remember("not a url", "old", None);
        let changes = collect_website(&harness.context(), &site(&start, &[])).await.unwrap();
        assert_eq!(upserted(&changes), [at("/"), at("/guide")]);
        assert_eq!(paths(&changes.deletions), ["not a url"]);
        assert_eq!(errors(&changes), [format!("{} (status)", at("/flaky"))]);
    }

    #[tokio::test]
    async fn an_unchanged_start_page_is_confirmed() {
        let server = MockServer::start().await;
        let uri = server.uri();
        serve(&server, "/robots.txt", 404, "text/plain", "").await;
        serve(&server, "/sitemap.xml", 404, "text/plain", "").await;
        serve(&server, "/", 304, "text/html", "").await;
        let start = url(&format!("{uri}/"));
        let mut harness = Harness::new();
        harness.remember(start.as_str(), "old", Some("\"v1\""));
        let changes = collect_website(&harness.context(), &site(&start, &[])).await.unwrap();
        assert_eq!(paths(&changes.unchanged), [start.as_str()]);
        assert_eq!(upserted(&changes), [] as [&str; 0]);
    }

    #[tokio::test]
    async fn max_pages_stops_the_crawl_after_the_stored_pages() {
        let server = MockServer::start().await;
        let uri = server.uri();
        let at = |p: &str| format!("{uri}{p}");
        serve(&server, "/robots.txt", 403, "text/plain", "").await;
        serve(
            &server,
            "/map.xml",
            200,
            "application/xml",
            &sitemap(&[at("/a"), at("/b"), at("/c")]),
        )
        .await;
        html(&server, "/", "home").await;
        html(&server, "/a", "A").await;
        html(&server, "/c", "C").await;
        let start = url(&at("/"));
        let configured = url(&at("/map.xml"));
        let mut harness = Harness::new();
        harness.remember(&at("/c"), "old", None);
        let website = Website {
            sitemap: Some(&configured),
            max_pages: 2,
            ..site(&start, &[])
        };
        let changes = collect_website(&harness.context(), &website).await.unwrap();
        assert_eq!(upserted(&changes), [at("/"), at("/c")]);
        assert_eq!(paths(&changes.deletions), [] as [&str; 0]);
    }

    #[tokio::test]
    async fn a_crawl_that_ends_exactly_at_max_pages_reaches_every_page() {
        let server = MockServer::start().await;
        let uri = server.uri();
        let at = |p: &str| format!("{uri}{p}");
        serve(&server, "/robots.txt", 404, "text/plain", "").await;
        serve(&server, "/sitemap.xml", 200, "application/xml", &sitemap(&[at("/a")])).await;
        html(&server, "/", "home").await;
        html(&server, "/a", "A").await;
        let start = url(&at("/"));
        let harness = Harness::new();
        let website = Website {
            max_pages: 2,
            ..site(&start, &[])
        };
        let changes = collect_website(&harness.context(), &website).await.unwrap();
        assert_eq!(upserted(&changes), [at("/"), at("/a")]);
    }

    #[tokio::test]
    async fn a_start_page_outside_the_prefixes_stays_in_the_scope() {
        let server = MockServer::start().await;
        let uri = server.uri();
        serve(&server, "/robots.txt", 404, "text/plain", "").await;
        serve(&server, "/sitemap.xml", 404, "text/plain", "").await;
        serve(&server, "/", 304, "text/html", "").await;
        let start = url(&format!("{uri}/"));
        let prefixes = ["/docs/".to_owned()];
        let mut harness = Harness::new();
        harness.remember(start.as_str(), "old", Some("\"v1\""));
        let changes = collect_website(&harness.context(), &site(&start, &prefixes))
            .await
            .unwrap();
        assert_eq!(paths(&changes.unchanged), [start.as_str()]);
        assert_eq!(paths(&changes.deletions), [] as [&str; 0]);
    }

    #[tokio::test]
    async fn prefixes_robots_and_the_crawl_delay_shape_the_crawl() {
        let server = MockServer::start().await;
        let uri = server.uri();
        let at = |p: &str| format!("{uri}{p}");
        serve(
            &server,
            "/robots.txt",
            200,
            "text/plain",
            "User-agent: zenodotus\nDisallow: /\nAllow: /api/\nAllow: /guide/\nCrawl-delay: 0.2\n",
        )
        .await;
        serve(&server, "/sitemap.xml", 404, "text/plain", "").await;
        html(
            &server,
            "/api/",
            r#"<a href="/guide/x">x</a> <a href="/other/">o</a> <a href="/api/y">y</a>"#,
        )
        .await;
        html(&server, "/guide/x", "X").await;
        html(&server, "/api/y", "Y").await;
        let start = url(&at("/api/"));
        let prefixes = ["/api/".to_owned(), "/guide/".to_owned()];
        let harness = Harness::new();
        let began = Instant::now();
        let changes = collect_website(&harness.context(), &site(&start, &prefixes))
            .await
            .unwrap();
        assert!(began.elapsed().as_millis() >= 400, "{:?}", began.elapsed());
        assert_eq!(upserted(&changes), [at("/api/"), at("/guide/x"), at("/api/y")]);
    }

    #[tokio::test]
    async fn a_start_page_disallowed_by_robots_is_reported_and_the_sitemap_still_crawled() {
        let server = MockServer::start().await;
        let uri = server.uri();
        let at = |p: &str| format!("{uri}{p}");
        serve(
            &server,
            "/robots.txt",
            200,
            "text/plain",
            "User-agent: *\nDisallow: /docs/$\n",
        )
        .await;
        serve(
            &server,
            "/sitemap.xml",
            200,
            "application/xml",
            &sitemap(&[at("/docs/a")]),
        )
        .await;
        html(&server, "/docs/a", "A").await;
        let start = url(&at("/docs/"));
        let harness = Harness::new();
        let changes = collect_website(&harness.context(), &site(&start, &[])).await.unwrap();
        assert_eq!(upserted(&changes), [at("/docs/a")]);
        assert_eq!(errors(&changes), [format!("{} (robots_disallowed)", at("/docs/"))]);
        assert_eq!(
            changes.errors[0].message,
            format!("{}: disallowed by robots.txt", at("/docs/"))
        );
    }

    async fn failure(routes: &[(&str, u16, &str)], configured_sitemap: Option<&str>) -> String {
        let server = MockServer::start().await;
        for (route, status, body) in routes {
            serve(&server, route, *status, "application/xml", body).await;
        }
        let start = url(&format!("{}/", server.uri()));
        let configured = configured_sitemap.map(|p| url(&format!("{}{p}", server.uri())));
        let website = Website {
            sitemap: configured.as_ref(),
            ..site(&start, &[])
        };
        let error = collect_website(&Harness::new().context(), &website).await.unwrap_err();
        assert!(matches!(error, IngestError::ListingUnavailable { .. }), "{error:?}");
        error.to_string().replace(&server.uri(), "")
    }

    #[tokio::test]
    async fn listings_that_cannot_be_read_fail_the_version() {
        let ok_robots = ("/robots.txt", 404, "");
        assert_eq!(
            failure(&[("/robots.txt", 503, "")], None).await,
            "cannot list /robots.txt: HTTP status 503"
        );
        assert_eq!(
            failure(&[ok_robots], Some("/map.xml")).await,
            "cannot list /map.xml: not found (HTTP 404 or 410)"
        );
        assert_eq!(
            failure(&[ok_robots, ("/map.xml", 500, "")], Some("/map.xml")).await,
            "cannot list /map.xml: HTTP status 500"
        );
        assert_eq!(
            failure(&[ok_robots, ("/map.xml", 200, "<html/>")], Some("/map.xml")).await,
            "cannot list /map.xml: not a sitemap: no <urlset> or <sitemapindex> element"
        );
        let empty = "<urlset></urlset>";
        assert_eq!(
            failure(&[ok_robots, ("/sitemap.xml", 200, empty), ("/", 500, "")], None).await,
            "cannot list /: /: HTTP status 500"
        );
        assert_eq!(
            failure(&[ok_robots, ("/sitemap.xml", 200, empty), ("/", 404, "")], None).await,
            "cannot list /: the start page answers 404 or 410"
        );
    }

    /// The `message (kind)` of every error.
    fn messages(changes: &VersionChanges) -> Vec<String> {
        changes
            .errors
            .iter()
            .map(|error| format!("{} ({})", error.message, error.kind))
            .collect()
    }

    /// Crawls a site whose start page links to `/a`, with `/stored` remembered from an earlier sync.
    async fn crawl_with(routes: &[(&str, u16, &str)], configured_sitemap: Option<&str>) -> (VersionChanges, String) {
        let server = MockServer::start().await;
        let uri = server.uri();
        for (route, status, body) in routes {
            serve(&server, route, *status, "application/xml", &body.replace("{uri}", &uri)).await;
        }
        html(&server, "/", r#"<a href="/a">a</a>"#).await;
        html(&server, "/a", "A").await;
        html(&server, "/stored", "S").await;
        let start = url(&format!("{uri}/"));
        let configured = configured_sitemap.map(|p| url(&format!("{uri}{p}")));
        let mut harness = Harness::new();
        harness.remember(&format!("{uri}/stored"), "old", None);
        let website = Website {
            sitemap: configured.as_ref(),
            ..site(&start, &[])
        };
        let changes = collect_website(&harness.context(), &website).await.unwrap();
        (changes, uri)
    }

    #[tokio::test]
    async fn sitemaps_that_are_not_configured_fail_without_failing_the_crawl() {
        type Case<'a> = (&'a [(&'a str, u16, &'a str)], Option<&'a str>, Option<&'a str>);
        let looping = "<sitemapindex><sitemap><loc>/index.xml</loc></sitemap></sitemapindex>";
        let cases: [Case<'_>; 4] = [
            (
                &[("/robots.txt", 404, ""), ("/sitemap.xml", 200, "<html/>")],
                None,
                None,
            ),
            (&[("/robots.txt", 404, ""), ("/sitemap.xml", 503, "")], None, None),
            (
                &[
                    ("/robots.txt", 200, "Sitemap: {uri}/listed.xml"),
                    ("/listed.xml", 500, ""),
                ],
                None,
                Some("/listed.xml: HTTP status 500 (sitemap)"),
            ),
            (
                &[("/robots.txt", 404, ""), ("/index.xml", 200, looping)],
                Some("/index.xml"),
                Some("/index.xml: more than 50 sitemaps; later sitemaps were not read (sitemap)"),
            ),
        ];
        for (routes, configured, error) in cases {
            let (changes, uri) = crawl_with(routes, configured).await;
            let pages = ["/", "/stored", "/a"].map(|page| format!("{uri}{page}"));
            assert_eq!(upserted(&changes), pages, "{routes:?}");
            assert_eq!(paths(&changes.deletions), [] as [&str; 0], "{routes:?}");
            let expected: Vec<String> = error.iter().map(|e| format!("{uri}{e}")).collect();
            assert_eq!(messages(&changes), expected, "{routes:?}");
        }
    }

    #[tokio::test]
    async fn a_listed_sitemap_on_another_host_is_reported_and_the_next_one_read() {
        let robots = "Sitemap: https://cdn.example/sitemap.xml\nSitemap: {uri}/pages.xml";
        let pages = "<urlset><url><loc>{uri}/listed</loc></url></urlset>";
        let routes = [
            ("/robots.txt", 200, robots),
            ("/pages.xml", 200, pages),
            ("/listed", 200, "# Listed"),
        ];
        let (changes, uri) = crawl_with(&routes, None).await;
        assert_eq!(
            messages(&changes),
            ["https://cdn.example/sitemap.xml: the host is not one this source may reach (sitemap)"]
        );
        assert!(
            upserted(&changes).contains(&format!("{uri}/listed").as_str()),
            "{:?}",
            upserted(&changes)
        );
    }

    #[test]
    fn the_scope_needs_the_same_host_and_port() {
        let start = url("https://docs.example/docs/");
        let scope = CrawlScope {
            start: &start,
            prefixes: vec!["/docs/"],
            robots: Robots::allow_all(),
        };
        assert!(scope.contains(&url("https://docs.example/docs/a")));
        assert!(scope.contains(&url("https://docs.example:443/docs/a")));
        assert!(!scope.contains(&url("https://docs.example:8443/docs/a")));
        assert!(!scope.contains(&url("https://other.example/docs/a")));
        assert!(!scope.contains(&url("https://docs.example/blog/")));
        assert!(!scope.contains(&url("ftp://docs.example/docs/a")));
    }
}
