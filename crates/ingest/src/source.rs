//! One entry point for every source kind, and the fetch-and-record step the remote sources share.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use url::Url;
use zenodotus_core::config::FetchSettings;
use zenodotus_core::ids::{DocKey, Scope};
use zenodotus_core::model::{DocError, DocState, SourceSpec, VersionChanges};
use zenodotus_core::time::Clock;

use crate::document::{self, Fetched};
use crate::error::IngestError;
use crate::fetch::{FetchError, FetchRequest, FetchResponse, Fetcher, HostSet, Validators};
use crate::github::{self, GitHub};
use crate::html::html_to_markdown;
use crate::llms_txt::collect_llms_txt;
use crate::local::{Limits, LocalSource, collect_local};
use crate::sectioner::{SectionRules, SourceRef};
use crate::url::collect_urls;
use crate::website::{Website, collect_website};

/// Everything a collection run needs besides the source itself.
#[derive(Clone, Copy)]
pub struct Context<'a> {
    /// The version being collected.
    pub scope: &'a Scope,
    /// The stored state of its documents.
    pub previous: &'a BTreeMap<DocKey, DocState>,
    /// The commit the last successful sync of a GitHub source resolved to.
    pub previous_commit: Option<&'a str>,
    /// The time source for `fetched_at`.
    pub clock: &'a dyn Clock,
    /// The HTTP client.
    pub fetcher: &'a Fetcher,
    /// Size limits and timeouts.
    pub settings: &'a FetchSettings,
    /// The configured user agent, matched against robots.txt.
    pub user_agent: &'a str,
    /// Sectioning rules.
    pub rules: SectionRules,
    /// Hosts the version may reach besides those of its own URLs.
    pub extra_hosts: &'a [String],
    /// The directory relative local paths are taken from.
    pub base_dir: &'a Path,
    /// The GitHub endpoints and token.
    pub github: &'a GitHub,
}

/// Lists, fetches and sections the documents of one version.
///
/// # Errors
/// [`IngestError`] when the listing cannot be obtained; the version's sections must then stay as they are.
pub async fn collect(spec: &SourceSpec, ctx: &Context<'_>) -> Result<VersionChanges, IngestError> {
    match spec {
        SourceSpec::Local { path, include, exclude } => {
            let root = ctx.base_dir.join(path);
            let source = LocalSource {
                root: &root,
                include,
                exclude,
            };
            let limits = Limits {
                max_bytes: ctx.settings.page_max_bytes,
                rules: ctx.rules,
            };
            collect_local(ctx.scope, &source, ctx.previous, ctx.clock, &limits)
        }
        SourceSpec::GitHub { repo, reference, paths } => {
            github::collect_github(ctx, repo, reference.as_deref(), paths).await
        }
        SourceSpec::Url { urls } => Ok(collect_urls(ctx, urls).await),
        SourceSpec::LlmsTxt { url } => collect_llms_txt(ctx, url).await,
        SourceSpec::Website {
            start_url,
            sitemap,
            allow_prefixes,
            max_pages,
        } => {
            let website = Website {
                start_url,
                sitemap: sitemap.as_ref(),
                allow_prefixes,
                max_pages: *max_pages,
            };
            collect_website(ctx, &website).await
        }
    }
}

/// What fetching one document found, once recorded in the changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Visit {
    /// The body was downloaded, as Markdown, with the links of an HTML page.
    Fetched {
        /// The Markdown that was indexed.
        markdown: String,
        /// The page's links; empty for Markdown bodies.
        links: Vec<Url>,
    },
    /// The server confirmed the stored copy.
    NotModified,
    /// The server answered 404 or 410.
    Gone,
    /// The URL redirects to `target`, given without its fragment. Nothing is indexed under the requested URL; when
    /// `indexed_as` is set, the body was indexed under that key, the target's, and `links` are its links.
    Moved {
        /// Where the URL redirects.
        target: Url,
        /// The key the body was indexed under, if it was.
        indexed_as: Option<DocKey>,
        /// The links of the indexed body; empty otherwise.
        links: Vec<Url>,
    },
    /// The fetch failed; the error is recorded.
    Failed,
}

