//! The systems under comparison, and the MCP client they share. Each system is reached the way an agent reaches it:
//! zenodotus, Grounded Docs and Neuledge Context as local processes over stdio MCP, Context7 over Streamable HTTP.

pub mod context7;
pub mod grounded;
pub mod neuledge;
pub mod zenodotus;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde_json::{Map, Value};
use tokio::process::Command;

use crate::corpus::ScopeCase;
use crate::questions::Question;

/// The ranked sections one query returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranked {
    /// The text of each result, best first.
    pub sections: Vec<String>,
    /// The whole response text, as the agent would read it.
    pub raw: String,
    /// The time from sending the call to receiving its response.
    pub latency: Duration,
}

/// Why a query has no ranking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryFailure {
    /// The system does not have the requested version; the question is excluded from its score.
    VersionUnavailable,
    /// The call failed.
    Failed(String),
}

/// Why a system cannot be evaluated at all; it is reported as `not run: <reason>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemUnavailable(pub String);

/// A documentation retrieval system under evaluation. Each system is built ready to query by its own `start`.
#[allow(
    async_fn_in_trait,
    reason = "the harness drives systems by generic functions, never through dyn"
)]
pub trait RetrievalSystem {
    /// Up to `k` sections for `question`, which asks about `case`.
    async fn query(&mut self, question: &Question, case: &ScopeCase, k: usize) -> Result<Ranked, QueryFailure>;
}

/// How to start a program: `program prefix... args...`, with extra environment variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launcher {
    /// The executable.
    pub program: PathBuf,
    /// Arguments before the command's own, such as `-y <package>` for `npx`.
    pub prefix: Vec<String>,
    /// Extra environment variables.
    pub env: Vec<(String, String)>,
}

impl Launcher {
    /// A launcher of `program` with no prefix and no extra environment.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            prefix: Vec::new(),
            env: Vec::new(),
        }
    }

    /// `npx -y <package>` through the `npx` executable at `npx`.
    #[must_use]
    pub fn npx(npx: impl Into<PathBuf>, package: &str) -> Self {
        Self {
            prefix: vec!["-y".to_owned(), package.to_owned()],
            ..Self::new(npx)
        }
    }

    /// The command `program prefix... args...`, killed when dropped.
    #[must_use]
    pub fn command<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.prefix)
            .args(args)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }
}

