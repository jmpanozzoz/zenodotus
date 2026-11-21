//! Why a command failed, and the exit code it ends with.

use std::path::PathBuf;

use zenodotus_core::CoreError;
use zenodotus_ingest::IngestError;
use zenodotus_jobs::JobsError;
use zenodotus_mcp::ServeError;
use zenodotus_search::SearchError;

/// A failed command.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The arguments are invalid.
    #[error("{0}")]
    Usage(String),
    /// The config file is missing, invalid or cannot be edited.
    #[error(transparent)]
    Core(#[from] CoreError),
    /// The index cannot be read or written.
    #[error(transparent)]
    Search(#[from] SearchError),
    /// The HTTP client for remote sources cannot be built.
    #[error(transparent)]
    Ingest(#[from] IngestError),
    /// The MCP server cannot start or failed.
    #[error(transparent)]
    Serve(#[from] ServeError),
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
    /// An MCP client's config file is not valid JSON or TOML of the expected shape; it is left unchanged.
    #[error("{path} cannot be edited: {reason}; it was left unchanged")]
    ClientConfig {
        /// The client's config file.
        path: PathBuf,
        /// What is wrong with it.
        reason: String,
    },
    /// The command's output cannot be written.
    #[error("cannot write the output: {0}")]
    Output(#[from] std::io::Error),
    /// At least one version ended `partial` or `failed`.
    #[error("{count} version(s) did not sync completely; see above")]
    Incomplete {
        /// How many versions ended `partial` or `failed`.
        count: usize,
    },
    /// A rebuild was abandoned because a version failed; the previous index is kept.
    #[error("the rebuild was abandoned because {count} version(s) failed; the previous index is unchanged")]
    RebuildAbandoned {
        /// How many versions failed.
        count: usize,
    },
}

impl AppError {
    /// The process exit code: 1 for runtime failures, 2 for usage and configuration errors, 3 for incomplete syncs
    /// and 4 when another sync holds the lock.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) | Self::Core(_) => match self {
                Self::Core(CoreError::ConfigIo { .. }) => 1,
                _ => 2,
            },
            Self::Search(SearchError::Locked) | Self::Busy { .. } => 4,
            Self::Search(SearchError::SchemaMismatch { .. })
            | Self::Serve(ServeError::InsecureBind { .. })
            | Self::ClientConfig { .. } => 2,
            Self::Incomplete { .. } | Self::RebuildAbandoned { .. } => 3,
            Self::Search(_) | Self::Serve(_) | Self::Ingest(_) | Self::Io { .. } | Self::Output(_) => 1,
        }
    }

    /// Wraps an I/O error with the path it concerns.
    pub(crate) fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io { path, source }
    }
}

impl From<JobsError> for AppError {
    /// Keeps each job error's exit code: a held lock is 4, a selection that is not configured 2.
    fn from(error: JobsError) -> Self {
        match error {
            JobsError::Usage(message) => Self::Usage(message),
            JobsError::Core(error) => Self::Core(error),
            JobsError::Search(error) => Self::Search(error),
            JobsError::Ingest(error) => Self::Ingest(error),
            JobsError::Busy { path } => Self::Busy { path },
            JobsError::Io { path, source } => Self::Io { path, source },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_errors_keep_their_exit_codes() {
        let io = || std::io::Error::other("denied");
        let cases = [
            (JobsError::Usage("no such version".to_owned()), 2),
            (
                JobsError::Core(CoreError::UnknownLibrary {
                    library: "a".to_owned(),
                }),
                2,
            ),
            (JobsError::Search(SearchError::Locked), 4),
            (
                JobsError::Ingest(IngestError::Client {
                    message: "no TLS".to_owned(),
                }),
                1,
            ),
            (
                JobsError::Busy {
                    path: PathBuf::from("sync.lock"),
                },
                4,
            ),
            (
                JobsError::Io {
                    path: PathBuf::from("data"),
                    source: io(),
                },
                1,
            ),
        ];
        for (error, code) in cases {
            let message = error.to_string();
            let error = AppError::from(error);
            assert_eq!((error.exit_code(), error.to_string()), (code, message));
        }
    }
}
