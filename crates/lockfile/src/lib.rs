//! Reads a project's lockfiles, manifests and runtime version files as untrusted data (design §7.7 and §8).
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod diagnostics;
pub mod error;
mod formats;
mod private;
mod read;

pub use diagnostics::{Diagnostic, DiagnosticKind};
pub use error::LockfileError;

use serde::Serialize;
use zenodotus_core::Ecosystem;

/// One locked or declared package of a project.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Dependency {
    /// Its ecosystem; a git dependency on GitHub is [`Ecosystem::Github`].
    pub ecosystem: Ecosystem,
    /// The name part of its canonical ID (design §6.2).
    pub name: String,
    /// The locked version, or the commit of a git dependency; `None` for a manifest entry, which resolves to the
    /// latest stable version.
    pub version: Option<String>,
    // direct: pending design draft 6
}

impl Dependency {
    /// The canonical ID, `<prefix>:<name>`.
    #[must_use]
    pub fn canonical_id(&self) -> String {
        format!("{}:{}", self.ecosystem.prefix(), self.name)
    }
}

/// What the caller knows about private names (design §8.5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// `settings.private_scopes`: canonical-ID prefixes such as `"npm:@acme/"`.
    pub private_scopes: Vec<String>,
    /// The patterns of `GOPRIVATE` and `GONOPROXY`.
    pub go_private: Vec<String>,
}

impl Options {
    /// Adds the comma-separated patterns of a `GOPRIVATE` or `GONOPROXY` value.
    pub fn add_go_patterns(&mut self, value: &str) {
        let patterns = value.split(',').map(str::trim).filter(|pattern| !pattern.is_empty());
        self.go_private.extend(patterns.map(str::to_owned));
    }
}

/// One package as a parser found it, before names are canonicalized and private ones removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    /// The ecosystem of the file.
    pub(crate) ecosystem: Ecosystem,
    /// The name as written.
    pub(crate) name: String,
    /// The version, when the file locks one.
    pub(crate) version: Option<String>,
    /// Where it comes from.
    pub(crate) source: Source,
}

impl Entry {
    /// A package from its ecosystem's public registry.
    pub(crate) fn registry(ecosystem: Ecosystem, name: &str, version: Option<&str>) -> Self {
        Self {
            ecosystem,
            name: name.to_owned(),
            version: version.map(str::to_owned),
            source: Source::Registry,
        }
    }

    /// A package pinned to `commit` of the git repository at `url`.
    pub(crate) fn git(ecosystem: Ecosystem, name: &str, url: &str, commit: &str) -> Self {
        Self {
            ecosystem,
            name: name.to_owned(),
            version: None,
            source: Source::Git {
                url: url.to_owned(),
                commit: commit.to_owned(),
            },
        }
    }
}

/// Where a package comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    /// The ecosystem's public registry.
    Registry,
    /// Another registry; the package is private.
    Private {
        /// The registry or download URL.
        url: String,
    },
    /// A git repository at a commit.
    Git {
        /// The repository URL.
        url: String,
        /// The commit.
        commit: String,
    },
}
