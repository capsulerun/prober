use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use prober_core::aggregate::ShadowRuns;
use prober_core::config::Config;
use prober_core::files::RepoPath;
use prober_core::junit::{ReportPaths, read_junit_report};
use prober_core::outcomes::TestOutcomes;
use prober_core::report::CONFIG_FILE_NAME;
use prober_core::shadow::{RecordedSelection, ShadowResult};
use prober_vcs::GitRepository;

use crate::write_to_stdout;

const JOB_SUMMARY_VARIABLE: &str = "GITHUB_STEP_SUMMARY";

const SHADOW_EXTENSION: &str = "json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AggregateFormat {
    Text,
    Json,
}

#[derive(Debug, Args)]
pub struct ShadowArguments {
    #[arg(
        long,
        required_unless_present = "aggregate",
        help = "Selection written by `prober --format json`"
    )]
    selection: Option<PathBuf>,

    #[arg(
        long = "report",
        value_name = "REPORT",
        required_unless_present = "aggregate",
        help = "JUnit XML report of the full test run, repeated for sharded runs"
    )]
    reports: Vec<PathBuf>,

    #[arg(
        long,
        help = "Directory the test runner ran in, relative to the repository root, when report paths are relative to it"
    )]
    runner_directory: Option<String>,

    #[arg(
        long,
        value_name = "SHADOW_JSON",
        num_args = 1..,
        conflicts_with_all = ["selection", "reports", "runner_directory"],
        help = "Read the JSON files written by earlier shadow runs (or the directories holding them) and recommend aggressive options"
    )]
    aggregate: Vec<PathBuf>,

    #[arg(
        long,
        requires = "aggregate",
        help = "prober.toml whose [protected] and [[defer]] the recommendations follow, by default the one at the repository root"
    )]
    config: Option<PathBuf>,

    #[arg(
        long,
        value_enum,
        default_value_t = AggregateFormat::Text,
        requires = "aggregate",
        help = "Output format of the aggregation"
    )]
    format: AggregateFormat,
}

pub fn run(arguments: &ShadowArguments) -> ExitCode {
    if !arguments.aggregate.is_empty() {
        return match aggregate(arguments) {
            Ok(exit_code) => exit_code,
            Err(error) => {
                eprintln!("prober shadow: {error:#}");

                ExitCode::FAILURE
            }
        };
    }

    match compare(arguments) {
        Ok(shadow_result) => print_shadow_result(&shadow_result),
        Err(error) => {
            eprintln!("prober shadow: {error:#}");

            ExitCode::FAILURE
        }
    }
}

fn aggregate(arguments: &ShadowArguments) -> Result<ExitCode> {
    let mut shadow_runs = ShadowRuns::default();

    for shadow_file in shadow_files(&arguments.aggregate)? {
        let shadow_json = fs::read_to_string(&shadow_file)
            .with_context(|| format!("cannot read {}", shadow_file.display()))?;

        shadow_runs
            .add(&shadow_json)
            .with_context(|| shadow_file.display().to_string())?;
    }

    if shadow_runs.is_empty() {
        bail!("no shadow JSON file found");
    }

    let config = read_config(arguments.config.as_deref())?;
    let aggregate = shadow_runs.aggregate(&config.aggressive);
    let markdown = aggregate.to_markdown();

    if let Err(error) = append_job_summary(&markdown) {
        eprintln!("prober shadow: warning: cannot write the job summary: {error:#}");
    }

    eprintln!("{}", aggregate.summary_line());

    let output = match arguments.format {
        AggregateFormat::Text => markdown,
        AggregateFormat::Json => aggregate.to_json(),
    };

    Ok(write_to_stdout(&output))
}

fn shadow_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut shadow_files = Vec::new();

    for path in paths {
        if path.is_dir() {
            collect_shadow_files(path, &mut shadow_files)?;
        } else {
            shadow_files.push(path.clone());
        }
    }

    Ok(shadow_files)
}

fn collect_shadow_files(directory: &Path, shadow_files: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries: Vec<PathBuf> = fs::read_dir(directory)
        .with_context(|| format!("cannot read {}", directory.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<_>>()?;

    entries.sort();

    for entry in entries {
        if entry.is_dir() {
            collect_shadow_files(&entry, shadow_files)?;
        } else if entry
            .extension()
            .is_some_and(|extension| extension == SHADOW_EXTENSION)
        {
            shadow_files.push(entry);
        }
    }

    Ok(())
}

fn read_config(config_path: Option<&Path>) -> Result<Config> {
    let config_path = match config_path {
        Some(config_path) => config_path.to_path_buf(),
        None => {
            let default_path = repository_root()?.join(CONFIG_FILE_NAME);

            if !default_path.exists() {
                return Config::parse("");
            }

            default_path
        }
    };
    let config_text = fs::read_to_string(&config_path)
        .with_context(|| format!("cannot read {}", config_path.display()))?;

    Config::parse(&config_text)
}

fn compare(arguments: &ShadowArguments) -> Result<ShadowResult> {
    let selection_path = arguments
        .selection
        .as_ref()
        .expect("clap requires --selection without --aggregate");
    let selection_json = fs::read_to_string(selection_path)
        .with_context(|| format!("cannot read {}", selection_path.display()))?;
    let selection = RecordedSelection::parse(&selection_json)
        .with_context(|| selection_path.display().to_string())?;
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
