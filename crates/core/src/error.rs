//! Errors raised by the core crate.

use std::path::PathBuf;

/// Everything that can go wrong while validating identifiers or loading and editing the configuration.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// A library id breaks the slug rules.
    #[error("invalid library id {value:?}: {reason}")]
    InvalidLibraryId {
        /// The rejected value.
        value: String,
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// A canonical ID breaks the grammar of design §6.2.
    #[error("invalid canonical id {value:?}: {reason}")]
    InvalidCanonicalId {
        /// The rejected value.
        value: String,
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// A stored plan fails to parse, names an unknown source kind or does not fit a source (design §12.5).
    #[error("invalid plan: {reason}")]
    InvalidPlan {
        /// What is wrong with it.
        reason: String,
    },
    /// A version string breaks the version rules.
    #[error("invalid version {value:?}: {reason}")]
    InvalidVersion {
        /// The rejected value.
        value: String,
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// A document path cannot be used in a key.
    #[error("invalid document path {value:?}: {reason}")]
    InvalidDocPath {
        /// The rejected value.
        value: String,
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// The configuration file could not be read or written.
    #[error("cannot access config file {path}: {source}")]
    ConfigIo {
        /// The file involved.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The configuration file is not valid TOML or does not match the schema.
    #[error("invalid config file {path}: {message}")]
    ConfigParse {
        /// The file involved.
        path: PathBuf,
        /// What the parser reported, with the line and column.
        message: String,
    },
    /// `init` found a config file already in place.
    #[error("config file {path} already exists; edit it or pass another --config")]
    ConfigExists {
        /// The existing file.
        path: PathBuf,
    },
    /// Two `[[library]]` entries share an id.
    #[error("library {library:?} is configured twice")]
    DuplicateLibrary {
        /// The repeated id.
        library: String,
    },
    /// A library lists the same version twice.
    #[error("library {library:?} lists version {version:?} twice")]
    DuplicateVersion {
        /// The library.
        library: String,
        /// The repeated version.
        version: String,
    },
    /// More than one version of a library is marked `default = true`.
    #[error("library {library:?} marks more than one version as default")]
    MultipleDefaults {
        /// The library.
        library: String,
    },
    /// A library has no `[[library.version]]` entry.
    #[error("library {library:?} has no versions")]
    NoVersions {
        /// The library.
        library: String,
    },
    /// A version's source breaks a rule.
    #[error("library {library:?} version {version:?}: invalid source: {reason}")]
    InvalidSource {
        /// The library.
        library: String,
        /// The version.
        version: String,
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// A `[settings]` value is out of range.
    #[error("invalid setting `{field}`: {reason}")]
    InvalidSettings {
        /// The setting, such as `ranking.symbol_boost`.
        field: &'static str,
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// A command named a library that the config does not contain.
    #[error("library {library:?} is not in the config; run `zenodotus list` to see the configured libraries")]
    UnknownLibrary {
        /// The requested id.
        library: String,
    },
    /// No location could be derived for a file or directory, because the environment variables it falls back to are
    /// unset.
    #[error("cannot locate the {what}: pass it explicitly or set HOME")]
    NoLocation {
        /// What was being located.
        what: &'static str,
    },
}
