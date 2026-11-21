//! The domain model shared by ingestion, storage and serving: sources, sections, documents and sync state.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::ids::{ChunkId, DocKey, Scope, to_hex};
use crate::plan::Plan;
use crate::time::Timestamp;

/// Default page cap for a [`SourceSpec::Website`] crawl.
pub const DEFAULT_MAX_PAGES: u32 = 2000;

/// Where the documents of one library version come from.
///
/// Each source may reach only the hosts of its own URLs, plus the version's `extra_hosts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceSpec {
    /// Markdown files under a local directory.
    Local {
        /// The source root.
        path: PathBuf,
        /// Glob patterns of files to keep, relative to the root; empty keeps every Markdown file.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        include: Vec<String>,
        /// Glob patterns of files to drop, relative to the root.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude: Vec<String>,
    },
    /// Markdown files from a GitHub repository tarball.
    #[serde(rename = "github")]
    GitHub {
        /// `owner/name`.
        repo: String,
        /// A branch, tag or commit; the default branch when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
        /// Directories inside the repository to index; empty means the whole repository.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        paths: Vec<String>,
    },
    /// Markdown documents at fixed URLs, such as `llms-full.txt`.
    Url {
        /// The documents.
        urls: Vec<Url>,
    },
    /// An `llms.txt` file and the Markdown pages it links to on the same host.
    LlmsTxt {
        /// The `llms.txt` file.
        url: Url,
    },
    /// HTML pages of a documentation website, discovered by sitemap and links.
    Website {
        /// The first page and the root of the crawl.
        start_url: Url,
        /// The sitemap; found through robots.txt or `/sitemap.xml` when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sitemap: Option<Url>,
        /// URL path prefixes a page must lie under, on whole path segments; empty means the start page's directory,
        /// which is the whole host for a start page one segment deep.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        allow_prefixes: Vec<String>,
        /// The most pages to fetch.
        #[serde(default = "default_max_pages")]
        max_pages: u32,
    },
}

/// Serde default for [`SourceSpec::Website::max_pages`].
const fn default_max_pages() -> u32 {
    DEFAULT_MAX_PAGES
}

impl SourceSpec {
    /// Checks the rules a source must meet before it can be synced.
    ///
    /// # Errors
    /// A short reason naming the first broken rule.
    pub fn check(&self) -> Result<(), &'static str> {
        match self {
            Self::Local { path, .. } => {
                if path.as_os_str().is_empty() {
                    return Err("`path` is empty");
                }
            }
            Self::GitHub { repo, reference, paths } => {
                if !is_github_repo(repo) {
                    return Err("`repo` must be `owner/name`");
                }
                if reference
                    .as_deref()
                    .is_some_and(|r| r.is_empty() || r.contains(char::is_whitespace))
                {
                    return Err("`reference` is empty or contains whitespace");
                }
                if paths.iter().any(|p| !is_relative_inner_path(p)) {
                    return Err("`paths` must be relative and must not contain `..`");
                }
            }
            Self::Url { urls } => {
                if urls.is_empty() {
                    return Err("`urls` is empty");
                }
                check_web_urls(urls.iter())?;
            }
            Self::LlmsTxt { url } => check_web_urls([url].into_iter())?,
            Self::Website {
                start_url,
                sitemap,
                allow_prefixes,
                max_pages,
            } => {
                check_web_urls([start_url].into_iter().chain(sitemap))?;
                if allow_prefixes.iter().any(|p| !p.starts_with('/')) {
                    return Err("`allow_prefixes` entries must start with `/`");
                }
                if *max_pages == 0 {
                    return Err("`max_pages` must be at least 1");
                }
            }
        }
        Ok(())
    }

    /// A one-line description such as `github:tokio-rs/website@71cd…`, shown by `list`, `status` and
    /// `resolve_library`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Local { path, .. } => format!("local:{}", path.display()),
            Self::GitHub { repo, reference, paths } => {
                let at = reference.as_ref().map(|r| format!("@{r}")).unwrap_or_default();
                let within = if paths.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", paths.join(", "))
                };
                format!("github:{repo}{at}{within}")
            }
            Self::Url { urls } => match urls.as_slice() {
                [only] => format!("url:{only}"),
                [first, rest @ ..] => format!("url:{first} (+{} more)", rest.len()),
                [] => "url:".to_owned(),
            },
            Self::LlmsTxt { url } => format!("llms_txt:{url}"),
            Self::Website { start_url, .. } => format!("website:{start_url}"),
        }
    }
}

