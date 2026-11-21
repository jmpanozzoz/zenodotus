//! Errors of the evaluation harness.

use std::path::{Path, PathBuf};

use zenodotus_core::error::CoreError;
use zenodotus_search::error::SearchError;

use crate::questions::Set;

/// Why an evaluation command could not run.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    /// A file or directory could not be read or written.
    #[error("cannot access {}: {source}", path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// The cause.
        #[source]
        source: std::io::Error,
    },
    /// A line of a question set, or `context7.toml`, is malformed.
    #[error("{}:{line}: {message}", path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// The line, from 1; 0 for a whole-file error.
        line: usize,
        /// What is wrong.
        message: String,
    },
    /// The corpus config could not be loaded.
    #[error(transparent)]
    Config(#[from] CoreError),
    /// The synced index could not be read.
    #[error(transparent)]
    Search(#[from] SearchError),
    /// Two questions share an id.
    #[error("question {0} appears more than once")]
    DuplicateId(String),
    /// A question sits in the other set than the one its id hashes to.
    #[error("question {id} is in the {found} set, but its id hashes to the {expected} set")]
    WrongSet {
        /// The question.
        id: String,
        /// The file it is in.
        found: Set,
        /// The set its id hashes to.
        expected: Set,
    },
    /// A question names a library version the corpus does not have.
    #[error("question {id}: {scope} is not a library version of the corpus")]
    UnknownScope {
        /// The question.
        id: String,
        /// `library` or `library@version`.
        scope: String,
    },
    /// A corpus version is not a GitHub source with exactly one path.
    #[error("{0}: every corpus version must be a GitHub source with exactly one path")]
    UnsupportedSource(String),
    /// `validate` rejected some questions.
    #[error("{0} question(s) failed validation")]
    Invalid(usize),
    /// A performance config does not list the library to measure.
    #[error("{library} is not a library of {}", config.display())]
    NoLibrary {
        /// The config.
        config: PathBuf,
        /// The library id.
        library: String,
    },
    /// The zenodotus server under measurement could not start or failed a call.
    #[error("the zenodotus server failed: {0}")]
    Server(String),
}

impl EvalError {
    /// A function that wraps an I/O error on `path`.
    pub fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Self + '_ {
        move |source| Self::Io {
            path: path.to_owned(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_names_what_went_wrong() {
        let path = Path::new("eval/questions/dev.jsonl");
        let cases = [
            (
                EvalError::io(path)(std::io::Error::other("denied")),
                "cannot access eval/questions/dev.jsonl: denied",
            ),
            (
                EvalError::Parse {
                    path: path.to_owned(),
                    line: 3,
                    message: "expected value".to_owned(),
                },
                "eval/questions/dev.jsonl:3: expected value",
            ),
            (
                EvalError::Config(CoreError::InvalidSettings {
                    field: "user_agent",
                    reason: "must not be empty",
                }),
                "invalid setting `user_agent`: must not be empty",
            ),
            (
                EvalError::Search(SearchError::IndexEmpty),
                "the index is empty; run `zenodotus sync` first",
            ),
            (
                EvalError::DuplicateId("vite-001".to_owned()),
                "question vite-001 appears more than once",
            ),
            (
                EvalError::WrongSet {
                    id: "vite-001".to_owned(),
                    found: Set::Test,
                    expected: Set::Dev,
                },
                "question vite-001 is in the test set, but its id hashes to the dev set",
            ),
            (
                EvalError::UnknownScope {
                    id: "vite-001".to_owned(),
                    scope: "vite@9".to_owned(),
                },
                "question vite-001: vite@9 is not a library version of the corpus",
            ),
            (
                EvalError::UnsupportedSource("vite@latest".to_owned()),
                "vite@latest: every corpus version must be a GitHub source with exactly one path",
            ),
            (EvalError::Invalid(2), "2 question(s) failed validation"),
            (
                EvalError::NoLibrary {
                    config: PathBuf::from("target/perf/zenodotus.toml"),
                    library: "mdn".to_owned(),
                },
                "mdn is not a library of target/perf/zenodotus.toml",
            ),
            (
                EvalError::Server("query_docs: timeout".to_owned()),
                "the zenodotus server failed: query_docs: timeout",
            ),
        ];
        for (error, message) in cases {
            assert_eq!(error.to_string(), message);
        }
    }
}
