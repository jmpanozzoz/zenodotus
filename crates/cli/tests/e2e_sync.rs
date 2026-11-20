//! End-to-end tests of `zenodotus sync` against a documentation website served by wiremock: an incremental re-sync,
//! pages that keep failing, and a sitemap that cannot be read.

use std::process::Output;

use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The binary under test.
const BINARY: &str = env!("CARGO_BIN_EXE_zenodotus");

/// A home directory whose config lists the website at `<server>/docs/` as `site@1`, with http and local addresses
/// allowed, one retry and no spacing between requests.
struct Home {
    dir: TempDir,
}

impl Home {
    async fn new(server: &MockServer) -> Self {
        let home = Self {
            dir: TempDir::new().unwrap(),
        };
        home.success(&["init"]).await;
        let start = format!("{}/docs/", server.uri());
        home.success(&["add", "site", "--version", "1", "--website", &start, "--name", "Site"])
            .await;
        let path = home.dir.path().join("zenodotus.toml");
        let config = std::fs::read_to_string(&path).unwrap();
        let config = config
            .replace("# allow_http = false", "allow_http = true")
            .replace("# allow_private_networks = false", "allow_private_networks = true")
            .replace("# retries = 3", "retries = 1\nhost_interval_ms = 0");
        std::fs::write(&path, config).unwrap();
        home
    }

    async fn output(&self, args: &[&str]) -> Output {
        tokio::process::Command::new(BINARY)
            .arg("--config")
            .arg(self.dir.path().join("zenodotus.toml"))
            .args(args)
            .env("HOME", self.dir.path())
            .env_remove("XDG_DATA_HOME")
            .env_remove("ZENODOTUS_LOG")
            .env_remove("GITHUB_TOKEN")
            .output()
            .await
            .unwrap()
    }

    /// Runs `args`, which must succeed, and returns stdout.
    async fn success(&self, args: &[&str]) -> String {
        let output = self.output(args).await;
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        stdout(&output)
    }

    /// Runs `sync`, which must exit with `code`, and returns stdout.
    async fn sync(&self, code: i32) -> String {
        let output = self.output(&["sync"]).await;
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}{}",
            stdout(&output),
            stderr(&output)
        );
        stdout(&output)
    }

    /// The text `search site <query>` prints.
    async fn search(&self, query: &str) -> String {
        self.success(&["search", "site", query]).await
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Serves `route` as an HTML page with `body` and an `ETag`.
async fn page(server: &MockServer, route: &str, body: &str, etag: &str) {
    let html = format!("<html><head><title>{route}</title></head><body><main><p>{body}</p></main></body></html>");
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(html, "text/html")
                .insert_header("etag", etag),
        )
        .mount(server)
        .await;
}

/// Answers `route` with `status` and no body.
async fn status(server: &MockServer, route: &str, code: u16) {
    Mock::given(path(route))
        .respond_with(ResponseTemplate::new(code))
        .mount(server)
        .await;
}

/// Answers 304 to requests for `route` that carry `etag`.
async fn not_modified(server: &MockServer, route: &str, etag: &str) {
    Mock::given(path(route))
        .and(header("if-none-match", etag))
        .respond_with(ResponseTemplate::new(304))
        .mount(server)
        .await;
}

/// No robots.txt, and a sitemap listing pages `a`, `b` and `c` under `/docs/`.
async fn site_map(server: &MockServer) {
    status(server, "/robots.txt", 404).await;
    let uri = server.uri();
    let sitemap = format!(
        "<urlset><url><loc>{uri}/docs/a</loc></url><url><loc>{uri}/docs/b</loc></url>\
         <url><loc>{uri}/docs/c</loc></url></urlset>"
    );
    Mock::given(path("/sitemap.xml"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sitemap, "application/xml"))
        .mount(server)
        .await;
}

/// The `N sections` count of a sync report line.
fn sections(report: &str) -> &str {
    let (_, rest) = report.split_once("documents, ").unwrap();
    rest.split_once(" sections").unwrap().0
}

#[tokio::test(flavor = "multi_thread")]
async fn an_incremental_resync_fetches_only_what_changed_and_keeps_pages_that_fail() {
    let server = MockServer::start().await;
    site_map(&server).await;
    page(&server, "/docs/", "Welcome to the handbook of the site.", "\"home\"").await;
    page(&server, "/docs/a", "Alpha explains configuration files.", "\"a1\"").await;
    page(&server, "/docs/b", "Bravo describes the original widget.", "\"b1\"").await;
    page(&server, "/docs/c", "Charlie covers the retired gadget.", "\"c1\"").await;
    let home = Home::new(&server).await;
    let first = home.sync(0).await;
    assert!(first.starts_with("site@1: ok, revision 1, 4 documents, "), "{first}");
    assert!(
        first.contains("(4 changed, 0 deleted, 0 unchanged, 0 failed)"),
        "{first}"
    );
    assert!(home.search("gadget").await.contains("retired gadget"));

    server.reset().await;
    site_map(&server).await;
    not_modified(&server, "/docs/", "\"home\"").await;
    not_modified(&server, "/docs/a", "\"a1\"").await;
    page(&server, "/docs/b", "Bravo describes the redesigned sprocket.", "\"b2\"").await;
    status(&server, "/docs/c", 404).await;
    let second = home.sync(0).await;
    assert!(second.starts_with("site@1: ok, revision 2, 3 documents, "), "{second}");
    assert!(
        second.contains("(1 changed, 1 deleted, 2 unchanged, 0 failed)"),
        "{second}"
    );
    assert!(home.search("sprocket").await.contains("redesigned sprocket"));
    assert!(!home.search("gadget").await.contains("retired gadget"));

    server.reset().await;
    site_map(&server).await;
    not_modified(&server, "/docs/", "\"home\"").await;
    status(&server, "/docs/a", 500).await;
    status(&server, "/docs/b", 500).await;
    status(&server, "/docs/c", 404).await;
    let third = home.sync(3).await;
    assert!(
        third.starts_with("site@1: partial, revision 2, 3 documents, "),
        "{third}"
    );
    assert!(
        third.contains("(0 changed, 0 deleted, 1 unchanged, 2 failed)"),
        "{third}"
    );
    let requests = server.received_requests().await.unwrap();
    let attempts = requests
        .iter()
        .filter(|request| request.url.path() == "/docs/a")
        .count();
    assert_eq!(attempts, 2, "one attempt and one retry");
    assert!(home.search("sprocket").await.contains("redesigned sprocket"));
    assert!(
        home.search("configuration")
            .await
            .contains("Alpha explains configuration")
    );

    server.reset().await;
    status(&server, "/robots.txt", 404).await;
    status(&server, "/sitemap.xml", 500).await;
    let fourth = home.sync(3).await;
    assert!(
        fourth.starts_with("site@1: failed, revision 2, 3 documents, "),
        "{fourth}"
    );
    assert_eq!(sections(&fourth), sections(&third));
    assert!(fourth.contains("  last error: cannot list "), "{fourth}");
    assert!(fourth.contains("/sitemap.xml: HTTP status 500"), "{fourth}");
    assert!(home.search("sprocket").await.contains("redesigned sprocket"));
}
