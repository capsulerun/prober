//! The prober command line: prints the test files impacted by the changes between two revisions.

mod pipeline;

use std::env;
use std::io::{self, Write};
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser, ValueEnum};
use prober_core::report::{ListSeparator, Report};

use crate::pipeline::{OnError, Revisions};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Json,
    List,
}

#[derive(Debug, Parser)]
#[command(
    name = "prober",
    version,
    about = "Selects the test files impacted by the changes between two revisions"
)]
struct Arguments {
    #[arg(long, help = "Base revision, e.g. origin/main")]
    base: String,

    #[arg(
        long,
        default_value = "HEAD",
        help = "Head revision, which must be the checked-out commit"
    )]
    head: String,

    #[arg(long, value_enum, default_value_t = Format::List, help = "Output format")]
    format: Format,

    #[arg(
        short = '0',
        long = "null",
        help = "Separate the list with NUL characters instead of newlines"
    )]
    nul_separated: bool,

    #[arg(
        long,
        value_enum,
        default_value_t = OnError::All,
        help = "On error, run every test (all) or exit with an error (fail)"
    )]
    on_error: OnError,
}

fn main() -> ExitCode {
    let arguments = Arguments::parse();

    if arguments.nul_separated && arguments.format != Format::List {
        Arguments::command()
            .error(
                ErrorKind::ArgumentConflict,
                "-0 only applies to --format list",
            )
            .exit();
    }

    let revisions = Revisions {
        base: arguments.base.clone(),
        head: arguments.head.clone(),
    };
    let report = env::current_dir()
        .map_err(anyhow::Error::from)
        .and_then(|current_directory| {
            pipeline::run(&current_directory, &revisions, arguments.on_error)
        });

    match report {
        Ok(report) => print_report(&report, &arguments),
        Err(error) => {
            eprintln!("prober: {error:#}");

            ExitCode::FAILURE
        }
    }
}

fn print_report(report: &Report, arguments: &Arguments) -> ExitCode {
    let output = match arguments.format {
        Format::Json => report.to_json(),
        Format::List if arguments.nul_separated => report.to_list(ListSeparator::Nul),
        Format::List => report.to_list(ListSeparator::Newline),
    };

    eprintln!("{}", report.summary_line());

    match io::stdout().lock().write_all(output.as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("prober: cannot write the report: {error}");

            ExitCode::FAILURE
        }
    }
}
