//! Core types for zenodotus: validated identifiers, the configuration file model and the domain model shared by
//! ingestion, search and the MCP server.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod config;
pub mod config_edit;
pub mod ecosystem;
pub mod error;
pub mod fences;
pub mod ident;
pub mod ids;
pub mod model;
pub mod plan;
pub mod runtime;
pub mod time;
pub mod version;

pub use ecosystem::Ecosystem;
pub use error::CoreError;
pub use ids::{CanonicalId, ChunkId, DocKey, LibraryId, LibraryRef, Scope, Version};
pub use runtime::Runtime;
