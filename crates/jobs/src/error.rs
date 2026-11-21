//! Errors of the sync lock and of sync jobs.

use std::path::PathBuf;

use zenodotus_core::CoreError;
use zenodotus_ingest::IngestError;
use zenodotus_search::SearchError;

/// Why a job, or taking the sync lock, failed.
#[derive(Debug, thiserror::Error)]
pub enum JobsError {
    /// The selection names nothing that is configured.
    #[error("{0}")]
    Usage(String),
    /// A library id of the selection is invalid or not configured.
    #[error(transparent)]
    Core(#[from] CoreError),
    /// The index cannot be read or written.
    #[error(transparent)]
    Search(#[from] SearchError),
    /// The HTTP client for remote sources cannot be built.
    #[error(transparent)]
    Ingest(#[from] IngestError),
    /// Another sync holds the sync lock.
    #[error("another sync is running ({path} is locked); try again when it ends")]
    Busy {
        /// The lock file.
        path: PathBuf,
    },
    /// A file or directory cannot be accessed.
    #[error("cannot access {path}: {source}")]
    Io {
        /// The file or directory.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
}

impl JobsError {
    /// Wraps an I/O error with the path it concerns.
    pub(crate) fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io { path, source }
    }
}
