//! `cargo xtask perf`: builds the MDN performance index and measures zenodotus on it with external tools.
//!
//! This module only orchestrates `cargo`, `zenodotus`, `zenodotus-eval`, GNU `time` and `hyperfine`, so it is the
//! one file excluded from coverage. Everything it reports is printed by those tools, read from their files, or
//! computed by the tested functions of `perf_report`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use crate::perf_report::{belongs_to_run, counts, cpu_seconds, time_field, time_report};

/// The pinned `mdn/content` commit of the performance corpus.
const MDN_COMMIT: &str = "defd0924e23d3a2bbaec3dfe079a1f201b536abe";

/// The performance config. The tarball limits are raised for `mdn/content`, which is larger than the defaults
/// allow; the data directory is relative to the config.
const CONFIG: &str = "# The performance corpus of `cargo xtask perf`: MDN's English pages at a pinned commit.

[settings]
data_dir = \"data\"

[settings.fetch]
tarball_max_bytes = 2147483648
tar_total_max_bytes = 4294967296
tar_max_entries = 1000000
tarball_timeout_secs = 1800

[[library]]
id = \"mdn\"
name = \"MDN Web Docs\"
[[library.version]]
version = \"latest\"
source = { kind = \"github\", repo = \"mdn/content\", reference = \"COMMIT\", paths = [\"files/en-us\"] }
";

/// GNU `time`.
const TIME: &str = "/usr/bin/time";

/// The paths `perf` works with.
struct Paths {
    root: PathBuf,
    dir: PathBuf,
    config: PathBuf,
    zenodotus: PathBuf,
    eval: PathBuf,
    queries: PathBuf,
}

impl Paths {
    fn new(root: &Path) -> Self {
        let dir = root.join("target/perf");
        Self {
            root: root.to_owned(),
            config: dir.join("zenodotus.toml"),
            zenodotus: root.join("target/release/zenodotus"),
            eval: root.join("target/release/zenodotus-eval"),
            queries: root.join("eval/perf_queries.txt"),
            dir,
        }
    }
}

/// Runs `command` in `dir`, failing on a non-zero exit, and returns its stdout.
fn run(command: &mut Command, dir: &Path) -> Result<String, String> {
    let shown = format!("{command:?}");
    let output = command
        .current_dir(dir)
        .output()
        .map_err(|error| format!("cannot run {shown}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{shown} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Runs `zenodotus sync` under `time -v` and returns the report and the wall-clock seconds.
fn timed_sync(paths: &Paths, name: &str) -> Result<(String, f64), String> {
    let report = paths.dir.join(time_report(name));
    let start = Instant::now();
    run(
        Command::new(TIME)
            .arg("-v")
            .arg("-o")
            .arg(&report)
            .arg(&paths.zenodotus)
            .arg("--config")
            .arg(&paths.config)
            .arg("sync"),
        &paths.root,
    )?;
    let seconds = start.elapsed().as_secs_f64();
    let text =
        std::fs::read_to_string(&report).map_err(|error| format!("cannot read {}: {error}", report.display()))?;
    Ok((text, seconds))
}

/// The total size in bytes of the files under `dir`.
fn size_of(dir: &Path) -> u64 {
    std::fs::read_dir(dir).map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .map(|entry| match entry.file_type() {
                Ok(kind) if kind.is_dir() => size_of(&entry.path()),
                Ok(_) => entry.metadata().map_or(0, |metadata| metadata.len()),
                Err(_) => 0,
            })
            .sum()
    })
}

/// Copies this run's files into `target/perf/runs/<UTC time>/`, since the next run overwrites them, and returns that
/// directory. A run that did not sync (`--reuse` with an index) leaves out `sync-time.txt`, which an earlier run wrote.
fn keep_run(paths: &Paths, synced: bool) -> Result<PathBuf, String> {
    let stamp = run(Command::new("date").args(["-u", "+%Y-%m-%dT%H-%M-%SZ"]), &paths.root)?;
    let kept = paths.dir.join("runs").join(stamp.trim());
    std::fs::create_dir_all(&kept).map_err(|error| format!("cannot create {}: {error}", kept.display()))?;
    let entries =
        std::fs::read_dir(&paths.dir).map_err(|error| format!("cannot read {}: {error}", paths.dir.display()))?;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_file() && belongs_to_run(&entry.file_name().to_string_lossy(), synced) {
            std::fs::copy(&path, kept.join(entry.file_name()))
                .map_err(|error| format!("cannot copy {}: {error}", path.display()))?;
        }
    }
    Ok(kept)
}

/// The machine: CPU, cores, memory, kernel and compiler.
fn machine(root: &Path) -> String {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let cpu = cpuinfo
        .lines()
        .find_map(|line| line.strip_prefix("model name"))
        .and_then(|line| line.split_once(':'))
        .map_or("unknown", |(_, name)| name.trim());
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let memory = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .map_or("unknown", str::trim);
    let threads = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    let kernel = run(Command::new("uname").arg("-srm"), root).unwrap_or_default();
    let rustc = run(Command::new("rustc").arg("-V"), root).unwrap_or_default();
    format!(
        "CPU: {cpu}, {threads} threads\nMemory: {memory}\nKernel: {}\nCompiler: {}\n",
        kernel.trim(),
        rustc.trim()
    )
}

/// Runs `perf` when `args` ask for it, as `perf [--reuse]`; `None` for any other task.
pub fn dispatch(args: &[String], root: &Path) -> Option<ExitCode> {
    if args.first().map(String::as_str) != Some("perf") {
        return None;
    }
    Some(match perf(root, args[1..].iter().any(|arg| arg == "--reuse")) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    })
}

