//! The MCP server of zenodotus: the `resolve_library` and `query_docs` tools, served over stdio or Streamable HTTP.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod error;
#[cfg(test)]
mod fixture;
pub mod render;
pub mod server;
pub mod transport;

pub use error::ServeError;
pub use server::{DocsServer, Index};
