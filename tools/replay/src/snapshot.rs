use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const SECONDS_PER_DAY: i64 = 86_400;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CiRun {
    pub database_id: u64,
    pub event: String,
    pub head_sha: String,
    pub head_branch: String,
    pub created_at: String,
    pub url: String,
}

impl CiRun {
    pub fn is_pull_request(&self) -> bool {
        self.event == "pull_request"
    }

    pub fn created_at_seconds(&self) -> Option<i64> {
        unix_seconds(&self.created_at)
    }
}

pub struct Snapshot {
    directory: PathBuf,
}

impl Snapshot {
    pub fn new(directory: &Path) -> Self {
        Self {
            directory: directory.to_path_buf(),
        }
    }

    pub fn runs(&self) -> Result<Vec<CiRun>> {
        let runs_path = self.runs_path();
        let runs_text = match fs::read_to_string(&runs_path) {
            Ok(runs_text) => runs_text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", runs_path.display()));
            }
        };

        runs_text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line)
                    .with_context(|| format!("invalid run in {}", runs_path.display()))
            })
            .collect()
    }

    pub fn save_runs(&self, runs: &[CiRun]) -> Result<()> {
        let mut runs_by_id: Vec<&CiRun> = runs.iter().collect();

        runs_by_id.sort_by_key(|run| run.database_id);

        let runs_text: String = runs_by_id
            .iter()
            .map(|run| serde_json::to_string(run).map(|line| line + "\n"))
            .collect::<serde_json::Result<_>>()?;

        self.write(&self.runs_path(), &runs_text)
    }

    pub fn job_log_path(&self, run: &CiRun, job_id: &str) -> PathBuf {
        self.job_logs_directory(run).join(format!("{job_id}.log"))
    }

    pub fn job_logs(&self, run: &CiRun) -> Result<Vec<PathBuf>> {
        files_with_extension(&self.job_logs_directory(run), "log")
    }

    pub fn junit_directory(&self, run: &CiRun) -> PathBuf {
        self.directory
            .join("junit")
            .join(run.database_id.to_string())
    }

    pub fn junit_reports(&self, run: &CiRun) -> Result<Vec<PathBuf>> {
        files_with_extension(&self.junit_directory(run), "xml")
    }

    pub fn changes_path(&self) -> PathBuf {
        self.directory.join("changes.jsonl")
    }

    pub fn failures_path(&self) -> PathBuf {
        self.directory.join("failures.jsonl")
    }

    pub fn write(&self, path: &Path, content: &str) -> Result<()> {
        if let Some(parent_directory) = path.parent() {
            fs::create_dir_all(parent_directory)?;
        }

        fs::write(path, content).with_context(|| format!("cannot write {}", path.display()))
    }

    fn runs_path(&self) -> PathBuf {
        self.directory.join("runs.jsonl")
    }

    fn job_logs_directory(&self, run: &CiRun) -> PathBuf {
        self.directory
            .join("logs")
            .join(run.database_id.to_string())
    }
}

fn files_with_extension(directory: &Path, extension: &str) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    collect_files_with_extension(directory, extension, &mut files)?;
    files.sort();

    Ok(files)
}

fn collect_files_with_extension(
    directory: &Path,
    extension: &str,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };

    for entry in entries {
        let entry_path = entry?.path();

        if entry_path.is_dir() {
            collect_files_with_extension(&entry_path, extension, files)?;
        } else if entry_path
            .extension()
            .is_some_and(|entry_extension| entry_extension == extension)
        {
            files.push(entry_path);
        }
    }

    Ok(())
}

fn unix_seconds(timestamp: &str) -> Option<i64> {
    let (date, time) = timestamp.strip_suffix('Z')?.split_once('T')?;
    let mut date_parts = date.split('-').map(|part| part.parse::<i64>().ok());
    let mut time_parts = time.split(':').map(|part| part.parse::<f64>().ok());
    let (year, month, day) = (
        date_parts.next()??,
        date_parts.next()??,
        date_parts.next()??,
    );
    let (hours, minutes, seconds) = (
        time_parts.next()??,
        time_parts.next()??,
        time_parts.next()??,
    );
    let seconds_in_day = hours as i64 * 3_600 + minutes as i64 * 60 + seconds as i64;

    Some(days_since_epoch(year, month, day) * SECONDS_PER_DAY + seconds_in_day)
}

fn days_since_epoch(year: i64, month: i64, day: i64) -> i64 {
    let year_starting_in_march = if month <= 2 { year - 1 } else { year };
    let era = year_starting_in_march.div_euclid(400);
    let year_of_era = year_starting_in_march - era * 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;

    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_timestamps_convert_to_unix_seconds() {
        assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_seconds("2026-10-08T09:55:30Z"), Some(1_791_453_330));
        assert_eq!(unix_seconds("2000-02-29T23:59:59Z"), Some(951_868_799));
        assert_eq!(unix_seconds("yesterday"), None);
    }

    #[test]
    fn gh_run_list_output_reads_as_runs() {
        let gh_output = r#"{"createdAt":"2026-10-08T09:55:30Z","databaseId":101,"event":"pull_request","headBranch":"fix/parser","headSha":"c6506e8","url":"https://github.com/acme/app/actions/runs/101"}"#;
        let run: CiRun = serde_json::from_str(gh_output).unwrap();

        assert_eq!(
            run,
            CiRun {
                database_id: 101,
                event: "pull_request".to_owned(),
                head_sha: "c6506e8".to_owned(),
                head_branch: "fix/parser".to_owned(),
                created_at: "2026-10-08T09:55:30Z".to_owned(),
                url: "https://github.com/acme/app/actions/runs/101".to_owned(),
            }
        );
        assert!(run.is_pull_request());
    }
}
