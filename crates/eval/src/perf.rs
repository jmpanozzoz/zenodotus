//! Serving performance on a large index: the query workload, query latency over stdio MCP and in process, the time
//! from starting `serve` to its first `tools/list` response, and the server's peak RSS.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;
use zenodotus_core::config::{Config, RankingSettings};
use zenodotus_core::ids::Scope;
use zenodotus_search::query::{QueryRequest, query_docs};
use zenodotus_search::store::Store;

use crate::error::EvalError;
use crate::metrics::percentile;
use crate::run::DEPTH;
use crate::systems::{Launcher, McpClient, object};

/// Calls made before timing starts.
pub const WARMUP: usize = 1_000;

/// Timed passes over the workload.
pub const ROUNDS: usize = 10;

/// The workload takes every page at this stride, in path order.
pub const STRIDE: usize = 70;

/// The size of the workload.
pub const WORKLOAD: usize = 200;

/// The response budget of every query, the server's default.
pub const MAX_CHARS: usize = 12_000;

/// The line of a GNU `time -v` report that holds the peak RSS.
const PEAK_RSS: &str = "Maximum resident set size (kbytes):";

/// The index a performance config points at, and the default version of the library to measure.
pub struct PerfIndex {
    /// The library id.
    pub library: String,
    /// Its default version.
    pub scope: Scope,
    /// The opened index.
    pub store: Store,
    /// The ranking settings of the config.
    pub ranking: RankingSettings,
}

impl PerfIndex {
    /// Opens the index of `config_path` and finds the default version of `library` in it.
    ///
    /// # Errors
    ///
    /// A config or search error, and [`EvalError::NoLibrary`] when the config does not list `library`.
    pub fn open(config_path: &Path, library: &str) -> Result<Self, EvalError> {
        let config = Config::load(config_path)?;
        let data_dir = config.settings.data_dir(config_path, &|_| None)?;
        let no_library = || EvalError::NoLibrary {
            config: config_path.to_owned(),
            library: library.to_owned(),
        };
        let found = config
            .libraries
            .iter()
            .find(|candidate| candidate.id.as_str() == library)
            .ok_or_else(no_library)?;
        let version = found
            .versions
            .iter()
            .find(|version| version.default)
            .or_else(|| found.versions.first())
            .ok_or_else(no_library)?;
        Ok(Self {
            library: library.to_owned(),
            scope: Scope::new(found.id.clone(), version.version.clone()),
            store: Store::open(&data_dir, &config.settings.index)?,
            ranking: config.settings.ranking,
        })
    }

    /// The workload: every [`STRIDE`]th page in path order, up to [`WORKLOAD`] pages, each as its title followed by
    /// the first identifier of the page when the title does not already hold it.
    ///
    /// # Errors
    ///
    /// A search error when the sections cannot be read.
    pub fn workload(&self) -> Result<Vec<String>, EvalError> {
        let mut pages: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
        let mut sections = self.store.sections(&self.scope)?;
        sections.sort_by_key(|section| section.ordinal);
        for section in sections {
            let page = pages
                .entry(section.doc_key.doc_path().to_owned())
                .or_insert_with(|| (section.title.clone(), None));
            page.1 = page.1.take().or_else(|| section.symbols.first().cloned());
        }
        Ok(pages
            .into_values()
            .step_by(STRIDE)
            .take(WORKLOAD)
            .map(|(title, symbol)| match symbol {
                Some(symbol) if !title.contains(&symbol) => format!("{title} {symbol}"),
                _ => title,
            })
            .collect())
    }

    /// The latency of every `query_docs` call of `rounds` passes over `queries`, in process, after one untimed pass.
    ///
    /// # Errors
    ///
    /// A search error when a query fails.
    pub fn in_process(&self, queries: &[String], rounds: usize) -> Result<Vec<Duration>, EvalError> {
        let mut latencies = Vec::with_capacity(queries.len() * rounds);
        for round in 0..=rounds {
            for query in queries {
                let request = QueryRequest {
                    library_id: &self.library,
                    query,
                    version: None,
                    language: None,
                    max_results: DEPTH,
                    max_chars: MAX_CHARS,
                };
                let start = Instant::now();
                query_docs(&self.store, &request, &self.ranking)?;
                if round > 0 {
                    latencies.push(start.elapsed());
                }
            }
        }
        Ok(latencies)
    }
}

