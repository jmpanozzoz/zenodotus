//! zenodotus, through `zenodotus --config eval/corpus.toml serve` over stdio MCP, on the index synced from the corpus.

use serde_json::{Value, json};

use super::{Launcher, McpClient, QueryFailure, Ranked, RetrievalSystem, SystemUnavailable, object};
use crate::corpus::{Corpus, ScopeCase};
use crate::questions::Question;

/// A running `zenodotus serve`.
pub struct Zenodotus {
    client: McpClient,
}

impl Zenodotus {
    /// Starts the `zenodotus` binary that `launcher` runs on the corpus config.
    ///
    /// # Errors
    ///
    /// [`SystemUnavailable`] when the server cannot start.
    pub async fn start(launcher: &Launcher, corpus: &Corpus) -> Result<Self, SystemUnavailable> {
        let config = corpus.config_path.as_os_str();
        let command = launcher.command(&["--config".as_ref(), config, "serve".as_ref()]);
        let client = McpClient::spawn(command).await.map_err(SystemUnavailable)?;
        Ok(Self { client })
    }
}

impl RetrievalSystem for Zenodotus {
    async fn query(&mut self, question: &Question, _case: &ScopeCase, k: usize) -> Result<Ranked, QueryFailure> {
        let arguments = object(&[
            ("library_id", Some(json!(question.library))),
            ("query", Some(json!(question.query))),
            ("version", question.version.as_ref().map(|version| json!(version))),
            ("max_results", Some(json!(k))),
        ]);
        let called = self
            .client
            .call("query_docs", arguments)
            .await
            .map_err(QueryFailure::Failed)?;
        let Some(results) = called
            .structured
            .as_ref()
            .and_then(|structured| structured.get("results"))
            .and_then(Value::as_array)
        else {
            return Err(QueryFailure::Failed(format!(
                "query_docs returned no results array: {}",
                called.text
            )));
        };
        let sections = results
            .iter()
            .map(|item| item.get("text").and_then(Value::as_str).unwrap_or_default().to_owned())
            .collect();
        Ok(Ranked {
            sections,
            raw: called.text,
            latency: called.latency,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::tests::corpus;
    use crate::questions::tests::question;
    use crate::systems::tests::{calls, stub};

    #[tokio::test]
    async fn queries_go_to_query_docs_and_results_come_from_the_structured_content() {
        let (_root, corpus) = corpus();
        let (dir, launcher) = stub();
        std::fs::write(dir.path().join("query_docs.txt"), "rendered").unwrap();
        std::fs::write(
            dir.path().join("query_docs.json"),
            r#"{"results": [{"text": "first"}, {"text": "second"}, {"rank": 3}]}"#,
        )
        .unwrap();
        let mut system = Zenodotus::start(&launcher, &corpus).await.unwrap();
        let case = &corpus.scopes[1];
        let ranked = system
            .query(&question("q", "pydantic", Some("1.10"), &["x"]), case, 10)
            .await
            .unwrap();
        assert_eq!(ranked.sections, ["first", "second", ""]);
        assert_eq!(ranked.raw, "rendered");
        let calls = calls(&dir);
        let config = corpus.config_path.to_str().unwrap();
        assert_eq!(calls[0]["argv"], json!(["--config", config, "serve"]));
        assert_eq!(
            calls[1]["arguments"],
            json!({"library_id": "pydantic", "query": "query of q", "version": "1.10", "max_results": 10})
        );
        let ranked = system
            .query(&question("q2", "tokio", None, &["x"]), case, 5)
            .await
            .unwrap();
        assert_eq!(ranked.sections.len(), 3);
        assert_eq!(
            calls_arguments(&dir, 2),
            json!({"library_id": "tokio", "query": "query of q2", "max_results": 5})
        );
    }

    fn calls_arguments(dir: &tempfile::TempDir, index: usize) -> Value {
        calls(dir)[index]["arguments"].clone()
    }

    #[tokio::test]
    async fn failures_and_missing_results_are_query_failures() {
        let (_root, corpus) = corpus();
        let (dir, launcher) = stub();
        let mut system = Zenodotus::start(&launcher, &corpus).await.unwrap();
        let case = &corpus.scopes[0];
        let mut failing = question("q", "tokio", None, &["x"]);
        failing.query = "fail".to_owned();
        let error = system.query(&failing, case, 10).await.unwrap_err();
        assert_eq!(
            error,
            QueryFailure::Failed("query_docs returned an error: stub failure".to_owned())
        );
        std::fs::write(dir.path().join("query_docs.txt"), "no structure").unwrap();
        let error = system
            .query(&question("q", "tokio", None, &["x"]), case, 10)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            QueryFailure::Failed("query_docs returned no results array: no structure".to_owned())
        );
    }

    #[tokio::test]
    async fn a_binary_that_cannot_start_makes_the_system_unavailable() {
        let (_root, corpus) = corpus();
        let error = Zenodotus::start(&Launcher::new("/nonexistent/zenodotus"), &corpus)
            .await
            .err()
            .unwrap();
        assert!(
            error.0.starts_with("cannot start /nonexistent/zenodotus --config "),
            "{error:?}"
        );
    }
}
