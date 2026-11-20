//! The `github` source: Markdown files of a repository at a resolved commit, read from its tarball while it streams.
//! The tarball is decompressed in a blocking task and never written to disk.

use std::collections::BTreeSet;
use std::io::{self, Read};
use std::time::Duration;

use bytes::{Buf, Bytes};
use flate2::read::GzDecoder;
use tokio::sync::mpsc;
use url::Url;
use zenodotus_core::ids::DocKey;
use zenodotus_core::model::{DocError, VersionChanges};

use crate::document::{self, Fetched};
use crate::error::IngestError;
use crate::fetch::{FetchError, FetchRequest, FetchResponse, HostSet, Opened, Stream};
use crate::local::is_markdown;
use crate::sectioner::SourceRef;
use crate::source::Context;

/// A tarball may decompress to at most this many times its compressed size limit.
pub const MAX_EXPANSION: u64 = 16;

/// How many chunks of the tarball may wait for the reader.
const CHANNEL_CHUNKS: usize = 16;

/// The largest answer accepted from the commit endpoint.
const COMMIT_MAX_BYTES: u64 = 4096;

/// Where GitHub sources are fetched from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHub {
    /// The REST API root.
    pub api: Url,
    /// The tarball host root.
    pub codeload: Url,
    /// A token for the API, which only raises its rate limit.
    pub token: Option<String>,
}

impl Default for GitHub {
    fn default() -> Self {
        Self {
            api: Url::parse("https://api.github.com/").expect("a valid URL"),
            codeload: Url::parse("https://codeload.github.com/").expect("a valid URL"),
            token: None,
        }
    }
}

/// Resolves the reference to a commit (a full commit id is used as is, without asking the API) and indexes the repository's Markdown files under `paths` at that commit.
/// When the commit equals the last one synced, every stored document is confirmed without downloading anything.
/// The tarball lists every file, so a stored document missing from it is deleted; one that is present but cannot be
/// read keeps its previous content.
///
/// # Errors
/// [`IngestError::ListingUnavailable`] when the commit cannot be resolved or the tarball cannot be read in full.
pub async fn collect_github(
    ctx: &Context<'_>,
    repo: &str,
    reference: Option<&str>,
    paths: &[String],
) -> Result<VersionChanges, IngestError> {
    let hosts = HostSet::of(&[&ctx.github.api, &ctx.github.codeload], ctx.extra_hosts);
    let commit = match reference {
        Some(reference) if is_commit_id(reference) => reference.to_owned(),
        reference => resolve_commit(ctx, repo, reference.unwrap_or("HEAD"), &hosts).await?,
    };
    let mut changes = VersionChanges {
        resolved_commit: Some(commit.clone()),
        ..VersionChanges::default()
    };
    if ctx.previous_commit == Some(commit.as_str()) {
        changes.unchanged = ctx.previous.keys().cloned().collect();
        return Ok(changes);
    }
    let tarball = endpoint(&ctx.github.codeload, repo.split('/').chain(["tar.gz", &commit]));
    let listing = download(ctx, &tarball, &hosts, paths).await?;
    // A file that is listed but could not be read keeps its previous content, so it is not deleted.
    let mut listed: BTreeSet<DocKey> = listing
        .errors
        .iter()
        .filter_map(|error| DocKey::new(ctx.scope, &error.doc_path).ok())
        .collect();
    changes.errors = listing.errors;
    for file in &listing.files {
        let doc_key = match DocKey::new(ctx.scope, &file.path) {
            Ok(doc_key) => doc_key,
            Err(error) => {
                changes
                    .errors
                    .push(doc_error(&file.path, "invalid_path", error.to_string()));
                continue;
            }
        };
        listed.insert(doc_key.clone());
        let source = format!("https://github.com/{repo}/blob/{commit}/{}", file.path);
        let fetched = Fetched {
            doc_key: &doc_key,
            source: SourceRef::Url(&source),
            text: &file.text,
            etag: None,
            last_modified: None,
            fetched_at: ctx.clock.now(),
        };
        match document::build(ctx.scope, &fetched, ctx.previous.get(&doc_key), &ctx.rules) {
            Some(document) => changes.upserts.push(document),
            None => changes.unchanged.push(doc_key),
        }
    }
    changes
        .deletions
        .extend(ctx.previous.keys().filter(|key| !listed.contains(*key)).cloned());
    Ok(changes)
}

