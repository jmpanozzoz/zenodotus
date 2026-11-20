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
    /// An `include` or `exclude` pattern is not a valid glob.
    #[error("invalid glob pattern {pattern:?}: {message}")]
    InvalidPattern {
        /// The pattern.
        pattern: String,
        /// What the glob parser reported.
        message: String,
    },
}