/// Whether `repo` looks like `owner/name`, each part made of ASCII letters, digits, `.`, `_` or `-`.
fn is_github_repo(repo: &str) -> bool {
    let valid = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    repo.split_once('/')
        .is_some_and(|(owner, name)| valid(owner) && valid(name))
}

/// Whether `path` is relative and stays inside its root.
fn is_relative_inner_path(path: &str) -> bool {
    !path.starts_with('/') && !path.contains('\\') && path.split('/').all(|part| part != "..")
}

/// Every URL must use `http` or `https` and have no fragment: a fragment names no other resource, and a configured
/// URL is compared with the URL that answered it.
fn check_web_urls<'a>(urls: impl Iterator<Item = &'a Url>) -> Result<(), &'static str> {
    for url in urls {
        if !matches!(url.scheme(), "http" | "https") {
            return Err("URLs must use http or https");
        }
        if url.fragment().is_some() {
            return Err("URLs must not have a fragment");
        }
    }
    Ok(())
}

/// The lowercase hex SHA-256 of `text`, used to detect changed documents and duplicate sections.
#[must_use]
pub fn content_hash(text: &str) -> String {
    to_hex(&Sha256::digest(text.as_bytes()))
}

/// One retrievable unit: a heading and the text under it, up to the next heading of the same or higher level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    /// Stable id, derived from the scope, document path and ordinal.
    pub chunk_id: ChunkId,
    /// The document this section belongs to.
    pub doc_key: DocKey,
    /// Position in the document, from 0.
    pub ordinal: u32,
    /// The document title.
    pub title: String,
    /// The headings from the document root down to this section.
    pub heading_path: Vec<String>,
    /// Markdown, code fences included.
    pub text: String,
    /// Languages of the code fences in `text`, in order of first appearance.
    pub code_languages: Vec<String>,
    /// Identifiers found in headings and code.
    pub symbols: Vec<String>,
    /// The canonical URL plus `#anchor` when known, or `local:<path relative to the source root>`.
    pub source_ref: String,
    /// [`content_hash`] of `text`.
    pub content_hash: String,
}

/// What is remembered about one document between syncs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocState {
    /// The document.
    pub doc_key: DocKey,
    /// The `ETag` the server sent with the body.
    pub etag: Option<String>,
    /// The `Last-Modified` header the server sent with the body.
    pub last_modified: Option<String>,
    /// [`content_hash`] of the raw document.
    pub content_hash: String,
    /// When the body was last downloaded or read.
    pub fetched_at: Timestamp,
    /// The `id` of the plan source that produced the document (design §12.4); `None` for a configured library's
    /// single source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// A new or changed document with its sections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    /// Change-detection state to store.
    pub state: DocState,
    /// The sections, in document order.
    pub sections: Vec<Section>,
}

/// A document that could not be fetched or parsed. Its previous content, if any, is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocError {
    /// The document path or URL.
    pub doc_path: String,
    /// The error kind, in `snake_case`, such as `too_large` or `status`.
    pub kind: String,
    /// A human-readable explanation.
    pub message: String,
}

impl fmt::Display for DocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} ({})", self.doc_path, self.message, self.kind)
    }
}

/// Everything one ingestion run found for one version, ready to be applied in one commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionChanges {
    /// New or changed documents.
    pub upserts: Vec<Document>,
    /// Documents confirmed current, left untouched.
    pub unchanged: Vec<DocKey>,
    /// Documents proven gone.
    pub deletions: Vec<DocKey>,
    /// Documents that failed and keep their previous content.
    pub errors: Vec<DocError>,
    /// The commit a GitHub reference resolved to.
    pub resolved_commit: Option<String>,
}