/// `base` with `segments` appended, each percent-encoded.
fn endpoint<'a>(base: &Url, segments: impl IntoIterator<Item = &'a str>) -> Url {
    let mut url = base.clone();
    url.path_segments_mut()
        .expect("an http URL has path segments")
        .pop_if_empty()
        .extend(segments);
    url
}

/// The commit `reference` of `repo` points to, as 40 lowercase hex digits.
async fn resolve_commit(
    ctx: &Context<'_>,
    repo: &str,
    reference: &str,
    hosts: &HostSet,
) -> Result<String, IngestError> {
    let url = endpoint(
        &ctx.github.api,
        ["repos"]
            .into_iter()
            .chain(repo.split('/'))
            .chain(["commits"])
            .chain(reference.split('/')),
    );
    let mut headers = vec![("accept", "application/vnd.github.sha".to_owned())];
    if let Some(token) = &ctx.github.token {
        headers.push(("authorization", format!("Bearer {token}")));
    }
    let request = FetchRequest {
        url: &url,
        allowed_hosts: hosts,
        max_bytes: COMMIT_MAX_BYTES,
        validators: None,
        timeout: ctx.timeout(),
        headers: &headers,
    };
    let body = match ctx.fetcher.get(&request).await {
        Ok(FetchResponse::Body(body)) => body,
        Ok(_) => return Err(IngestError::listing(&url, &"repository or reference not found")),
        Err(error) => return Err(IngestError::listing(&url, &error)),
    };
    let commit = String::from_utf8_lossy(&body.bytes).trim().to_owned();
    if is_commit_id(&commit) {
        Ok(commit)
    } else {
        Err(IngestError::listing(&url, &"the answer is not a commit id"))
    }
}

/// Whether `text` is a full commit id: 40 lowercase hex digits.
fn is_commit_id(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// One Markdown file of the tarball.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TarFile {
    /// The path inside the repository.
    path: String,
    /// Its text.
    text: String,
}

/// What the tarball holds: the selected files, and those that could not be read.
#[derive(Debug, Default)]
struct TarListing {
    files: Vec<TarFile>,
    errors: Vec<DocError>,
}

/// Limits on one tarball.
#[derive(Debug, Clone, Copy)]
struct TarLimits {
    entry_max_bytes: u64,
    total_max_bytes: u64,
    max_entries: u64,
    expanded_max_bytes: u64,
}

/// Streams the tarball at `url` into a blocking reader that keeps the Markdown files under `paths`.
async fn download(ctx: &Context<'_>, url: &Url, hosts: &HostSet, paths: &[String]) -> Result<TarListing, IngestError> {
    let fetch = &ctx.settings;
    let request = FetchRequest {
        url,
        allowed_hosts: hosts,
        max_bytes: fetch.tarball_max_bytes,
        validators: None,
        timeout: Duration::from_secs(fetch.tarball_timeout_secs),
        headers: &[],
    };
    let stream = match ctx.fetcher.open(&request).await {
        Ok(Opened::Stream(stream)) => stream,
        Ok(_) => return Err(IngestError::listing(url, &"not found (HTTP 404 or 410)")),
        Err(error) => return Err(IngestError::listing(url, &error)),
    };
    let limits = TarLimits {
        entry_max_bytes: fetch.tar_entry_max_bytes,
        total_max_bytes: fetch.tar_total_max_bytes,
        max_entries: fetch.tar_max_entries,
        expanded_max_bytes: fetch.tarball_max_bytes.saturating_mul(MAX_EXPANSION),
    };
    let (sender, receiver) = mpsc::channel(CHANNEL_CHUNKS);
    let paths = paths.to_vec();
    let reader = tokio::task::spawn_blocking(move || read_tarball(ChannelReader::new(receiver), &paths, limits));
    let pumped = pump(stream, sender).await;
    let read = reader.await.expect("the tarball reader does not panic");
    pumped.map_err(|error| IngestError::listing(url, &error))?;
    read.map_err(|reason| IngestError::listing(url, &reason))
}

