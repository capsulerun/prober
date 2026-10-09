//! Probes: one-file changes made at one commit and replayed through prober, to measure which tests a change to each file reaches.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::Value;

use crate::changes::Change;
use crate::commands::git;
use crate::record::{RecordWriter, ReplayRecord, recorded_ids};
use crate::selections::{ProberArguments, ReplaySettings, run_prober, summary_line};
use crate::worktree::Worktree;

const PROBE_COMMIT_SETTINGS: [&str; 6] = [
    "-c",
    "user.name=prober-replay",
    "-c",
    "user.email=prober-replay@localhost",
    "-c",
    "commit.gpgsign=false",
];

#[derive(Debug, Args)]
pub struct ProbeArguments {
    #[arg(long, help = "Repository the probed commit belongs to")]
    repo: PathBuf,

    #[arg(long, default_value = "HEAD", help = "Commit whose files are probed")]
    commit: String,

    #[arg(
        long,
        help = "Text file listing the repository paths to probe, one per line: each probe adds a line to one file and replays that change"
    )]
    files: PathBuf,

    #[arg(
        long,
        help = "JSON lines file the records are appended to, one per probed file"
    )]
    output: PathBuf,

    #[arg(
        long,
        help = "Text file listing test files, one per line: records keep the targets, reasons and distances of these tests only"
    )]
    keep_tests: Option<PathBuf>,

    #[command(flatten)]
    prober: ProberArguments,
}

pub fn run(arguments: &ProbeArguments) -> Result<String> {
    let commit = git(
        &arguments.repo,
        &[
            "rev-parse",
            "--verify",
            &format!("{}^{{commit}}", arguments.commit),
        ],
    )?
    .trim()
    .to_owned();
    let files = read_paths(&arguments.files)?;
    let already_recorded_ids = recorded_ids(&arguments.output)?;
    let pending_files: Vec<&String> = files
        .iter()
        .filter(|file| !already_recorded_ids.contains(*file))
        .collect();
    let already_recorded_count = files.len() - pending_files.len();

    if pending_files.is_empty() {
        return Ok(format!(
            "prober-replay: nothing to probe, {already_recorded_count} files already recorded"
        ));
    }

    let kept_tests: Option<HashSet<String>> = arguments
        .keep_tests
        .as_deref()
        .map(read_paths)
        .transpose()?
        .map(|tests| tests.into_iter().collect());
    let replay_settings = arguments.prober.settings()?;
    let worktree = arguments.prober.open_worktree(&arguments.repo)?;
    let mut record_writer = RecordWriter::append_to(&arguments.output)?;
    let mut records = Vec::new();

    for file in pending_files {
        let mut record = probe(&worktree, &replay_settings, &commit, file);

        if let (Some(selection), Some(kept_tests)) = (&mut record.selection, &kept_tests) {
            keep_only(selection, kept_tests);
        }

        record_writer.write(&record)?;
        records.push(record);
    }

    worktree.check_out(&commit)?;

    Ok(summary_line(&records, already_recorded_count))
}

fn probe(worktree: &Worktree, settings: &ReplaySettings, commit: &str, file: &str) -> ReplayRecord {
    let unchanged = Change {
        id: file.to_owned(),
        base: commit.to_owned(),
        head: commit.to_owned(),
    };

    match commit_probe(worktree, commit, file) {
        Ok(head) => run_prober(worktree, settings, &Change { head, ..unchanged }),
        Err(error) => ReplayRecord::failed(&unchanged, format!("{error:#}")),
    }
}

fn commit_probe(worktree: &Worktree, commit: &str, file: &str) -> Result<String> {
    worktree.check_out(commit)?;

    let probed_path = worktree.path().join(file);

    if !probed_path.is_file() {
        bail!("{file} is not a file at {commit}");
    }

    let mut probed_file = OpenOptions::new()
        .append(true)
        .open(&probed_path)
        .with_context(|| format!("cannot open {file}"))?;

    writeln!(probed_file).with_context(|| format!("cannot change {file}"))?;

    let message = format!("probe {file}");
    let mut commit_arguments = PROBE_COMMIT_SETTINGS.to_vec();

    commit_arguments.extend([
        "commit",
        "--quiet",
        "--no-verify",
        "--message",
        &message,
        "--",
        file,
    ]);
    git(worktree.path(), &commit_arguments)?;

    Ok(git(worktree.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_owned())
}

fn keep_only(selection: &mut Value, kept_tests: &HashSet<String>) {
    if let Some(targets) = selection.get_mut("targets").and_then(Value::as_object_mut) {
        for tests in targets.values_mut().filter_map(Value::as_array_mut) {
            tests.retain(|test| test.as_str().is_some_and(|test| kept_tests.contains(test)));
        }
    }

    for field in ["reasons", "distances"] {
        if let Some(by_test) = selection.get_mut(field).and_then(Value::as_object_mut) {
            by_test.retain(|test, _| kept_tests.contains(test));
        }
    }
}

pub fn read_paths(list_path: &Path) -> Result<Vec<String>> {
    let list_text = fs::read_to_string(list_path)
        .with_context(|| format!("cannot read {}", list_path.display()))?;

    Ok(list_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn kept_selection_holds_only_the_listed_tests_and_every_stat() {
        let mut selection = json!({
            "mode": "selected",
            "targets": { "typescript": ["a.test.ts", "b.test.ts"], "python": ["test_c.py"] },
            "reasons": { "a.test.ts": ["src/a.ts (modified)"], "b.test.ts": ["src/a.ts (modified)"] },
            "distances": { "a.test.ts": 1, "b.test.ts": 2 },
            "stats": { "tests_selected": 3 }
        });
        let kept_tests = HashSet::from(["b.test.ts".to_owned()]);

        keep_only(&mut selection, &kept_tests);

        assert_eq!(
            selection,
            json!({
                "mode": "selected",
                "targets": { "typescript": ["b.test.ts"], "python": [] },
                "reasons": { "b.test.ts": ["src/a.ts (modified)"] },
                "distances": { "b.test.ts": 2 },
                "stats": { "tests_selected": 3 }
            })
        );
    }
}
