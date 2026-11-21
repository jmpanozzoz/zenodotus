//! Ingestion for zenodotus: sources, Markdown parsing, sectioning and change detection. Never touches storage: every
//! collector returns a [`zenodotus_core::model::VersionChanges`] for the search crate to apply.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod document;
pub mod error;
pub mod fetch;
pub mod github;
pub mod guard;
pub mod html;
pub mod llms_txt;
pub mod local;
pub mod markdown;
pub mod retry;
pub mod robots;
pub mod sectioner;
pub mod sitemap;
pub mod source;
pub mod symbols;
pub mod url;
pub mod website;

pub use error::IngestError;