/// Sends every chunk of `stream` to the reader, until the stream ends or the reader stops.
async fn pump(mut stream: Box<Stream>, sender: mpsc::Sender<Bytes>) -> Result<(), FetchError> {
    while let Some(chunk) = stream.chunk().await? {
        if sender.send(chunk).await.is_err() {
            break;
        }
    }
    Ok(())
}

/// A blocking reader over the chunks of a channel; the end of the channel is the end of the data.
struct ChannelReader {
    receiver: mpsc::Receiver<Bytes>,
    current: Bytes,
}

impl ChannelReader {
    const fn new(receiver: mpsc::Receiver<Bytes>) -> Self {
        Self {
            receiver,
            current: Bytes::new(),
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.current.is_empty() {
            match self.receiver.blocking_recv() {
                Some(chunk) => self.current = chunk,
                None => return Ok(0),
            }
        }
        let count = buf.len().min(self.current.len());
        buf[..count].copy_from_slice(&self.current[..count]);
        self.current.advance(count);
        Ok(count)
    }
}

/// A reader that fails once more than `limit` bytes came through, against decompression bombs.
struct Capped<R> {
    inner: R,
    left: u64,
    limit: u64,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buf)?;
        self.left = self
            .left
            .checked_sub(count as u64)
            .ok_or_else(|| io::Error::other(format!("larger than {} bytes once decompressed", self.limit)))?;
        Ok(count)
    }
}

