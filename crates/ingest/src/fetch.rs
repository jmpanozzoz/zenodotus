//! Every network request goes through [`Fetcher`]: it checks the scheme, the host and the resolved address of each
//! hop, follows redirects itself, retries transient failures, spaces requests per host, sends conditional headers and
//! caps every body while it streams.

use std::collections::BTreeSet;
use std::error::Error as _;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use reqwest::header::{
    CONTENT_TYPE, ETAG, HeaderMap, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, LOCATION, RETRY_AFTER,
};
use reqwest::{Response, StatusCode, redirect};
use url::{Host, Url};
use zenodotus_core::config::FetchSettings;
use zenodotus_core::model::{DocError, DocState};

use crate::error::IngestError;
use crate::guard::{Blocked, GuardedResolver, Resolve, is_blocked};
use crate::retry::{HostSchedule, RetryPolicy, parse_retry_after};

/// The most redirects one fetch follows.
pub const MAX_REDIRECTS: usize = 5;

/// The hosts a source may reach: those of its own URLs plus its `extra_hosts`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostSet(BTreeSet<String>);

impl HostSet {
    /// The hosts of `urls` plus `extra`, lowercased.
    #[must_use]
    pub fn of(urls: &[&Url], extra: &[String]) -> Self {
        let hosts = urls
            .iter()
            .filter_map(|url| url.host_str())
            .chain(extra.iter().map(String::as_str));
        Self(hosts.map(str::to_ascii_lowercase).collect())
    }

    /// Whether `host` may be reached.
    #[must_use]
    pub fn contains(&self, host: &str) -> bool {
        self.0.contains(&host.to_ascii_lowercase())
    }
}

/// Why one document could not be fetched. The document keeps its previous content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// Only `https` may be fetched, and `http` when `fetch.allow_http` is set.
    #[error("the scheme is not allowed; only https URLs are fetched")]
    Scheme {
        /// The URL.
        url: String,
    },
    /// The host is neither one of the source's own URLs nor in its `extra_hosts`.
    #[error("the host is not one this source may reach")]
    HostNotAllowed {
        /// The URL.
        url: String,
    },
    /// A redirect leads to a host that is neither one of the source's own URLs nor in its `extra_hosts`. A crawl
    /// treats it as leaving its scope; every other caller, as [`FetchError::HostNotAllowed`].
    #[error("redirects to a host this source may not reach")]
    RedirectedAway {
        /// The redirect target.
        url: Url,
        /// Whether every redirect followed was permanent (301 or 308).
        permanent: bool,
    },
    /// The host resolves only to private, loopback or other blocked addresses.
    #[error("the host resolves only to blocked addresses")]
    BlockedAddress {
        /// The URL.
        url: String,
    },
    /// More than [`MAX_REDIRECTS`] redirects.
    #[error("more than {MAX_REDIRECTS} redirects")]
    TooManyRedirects {
        /// The first URL.
        url: String,
    },
    /// The body exceeded its size limit.
    #[error("larger than {limit} bytes")]
    TooLarge {
        /// The URL.
        url: String,
        /// The limit.
        limit: u64,
    },
    /// An HTTP status that is neither success, redirect, 304, 404 nor 410, a 429 or 5xx after every retry, or a 404
    /// or 410 reached through a temporary redirect.
    #[error("HTTP status {status}")]
    Status {
        /// The URL.
        url: String,
        /// The status code.
        status: u16,
    },
    /// The request timed out after every retry.
    #[error("timed out")]
    Timeout {
        /// The URL.
        url: String,
    },
    /// A connection or protocol error.
    #[error("{message}")]
    Network {
        /// The URL.
        url: String,
        /// The error and its causes.
        message: String,
    },
    /// The site's robots.txt disallows the page.
    #[error("disallowed by robots.txt")]
    RobotsDisallowed {
        /// The URL.
        url: String,
    },
    /// The body is not valid UTF-8, or not in the expected format.
    #[error("{message}")]
    Decode {
        /// The URL.
        url: String,
        /// What was wrong.
        message: String,
    },
}

impl FetchError {
    /// The URL the error is about.
    #[must_use]
    pub fn url(&self) -> &str {
        match self {
            Self::Scheme { url }
            | Self::HostNotAllowed { url }
            | Self::BlockedAddress { url }
            | Self::TooManyRedirects { url }
            | Self::TooLarge { url, .. }
            | Self::Status { url, .. }
            | Self::Timeout { url }
            | Self::Network { url, .. }
            | Self::RobotsDisallowed { url }
            | Self::Decode { url, .. } => url,
            Self::RedirectedAway { url, .. } => url.as_str(),
        }
    }

    /// The error kind in `snake_case`, as stored in [`DocError::kind`].
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Scheme { .. } => "scheme",
            Self::HostNotAllowed { .. } | Self::RedirectedAway { .. } => "host_not_allowed",
            Self::BlockedAddress { .. } => "blocked_address",
            Self::TooManyRedirects { .. } => "too_many_redirects",
            Self::TooLarge { .. } => "too_large",
            Self::Status { .. } => "status",
            Self::Timeout { .. } => "timeout",
            Self::Network { .. } => "network",
            Self::RobotsDisallowed { .. } => "robots_disallowed",
            Self::Decode { .. } => "decode",
        }
    }

    /// The error as a document error of `doc_path`.
    #[must_use]
    pub fn doc_error(&self, doc_path: &str) -> DocError {
        DocError {
            doc_path: doc_path.to_owned(),
            kind: self.kind().to_owned(),
            message: format!("{}: {self}", self.url()),
        }
    }
}

