//! The `zenodotus` command: configure libraries, sync them into the index, search it and serve it over MCP.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod app;
pub mod args;
pub mod error;
pub mod output;
pub mod setup;

pub use error::AppError;
