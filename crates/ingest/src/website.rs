//! The `website` source: HTML pages found through the sitemap and a breadth-first crawl from the start page,
//! within robots.txt and the configured path prefixes.

use std::collections::{BTreeSet, VecDeque};

use url::Url;
use zenodotus_core::ids::DocKey;
use zenodotus_core::model::VersionChanges;

use crate::error::IngestError;
use crate::fetch::{FetchError, FetchRequest, FetchResponse, HostSet};
use crate::robots::Robots;
use crate::sitemap::parse_sitemap;
use crate::source::{Context, Visit, Visiting};

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
    /// Same scheme family, host and port as the start page, under a prefix, and allowed by robots.txt.
    fn contains(&self, url: &Url) -> bool {
        matches!(url.scheme(), "http" | "https")
            && url.host_str() == self.start.host_str()
            && url.port_or_known_default() == self.start.port_or_known_default()
            && self.prefixes.iter().any(|prefix| url.path().starts_with(prefix))
            && self.robots.allowed(url)
    }
}

/// Crawls the site. A page is deleted when it answers 404 or 410, when it leaves the crawl scope, or, after a
/// complete crawl, when it was not reached. The crawl is complete when a sitemap was read in full and `max_pages`
/// was not reached; only then are unreached pages deleted.
///
/// # Errors
/// [`IngestError::ListingUnavailable`] when robots.txt, the sitemap or the start page cannot be fetched.
pub async fn collect_website(ctx: &Context<'_>, site: &Website<'_>) -> Result<VersionChanges, IngestError> {
    let own: Vec<&Url> = std::iter::once(site.start_url).chain(site.sitemap).collect();
    let hosts = HostSet::of(&own, ctx.extra_hosts);
    let robots = fetch_robots(ctx, site.start_url, &hosts).await?;
    if let (Some(delay), Some(host)) = (robots.crawl_delay(), site.start_url.host_str()) {
        ctx.fetcher.slow_down(host, delay);
    }
    let sitemap_pages = sitemap_pages(ctx, site, &robots, &hosts).await?;
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
        changes: VersionChanges::default(),
        queue: VecDeque::new(),
        seen: BTreeSet::new(),
        reached: BTreeSet::new(),
    };
    crawl.start(site.start_url).await?;
    let complete_listing = sitemap_pages.is_some();
    for page in sitemap_pages.iter().flatten() {
        crawl.enqueue(page.clone());
    }
    if !complete_listing {
        for key in ctx.previous.keys() {
            if let Ok(page) = Url::parse(key.doc_path()) {
                crawl.enqueue(page);
            }
        }
    }
    let truncated = crawl.run(site.max_pages).await;
    Ok(crawl.finish(complete_listing && !truncated))
}

/// The state of one crawl.
struct Crawl<'a, 'c> {
    ctx: &'a Context<'c>,
    hosts: &'a HostSet,
    scope: &'a CrawlScope<'a>,
    changes: VersionChanges,
    queue: VecDeque<Url>,
    seen: BTreeSet<Url>,
    reached: BTreeSet<DocKey>,
}