/// What a crawl does with a page that redirects to another URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Redirect {
    /// Index the body under the target, and delete a stored copy under the requested URL.
    Follow,
    /// Index nothing and delete a stored copy under the requested URL: the target left the scope or is indexed
    /// already.
    Drop,
    /// Index nothing and keep a stored copy under the requested URL: the target failed earlier in this crawl.
    Keep,
}

/// How one document is fetched.
#[derive(Clone, Copy)]
pub(crate) struct Visiting<'a> {
    /// The document URL, which is also its document path.
    pub url: &'a Url,
    /// Its key.
    pub doc_key: &'a DocKey,
    /// The hosts it may reach.
    pub hosts: &'a HostSet,
    /// Its size limit.
    pub max_bytes: u64,
    /// Whether to send the stored validators, so that an unchanged document answers 304.
    pub conditional: bool,
    /// Whether a 404 or 410 for a document never stored is an error rather than nothing.
    pub report_missing: bool,
    /// For crawled pages, which are keyed by the URL that serves them: a redirect to another URL means the document
    /// moved, and this decides what happens to it. `None` for configured URLs.
    pub moves: Option<&'a (dyn Fn(&Url) -> Redirect + Sync)>,
}

impl Context<'_> {
    /// The key of the document at `url`, or `None` after recording why the URL cannot be one.
    pub(crate) fn doc_key(&self, url: &Url, changes: &mut VersionChanges) -> Option<DocKey> {
        DocKey::new(self.scope, url.as_str())
            .map_err(|error| {
                changes.errors.push(DocError {
                    doc_path: truncated(url.as_str()),
                    kind: "invalid_path".to_owned(),
                    message: error.to_string(),
                });
            })
            .ok()
    }

    /// The request timeout of ordinary documents.
    pub(crate) const fn timeout(&self) -> Duration {
        Duration::from_secs(self.settings.request_timeout_secs)
    }

    /// Fetches one document and records the result in `changes`: an upsert or an unchanged document for a body, an
    /// unchanged document for a 304, a deletion for a 404 or 410 of a stored document, and an error otherwise.
    pub(crate) async fn visit(&self, visiting: &Visiting<'_>, changes: &mut VersionChanges) -> Visit {
        let key = visiting.doc_key;
        let previous = self.previous.get(key);
        let validators = previous.filter(|_| visiting.conditional).map(Validators::of);
        let request = FetchRequest {
            url: visiting.url,
            allowed_hosts: visiting.hosts,
            max_bytes: visiting.max_bytes,
            validators: validators.as_ref(),
            timeout: self.timeout(),
            headers: &[],
        };
        let doc_path = key.doc_path();
        let failed = |changes: &mut VersionChanges, error: &FetchError| {
            changes.errors.push(error.doc_error(doc_path));
            Visit::Failed
        };
        let body = match self.fetcher.get(&request).await {
            Ok(FetchResponse::Body(body)) => body,
            Ok(FetchResponse::NotModified) if previous.is_some() => {
                changes.unchanged.push(key.clone());
                return Visit::NotModified;
            }
            Ok(FetchResponse::NotModified) => {
                let error = FetchError::Status {
                    url: visiting.url.to_string(),
                    status: 304,
                };
                return failed(changes, &error);
            }
            Ok(FetchResponse::Gone) => {
                if previous.is_some() {
                    changes.deletions.push(key.clone());
                } else if visiting.report_missing {
                    changes.errors.push(not_found(doc_path));
                }
                return Visit::Gone;
            }
            Err(FetchError::RedirectedAway { url: target, permanent }) if visiting.moves.is_some() => {
                return stopped_at(target, permanent && previous.is_some(), key, changes);
            }
            Err(error) => return failed(changes, &error),
        };
        let mut source_url = body.final_url.clone();
        source_url.set_fragment(None);
        let mut moved = None;
        if let Some(decide) = visiting.moves {
            let target = source_url.clone();
            if target != *visiting.url {
                let redirect = decide(&target);
                if redirect != Redirect::Follow {
                    let delete = redirect == Redirect::Drop && body.permanent && previous.is_some();
                    return stopped_at(target, delete, key, changes);
                }
                let Some(target_key) = self.doc_key(&target, changes) else {
                    return Visit::Failed;
                };
                moved = Some((target, target_key));
            }
        }
        let text = match body.text() {
            Ok(text) => text,
            Err(error) => return failed(changes, &error),
        };
        let (markdown, links) = if body.is_html() {
            let page = html_to_markdown(text, &body.final_url);
            (page.markdown, page.links)
        } else {
            (text.to_owned(), Vec::new())
        };
        let (indexed_key, indexed_previous) = match &moved {
            Some((_, target_key)) => (target_key, self.previous.get(target_key)),
            None => (key, previous),
        };
        let fetched = Fetched {
            doc_key: indexed_key,
            source: SourceRef::Url(source_url.as_str()),
            text: &markdown,
            etag: body.validators.etag.as_deref(),
            last_modified: body.validators.last_modified.as_deref(),
            fetched_at: self.clock.now(),
        };
        match document::build(self.scope, &fetched, indexed_previous, &self.rules) {
            Some(document) => changes.upserts.push(document),
            None => changes.unchanged.push(indexed_key.clone()),
        }
        if let Some((target, target_key)) = moved {
            if body.permanent && previous.is_some() {
                changes.deletions.push(key.clone());
            }
            return Visit::Moved {
                target,
                indexed_as: Some(target_key),
                links,
            };
        }
        Visit::Fetched { markdown, links }
    }
}