/// Builds the index unless `reuse` and it exists, measures, and prints the results.
fn perf(root: &Path, reuse: bool) -> Result<(), String> {
    let paths = Paths::new(root);
    std::fs::create_dir_all(&paths.dir).map_err(|error| format!("cannot create {}: {error}", paths.dir.display()))?;
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    println!("Building release binaries");
    run(
        Command::new(&cargo).args(["build", "--release", "--bin", "zenodotus", "--bin", "zenodotus-eval"]),
        root,
    )?;
    std::fs::write(&paths.config, CONFIG.replace("COMMIT", MDN_COMMIT))
        .map_err(|error| format!("cannot write {}: {error}", paths.config.display()))?;
    let data = paths.dir.join("data");
    let mut report = machine(root);
    let sync = !(reuse && data.exists());
    if sync {
        // A fresh build times indexing from scratch, download included.
        let _ = std::fs::remove_dir_all(&data);
        println!("Syncing mdn/content@{MDN_COMMIT} (download and indexing)");
        let (time, seconds) = timed_sync(&paths, "sync")?;
        let status = run(
            Command::new(&paths.zenodotus)
                .arg("--config")
                .arg(&paths.config)
                .arg("status"),
            root,
        )?;
        let (documents, count) = counts(&status).ok_or("cannot read the document and section counts from `status`")?;
        let documents = f64::from(documents);
        let cpu = cpu_seconds(&time).ok_or("cannot read the user and system CPU time of the sync")?;
        let _ = write!(
            report,
            "\nInitial sync: {seconds:.1} s for {count} sections, {:.0} sections/s, download included\n\
             Indexing throughput: {documents:.0} documents in {cpu:.2} CPU s (user + system), {:.1} documents per CPU \
             second\n\
             Peak RSS while syncing: {} KiB\n",
            f64::from(count) / seconds,
            documents / cpu,
            time_field(&time, "Maximum resident set size (kbytes):").unwrap_or("unknown"),
        );
    }
    println!("Re-syncing with no upstream change");
    let (time, seconds) = timed_sync(&paths, "resync")?;
    let _ = write!(
        report,
        "Re-sync with no upstream change: {seconds:.2} s, peak RSS {} KiB\nIndex size on disk: {:.1} MiB\n",
        time_field(&time, "Maximum resident set size (kbytes):").unwrap_or("unknown"),
        f64::from(u32::try_from(size_of(&data) / 1024).unwrap_or(u32::MAX)) / 1024.0,
    );
    if !paths.queries.exists() {
        println!("Generating {}", paths.queries.display());
        let queries = run(
            Command::new(&paths.eval)
                .arg("perf-queries")
                .arg("--config")
                .arg(&paths.config),
            root,
        )?;
        std::fs::write(&paths.queries, queries)
            .map_err(|error| format!("cannot write {}: {error}", paths.queries.display()))?;
    }
    println!("Timing cold start with hyperfine");
    let cold_start = format!(
        "{} cold-start --config {} --zenodotus {}",
        paths.eval.display(),
        paths.config.display(),
        paths.zenodotus.display()
    );
    let hyperfine = run(
        Command::new("hyperfine")
            .args(["-N", "--warmup", "3", "--runs", "30", "--export-json"])
            .arg(paths.dir.join("cold-start.json"))
            .arg(&cold_start),
        root,
    )?;
    let _ = write!(
        report,
        "\nCold start, serve to first tools/list (hyperfine):\n{hyperfine}"
    );
    println!("Measuring query latency and peak RSS while serving");
    let latency = run(
        Command::new(&paths.eval)
            .arg("perf")
            .arg("--config")
            .arg(&paths.config)
            .arg("--queries")
            .arg(&paths.queries)
            .arg("--zenodotus")
            .arg(&paths.zenodotus)
            .arg("--time")
            .arg(TIME)
            .arg("--out")
            .arg(&paths.dir),
        root,
    )?;
    let _ = write!(report, "\n{latency}");
    let summary = paths.dir.join("summary.txt");
    std::fs::write(&summary, &report).map_err(|error| format!("cannot write {}: {error}", summary.display()))?;
    let kept = keep_run(&paths, sync)?;
    println!("\n{report}\nThis run's files: {}", kept.display());
    Ok(())
}
