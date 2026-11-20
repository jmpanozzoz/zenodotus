//! The command line: check the question sets, run systems on a set, or compare every system on the test set.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use zenodotus_core::time::Clock;

use crate::corpus::{Corpus, ScopeCase};
use crate::error::EvalError;
use crate::perf::{self, PerfIndex, PerfPlan};
use crate::questions::{self, Question, Set};
use crate::report::{SystemResult, compose, render, write};
use crate::run::{Record, evaluate};
use crate::systems::context7::{self, Context7};
use crate::systems::grounded::{self, Grounded};
use crate::systems::neuledge::{self, Neuledge};
use crate::systems::zenodotus::Zenodotus;
use crate::systems::{Launcher, RetrievalSystem, SystemUnavailable};
use crate::validate::validate;

/// Measures the retrieval quality and cost of zenodotus and its competitors on the pinned corpus.
#[derive(Debug, Parser)]
#[command(name = "zenodotus-eval", version)]
pub struct Cli {
    /// The repository root, which holds `eval/`.
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
    /// Where the corpus sources are checked out, one directory per version.
    #[arg(long, default_value = "target/eval-sources")]
    pub sources: PathBuf,
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// A subcommand.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Checks both question sets against the index synced from the corpus.
    Validate,
    /// Runs systems on a question set: zenodotus on the dev set unless told otherwise.
    Run(RunArgs),
    /// Runs systems on a question set: every system on the test set unless told otherwise.
    Compare(RunArgs),
    /// Measures query latency and the server's peak RSS on a performance index.
    Perf(PerfArgs),
    /// Prints the performance workload, generated from a performance index.
    PerfQueries(IndexArgs),
    /// Starts `serve` and waits for its first `tools/list` response, for timing with hyperfine.
    ColdStart(ServeArgs),
}

/// The set and systems `run` uses unless told otherwise.
pub const RUN_DEFAULTS: (Set, &[SystemName]) = (Set::Dev, &[SystemName::Zenodotus]);

/// The set and systems `compare` uses unless told otherwise.
pub const COMPARE_DEFAULTS: (Set, &[SystemName]) = (Set::Test, &SystemName::ALL);

/// A performance index.
#[derive(Debug, Args)]
pub struct IndexArgs {
    /// The performance config.
    #[arg(long, default_value = "target/perf/zenodotus.toml")]
    pub config: PathBuf,
    /// The library to measure.
    #[arg(long, default_value = "mdn")]
    pub library: String,
}

/// A server to start.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// The config it serves.
    #[arg(long, default_value = "target/perf/zenodotus.toml")]
    pub config: PathBuf,
    /// The zenodotus binary.
    #[arg(long, default_value = "target/release/zenodotus")]
    pub zenodotus: PathBuf,
}

/// The options of `perf`.
#[derive(Debug, Args)]
pub struct PerfArgs {
    /// The index and library.
    #[command(flatten)]
    pub index: IndexArgs,
    /// The workload: one query per line.
    #[arg(long, default_value = "eval/perf_queries.txt")]
    pub queries: PathBuf,
    /// The zenodotus binary.
    #[arg(long, default_value = "target/release/zenodotus")]
    pub zenodotus: PathBuf,
    /// GNU `time`, run around the server to measure its peak RSS.
    #[arg(long)]
    pub time: Option<PathBuf>,
    /// Where `perf.json` and the `time` report go.
    #[arg(long, default_value = "target/perf")]
    pub out: PathBuf,
    /// Untimed calls before timing.
    #[arg(long, default_value_t = perf::WARMUP)]
    pub warmup: usize,
    /// Timed passes over the workload.
    #[arg(long, default_value_t = perf::ROUNDS)]
    pub rounds: usize,
}

impl PerfArgs {
    /// The measurement these options describe.
    #[must_use]
    pub fn plan(self) -> PerfPlan {
        PerfPlan {
            config: self.index.config,
            library: self.index.library,
            queries: self.queries,
            zenodotus: self.zenodotus,
            time: self.time,
            out: self.out,
            warmup: self.warmup,
            rounds: self.rounds,
        }
    }
}

/// A system the harness can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum SystemName {
    /// zenodotus itself.
    Zenodotus,
    /// The hosted Context7 service.
    Context7,
    /// Grounded Docs, full-text only.
    GroundedDocs,
    /// Neuledge Context.
    NeuledgeContext,
}

