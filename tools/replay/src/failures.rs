use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use prober_core::files::RepoPath;
use prober_core::junit::{ReportPaths, read_junit_report};
use prober_core::outcomes::{TestOutcomes, TestStatus};
use serde::Serialize;

use crate::config::HarnessConfig;
use crate::logs::failed_test_file_paths;
use crate::repository_files::RepositoryFiles;
use crate::snapshot::{CiRun, Snapshot};

const SECONDS_PER_HOUR: i64 = 3_600;

#[derive(Debug, Args)]
pub struct FailuresArguments {
    #[arg(long, help = "Harness config of the benchmarked repository")]
    config: PathBuf,

    #[arg(long, help = "Snapshot directory written by collect")]
    snapshot: PathBuf,

    #[arg(
        long,
        help = "Clone of the benchmarked repository: a reported path missing from it is matched to the only file ending with it (runners started in a package directory)"
    )]
    repo: Option<PathBuf>,
}

#[derive(Serialize)]
struct ChangeRecord<'a> {
    id: &'a str,
    base: &'a str,
    head: &'a str,
}

#[derive(Serialize)]
struct FailureRecord<'a> {
    change: &'a str,
    file: &'a str,
    runs: Vec<u64>,
    pre_existing: bool,
    in_repository: bool,
}

struct RunFailures<'a> {
    run: &'a CiRun,
    failed_files: BTreeSet<RepoPath>,
}

struct ChangeFailures<'a> {
    head: &'a str,
    failed_runs_by_file: BTreeMap<RepoPath, Vec<&'a CiRun>>,
}

pub fn run(arguments: &FailuresArguments) -> Result<String> {
    let config = HarnessConfig::read(&arguments.config)?;
    let snapshot = Snapshot::new(&arguments.snapshot);
    let runs = snapshot.runs()?;
    let runner_directory = config
        .runner_directory
        .clone()
        .map(RepoPath::new)
        .transpose()?;
    let repository_files = arguments
        .repo
        .as_deref()
        .map(|repository| RepositoryFiles::read(repository, &config.base_branch))
        .transpose()?;
    let extractor = FailureExtractor {
        snapshot: &snapshot,
        repository_files: repository_files.as_ref(),
        report_paths: ReportPaths::new(&config.ci_workspace(), runner_directory),
    };
    let run_failures = runs
        .iter()
        .map(|run| extractor.failures_of(run))
        .collect::<Result<Vec<_>>>()?;
    let (pull_request_failures, other_failures): (Vec<_>, Vec<_>) = run_failures
        .into_iter()
        .partition(|run_failures| run_failures.run.is_pull_request());
    let base_branch_failures: Vec<RunFailures> = other_failures
        .into_iter()
        .filter(|run_failures| run_failures.run.head_branch == config.base_branch)
        .collect();
    let runs_without_failed_test_file = pull_request_failures
        .iter()
        .chain(&base_branch_failures)
        .filter(|run_failures| run_failures.failed_files.is_empty())
        .count();
    let change_failures = group_by_change(&pull_request_failures);
    let pre_existing_window_seconds = config.pre_existing_window_hours as i64 * SECONDS_PER_HOUR;
    let mut changes_lines = String::new();
    let mut failures_lines = String::new();
    let mut failed_test_file_count = 0;
    let mut pre_existing_count = 0;

    for change in &change_failures {
        let change_record = ChangeRecord {
            id: change.head,
            base: &config.base_branch,
            head: change.head,
        };

        changes_lines.push_str(&(serde_json::to_string(&change_record)? + "\n"));

        for (file, failed_runs) in &change.failed_runs_by_file {
            let pre_existing = failed_on_base_branch_nearby(
                file,
                failed_runs,
                &base_branch_failures,
                pre_existing_window_seconds,
            );
            let failure_record = FailureRecord {
                change: change.head,
                file: file.as_str(),
                runs: failed_runs.iter().map(|run| run.database_id).collect(),
                pre_existing,
                in_repository: repository_files
                    .as_ref()
                    .is_none_or(|repository_files| repository_files.contains(file)),
            };

            failures_lines.push_str(&(serde_json::to_string(&failure_record)? + "\n"));
            failed_test_file_count += 1;
            pre_existing_count += usize::from(pre_existing);
        }
    }

    snapshot.write(&snapshot.changes_path(), &changes_lines)?;
    snapshot.write(&snapshot.failures_path(), &failures_lines)?;

    Ok(format!(
        "prober-replay: {} failed runs on pull requests and {} on {}: {} changes with {failed_test_file_count} failed test files ({pre_existing_count} pre-existing); {runs_without_failed_test_file} failed runs without a failed test file",
        pull_request_failures.len(),
        base_branch_failures.len(),
        config.base_branch,
        change_failures.len()
    ))
}