impl VersionChanges {
    /// `ok` when every document succeeded, `partial` otherwise.
    #[must_use]
    pub fn outcome(&self) -> Outcome {
        if self.errors.is_empty() {
            Outcome::Ok
        } else {
            Outcome::Partial
        }
    }

    /// Whether applying these changes alters any section.
    #[must_use]
    pub fn changes_sections(&self) -> bool {
        !self.upserts.is_empty() || !self.deletions.is_empty()
    }
}

/// How the last sync of a version ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Every document succeeded.
    Ok,
    /// Some documents failed and kept their previous content.
    Partial,
    /// No section changed, because the listing could not be obtained or the commit failed.
    Failed,
}

impl Outcome {
    /// The outcome as written in reports: `ok`, `partial` or `failed`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Partial => "partial",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The record of one synced version: a snapshot of its library metadata plus its sync status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionState {
    /// The library and version.
    pub scope: Scope,
    /// Display name of the library.
    pub name: String,
    /// One-line description of the library.
    pub description: String,
    /// Other names the library is known by.
    pub aliases: Vec<String>,
    /// The library home page.
    pub homepage: Option<String>,
    /// The documentation license, as written in the config.
    pub license: Option<String>,
    /// [`SourceSpec::describe`] of the version's source.
    pub source: String,
    /// Whether the config marks this version as the default.
    pub default: bool,
    /// Position of the version in the config, used when no version is the default.
    pub list_order: u32,
    /// Goes up by one with every commit that changes a section.
    pub revision: u64,
    /// When a sync last ended `ok` or `partial`.
    pub last_success: Option<Timestamp>,
    /// When a sync was last attempted.
    pub last_attempt: Timestamp,
    /// When every document was last confirmed current.
    pub last_verified: Option<Timestamp>,
    /// How the last sync ended.
    pub outcome: Outcome,
    /// The error of the last sync, when it did not end `ok`.
    pub last_error: Option<String>,
    /// Indexed documents.
    pub document_count: u64,
    /// Indexed sections.
    pub section_count: u64,
    /// The commit a GitHub reference resolved to at the last successful sync.
    pub resolved_commit: Option<String>,
    /// Where the plan came from; `config` for every schema-1 record.
    #[serde(default)]
    pub origin: Origin,
    /// The sources and revisions; `None` for a configured library.
    #[serde(default)]
    pub plan: Option<Plan>,
    /// The stored text length of the scope's sections, the eviction measure (design §14); 0 until measured.
    #[serde(default)]
    pub bytes: u64,
}

/// Where a scope's plan came from (design §12.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// A library of the config file.
    Config,
    /// A recipe, or a built-in runtime plan.
    Recipe,
    /// Discovery from registry metadata.
    Discovered,
}