impl SystemName {
    /// Every system, in the order they run.
    pub const ALL: [Self; 4] = [
        Self::Zenodotus,
        Self::Context7,
        Self::GroundedDocs,
        Self::NeuledgeContext,
    ];

    /// The name reports use.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Zenodotus => crate::report::ZENODOTUS,
            Self::Context7 => crate::report::CONTEXT7,
            Self::GroundedDocs => "grounded-docs",
            Self::NeuledgeContext => "neuledge-context",
        }
    }
}

/// The options of `run` and `compare`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// The question set.
    #[arg(long, value_enum)]
    pub set: Option<Set>,
    /// A system to run; repeat the option or separate names with commas.
    #[arg(long = "system", value_enum, value_delimiter = ',')]
    pub systems: Vec<SystemName>,
    /// The zenodotus binary.
    #[arg(long, default_value = "target/release/zenodotus")]
    pub zenodotus: PathBuf,
    /// The Context7 MCP endpoint.
    #[arg(long, default_value = context7::DEFAULT_URL)]
    pub context7_url: String,
    /// A Context7 API key, sent as a bearer token; Context7 also answers without one, under a lower rate limit.
    #[arg(long, env = "CONTEXT7_API_KEY", hide_env_values = true)]
    pub context7_api_key: Option<String>,
    /// The npx that runs Grounded Docs and Neuledge Context.
    #[arg(long, default_value = "npx")]
    pub npx: PathBuf,
    /// The working directory of the competitors' stores.
    #[arg(long, default_value = "target/eval-work")]
    pub work: PathBuf,
    /// Where each run writes its results, in a directory named after its start time and set.
    #[arg(long, default_value = "eval/results")]
    pub out: PathBuf,
}

/// How to reach every system.
#[derive(Debug, Clone)]
pub struct Tools {
    /// Runs the zenodotus binary.
    pub zenodotus: Launcher,
    /// Runs Grounded Docs.
    pub grounded: Launcher,
    /// Runs Neuledge Context.
    pub neuledge: Launcher,
    /// The Context7 endpoint.
    pub context7_url: String,
    /// The Context7 API key.
    pub context7_token: Option<String>,
    /// The competitors' working directory.
    pub work: PathBuf,
}

/// A resolved `run` or `compare`.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The question set.
    pub set: Set,
    /// The systems, each once, in [`SystemName::ALL`] order.
    pub systems: Vec<SystemName>,
    /// How to reach them.
    pub tools: Tools,
    /// The results directory.
    pub out: PathBuf,
}

impl Plan {
    /// Resolves `args`, filling in the set and systems of `defaults` when they are not given.
    #[must_use]
    pub fn new(args: RunArgs, defaults: (Set, &[SystemName])) -> Self {
        let (set, systems) = defaults;
        let mut chosen = if args.systems.is_empty() {
            systems.to_vec()
        } else {
            args.systems
        };
        chosen.sort_unstable();
        chosen.dedup();
        Self {
            set: args.set.unwrap_or(set),
            systems: chosen,
            tools: Tools {
                zenodotus: Launcher::new(args.zenodotus),
                grounded: Launcher::npx(args.npx.clone(), grounded::PACKAGE),
                neuledge: Launcher::npx(args.npx.clone(), neuledge::PACKAGE),
                context7_url: args.context7_url,
                context7_token: args.context7_api_key,
                work: args.work,
            },
            out: args.out,
        }
    }
}

/// Runs `cli`, returning the text to print and how it ended.
pub async fn run(cli: Cli, clock: &dyn Clock) -> (String, Result<(), EvalError>) {
    let result = match cli.command {
        Command::Validate => {
            let mut text = String::new();
            let result = check(&cli.root, &cli.sources, &mut text);
            return (text, result);
        }
        Command::Run(args) => execute(&cli.root, &cli.sources, &Plan::new(args, RUN_DEFAULTS), clock).await,
        Command::Compare(args) => execute(&cli.root, &cli.sources, &Plan::new(args, COMPARE_DEFAULTS), clock).await,
        Command::Perf(args) => perf::measure(&args.plan()).await.map(|report| perf::render(&report)),
        Command::PerfQueries(args) => PerfIndex::open(&args.config, &args.library)
            .and_then(|index| index.workload())
            .map(|queries| queries.iter().flat_map(|query| [query.as_str(), "\n"]).collect()),
        Command::ColdStart(args) => perf::cold_start(&Launcher::new(args.zenodotus), &args.config)
            .await
            .map(|elapsed| {
                format!(
                    "tools/list answered {:.1} ms after start\n",
                    elapsed.as_secs_f64() * 1000.0
                )
            }),
    };
    match result {
        Ok(text) => (text, Ok(())),
        Err(error) => (String::new(), Err(error)),
    }
}