/// The `ETag` and `Last-Modified` of a stored document, sent back as `If-None-Match` and `If-Modified-Since`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Validators {
    /// The `ETag` header.
    pub etag: Option<String>,
    /// The `Last-Modified` header.
    pub last_modified: Option<String>,
}

impl Validators {
    /// The validators stored with `state`.
    #[must_use]
    pub fn of(state: &DocState) -> Self {
        Self {
            etag: state.etag.clone(),
            last_modified: state.last_modified.clone(),
        }
    }

    fn from_headers(headers: &HeaderMap) -> Self {
        let text = |name| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        Self {
            etag: text(ETAG),
            last_modified: text(LAST_MODIFIED),
        }
    }
}

/// One fetch.
#[derive(Debug, Clone, Copy)]
pub struct FetchRequest<'a> {
    /// What to fetch.
    pub url: &'a Url,
    /// The hosts this fetch and its redirects may reach.
    pub allowed_hosts: &'a HostSet,
    /// The largest body accepted, counted after decoding.
    pub max_bytes: u64,
    /// Validators for a conditional request.
    pub validators: Option<&'a Validators>,
    /// Whether a redirect target gets the validators too: true when the document keeps its key across redirects
    /// (configured URLs, `llms.txt` links), false when it is keyed by the URL that serves it (crawled pages), where
    /// a target is another document.
    pub validators_follow_redirects: bool,
    /// The timeout of each attempt, body included.
    pub timeout: Duration,
    /// Extra request headers, sent only to the origin of `url`.
    pub headers: &'a [(&'static str, String)],
}

/// A fetched body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Body {
    /// The bytes.
    pub bytes: Vec<u8>,
    /// The URL after redirects.
    pub final_url: Url,
    /// Whether every redirect followed was permanent (301 or 308); true when none was. Only a permanent redirect
    /// shows that a document moved.
    pub permanent: bool,
    /// The response validators.
    pub validators: Validators,
    /// The `Content-Type` header.
    pub content_type: Option<String>,
}

impl Body {
    /// The body as UTF-8 text.
    ///
    /// # Errors
    /// [`FetchError::Decode`] when the body is not valid UTF-8.
    pub fn text(&self) -> Result<&str, FetchError> {
        std::str::from_utf8(&self.bytes).map_err(|error| FetchError::Decode {
            url: self.final_url.to_string(),
            message: format!("the body is not UTF-8: {error}"),
        })
    }

    /// Whether the `Content-Type` is HTML.
    #[must_use]
    pub fn is_html(&self) -> bool {
        self.content_type.as_deref().is_some_and(|value| {
            let value = value.trim_start().to_ascii_lowercase();
            value.starts_with("text/html") || value.starts_with("application/xhtml")
        })
    }
}

/// What a fetch found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchResponse {
    /// 304: the stored validators are current.
    NotModified,
    /// 404 or 410, directly or through permanent redirects only: the document is gone.
    Gone,
    /// The body.
    Body(Body),
}

/// An opened response whose body is read chunk by chunk.
#[derive(Debug)]
pub struct Stream {
    response: Response,
    final_url: Url,
    permanent: bool,
    max_bytes: u64,
    read: u64,
}

impl Stream {
    fn new(response: Response, final_url: Url, permanent: bool, max_bytes: u64) -> Result<Self, FetchError> {
        if response.content_length().is_some_and(|length| length > max_bytes) {
            return Err(FetchError::TooLarge {
                url: final_url.to_string(),
                limit: max_bytes,
            });
        }
        Ok(Self {
            response,
            final_url,
            permanent,
            max_bytes,
            read: 0,
        })
    }

    /// The URL after redirects.
    #[must_use]
    pub const fn final_url(&self) -> &Url {
        &self.final_url
    }

    /// The next chunk, or `None` at the end.
    ///
    /// # Errors
    /// [`FetchError::TooLarge`] once the body exceeds its limit, or the error that broke the body.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, FetchError> {
        let chunk = self
            .response
            .chunk()
            .await
            .map_err(|error| fetch_error(&self.final_url, &error))?;
        if let Some(bytes) = &chunk {
            self.read += bytes.len() as u64;
            if self.read > self.max_bytes {
                return Err(FetchError::TooLarge {
                    url: self.final_url.to_string(),
                    limit: self.max_bytes,
                });
            }
        }
        Ok(chunk)
    }

    /// Reads the whole body.
    ///
    /// # Errors
    /// As [`Stream::chunk`].
    pub async fn body(mut self) -> Result<Body, FetchError> {
        let headers = self.response.headers();
        let validators = Validators::from_headers(headers);
        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mut bytes = Vec::new();
        while let Some(chunk) = self.chunk().await? {
            bytes.extend_from_slice(&chunk);
        }
        Ok(Body {
            bytes,
            final_url: self.final_url,
            permanent: self.permanent,
            validators,
            content_type,
        })
    }
}

