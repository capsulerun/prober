use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;

use crate::commands::gh;
use crate::config::HarnessConfig;
use crate::snapshot::{CiRun, Snapshot};

const RUN_FIELDS: &str = "databaseId,event,headSha,headBranch,createdAt,url";

#[derive(Debug, Args)]
pub struct CollectArguments {
    #[arg(long, help = "Harness config of the benchmarked repository")]
    config: PathBuf,

    #[arg(
        long,
        help = "Snapshot directory the failed runs and their test job logs are kept in"
    )]
    snapshot: PathBuf,

    #[arg(
        long,
        default_value_t = 200,
        help = "How many of the most recent failed runs to list"
    )]
    limit: usize,
}

struct FailedJob {
    id: String,
    name: String,
}

pub fn run(arguments: &CollectArguments) -> Result<String> {
    let config = HarnessConfig::read(&arguments.config)?;
    let snapshot = Snapshot::new(&arguments.snapshot);
    let test_jobs = config.test_job_matcher()?;
    let mut runs = snapshot.runs()?;
    let known_run_ids: HashSet<u64> = runs.iter().map(|run| run.database_id).collect();
    let new_runs: Vec<CiRun> = list_failed_runs(&config, arguments.limit)?
        .into_iter()
        .filter(|run| !known_run_ids.contains(&run.database_id))
        .collect();
    let mut downloaded_log_count = 0;
    let mut unavailable_log_count = 0;
    let mut skipped_run_count = 0;

    for new_run in &new_runs {
        let Ok(failed_jobs) = failed_jobs(&config, new_run) else {
            skipped_run_count += 1;

            continue;
        };
        let failed_test_jobs = failed_jobs
            .into_iter()
            .filter(|failed_job| test_jobs.matches(&failed_job.name));

        for failed_test_job in failed_test_jobs {
            let job_log = gh(&[
                "api",
                "--allow-escape-sequences",
                &format!(
                    "repos/{}/actions/jobs/{}/logs",
                    config.repository, failed_test_job.id
                ),
            ]);

            match job_log {
                Ok(job_log) => {
                    snapshot.write(
                        &snapshot.job_log_path(new_run, &failed_test_job.id),
                        &job_log,
                    )?;
                    downloaded_log_count += 1;
                }
                Err(_) => unavailable_log_count += 1,
            }
        }

        download_junit_artifacts(&config, &snapshot, new_run)?;
        runs.push(new_run.clone());
        snapshot.save_runs(&runs)?;
    }

    Ok(format!(
        "prober-replay: {} failed runs in the snapshot, {} new; {downloaded_log_count} failed test job logs downloaded, {unavailable_log_count} unavailable (expired or not found); {skipped_run_count} runs skipped after an error, rerun to retry them",
        runs.len(),
        new_runs.len() - skipped_run_count
    ))
}

fn list_failed_runs(config: &HarnessConfig, limit: usize) -> Result<Vec<CiRun>> {
    let runs_json = gh(&[
        "run",
        "list",
        "--repo",
        &config.repository,
        "--workflow",
        &config.workflow,
        "--status",
        "failure",
        "--limit",
        &limit.to_string(),
        "--json",
        RUN_FIELDS,
    ])?;

    serde_json::from_str(&runs_json).context("unexpected gh run list output")
}

fn failed_jobs(config: &HarnessConfig, run: &CiRun) -> Result<Vec<FailedJob>> {
    let failed_job_lines = gh(&[
        "api",
        "--paginate",
        &format!(
            "repos/{}/actions/runs/{}/jobs?filter=latest&per_page=100",
            config.repository, run.database_id
        ),
        "--jq",
        r#".jobs[] | select(.conclusion == "failure") | [(.id | tostring), .name] | @tsv"#,
    ])?;

    Ok(failed_job_lines
        .lines()
        .filter_map(|job_line| job_line.split_once('\t'))
        .map(|(job_id, job_name)| FailedJob {
            id: job_id.to_owned(),
            name: job_name.to_owned(),
        })
        .collect())
}

fn download_junit_artifacts(
    config: &HarnessConfig,
    snapshot: &Snapshot,
    run: &CiRun,
) -> Result<()> {
    if config.junit_artifacts.is_empty() {
        return Ok(());
    }

    let artifact_matcher = config.junit_artifact_matcher()?;
    let artifact_names = gh(&[
        "api",
        &format!(
            "repos/{}/actions/runs/{}/artifacts",
            config.repository, run.database_id
        ),
        "--jq",
        ".artifacts[].name",
    ])?;
    let junit_artifact_names = artifact_names
        .lines()
        .filter(|artifact_name| artifact_matcher.matches(artifact_name));

    for artifact_name in junit_artifact_names {
        let artifact_directory = snapshot.junit_directory(run).join(artifact_name);

        gh(&[
            "run",
            "download",
            &run.database_id.to_string(),
            "--repo",
            &config.repository,
            "--name",
            artifact_name,
            "--dir",
            &artifact_directory.to_string_lossy(),
        ])?;
    }

    Ok(())
}
