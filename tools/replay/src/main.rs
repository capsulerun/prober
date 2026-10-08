mod changes;
mod collect;
mod commands;
mod config;
mod failures;
mod logs;
mod record;
mod selections;
mod snapshot;
mod worktree;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::collect::CollectArguments;
use crate::failures::FailuresArguments;
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
        about = "Downloads the failed runs of a CI workflow and their logs into a snapshot (needs gh)"
    )]
    Collect(CollectArguments),

    #[command(about = "Extracts the changes and their failed test files from a snapshot")]
    Failures(FailuresArguments),
}

fn main() -> ExitCode {
    let outcome = match Arguments::parse().command {
        Command::Selections(arguments) => selections::run(&arguments),
        Command::Collect(arguments) => collect::run(&arguments),
        Command::Failures(arguments) => failures::run(&arguments),
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
