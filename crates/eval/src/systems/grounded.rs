//! Grounded Docs (`@arabold/docs-mcp-server`), full-text only: no embedding model is configured, so BM25 over its
//! SQLite FTS5 index ranks alone. It indexes the local copy of every corpus version, then serves over stdio MCP.

use std::path::Path;

use serde_json::json;

use super::{Launcher, McpClient, QueryFailure, Ranked, RetrievalSystem, SystemUnavailable, object, run};
use crate::corpus::{Corpus, ScopeCase};
use crate::questions::Question;

/// The pinned package.
pub const PACKAGE: &str = "@arabold/docs-mcp-server@3.4.1";

/// The line of 60 dashes and the label that open every result of a `search_docs` response.
pub const SEPARATOR: &str = "\n------------------------------------------------------------\nResult ";

/// The results of a `search_docs` response, without their `N: <url>` label lines.
#[must_use]
pub fn split_results(text: &str) -> Vec<String> {
    text.split(SEPARATOR)
        .skip(1)
        .map(|result| result.split_once('\n').map_or("", |(_, body)| body).trim().to_owned())
        .collect()
}

/// Folder names that Grounded Docs excludes by default wherever they appear in a path (its
/// `DEFAULT_FOLDER_EXCLUSIONS`), so a local copy under one of them is indexed as one empty page and every search finds
/// nothing. `target` is the one the default `--sources` lies under.
pub const EXCLUDED_FOLDERS: [&str; 17] = [
    "archive",
    "archived",
    "deprecated",
    "legacy",
    "old",
    "outdated",
    "previous",
    "superseded",
    "test",
    "tests",
    "__tests__",
    "spec",
    "dist",
    "build",
    "out",
    "target",
    ".next",
];

/// The first folder of `docs` that Grounded Docs would exclude, if any.
fn excluded_folder(docs: &Path) -> Option<String> {
    docs.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .find(|name| EXCLUDED_FOLDERS.contains(&name.as_str()))
}

/// A Grounded Docs server on its own store.
pub struct Grounded {
    client: McpClient,
}

/// The arguments shared by every Grounded Docs command: the store, and no logo.
fn common(store: &Path) -> Vec<String> {
    vec![
        "--store-path".to_owned(),
        store.to_string_lossy().into_owned(),
        "--logo".to_owned(),
        "false".to_owned(),
    ]
}

