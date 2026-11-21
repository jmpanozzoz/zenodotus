//! Core types for zenodotus: validated identifiers, the configuration file model and the domain model shared by
//! ingestion, search and the MCP server.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod config;
pub mod config_edit;
pub mod error;
pub mod fences;
pub mod ident;
pub mod ids;
pub mod model;
pub mod time;

pub use error::CoreError;
pub use ids::{ChunkId, DocKey, LibraryId, Scope, Version};
