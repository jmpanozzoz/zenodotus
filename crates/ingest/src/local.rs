//! The `local` source: Markdown files under a directory, read without following symbolic links.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use zenodotus_core::ids::{DocKey, Scope};
use zenodotus_core::model::{DocError, DocState, VersionChanges};
use zenodotus_core::time::Clock;

use crate::document::{self, Fetched};
use crate::error::IngestError;
use crate::sectioner::{SectionRules, SourceRef};

/// File extensions read as Markdown.
const MARKDOWN_EXTENSIONS: [&str; 3] = ["md", "mdx", "markdown"];
/// Directory names never entered.
const SKIPPED_DIRECTORIES: [&str; 1] = ["node_modules"];

/// A local source as configured.
#[derive(Debug, Clone, Copy)]
pub struct LocalSource<'a> {
    /// The source root.
    pub root: &'a Path,
    /// Globs of files to keep, relative to the root; empty keeps every Markdown file.
    pub include: &'a [String],
    /// Globs of files to drop.
    pub exclude: &'a [String],
}

/// Limits and rules for one collection run.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Files larger than this are reported and skipped.
    pub max_bytes: u64,
    /// Sectioning rules.
    pub rules: SectionRules,
}

/// Lists, reads and sections every Markdown file under the source root. A file whose content hash matches its
/// previous state is unchanged; a previous document that is no longer listed is deleted.
///
/// # Errors
/// [`IngestError::RootNotFound`] when the root is not a directory, [`IngestError::Io`] when a directory cannot be
/// read, and [`IngestError::InvalidPattern`] for a bad glob. In each case nothing is listed.
pub fn collect_local(
    scope: &Scope,
    source: &LocalSource<'_>,
    previous: &BTreeMap<DocKey, DocState>,
    clock: &dyn Clock,
    limits: &Limits,
) -> Result<VersionChanges, IngestError> {
    if !source.root.is_dir() {
        return Err(IngestError::RootNotFound {
            path: source.root.to_owned(),
        });
    }
    let include = glob_set(source.include)?;
    let exclude = glob_set(source.exclude)?;
    let mut files = Vec::new();
    walk(source.root, "", &mut files)?;
    let mut changes = VersionChanges::default();
    let mut listed = BTreeSet::new();
    for file in files {
        let selected = is_markdown(&file.relative)
            && (source.include.is_empty() || include.is_match(&file.relative))
            && !exclude.is_match(&file.relative);
        if !selected {
            continue;
        }
        let Ok(doc_key) = DocKey::new(scope, &file.relative) else {
            changes.errors.push(doc_error(
                &file.relative,
                "invalid_path",
                "path cannot be used as a key".into(),
            ));
            continue;
        };
        listed.insert(doc_key.clone());
        match read(&file, limits.max_bytes) {
            Ok(text) => {
                let fetched = Fetched {
                    doc_key: &doc_key,
                    source: SourceRef::Local(&file.relative),
                    text: &text,
                    etag: None,
                    last_modified: None,
                    fetched_at: clock.now(),
                };
                match document::build(scope, &fetched, previous.get(&doc_key), &limits.rules) {
                    Some(document) => changes.upserts.push(document),
                    None => changes.unchanged.push(doc_key),
                }
            }
            Err(error) => changes.errors.push(error),
        }
    }
    changes.deletions = previous.keys().filter(|key| !listed.contains(*key)).cloned().collect();
    Ok(changes)
}

/// A regular file found under the root.
struct ListedFile {
    relative: String,
    path: PathBuf,
    len: u64,
}

/// Appends every regular file under `dir` in name order, skipping symbolic links, hidden entries and
/// [`SKIPPED_DIRECTORIES`]. Names that are not UTF-8 are skipped.
fn walk(dir: &Path, prefix: &str, files: &mut Vec<ListedFile>) -> Result<(), IngestError> {
    let io = |source| IngestError::Io {
        path: dir.to_owned(),
        source,
    };
    let mut entries = std::fs::read_dir(dir)
        .and_then(std::iter::Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(io)?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let file_type = entry.file_type().map_err(io)?;
        if name.starts_with('.') || file_type.is_symlink() {
            continue;
        }
        let relative = format!("{prefix}{name}");
        if file_type.is_dir() {
            if !SKIPPED_DIRECTORIES.contains(&name.as_str()) {
                walk(&entry.path(), &format!("{relative}/"), files)?;
            }
        } else if file_type.is_file() {
            let len = entry.metadata().map_err(io)?.len();
            files.push(ListedFile {
                relative,
                path: entry.path(),
                len,
            });
        }
    }
    Ok(())
}