impl Grounded {
    /// Indexes every corpus version into a fresh store under `work`, then starts the MCP server on it.
    ///
    /// # Errors
    ///
    /// [`SystemUnavailable`] when a local copy is missing, indexing fails, or the server cannot start.
    pub async fn start(launcher: &Launcher, corpus: &Corpus, work: &Path) -> Result<Self, SystemUnavailable> {
        let store = work.join("grounded");
        // A missing store is the usual case; any other failure surfaces when indexing writes to it.
        let _ = std::fs::remove_dir_all(&store);
        for case in &corpus.scopes {
            let docs = super::local_docs(case)?;
            if let Some(name) = excluded_folder(&docs) {
                return Err(SystemUnavailable(format!(
                    "cannot index {}: Grounded Docs skips every path under a `{name}` folder, and {} is under one; \
                     check the sources out elsewhere and pass `--sources`",
                    case.key,
                    docs.display()
                )));
            }
            let url = format!("file://{}", docs.display());
            let mut args = vec![
                "scrape".to_owned(),
                case.library.clone(),
                url,
                "--version".to_owned(),
                case.version.clone(),
                "--replace".to_owned(),
                "--max-pages".to_owned(),
                "100000".to_owned(),
                "--max-depth".to_owned(),
                "100".to_owned(),
            ];
            args.extend(common(&store));
            let mut command = launcher.command(&args);
            command
                .env("DOCS_MCP_TELEMETRY", "false")
                .env("DOCS_MCP_SCRAPER_SECURITY_FILE_ACCESS_ALLOWED_ROOTS", &docs);
            run(command)
                .await
                .map_err(|error| SystemUnavailable(format!("cannot index {}: {error}", case.key)))?;
        }
        let mut args = vec!["mcp", "--protocol", "stdio", "--read-only"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        args.extend(common(&store));
        let mut command = launcher.command(&args);
        command.env("DOCS_MCP_TELEMETRY", "false");
        let client = McpClient::spawn(command).await.map_err(SystemUnavailable)?;
        Ok(Self { client })
    }
}

impl RetrievalSystem for Grounded {
    async fn query(&mut self, question: &Question, case: &ScopeCase, k: usize) -> Result<Ranked, QueryFailure> {
        let arguments = object(&[
            ("library", Some(json!(case.library))),
            ("version", Some(json!(case.version))),
            ("query", Some(json!(question.query))),
            ("limit", Some(json!(k))),
        ]);
        let called = self
            .client
            .call("search_docs", arguments)
            .await
            .map_err(QueryFailure::Failed)?;
        Ok(Ranked {
            sections: split_results(&called.text),
            raw: called.text,
            latency: called.latency,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::corpus::tests::corpus;
    use crate::questions::tests::question;
    use crate::systems::tests::{calls, fixture, stub};

    /// Creates the local copy of every corpus version.
    pub(crate) fn checkouts(corpus: &Corpus) {
        for case in &corpus.scopes {
            std::fs::create_dir_all(case.docs()).unwrap();
        }
    }

    #[test]
    fn a_recorded_response_splits_into_its_results() {
        let results = split_results(&fixture("grounded_search.txt"));
        assert_eq!(results.len(), 2);
        assert!(results[0].starts_with("---\ndate: \"2022-10-27\"\ntitle: \"Announcing async-backtrace\""));
        assert!(results[1].contains("tokio::select!"));
        assert_eq!(split_results("No results found."), [] as [String; 0]);
        assert_eq!(split_results(&format!("{SEPARATOR}1: file:///x")), [""]);
    }

    #[tokio::test]
    async fn every_version_is_indexed_then_searched_by_library_and_version() {
        let (root, corpus) = corpus();
        checkouts(&corpus);
        let (dir, launcher) = stub();
        std::fs::write(dir.path().join("search_docs.txt"), fixture("grounded_search.txt")).unwrap();
        let work = root.path().join("work");
        std::fs::create_dir_all(work.join("grounded")).unwrap();
        let mut system = Grounded::start(&launcher, &corpus, &work).await.unwrap();
        let calls = calls(&dir);
        let store = work.join("grounded");
        let store = store.to_str().unwrap();
        let tokio = corpus.scopes[0].docs();
        assert_eq!(
            calls[0]["argv"],
            json!([
                "scrape",
                "tokio",
                format!("file://{}", tokio.display()),
                "--version",
                "latest",
                "--replace",
                "--max-pages",
                "100000",
                "--max-depth",
                "100",
                "--store-path",
                store,
                "--logo",
                "false"
            ])
        );
        assert_eq!(calls[0]["env"]["DOCS_MCP_TELEMETRY"], "false");
        assert_eq!(
            calls[0]["env"]["DOCS_MCP_SCRAPER_SECURITY_FILE_ACCESS_ALLOWED_ROOTS"],
            tokio.to_str().unwrap()
        );
        assert_eq!(calls[2]["argv"][3], "--version");
        assert_eq!(calls[2]["argv"][4], "2.12");
        assert_eq!(
            calls[3]["argv"],
            json!([
                "mcp",
                "--protocol",
                "stdio",
                "--read-only",
                "--store-path",
                store,
                "--logo",
                "false"
            ])
        );
        let case = &corpus.scopes[1];
        let ranked = system
            .query(&question("q", "pydantic", Some("1.10"), &["x"]), case, 10)
            .await
            .unwrap();
        assert_eq!(ranked.sections.len(), 2);
        let arguments: Value = crate::systems::tests::calls(&dir)[4]["arguments"].clone();
        assert_eq!(
            arguments,
            json!({"library": "pydantic", "version": "1.10", "query": "query of q", "limit": 10})
        );
        let mut failing = question("q", "pydantic", Some("1.10"), &["x"]);
        failing.query = "fail".to_owned();
        let error = system.query(&failing, case, 10).await.unwrap_err();
        assert_eq!(
            error,
            QueryFailure::Failed("search_docs returned an error: stub failure".to_owned())
        );
    }

    #[tokio::test]
    async fn missing_copies_failed_indexing_and_a_dead_server_make_it_unavailable() {
        let (root, corpus) = corpus();
        let (_dir, mut launcher) = stub();
        let work = root.path().join("work");
        let error = Grounded::start(&launcher, &corpus, &work).await.err().unwrap();
        assert!(error.0.starts_with("no local copy of tokio at "), "{error:?}");
        checkouts(&corpus);
        launcher.env.push(("STUB_PREPARE_EXIT".to_owned(), "1".to_owned()));
        let error = Grounded::start(&launcher, &corpus, &work).await.err().unwrap();
        assert!(error.0.starts_with("cannot index tokio: "), "{error:?}");
        assert!(error.0.ends_with(": stub prepare failed"), "{error:?}");
        let error = Grounded::start(&Launcher::new("/bin/true"), &corpus, &work)
            .await
            .err()
            .unwrap();
        assert!(error.0.starts_with("/bin/true mcp --protocol stdio"), "{error:?}");
    }

    #[tokio::test]
    async fn a_local_copy_under_a_folder_grounded_docs_excludes_makes_it_unavailable() {
        let (root, mut corpus) = corpus();
        let checkout = root.path().join("target/eval-sources/tokio");
        corpus.scopes[0].checkout.clone_from(&checkout);
        checkouts(&corpus);
        let (_dir, launcher) = stub();
        let error = Grounded::start(&launcher, &corpus, &root.path().join("work"))
            .await
            .err()
            .unwrap();
        assert!(
            error
                .0
                .starts_with("cannot index tokio: Grounded Docs skips every path under a `target` folder"),
            "{error:?}"
        );
        assert_eq!(excluded_folder(Path::new("/home/u/sources/tokio/content")), None);
        assert_eq!(excluded_folder(Path::new("/srv/build/x")).as_deref(), Some("build"));
    }
}