/// What opening a fetch found.
#[derive(Debug)]
pub enum Opened {
    /// 304: the stored validators are current.
    NotModified,
    /// 404 or 410, directly or through permanent redirects only: the document is gone.
    Gone,
    /// A successful response, its body not read yet.
    Stream(Box<Stream>),
}

/// The HTTP client of every source.
#[derive(Debug)]
pub struct Fetcher {
    client: reqwest::Client,
    allow_http: bool,
    allow_private_networks: bool,
    retry: RetryPolicy,
    schedule: HostSchedule,
}

impl Fetcher {
    /// A client with the limits of `settings` that identifies as `user_agent` and resolves through `resolver`, behind
    /// the address guard.
    ///
    /// # Errors
    /// [`IngestError::Client`] when the client cannot be built, such as for a user agent that is not a valid header.
    pub fn new(settings: &FetchSettings, user_agent: &str, resolver: Arc<dyn Resolve>) -> Result<Self, IngestError> {
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .no_proxy()
            .redirect(redirect::Policy::none())
            .connect_timeout(Duration::from_secs(settings.connect_timeout_secs))
            .dns_resolver(Arc::new(GuardedResolver::new(
                resolver,
                settings.allow_private_networks,
            )))
            .build()
            .map_err(IngestError::client)?;
        Ok(Self {
            client,
            allow_http: settings.allow_http,
            allow_private_networks: settings.allow_private_networks,
            retry: RetryPolicy {
                retries: settings.retries,
            },
            schedule: HostSchedule::new(Duration::from_millis(settings.host_interval_ms)),
        })
    }

    /// Spaces requests to `host` at least `interval` apart, for a robots.txt `Crawl-delay`.
    pub fn slow_down(&self, host: &str, interval: Duration) {
        self.schedule.slow_down(host, interval);
    }

    /// Fetches a whole body.
    ///
    /// # Errors
    /// Any [`FetchError`] except [`FetchError::RobotsDisallowed`], which only crawls report.
    pub async fn get(&self, request: &FetchRequest<'_>) -> Result<FetchResponse, FetchError> {
        Ok(match self.open(request).await? {
            Opened::NotModified => FetchResponse::NotModified,
            Opened::Gone => FetchResponse::Gone,
            Opened::Stream(stream) => FetchResponse::Body(stream.body().await?),
        })
    }

    /// Follows redirects up to a successful response and returns it before reading its body.
    ///
    /// # Errors
    /// As [`Fetcher::get`].
    pub async fn open(&self, request: &FetchRequest<'_>) -> Result<Opened, FetchError> {
        let mut url = request.url.clone();
        let mut permanent = true;
        for hop in 0..=MAX_REDIRECTS {
            if hop > 0 && !request.allowed_hosts.contains(url.host_str().unwrap_or_default()) {
                return Err(FetchError::RedirectedAway { url, permanent });
            }
            self.check(&url, request.allowed_hosts)?;
            let response = self.send(&url, request).await?;
            let status = response.status();
            if status == StatusCode::NOT_MODIFIED {
                return Ok(Opened::NotModified);
            }
            if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
                if permanent {
                    return Ok(Opened::Gone);
                }
                return Err(FetchError::Status {
                    url: url.to_string(),
                    status: status.as_u16(),
                });
            }
            if status.is_redirection() {
                permanent &= matches!(status, StatusCode::MOVED_PERMANENTLY | StatusCode::PERMANENT_REDIRECT);
                url = redirect_target(&url, &response)?;
                continue;
            }
            if !status.is_success() {
                return Err(FetchError::Status {
                    url: url.to_string(),
                    status: status.as_u16(),
                });
            }
            return Ok(Opened::Stream(Box::new(Stream::new(
                response,
                url,
                permanent,
                request.max_bytes,
            )?)));
        }
        Err(FetchError::TooManyRedirects {
            url: request.url.to_string(),
        })
    }

    /// Checks the scheme, the host and an IP-literal host of one hop.
    fn check(&self, url: &Url, allowed_hosts: &HostSet) -> Result<(), FetchError> {
        let refuse = |error: fn(String) -> FetchError| Err(error(url.to_string()));
        if !(url.scheme() == "https" || (url.scheme() == "http" && self.allow_http)) {
            return refuse(|url| FetchError::Scheme { url });
        }
        if !allowed_hosts.contains(url.host_str().unwrap_or_default()) {
            return refuse(|url| FetchError::HostNotAllowed { url });
        }
        let literal = match url.host() {
            Some(Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
            Some(Host::Ipv6(ip)) => Some(IpAddr::V6(ip)),
            _ => None,
        };
        if !self.allow_private_networks && literal.is_some_and(is_blocked) {
            return refuse(|url| FetchError::BlockedAddress { url });
        }
        Ok(())
    }

    /// Sends one hop, retrying connection errors, timeouts, 429 and 5xx answers. After the last retry, a 429 or 5xx
    /// answer is returned as is.
    async fn send(&self, url: &Url, request: &FetchRequest<'_>) -> Result<Response, FetchError> {
        let host = url.host_str().unwrap_or_default();
        let mut attempt = 0;
        loop {
            self.schedule.wait(host).await;
            let result = self.build(url, request).send().await;
            let retry_after = match &result {
                Ok(response) if is_retryable(response.status()) => response
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(parse_retry_after),
                Err(error) if is_transient(error) => None,
                _ => return result.map_err(|error| fetch_error(url, &error)),
            };
            if attempt == self.retry.retries {
                return result.map_err(|error| fetch_error(url, &error));
            }
            let wait = self.retry.delay(attempt, url, retry_after);
            tracing::debug!(%url, attempt, ?wait, "retrying");
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }

    fn build(&self, url: &Url, request: &FetchRequest<'_>) -> reqwest::RequestBuilder {
        let mut builder = self.client.get(url.clone()).timeout(request.timeout);
        // Extra headers can carry credentials, so a redirect to another origin does not get them.
        if url.origin() == request.url.origin() {
            for (name, value) in request.headers {
                builder = builder.header(*name, value);
            }
        }
        // Validators belong to the requested document, so a redirect target gets them only when it is that document.
        if let Some(validators) = request
            .validators
            .filter(|_| request.validators_follow_redirects || url == request.url)
        {
            if let Some(etag) = &validators.etag {
                builder = builder.header(IF_NONE_MATCH, etag);
            }
            if let Some(date) = &validators.last_modified {
                builder = builder.header(IF_MODIFIED_SINCE, date);
            }
        }
        builder
    }
}

/// Whether a status is worth retrying.
fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Whether an error is worth retrying: a timeout or a connection error that the address guard did not cause.
fn is_transient(error: &reqwest::Error) -> bool {
    (error.is_timeout() || error.is_connect()) && !was_blocked(error)
}

/// Whether the address guard caused `error`.
fn was_blocked(error: &reqwest::Error) -> bool {
    // The guard's error reaches reqwest inside an `io::Error`, whose `source` skips it.
    causes(error).any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
            .is_some_and(<dyn std::error::Error + Send + Sync>::is::<Blocked>)
    })
}