/// Reads the workload file: one query per non-empty line.
///
/// # Errors
///
/// [`EvalError::Io`] when the file cannot be read, and [`EvalError::Parse`] when it holds no query.
pub fn load_queries(path: &Path) -> Result<Vec<String>, EvalError> {
    let text = std::fs::read_to_string(path).map_err(EvalError::io(path))?;
    let queries: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    if queries.is_empty() {
        return Err(EvalError::Parse {
            path: path.to_owned(),
            line: 0,
            message: "holds no query".to_owned(),
        });
    }
    Ok(queries)
}

/// Starts `zenodotus --config <config> serve` with `launcher`.
///
/// # Errors
///
/// [`EvalError::Server`] when it cannot start.
pub async fn serve(launcher: &Launcher, config: &Path) -> Result<McpClient, EvalError> {
    let command = launcher.command(&["--config".as_ref(), config.as_os_str(), "serve".as_ref()]);
    McpClient::spawn(command).await.map_err(EvalError::Server)
}

/// The time from starting the server to its first `tools/list` response, which must list `query_docs`.
///
/// # Errors
///
/// [`EvalError::Server`] when the server cannot start, the listing fails, or `query_docs` is missing.
pub async fn cold_start(launcher: &Launcher, config: &Path) -> Result<Duration, EvalError> {
    let start = Instant::now();
    let client = serve(launcher, config).await?;
    let tools = client.list_tools().await.map_err(EvalError::Server)?;
    let elapsed = start.elapsed();
    client.close().await;
    if !tools.iter().any(|tool| tool == "query_docs") {
        return Err(EvalError::Server(format!("tools/list has no query_docs: {tools:?}")));
    }
    Ok(elapsed)
}

/// The latency of every `query_docs` call of `rounds` passes over `queries` over `client`, after `warmup` untimed
/// calls that cycle through them.
///
/// # Errors
///
/// [`EvalError::Server`] when a call fails.
pub async fn over_stdio(
    client: &McpClient,
    library: &str,
    queries: &[String],
    warmup: usize,
    rounds: usize,
) -> Result<Vec<Duration>, EvalError> {
    let call = |query: &String| {
        let arguments = object(&[
            ("library_id", Some(json!(library))),
            ("query", Some(json!(query))),
            ("max_results", Some(json!(DEPTH))),
        ]);
        client.call("query_docs", arguments)
    };
    for query in queries.iter().cycle().take(warmup) {
        call(query).await.map_err(EvalError::Server)?;
    }
    let mut latencies = Vec::with_capacity(queries.len() * rounds);
    for _ in 0..rounds {
        for query in queries {
            latencies.push(call(query).await.map_err(EvalError::Server)?.latency);
        }
    }
    Ok(latencies)
}

/// The peak RSS in KiB from a GNU `time -v` report.
#[must_use]
pub fn peak_rss_kib(report: &str) -> Option<u64> {
    report
        .lines()
        .find_map(|line| line.trim().strip_prefix(PEAK_RSS))
        .and_then(|value| value.trim().parse().ok())
}

/// Latency percentiles in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Latency {
    /// Timed calls.
    pub calls: usize,
    /// The median.
    pub p50_ms: f64,
    /// The 95th percentile.
    pub p95_ms: f64,
    /// The slowest call.
    pub max_ms: f64,
}

impl Latency {
    /// The percentiles of `latencies`, or `None` when there are none.
    #[must_use]
    pub fn of(latencies: &[Duration]) -> Option<Self> {
        let millis = |p: f64| percentile(latencies, p).map(|latency| latency.as_secs_f64() * 1000.0);
        Some(Self {
            calls: latencies.len(),
            p50_ms: millis(50.0)?,
            p95_ms: millis(95.0)?,
            max_ms: millis(100.0)?,
        })
    }
}

/// What `perf` measured.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PerfReport {
    /// The library measured.
    pub library: String,
    /// Its sections in the index.
    pub sections: usize,
    /// Queries in the workload.
    pub queries: usize,
    /// Latency over stdio MCP, as an agent sees it.
    pub stdio: Option<Latency>,
    /// Latency of `query_docs` in process.
    pub in_process: Option<Latency>,
    /// The server's peak RSS in KiB, when it ran under `time -v`.
    pub serve_peak_rss_kib: Option<u64>,
}

