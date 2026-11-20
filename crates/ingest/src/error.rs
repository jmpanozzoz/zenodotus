//! Errors that stop the ingestion of a whole version. Errors of single documents are kept in
//! [`zenodotus_core::model::DocError`] instead.

use std::path::PathBuf;

/// Why no document of a version could be listed; the version's outcome becomes `failed`.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    /// The local source root does not exist or is not a directory.
    #[error("source directory {path} does not exist or is not a directory")]
    RootNotFound {
        /// The configured root.
        path: PathBuf,
    },
    /// A directory of the source could not be read, so the listing is incomplete.
    #[error("cannot read {path}: {source}")]
    Io {
        /// The directory.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The listing of a remote source (a tarball, an `llms.txt` file or a sitemap) could not be obtained.
    #[error("cannot list {what}: {reason}")]
    ListingUnavailable {
        /// What was being listed, usually a URL.
        what: String,
        /// Why it failed.
        reason: String,
    },
    /// The HTTP client could not be built.
    #[error("cannot build the HTTP client: {message}")]
    Client {
        /// What the client builder reported.
        message: String,
    },
    /// An `include` or `exclude` pattern is not a valid glob.
    #[error("invalid glob pattern {pattern:?}: {message}")]
    InvalidPattern {
        /// The pattern.
        pattern: String,
        /// What the glob parser reported.
        message: String,
    },
}

impl IngestError {
    /// A [`IngestError::Client`] from the builder's error.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "a `map_err` function receives the error by value"
    )]
    pub(crate) fn client(error: reqwest::Error) -> Self {
        Self::Client {
            message: error.to_string(),
        }
    }

    /// A [`IngestError::ListingUnavailable`] for `what`.
    pub(crate) fn listing(what: &impl std::fmt::Display, reason: &impl std::fmt::Display) -> Self {
        Self::ListingUnavailable {
            what: what.to_string(),
            reason: reason.to_string(),
        }
    }
}