impl Crawl<'_, '_> {
    /// Fetches the start page. When robots.txt disallows it, that is recorded and the crawl goes on from the sitemap.
    async fn start(&mut self, start: &Url) -> Result<(), IngestError> {
        self.seen.insert(start.clone());
        if !self.scope.robots.allowed(start) {
            let error = FetchError::RobotsDisallowed { url: start.to_string() };
            self.changes.errors.push(error.doc_error(start.as_str()));
            return Ok(());
        }
        match self.visit(start, true).await {
            Some(Visit::Fetched { links, .. }) => {
                for link in links {
                    self.enqueue(link);
                }
                Ok(())
            }
            Some(Visit::NotModified) => Ok(()),
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

    /// Visits queued pages until the queue is empty or `max_pages` pages were fetched; returns whether pages were
    /// left over.
    async fn run(&mut self, max_pages: u32) -> bool {
        let max_pages = usize::try_from(max_pages).unwrap_or(usize::MAX);
        loop {
            if self.reached.len() >= max_pages {
                return !self.queue.is_empty();
            }
            let Some(page) = self.queue.pop_front() else {
                return false;
            };
            if let Some(Visit::Fetched { links, .. }) = self.visit(&page, false).await {
                for link in links {
                    self.enqueue(link);
                }
            }
        }
    }

    /// Fetches one page and records it; `None` when its URL cannot be a key.
    async fn visit(&mut self, page: &Url, report_missing: bool) -> Option<Visit> {
        let doc_key = self.ctx.doc_key(page, &mut self.changes)?;
        self.reached.insert(doc_key.clone());
        let visiting = Visiting {
            url: page,
            doc_key: &doc_key,
            hosts: self.hosts,
            max_bytes: self.ctx.settings.page_max_bytes,
            conditional: true,
            report_missing,
        };
        Some(self.ctx.visit(&visiting, &mut self.changes).await)
    }

    /// Adds the deletions of stored pages that left the scope, or that a complete crawl did not reach.
    fn finish(mut self, listing_complete: bool) -> VersionChanges {
        let deleted: BTreeSet<DocKey> = self.changes.deletions.iter().cloned().collect();
        for key in self.ctx.previous.keys() {
            let in_scope = Url::parse(key.doc_path()).is_ok_and(|page| self.scope.contains(&page));
            let unreached = listing_complete && !self.reached.contains(key);
            if (!in_scope || unreached) && !deleted.contains(key) {
                self.changes.deletions.push(key.clone());
            }
        }
        self.changes.listing_complete = listing_complete;
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

/// Every page URL of the site's sitemaps, following indexes: the configured sitemap, else those robots.txt lists,
/// else `/sitemap.xml`. `None` when there is no configured or listed sitemap and `/sitemap.xml` does not exist.
async fn sitemap_pages(
    ctx: &Context<'_>,
    site: &Website<'_>,
    robots: &Robots,
    hosts: &HostSet,
) -> Result<Option<Vec<Url>>, IngestError> {
    let listed = robots.sitemaps(site.start_url);
    let (mut queue, optional): (VecDeque<Url>, bool) = match site.sitemap {
        Some(sitemap) => (VecDeque::from([sitemap.clone()]), false),
        None if !listed.is_empty() => (listed.into(), false),
        None => (VecDeque::from([at_root(site.start_url, "/sitemap.xml")]), true),
    };
    let mut pages = Vec::new();
    let mut read = 0;
    while let Some(sitemap) = queue.pop_front() {
        if read == MAX_SITEMAPS {
            return Err(IngestError::listing(
                &sitemap,
                &format!("more than {MAX_SITEMAPS} sitemaps"),
            ));
        }
        read += 1;
        let request = FetchRequest {
            url: &sitemap,
            allowed_hosts: hosts,
            max_bytes: ctx.settings.document_max_bytes,
            validators: None,
            timeout: ctx.timeout(),
            headers: &[],
        };
        let body = match ctx.fetcher.get(&request).await {
            Ok(FetchResponse::Body(body)) => body,
            Ok(_) if optional => return Ok(None),
            Ok(_) => return Err(IngestError::listing(&sitemap, &"not found (HTTP 404 or 410)")),
            Err(error) => return Err(IngestError::listing(&sitemap, &error)),
        };
        let entries = parse_sitemap(&body.bytes, ctx.settings.document_max_bytes)
            .map_err(|reason| IngestError::listing(&sitemap, &reason))?;
        pages.extend(entries.pages.iter().filter_map(|page| sitemap.join(page).ok()));
        queue.extend(entries.sitemaps.iter().filter_map(|nested| sitemap.join(nested).ok()));
    }
    Ok(Some(pages))
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
    async fn a_complete_crawl_follows_the_sitemap_index_and_links_and_deletes_unreached_pages() {
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
        let start = url(&at("/docs/"));
        let mut harness = Harness::new();
        harness.remember(&at("/docs/gone"), "old", None);
        harness.remember(&at("/docs/unreached"), "old", None);
        harness.remember(&at("/blog/old"), "old", None);
        harness.remember("not a url", "old", None);
        let changes = collect_website(&harness.context(), &site(&start, &[])).await.unwrap();
        assert!(changes.listing_complete);
        assert_eq!(upserted(&changes), [at("/docs/"), at("/docs/b"), at("/docs/a")]);
        assert_eq!(
            paths(&changes.deletions),
            [
                at("/docs/gone").as_str(),
                &at("/blog/old"),
                &at("/docs/unreached"),
                "not a url"
            ]
        );
        assert!(changes.errors.is_empty(), "{:?}", errors(&changes));
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
        assert!(!changes.listing_complete);
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
    async fn max_pages_stops_the_crawl_and_leaves_the_listing_incomplete() {
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
        assert!(!changes.listing_complete);
        assert_eq!(upserted(&changes), [at("/"), at("/a")]);
        assert_eq!(paths(&changes.deletions), [] as [&str; 0]);
    }

    #[tokio::test]
    async fn a_crawl_that_ends_exactly_at_max_pages_is_complete() {
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
        assert!(changes.listing_complete);
        assert_eq!(upserted(&changes), [at("/"), at("/a")]);
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
            failure(&[ok_robots, ("/sitemap.xml", 200, "<html/>")], None).await,
            "cannot list /sitemap.xml: not a sitemap: no <urlset> or <sitemapindex> element"
        );
        let looping = "<sitemapindex><sitemap><loc>/sitemap.xml</loc></sitemap></sitemapindex>";
        assert_eq!(
            failure(&[ok_robots, ("/sitemap.xml", 200, looping)], None).await,
            format!("cannot list /sitemap.xml: more than {MAX_SITEMAPS} sitemaps")
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
