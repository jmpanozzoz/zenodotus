//! The stdio and Streamable HTTP transports.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rmcp::ServiceExt;
use rmcp::service::RoleServer;
use rmcp::transport::IntoTransport;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower::limit::ConcurrencyLimitLayer;
use zenodotus_core::config::HttpSettings;

use crate::error::ServeError;
use crate::server::DocsServer;

/// The path the MCP endpoint is served on.
pub const MCP_PATH: &str = "/mcp";
/// `Host` values always accepted.
pub const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// Serves `server` over `transport` until the client disconnects.
///
/// # Errors
///
/// [`ServeError::Initialize`] when the handshake fails, or [`ServeError::Session`] when the session task panics.
pub async fn serve_transport<T, E, A>(server: DocsServer, transport: T) -> Result<(), ServeError>
where
    T: IntoTransport<RoleServer, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    server.serve(transport).await?.waiting().await?;
    Ok(())
}

/// Serves `server` over stdin and stdout. Nothing else may write to stdout.
///
/// # Errors
///
/// As [`serve_transport`].
pub async fn serve_stdio(server: DocsServer) -> Result<(), ServeError> {
    serve_transport(server, rmcp::transport::stdio()).await
}

/// How the Streamable HTTP transport listens and whom it serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpOptions {
    /// The address to listen on.
    pub listen: SocketAddr,
    /// `Host` values accepted besides [`LOOPBACK_HOSTS`].
    pub allowed_hosts: Vec<String>,
    /// `Origin` values accepted. A request with any other `Origin` is refused, every one when empty; a request
    /// without one, as MCP clients outside a browser send, passes.
    pub allowed_origins: Vec<String>,
    /// Largest request body, in bytes.
    pub max_body_bytes: usize,
    /// Most requests served at once.
    pub max_in_flight: usize,
    /// The bearer token every request must carry, if any.
    pub token: Option<String>,
}

impl HttpOptions {
    /// Options from `[settings.http]`, listening on `listen` when given.
    #[must_use]
    pub fn new(settings: &HttpSettings, listen: Option<SocketAddr>, token: Option<String>) -> Self {
        Self {
            listen: listen.unwrap_or(settings.listen),
            allowed_hosts: settings.allowed_hosts.clone(),
            allowed_origins: settings.allowed_origins.clone(),
            max_body_bytes: usize::try_from(settings.max_body_bytes).unwrap_or(usize::MAX),
            max_in_flight: usize::try_from(settings.max_in_flight).unwrap_or(usize::MAX),
            token: token.filter(|token| !token.is_empty()),
        }
    }
}

/// A bound Streamable HTTP server, not yet serving.
#[derive(Debug)]
pub struct HttpServer {
    listener: TcpListener,
    router: Router,
    shutdown: CancellationToken,
}

impl HttpServer {
    /// Binds `options.listen` for `server`; cancelling `shutdown` stops it gracefully.
    ///
    /// # Errors
    ///
    /// [`ServeError::InsecureBind`] for a non-loopback address without a token, and [`ServeError::Bind`] when the
    /// socket cannot be opened.
    pub async fn bind(
        options: HttpOptions,
        server: DocsServer,
        shutdown: CancellationToken,
    ) -> Result<Self, ServeError> {
        let addr = options.listen;
        if !addr.ip().is_loopback() && options.token.is_none() {
            return Err(ServeError::InsecureBind { addr });
        }
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|source| ServeError::Bind { addr, source })?;
        Ok(Self {
            listener,
            router: router(options, server, &shutdown),
            shutdown,
        })
    }

    /// The address the server listens on.
    ///
    /// # Errors
    ///
    /// [`ServeError::Io`] when the socket cannot report it.
    pub fn local_addr(&self) -> Result<SocketAddr, ServeError> {
        Ok(self.listener.local_addr()?)
    }

    /// Serves requests until `shutdown` is cancelled, then lets requests in flight finish.
    ///
    /// # Errors
    ///
    /// [`ServeError::Io`] when the server fails.
    pub async fn run(self) -> Result<(), ServeError> {
        let addr = self.listener.local_addr()?;
        tracing::info!(%addr, path = MCP_PATH, "serving MCP over Streamable HTTP");
        axum::serve(self.listener, self.router)
            .with_graceful_shutdown(self.shutdown.cancelled_owned())
            .await?;
        Ok(())
    }
}