/// The causes of `error`, nearest first.
fn causes(error: &reqwest::Error) -> impl Iterator<Item = &(dyn std::error::Error + 'static)> {
    std::iter::successors(error.source(), |&cause| cause.source())
}

/// `error` as a [`FetchError`] about `url`.
fn fetch_error(url: &Url, error: &reqwest::Error) -> FetchError {
    let url = url.to_string();
    if was_blocked(error) {
        FetchError::BlockedAddress { url }
    } else if error.is_timeout() {
        FetchError::Timeout { url }
    } else {
        let messages: Vec<String> = std::iter::once(error.to_string())
            .chain(causes(error).map(ToString::to_string))
            .collect();
        let message = messages.join(": ");
        FetchError::Network { url, message }
    }
}

/// Where a redirect from `url` points.
fn redirect_target(url: &Url, response: &Response) -> Result<Url, FetchError> {
    response
        .headers()
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|location| url.join(location).ok())
        .ok_or_else(|| FetchError::Status {
            url: url.to_string(),
            status: response.status().as_u16(),
        })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::guard::{BoxFuture, SystemResolve};

    /// Fetch settings for tests against a local server: http and private networks allowed, no spacing, no retries.
    pub(crate) fn local_settings() -> FetchSettings {
        FetchSettings {
            allow_http: true,
            allow_private_networks: true,
            host_interval_ms: 0,
            retries: 0,
            ..FetchSettings::default()
        }
    }

    pub(crate) fn fetcher(settings: &FetchSettings) -> Fetcher {
        Fetcher::new(settings, "zenodotus-test", Arc::new(SystemResolve)).unwrap()
    }

    /// Answers every name with fixed addresses, counting the lookups.
    struct Fixed(Vec<IpAddr>, std::sync::atomic::AtomicUsize);

    impl Resolve for Fixed {
        fn resolve<'a>(&'a self, _host: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { Ok(self.0.clone()) })
        }
    }

    pub(crate) fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    fn request<'a>(url: &'a Url, hosts: &'a HostSet) -> FetchRequest<'a> {
        FetchRequest {
            url,
            allowed_hosts: hosts,
            max_bytes: 1000,
            validators: None,
            validators_follow_redirects: false,
            timeout: Duration::from_secs(5),
            headers: &[],
        }
    }

    async fn fetch(fetcher: &Fetcher, url: &Url) -> Result<FetchResponse, FetchError> {
        let hosts = HostSet::of(&[url], &[]);
        fetcher.get(&request(url, &hosts)).await
    }

    fn body(response: FetchResponse) -> Body {
        match response {
            FetchResponse::Body(body) => body,
            other => panic!("expected a body, got {other:?}"),
        }
    }

    fn stream(opened: Opened) -> Box<Stream> {
        match opened {
            Opened::Stream(stream) => stream,
            other => panic!("expected a stream, got {other:?}"),
        }
    }

    #[test]
    fn the_test_helpers_refuse_what_they_do_not_expect() {
        assert!(std::panic::catch_unwind(|| body(FetchResponse::Gone)).is_err());
        assert!(std::panic::catch_unwind(|| stream(Opened::Gone)).is_err());
    }

    #[test]
    fn host_sets_hold_the_source_hosts_and_the_extra_ones() {
        let hosts = HostSet::of(
            &[&url("https://Docs.Example/a"), &url("https://cdn.example/b")],
            &["Extra.Example".to_owned()],
        );
        assert!(hosts.contains("docs.example"));
        assert!(hosts.contains("CDN.example"));
        assert!(hosts.contains("extra.example"));
        assert!(!hosts.contains("other.example"));
    }

    #[test]
    fn errors_know_their_url_kind_and_document_form() {
        let url = || "https://docs.example/a".to_owned();
        let errors = [
            (FetchError::Scheme { url: url() }, "scheme"),
            (FetchError::HostNotAllowed { url: url() }, "host_not_allowed"),
            (
                FetchError::RedirectedAway {
                    url: Url::parse(&url()).unwrap(),
                    permanent: true,
                },
                "host_not_allowed",
            ),
            (FetchError::BlockedAddress { url: url() }, "blocked_address"),
            (FetchError::TooManyRedirects { url: url() }, "too_many_redirects"),
            (FetchError::TooLarge { url: url(), limit: 9 }, "too_large"),
            (
                FetchError::Status {
                    url: url(),
                    status: 500,
                },
                "status",
            ),
            (FetchError::Timeout { url: url() }, "timeout"),
            (
                FetchError::Network {
                    url: url(),
                    message: "reset".into(),
                },
                "network",
            ),
            (FetchError::RobotsDisallowed { url: url() }, "robots_disallowed"),
            (
                FetchError::Decode {
                    url: url(),
                    message: "bad".into(),
                },
                "decode",
            ),
        ];
        for (error, kind) in errors {
            assert_eq!(error.kind(), kind);
            assert_eq!(error.url(), "https://docs.example/a");
            let doc = error.doc_error("a.md");
            assert_eq!((doc.doc_path.as_str(), doc.kind.as_str()), ("a.md", kind));
            assert_eq!(doc.message, format!("https://docs.example/a: {error}"));
        }
        assert_eq!(
            FetchError::TooLarge { url: url(), limit: 9 }.to_string(),
            "larger than 9 bytes"
        );
    }

    #[tokio::test]
    async fn a_page_comes_back_with_its_validators_and_type() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .and(header("user-agent", "zenodotus-test"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v1\"")
                    .insert_header("last-modified", "Wed, 21 Oct 2015 07:28:00 GMT")
                    .set_body_raw("<h1>Hi</h1>", "text/html; charset=utf-8"),
            )
            .mount(&server)
            .await;
        let page = url(&format!("{}/page", server.uri()));
        let body = body(fetch(&fetcher(&local_settings()), &page).await.unwrap());
        assert_eq!(body.text().unwrap(), "<h1>Hi</h1>");
        assert!(body.is_html());
        assert_eq!(body.final_url, page);
        assert_eq!(
            body.validators,
            Validators {
                etag: Some("\"v1\"".to_owned()),
                last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_owned()),
            }
        );
    }

    #[tokio::test]
    async fn bodies_are_typed_and_decoded() {
        let page = url("https://docs.example/a");
        let typed = |content_type: Option<&str>, bytes: &[u8]| Body {
            bytes: bytes.to_vec(),
            final_url: page.clone(),
            permanent: true,
            validators: Validators::default(),
            content_type: content_type.map(str::to_owned),
        };
        assert!(typed(Some(" Application/XHTML+xml"), b"").is_html());
        assert!(!typed(Some("text/markdown"), b"").is_html());
        assert!(!typed(None, b"").is_html());
        let error = typed(None, &[0xff, 0xfe]).text().unwrap_err();
        assert_eq!(error.kind(), "decode");
        assert!(error.to_string().starts_with("the body is not UTF-8: "), "{error}");
    }

    #[tokio::test]
    async fn stored_validators_make_the_request_conditional() {
        let server = MockServer::start().await;
        Mock::given(path("/page"))
            .and(header("if-none-match", "\"v1\""))
            .and(|request: &wiremock::Request| {
                request
                    .headers
                    .get("if-modified-since")
                    .is_some_and(|value| value == "Wed, 21 Oct 2015 07:28:00 GMT")
            })
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        Mock::given(path("/etag-only"))
            .and(header("if-none-match", "\"v2\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        let fetcher = fetcher(&local_settings());
        let page = url(&format!("{}/page", server.uri()));
        let hosts = HostSet::of(&[&page], &[]);
        let state = DocState {
            doc_key: zenodotus_core::ids::DocKey::new(
                &zenodotus_core::ids::Scope::new(
                    zenodotus_core::ids::LibraryId::parse("a").unwrap(),
                    zenodotus_core::ids::Version::parse("1").unwrap(),
                ),
                "page",
            )
            .unwrap(),
            etag: Some("\"v1\"".to_owned()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_owned()),
            content_hash: String::new(),
            fetched_at: zenodotus_core::time::Timestamp::from_unix(0),
        };
        let validators = Validators::of(&state);
        let conditional = FetchRequest {
            validators: Some(&validators),
            ..request(&page, &hosts)
        };
        assert_eq!(fetcher.get(&conditional).await.unwrap(), FetchResponse::NotModified);
        let etag_only = url(&format!("{}/etag-only", server.uri()));
        let validators = Validators {
            etag: Some("\"v2\"".to_owned()),
            last_modified: None,
        };
        let conditional = FetchRequest {
            validators: Some(&validators),
            ..request(&etag_only, &hosts)
        };
        assert_eq!(fetcher.get(&conditional).await.unwrap(), FetchResponse::NotModified);
    }

    #[tokio::test]
    async fn not_found_and_gone_mean_deleted_and_other_statuses_fail() {
        let server = MockServer::start().await;
        for (route, status) in [("/missing", 404), ("/gone", 410), ("/forbidden", 403)] {
            Mock::given(path(route))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
        }
        let fetcher = fetcher(&local_settings());
        let at = |route: &str| url(&format!("{}{route}", server.uri()));
        assert_eq!(fetch(&fetcher, &at("/missing")).await.unwrap(), FetchResponse::Gone);
        assert_eq!(fetch(&fetcher, &at("/gone")).await.unwrap(), FetchResponse::Gone);
        assert_eq!(
            fetch(&fetcher, &at("/forbidden")).await.unwrap_err(),
            FetchError::Status {
                url: at("/forbidden").to_string(),
                status: 403
            }
        );
    }

    #[tokio::test]
    async fn redirects_are_followed_and_checked_hop_by_hop() {
        let server = MockServer::start().await;
        let redirect = |to: &str| ResponseTemplate::new(301).insert_header("location", to);
        Mock::given(path("/old"))
            .respond_with(redirect("/new"))
            .mount(&server)
            .await;
        Mock::given(path("/new"))
            .respond_with(ResponseTemplate::new(200).set_body_string("moved"))
            .mount(&server)
            .await;
        for hop in 0..6 {
            Mock::given(path(format!("/loop{hop}")))
                .respond_with(redirect(&format!("/loop{}", hop + 1)))
                .mount(&server)
                .await;
        }
        Mock::given(path("/away"))
            .respond_with(redirect("http://elsewhere.example/x"))
            .mount(&server)
            .await;
        Mock::given(path("/ftp"))
            .respond_with(redirect("ftp://files.example/x"))
            .mount(&server)
            .await;
        Mock::given(path("/nowhere"))
            .respond_with(ResponseTemplate::new(302))
            .mount(&server)
            .await;
        Mock::given(path("/broken"))
            .respond_with(redirect("http://[::1"))
            .mount(&server)
            .await;
        let here = url(&server.uri());
        Mock::given(path("/ftp-here"))
            .respond_with(redirect(&format!("ftp://{}/x", here.host_str().unwrap())))
            .mount(&server)
            .await;
        for (route, status) in [("/via301-to-missing", 301), ("/via302-to-missing", 302)] {
            Mock::given(path(route))
                .respond_with(ResponseTemplate::new(status).insert_header("location", "/missing-target"))
                .mount(&server)
                .await;
        }
        let fetcher = fetcher(&local_settings());
        let at = |route: &str| url(&format!("{}{route}", server.uri()));
        assert_eq!(
            fetch(&fetcher, &at("/via301-to-missing")).await.unwrap(),
            FetchResponse::Gone
        );
        let moved = body(fetch(&fetcher, &at("/old")).await.unwrap());
        assert_eq!((moved.text().unwrap(), &moved.final_url), ("moved", &at("/new")));
        assert!(moved.permanent);
        assert!(body(fetch(&fetcher, &at("/new")).await.unwrap()).permanent);
        for (status, permanent) in [(308, true), (302, false), (303, false), (307, false)] {
            let route = format!("/via{status}");
            Mock::given(path(route.as_str()))
                .respond_with(ResponseTemplate::new(status).insert_header("location", "/old"))
                .mount(&server)
                .await;
            let landed = body(fetch(&fetcher, &at(&route)).await.unwrap());
            assert_eq!(
                (landed.final_url.path(), landed.permanent),
                ("/new", permanent),
                "{status}"
            );
        }
        let kinds = [
            ("/loop0", "too_many_redirects"),
            ("/away", "host_not_allowed"),
            ("/ftp", "host_not_allowed"),
            ("/ftp-here", "scheme"),
            ("/via302-to-missing", "status"),
            ("/nowhere", "status"),
            ("/broken", "status"),
        ];
        for (route, kind) in kinds {
            let error = fetch(&fetcher, &at(route)).await.unwrap_err();
            assert_eq!(error.kind(), kind, "{route}: {error}");
        }
        Mock::given(path("/away302"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/away"))
            .mount(&server)
            .await;
        for (route, permanent) in [("/away", true), ("/away302", false)] {
            let error = fetch(&fetcher, &at(route)).await.unwrap_err();
            let target = url("http://elsewhere.example/x");
            assert_eq!(error, FetchError::RedirectedAway { url: target, permanent }, "{route}");
            assert_eq!(error.to_string(), "redirects to a host this source may not reach");
        }
        assert_eq!(
            fetch(&fetcher, &at("/loop0")).await.unwrap_err().url(),
            at("/loop0").as_str()
        );
        // Five redirects are followed: /loop1 reaches /loop6, which does not exist.
        assert_eq!(fetch(&fetcher, &at("/loop1")).await.unwrap(), FetchResponse::Gone);
    }

    #[tokio::test]
    async fn validators_go_to_a_redirect_target_only_when_it_is_the_same_document() {
        let server = MockServer::start().await;
        Mock::given(path("/old"))
            .respond_with(ResponseTemplate::new(301).insert_header("location", "/new"))
            .mount(&server)
            .await;
        Mock::given(path("/new"))
            .and(wiremock::matchers::header("if-none-match", "\"v1\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        Mock::given(path("/new"))
            .respond_with(ResponseTemplate::new(200).set_body_string("new"))
            .mount(&server)
            .await;
        let old = url(&format!("{}/old", server.uri()));
        let hosts = HostSet::of(&[&old], &[]);
        let validators = Validators {
            etag: Some("\"v1\"".to_owned()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_owned()),
        };
        let fetcher = fetcher(&local_settings());
        let crawled = FetchRequest {
            validators: Some(&validators),
            ..request(&old, &hosts)
        };
        let moved = body(fetcher.get(&crawled).await.unwrap());
        assert_eq!(moved.final_url.path(), "/new");
        let configured = FetchRequest {
            validators_follow_redirects: true,
            ..crawled
        };
        assert_eq!(fetcher.get(&configured).await.unwrap(), FetchResponse::NotModified);
        let conditional: Vec<(String, bool)> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| (r.url.path().to_owned(), r.headers.contains_key("if-modified-since")))
            .collect();
        assert_eq!(
            conditional,
            [
                ("/old".to_owned(), true),
                ("/new".to_owned(), false),
                ("/old".to_owned(), true),
                ("/new".to_owned(), true)
            ]
        );
    }

    #[tokio::test]
    async fn extra_headers_are_not_sent_to_another_origin() {
        let first = MockServer::start().await;
        let second = MockServer::start().await;
        let redirect = |to: String| ResponseTemplate::new(302).insert_header("location", to.as_str());
        Mock::given(path("/start"))
            .respond_with(redirect(format!("{}/same", first.uri())))
            .mount(&first)
            .await;
        Mock::given(path("/same"))
            .respond_with(redirect(format!("{}/other", second.uri())))
            .mount(&first)
            .await;
        Mock::given(path("/other"))
            .respond_with(ResponseTemplate::new(200).set_body_string("landed"))
            .mount(&second)
            .await;
        let start = url(&format!("{}/start", first.uri()));
        let hosts = HostSet::of(&[&start], &[]);
        let headers = [("authorization", "Bearer secret".to_owned())];
        let request = FetchRequest {
            headers: &headers,
            ..request(&start, &hosts)
        };
        let landed = body(fetcher(&local_settings()).get(&request).await.unwrap());
        assert_eq!(landed.text().unwrap(), "landed");
        let authorized = |requests: Vec<wiremock::Request>| -> Vec<(String, bool)> {
            requests
                .iter()
                .map(|r| (r.url.path().to_owned(), r.headers.contains_key("authorization")))
                .collect()
        };
        assert_eq!(
            authorized(first.received_requests().await.unwrap()),
            [("/start".to_owned(), true), ("/same".to_owned(), true)]
        );
        assert_eq!(
            authorized(second.received_requests().await.unwrap()),
            [("/other".to_owned(), false)]
        );
    }

    #[tokio::test]
    async fn bodies_over_the_limit_are_refused() {
        let server = MockServer::start().await;
        Mock::given(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(1001)))
            .mount(&server)
            .await;
        let error = fetch(&fetcher(&local_settings()), &url(&format!("{}/big", server.uri())))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), "too_large");
    }

    /// Serves one raw HTTP response to one connection, then closes it.
    pub(crate) async fn raw_server(response: &'static [u8]) -> Url {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await;
            let _ = socket.write_all(response).await;
            let _ = socket.shutdown().await;
        });
        url(&format!("http://{addr}/doc"))
    }

    #[tokio::test]
    async fn streamed_bodies_are_capped_while_reading_and_broken_bodies_fail() {
        let unsized_body: &[u8] = b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n";
        let mut long = unsized_body.to_vec();
        long.extend(std::iter::repeat_n(b'x', 1001));
        let long: &'static [u8] = long.leak();
        let fetcher = fetcher(&local_settings());
        let error = fetch(&fetcher, &raw_server(long).await).await.unwrap_err();
        assert_eq!(error.kind(), "too_large");
        let mut exact = unsized_body.to_vec();
        exact.extend(std::iter::repeat_n(b'x', 1000));
        let exact = fetch(&fetcher, &raw_server(exact.leak()).await).await.unwrap();
        assert_eq!(body(exact).bytes.len(), 1000);
        let truncated = raw_server(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nshort").await;
        let error = fetch(&fetcher, &truncated).await.unwrap_err();
        assert_eq!(error.kind(), "network", "{error}");
        let small = raw_server(b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\nsmall").await;
        let hosts = HostSet::of(&[&small], &[]);
        let mut stream = stream(fetcher.open(&request(&small, &hosts)).await.unwrap());
        assert_eq!(stream.final_url(), &small);
        assert_eq!(stream.chunk().await.unwrap().unwrap(), Bytes::from_static(b"small"));
        assert_eq!(stream.chunk().await.unwrap(), None);
    }

    #[tokio::test]
    async fn transient_failures_are_retried_until_the_retries_run_out() {
        let server = MockServer::start().await;
        Mock::given(path("/flaky"))
            .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(path("/flaky"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;
        Mock::given(path("/busy"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
            .mount(&server)
            .await;
        Mock::given(path("/forbidden"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let settings = FetchSettings {
            retries: 1,
            ..local_settings()
        };
        let fetcher = fetcher(&settings);
        let at = |route: &str| url(&format!("{}{route}", server.uri()));
        assert_eq!(body(fetch(&fetcher, &at("/flaky")).await.unwrap()).bytes, b"ok");
        assert_eq!(
            fetch(&fetcher, &at("/busy")).await.unwrap_err(),
            FetchError::Status {
                url: at("/busy").to_string(),
                status: 429
            }
        );
        // Other statuses are final: /forbidden is asked once.
        assert_eq!(fetch(&fetcher, &at("/forbidden")).await.unwrap_err().kind(), "status");
        let asked: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| request.url.path().to_owned())
            .collect();
        assert_eq!(asked, ["/flaky", "/flaky", "/busy", "/busy", "/forbidden"]);
    }

    #[tokio::test]
    async fn refused_connections_are_retried_then_reported() {
        // Port 1 needs privileges to listen on, so nothing answers there; a port freed by the test itself could be
        // taken by a parallel test.
        let closed = url("http://127.0.0.1:1/doc");
        let settings = FetchSettings {
            retries: 1,
            ..local_settings()
        };
        let started = std::time::Instant::now();
        let error = fetch(&fetcher(&settings), &closed).await.unwrap_err();
        assert_eq!(error.kind(), "network", "{error}");
        assert!(started.elapsed() >= Duration::from_millis(500));
    }

    #[tokio::test]
    async fn slow_answers_time_out() {
        let server = MockServer::start().await;
        Mock::given(path("/slow"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
            .mount(&server)
            .await;
        let page = url(&format!("{}/slow", server.uri()));
        let hosts = HostSet::of(&[&page], &[]);
        let quick = FetchRequest {
            timeout: Duration::from_millis(100),
            ..request(&page, &hosts)
        };
        let error = fetcher(&local_settings()).get(&quick).await.unwrap_err();
        assert_eq!(error, FetchError::Timeout { url: page.to_string() });
    }

    #[tokio::test]
    async fn schemes_hosts_and_literal_addresses_are_checked_before_connecting() {
        let fetcher = fetcher(&FetchSettings {
            host_interval_ms: 0,
            ..FetchSettings::default()
        });
        let kind = |url: Url, hosts: HostSet| {
            let fetcher = &fetcher;
            async move { fetcher.get(&request(&url, &hosts)).await.unwrap_err().kind() }
        };
        let plain = url("http://docs.example/a");
        assert_eq!(kind(plain.clone(), HostSet::of(&[&plain], &[])).await, "scheme");
        let page = url("https://docs.example/a");
        assert_eq!(kind(page, HostSet::default()).await, "host_not_allowed");
        for literal in ["https://127.0.0.1/a", "https://[::1]/a", "https://[::ffff:10.0.0.1]/a"] {
            let literal = url(literal);
            assert_eq!(
                kind(literal.clone(), HostSet::of(&[&literal], &[])).await,
                "blocked_address"
            );
        }
    }

    #[tokio::test]
    async fn names_that_resolve_to_blocked_addresses_are_never_reached() {
        let server = MockServer::start().await;
        Mock::given(path("/a"))
            .respond_with(ResponseTemplate::new(200).set_body_string("reached"))
            .mount(&server)
            .await;
        let port = server.address().port();
        let page = url(&format!("http://docs.example:{port}/a"));
        let loopback = || Arc::new(Fixed(vec![IpAddr::from([127, 0, 0, 1])], 0.into()));
        let guarded = FetchSettings {
            allow_http: true,
            host_interval_ms: 0,
            ..FetchSettings::default()
        };
        let resolver = loopback();
        let fetcher = Fetcher::new(&guarded, "zenodotus-test", resolver.clone()).unwrap();
        assert_eq!(
            fetch(&fetcher, &page).await.unwrap_err(),
            FetchError::BlockedAddress { url: page.to_string() }
        );
        // A blocked address is final: it is not retried, although the settings allow retries.
        assert!(guarded.retries > 0);
        assert_eq!(resolver.1.load(std::sync::atomic::Ordering::SeqCst), 1);
        let open = Fetcher::new(&local_settings(), "zenodotus-test", loopback()).unwrap();
        assert_eq!(body(fetch(&open, &page).await.unwrap()).bytes, b"reached");
    }

    #[tokio::test]
    async fn invalid_request_headers_are_network_errors() {
        let server = MockServer::start().await;
        let page = url(&format!("{}/a", server.uri()));
        let hosts = HostSet::of(&[&page], &[]);
        let validators = Validators {
            etag: Some("bad\nvalue".to_owned()),
            last_modified: None,
        };
        let bad = FetchRequest {
            validators: Some(&validators),
            ..request(&page, &hosts)
        };
        let error = fetcher(&local_settings()).get(&bad).await.unwrap_err();
        assert_eq!(error.kind(), "network");
    }

    #[test]
    fn a_user_agent_that_is_not_a_header_value_is_a_client_error() {
        let error = Fetcher::new(&local_settings(), "bad\nagent", Arc::new(SystemResolve)).unwrap_err();
        assert!(matches!(error, IngestError::Client { .. }), "{error}");
        assert!(
            error.to_string().starts_with("cannot build the HTTP client: "),
            "{error}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn crawl_delays_space_requests_to_one_host() {
        let fetcher = fetcher(&local_settings());
        fetcher.slow_down("docs.example", Duration::from_secs(3));
        let start = tokio::time::Instant::now();
        fetcher.schedule.wait("docs.example").await;
        fetcher.schedule.wait("docs.example").await;
        assert_eq!(start.elapsed(), Duration::from_secs(3));
    }
}