/// What `perf` needs.
#[derive(Debug, Clone)]
pub struct PerfPlan {
    /// The performance config.
    pub config: PathBuf,
    /// The library to measure.
    pub library: String,
    /// The workload file.
    pub queries: PathBuf,
    /// Runs the zenodotus binary.
    pub zenodotus: PathBuf,
    /// GNU `time`, to measure the server's peak RSS.
    pub time: Option<PathBuf>,
    /// Where `perf.json` and the `time` report go.
    pub out: PathBuf,
    /// Untimed calls before timing.
    pub warmup: usize,
    /// Timed passes over the workload.
    pub rounds: usize,
}

/// Measures latency over stdio and in process, and the server's peak RSS, then writes `perf.json` into the out
/// directory.
///
/// # Errors
///
/// Every error of opening the index, reading the workload, serving and querying, and [`EvalError::Io`] when the
/// results cannot be written.
pub async fn measure(plan: &PerfPlan) -> Result<PerfReport, EvalError> {
    let index = PerfIndex::open(&plan.config, &plan.library)?;
    let queries = load_queries(&plan.queries)?;
    std::fs::create_dir_all(&plan.out).map_err(EvalError::io(&plan.out))?;
    let time_report = plan.out.join("serve-time.txt");
    let launcher = match &plan.time {
        Some(time) => {
            let mut launcher = Launcher::new(time);
            launcher.prefix = vec![
                "-v".to_owned(),
                "-o".to_owned(),
                time_report.to_string_lossy().into_owned(),
                plan.zenodotus.to_string_lossy().into_owned(),
            ];
            launcher
        }
        None => Launcher::new(&plan.zenodotus),
    };
    let client = serve(&launcher, &plan.config).await?;
    let stdio = over_stdio(&client, &plan.library, &queries, plan.warmup, plan.rounds).await;
    client.close().await;
    let stdio = stdio?;
    let serve_peak_rss_kib = match plan.time {
        Some(_) => peak_rss_kib(&std::fs::read_to_string(&time_report).map_err(EvalError::io(&time_report))?),
        None => None,
    };
    let report = PerfReport {
        library: plan.library.clone(),
        sections: index.store.sections(&index.scope)?.len(),
        queries: queries.len(),
        stdio: Latency::of(&stdio),
        in_process: Latency::of(&index.in_process(&queries, plan.rounds)?),
        serve_peak_rss_kib,
    };
    let path = plan.out.join("perf.json");
    let json = serde_json::to_string_pretty(&report).expect("reports serialize");
    std::fs::write(&path, json).map_err(EvalError::io(&path))?;
    Ok(report)
}

/// The report as text.
#[must_use]
pub fn render(report: &PerfReport) -> String {
    let mut out = format!(
        "{}: {} sections, {} queries\n\n| Path | Calls | p50 ms | p95 ms | max ms |\n|---|---|---|---|---|\n",
        report.library, report.sections, report.queries
    );
    for (path, latency) in [("stdio MCP", report.stdio), ("in process", report.in_process)] {
        match latency {
            Some(latency) => {
                let _ = writeln!(
                    out,
                    "| {path} | {} | {:.2} | {:.2} | {:.2} |",
                    latency.calls, latency.p50_ms, latency.p95_ms, latency.max_ms
                );
            }
            None => {
                let _ = writeln!(out, "| {path} | 0 | - | - | - |");
            }
        }
    }
    if let Some(kib) = report.serve_peak_rss_kib {
        let _ = writeln!(
            out,
            "\nPeak RSS while serving: {kib} KiB ({:.1} MiB)",
            count(kib) / 1024.0
        );
    }
    out
}

/// `value` as a float, for display.
fn count(value: u64) -> f64 {
    value.to_string().parse().unwrap_or(f64::MAX)
}

