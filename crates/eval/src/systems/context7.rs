//! Context7, through its hosted Streamable HTTP endpoint, with the library id of each version pinned in
//! `eval/context7.toml`.

use serde_json::json;

use super::{McpClient, QueryFailure, Ranked, RetrievalSystem, SystemUnavailable, object};
use crate::corpus::ScopeCase;
use crate::questions::Question;

/// The hosted endpoint.
pub const DEFAULT_URL: &str = "https://mcp.context7.com/mcp";

/// The line of 32 dashes between two snippets of a `query-docs` response.
pub const SEPARATOR: &str = "\n--------------------------------\n";

/// The start of a response that serves another version than the one requested.
pub const VERSION_NOTE: &str = "Version note: ";

/// The snippets of a `query-docs` response, in the order returned.
#[must_use]
pub fn split_snippets(text: &str) -> Vec<String> {
    text.split(SEPARATOR)
        .map(str::trim)
        .filter(|snippet| !snippet.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether a response says that the requested version is not indexed and another one is served instead.
#[must_use]
pub fn serves_another_version(text: &str) -> bool {
    text.starts_with(VERSION_NOTE) && text.contains("has no indexed documentation for version")
}

/// A Context7 session.
pub struct Context7 {
    client: McpClient,
}

impl Context7 {
    /// Opens a session with the endpoint at `url`, sending `token` (a Context7 API key) as a bearer token if given.
    ///
    /// # Errors
    ///
    /// [`SystemUnavailable`] when the endpoint cannot be reached.
    pub async fn start(url: &str, token: Option<&str>) -> Result<Self, SystemUnavailable> {
        let client = McpClient::http(url, token).await.map_err(SystemUnavailable)?;
        Ok(Self { client })
    }
}

impl RetrievalSystem for Context7 {
    async fn query(&mut self, question: &Question, case: &ScopeCase, _k: usize) -> Result<Ranked, QueryFailure> {
        let Some(library_id) = &case.context7 else {
            return Err(QueryFailure::VersionUnavailable);
        };
        let arguments = object(&[
            ("libraryId", Some(json!(library_id))),
            ("query", Some(json!(question.query))),
        ]);
        let called = self
            .client
            .call("query-docs", arguments)
            .await
            .map_err(QueryFailure::Failed)?;
        if serves_another_version(&called.text) {
            return Err(QueryFailure::VersionUnavailable);
        }
        Ok(Ranked {
            sections: split_snippets(&called.text),
            raw: called.text,
            latency: called.latency,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::Value;
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    use super::*;
    use crate::corpus::tests::corpus;
    use crate::questions::tests::question;
    use crate::systems::tests::fixture;

    /// A Streamable HTTP MCP server that answers every tool call with `text`, as a tool error when `is_error`.
    pub(crate) struct FakeMcp {
        pub(crate) text: String,
        pub(crate) is_error: bool,
    }

    impl Respond for FakeMcp {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let Some(id) = body.get("id") else {
                return ResponseTemplate::new(202);
            };
            let result = if body["method"] == "initialize" {
                json!({
                    "protocolVersion": body["params"]["protocolVersion"],
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fake", "version": "0"},
                })
            } else {
                json!({"content": [{"type": "text", "text": self.text}], "isError": self.is_error})
            };
            ResponseTemplate::new(200).set_body_json(json!({"jsonrpc": "2.0", "id": id, "result": result}))
        }
    }

    /// A fake endpoint answering `text`, and its URL.
    pub(crate) async fn fake(text: &str, is_error: bool) -> (MockServer, String) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(FakeMcp {
                text: text.to_owned(),
                is_error,
            })
            .mount(&server)
            .await;
        let url = format!("{}/mcp", server.uri());
        (server, url)
    }

    #[test]
    fn a_recorded_response_splits_into_its_snippets() {
        let snippets = split_snippets(&fixture("context7_query.txt"));
        assert_eq!(snippets.len(), 5);
        assert!(snippets[0].starts_with("### spawn\n\nSource: https://docs.rs/tokio/"));
        assert!(snippets.iter().all(|snippet| snippet.starts_with("### ")));
        assert_eq!(
            split_snippets(&format!("{SEPARATOR}a{SEPARATOR}{SEPARATOR}b\n")),
            ["a", "b"]
        );
    }

    #[test]
    fn a_version_note_marks_another_version() {
        assert!(serves_another_version(&fixture("context7_version_note.txt")));
        assert!(!serves_another_version(&fixture("context7_query.txt")));
        assert!(!serves_another_version("Version note: served v2.12.5 as requested"));
    }

    #[tokio::test]
    async fn queries_send_the_pinned_id_and_split_the_answer() {
        let (_root, corpus) = corpus();
        let (server, url) = fake(&fixture("context7_query.txt"), false).await;
        let mut system = Context7::start(&url, None).await.unwrap();
        let tokio = &corpus.scopes[0];
        let ranked = system
            .query(&question("q", "tokio", None, &["x"]), tokio, 10)
            .await
            .unwrap();
        assert_eq!(ranked.sections.len(), 5);
        assert_eq!(ranked.raw, fixture("context7_query.txt"));
        let requests = server.received_requests().await.unwrap();
        let call: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
        assert_eq!(call["params"]["name"], "query-docs");
        assert_eq!(
            call["params"]["arguments"],
            json!({"libraryId": "/websites/rs_tokio", "query": "query of q"})
        );
        let unpinned = &corpus.scopes[1];
        let error = system
            .query(&question("q", "pydantic", Some("1.10"), &["x"]), unpinned, 10)
            .await
            .unwrap_err();
        assert_eq!(error, QueryFailure::VersionUnavailable);
    }

    #[tokio::test]
    async fn another_version_and_tool_errors_are_query_failures() {
        let (_root, corpus) = corpus();
        let tokio = &corpus.scopes[0];
        let (_server, url) = fake(&fixture("context7_version_note.txt"), false).await;
        let mut system = Context7::start(&url, None).await.unwrap();
        let error = system
            .query(&question("q", "tokio", None, &["x"]), tokio, 10)
            .await
            .unwrap_err();
        assert_eq!(error, QueryFailure::VersionUnavailable);
        let (_server, url) = fake("Rate limited", true).await;
        let mut system = Context7::start(&url, None).await.unwrap();
        let error = system
            .query(&question("q", "tokio", None, &["x"]), tokio, 10)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            QueryFailure::Failed("query-docs returned an error: Rate limited".to_owned())
        );
    }

    #[tokio::test]
    async fn an_api_key_is_sent_as_a_bearer_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("authorization", "Bearer secret"))
            .respond_with(FakeMcp {
                text: String::new(),
                is_error: false,
            })
            .expect(2..)
            .mount(&server)
            .await;
        Context7::start(&format!("{}/mcp", server.uri()), Some("secret"))
            .await
            .unwrap();
        let error = Context7::start("http://127.0.0.1:9/mcp", None).await.err().unwrap();
        assert!(error.0.starts_with("http://127.0.0.1:9/mcp: "), "{error:?}");
    }
}