/// Validates both question sets, writing one line per set and one per rejected question into `out`.
///
/// # Errors
///
/// [`EvalError::Invalid`] when any question is rejected, and every error of loading and validating.
pub fn check(root: &Path, sources: &Path, out: &mut String) -> Result<(), EvalError> {
    let corpus = Corpus::load(root, sources)?;
    let mut rejected = 0;
    for set in Set::ALL {
        let questions = questions::load(root, set)?;
        let rejections = validate(&corpus, &questions)?;
        let _ = writeln!(
            out,
            "{set}: {} questions, {} rejected",
            questions.len(),
            rejections.len()
        );
        for rejection in &rejections {
            let _ = writeln!(out, "  {}: {}", rejection.id, rejection.reason);
        }
        rejected += rejections.len();
    }
    if rejected == 0 {
        Ok(())
    } else {
        Err(EvalError::Invalid(rejected))
    }
}

/// Runs `plan` and writes its results under a directory named after the time of `clock` and the set; returns the
/// report text.
///
/// # Errors
///
/// Every error of loading the corpus and the questions, and [`EvalError::Io`] when the results cannot be written. A
/// system that cannot start is reported, not an error.
pub async fn execute(root: &Path, sources: &Path, plan: &Plan, clock: &dyn Clock) -> Result<String, EvalError> {
    let corpus = Corpus::load(root, sources)?;
    let questions = questions::load(root, plan.set)?;
    let cases = questions
        .iter()
        .map(|question| Ok((question, corpus.scope_of(question)?)))
        .collect::<Result<Vec<_>, EvalError>>()?;
    let mut results = Vec::with_capacity(plan.systems.len());
    for system in &plan.systems {
        results.push(measure(*system, &corpus, &cases, &plan.tools).await);
    }
    let report = compose(plan.set, &questions, &results);
    let started = clock.now().to_string().replace(':', "-");
    let dir = plan.out.join(format!("{started}-{}", plan.set));
    let text = render(&report, &dir);
    write(&dir, &results, &report, &text)?;
    Ok(text)
}

/// Starts `system` and asks it every question of `cases`.
async fn measure(
    system: SystemName,
    corpus: &Corpus,
    cases: &[(&Question, &ScopeCase)],
    tools: &Tools,
) -> SystemResult {
    let outcome = match system {
        SystemName::Zenodotus => records(Zenodotus::start(&tools.zenodotus, corpus).await, cases).await,
        SystemName::Context7 => {
            let started = Context7::start(&tools.context7_url, tools.context7_token.as_deref()).await;
            records(started, cases).await
        }
        SystemName::GroundedDocs => records(Grounded::start(&tools.grounded, corpus, &tools.work).await, cases).await,
        SystemName::NeuledgeContext => {
            records(Neuledge::start(&tools.neuledge, corpus, &tools.work).await, cases).await
        }
    };
    SystemResult {
        name: system.name(),
        outcome,
    }
}