impl Default for Origin {
    /// Every schema-1 record comes from a configured library.
    fn default() -> Self {
        Self::Config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{LibraryId, Version};

    #[derive(Debug, Deserialize, Serialize, PartialEq)]
    struct Wrapper {
        source: SourceSpec,
    }

    fn parse(source: &str) -> Result<SourceSpec, toml::de::Error> {
        toml::from_str::<Wrapper>(&format!("source = {source}")).map(|w| w.source)
    }

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn parses_every_source_kind_with_defaults() {
        assert_eq!(
            parse(r#"{ kind = "local", path = "docs" }"#).unwrap(),
            SourceSpec::Local {
                path: "docs".into(),
                include: vec![],
                exclude: vec![]
            }
        );
        assert_eq!(
            parse(r#"{ kind = "github", repo = "tokio-rs/website" }"#).unwrap(),
            SourceSpec::GitHub {
                repo: "tokio-rs/website".into(),
                reference: None,
                paths: vec![]
            }
        );
        assert_eq!(
            parse(r#"{ kind = "url", urls = ["https://x.dev/llms-full.txt"] }"#).unwrap(),
            SourceSpec::Url {
                urls: vec![url("https://x.dev/llms-full.txt")]
            }
        );
        assert_eq!(
            parse(r#"{ kind = "llms_txt", url = "https://x.dev/llms.txt" }"#).unwrap(),
            SourceSpec::LlmsTxt {
                url: url("https://x.dev/llms.txt")
            }
        );
        assert_eq!(
            parse(r#"{ kind = "website", start_url = "https://x.dev/docs/" }"#).unwrap(),
            SourceSpec::Website {
                start_url: url("https://x.dev/docs/"),
                sitemap: None,
                allow_prefixes: vec![],
                max_pages: DEFAULT_MAX_PAGES
            }
        );
        assert_eq!(DEFAULT_MAX_PAGES, 2000, "the design's default");
    }

    #[test]
    fn rejects_unknown_kinds_and_fields() {
        assert!(parse(r#"{ kind = "ftp", path = "x" }"#).is_err());
        assert!(parse(r#"{ kind = "local", path = "x", recurse = true }"#).is_err());
    }

    #[test]
    fn serializes_back_to_the_same_source() {
        let source = SourceSpec::Website {
            start_url: url("https://x.dev/docs/"),
            sitemap: Some(url("https://x.dev/sitemap.xml")),
            allow_prefixes: vec!["/docs/".into()],
            max_pages: 5,
        };
        let text = toml::to_string(&Wrapper { source: source.clone() }).unwrap();
        assert_eq!(toml::from_str::<Wrapper>(&text).unwrap().source, source);
    }

    #[test]
    fn valid_sources_pass_the_check() {
        for source in [
            r#"{ kind = "local", path = "docs" }"#,
            r#"{ kind = "github", repo = "vercel/next.js", reference = "v15.1.0", paths = ["docs", "a/b"] }"#,
            r#"{ kind = "url", urls = ["http://x.dev/a.md", "https://x.dev/b.md"] }"#,
            r#"{ kind = "llms_txt", url = "https://x.dev/llms.txt" }"#,
            r#"{ kind = "website", start_url = "https://x.dev/", sitemap = "https://x.dev/s.xml", allow_prefixes = ["/docs"], max_pages = 1 }"#,
        ] {
            assert_eq!(parse(source).unwrap().check(), Ok(()), "{source}");
        }
    }

    #[test]
    fn each_source_rule_has_its_own_reason() {
        let cases = [
            (r#"{ kind = "local", path = "" }"#, "`path` is empty"),
            (r#"{ kind = "github", repo = "tokio" }"#, "`repo` must be `owner/name`"),
            (r#"{ kind = "github", repo = "a/" }"#, "`repo` must be `owner/name`"),
            (r#"{ kind = "github", repo = "/b" }"#, "`repo` must be `owner/name`"),
            (r#"{ kind = "github", repo = "../b" }"#, "`repo` must be `owner/name`"),
            (r#"{ kind = "github", repo = "a/." }"#, "`repo` must be `owner/name`"),
            (r#"{ kind = "github", repo = "a/b c" }"#, "`repo` must be `owner/name`"),
            (
                r#"{ kind = "github", repo = "a/b", reference = "" }"#,
                "`reference` is empty or contains whitespace",
            ),
            (
                r#"{ kind = "github", repo = "a/b", reference = "main x" }"#,
                "`reference` is empty or contains whitespace",
            ),
            (
                r#"{ kind = "github", repo = "a/b", paths = ["../x"] }"#,
                "`paths` must be relative and must not contain `..`",
            ),
            (
                r#"{ kind = "github", repo = "a/b", paths = ["/etc"] }"#,
                "`paths` must be relative and must not contain `..`",
            ),
            (
                r#"{ kind = "github", repo = "a/b", paths = ["a\\b"] }"#,
                "`paths` must be relative and must not contain `..`",
            ),
            (r#"{ kind = "url", urls = [] }"#, "`urls` is empty"),
            (
                r#"{ kind = "url", urls = ["https://x.dev/a", "file:///etc/passwd"] }"#,
                "URLs must use http or https",
            ),
            (
                r#"{ kind = "llms_txt", url = "ftp://x.dev/llms.txt" }"#,
                "URLs must use http or https",
            ),
            (
                r#"{ kind = "website", start_url = "https://x.dev/", sitemap = "file:///s.xml" }"#,
                "URLs must use http or https",
            ),
            (
                r#"{ kind = "url", urls = ["https://x.dev/a", "https://x.dev/b#v2"] }"#,
                "URLs must not have a fragment",
            ),
            (
                r#"{ kind = "llms_txt", url = "https://x.dev/llms.txt#" }"#,
                "URLs must not have a fragment",
            ),
            (
                r#"{ kind = "website", start_url = "https://x.dev/#top" }"#,
                "URLs must not have a fragment",
            ),
            (
                r#"{ kind = "website", start_url = "https://x.dev/", allow_prefixes = ["docs"] }"#,
                "`allow_prefixes` entries must start with `/`",
            ),
            (
                r#"{ kind = "website", start_url = "https://x.dev/", max_pages = 0 }"#,
                "`max_pages` must be at least 1",
            ),
        ];
        for (source, reason) in cases {
            assert_eq!(parse(source).unwrap().check(), Err(reason), "{source}");
        }
    }

    #[test]
    fn describes_each_source_in_one_line() {
        let describe = |s: &str| parse(s).unwrap().describe();
        assert_eq!(describe(r#"{ kind = "local", path = "/srv/docs" }"#), "local:/srv/docs");
        assert_eq!(
            describe(r#"{ kind = "github", repo = "a/b", reference = "v1" }"#),
            "github:a/b@v1"
        );
        assert_eq!(describe(r#"{ kind = "github", repo = "a/b" }"#), "github:a/b");
        assert_eq!(
            describe(r#"{ kind = "github", repo = "a/b", reference = "v1", paths = ["docs", "guide"] }"#),
            "github:a/b@v1 (docs, guide)"
        );
        assert_eq!(
            describe(r#"{ kind = "url", urls = ["https://x.dev/a"] }"#),
            "url:https://x.dev/a"
        );
        assert_eq!(
            describe(r#"{ kind = "url", urls = ["https://x.dev/a", "https://x.dev/b", "https://x.dev/c"] }"#),
            "url:https://x.dev/a (+2 more)"
        );
        assert_eq!(describe(r#"{ kind = "url", urls = [] }"#), "url:");
        assert_eq!(
            describe(r#"{ kind = "llms_txt", url = "https://x.dev/llms.txt" }"#),
            "llms_txt:https://x.dev/llms.txt"
        );
        assert_eq!(
            describe(r#"{ kind = "website", start_url = "https://x.dev/" }"#),
            "website:https://x.dev/"
        );
    }

    #[test]
    fn content_hash_is_lowercase_hex_sha256() {
        assert_eq!(
            content_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn doc_key(path: &str) -> DocKey {
        let scope = Scope::new(LibraryId::parse("a").unwrap(), Version::parse("1").unwrap());
        DocKey::new(&scope, path).unwrap()
    }

    #[test]
    fn outcome_is_partial_when_any_document_failed() {
        let mut changes = VersionChanges::default();
        assert_eq!(changes.outcome(), Outcome::Ok);
        changes.errors.push(DocError {
            doc_path: "a.md".into(),
            kind: "too_large".into(),
            message: "body exceeds 10 bytes".into(),
        });
        assert_eq!(changes.outcome(), Outcome::Partial);
    }

    #[test]
    fn only_upserts_and_deletions_change_sections() {
        let mut changes = VersionChanges {
            unchanged: vec![doc_key("a.md")],
            ..VersionChanges::default()
        };
        assert!(!changes.changes_sections());
        changes.deletions.push(doc_key("b.md"));
        assert!(changes.changes_sections());
        changes.deletions.clear();
        changes.upserts.push(Document {
            state: DocState {
                doc_key: doc_key("c.md"),
                etag: None,
                last_modified: None,
                content_hash: content_hash(""),
                fetched_at: Timestamp::from_unix(0),
                source: None,
            },
            sections: vec![],
        });
        assert!(changes.changes_sections());
    }

    #[test]
    fn outcomes_render_in_snake_case() {
        #[derive(Serialize)]
        struct W {
            o: Outcome,
        }
        assert_eq!(Outcome::Ok.to_string(), "ok");
        assert_eq!(Outcome::Partial.to_string(), "partial");
        assert_eq!(Outcome::Failed.to_string(), "failed");
        assert_eq!(
            toml::to_string(&W { o: Outcome::Partial }).unwrap().trim(),
            "o = \"partial\""
        );
    }

    #[test]
    fn doc_errors_display_path_message_and_kind() {
        let error = DocError {
            doc_path: "https://x.dev/a".into(),
            kind: "status".into(),
            message: "HTTP 503".into(),
        };
        assert_eq!(error.to_string(), "https://x.dev/a: HTTP 503 (status)");
    }

    #[test]
    fn version_state_round_trips() {
        let state = VersionState {
            scope: Scope::new(LibraryId::parse("a").unwrap(), Version::parse("1").unwrap()),
            name: "A".into(),
            description: "d".into(),
            aliases: vec!["aa".into()],
            homepage: Some("https://a.dev".into()),
            license: Some("MIT".into()),
            source: "local:/a".into(),
            default: true,
            list_order: 0,
            revision: 3,
            last_success: Some(Timestamp::from_unix(5)),
            last_attempt: Timestamp::from_unix(6),
            last_verified: None,
            outcome: Outcome::Partial,
            last_error: Some("x".into()),
            document_count: 2,
            section_count: 9,
            resolved_commit: None,
            origin: Origin::Config,
            plan: None,
            bytes: 0,
        };
        let text = toml::to_string(&state).unwrap();
        assert_eq!(toml::from_str::<VersionState>(&text).unwrap(), state);

        let source = crate::plan::PlanSource {
            id: "guides".into(),
            kind: crate::plan::SourceKind::Url,
            spec: serde_json::Map::from_iter([("urls".to_owned(), serde_json::json!(["https://a.dev/llms.txt"]))]),
            revision: None,
        };
        let catalog = VersionState {
            origin: Origin::Discovered,
            plan: Some(crate::plan::Plan::new(None, None, vec![], vec![source]).unwrap()),
            bytes: 4096,
            ..state
        };
        let text = serde_json::to_string(&catalog).unwrap();
        assert!(text.contains(r#""origin":"discovered""#), "{text}");
        assert_eq!(serde_json::from_str::<VersionState>(&text).unwrap(), catalog);
    }

    #[test]
    fn origins_render_in_snake_case_and_default_to_config() {
        assert_eq!(Origin::default(), Origin::Config);
        for (origin, name) in [
            (Origin::Config, "config"),
            (Origin::Recipe, "recipe"),
            (Origin::Discovered, "discovered"),
        ] {
            assert_eq!(serde_json::to_value(origin).unwrap(), serde_json::json!(name));
            assert_eq!(
                serde_json::from_value::<Origin>(serde_json::json!(name)).unwrap(),
                origin
            );
        }
    }

    #[test]
    fn schema_one_records_decode_with_the_new_fields_defaulted() {
        let v1_state = r#"{"scope":{"library":"a","version":"1"},"name":"A","description":"d","aliases":[],
            "homepage":null,"license":null,"source":"local:/a","default":true,"list_order":0,"revision":1,
            "last_success":null,"last_attempt":6,"last_verified":null,"outcome":"ok",
            "last_error":null,"document_count":0,"section_count":0,"resolved_commit":null}"#;
        let state: VersionState = serde_json::from_str(v1_state).unwrap();
        assert_eq!(state.origin, Origin::Config);
        assert_eq!(state.plan, None);
        assert_eq!(state.bytes, 0);
        assert_eq!(state.scope.to_string(), "a@1");

        let v1_doc = r#"{"doc_key":"a\u001f1\u001fx.md","etag":null,"last_modified":null,
            "content_hash":"00","fetched_at":0}"#;
        let doc: DocState = serde_json::from_str(v1_doc).unwrap();
        assert_eq!(doc.source, None);
        assert!(!serde_json::to_string(&doc).unwrap().contains("source"));

        let sourced = DocState {
            source: Some("guides".into()),
            ..doc
        };
        let text = serde_json::to_string(&sourced).unwrap();
        assert!(text.contains(r#""source":"guides""#), "{text}");
        assert_eq!(serde_json::from_str::<DocState>(&text).unwrap(), sourced);
    }
}
