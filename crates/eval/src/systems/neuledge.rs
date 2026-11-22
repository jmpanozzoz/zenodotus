//! Neuledge Context (`@neuledge/context`): one package built from the local copy of every corpus version, served
//! over stdio MCP. Its data lives under `HOME`, so it gets a home directory of its own.

use std::path::Path;

use serde::Deserialize;
use serde_json::json;

use super::{Launcher, McpClient, QueryFailure, Ranked, RetrievalSystem, SystemUnavailable, object, run};
use crate::corpus::{Corpus, ScopeCase};
use crate::questions::Question;

/// The pinned package.
pub const PACKAGE: &str = "@neuledge/context@1.4.4";

/// A `get_docs` response.
#[derive(Debug, Deserialize)]
struct Response {
    results: Vec<Item>,
}

/// One result of a `get_docs` response.
#[derive(Debug, Deserialize)]
struct Item {
    title: String,
    content: String,
}

/// The results of a `get_docs` response, each as its title heading and content.
///
/// # Errors
///
/// A message when the response is not the expected JSON.
pub fn parse_results(text: &str) -> Result<Vec<String>, String> {
    let response: Response =
        serde_json::from_str(text).map_err(|error| format!("unreadable get_docs response: {error}"))?;
    Ok(response
        .results
        .into_iter()
        .map(|item| format!("## {}\n\n{}", item.title, item.content))
        .collect())
}

/// The package name of `case`: `library@version`.
fn package(case: &ScopeCase) -> String {
    format!("{}@{}", case.library, case.version)
}

/// A Neuledge Context server.
pub struct Neuledge {
    client: McpClient,
}

impl Neuledge {
    /// Builds a package of every corpus version in a fresh home directory under `work`, then starts the MCP server
    /// restricted to them. npm keeps its cache in `work/npm-cache`, so the package is downloaded once.
    ///
    /// # Errors
    ///
    /// [`SystemUnavailable`] when a local copy cannot be resolved or is not a folder, a package cannot be built,
    /// or the server cannot start.
    pub async fn start(launcher: &Launcher, corpus: &Corpus, work: &Path) -> Result<Self, SystemUnavailable> {
        let home = work.join("neuledge");
        // A missing home is the usual case; any other failure surfaces when a package is built in it.
        let _ = std::fs::remove_dir_all(&home);
        let cache = work.join("npm-cache");
        let isolated = |mut command: tokio::process::Command| {
            command.env("HOME", &home).env("npm_config_cache", &cache);
            command
        };
        for case in &corpus.scopes {
            super::local_docs(case)?;
            let checkout = case.checkout.to_string_lossy();
            let args = [
                "add",
                &checkout,
                "--path",
                &case.path,
                "--name",
                &case.library,
                "--pkg-version",
                &case.version,
            ];
            run(isolated(launcher.command(&args)))
                .await
                .map_err(|error| SystemUnavailable(format!("cannot build {}: {error}", case.key)))?;
        }
        let mut args = vec!["serve".to_owned(), "--libs".to_owned()];
        args.extend(corpus.scopes.iter().map(package));
        let client = McpClient::spawn(isolated(launcher.command(&args)))
            .await
            .map_err(SystemUnavailable)?;
        Ok(Self { client })
    }
}

impl RetrievalSystem for Neuledge {
    async fn query(&mut self, question: &Question, case: &ScopeCase, _k: usize) -> Result<Ranked, QueryFailure> {
        let arguments = object(&[
            ("library", Some(json!(package(case)))),
            ("topic", Some(json!(question.query))),
        ]);
        let called = self
            .client
            .call("get_docs", arguments)
            .await
            .map_err(QueryFailure::Failed)?;
        Ok(Ranked {
            sections: parse_results(&called.text).map_err(QueryFailure::Failed)?,
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
    use crate::systems::tests::{calls, fixture, stub};

    #[test]
    fn recorded_responses_parse_into_titled_results() {
        let results = parse_results(&fixture("neuledge_get_docs.json")).unwrap();
        assert!(results.len() >= 2);
        assert!(
            results[0].starts_with("## Glossary > Valves\n\nA valve is what the [`plumbing::open_valve`] function")
        );
        assert_eq!(
            parse_results(&fixture("neuledge_empty.json")).unwrap(),
            [] as [String; 0]
        );
        let error = parse_results("not json").unwrap_err();
        assert!(error.starts_with("unreadable get_docs response: "), "{error}");
    }

    #[tokio::test]
    async fn every_version_becomes_a_package_in_an_isolated_home() {
        let (root, corpus) = corpus();
        for case in &corpus.scopes {
            std::fs::create_dir_all(case.docs()).unwrap();
        }
        let (dir, launcher) = stub();
        std::fs::write(dir.path().join("get_docs.txt"), fixture("neuledge_get_docs.json")).unwrap();
        let work = root.path().join("work");
        let mut system = Neuledge::start(&launcher, &corpus, &work).await.unwrap();
        let calls = calls(&dir);
        let checkout = corpus.scopes[1].checkout.to_str().unwrap();
        assert_eq!(
            calls[1]["argv"],
            json!([
                "add",
                checkout,
                "--path",
                "docs",
                "--name",
                "pydantic",
                "--pkg-version",
                "1.10"
            ])
        );
        assert_eq!(calls[1]["env"]["HOME"], work.join("neuledge").to_str().unwrap());
        assert_eq!(
            calls[1]["env"]["npm_config_cache"],
            work.join("npm-cache").to_str().unwrap()
        );
        assert_eq!(
            calls[3]["argv"],
            json!(["serve", "--libs", "tokio@latest", "pydantic@1.10", "pydantic@2.12"])
        );
        let case = &corpus.scopes[0];
        let ranked = system
            .query(&question("q", "tokio", None, &["x"]), case, 10)
            .await
            .unwrap();
        assert!(ranked.sections.len() >= 2);
        assert_eq!(
            crate::systems::tests::calls(&dir)[4]["arguments"],
            json!({"library": "tokio@latest", "topic": "query of q"})
        );
        std::fs::write(dir.path().join("get_docs.txt"), "oops").unwrap();
        let error = system
            .query(&question("q", "tokio", None, &["x"]), case, 10)
            .await
            .unwrap_err();
        assert!(matches!(error, QueryFailure::Failed(message) if message.starts_with("unreadable")));
        let mut failing = question("q", "tokio", None, &["x"]);
        failing.query = "fail".to_owned();
        let error = system.query(&failing, case, 10).await.unwrap_err();
        assert_eq!(
            error,
            QueryFailure::Failed("get_docs returned an error: stub failure".to_owned())
        );
    }

    #[tokio::test]
    async fn missing_copies_failed_builds_and_a_dead_server_make_it_unavailable() {
        let (root, corpus) = corpus();
        let (_dir, mut launcher) = stub();
        let work = root.path().join("work");
        let error = Neuledge::start(&launcher, &corpus, &work).await.err().unwrap();
        assert!(error.0.starts_with("no usable local copy of tokio at "), "{error:?}");
        for case in &corpus.scopes {
            std::fs::create_dir_all(case.docs()).unwrap();
        }
        launcher.env.push(("STUB_PREPARE_EXIT".to_owned(), "1".to_owned()));
        let error = Neuledge::start(&launcher, &corpus, &work).await.err().unwrap();
        assert!(error.0.starts_with("cannot build tokio: "), "{error:?}");
        let error = Neuledge::start(&Launcher::new("/bin/true"), &corpus, &work)
            .await
            .err()
            .unwrap();
        assert!(error.0.starts_with("/bin/true serve --libs"), "{error:?}");
    }
}