#[cfg(test)]
pub(crate) mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::corpus::tests::corpus;
    use crate::systems::tests::{calls, stub, stub_program};
    use crate::validate::tests::index;

    /// A corpus whose tokio index holds 150 pages of one section each: page 0 titled `Spawning` that mentions
    /// `task::spawn`, pages 27 and 90 with the symbol `pageN::run`, and plain pages.
    pub(crate) fn indexed() -> (TempDir, PathBuf) {
        let (root, corpus) = corpus();
        let texts: Vec<String> = (0..150)
            .map(|n| match n {
                0 => "# Spawn with task::spawn\nuse task::spawn".to_owned(),
                27 | 90 => format!("page {n} calls page{n}::run then other::thing"),
                _ => format!("page {n}"),
            })
            .collect();
        index(&corpus.data_dir, &corpus.index, &corpus.scopes[0].scope, &texts);
        (root, corpus.config_path)
    }

    #[test]
    fn the_workload_takes_every_seventieth_page_with_its_first_symbol() {
        let (_root, config) = indexed();
        let index = PerfIndex::open(&config, "tokio").unwrap();
        // In path order, doc0, doc27 and doc90 are pages 0, 70 and 140.
        assert_eq!(
            index.workload().unwrap(),
            ["Spawn with task::spawn", "doc27.md page27::run", "doc90.md page90::run"]
        );
        let pydantic = PerfIndex::open(&config, "pydantic").unwrap();
        assert_eq!(pydantic.scope.to_string(), "pydantic@2.12");
        assert_eq!(pydantic.workload().unwrap(), [] as [String; 0]);
        let error = PerfIndex::open(&config, "react").err().unwrap();
        assert!(
            matches!(&error, EvalError::NoLibrary { library, .. } if library == "react"),
            "{error}"
        );
        let missing = config.with_file_name("missing.toml");
        assert!(matches!(PerfIndex::open(&missing, "tokio"), Err(EvalError::Config(_))));
    }

    #[test]
    fn the_workload_file_needs_one_query() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("queries.txt");
        std::fs::write(&path, "  fetch json \n\nArray map\n").unwrap();
        assert_eq!(load_queries(&path).unwrap(), ["fetch json", "Array map"]);
        std::fs::write(&path, "\n  \n").unwrap();
        let error = load_queries(&path).unwrap_err();
        assert!(error.to_string().ends_with(":0: holds no query"), "{error}");
        let error = load_queries(&dir.path().join("missing")).unwrap_err();
        assert!(matches!(error, EvalError::Io { .. }), "{error}");
    }

    #[test]
    fn peak_rss_comes_from_a_time_report() {
        let report = "\tCommand being timed: \"zenodotus serve\"\n\tMaximum resident set size (kbytes): 40960\n\tExit status: 0\n";
        assert_eq!(peak_rss_kib(report), Some(40_960));
        assert_eq!(peak_rss_kib("\tMaximum resident set size (kbytes): many\n"), None);
        assert_eq!(peak_rss_kib(""), None);
    }

    #[test]
    fn latency_percentiles_are_in_milliseconds() {
        let latencies: Vec<Duration> = (1..=20).map(Duration::from_millis).collect();
        let latency = Latency::of(&latencies).unwrap();
        assert_eq!(
            latency,
            Latency {
                calls: 20,
                p50_ms: 10.0,
                p95_ms: 19.0,
                max_ms: 20.0
            }
        );
        assert_eq!(Latency::of(&[]), None);
    }

    #[test]
    fn in_process_latency_times_every_round_after_a_warm_pass() {
        let (_root, config) = indexed();
        let index = PerfIndex::open(&config, "tokio").unwrap();
        let queries = ["page".to_owned(), "spawn".to_owned()];
        assert_eq!(index.in_process(&queries, 3).unwrap().len(), 6);
        let error = index.in_process(&["   ".to_owned()], 1).unwrap_err();
        assert!(matches!(error, EvalError::Search(_)), "{error}");
    }

    #[tokio::test]
    async fn cold_start_ends_at_a_tools_list_with_query_docs() {
        let (_root, config) = indexed();
        let (_dir, mut launcher) = stub();
        assert!(cold_start(&launcher, &config).await.unwrap() > Duration::ZERO);
        launcher
            .env
            .push(("STUB_TOOLS".to_owned(), "resolve_library".to_owned()));
        let error = cold_start(&launcher, &config).await.unwrap_err();
        assert!(error.to_string().contains("tools/list has no query_docs"), "{error}");
        launcher.env.push(("STUB_TOOLS".to_owned(), "error".to_owned()));
        let error = cold_start(&launcher, &config).await.unwrap_err();
        assert!(error.to_string().contains("tools/list: "), "{error}");
        let error = cold_start(&Launcher::new("/bin/true"), &config).await.unwrap_err();
        assert!(matches!(error, EvalError::Server(_)), "{error}");
    }

    #[tokio::test]
    async fn stdio_latency_times_every_round_after_the_warmup() {
        let (_root, config) = indexed();
        let (dir, launcher) = stub();
        let client = serve(&launcher, &config).await.unwrap();
        let queries = ["one".to_owned(), "two".to_owned()];
        let latencies = over_stdio(&client, "tokio", &queries, 3, 2).await.unwrap();
        assert_eq!(latencies.len(), 4);
        let logged = calls(&dir);
        assert_eq!(logged.len(), 8);
        assert_eq!(
            logged[3]["arguments"],
            json!({"library_id": "tokio", "query": "one", "max_results": 10})
        );
        let failing = ["fail".to_owned()];
        let error = over_stdio(&client, "tokio", &failing, 1, 1).await.unwrap_err();
        assert!(error.to_string().contains("stub failure"), "{error}");
        let error = over_stdio(&client, "tokio", &failing, 0, 1).await.unwrap_err();
        assert!(matches!(error, EvalError::Server(_)), "{error}");
        client.close().await;
    }

    fn plan(root: &Path, config: PathBuf, time: Option<PathBuf>) -> PerfPlan {
        let queries = root.join("queries.txt");
        std::fs::write(&queries, "page\nspawn\n").unwrap();
        PerfPlan {
            config,
            library: "tokio".to_owned(),
            queries,
            zenodotus: stub_program(),
            time,
            out: root.join("perf"),
            warmup: 2,
            rounds: 2,
        }
    }

    #[tokio::test]
    async fn measuring_writes_latency_and_the_peak_rss() {
        let (root, config) = indexed();
        let mut plan = plan(root.path(), config, Some(PathBuf::from("/usr/bin/time")));
        let report = measure(&plan).await.unwrap();
        assert_eq!(
            (report.library.as_str(), report.sections, report.queries),
            ("tokio", 150, 2)
        );
        assert_eq!(report.stdio.unwrap().calls, 4);
        assert_eq!(report.in_process.unwrap().calls, 4);
        assert!(report.serve_peak_rss_kib.unwrap() > 0);
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(plan.out.join("perf.json")).unwrap()).unwrap();
        assert_eq!(written["sections"], 150);
        let text = render(&report);
        assert!(text.starts_with("tokio: 150 sections, 2 queries\n\n| Path |"), "{text}");
        assert!(text.contains("| stdio MCP | 4 | "), "{text}");
        assert!(text.contains("\nPeak RSS while serving: "), "{text}");

        plan.time = None;
        let report = measure(&plan).await.unwrap();
        assert_eq!(report.serve_peak_rss_kib, None);
        assert!(!render(&report).contains("Peak RSS"));
        let empty = PerfReport {
            stdio: None,
            in_process: None,
            ..report
        };
        assert!(render(&empty).contains("| stdio MCP | 0 | - | - | - |\n| in process | 0 | - | - | - |\n"));
        assert_eq!(count(3), 3.0);
    }

    #[tokio::test]
    async fn measuring_reports_what_failed() {
        let (root, config) = indexed();
        let mut plan = plan(root.path(), config.clone(), None);
        plan.library = "react".to_owned();
        assert!(matches!(measure(&plan).await, Err(EvalError::NoLibrary { .. })));
        plan.library = "tokio".to_owned();
        std::fs::write(&plan.queries, "").unwrap();
        assert!(matches!(measure(&plan).await, Err(EvalError::Parse { .. })));
        std::fs::write(&plan.queries, "fail\n").unwrap();
        assert!(matches!(measure(&plan).await, Err(EvalError::Server(_))));
        std::fs::write(&plan.queries, "x".repeat(2_000)).unwrap();
        assert!(matches!(measure(&plan).await, Err(EvalError::Search(_))));
        std::fs::write(&plan.queries, "page\n").unwrap();
        plan.zenodotus = PathBuf::from("/bin/true");
        assert!(matches!(measure(&plan).await, Err(EvalError::Server(_))));
        plan.zenodotus = stub_program();
        // The stub stands in for `time` too: it serves, but writes no report.
        plan.time = Some(stub_program());
        let error = measure(&plan).await.unwrap_err();
        assert!(error.to_string().contains("serve-time.txt"), "{error}");
        plan.time = None;
        std::fs::create_dir_all(plan.out.join("perf.json")).unwrap();
        let error = measure(&plan).await.unwrap_err();
        assert!(error.to_string().contains("perf.json"), "{error}");
        std::fs::remove_dir_all(&plan.out).unwrap();
        std::fs::write(&plan.out, "").unwrap();
        assert!(matches!(measure(&plan).await, Err(EvalError::Io { .. })));
    }
}
