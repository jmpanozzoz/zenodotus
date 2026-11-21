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

pub use error::SearchError;
