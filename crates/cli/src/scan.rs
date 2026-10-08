use std::env;
use std::process::ExitCode;

use clap::{Args, ValueEnum};

use crate::pipeline::{self, ScanRun};
use crate::write_to_stdout;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ScanFormat {
    Text,
    Json,
}

#[derive(Debug, Args)]
pub struct ScanArguments {
    #[arg(long, value_enum, default_value_t = ScanFormat::Text, help = "Output format")]
    format: ScanFormat,
}

pub fn run(arguments: &ScanArguments) -> ExitCode {
    let scan_run = env::current_dir()
        .map_err(anyhow::Error::from)
        .and_then(|current_directory| pipeline::scan(&current_directory));

    match scan_run {
        Ok(scan_run) => print_scan(&scan_run, arguments.format),
        Err(error) => {
            eprintln!("prober scan: {error:#}");

            ExitCode::FAILURE
        }
    }
}

fn print_scan(scan_run: &ScanRun, format: ScanFormat) -> ExitCode {
    let output = match format {
        ScanFormat::Text => scan_run.report.to_text(),
        ScanFormat::Json => scan_run.report.to_json(),
    };

    for warning in &scan_run.warnings {
        eprintln!("prober scan: warning: {warning}");
    }

    eprintln!("{}", scan_run.report.summary_line());

    write_to_stdout(&output)
}