/// Reads one file as UTF-8, within `max_bytes`.
fn read(file: &ListedFile, max_bytes: u64) -> Result<String, DocError> {
    if file.len > max_bytes {
        return Err(doc_error(
            &file.relative,
            "too_large",
            format!("{} bytes exceeds the limit of {max_bytes}", file.len),
        ));
    }
    let bytes = std::fs::read(&file.path).map_err(|e| doc_error(&file.relative, "io", e.to_string()))?;
    String::from_utf8(bytes).map_err(|_| doc_error(&file.relative, "decode", "not valid UTF-8".into()))
}

/// Whether `relative` has a Markdown extension, ignoring ASCII case.
pub(crate) fn is_markdown(relative: &str) -> bool {
    Path::new(relative)
        .extension()
        .is_some_and(|ext| MARKDOWN_EXTENSIONS.iter().any(|md| ext.eq_ignore_ascii_case(md)))
}

/// Compiles `patterns`, where `*` does not cross `/` and `**` does.
fn glob_set(patterns: &[String]) -> Result<GlobSet, IngestError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map_err(pattern_error)?,
        );
    }
    builder.build().map_err(pattern_error)
}

/// Converts a glob error, naming the pattern when the error knows it.
#[expect(clippy::needless_pass_by_value, reason = "passed to `map_err` by name")]
fn pattern_error(error: globset::Error) -> IngestError {
    IngestError::InvalidPattern {
        pattern: error.glob().unwrap_or_default().to_owned(),
        message: error.kind().to_string(),
    }
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
    use std::fs;

    use zenodotus_core::ids::{LibraryId, Version};
    use zenodotus_core::model::Outcome;
    use zenodotus_core::time::{FixedClock, Timestamp};

    use super::*;

    fn scope() -> Scope {
        Scope::new(LibraryId::parse("lib").unwrap(), Version::parse("1").unwrap())
    }

    fn limits() -> Limits {
        Limits {
            max_bytes: 1024,
            rules: SectionRules::default(),
        }
    }

    fn write(root: &Path, relative: &str, text: &[u8]) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn collect(
        root: &Path,
        include: &[String],
        exclude: &[String],
        previous: &BTreeMap<DocKey, DocState>,
    ) -> VersionChanges {
        let source = LocalSource { root, include, exclude };
        collect_local(
            &scope(),
            &source,
            previous,
            &FixedClock::new(Timestamp::from_unix(50)),
            &limits(),
        )
        .unwrap()
    }

    fn paths(keys: impl IntoIterator<Item = DocKey>) -> Vec<String> {
        keys.into_iter().map(|key| key.doc_path().to_owned()).collect()
    }

    fn states(changes: &VersionChanges) -> BTreeMap<DocKey, DocState> {
        changes
            .upserts
            .iter()
            .map(|doc| (doc.state.doc_key.clone(), doc.state.clone()))
            .collect()
    }

    #[test]
    fn lists_markdown_files_in_order_and_skips_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "b.md", b"# B\n");
        write(root, "a/z.MDX", b"# Z\n");
        write(root, "a/y.markdown", b"# Y\n");
        write(root, "notes.txt", b"x");
        write(root, ".hidden/h.md", b"# H\n");
        write(root, ".draft.md", b"# D\n");
        write(root, "node_modules/pkg/readme.md", b"# R\n");
        let changes = collect(root, &[], &[], &BTreeMap::new());
        let upserted: Vec<String> = changes
            .upserts
            .iter()
            .map(|d| d.state.doc_key.doc_path().to_owned())
            .collect();
        assert_eq!(upserted, ["a/y.markdown", "a/z.MDX", "b.md"]);
        assert_eq!(changes.outcome(), Outcome::Ok);
        assert_eq!(changes.upserts[2].state.fetched_at, Timestamp::from_unix(50));
        assert_eq!(changes.upserts[2].sections[0].source_ref, "local:b.md");
    }

    #[test]
    fn include_and_exclude_globs_filter_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for path in ["guide/a.md", "guide/deep/b.md", "api/c.md", "top.md"] {
            write(root, path, b"# T\n");
        }
        let changes = collect(
            root,
            &["guide/**".to_owned(), "*.md".to_owned()],
            &["guide/deep/*".to_owned()],
            &BTreeMap::new(),
        );
        let upserted: Vec<String> = changes
            .upserts
            .iter()
            .map(|d| d.state.doc_key.doc_path().to_owned())
            .collect();
        assert_eq!(upserted, ["guide/a.md", "top.md"]);
    }

    #[test]
    fn unchanged_changed_and_deleted_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "keep.md", b"# Keep\n");
        write(root, "edit.md", b"# Edit\n");
        write(root, "gone.md", b"# Gone\n");
        let first = collect(root, &[], &[], &BTreeMap::new());
        assert_eq!(first.upserts.len(), 3);
        write(root, "edit.md", b"# Edit\n\nchanged\n");
        fs::remove_file(root.join("gone.md")).unwrap();
        let second = collect(root, &[], &[], &states(&first));
        assert_eq!(
            paths(second.upserts.iter().map(|d| d.state.doc_key.clone())),
            ["edit.md"]
        );
        assert_eq!(paths(second.unchanged), ["keep.md"]);
        assert_eq!(paths(second.deletions), ["gone.md"]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed_so_nothing_escapes_the_root() {
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "secret.md", b"# Secret\n");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "real.md", b"# Real\n");
        std::os::unix::fs::symlink(outside.path().join("secret.md"), root.join("link.md")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("linked_dir")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(root.join("socket.md")).unwrap();
        let changes = collect(root, &[], &[], &BTreeMap::new());
        let upserted: Vec<String> = changes
            .upserts
            .iter()
            .map(|d| d.state.doc_key.doc_path().to_owned())
            .collect();
        assert_eq!(upserted, ["real.md"]);
    }

    #[test]
    fn oversized_and_non_utf8_files_are_document_errors_and_keep_their_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "big.md", &[b'a'; 2000]);
        write(root, "limit.md", &[b'a'; 1024]);
        write(root, "bad.md", b"# \xff\xfe\n");
        let previous_key = DocKey::new(&scope(), "big.md").unwrap();
        let previous = BTreeMap::from([(
            previous_key.clone(),
            DocState {
                doc_key: previous_key,
                etag: None,
                last_modified: None,
                content_hash: "old".into(),
                fetched_at: Timestamp::from_unix(1),
                source: None,
            },
        )]);
        let changes = collect(root, &[], &[], &previous);
        let kinds: Vec<(&str, &str)> = changes
            .errors
            .iter()
            .map(|e| (e.doc_path.as_str(), e.kind.as_str()))
            .collect();
        assert_eq!(kinds, [("bad.md", "decode"), ("big.md", "too_large")]);
        assert_eq!(changes.errors[1].message, "2000 bytes exceeds the limit of 1024");
        assert_eq!(changes.deletions, [] as [zenodotus_core::DocKey; 0]);
        assert_eq!(changes.outcome(), Outcome::Partial);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_files_and_bad_names_are_document_errors() {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "locked.md", b"# L\n");
        fs::set_permissions(root.join("locked.md"), fs::Permissions::from_mode(0o000)).unwrap();
        write(root, "tab\tname.md", b"# T\n");
        fs::write(root.join(std::ffi::OsStr::from_bytes(b"\xff.md")), b"# X\n").unwrap();
        let changes = collect(root, &[], &[], &BTreeMap::new());
        let kinds: Vec<(&str, &str)> = changes
            .errors
            .iter()
            .map(|e| (e.doc_path.as_str(), e.kind.as_str()))
            .collect();
        assert_eq!(kinds, [("locked.md", "io"), ("tab\tname.md", "invalid_path")]);
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_root_or_unreadable_directory_fails_the_listing() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let source = |root| LocalSource {
            root,
            include: &[],
            exclude: &[],
        };
        let clock = FixedClock::new(Timestamp::from_unix(0));
        let missing = dir.path().join("missing");
        assert!(matches!(
            collect_local(&scope(), &source(&missing), &BTreeMap::new(), &clock, &limits()),
            Err(IngestError::RootNotFound { path }) if path == missing
        ));
        write(dir.path(), "file.md", b"");
        let file = dir.path().join("file.md");
        assert!(matches!(
            collect_local(&scope(), &source(&file), &BTreeMap::new(), &clock, &limits()),
            Err(IngestError::RootNotFound { .. })
        ));
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let result = collect_local(&scope(), &source(dir.path()), &BTreeMap::new(), &clock, &limits());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(result, Err(IngestError::Io { path, .. }) if path == locked));
    }

    #[test]
    fn invalid_globs_fail_the_listing() {
        let dir = tempfile::tempdir().unwrap();
        let source = LocalSource {
            root: dir.path(),
            include: &[],
            exclude: &["a[".to_owned()],
        };
        let clock = FixedClock::new(Timestamp::from_unix(0));
        assert!(matches!(
            collect_local(&scope(), &source, &BTreeMap::new(), &clock, &limits()),
            Err(IngestError::InvalidPattern { pattern, .. }) if pattern == "a["
        ));
    }
}