/// Reads a gzip-compressed tarball and keeps its regular Markdown files under `paths`. Links, directories, paths
/// that are absolute or contain `..`, and other entries are skipped. A file over the entry limit or not in UTF-8 is
/// reported; too many entries, too many kept bytes, or a corrupt archive end the listing.
fn read_tarball(reader: impl Read, paths: &[String], limits: TarLimits) -> Result<TarListing, String> {
    let decompressed = Capped {
        inner: GzDecoder::new(reader),
        left: limits.expanded_max_bytes,
        limit: limits.expanded_max_bytes,
    };
    let mut archive = tar::Archive::new(decompressed);
    let mut listing = TarListing::default();
    let mut entries_seen = 0_u64;
    let mut kept_bytes = 0_u64;
    for entry in archive.entries().map_err(error_text)? {
        let mut entry = entry.map_err(error_text)?;
        entries_seen += 1;
        if entries_seen > limits.max_entries {
            return Err(format!("more than {} entries", limits.max_entries));
        }
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let Some(path) = inner_path(&entry.path_bytes()) else {
            continue;
        };
        if !is_markdown(&path) || !is_selected(&path, paths) {
            continue;
        }
        let size = entry.size();
        if size > limits.entry_max_bytes {
            let message = format!("{size} bytes exceeds the limit of {}", limits.entry_max_bytes);
            listing.errors.push(doc_error(&path, "too_large", message));
            continue;
        }
        kept_bytes += size;
        if kept_bytes > limits.total_max_bytes {
            return Err(format!("the Markdown files exceed {} bytes", limits.total_max_bytes));
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(error_text)?;
        match String::from_utf8(bytes) {
            Ok(text) => listing.files.push(TarFile { path, text }),
            Err(_) => listing
                .errors
                .push(doc_error(&path, "decode", "not valid UTF-8".to_owned())),
        }
    }
    Ok(listing)
}

/// The path of an entry inside the repository, without the tarball's top directory; `None` for paths that are not
/// UTF-8, are absolute, contain `..`, or name the top directory itself.
fn inner_path(raw: &[u8]) -> Option<String> {
    let path = std::str::from_utf8(raw).ok()?;
    if path.starts_with('/') {
        return None;
    }
    let parts: Vec<&str> = path
        .split('/')
        .skip(1)
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if parts.is_empty() || parts.contains(&"..") {
        return None;
    }
    Some(parts.join("/"))
}

/// Whether `path` lies under one of `paths`; every path when there are none.
fn is_selected(path: &str, paths: &[String]) -> bool {
    paths.is_empty()
        || paths.iter().any(|dir| {
            let dir = dir.trim_matches('/');
            dir.is_empty() || path == dir || path.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'))
        })
}

/// The text of an error, for `map_err`.
#[expect(clippy::needless_pass_by_value, reason = "passed to `map_err` by name")]
fn error_text(error: io::Error) -> String {
    error.to_string()
}

/// A per-document error.
fn doc_error(doc_path: &str, kind: &str, message: String) -> DocError {
    DocError {
        doc_path: doc_path.to_owned(),
        kind: kind.to_owned(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::{EntryType, Header};
    use wiremock::matchers::{header, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zenodotus_core::config::FetchSettings;

    use super::*;
    use crate::fetch::tests::{local_settings, raw_server, url};
    use crate::source::tests::{Harness, errors, paths};

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A gzip-compressed tarball of `entries`, written with raw names so that unsafe paths can be built.
    fn tarball(entries: &[(&str, EntryType, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        for (name, kind, data) in entries {
            let mut header = Header::new_gnu();
            header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
            header.set_entry_type(*kind);
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn repository() -> Vec<u8> {
        let big = vec![b'x'; 300];
        tarball(&[
            ("o-n-0123456/", EntryType::Directory, b""),
            ("o-n-0123456/docs/a.md", EntryType::Regular, b"# A\n\nAlpha\n"),
            ("o-n-0123456/docs/./b.mdx", EntryType::Regular, b"# B\n"),
            ("o-n-0123456/docs/logo.png", EntryType::Regular, b"\x89PNG"),
            ("o-n-0123456/docs/link.md", EntryType::Symlink, b""),
            ("o-n-0123456/docs/hard.md", EntryType::Link, b""),
            ("o-n-0123456/docs/../escape.md", EntryType::Regular, b"# Escape\n"),
            ("/etc/passwd.md", EntryType::Regular, b"# Root\n"),
            ("o-n-0123456/docs/big.md", EntryType::Regular, &big),
            ("o-n-0123456/docs/latin1.md", EntryType::Regular, b"caf\xe9\n"),
            ("o-n-0123456/docsextra/c.md", EntryType::Regular, b"# C\n"),
            ("o-n-0123456/README.md", EntryType::Regular, b"# Readme\n"),
            ("o-n-0123456/docs/bad\x01.md", EntryType::Regular, b"# Bad\n"),
        ])
    }

    fn settings() -> FetchSettings {
        FetchSettings {
            tar_entry_max_bytes: 200,
            ..local_settings()
        }
    }

    fn github(server: &MockServer, harness: &mut Harness) {
        let root = url(&format!("{}/", server.uri()));
        harness.github = GitHub {
            api: root.join("api/").unwrap(),
            codeload: root.join("codeload").unwrap(),
            token: None,
        };
    }

    async fn commit(server: &MockServer, route: &str, status: u16, body: &str) {
        Mock::given(path(route))
            .and(header("accept", "application/vnd.github.sha"))
            .respond_with(ResponseTemplate::new(status).set_body_raw(body.to_owned(), "text/plain"))
            .mount(server)
            .await;
    }

    async fn serve_tarball(server: &MockServer, status: u16, body: Vec<u8>, expected: u64) {
        Mock::given(path(format!("/codeload/o/n/tar.gz/{SHA}")))
            .respond_with(ResponseTemplate::new(status).set_body_raw(body, "application/x-gzip"))
            .expect(expected)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn markdown_files_under_the_paths_are_indexed_and_unsafe_entries_skipped() {
        let server = MockServer::start().await;
        Mock::given(path("/api/repos/o/n/commits/release/v1"))
            .and(header("authorization", "Bearer secret"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(format!("{SHA}\n"), "text/plain"))
            .expect(1)
            .mount(&server)
            .await;
        serve_tarball(&server, 200, repository(), 1).await;
        let mut harness = Harness::with_settings(settings());
        github(&server, &mut harness);
        harness.github.token = Some("secret".to_owned());
        harness.remember("docs/a.md", "# A\n\nAlpha\n", None);
        harness.remember("docs/removed.md", "old", None);
        harness.remember("docs/big.md", "# Big\n\nThe last good version\n", None);
        harness.remember("docs/latin1.md", "# Latin\n", None);
        let docs = ["/docs/".to_owned()];
        let changes = collect_github(&harness.context(), "o/n", Some("release/v1"), &docs)
            .await
            .unwrap();
        assert_eq!(changes.resolved_commit.as_deref(), Some(SHA));
        let upserted: Vec<&str> = changes.upserts.iter().map(|doc| doc.state.doc_key.doc_path()).collect();
        assert_eq!(upserted, ["docs/b.mdx"]);
        assert_eq!(
            changes.upserts[0].sections[0].source_ref,
            format!("https://github.com/o/n/blob/{SHA}/docs/b.mdx#b")
        );
        assert_eq!(paths(&changes.unchanged), ["docs/a.md"]);
        // Files that could not be read keep their previous content: only the removed file is deleted.
        assert_eq!(paths(&changes.deletions), ["docs/removed.md"]);
        assert_eq!(
            errors(&changes),
            [
                "docs/big.md (too_large)",
                "docs/latin1.md (decode)",
                "docs/bad\u{1}.md (invalid_path)"
            ]
        );
        assert_eq!(changes.errors[0].message, "300 bytes exceeds the limit of 200");
    }

    #[tokio::test]
    async fn a_reference_that_is_a_commit_id_is_used_without_asking_the_api() {
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::path_regex("^/api/"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        serve_tarball(&server, 200, repository(), 1).await;
        let mut harness = Harness::with_settings(settings());
        github(&server, &mut harness);
        let docs = ["docs".to_owned()];
        let changes = collect_github(&harness.context(), "o/n", Some(SHA), &docs)
            .await
            .unwrap();
        assert_eq!(changes.resolved_commit.as_deref(), Some(SHA));
        assert_ne!(changes.upserts.len(), 0);
    }

    #[tokio::test]
    async fn an_unchanged_commit_confirms_every_document_without_a_download() {
        let server = MockServer::start().await;
        commit(&server, "/api/repos/o/n/commits/HEAD", 200, SHA).await;
        serve_tarball(&server, 200, Vec::new(), 0).await;
        let mut harness = Harness::new();
        github(&server, &mut harness);
        harness.previous_commit = Some(SHA.to_owned());
        harness.remember("a.md", "old", None);
        let changes = collect_github(&harness.context(), "o/n", None, &[]).await.unwrap();
        assert_eq!(paths(&changes.unchanged), ["a.md"]);
        assert_eq!((changes.upserts.len(), changes.deletions.len()), (0, 0));
        assert_eq!(changes.resolved_commit.as_deref(), Some(SHA));
    }

    #[tokio::test]
    async fn the_whole_repository_is_read_when_no_paths_are_given() {
        let server = MockServer::start().await;
        commit(&server, "/api/repos/o/n/commits/HEAD", 200, SHA).await;
        serve_tarball(&server, 200, repository(), 1).await;
        let mut harness = Harness::with_settings(settings());
        github(&server, &mut harness);
        let changes = collect_github(&harness.context(), "o/n", None, &[]).await.unwrap();
        let upserted: Vec<&str> = changes.upserts.iter().map(|doc| doc.state.doc_key.doc_path()).collect();
        assert_eq!(upserted, ["docs/a.md", "docs/b.mdx", "docsextra/c.md", "README.md"]);
    }

    async fn failure(commit_answer: (u16, &str), tar: Option<(u16, Vec<u8>)>, settings: FetchSettings) -> String {
        let server = MockServer::start().await;
        commit(&server, "/api/repos/o/n/commits/HEAD", commit_answer.0, commit_answer.1).await;
        if let Some((status, body)) = tar {
            serve_tarball(&server, status, body, 1).await;
        }
        let mut harness = Harness::with_settings(settings);
        github(&server, &mut harness);
        let error = collect_github(&harness.context(), "o/n", None, &[]).await.unwrap_err();
        assert!(matches!(error, IngestError::ListingUnavailable { .. }), "{error:?}");
        error.to_string().replace(&server.uri(), "")
    }

    #[tokio::test]
    async fn a_commit_that_cannot_be_resolved_fails_the_version() {
        let commits = "cannot list /api/repos/o/n/commits/HEAD";
        assert_eq!(
            failure((404, ""), None, settings()).await,
            format!("{commits}: repository or reference not found")
        );
        assert_eq!(
            failure((500, ""), None, settings()).await,
            format!("{commits}: HTTP status 500")
        );
        assert_eq!(
            failure((200, "<html>rate limited</html>"), None, settings()).await,
            format!("{commits}: the answer is not a commit id")
        );
        assert_eq!(
            failure((200, &SHA.to_uppercase()), None, settings()).await,
            format!("{commits}: the answer is not a commit id")
        );
    }

    #[tokio::test]
    async fn tarballs_that_cannot_be_read_in_full_fail_the_version() {
        let tar = format!("cannot list /codeload/o/n/tar.gz/{SHA}");
        let ok = (200, SHA);
        assert_eq!(
            failure(ok, Some((404, Vec::new())), settings()).await,
            format!("{tar}: not found (HTTP 404 or 410)")
        );
        assert_eq!(
            failure(ok, Some((502, Vec::new())), settings()).await,
            format!("{tar}: HTTP status 502")
        );
        let small = FetchSettings {
            tarball_max_bytes: 100,
            ..settings()
        };
        assert_eq!(
            failure(ok, Some((200, repository())), small).await,
            format!("{tar}: larger than 100 bytes")
        );
        let corrupt = failure(ok, Some((200, b"\x1f\x8b\x08\x00garbage".to_vec())), settings()).await;
        assert!(corrupt.starts_with(&format!("{tar}: ")), "{corrupt}");
        let few = FetchSettings {
            tar_max_entries: 3,
            ..settings()
        };
        assert_eq!(
            failure(ok, Some((200, repository())), few).await,
            format!("{tar}: more than 3 entries")
        );
        let total = FetchSettings {
            tar_total_max_bytes: 12,
            ..settings()
        };
        assert_eq!(
            failure(ok, Some((200, repository())), total).await,
            format!("{tar}: the Markdown files exceed 12 bytes")
        );
    }

    #[tokio::test]
    async fn a_tarball_that_outgrows_its_limit_while_streaming_fails_the_version() {
        let server = MockServer::start().await;
        commit(&server, "/api/repos/o/n/commits/HEAD", 200, SHA).await;
        let mut unsized_body = b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n".to_vec();
        unsized_body.extend(repository());
        let codeload = raw_server(unsized_body.leak()).await.join("/").unwrap();
        let mut harness = Harness::with_settings(FetchSettings {
            tarball_max_bytes: 100,
            ..settings()
        });
        github(&server, &mut harness);
        harness.github.codeload = codeload.clone();
        let error = collect_github(&harness.context(), "o/n", None, &[]).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("cannot list {codeload}o/n/tar.gz/{SHA}: larger than 100 bytes")
        );
    }

    #[test]
    fn a_tarball_exactly_at_its_limits_is_read() {
        let data = tarball(&[
            ("o-n-0123456/docs/a.md", EntryType::Regular, b"12345"),
            ("o-n-0123456/docs/b.md", EntryType::Regular, b"123"),
        ]);
        let limits = TarLimits {
            entry_max_bytes: 5,
            total_max_bytes: 8,
            max_entries: 2,
            expanded_max_bytes: 1 << 20,
        };
        let listing = read_tarball(&data[..], &["docs".to_owned()], limits).unwrap();
        let files: Vec<(&str, &str)> = listing
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.text.as_str()))
            .collect();
        assert_eq!(files, [("docs/a.md", "12345"), ("docs/b.md", "123")]);
        assert_eq!(listing.errors, []);
    }

    #[tokio::test]
    async fn a_decompression_bomb_is_stopped() {
        let zeros = vec![0_u8; 1_000_000];
        let bomb = tarball(&[("o-n-0123456/zeros.bin", EntryType::Regular, &zeros)]);
        assert!(bomb.len() < 5_000, "{}", bomb.len());
        let tight = FetchSettings {
            tarball_max_bytes: 5_000,
            ..settings()
        };
        let tar = format!("cannot list /codeload/o/n/tar.gz/{SHA}");
        assert_eq!(
            failure((200, SHA), Some((200, bomb)), tight).await,
            format!("{tar}: larger than {} bytes once decompressed", 5_000 * MAX_EXPANSION)
        );
    }

    #[test]
    fn entry_paths_drop_the_top_directory_and_refuse_escapes() {
        assert_eq!(inner_path(b"top/docs/a.md").as_deref(), Some("docs/a.md"));
        assert_eq!(inner_path(b"top//docs/./a.md").as_deref(), Some("docs/a.md"));
        assert_eq!(inner_path(b"top/"), None);
        assert_eq!(inner_path(b"top/../a.md"), None);
        assert_eq!(inner_path(b"/abs/a.md"), None);
        assert_eq!(inner_path(b"top/\xff.md"), None);
        assert!(is_selected("docs/a.md", &["/".to_owned()]));
        assert!(is_selected("docs", &["docs".to_owned()]));
        assert!(!is_selected("docsextra/a.md", &["docs".to_owned()]));
    }

    #[test]
    fn the_channel_reader_skips_empty_chunks_and_ends_with_the_channel() {
        let (sender, receiver) = mpsc::channel(4);
        sender.try_send(Bytes::new()).unwrap();
        sender.try_send(Bytes::from_static(b"abc")).unwrap();
        drop(sender);
        let mut reader = ChannelReader::new(receiver);
        let mut buf = [0_u8; 2];
        assert_eq!(reader.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf, b"ab");
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"c");
        assert_eq!(GitHub::default().api.as_str(), "https://api.github.com/");
        assert_eq!(GitHub::default().codeload.as_str(), "https://codeload.github.com/");
    }

    #[tokio::test]
    async fn a_reader_that_stops_early_ends_the_download() {
        let mut state = 7_u32;
        let noise: Vec<u8> = (0..2_000_000)
            .map(|_| {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                state.to_be_bytes()[0]
            })
            .collect();
        let body = tarball(&[("o-n-0123456/noise.bin", EntryType::Regular, &noise)]);
        let none = FetchSettings {
            tar_max_entries: 0,
            ..settings()
        };
        let tar = format!("cannot list /codeload/o/n/tar.gz/{SHA}");
        assert_eq!(
            failure((200, SHA), Some((200, body)), none).await,
            format!("{tar}: more than 0 entries")
        );
    }
}
