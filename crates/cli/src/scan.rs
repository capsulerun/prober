use std::env;
use std::process::ExitCode;

use clap::{Args, ValueEnum};

use prober_core::scan::ScanReport;

use crate::pipeline;
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
    let scan_report = env::current_dir()
        .map_err(anyhow::Error::from)
        .and_then(|current_directory| pipeline::scan(&current_directory));

    match scan_report {
        Ok(scan_report) => print_scan(&scan_report, arguments.format),
        Err(error) => {
            eprintln!("prober scan: {error:#}");

            ExitCode::FAILURE
        }
    }
}

fn print_scan(scan_report: &ScanReport, format: ScanFormat) -> ExitCode {
    let output = match format {
        ScanFormat::Text => scan_report.to_text(),
        ScanFormat::Json => scan_report.to_json(),
    };

    eprintln!("{}", scan_report.summary_line());

    write_to_stdout(&output)
}
