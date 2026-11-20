//! Core types for zenodotus: validated identifiers, the configuration file model and the domain model shared by
//! ingestion, search and the MCP server.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod error;
pub mod ids;

pub use error::CoreError;
pub use ids::{ChunkId, DocKey, LibraryId, Scope, Version};
