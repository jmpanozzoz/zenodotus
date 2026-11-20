//! Errors raised by the core crate.

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
}
