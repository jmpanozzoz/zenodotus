//! `zenodotus-eval`: measures retrieval quality and cost of zenodotus and its competitors on a pinned corpus.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod app;
mod bootstrap;
mod corpus;
mod error;
mod judge;
mod metrics;
mod questions;
mod report;
mod run;
mod systems;
mod validate;

use std::process::ExitCode;

use clap::Parser as _;
use zenodotus_core::time::SystemClock;

#[tokio::main]
async fn main() -> ExitCode {
    let (text, result) = app::run(app::Cli::parse(), &SystemClock).await;
    report::print(&text);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report::print_error(&error);
            ExitCode::FAILURE
        }
    }
}