/// Runs `command` to completion and returns its stdout.
///
/// # Errors
///
/// A message with the command and the last line of its stderr when it cannot start or exits unsuccessfully.
pub async fn run(mut command: Command) -> Result<String, String> {
    let shown = shown(&command);
    let output = command
        .output()
        .await
        .map_err(|error| format!("cannot start {shown}: {error}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last = stderr.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("");
    Err(format!("{shown} exited with {}: {last}", output.status))
}

/// `command` as one line: the program and its arguments.
fn shown(command: &Command) -> String {
    let command = command.as_std();
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The documentation folder of the local copy of `case`, as its real path: absolute, without `..` components or
/// symbolic links. Grounded Docs gets it as a `file://` URL, matches its exclusions against it, and refuses a path
/// through a symbolic link; Neuledge only needs the folder to exist, and is given the checkout as configured.
///
/// # Errors
///
/// [`SystemUnavailable`] with the reason when the path cannot be resolved (it is missing, or a folder on the way
/// cannot be searched) or is not a folder.
pub fn local_docs(case: &ScopeCase) -> Result<PathBuf, SystemUnavailable> {
    let docs = case.docs();
    let resolved = std::fs::canonicalize(&docs)
        .and_then(|real| std::fs::metadata(&real).map(|metadata| (real, metadata.is_dir())));
    let problem = match resolved {
        Ok((real, true)) => return Ok(real),
        Ok(_) => "not a folder".to_owned(),
        Err(error) => error.to_string(),
    };
    Err(SystemUnavailable(format!(
        "no usable local copy of {} at {}: {problem}; CONTRIBUTING says how to fetch the corpus sources",
        case.key,
        docs.display()
    )))
}

/// The response to one tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct Called {
    /// Every text block of the result, joined by line breaks.
    pub text: String,
    /// The structured content, if any.
    pub structured: Option<Value>,
    /// The time from sending the call to receiving the response.
    pub latency: Duration,
}

/// An initialized MCP client session.
pub struct McpClient {
    service: RunningService<RoleClient, ()>,
}

impl McpClient {
    /// Starts `command` and initializes a session with it over stdio. Its stderr is discarded.
    ///
    /// # Errors
    ///
    /// A message when the program cannot start or the handshake fails.
    pub async fn spawn(command: Command) -> Result<Self, String> {
        let shown = shown(&command);
        let (transport, _) = TokioChildProcess::builder(command)
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("cannot start {shown}: {error}"))?;
        let service = ().serve(transport).await.map_err(|error| format!("{shown}: {error}"))?;
        Ok(Self { service })
    }

    /// Initializes a session with the Streamable HTTP endpoint at `url`, sending `token` as a bearer token if given.
    ///
    /// # Errors
    ///
    /// A message when the endpoint cannot be reached or the handshake fails.
    pub async fn http(url: &str, token: Option<&str>) -> Result<Self, String> {
        let mut config = StreamableHttpClientTransportConfig::with_uri(url);
        if let Some(token) = token {
            config = config.auth_header(token);
        }
        let service =
            ().serve(StreamableHttpClientTransport::from_config(config))
                .await
                .map_err(|error| format!("{url}: {error}"))?;
        Ok(Self { service })
    }

    /// Calls `tool` with `arguments` and times the call.
    ///
    /// # Errors
    ///
    /// A message when the call fails or the tool reports an error.
    pub async fn call(&self, tool: &str, arguments: Map<String, Value>) -> Result<Called, String> {
        let start = Instant::now();
        let result = self
            .service
            .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments))
            .await
            .map_err(|error| format!("{tool}: {error}"))?;
        let latency = start.elapsed();
        let text = text_of(&result);
        if result.is_error == Some(true) {
            return Err(format!("{tool} returned an error: {text}"));
        }
        Ok(Called {
            text,
            structured: result.structured_content,
            latency,
        })
    }

    /// The names of the server's tools.
    ///
    /// # Errors
    ///
    /// A message when the listing fails.
    pub async fn list_tools(&self) -> Result<Vec<String>, String> {
        let tools = self
            .service
            .list_all_tools()
            .await
            .map_err(|error| format!("tools/list: {error}"))?;
        Ok(tools.into_iter().map(|tool| tool.name.into_owned()).collect())
    }

    /// Ends the session and waits for a child server to exit after its stdin closes.
    pub async fn close(self) {
        // The session is over either way; a failed join only means the task had already ended.
        let _ = self.service.cancel().await;
    }
}

