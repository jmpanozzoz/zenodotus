//! Why serving failed.

use std::net::SocketAddr;

/// A failure to start or run a transport.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// A non-loopback address was given without a bearer token.
    #[error(
        "refusing to serve on {addr} without authentication: set ZENODOTUS_HTTP_TOKEN, or listen on a loopback \
         address such as 127.0.0.1"
    )]
    InsecureBind {
        /// The requested address.
        addr: SocketAddr,
    },
    /// The listening socket could not be opened.
    #[error("cannot listen on {addr}: {source}")]
    Bind {
        /// The requested address.
        addr: SocketAddr,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The HTTP server failed while running.
    #[error("the HTTP server failed: {0}")]
    Io(#[from] std::io::Error),
    /// The client did not complete the MCP handshake.
    #[error("the MCP session did not start: {0}")]
    Initialize(Box<rmcp::service::ServerInitializeError>),
    /// The task running the MCP session ended abnormally.
    #[error("the MCP session ended abnormally: {0}")]
    Session(#[from] tokio::task::JoinError),
}

impl From<rmcp::service::ServerInitializeError> for ServeError {
    fn from(error: rmcp::service::ServerInitializeError) -> Self {
        Self::Initialize(Box::new(error))
    }
}
