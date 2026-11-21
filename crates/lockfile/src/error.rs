//! Why a project file was skipped or could not be parsed. Every error surfaces as a [`crate::Diagnostic`]; none
//! stops a project from resolving.

use std::path::PathBuf;

/// One file or member that could not be used.
#[derive(Debug, thiserror::Error)]
pub enum LockfileError {
    /// The path is a symlink, a FIFO, a device or another non-regular file, or a directory on the way to it is one.
    #[error("{path}: not a regular file")]
    NotRegularFile {
        /// The path, relative to the root.
        path: PathBuf,
    },
    /// The file's size, read from its metadata before reading, is over the limit for its format.
    #[error("{path}: {size} bytes is over the {limit}-byte limit")]
    TooLarge {
        /// The path, relative to the root.
        path: PathBuf,
        /// The size from the file's metadata.
        size: u64,
        /// The limit for this format.
        limit: u64,
    },
    /// A workspace member or a project path is absolute or leaves the root once normalized.
    #[error("{file}: member {member:?} is outside the root")]
    PathEscapesRoot {
        /// The file that names the member, relative to the root.
        file: PathBuf,
        /// The member as written.
        member: String,
    },
    /// A member glob uses `**` or a pattern outside its last path segment.
    #[error("{file}: member glob {pattern:?} is refused; only one directory level expands")]
    GlobRefused {
        /// The file that holds the glob, relative to the root.
        file: PathBuf,
        /// The glob as written.
        pattern: String,
    },
    /// The root already had its quota of files read.
    #[error("{path}: not read, the root already read {limit} files")]
    FileLimit {
        /// The file that was not read, relative to the root.
        path: PathBuf,
        /// The per-root file limit.
        limit: usize,
    },
    /// The member lists name more members than are read.
    #[error("{file}: more than {limit} members; the rest are ignored")]
    TooManyMembers {
        /// The file whose members overflowed, relative to the root.
        file: PathBuf,
        /// The member limit.
        limit: usize,
    },
    /// The file is not valid in its format.
    #[error("{file}{}: {message}", line.map(|line| format!(":{line}")).unwrap_or_default())]
    Parse {
        /// The file, relative to the root.
        file: PathBuf,
        /// The 1-based line of the error, when the parser reports one.
        line: Option<u64>,
        /// What the parser reported.
        message: String,
    },
    /// A YAML file exceeded the parser budget: too many aliases, anchors, nodes or replayed events.
    #[error("{file}: YAML budget exceeded")]
    YamlBudget {
        /// The file, relative to the root.
        file: PathBuf,
    },
    /// The file exists but could not be opened or read.
    #[error("{path}: {source}")]
    Io {
        /// The path, relative to the root.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}

/// Why a diagnostic was raised, without the details: one value per [`LockfileError`] variant. It lets a caller tell a
/// symlink from an oversized file, which share the kind `skipped_file`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cause {
    /// [`LockfileError::NotRegularFile`].
    NotRegularFile,
    /// [`LockfileError::TooLarge`].
    TooLarge,
    /// [`LockfileError::PathEscapesRoot`].
    PathEscapesRoot,
    /// [`LockfileError::GlobRefused`].
    GlobRefused,
    /// [`LockfileError::FileLimit`].
    FileLimit,
    /// [`LockfileError::TooManyMembers`].
    TooManyMembers,
    /// [`LockfileError::Parse`].
    Parse,
    /// [`LockfileError::YamlBudget`].
    YamlBudget,
    /// [`LockfileError::Io`].
    Io,
}

impl LockfileError {
    /// The variant, without its details.
    #[must_use]
    pub const fn cause(&self) -> Cause {
        match self {
            Self::NotRegularFile { .. } => Cause::NotRegularFile,
            Self::TooLarge { .. } => Cause::TooLarge,
            Self::PathEscapesRoot { .. } => Cause::PathEscapesRoot,
            Self::GlobRefused { .. } => Cause::GlobRefused,
            Self::FileLimit { .. } => Cause::FileLimit,
            Self::TooManyMembers { .. } => Cause::TooManyMembers,
            Self::Parse { .. } => Cause::Parse,
            Self::YamlBudget { .. } => Cause::YamlBudget,
            Self::Io { .. } => Cause::Io,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_has_its_own_cause() {
        let path = || PathBuf::from("f");
        let errors = [
            LockfileError::NotRegularFile { path: path() },
            LockfileError::TooLarge {
                path: path(),
                size: 2,
                limit: 1,
            },
            LockfileError::PathEscapesRoot {
                file: path(),
                member: "..".to_owned(),
            },
            LockfileError::GlobRefused {
                file: path(),
                pattern: "**".to_owned(),
            },
            LockfileError::FileLimit {
                path: path(),
                limit: 64,
            },
            LockfileError::TooManyMembers {
                file: path(),
                limit: 64,
            },
            LockfileError::Parse {
                file: path(),
                line: Some(3),
                message: "bad".to_owned(),
            },
            LockfileError::YamlBudget { file: path() },
            LockfileError::Io {
                path: path(),
                source: std::io::Error::other("denied"),
            },
        ];
        let causes: Vec<Cause> = errors.iter().map(LockfileError::cause).collect();
        assert_eq!(
            causes,
            [
                Cause::NotRegularFile,
                Cause::TooLarge,
                Cause::PathEscapesRoot,
                Cause::GlobRefused,
                Cause::FileLimit,
                Cause::TooManyMembers,
                Cause::Parse,
                Cause::YamlBudget,
                Cause::Io,
            ]
        );
        assert_eq!(errors[6].to_string(), "f:3: bad");
    }
}
