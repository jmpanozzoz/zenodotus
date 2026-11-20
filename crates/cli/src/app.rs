//! One function per command.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;
use url::Url;
use zenodotus_core::config::{Config, Env, VersionSpec, locate_config};
use zenodotus_core::config_edit::{LibraryMeta, add_version, init_template, write_new};
use zenodotus_core::ids::{LibraryId, Version};
use zenodotus_core::model::{DEFAULT_MAX_PAGES, SourceSpec, VersionState};
use zenodotus_core::time::Clock;
use zenodotus_mcp::render::query_markdown;
use zenodotus_mcp::server::{MAX_CHARS, MAX_RESULTS, bounded};
use zenodotus_mcp::transport::{HttpOptions, HttpServer, MCP_PATH, serve_stdio};
use zenodotus_mcp::{DocsServer, Index, ServeError};
use zenodotus_search::SearchError;
use zenodotus_search::query::{QueryRequest, query_docs};
use zenodotus_search::store::{INDEX_DIR, Store};

use crate::args::{AddArgs, Cli, Command, SearchArgs, ServeArgs, SyncArgs};
use crate::error::AppError;
use crate::output;
use crate::sync::{self, Paths, Selection};

/// The environment variable holding the HTTP bearer token.
pub const TOKEN_VARIABLE: &str = "ZENODOTUS_HTTP_TOKEN";

/// What a command reads and writes besides its arguments.
pub struct Context<'a> {
    /// Environment variables.
    pub env: Env<'a>,
    /// The wall clock.
    pub clock: &'a dyn Clock,
    /// Where results go; logs and errors go to stderr.
    pub out: &'a mut dyn Write,
}

/// Runs the command of `cli`.
///
/// # Errors
///
/// The failure of the command; [`AppError::exit_code`] gives the process exit code.
pub async fn run(cli: Cli, ctx: &mut Context<'_>) -> Result<(), AppError> {
    let config_path = locate_config(cli.config.as_deref(), ctx.env)?;
    match cli.command {
        Command::Init => init(&config_path, ctx),
        Command::Add(args) => add(&config_path, &args, ctx),
        Command::List(flag) => list(&config_path, flag.json, ctx),
        Command::Sync(args) => sync_command(&config_path, &args, ctx),
        Command::Search(args) => search(&config_path, &args, ctx),
        Command::Serve(args) => serve(&config_path, &args, ctx).await,
        Command::Status(flag) => status(&config_path, flag.json, ctx),
    }
}

/// The loaded config and its data directory.
fn load(config_path: &Path, env: Env<'_>) -> Result<(Config, PathBuf), AppError> {
    let config = Config::load(config_path)?;
    let data_dir = config.settings.data_dir(config_path, env)?;
    Ok((config, data_dir))
}

/// `init`.
fn init(config_path: &Path, ctx: &mut Context<'_>) -> Result<(), AppError> {
    write_new(config_path, &init_template())?;
    let (_, data_dir) = load(config_path, ctx.env)?;
    std::fs::create_dir_all(&data_dir).map_err(AppError::io(&data_dir))?;
    writeln!(
        ctx.out,
        "Wrote {}\nData directory: {}",
        config_path.display(),
        data_dir.display()
    )?;
    Ok(())
}

/// `add`.
fn add(config_path: &Path, args: &AddArgs, ctx: &mut Context<'_>) -> Result<(), AppError> {
    let id = LibraryId::parse(&args.id)?;
    let spec = VersionSpec {
        version: Version::parse(&args.version)?,
        default: args.default,
        source: source_spec(args)?,
        extra_hosts: Vec::new(),
    };
    let meta = LibraryMeta {
        name: args.name.as_deref(),
        description: args.description.as_deref(),
    };
    add_version(config_path, &id, meta, &spec)?;
    writeln!(
        ctx.out,
        "Added {id}@{} from {}; run `zenodotus sync {id}` to index it",
        spec.version,
        spec.source.describe()
    )?;
    Ok(())
}

