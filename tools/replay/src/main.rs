mod base_branches;
mod changes;
mod collect;
mod commands;
mod config;
mod failures;
mod logs;
mod probes;
mod record;
mod report;
mod repository_files;
mod selections;
mod snapshot;
mod worktree;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::collect::CollectArguments;
use crate::failures::FailuresArguments;
use crate::probes::ProbeArguments;
use crate::report::ReportArguments;
use crate::selections::SelectionsArguments;

#[derive(Debug, Parser)]
#[command(
    name = "prober-replay",
    about = "Benchmark tooling for Prober: replays history through prober and collects the test failures of a CI"
)]
struct Arguments {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Replays changes through prober and records each selection")]
    Selections(SelectionsArguments),

    #[command(
        about = "Changes one file at a time at one commit and records the selection of each change"
    )]
    Probe(ProbeArguments),

    #[command(
        about = "Downloads the failed runs of a CI workflow and their logs into a snapshot (needs gh)"
    )]
    Collect(CollectArguments),

    #[command(about = "Extracts the changes and their failed test files from a snapshot")]
    Failures(FailuresArguments),

    #[command(
        about = "Writes the benchmark report: savings on merged changes and misses on failed changes"
    )]
    Report(ReportArguments),
}

fn main() -> ExitCode {
    let outcome = match Arguments::parse().command {
        Command::Selections(arguments) => selections::run(&arguments),
        Command::Probe(arguments) => probes::run(&arguments),
        Command::Collect(arguments) => collect::run(&arguments),
        Command::Failures(arguments) => failures::run(&arguments),
        Command::Report(arguments) => report::run(&arguments),
    };

    match outcome {
        Ok(summary_line) => {
            eprintln!("{summary_line}");

            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("prober-replay: {error:#}");

            ExitCode::FAILURE
        }
    }
}
