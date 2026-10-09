mod pipeline;
mod scan;
mod shadow;
mod why;

use std::env;
use std::io::{self, Write};
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use prober_core::report::ListSeparator;

use crate::pipeline::{OnError, Revisions, Selection};
use crate::scan::ScanArguments;
use crate::shadow::ShadowArguments;
use crate::why::WhyArguments;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Json,
    List,
}

#[derive(Debug, Parser)]
#[command(
    name = "prober",
    version,
    about = "Selects the test files impacted by the changes between two revisions",
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct Arguments {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    selection: SelectionArguments,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Compares a recorded selection with the JUnit reports of a full test run")]
    Shadow(ShadowArguments),

    #[command(
        about = "Reports what Prober infers from the repository, what it cannot resolve, and the files no test reaches"
    )]
    Scan(ScanArguments),

    #[command(
        about = "Explains why a test file is selected for the changes between two revisions, or why it is not"
    )]
    Why(WhyArguments),
}

#[derive(Debug, Args)]
struct SelectionArguments {
    #[arg(long, required = true, help = "Base revision, e.g. origin/main")]
    base: Option<String>,

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

    match &arguments.command {
        Some(Command::Shadow(shadow_arguments)) => shadow::run(shadow_arguments),
        Some(Command::Scan(scan_arguments)) => scan::run(scan_arguments),
        Some(Command::Why(why_arguments)) => why::run(why_arguments),
        None => select(&arguments.selection),
    }
}

fn select(arguments: &SelectionArguments) -> ExitCode {
    if arguments.nul_separated && arguments.format != Format::List {
        Arguments::command()
            .error(
                ErrorKind::ArgumentConflict,
                "-0 only applies to --format list",
            )
            .exit();
    }

    let revisions = Revisions {
        base: arguments
            .base
            .clone()
            .expect("clap requires --base without a subcommand"),
        head: arguments.head.clone(),
    };
    let selection = env::current_dir()
        .map_err(anyhow::Error::from)
        .and_then(|current_directory| {
            pipeline::run(&current_directory, &revisions, arguments.on_error)
        });

    match selection {
        Ok(selection) => print_selection(&selection, arguments),
        Err(error) => {
            eprintln!("prober: {error:#}");

            ExitCode::FAILURE
        }
    }
}

fn print_selection(selection: &Selection, arguments: &SelectionArguments) -> ExitCode {
    let report = &selection.report;

    print_warnings(selection);

    let output = match arguments.format {
        Format::Json => report.to_json(),
        Format::List if arguments.nul_separated => report.to_list(ListSeparator::Nul),
        Format::List => report.to_list(ListSeparator::Newline),
    };

    eprintln!("{}", report.summary_line());

    write_to_stdout(&output)
}

fn print_warnings(selection: &Selection) {
    for warning in &selection.warnings {
        eprintln!("prober: warning: {warning}");
    }
}

fn write_to_stdout(output: &str) -> ExitCode {
    match io::stdout().lock().write_all(output.as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("prober: cannot write the output: {error}");

            ExitCode::FAILURE
        }
    }
}