/// The visit of a page whose redirect to `target` is not followed: nothing is indexed, and the stored copy under
/// `key` is deleted when `delete` holds.
fn stopped_at(target: Url, delete: bool, key: &DocKey, changes: &mut VersionChanges) -> Visit {
    if delete {
        changes.deletions.push(key.clone());
    }
    Visit::Moved {
        target,
        indexed_as: None,
        links: Vec::new(),
    }
}

/// The error of a document that was asked for by name and does not exist.
fn not_found(doc_path: &str) -> DocError {
    DocError {
        doc_path: doc_path.to_owned(),
        kind: "not_found".to_owned(),
        message: format!("{doc_path}: not found (HTTP 404 or 410)"),
    }
}

/// `text` cut to 200 characters, for error messages about URLs too long to be keys.
fn truncated(text: &str) -> String {
    text.chars().take(200).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zenodotus_core::ids::{LibraryId, Version};
    use zenodotus_core::model::{DocState, Outcome, content_hash};
    use zenodotus_core::time::{FixedClock, Timestamp};

    use super::*;
    use crate::fetch::tests::{local_settings, url};
    use crate::guard::SystemResolve;

    /// Owns everything a [`Context`] borrows.
    pub(crate) struct Harness {
        pub scope: Scope,
        pub previous: BTreeMap<DocKey, DocState>,
        pub previous_commit: Option<String>,
        pub clock: FixedClock,
        pub fetcher: Fetcher,
        pub settings: FetchSettings,
        pub extra_hosts: Vec<String>,
        pub base_dir: PathBuf,
        pub github: GitHub,
    }

    impl Harness {
        pub(crate) fn new() -> Self {
            Self::with_settings(local_settings())
        }

        pub(crate) fn with_settings(settings: FetchSettings) -> Self {
            Self {
                scope: Scope::new(LibraryId::parse("lib").unwrap(), Version::parse("1").unwrap()),
                previous: BTreeMap::new(),
                previous_commit: None,
                clock: FixedClock::new(Timestamp::from_unix(100)),
                fetcher: Fetcher::new(&settings, "zenodotus/0.1", Arc::new(SystemResolve)).unwrap(),
                settings,
                extra_hosts: Vec::new(),
                base_dir: PathBuf::from("/nonexistent"),
                github: GitHub::default(),
            }
        }

        pub(crate) fn context(&self) -> Context<'_> {
            Context {
                scope: &self.scope,
                previous: &self.previous,
                previous_commit: self.previous_commit.as_deref(),
                clock: &self.clock,
                fetcher: &self.fetcher,
                settings: &self.settings,
                user_agent: "zenodotus/0.1",
                rules: SectionRules::default(),
                extra_hosts: &self.extra_hosts,
                base_dir: &self.base_dir,
                github: &self.github,
            }
        }

        pub(crate) fn key(&self, doc_path: &str) -> DocKey {
            DocKey::new(&self.scope, doc_path).unwrap()
        }

        /// Stores a previous state for `doc_path` with `text` as its content and `etag` as its validator.
        pub(crate) fn remember(&mut self, doc_path: &str, text: &str, etag: Option<&str>) {
            let doc_key = self.key(doc_path);
            self.previous.insert(
                doc_key.clone(),
                DocState {
                    doc_key,
                    etag: etag.map(str::to_owned),
                    last_modified: None,
                    content_hash: content_hash(text),
                    fetched_at: Timestamp::from_unix(1),
                },
            );
        }
    }

    /// The doc paths of `keys`.
    pub(crate) fn paths(keys: &[DocKey]) -> Vec<&str> {
        keys.iter().map(DocKey::doc_path).collect()
    }

    /// The `doc_path (kind)` of every error.
    pub(crate) fn errors(changes: &VersionChanges) -> Vec<String> {
        changes
            .errors
            .iter()
            .map(|error| format!("{} ({})", error.doc_path, error.kind))
            .collect()
    }

    fn visiting<'a>(target: &'a Url, key: &'a DocKey, hosts: &'a HostSet) -> Visiting<'a> {
        Visiting {
            url: target,
            doc_key: key,
            hosts,
            max_bytes: 1000,
            conditional: true,
            report_missing: true,
            moves: None,
        }
    }

    #[tokio::test]
    async fn a_visit_records_bodies_304s_gone_documents_and_failures() {
        let server = MockServer::start().await;
        let html = "<html><head><title>T</title></head><body><p>Hi <a href=\"/x#y\">x</a></p></body></html>";
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(html, "text/html"))
            .mount(&server)
            .await;
        Mock::given(path("/same"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("# Same\n", "text/markdown"))
            .mount(&server)
            .await;
        Mock::given(path("/cached"))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        Mock::given(path("/gone"))
            .respond_with(ResponseTemplate::new(410))
            .mount(&server)
            .await;
        Mock::given(path("/broken"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(path("/binary"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(vec![0xff, 0xfe], "text/plain"))
            .mount(&server)
            .await;
        let base = url(&server.uri());
        let mut harness = Harness::new();
        let at = |p: &str| base.join(p).unwrap();
        harness.remember(at("/same").as_str(), "# Same\n", None);
        harness.remember(at("/cached").as_str(), "old", Some("\"v1\""));
        harness.remember(at("/gone").as_str(), "old", None);
        let ctx = harness.context();
        let hosts = HostSet::of(&[&base], &[]);
        let mut changes = VersionChanges::default();
        let mut run = async |p: &str, report_missing: bool| {
            let target = at(p);
            let key = ctx.doc_key(&target, &mut changes).unwrap();
            let visiting = Visiting {
                report_missing,
                ..visiting(&target, &key, &hosts)
            };
            ctx.visit(&visiting, &mut changes).await
        };
        assert_eq!(
            run("/page", true).await,
            Visit::Fetched {
                markdown: format!("---\ntitle: T\n---\n\nHi [x]({}#y)\n", at("/x")),
                links: vec![at("/x")],
            }
        );
        assert!(matches!(run("/same", true).await, Visit::Fetched { links, .. } if links.is_empty()));
        assert_eq!(run("/cached", true).await, Visit::NotModified);
        assert_eq!(run("/gone", true).await, Visit::Gone);
        assert_eq!(run("/never", true).await, Visit::Gone);
        assert_eq!(run("/never-quiet", false).await, Visit::Gone);
        assert_eq!(run("/broken", true).await, Visit::Failed);
        assert_eq!(run("/binary", true).await, Visit::Failed);
        drop(run);
        assert_eq!(changes.upserts.len(), 1);
        assert_eq!(changes.upserts[0].state.doc_key.doc_path(), at("/page").as_str());
        assert_eq!(changes.upserts[0].sections[0].source_ref, at("/page").as_str());
        assert_eq!(
            paths(&changes.unchanged),
            [at("/same").as_str(), at("/cached").as_str()]
        );
        assert_eq!(paths(&changes.deletions), [at("/gone").as_str()]);
        assert_eq!(
            errors(&changes),
            [
                format!("{} (not_found)", at("/never")),
                format!("{} (status)", at("/broken")),
                format!("{} (decode)", at("/binary")),
            ]
        );
        assert_eq!(
            changes.errors[0].message,
            format!("{}: not found (HTTP 404 or 410)", at("/never"))
        );
        assert_eq!(changes.outcome(), Outcome::Partial);
    }

    #[tokio::test]
    async fn a_304_for_a_document_never_stored_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(path("/odd"))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        let harness = Harness::new();
        let ctx = harness.context();
        let target = url(&format!("{}/odd", server.uri()));
        let hosts = HostSet::of(&[&target], &[]);
        let mut changes = VersionChanges::default();
        let key = ctx.doc_key(&target, &mut changes).unwrap();
        assert_eq!(
            ctx.visit(&visiting(&target, &key, &hosts), &mut changes).await,
            Visit::Failed
        );
        assert_eq!(changes.errors[0].message, format!("{target}: HTTP status 304"));
    }

    #[test]
    fn urls_too_long_to_be_keys_are_reported() {
        let harness = Harness::new();
        let long = url(&format!("https://docs.example/{}", "a".repeat(1100)));
        let mut changes = VersionChanges::default();
        assert_eq!(harness.context().doc_key(&long, &mut changes), None);
        assert_eq!(changes.errors[0].kind, "invalid_path");
        assert_eq!(changes.errors[0].doc_path.chars().count(), 200);
    }

    #[tokio::test]
    async fn every_remote_source_kind_is_dispatched() {
        let server = MockServer::start().await;
        let at = |p: &str| url(&format!("{}{p}", server.uri()));
        let mut harness = Harness::new();
        harness.github = GitHub {
            api: at("/api/"),
            codeload: at("/codeload/"),
            token: None,
        };
        let ctx = harness.context();
        let urls = SourceSpec::Url {
            urls: vec![at("/a.md")],
        };
        assert_eq!(
            errors(&collect(&urls, &ctx).await.unwrap()),
            [format!("{} (not_found)", at("/a.md"))]
        );
        let failures = [
            SourceSpec::LlmsTxt { url: at("/llms.txt") },
            SourceSpec::Website {
                start_url: at("/"),
                sitemap: Some(at("/map.xml")),
                allow_prefixes: Vec::new(),
                max_pages: 1,
            },
            SourceSpec::GitHub {
                repo: "o/n".to_owned(),
                reference: None,
                paths: Vec::new(),
            },
        ];
        for (spec, what) in failures
            .iter()
            .zip(["/llms.txt", "/map.xml", "/api/repos/o/n/commits/HEAD"])
        {
            let error = collect(spec, &ctx).await.unwrap_err();
            assert!(
                error.to_string().starts_with(&format!("cannot list {}: ", at(what))),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn local_sources_are_collected_from_the_base_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("docs")).unwrap();
        std::fs::write(dir.path().join("docs/a.md"), "# A\n").unwrap();
        let mut harness = Harness::new();
        harness.base_dir = dir.path().to_owned();
        let spec = SourceSpec::Local {
            path: PathBuf::from("docs"),
            include: Vec::new(),
            exclude: Vec::new(),
        };
        let changes = collect(&spec, &harness.context()).await.unwrap();
        assert_eq!(changes.upserts[0].state.doc_key.doc_path(), "a.md");
        harness.base_dir = dir.path().join("missing");
        assert!(matches!(
            collect(&spec, &harness.context()).await,
            Err(IngestError::RootNotFound { .. })
        ));
    }
}