/// The records of a started system, or why it did not start.
async fn records<S: RetrievalSystem>(
    started: Result<S, SystemUnavailable>,
    cases: &[(&Question, &ScopeCase)],
) -> Result<Vec<Record>, String> {
    let mut system = started.map_err(|unavailable| unavailable.0)?;
    Ok(evaluate(&mut system, cases).await)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;
    use zenodotus_core::time::{FixedClock, Timestamp};

    use super::*;
    use crate::corpus::tests::corpus;
    use crate::questions::tests::id_in;
    use crate::systems::context7::tests::fake;
    use crate::systems::tests::{calls, stub, stub_program};
    use crate::validate::tests::index;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("zenodotus-eval").chain(args.iter().copied())).unwrap()
    }

    /// The options of `run` and `compare` alone.
    #[derive(Debug, Parser)]
    struct RunOnly {
        #[command(flatten)]
        args: RunArgs,
    }

    /// The options of `perf` alone.
    #[derive(Debug, Parser)]
    struct PerfOnly {
        #[command(flatten)]
        args: PerfArgs,
    }

    fn plan_of(args: &[&str], defaults: (Set, &[SystemName])) -> Plan {
        let parsed = RunOnly::try_parse_from(std::iter::once("run").chain(args[1..].iter().copied())).unwrap();
        Plan::new(parsed.args, defaults)
    }

    #[test]
    fn run_and_compare_fill_in_their_own_defaults() {
        let plan = plan_of(&["run"], RUN_DEFAULTS);
        assert_eq!(
            (plan.set, plan.systems.as_slice()),
            (Set::Dev, &[SystemName::Zenodotus][..])
        );
        assert_eq!(plan.tools.zenodotus.program, Path::new("target/release/zenodotus"));
        assert_eq!(plan.tools.grounded.program, Path::new("npx"));
        assert_eq!(plan.tools.grounded.prefix, ["-y", grounded::PACKAGE]);
        assert_eq!(plan.tools.neuledge.prefix, ["-y", neuledge::PACKAGE]);
        assert_eq!(plan.tools.context7_url, context7::DEFAULT_URL);
        assert_eq!(plan.tools.work, Path::new("target/eval-work"));
        assert_eq!(plan.out, Path::new("eval/results"));
        let plan = plan_of(&["compare"], COMPARE_DEFAULTS);
        assert_eq!((plan.set, plan.systems.as_slice()), (Set::Test, &SystemName::ALL[..]));
        let plan = plan_of(
            &[
                "compare",
                "--set",
                "dev",
                "--system",
                "neuledge-context,zenodotus",
                "--system",
                "grounded-docs",
                "--system",
                "zenodotus",
                "--context7-api-key",
                "key",
                "--npx",
                "/opt/npx",
            ],
            COMPARE_DEFAULTS,
        );
        assert_eq!(plan.set, Set::Dev);
        assert_eq!(
            plan.systems,
            [
                SystemName::Zenodotus,
                SystemName::GroundedDocs,
                SystemName::NeuledgeContext
            ]
        );
        assert_eq!(plan.tools.context7_token.as_deref(), Some("key"));
        assert_eq!(plan.tools.neuledge.program, Path::new("/opt/npx"));
        let names: Vec<&str> = SystemName::ALL.iter().map(|system| system.name()).collect();
        assert_eq!(names, ["zenodotus", "context7", "grounded-docs", "neuledge-context"]);
        assert!(Cli::try_parse_from(["zenodotus-eval", "run", "--system", "bing"]).is_err());
        assert!(matches!(parse(&["validate"]).command, Command::Validate));
    }

    /// One question line on `tokio` that needs `needle`.
    fn line(id: &str, library: &str, needle: &str) -> String {
        json!({
            "id": id, "library": library, "query": format!("query of {id}"), "kind": "api", "tags": ["symbol"],
            "must_contain": [needle], "target": "doc0.md"
        })
        .to_string()
            + "\n"
    }

    /// A corpus root with a tokio index of 40 sections, one of which has `needle`, and the given question sets.
    fn root(dev: &str, test: &str) -> (TempDir, Corpus) {
        let (root, corpus) = corpus();
        let mut texts: Vec<String> = (0..40).map(|n| format!("filler text {n}")).collect();
        texts[3] = "has the needle".to_owned();
        index(&corpus.data_dir, &corpus.index, &corpus.scopes[0].scope, &texts);
        let dir = root.path().join("eval/questions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("dev.jsonl"), dev).unwrap();
        std::fs::write(dir.join("test.jsonl"), test).unwrap();
        (root, corpus)
    }

    #[test]
    fn validation_reports_every_rejected_question() {
        let good = line(&id_in(Set::Dev, "good"), "tokio", "needle");
        let bad = line(&id_in(Set::Dev, "bad"), "tokio", "absent");
        let filler = line(&id_in(Set::Test, "many"), "tokio", "filler");
        let (root, _corpus) = root(&(good.clone() + &bad), &filler);
        let sources = root.path().join("sources");
        let mut text = String::new();
        let error = check(root.path(), &sources, &mut text).unwrap_err();
        assert!(matches!(error, EvalError::Invalid(2)), "{error}");
        let bad_id = id_in(Set::Dev, "bad");
        let many_id = id_in(Set::Test, "many");
        assert_eq!(
            text,
            format!(
                "dev: 2 questions, 1 rejected\n  {bad_id}: matches none of the 40 sections of tokio\n\
                 test: 1 questions, 1 rejected\n  {many_id}: matches 39 of the 40 sections of tokio, more than 5%\n"
            )
        );
        std::fs::write(root.path().join("eval/questions/dev.jsonl"), &good).unwrap();
        std::fs::write(root.path().join("eval/questions/test.jsonl"), "").unwrap();
        let mut text = String::new();
        check(root.path(), &sources, &mut text).unwrap();
        assert_eq!(text, "dev: 1 questions, 0 rejected\ntest: 0 questions, 0 rejected\n");
        std::fs::write(root.path().join("eval/questions/test.jsonl"), "{").unwrap();
        assert!(matches!(
            check(root.path(), &sources, &mut text),
            Err(EvalError::Parse { .. })
        ));
        std::fs::write(root.path().join("eval/questions/dev.jsonl"), good.clone() + &good).unwrap();
        assert!(matches!(
            check(root.path(), &sources, &mut text),
            Err(EvalError::DuplicateId(_))
        ));
        let empty = TempDir::new().unwrap();
        assert!(matches!(
            check(empty.path(), &sources, &mut text),
            Err(EvalError::Config(_))
        ));
    }

    #[tokio::test]
    async fn a_run_asks_every_system_and_writes_the_report() {
        let id = id_in(Set::Dev, "spawn");
        let (root, _corpus) = root(&line(&id, "tokio", "needle"), "");
        let (dir, zenodotus) = stub();
        let structured = json!({"results": [{"text": "nothing"}, {"text": "the needle is here"}]});
        std::fs::write(dir.path().join("query_docs.json"), structured.to_string()).unwrap();
        let (_server, url) = fake("needle first\n--------------------------------\nthen more", false).await;
        let (_unused, grounded) = stub();
        let out = root.path().join("results");
        let plan = Plan {
            set: Set::Dev,
            systems: vec![SystemName::Zenodotus, SystemName::Context7, SystemName::GroundedDocs],
            tools: Tools {
                zenodotus,
                grounded: grounded.clone(),
                neuledge: grounded,
                context7_url: url,
                context7_token: None,
                work: root.path().join("work"),
            },
            out: out.clone(),
        };
        let clock = FixedClock::new(Timestamp::from_unix(100));
        let sources = root.path().join("sources");
        let text = execute(root.path(), &sources, &plan, &clock).await.unwrap();
        let results = out.join("1970-01-01T00-01-40Z-dev");
        assert!(text.contains(&format!("Raw results: {}", results.display())), "{text}");
        assert!(text.contains("| zenodotus | 1 | 0 | 0 | 1.000 ["), "{text}");
        assert!(text.contains("| context7 | 1 | 0 | 0 | 1.000 ["), "{text}");
        assert!(
            text.contains("| grounded-docs | not run: no local copy of tokio at "),
            "{text}"
        );
        assert!(text.contains("Goal 4: "), "{text}");
        assert_eq!(std::fs::read_to_string(results.join("summary.txt")).unwrap(), text);
        assert!(results.join("zenodotus.jsonl").exists());
        assert_eq!(calls(&dir)[1]["arguments"]["library_id"], "tokio");

        let unknown = line(&id_in(Set::Dev, "x"), "react", "needle");
        std::fs::write(root.path().join("eval/questions/dev.jsonl"), unknown).unwrap();
        let error = execute(root.path(), &sources, &plan, &clock).await.unwrap_err();
        assert!(matches!(error, EvalError::UnknownScope { .. }), "{error}");
        std::fs::write(root.path().join("eval/questions/dev.jsonl"), "{").unwrap();
        let error = execute(root.path(), &sources, &plan, &clock).await.unwrap_err();
        assert!(matches!(error, EvalError::Parse { .. }), "{error}");
        let empty = TempDir::new().unwrap();
        let error = execute(empty.path(), &sources, &plan, &clock).await.unwrap_err();
        assert!(matches!(error, EvalError::Config(_)), "{error}");
        std::fs::write(root.path().join("eval/questions/dev.jsonl"), "").unwrap();
        std::fs::remove_dir_all(&out).unwrap();
        std::fs::write(&out, "").unwrap();
        let error = execute(root.path(), &sources, &plan, &clock).await.unwrap_err();
        assert!(matches!(error, EvalError::Io { .. }), "{error}");
    }

    #[tokio::test]
    async fn the_command_line_dispatches_to_each_subcommand() {
        let id = id_in(Set::Dev, "spawn");
        let (root, corpus) = root(&line(&id, "tokio", "needle"), "");
        let neuledge_home = root.path().join("work");
        for case in &corpus.scopes {
            std::fs::create_dir_all(case.docs()).unwrap();
        }
        let base = [
            "--root".to_owned(),
            root.path().to_string_lossy().into_owned(),
            "--sources".to_owned(),
            root.path().join("sources").to_string_lossy().into_owned(),
        ];
        let tail = |command: &str, system: &str| -> Cli {
            let mut args: Vec<String> = base.to_vec();
            args.extend(
                [
                    command,
                    "--system",
                    system,
                    "--zenodotus",
                    stub_program().to_str().unwrap(),
                    "--npx",
                    "/nonexistent/npx",
                    "--work",
                    neuledge_home.to_str().unwrap(),
                    "--out",
                    root.path().join("results").to_str().unwrap(),
                ]
                .map(str::to_owned),
            );
            Cli::try_parse_from(std::iter::once("zenodotus-eval".to_owned()).chain(args)).unwrap()
        };
        let clock = FixedClock::new(Timestamp::from_unix(0));
        let (text, result) = run(tail("run", "zenodotus"), &clock).await;
        result.unwrap();
        assert!(text.contains("| zenodotus | 0 | 0 | 1 | - |"), "{text}");
        let (text, result) = run(tail("compare", "neuledge-context"), &clock).await;
        result.unwrap();
        assert!(text.starts_with("zenodotus-eval: test set, 0 questions\n"), "{text}");
        assert!(
            text.contains("| neuledge-context | not run: cannot build tokio: cannot start "),
            "{text}"
        );
        let mut validate: Vec<String> = base.to_vec();
        validate.push("validate".to_owned());
        let cli = Cli::try_parse_from(std::iter::once("zenodotus-eval".to_owned()).chain(validate)).unwrap();
        let (text, result) = run(cli, &clock).await;
        result.unwrap();
        assert_eq!(text, "dev: 1 questions, 0 rejected\ntest: 0 questions, 0 rejected\n");
        std::fs::write(root.path().join("eval/questions/dev.jsonl"), "{").unwrap();
        let (text, result) = run(tail("run", "zenodotus"), &clock).await;
        assert_eq!(text, "");
        assert!(matches!(result, Err(EvalError::Parse { .. })));
    }

    #[tokio::test]
    async fn the_performance_commands_measure_print_and_fail() {
        let (root, config) = crate::perf::tests::indexed();
        let config = config.to_string_lossy().into_owned();
        let stub = stub_program().to_string_lossy().into_owned();
        let clock = FixedClock::new(Timestamp::from_unix(0));
        let cli =
            |args: &[&str]| Cli::try_parse_from(std::iter::once("zenodotus-eval").chain(args.iter().copied())).unwrap();
        let (text, result) = run(
            cli(&["perf-queries", "--config", &config, "--library", "tokio"]),
            &clock,
        )
        .await;
        result.unwrap();
        assert_eq!(
            text,
            "Spawn with task::spawn\ndoc27.md page27::run\ndoc90.md page90::run\n"
        );
        let (text, result) = run(cli(&["cold-start", "--config", &config, "--zenodotus", &stub]), &clock).await;
        result.unwrap();
        assert!(
            text.starts_with("tools/list answered ") && text.ends_with(" ms after start\n"),
            "{text}"
        );
        let queries = root.path().join("queries.txt");
        std::fs::write(&queries, "page\n").unwrap();
        let out = root.path().join("perf").to_string_lossy().into_owned();
        let perf = [
            "perf",
            "--config",
            &config,
            "--library",
            "tokio",
            "--queries",
            queries.to_str().unwrap(),
            "--zenodotus",
            &stub,
            "--out",
            &out,
            "--warmup",
            "1",
            "--rounds",
            "2",
        ];
        let (text, result) = run(cli(&perf), &clock).await;
        result.unwrap();
        assert!(text.starts_with("tokio: 150 sections, 1 queries\n"), "{text}");
        assert!(text.contains("| stdio MCP | 2 | "), "{text}");
        let (text, result) = run(
            cli(&["perf-queries", "--config", &config, "--library", "react"]),
            &clock,
        )
        .await;
        assert_eq!(text, "");
        assert!(matches!(result, Err(EvalError::NoLibrary { .. })));
        assert!(matches!(cli(&["perf"]).command, Command::Perf(_)));
        let plan = PerfOnly::try_parse_from(["perf"]).unwrap().args.plan();
        assert_eq!(plan.config, Path::new("target/perf/zenodotus.toml"));
        assert_eq!((plan.library.as_str(), plan.warmup, plan.rounds), ("mdn", 1_000, 10));
        assert_eq!(
            (plan.queries.to_str(), plan.time),
            (Some("eval/perf_queries.txt"), None)
        );
    }
}