/// The source the flags of `add` describe.
fn source_spec(args: &AddArgs) -> Result<SourceSpec, AppError> {
    let source = &args.source;
    if let Some(path) = &source.path {
        return Ok(SourceSpec::Local {
            path: std::path::absolute(path).map_err(AppError::io(path))?,
            include: Vec::new(),
            exclude: Vec::new(),
        });
    }
    if let Some(repo) = &source.github {
        return Ok(SourceSpec::GitHub {
            repo: repo.clone(),
            reference: args.reference.clone(),
            paths: args.docs_path.clone(),
        });
    }
    if let Some(url) = &source.llms_txt {
        return Ok(SourceSpec::LlmsTxt { url: parse_url(url)? });
    }
    if let Some(url) = &source.website {
        return Ok(SourceSpec::Website {
            start_url: parse_url(url)?,
            sitemap: None,
            allow_prefixes: Vec::new(),
            max_pages: args.max_pages.unwrap_or(DEFAULT_MAX_PAGES),
        });
    }
    let urls = source.url.iter().map(|url| parse_url(url)).collect::<Result<_, _>>()?;
    Ok(SourceSpec::Url { urls })
}

/// `text` as a URL, or a usage error.
fn parse_url(text: &str) -> Result<Url, AppError> {
    Url::parse(text).map_err(|error| AppError::Usage(format!("invalid URL {text:?}: {error}")))
}

