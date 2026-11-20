//! The `zenodotus` binary.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::io::Write;
use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;
use zenodotus::app::{Context, run};
use zenodotus::args::Cli;
use zenodotus_core::time::SystemClock;

/// The environment variable that sets the log filter, such as `info` or `zenodotus_mcp=debug`.
const LOG_VARIABLE: &str = "ZENODOTUS_LOG";

fn main() -> ExitCode {
    let cli = Cli::parse();
    let filter = EnvFilter::try_from_env(LOG_VARIABLE).unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("the Tokio runtime starts");
    let env = |name: &str| std::env::var(name).ok();
    let mut stdout = std::io::stdout();
    let mut ctx = Context {
        env: &env,
        clock: &SystemClock,
        out: &mut stdout,
    };
    match runtime.block_on(run(cli, &mut ctx)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "error: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}
