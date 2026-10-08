use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Args;
use prober_core::files::RepoPath;
use prober_core::junit::{ReportPaths, read_junit_report};
use prober_core::outcomes::TestOutcomes;
use prober_core::shadow::{RecordedSelection, ShadowResult};
use prober_vcs::GitRepository;

use crate::write_to_stdout;

const JOB_SUMMARY_VARIABLE: &str = "GITHUB_STEP_SUMMARY";

#[derive(Debug, Args)]
pub struct ShadowArguments {
    #[arg(long, help = "Selection written by `prober --format json`")]
    selection: PathBuf,

    #[arg(
        long = "report",
        value_name = "REPORT",
        required = true,
        help = "JUnit XML report of the full test run, repeated for sharded runs"
    )]
    reports: Vec<PathBuf>,

    #[arg(
        long,
        help = "Directory the test runner ran in, relative to the repository root, when report paths are relative to it"
    )]
    runner_directory: Option<String>,
}

pub fn run(arguments: &ShadowArguments) -> ExitCode {
    match compare(arguments) {
        Ok(shadow_result) => print_shadow_result(&shadow_result),
        Err(error) => {
            eprintln!("prober shadow: {error:#}");

            ExitCode::FAILURE
        }
    }
}

fn compare(arguments: &ShadowArguments) -> Result<ShadowResult> {
    let selection_json = fs::read_to_string(&arguments.selection)
        .with_context(|| format!("cannot read {}", arguments.selection.display()))?;
    let selection = RecordedSelection::parse(&selection_json)
        .with_context(|| arguments.selection.display().to_string())?;
    let runner_directory = arguments
        .runner_directory
        .clone()
        .map(RepoPath::new)
        .transpose()
        .context("invalid --runner-directory")?;
    let report_paths = ReportPaths::new(&repository_root()?, runner_directory);
    let mut outcomes = TestOutcomes::default();

    for report_path in &arguments.reports {
        let report_xml = fs::read_to_string(report_path)
            .with_context(|| format!("cannot read {}", report_path.display()))?;

        read_junit_report(&report_xml, &report_paths, &mut outcomes)
            .with_context(|| report_path.display().to_string())?;
    }

    Ok(ShadowResult::compare(&selection, &outcomes))
}

fn repository_root() -> Result<PathBuf> {
    let current_directory = env::current_dir()?;

    match GitRepository::discover(&current_directory) {
        Ok(repository) => Ok(repository.work_dir()?.to_path_buf()),
        Err(_) => Ok(current_directory),
    }
}

fn print_shadow_result(shadow_result: &ShadowResult) -> ExitCode {
    if let Err(error) = append_job_summary(&shadow_result.to_markdown()) {
        eprintln!("prober shadow: warning: cannot write the job summary: {error:#}");
    }

    eprintln!("{}", shadow_result.summary_line());

    write_to_stdout(&shadow_result.to_json())
}

fn append_job_summary(markdown: &str) -> Result<()> {
    let Some(job_summary_path) = env::var_os(JOB_SUMMARY_VARIABLE).filter(|path| !path.is_empty())
    else {
        return Ok(());
    };
    let mut job_summary = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&job_summary_path)
        .with_context(|| format!("cannot open {}", PathBuf::from(&job_summary_path).display()))?;

    job_summary.write_all(markdown.as_bytes())?;

    Ok(())
}
