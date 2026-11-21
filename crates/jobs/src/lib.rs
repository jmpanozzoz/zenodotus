//! The single index writer of zenodotus: the sync lock, sync jobs and the document sink that streams into the store.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod error;
pub mod lock;
pub mod sync;

pub use error::JobsError;