/// The router: the MCP endpoint behind the concurrency limit and, with a token, the bearer check.
fn router(options: HttpOptions, server: DocsServer, shutdown: &CancellationToken) -> Router {
    let hosts = LOOPBACK_HOSTS
        .iter()
        .map(ToString::to_string)
        .chain(options.allowed_hosts);
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_allowed_hosts(hosts)
        .with_allowed_origins(options.allowed_origins)
        .enforce_origin_validation()
        .with_max_request_body_bytes(options.max_body_bytes)
        .with_cancellation_token(shutdown.child_token());
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    let router = Router::new()
        .nest_service(MCP_PATH, service)
        .layer(ConcurrencyLimitLayer::new(options.max_in_flight));
    match options.token {
        Some(token) => router.layer(middleware::from_fn_with_state(Arc::<str>::from(token), require_token)),
        None => router,
    }
}

/// Lets a request through only if it carries `Authorization: Bearer <token>`; the comparison takes the same time
/// wherever the first difference is.
async fn require_token(State(token): State<Arc<str>>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if presented.is_some_and(|presented| bool::from(presented.as_bytes().ct_eq(token.as_bytes()))) {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(WWW_AUTHENTICATE, "Bearer")],
        "missing or invalid bearer token",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode as Status;
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    use tempfile::TempDir;
    use tokio::io::AsyncWriteExt;
    use tokio::task::JoinHandle;

    use super::*;
    use crate::fixture::indexed;

    /// Extra request headers.
    type Headers<'a> = &'a [(&'a str, &'a [u8])];

    const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#;

    struct Running {
        _dir: TempDir,
        addr: SocketAddr,
        shutdown: CancellationToken,
        task: JoinHandle<Result<(), ServeError>>,
    }

    fn options() -> HttpOptions {
        let settings = HttpSettings {
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            ..HttpSettings::default()
        };
        HttpOptions::new(&settings, None, None)
    }

    async fn start(options: HttpOptions) -> Running {
        let (dir, server) = indexed();
        let shutdown = CancellationToken::new();
        let http = HttpServer::bind(options, server, shutdown.clone()).await.unwrap();
        let addr = http.local_addr().unwrap();
        Running {
            _dir: dir,
            addr,
            shutdown,
            task: tokio::spawn(http.run()),
        }
    }

    /// POSTs an `initialize` request with `headers` and returns the status.
    async fn initialize(running: &Running, headers: Headers<'_>, body: &str) -> Status {
        let mut request = reqwest::Client::new()
            .post(format!("http://{}{MCP_PATH}", running.addr))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(body.to_owned());
        for (name, value) in headers {
            request = request.header(*name, reqwest::header::HeaderValue::from_bytes(value).unwrap());
        }
        request.send().await.unwrap().status()
    }

    #[tokio::test]
    async fn an_mcp_client_lists_and_calls_both_tools_over_http() {
        let mut options = options();
        options.token = Some("secret".to_owned());
        let running = start(options).await;
        let config = StreamableHttpClientTransportConfig::with_uri(format!("http://{}{MCP_PATH}", running.addr))
            .auth_header("secret");
        let client = ().serve(StreamableHttpClientTransport::from_config(config)).await.unwrap();
        assert_eq!(client.list_all_tools().await.unwrap().len(), 2);
        let arguments = serde_json::json!({"library_id": "axum", "query": "router"});
        let result = client
            .call_tool(CallToolRequestParams::new("query_docs").with_arguments(arguments.as_object().unwrap().clone()))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false));
        assert!(result.content[0].as_text().unwrap().text.contains("Build a Router"));
        client.cancel().await.unwrap();
        running.shutdown.cancel();
        running.task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn requests_without_the_bearer_token_are_rejected() {
        let mut options = options();
        options.token = Some("secret".to_owned());
        let running = start(options).await;
        for headers in [
            &[][..],
            &[("authorization", &b"Bearer wrong"[..])][..],
            &[("authorization", &b"Bearer secret2"[..])][..],
            &[("authorization", &b"Basic secret"[..])][..],
            &[("authorization", &b"Bearer \xff"[..])][..],
        ] {
            assert_eq!(initialize(&running, headers, INITIALIZE).await, Status::UNAUTHORIZED);
        }
        let response = reqwest::Client::new()
            .post(format!("http://{}{MCP_PATH}", running.addr))
            .send()
            .await
            .unwrap();
        assert_eq!(response.headers()["www-authenticate"], "Bearer");
        let authorized = [("authorization", &b"Bearer secret"[..])];
        assert_eq!(initialize(&running, &authorized, INITIALIZE).await, Status::OK);
    }

    #[tokio::test]
    async fn foreign_hosts_and_origins_are_rejected() {
        let mut options = options();
        options.allowed_hosts = vec!["docs.example".to_owned()];
        options.allowed_origins = vec!["https://app.example:*".to_owned()];
        let running = start(options).await;
        let cases: [(Headers, Status); 5] = [
            (&[], Status::OK),
            (&[("host", b"docs.example")], Status::OK),
            (&[("host", b"evil.example")], Status::FORBIDDEN),
            (&[("origin", b"https://app.example")], Status::OK),
            (&[("origin", b"https://evil.example")], Status::FORBIDDEN),
        ];
        for (headers, status) in cases {
            assert_eq!(initialize(&running, headers, INITIALIZE).await, status, "{headers:?}");
        }
    }

    #[tokio::test]
    async fn with_no_allowed_origins_a_request_from_a_browser_page_is_rejected() {
        let running = start(options()).await;
        assert_eq!(initialize(&running, &[], INITIALIZE).await, Status::OK);
        let origin: Headers = &[("origin", b"https://evil.example")];
        assert_eq!(initialize(&running, origin, INITIALIZE).await, Status::FORBIDDEN);
    }

    #[tokio::test]
    async fn oversized_bodies_are_rejected() {
        let mut options = options();
        options.max_body_bytes = 1024;
        options.max_in_flight = 1;
        let running = start(options).await;
        let padded = INITIALIZE.replace("\"test\"", &format!("\"{}\"", "x".repeat(1024)));
        assert_eq!(initialize(&running, &[], &padded).await, Status::PAYLOAD_TOO_LARGE);
        assert_eq!(initialize(&running, &[], INITIALIZE).await, Status::OK);
    }

    #[tokio::test]
    async fn a_non_loopback_bind_needs_a_token() {
        let (_dir, server) = indexed();
        let mut options = options();
        options.listen = SocketAddr::from(([0, 0, 0, 0], 0));
        let error = HttpServer::bind(options.clone(), server.clone(), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(error, ServeError::InsecureBind { .. }), "{error}");
        assert!(error.to_string().contains("set ZENODOTUS_HTTP_TOKEN"), "{error}");
        options.token = Some("secret".to_owned());
        let http = HttpServer::bind(options, server, CancellationToken::new())
            .await
            .unwrap();
        assert!(http.local_addr().unwrap().ip().is_unspecified());
    }

    #[tokio::test]
    async fn a_busy_port_is_a_bind_error() {
        let (_dir, server) = indexed();
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut options = options();
        options.listen = taken.local_addr().unwrap();
        let error = HttpServer::bind(options, server, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(error, ServeError::Bind { .. }), "{error}");
        assert!(error.to_string().starts_with("cannot listen on 127.0.0.1:"), "{error}");
    }

    #[test]
    fn options_come_from_the_settings() {
        let settings = HttpSettings {
            allowed_hosts: vec!["a.example".to_owned()],
            allowed_origins: vec!["https://b.example".to_owned()],
            ..HttpSettings::default()
        };
        let listen = SocketAddr::from(([127, 0, 0, 1], 9000));
        assert_eq!(
            HttpOptions::new(&settings, Some(listen), Some(String::new())),
            HttpOptions {
                listen,
                allowed_hosts: vec!["a.example".to_owned()],
                allowed_origins: vec!["https://b.example".to_owned()],
                max_body_bytes: 65_536,
                max_in_flight: 64,
                token: None,
            }
        );
        let defaults = HttpOptions::new(&settings, None, Some("t".to_owned()));
        assert_eq!(
            (defaults.listen, defaults.token),
            (settings.listen, Some("t".to_owned()))
        );
    }

    #[tokio::test]
    async fn a_failed_handshake_is_an_initialize_error() {
        let (_dir, server) = indexed();
        let (server_side, mut client_side) = tokio::io::duplex(1024);
        client_side.write_all(b"not json\n").await.unwrap();
        drop(client_side);
        let error = serve_transport(server, server_side).await.unwrap_err();
        assert!(matches!(error, ServeError::Initialize(_)), "{error}");
        assert!(
            error.to_string().starts_with("the MCP session did not start: "),
            "{error}"
        );
    }

    #[tokio::test]
    async fn task_and_io_failures_are_reported() {
        let panicked = tokio::spawn(async { panic!("boom") }).await.unwrap_err();
        let error = ServeError::from(panicked);
        assert!(matches!(error, ServeError::Session(_)), "{error}");
        assert!(error.to_string().starts_with("the MCP session ended abnormally: "));
        let error = ServeError::from(std::io::Error::other("reset"));
        assert!(matches!(error, ServeError::Io(_)), "{error}");
        assert_eq!(error.to_string(), "the HTTP server failed: reset");
    }
}
