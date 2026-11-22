//! Tantivy-backed storage, ranking and retrieval for zenodotus.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod analyzer;
pub mod error;
pub mod query;
pub mod render_model;
pub mod resolve;
pub mod schema;
pub mod store;
#[cfg(feature = "test-support")]
pub mod v1_index;

pub use error::SearchError;
