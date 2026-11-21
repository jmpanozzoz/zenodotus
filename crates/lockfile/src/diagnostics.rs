//! Diagnostics returned with a project's dependencies (design §8.6). They are not errors: the project still
//! resolves.

use std::path::PathBuf;

use serde::Serialize;

use crate::error::LockfileError;

/// What a diagnostic reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticKind {
    /// A git dependency on a host other than `github.com`; it is skipped.
    UnsupportedGitHost,
    /// A dependency from a private registry or under `private_scopes`; it is never sent anywhere.
    PrivateDependency,
    /// A file or member that was not read: not a regular file, too large, a path escape, a refused glob or a limit.
    SkippedFile,
    /// A file that could not be parsed.
    ParseError,
}

/// One diagnostic: `{file, line?, kind, name?, message}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    /// The file concerned, relative to the root.
    pub file: PathBuf,
    /// The 1-based line, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    /// What happened.
    pub kind: DiagnosticKind,
    /// The dependency concerned, as written in the file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A human-readable explanation.
    pub message: String,
}

impl Diagnostic {
    /// A diagnostic about the dependency `name` of `file`.
    pub(crate) fn dependency(file: PathBuf, kind: DiagnosticKind, name: &str, message: String) -> Self {
        Self {
            file,
            line: None,
            kind,
            name: Some(name.to_owned()),
            message,
        }
    }
}

impl From<LockfileError> for Diagnostic {
    fn from(error: LockfileError) -> Self {
        let message = error.to_string();
        let (file, line, kind) = match error {
            LockfileError::NotRegularFile { path }
            | LockfileError::TooLarge { path, .. }
            | LockfileError::FileLimit { path, .. }
            | LockfileError::Io { path, .. } => (path, None, DiagnosticKind::SkippedFile),
            LockfileError::PathEscapesRoot { file, .. }
            | LockfileError::GlobRefused { file, .. }
            | LockfileError::TooManyMembers { file, .. } => (file, None, DiagnosticKind::SkippedFile),
            LockfileError::Parse { file, line, .. } => (file, line, DiagnosticKind::ParseError),
            LockfileError::YamlBudget { file } => (file, None, DiagnosticKind::ParseError),
        };
        Self {
            file,
            line,
            kind,
            name: None,
            message,
        }
    }
}