/// The text blocks of `result`, joined by line breaks.
#[must_use]
pub fn text_of(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| content.as_text())
        .map(|text| text.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A JSON object of `pairs`, leaving out `None` values.
#[must_use]
pub fn object(pairs: &[(&str, Option<Value>)]) -> Map<String, Value> {
    pairs
        .iter()
        .filter_map(|(key, value)| value.clone().map(|value| ((*key).to_owned(), value)))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;

    use rmcp::model::ContentBlock as Content;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn a_local_copy_is_its_real_path() {
        let (root, mut corpus) = crate::corpus::tests::corpus();
        let real = root.path().join("real");
        std::fs::create_dir_all(real.join("content")).unwrap();
        std::os::unix::fs::symlink(&real, root.path().join("link")).unwrap();
        let tokio = &mut corpus.scopes[0];
        tokio.checkout = root.path().join("link/../link");
        assert_eq!(
            local_docs(tokio).unwrap(),
            std::fs::canonicalize(real.join("content")).unwrap()
        );
        std::fs::write(real.join("file.md"), "").unwrap();
        let not_found = std::fs::metadata(real.join("missing")).unwrap_err().to_string();
        for (path, problem) in [("file.md", "not a folder"), ("missing", not_found.as_str())] {
            tokio.path = path.to_owned();
            assert_eq!(
                local_docs(tokio).unwrap_err().0,
                format!(
                    "no usable local copy of tokio at {}: {problem}; CONTRIBUTING says how to fetch the corpus sources",
                    tokio.docs().display()
                )
            );
        }
    }

    /// The stand-in program in `fixtures/eval/stub_mcp.py`.
    pub(crate) fn stub_program() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/eval/stub_mcp.py")
    }

    /// A recorded response in `fixtures/eval/`.
    pub(crate) fn fixture(name: &str) -> String {
        std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/eval")
                .join(name),
        )
        .unwrap()
    }

    /// A directory for the stub's canned responses and call log, and a launcher of the stub that uses it.
    pub(crate) fn stub() -> (TempDir, Launcher) {
        let dir = TempDir::new().unwrap();
        let mut launcher = Launcher::new(stub_program());
        launcher
            .env
            .push(("STUB_DIR".to_owned(), dir.path().to_string_lossy().into_owned()));
        (dir, launcher)
    }

    /// The JSON lines the stub logged.
    pub(crate) fn calls(dir: &TempDir) -> Vec<Value> {
        std::fs::read_to_string(dir.path().join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn launchers_put_the_prefix_before_the_arguments() {
        let launcher = Launcher::npx("/usr/bin/npx", "pkg@1.0.0");
        let command = launcher.command(&["serve", "--http"]);
        assert_eq!(shown(&command), "/usr/bin/npx -y pkg@1.0.0 serve --http");
        assert_eq!(launcher.env, []);
    }

    #[tokio::test]
    async fn run_reports_how_a_command_failed() {
        let (dir, mut launcher) = stub();
        let stdout = run(launcher.command(&["scrape", "lib"])).await.unwrap();
        assert_eq!(stdout, "Successfully scraped 2 pages\n");
        assert_eq!(calls(&dir)[0]["argv"], json!(["scrape", "lib"]));
        launcher.env.push(("STUB_PREPARE_EXIT".to_owned(), "3".to_owned()));
        let error = run(launcher.command(&["add"])).await.unwrap_err();
        assert!(
            error.ends_with("stub_mcp.py add exited with exit status: 3: stub prepare failed"),
            "{error}"
        );
        let error = run(launcher.command(&["other"])).await.unwrap_err();
        assert!(error.ends_with("exited with exit status: 2: "), "{error}");
        let error = run(Launcher::new("/nonexistent/program").command::<&str>(&[]))
            .await
            .unwrap_err();
        assert!(error.starts_with("cannot start /nonexistent/program: "), "{error}");
    }

    #[tokio::test]
    async fn a_stdio_session_calls_tools_and_times_them() {
        let (dir, launcher) = stub();
        std::fs::write(dir.path().join("search.txt"), "found it").unwrap();
        std::fs::write(dir.path().join("search.json"), "{\"results\": []}").unwrap();
        let client = McpClient::spawn(launcher.command(&["serve"])).await.unwrap();
        let called = client
            .call("search", object(&[("query", Some(json!("x"))), ("version", None)]))
            .await
            .unwrap();
        assert_eq!(called.text, "found it");
        assert_eq!(called.structured, Some(json!({"results": []})));
        assert!(called.latency > Duration::ZERO);
        assert_eq!(calls(&dir)[1]["arguments"], json!({"query": "x"}));
        let error = client
            .call("search", object(&[("query", Some(json!("fail")))]))
            .await
            .unwrap_err();
        assert_eq!(error, "search returned an error: stub failure");
        let error = client
            .call("search", object(&[("query", Some(json!("crash")))]))
            .await
            .unwrap_err();
        assert!(error.starts_with("search: "), "{error}");
    }

    #[tokio::test]
    async fn a_program_that_cannot_start_or_speak_mcp_is_reported() {
        let error = McpClient::spawn(Launcher::new("/nonexistent/program").command(&["serve"]))
            .await
            .err()
            .unwrap();
        assert!(
            error.starts_with("cannot start /nonexistent/program serve: "),
            "{error}"
        );
        let (_dir, launcher) = stub();
        let error = McpClient::spawn(launcher.command(&["other"])).await.err().unwrap();
        assert!(error.contains("stub_mcp.py other: "), "{error}");
    }

    #[tokio::test]
    async fn an_unreachable_http_endpoint_is_reported() {
        let error = McpClient::http("http://127.0.0.1:9/mcp", Some("token"))
            .await
            .err()
            .unwrap();
        assert!(error.starts_with("http://127.0.0.1:9/mcp: "), "{error}");
    }

    #[test]
    fn only_text_blocks_make_up_the_text() {
        let result = CallToolResult::success(vec![
            Content::text("one"),
            Content::image("aGk=", "image/png"),
            Content::text("two"),
        ]);
        assert_eq!(text_of(&result), "one\ntwo");
    }
}