struct FailureExtractor<'a> {
    snapshot: &'a Snapshot,
    report_paths: ReportPaths,
    repository_files: Option<&'a RepositoryFiles>,
}

impl FailureExtractor<'_> {
    fn failures_of<'r>(&self, run: &'r CiRun) -> Result<RunFailures<'r>> {
        let mut failed_files = BTreeSet::new();

        for job_log_path in self.snapshot.job_logs(run)? {
            let job_log_bytes = fs::read(&job_log_path)?;
            let reported_paths = failed_test_file_paths(&String::from_utf8_lossy(&job_log_bytes));

            failed_files.extend(
                reported_paths
                    .iter()
                    .filter_map(|reported_path| self.report_paths.resolve(reported_path))
                    .map(|file| self.located(file)),
            );
        }

        let mut outcomes = TestOutcomes::default();

        for report_path in self.snapshot.junit_reports(run)? {
            let report_xml = fs::read_to_string(&report_path)?;

            read_junit_report(&report_xml, &self.report_paths, &mut outcomes)
                .with_context(|| report_path.display().to_string())?;
        }

        failed_files.extend(
            outcomes
                .files()
                .filter(|outcome| outcome.status == TestStatus::Failed)
                .map(|outcome| self.located(outcome.file.clone())),
        );

        Ok(RunFailures { run, failed_files })
    }
}

impl FailureExtractor<'_> {
    fn located(&self, file: RepoPath) -> RepoPath {
        match self.repository_files {
            Some(repository_files) => repository_files.locate(file),
            None => file,
        }
    }
}

fn group_by_change<'a>(pull_request_failures: &[RunFailures<'a>]) -> Vec<ChangeFailures<'a>> {
    let mut change_failures: Vec<ChangeFailures> = Vec::new();

    for run_failures in pull_request_failures {
        let head = run_failures.run.head_sha.as_str();
        let change_index = match change_failures
            .iter()
            .position(|change| change.head == head)
        {
            Some(change_index) => change_index,
            None if run_failures.failed_files.is_empty() => continue,
            None => {
                change_failures.push(ChangeFailures {
                    head,
                    failed_runs_by_file: BTreeMap::new(),
                });

                change_failures.len() - 1
            }
        };

        for file in &run_failures.failed_files {
            change_failures[change_index]
                .failed_runs_by_file
                .entry(file.clone())
                .or_default()
                .push(run_failures.run);
        }
    }

    change_failures
}

fn failed_on_base_branch_nearby(
    file: &RepoPath,
    failed_pull_request_runs: &[&CiRun],
    base_branch_failures: &[RunFailures],
    window_seconds: i64,
) -> bool {
    base_branch_failures
        .iter()
        .filter(|base_branch_run| base_branch_run.failed_files.contains(file))
        .any(|base_branch_run| {
            failed_pull_request_runs.iter().any(|pull_request_run| {
                match (
                    pull_request_run.created_at_seconds(),
                    base_branch_run.run.created_at_seconds(),
                ) {
                    (Some(pull_request_time), Some(base_branch_time)) => {
                        (pull_request_time - base_branch_time).abs() <= window_seconds
                    }
                    _ => false,
                }
            })
        })
}
