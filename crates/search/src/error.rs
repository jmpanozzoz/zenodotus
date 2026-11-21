//! Errors of the store and of queries.

use std::path::PathBuf;

/// Why a store operation or a query failed.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// No indexed library matches the requested id.
    #[error("unknown library {library:?}; indexed libraries: {}", list(available))]
    UnknownLibrary {
        /// The requested id.
        library: String,
        /// The ids that are indexed.
        available: Vec<String>,
    },
    /// The library is indexed, but not at the requested version.
    #[error(
        "library {library:?} has no version {version:?}; available versions: {}",
        list(available)
    )]
    UnknownVersion {
        /// The library id.
        library: String,
        /// The requested version.
        version: String,
        /// The versions that are indexed.
        available: Vec<String>,
    },
    /// No word of the query survived analysis.
    #[error("the query has no searchable words")]
    EmptyQuery,
    /// The query is longer than the limit.
    #[error("the query has {chars} characters; the limit is {max}")]
    QueryTooLong {
        /// Characters in the query.
        chars: usize,
        /// The limit.
        max: usize,
    },
    /// Nothing has been synced yet.
    #[error("the index is empty; run `zenodotus sync` first")]
    IndexEmpty,
    /// The index was built by another schema version or with other `[settings.index]` values.
    #[error("the index was built with {found}, but this build expects {expected}; run `zenodotus sync --rebuild`")]
    SchemaMismatch {
        /// What the index records.
        found: String,
        /// What this build and configuration expect.
        expected: String,
    },
    /// Another process holds the index writer.
    #[error("another sync is writing the index")]
    Locked,
    /// A document of the changes belongs to another scope than the version being applied.
    #[error("document {doc_key} does not belong to {scope}")]
    ForeignDocument {
        /// The document key, as text.
        doc_key: String,
        /// The scope being applied.
        scope: String,
    },
    /// Tantivy failed.
    #[error("index error: {0}")]
    Index(#[from] tantivy::TantivyError),
    /// A file-system operation on the index directories failed.
    #[error("cannot access {path}: {source}")]
    Io {
        /// The path.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A stored payload could not be read back.
    #[error("corrupt stored payload: {0}")]
    Payload(#[from] serde_json::Error),
}

impl SearchError {
    /// The `snake_case` code reported to MCP clients and in JSON output.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnknownLibrary { .. } => "unknown_library",
            Self::UnknownVersion { .. } => "unknown_version",
            Self::EmptyQuery => "empty_query",
            Self::QueryTooLong { .. } => "query_too_long",
            Self::IndexEmpty => "index_empty",
            Self::SchemaMismatch { .. } => "schema_mismatch",
            Self::Locked => "locked",
            Self::ForeignDocument { .. } => "foreign_document",
            Self::Index(_) => "index",
            Self::Io { .. } => "io",
            Self::Payload(_) => "payload",
        }
    }

    /// Wraps an I/O error with the path it concerns.
    pub(crate) fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io { path, source }
    }
}

/// `a, b, c`, or `none` for an empty list.
fn list(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_owned()
    } else {
        items.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|&s| s.to_owned()).collect()
    }

    #[test]
    fn every_error_has_a_message_and_a_code() {
        let io = || std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let payload = serde_json::from_str::<u8>("x").unwrap_err();
        let cases = [
            (
                SearchError::UnknownLibrary {
                    library: "x".to_owned(),
                    available: strings(&["a", "b"]),
                },
                "unknown_library",
                "unknown library \"x\"; indexed libraries: a, b",
            ),
            (
                SearchError::UnknownLibrary {
                    library: "x".to_owned(),
                    available: Vec::new(),
                },
                "unknown_library",
                "unknown library \"x\"; indexed libraries: none",
            ),
            (
                SearchError::UnknownVersion {
                    library: "a".to_owned(),
                    version: "9".to_owned(),
                    available: strings(&["1", "2"]),
                },
                "unknown_version",
                "library \"a\" has no version \"9\"; available versions: 1, 2",
            ),
            (
                SearchError::EmptyQuery,
                "empty_query",
                "the query has no searchable words",
            ),
            (
                SearchError::QueryTooLong { chars: 1001, max: 1000 },
                "query_too_long",
                "the query has 1001 characters; the limit is 1000",
            ),
            (
                SearchError::IndexEmpty,
                "index_empty",
                "the index is empty; run `zenodotus sync` first",
            ),
            (
                SearchError::SchemaMismatch {
                    found: "a".to_owned(),
                    expected: "b".to_owned(),
                },
                "schema_mismatch",
                "the index was built with a, but this build expects b; run `zenodotus sync --rebuild`",
            ),
            (SearchError::Locked, "locked", "another sync is writing the index"),
            (
                SearchError::ForeignDocument {
                    doc_key: "k".to_owned(),
                    scope: "a@1".to_owned(),
                },
                "foreign_document",
                "document k does not belong to a@1",
            ),
            (
                SearchError::Index(tantivy::TantivyError::InvalidArgument("bad".to_owned())),
                "index",
                "index error: An invalid argument was passed: 'bad'",
            ),
            (SearchError::io("/x")(io()), "io", "cannot access /x: gone"),
            (
                SearchError::Payload(payload),
                "payload",
                "corrupt stored payload: expected value at line 1 column 1",
            ),
        ];
        for (error, code, message) in cases {
            assert_eq!(error.code(), code);
            assert_eq!(error.to_string(), message);
        }
    }
}
