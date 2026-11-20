//! Ingestion for zenodotus: sources, Markdown parsing, sectioning and change detection. Never touches storage: every
//! collector returns a [`zenodotus_core::model::VersionChanges`] for the search crate to apply.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod document;
pub mod error;
pub mod local;
pub mod markdown;
pub mod sectioner;
pub mod symbols;

pub use error::IngestError;
