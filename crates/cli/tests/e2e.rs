//! End-to-end tests of the `zenodotus` binary: the CLI, MCP over stdio and MCP over Streamable HTTP.

use std::fs::File;
use std::path::PathBuf;
use std::process::{ExitStatus, Output, Stdio};

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};

/// The binary under test.
const BINARY: &str = env!("CARGO_BIN_EXE_zenodotus");

/// A home directory whose config lists `docs/` as `tokio@1`, the default version.
struct Home {
    dir: TempDir,
}

impl Home {
    fn new() -> Self {
        let home = Self {
            dir: TempDir::new().unwrap(),
        };
        let docs = home.path("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(
            docs.join("guide.md"),
            "# Spawning\n\nUse `tokio::spawn` to run a task in the background.\n",
        )
        .unwrap();
        std::fs::write(docs.join("intro.md"), "# Intro\n\nTokio is an async runtime.\n").unwrap();
        home.success(&["init"]);
        let docs = docs.to_str().unwrap().to_owned();
        home.success(&[
            "add",
            "tokio",
            "--version",
            "1",
            "--path",
            &docs,
            "--default",
            "--name",
            "Tokio",
        ]);
        home
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    /// The binary with `--config <home>/zenodotus.toml` and `args`, isolated from the caller's environment.
    fn command(&self, args: &[&str]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(BINARY);
        command
            .arg("--config")
            .arg(self.path("zenodotus.toml"))
            .args(args)
            .env("HOME", self.dir.path())
            .env_remove("XDG_DATA_HOME")
            .env_remove("ZENODOTUS_HTTP_TOKEN")
            .env_remove("ZENODOTUS_LOG")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }

    async fn output(&self, args: &[&str]) -> Output {
        self.command(args).output().await.unwrap()
    }

    /// Runs `args`, which must succeed, and returns stdout.
    fn success(&self, args: &[&str]) -> String {
        let output = std::process::Command::new(BINARY)
            .arg("--config")
            .arg(self.path("zenodotus.toml"))
            .args(args)
            .env("HOME", self.dir.path())
            .env_remove("XDG_DATA_HOME")
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        String::from_utf8(output.stdout).unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn code(status: ExitStatus) -> Option<i32> {
    status.code()
}

/// Calls `tool` with JSON `arguments`.
async fn call(
    client: &RunningService<RoleClient, ()>,
    tool: &'static str,
    arguments: serde_json::Value,
) -> CallToolResult {
    let arguments = arguments.as_object().unwrap().clone();
    client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments))
        .await
        .unwrap()
}

/// Lists the tools and calls both, as an agent would.
async fn exercise(client: &RunningService<RoleClient, ()>) {
    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|tool| tool.name.into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["query_docs", "resolve_library"]);
    let resolved = call(client, "resolve_library", serde_json::json!({"library_name": "Tokio"})).await;
    let structured = resolved.structured_content.unwrap();
    assert_eq!(structured["matches"][0]["library_id"], "tokio");
    let found = call(
        client,
        "query_docs",
        serde_json::json!({"library_id": "tokio", "query": "tokio::spawn"}),
    )
    .await;
    assert_eq!(found.is_error, Some(false));
    let text = &found.content[0].as_text().unwrap().text;
    assert!(text.contains("Use `tokio::spawn` to run a task"), "{text}");
    let missing = call(
        client,
        "query_docs",
        serde_json::json!({"library_id": "vue", "query": "x"}),
    )
    .await;
    assert_eq!(missing.is_error, Some(true));
}

#[tokio::test]
async fn the_cli_indexes_searches_and_reports_with_exit_codes() {
    let home = Home::new();
    assert!(
        home.success(&["sync"])
            .starts_with("tokio@1: ok, revision 1, 2 documents")
    );
    let markdown = home.success(&["search", "tokio", "spawn", "a", "task"]);
    assert!(markdown.contains("Use `tokio::spawn`"), "{markdown}");
    let status: serde_json::Value = serde_json::from_str(&home.success(&["status", "--json"])).unwrap();
    assert_eq!(status["versions"][0]["outcome"], "ok");

    let unknown = home.output(&["sync", "vue"]).await;
    assert_eq!(code(unknown.status), Some(2));
    assert!(stderr(&unknown).starts_with("error: "), "{}", stderr(&unknown));
    let logged = home
        .command(&["search", "vue", "x"])
        .env("ZENODOTUS_LOG", "debug")
        .output()
        .await
        .unwrap();
    assert_eq!(code(logged.status), Some(1));
    let usage = home.output(&["search", "tokio"]).await;
    assert_eq!(code(usage.status), Some(2));

    let lock = File::create(home.path(".local/share/zenodotus/sync.lock")).unwrap();
    lock.try_lock().unwrap();
    let busy = home.output(&["sync"]).await;
    assert_eq!(code(busy.status), Some(4), "{}", stderr(&busy));
    drop(lock);

    std::fs::remove_dir_all(home.path("docs")).unwrap();
    let failed = home.output(&["sync"]).await;
    assert_eq!(code(failed.status), Some(3), "{}", stderr(&failed));
    let kept = home.success(&["search", "tokio", "spawn"]);
    assert!(kept.contains("Use `tokio::spawn`"), "{kept}");
}

#[tokio::test]
async fn an_agent_uses_both_tools_over_stdio_across_restarts() {
    let home = Home::new();
    home.success(&["sync"]);
    for _ in 0..2 {
        let transport = TokioChildProcess::new(home.command(&["serve"])).unwrap();
        let client = ().serve(transport).await.unwrap();
        assert_eq!(
            client.peer_info().unwrap().server_info.as_ref().unwrap().name,
            "zenodotus"
        );
        exercise(&client).await;
        client.cancel().await.unwrap();
    }
}

/// `serve --http 127.0.0.1:0` with `token`, and the URL it prints.
async fn serve_http(home: &Home, token: &str) -> (tokio::process::Child, String) {
    let mut child = home
        .command(&["serve", "--http", "127.0.0.1:0"])
        .env("ZENODOTUS_HTTP_TOKEN", token)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .await
        .unwrap();
    let url = line.trim_end().strip_prefix("Serving MCP on ").unwrap().to_owned();
    (child, url)
}

/// Sends `signal` to `child` and returns its exit code.
async fn stop(mut child: tokio::process::Child, signal: &str) -> Option<i32> {
    let pid = child.id().unwrap().to_string();
    let sent = std::process::Command::new("kill")
        .args([signal, &pid])
        .status()
        .unwrap();
    assert!(sent.success());
    code(child.wait().await.unwrap())
}

#[tokio::test]
async fn an_agent_uses_both_tools_over_http_until_the_server_is_stopped() {
    let home = Home::new();
    home.success(&["sync"]);
    for signal in ["-TERM", "-INT"] {
        let (child, url) = serve_http(&home, "secret").await;
        assert!(url.starts_with("http://127.0.0.1:") && url.ends_with("/mcp"), "{url}");
        let config = StreamableHttpClientTransportConfig::with_uri(url).auth_header("secret");
        let client = ().serve(StreamableHttpClientTransport::from_config(config)).await.unwrap();
        exercise(&client).await;
        client.cancel().await.unwrap();
        assert_eq!(stop(child, signal).await, Some(0));
    }
}

#[tokio::test]
async fn a_public_address_needs_a_token() {
    let home = Home::new();
    let refused = home.output(&["serve", "--http", "0.0.0.0:0"]).await;
    assert_eq!(code(refused.status), Some(2));
    assert!(stderr(&refused).contains("0.0.0.0:0"), "{}", stderr(&refused));
}