/// The sync records in the index of `data_dir`; none when nothing was ever synced.
fn indexed_states(data_dir: &Path, config: &Config) -> Result<Vec<VersionState>, AppError> {
    match Store::open(data_dir, &config.settings.index) {
        Ok(store) => Ok(store.version_states()?),
        Err(SearchError::IndexEmpty) => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

/// `list`.
fn list(config_path: &Path, json: bool, ctx: &mut Context<'_>) -> Result<(), AppError> {
    let (config, data_dir) = load(config_path, ctx.env)?;
    let states = indexed_states(&data_dir, &config)?;
    if json {
        writeln!(ctx.out, "{:#}", output::list_json(&config, &states))?;
    } else {
        write!(ctx.out, "{}", output::list_text(&config, &states))?;
    }
    Ok(())
}

/// `status`.
fn status(config_path: &Path, json: bool, ctx: &mut Context<'_>) -> Result<(), AppError> {
    let (config, data_dir) = load(config_path, ctx.env)?;
    let states = indexed_states(&data_dir, &config)?;
    let bytes = directory_size(&data_dir.join(INDEX_DIR))?;
    if json {
        writeln!(ctx.out, "{:#}", output::status_json(&states, bytes))?;
    } else {
        write!(ctx.out, "{}", output::status_text(&states, bytes))?;
    }
    Ok(())
}

/// The total size of the files under `path`, without following symbolic links; 0 when it does not exist.
fn directory_size(path: &Path) -> Result<u64, AppError> {
    if !path.is_dir() {
        return Ok(0);
    }
    let mut total = 0;
    for entry in std::fs::read_dir(path).map_err(AppError::io(path))? {
        let entry = entry.map_err(AppError::io(path))?;
        let metadata = entry.metadata().map_err(AppError::io(entry.path()))?;
        total += if metadata.is_dir() {
            directory_size(&entry.path())?
        } else {
            metadata.len()
        };
    }
    Ok(total)
}

/// `sync`.
fn sync_command(config_path: &Path, args: &SyncArgs, ctx: &mut Context<'_>) -> Result<(), AppError> {
    let (config, data_dir) = load(config_path, ctx.env)?;
    let selection = match (args.id.as_deref(), args.version.as_deref()) {
        _ if args.rebuild => Selection::Rebuild,
        (Some(id), Some(version)) => Selection::Version(id, version),
        (Some(id), None) => Selection::Library(id),
        (None, _) => Selection::All,
    };
    let paths = Paths {
        config: config_path,
        data_dir: &data_dir,
    };
    let report = sync::sync(&config, paths, selection, ctx.clock)?;
    write!(ctx.out, "{}", output::sync_text(&report))?;
    if report.abandoned {
        return Err(AppError::RebuildAbandoned { count: report.failed() });
    }
    match report.incomplete() {
        0 => Ok(()),
        count => Err(AppError::Incomplete { count }),
    }
}

/// `search`.
fn search(config_path: &Path, args: &SearchArgs, ctx: &mut Context<'_>) -> Result<(), AppError> {
    let max_results = bounded("--limit", Some(args.limit), MAX_RESULTS).map_err(AppError::Usage)?;
    let max_chars = bounded("--max-chars", Some(args.max_chars), MAX_CHARS).map_err(AppError::Usage)?;
    let (config, data_dir) = load(config_path, ctx.env)?;
    let store = Store::open(&data_dir, &config.settings.index)?;
    let query = args.query.join(" ");
    let request = QueryRequest {
        library_id: &args.id,
        query: &query,
        version: args.version.as_deref(),
        language: args.language.as_deref(),
        max_results,
        max_chars,
    };
    let result = query_docs(&store, &request, &config.settings.ranking)?;
    if args.json {
        serde_json::to_writer_pretty(&mut *ctx.out, &result).map_err(std::io::Error::from)?;
        writeln!(ctx.out)?;
    } else {
        write!(ctx.out, "{}", query_markdown(&result))?;
    }
    Ok(())
}

/// `serve`.
async fn serve(config_path: &Path, args: &ServeArgs, ctx: &mut Context<'_>) -> Result<(), AppError> {
    let (config, data_dir) = load(config_path, ctx.env)?;
    let index = Index::new(data_dir, config.settings.index.clone(), config.settings.ranking.clone());
    let server = DocsServer::new(Arc::new(index));
    let Some(listen) = args.http else {
        return Ok(serve_stdio(server).await?);
    };
    let options = HttpOptions::new(&config.settings.http, listen, (ctx.env)(TOKEN_VARIABLE));
    let shutdown = CancellationToken::new();
    let http = HttpServer::bind(options, server, shutdown.clone()).await?;
    let mut terminate = signal(SignalKind::terminate()).map_err(ServeError::Io)?;
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
        shutdown.cancel();
    });
    writeln!(ctx.out, "Serving MCP on http://{}{MCP_PATH}", http.local_addr()?)?;
    ctx.out.flush()?;
    Ok(http.run().await?)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::Parser;
    use tempfile::TempDir;
    use zenodotus_core::CoreError;
    use zenodotus_core::time::{FixedClock, Timestamp};

    use super::*;
    use crate::lock::SyncLock;

    /// A home directory with a docs tree; the config lives at `<home>/zenodotus.toml`.
    struct Home {
        dir: TempDir,
    }

    impl Home {
        fn new() -> Self {
            let home = Self {
                dir: TempDir::new().unwrap(),
            };
            home.write(
                "docs/guide.md",
                "# Guide\n\n## Spawning\n\nUse `tokio::spawn` to run a task.\n",
            );
            home.write("docs/intro.md", "# Intro\n\nTokio is a runtime.\n");
            home
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.dir.path().join(relative)
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.path(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        fn data_dir(&self) -> PathBuf {
            self.path(".local/share/zenodotus")
        }

        /// Runs `args` with `--config <home>/zenodotus.toml`, writing to `out`.
        async fn run_to(&self, args: &[&str], out: &mut dyn Write) -> Result<(), AppError> {
            let config = self.path("zenodotus.toml");
            let mut command_line = vec!["zenodotus", "--config", config.to_str().unwrap()];
            command_line.extend(args);
            self.run_argv(&command_line, out).await
        }

        async fn run_argv(&self, argv: &[&str], out: &mut dyn Write) -> Result<(), AppError> {
            let home = self.dir.path().display().to_string();
            let env = move |name: &str| (name == "HOME").then(|| home.clone());
            let clock = FixedClock::new(Timestamp::from_unix(1_700_000_000));
            let mut ctx = Context {
                env: &env,
                clock: &clock,
                out,
            };
            run(Cli::try_parse_from(argv).unwrap(), &mut ctx).await
        }

        /// Runs `args` and returns the output, or the error.
        async fn run(&self, args: &[&str]) -> Result<String, AppError> {
            let mut out = Vec::new();
            self.run_to(args, &mut out).await?;
            Ok(String::from_utf8(out).unwrap())
        }

        /// Runs `args`, which must succeed, and returns the output.
        async fn ok(&self, args: &[&str]) -> String {
            self.run(args).await.unwrap()
        }

        /// Runs `args`, which must fail, and returns the error.
        async fn fail(&self, args: &[&str]) -> AppError {
            self.run(args).await.unwrap_err()
        }

        /// `init`, then `add tokio --path docs --version 1 --default`.
        async fn with_tokio(self) -> Self {
            self.ok(&["init"]).await;
            let docs = self.path("docs");
            self.ok(&[
                "add",
                "tokio",
                "--version",
                "1",
                "--path",
                docs.to_str().unwrap(),
                "--default",
                "--name",
                "Tokio",
                "--description",
                "Async runtime",
            ])
            .await;
            self
        }
    }

    #[tokio::test]
    async fn init_add_sync_list_status_and_search() {
        let home = Home::new();
        let init = home.ok(&["init"]).await;
        assert!(init.starts_with("Wrote "), "{init}");
        assert!(home.data_dir().is_dir());
        let error = home.fail(&["init"]).await;
        assert!(
            matches!(error, AppError::Core(CoreError::ConfigExists { .. })),
            "{error}"
        );
        assert_eq!(error.exit_code(), 2);
        let docs = home.path("docs");
        home.ok(&[
            "add",
            "tokio",
            "--version",
            "1",
            "--path",
            docs.to_str().unwrap(),
            "--default",
            "--name",
            "Tokio",
        ])
        .await;
        assert_eq!(
            home.ok(&["list"]).await,
            format!(
                "tokio (Tokio)\n  1 (default): local:{}; not synced\n",
                home.path("docs").display()
            )
        );
        assert_eq!(
            home.ok(&["status"]).await,
            "The index is empty; run `zenodotus sync`.\nIndex size: 0 bytes\n"
        );
        assert_eq!(
            home.ok(&["sync"]).await,
            "tokio@1: ok, revision 1, 2 documents, 2 sections (2 changed, 0 deleted, 0 unchanged, 0 failed)\n"
        );
        assert!(
            home.ok(&["list"])
                .await
                .contains("revision 1, ok, last success 2023-11-14T22:13:20Z")
        );
        let status = home.ok(&["status"]).await;
        assert!(
            status.starts_with("tokio@1: ok, revision 1, 2 documents, 2 sections\n"),
            "{status}"
        );
        assert!(!status.contains("Index size: 0 bytes"), "{status}");
        let json: serde_json::Value = serde_json::from_str(&home.ok(&["status", "--json"]).await).unwrap();
        assert_eq!(json["versions"][0]["section_count"], 2);
        assert!(json["index_bytes"].as_u64().unwrap() > 0);
        let json: serde_json::Value = serde_json::from_str(&home.ok(&["list", "--json"]).await).unwrap();
        assert_eq!(json["libraries"][0]["versions"][0]["synced"]["revision"], 1);

        let markdown = home.ok(&["search", "tokio", "tokio::spawn"]).await;
        assert!(markdown.starts_with(zenodotus_mcp::render::PREAMBLE), "{markdown}");
        assert!(markdown.contains("Use `tokio::spawn` to run a task."), "{markdown}");
        let json = home
            .ok(&[
                "search",
                "tokio",
                "how",
                "to",
                "spawn",
                "--json",
                "--limit",
                "1",
                "--max-chars",
                "1000",
            ])
            .await;
        let json: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(json["version_resolution"], "default");
        assert_eq!(json["results"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_resync_reports_changed_unchanged_and_deleted_documents() {
        let home = Home::new().with_tokio().await;
        home.ok(&["sync", "tokio"]).await;
        std::fs::remove_file(home.path("docs/intro.md")).unwrap();
        home.write("docs/guide.md", "# Guide\n\nRewritten.\n");
        home.write("docs/new.md", "# New\n\nAdded.\n");
        assert_eq!(
            home.ok(&["sync", "tokio", "--version", "1"]).await,
            "tokio@1: ok, revision 2, 2 documents, 2 sections (2 changed, 1 deleted, 0 unchanged, 0 failed)\n"
        );
        assert_eq!(
            home.ok(&["sync"]).await,
            "tokio@1: ok, revision 2, 2 documents, 2 sections (0 changed, 0 deleted, 2 unchanged, 0 failed)\n"
        );
    }

    #[tokio::test]
    async fn the_selection_must_be_configured() {
        let home = Home::new().with_tokio().await;
        let error = home.fail(&["sync", "vue"]).await;
        assert!(
            matches!(error, AppError::Core(CoreError::UnknownLibrary { .. })),
            "{error}"
        );
        let error = home.fail(&["sync", "Bad Id"]).await;
        assert!(
            matches!(error, AppError::Core(CoreError::InvalidLibraryId { .. })),
            "{error}"
        );
        let error = home.fail(&["sync", "tokio", "--version", "9"]).await;
        assert!(matches!(error, AppError::Usage(_)), "{error}");
        assert_eq!(error.to_string(), "library tokio has no version 9 in the config");
        assert_eq!(error.exit_code(), 2);
    }

    #[tokio::test]
    async fn versions_that_cannot_be_listed_fail_without_stopping_the_others() {
        let home = Home::new().with_tokio().await;
        let missing = home.path("missing");
        home.ok(&["add", "gone", "--version", "1", "--path", missing.to_str().unwrap()])
            .await;
        home.ok(&["add", "remote", "--version", "1", "--github", "owner/repo"])
            .await;
        let mut out = Vec::new();
        let error = home.run_to(&["sync"], &mut out).await.unwrap_err();
        assert!(matches!(error, AppError::Incomplete { count: 2 }), "{error}");
        assert_eq!(error.exit_code(), 3);
        let out = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("tokio@1: ok"), "{out}");
        assert_eq!(lines[1], "gone@1: failed, revision 0, 0 documents, 0 sections");
        assert!(lines[2].starts_with("  last error: source directory "), "{out}");
        assert_eq!(
            lines[4],
            "  last error: github:owner/repo sources are not supported by this build"
        );
        home.write("docs/huge.md", &"x".repeat(20));
        let config = std::fs::read_to_string(home.path("zenodotus.toml")).unwrap();
        let config = config.replace("# page_max_bytes = 10_485_760", "page_max_bytes = 10");
        std::fs::write(home.path("zenodotus.toml"), config).unwrap();
        let error = home.fail(&["sync", "tokio"]).await;
        assert!(matches!(error, AppError::Incomplete { count: 1 }), "{error}");
    }

    #[tokio::test]
    async fn unconfigured_scopes_are_pruned_by_a_full_sync() {
        let home = Home::new().with_tokio().await;
        let docs = home.path("docs");
        home.ok(&["add", "other", "--version", "1", "--path", docs.to_str().unwrap()])
            .await;
        home.ok(&["sync"]).await;
        let config = std::fs::read_to_string(home.path("zenodotus.toml")).unwrap();
        let cut = config.find("[[library]]\nid = \"other\"").unwrap();
        std::fs::write(home.path("zenodotus.toml"), &config[..cut]).unwrap();
        assert!(home.ok(&["sync", "tokio"]).await.starts_with("tokio@1"));
        assert!(
            home.ok(&["sync"])
                .await
                .ends_with("other@1: removed, no longer configured\n")
        );
        let tokio_start = config.find("[[library]]\nid = \"tokio\"").unwrap();
        std::fs::write(home.path("zenodotus.toml"), &config[..tokio_start]).unwrap();
        assert_eq!(home.ok(&["sync"]).await, "tokio@1: removed, no longer configured\n");
        assert_eq!(
            home.ok(&["sync"]).await,
            "Nothing to sync: the config has no libraries.\n"
        );
        assert!(home.ok(&["status"]).await.starts_with("The index is empty"));
    }

    #[tokio::test]
    async fn a_rebuild_replaces_the_index_only_when_no_version_fails() {
        let home = Home::new().with_tokio().await;
        home.ok(&["sync"]).await;
        let config = std::fs::read_to_string(home.path("zenodotus.toml")).unwrap();
        let weighted = config.replace("# title_weight = 4", "title_weight = 5");
        std::fs::write(home.path("zenodotus.toml"), &weighted).unwrap();
        let error = home.fail(&["search", "tokio", "spawn"]).await;
        assert!(
            matches!(error, AppError::Search(SearchError::SchemaMismatch { .. })),
            "{error}"
        );
        assert_eq!(error.exit_code(), 2);
        assert!(
            home.ok(&["sync", "--rebuild"])
                .await
                .starts_with("tokio@1: ok, revision 1")
        );
        assert!(home.ok(&["search", "tokio", "spawn"]).await.contains("tokio::spawn"));

        let missing = home.path("missing");
        home.ok(&["add", "gone", "--version", "1", "--path", missing.to_str().unwrap()])
            .await;
        let error = home.fail(&["sync", "--rebuild"]).await;
        assert!(matches!(error, AppError::RebuildAbandoned { count: 1 }), "{error}");
        assert_eq!(error.exit_code(), 3);
        assert!(home.ok(&["search", "tokio", "spawn"]).await.contains("tokio::spawn"));
        assert!(!home.data_dir().join("index.new").exists());
    }

    #[tokio::test]
    async fn a_second_sync_waits_for_none() {
        let home = Home::new().with_tokio().await;
        let _held = SyncLock::acquire(&home.data_dir()).unwrap();
        let error = home.fail(&["sync"]).await;
        assert!(matches!(error, AppError::Busy { .. }), "{error}");
        assert_eq!(error.exit_code(), 4);
    }

    #[tokio::test]
    async fn search_checks_its_limits_and_the_index() {
        let home = Home::new().with_tokio().await;
        for (args, message) in [
            (&["--limit", "0"][..], "--limit must be between 1 and 20; got 0"),
            (
                &["--max-chars", "999"][..],
                "--max-chars must be between 1000 and 100000; got 999",
            ),
        ] {
            let mut argv = vec!["search", "tokio", "spawn"];
            argv.extend(args);
            let error = home.fail(&argv).await;
            assert_eq!(error.to_string(), message);
            assert_eq!(error.exit_code(), 2);
        }
        let error = home.fail(&["search", "tokio", "spawn"]).await;
        assert!(matches!(error, AppError::Search(SearchError::IndexEmpty)), "{error}");
        assert_eq!(error.exit_code(), 1);
        home.ok(&["sync"]).await;
        let error = home.fail(&["search", "vue", "spawn"]).await;
        assert!(
            matches!(error, AppError::Search(SearchError::UnknownLibrary { .. })),
            "{error}"
        );
    }

    #[tokio::test]
    async fn add_writes_every_kind_of_source() {
        let home = Home::new();
        let error = home.fail(&["add", "a", "--version", "1", "--github", "o/r"]).await;
        assert!(matches!(error, AppError::Core(CoreError::ConfigIo { .. })), "{error}");
        assert_eq!(error.exit_code(), 1);
        home.ok(&["init"]).await;
        let added = home
            .ok(&[
                "add",
                "a",
                "--version",
                "1",
                "--github",
                "o/r",
                "--ref",
                "v1",
                "--docs-path",
                "docs",
            ])
            .await;
        assert_eq!(
            added,
            "Added a@1 from github:o/r@v1; run `zenodotus sync a` to index it\n"
        );
        for (id, flags) in [
            ("b", &["--url", "https://b.dev/a.md", "https://b.dev/c.md"][..]),
            ("c", &["--llms-txt", "https://c.dev/llms.txt"][..]),
            ("d", &["--website", "https://d.dev/docs/", "--max-pages", "7"][..]),
            ("e", &["--website", "https://e.dev/"][..]),
        ] {
            let mut argv = vec!["add", id, "--version", "1"];
            argv.extend(flags);
            home.ok(&argv).await;
        }
        let config = Config::load(&home.path("zenodotus.toml")).unwrap();
        let sources: Vec<String> = config
            .libraries
            .iter()
            .map(|library| library.versions[0].source.describe())
            .collect();
        assert_eq!(
            sources,
            [
                "github:o/r@v1",
                "url:https://b.dev/a.md (+1 more)",
                "llms_txt:https://c.dev/llms.txt",
                "website:https://d.dev/docs/",
                "website:https://e.dev/",
            ]
        );
        assert!(matches!(
            &config.libraries[3].versions[0].source,
            SourceSpec::Website { max_pages: 7, .. }
        ));
        let error = home.fail(&["add", "f", "--version", "1", "--url", "not a url"]).await;
        assert!(error.to_string().starts_with("invalid URL \"not a url\": "), "{error}");
        let error = home.fail(&["add", "Bad Id", "--version", "1", "--path", "x"]).await;
        assert!(
            matches!(error, AppError::Core(CoreError::InvalidLibraryId { .. })),
            "{error}"
        );
    }

    #[tokio::test]
    async fn without_a_config_location_nothing_runs() {
        let home = Home::new();
        let env = |_: &str| None;
        let clock = FixedClock::new(Timestamp::from_unix(0));
        let mut out = Vec::new();
        let mut ctx = Context {
            env: &env,
            clock: &clock,
            out: &mut out,
        };
        let error = run(Cli::try_parse_from(["zenodotus", "list"]).unwrap(), &mut ctx)
            .await
            .unwrap_err();
        assert!(matches!(error, AppError::Core(CoreError::NoLocation { .. })), "{error}");
        drop(home);
    }

    /// A writer that always fails.
    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("closed"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn output_failures_are_reported() {
        let home = Home::new().with_tokio().await;
        let error = home.run_to(&["list"], &mut Broken).await.unwrap_err();
        assert!(matches!(error, AppError::Output(_)), "{error}");
        assert_eq!(error.to_string(), "cannot write the output: closed");
        assert_eq!(error.exit_code(), 1);
        assert!(Broken.flush().is_ok());
    }

    #[tokio::test]
    async fn every_command_reports_output_failures() {
        let home = Home::new();
        let failed = |result: Result<(), AppError>| matches!(result, Err(AppError::Output(_)));
        assert!(failed(home.run_to(&["init"], &mut Broken).await));
        let docs = home.path("docs");
        let add = ["add", "tokio", "--version", "1", "--path", docs.to_str().unwrap()];
        assert!(failed(home.run_to(&add, &mut Broken).await));
        assert!(failed(home.run_to(&["sync"], &mut Broken).await));
        for args in [
            &["search", "tokio", "spawn"][..],
            &["search", "tokio", "spawn", "--json"][..],
            &["status"][..],
            &["status", "--json"][..],
            &["list", "--json"][..],
        ] {
            assert!(failed(home.run_to(args, &mut Broken).await), "{args:?}");
        }
    }

    #[tokio::test]
    async fn list_and_status_report_an_index_that_does_not_match_the_config() {
        let home = Home::new().with_tokio().await;
        home.ok(&["sync"]).await;
        let config = std::fs::read_to_string(home.path("zenodotus.toml")).unwrap();
        std::fs::write(
            home.path("zenodotus.toml"),
            config.replace("# heading_weight = 2", "heading_weight = 3"),
        )
        .unwrap();
        for command in ["list", "status"] {
            let error = home.fail(&[command]).await;
            assert!(
                matches!(error, AppError::Search(SearchError::SchemaMismatch { .. })),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn serving_a_public_address_needs_a_token() {
        let home = Home::new().with_tokio().await;
        let error = home.fail(&["serve", "--http", "0.0.0.0:0"]).await;
        assert!(
            matches!(error, AppError::Serve(ServeError::InsecureBind { .. })),
            "{error}"
        );
    }

    #[test]
    fn directory_sizes_add_up_nested_files() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/one"), "12").unwrap();
        std::fs::write(dir.path().join("a/b/two"), "345").unwrap();
        assert_eq!(directory_size(dir.path()).unwrap(), 5);
        assert_eq!(directory_size(&dir.path().join("missing")).unwrap(), 0);
        assert_eq!(directory_size(Path::new("/proc/1/fd")).ok(), None);
    }

    #[test]
    fn exit_codes_follow_the_table() {
        let serve = |error: ServeError| AppError::Serve(error).exit_code();
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], 1));
        assert_eq!(serve(ServeError::InsecureBind { addr }), 2);
        assert_eq!(serve(ServeError::Io(std::io::Error::other("x"))), 1);
        assert_eq!(AppError::Search(SearchError::Locked).exit_code(), 4);
        assert_eq!(AppError::Search(SearchError::EmptyQuery).exit_code(), 1);
    }
}
