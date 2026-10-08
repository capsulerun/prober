use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::commands::git;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Change {
    #[serde(default)]
    pub id: String,

    pub base: String,
    pub head: String,
}

pub fn read_changes(changes_path: &Path) -> Result<Vec<Change>> {
    let changes_text = fs::read_to_string(changes_path)
        .with_context(|| format!("cannot read {}", changes_path.display()))?;

    changes_text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(line_index, line)| {
            let change: Change = serde_json::from_str(line).map_err(|error| {
                anyhow!(
                    "{} line {}: {error}",
                    changes_path.display(),
                    line_index + 1
                )
            })?;

            Ok(with_default_id(change))
        })
        .collect()
}

pub fn last_commits(repository: &Path, branch: &str, count: usize) -> Result<Vec<Change>> {
    let count_argument = format!("--max-count={count}");
    let commit_lines = git(
        repository,
        &[
            "rev-list",
            "--first-parent",
            "--parents",
            &count_argument,
            branch,
        ],
    )?;
    let mut changes: Vec<Change> = commit_lines
        .lines()
        .filter_map(|commit_line| {
            let mut commit_ids = commit_line.split_whitespace();
            let commit = commit_ids.next()?;
            let first_parent = commit_ids.next()?;

            Some(Change {
                id: commit.to_owned(),
                base: first_parent.to_owned(),
                head: commit.to_owned(),
            })
        })
        .collect();

    changes.reverse();

    Ok(changes)
}

fn with_default_id(change: Change) -> Change {
    if !change.id.is_empty() {
        return change;
    }

    Change {
        id: change.head.clone(),
        ..change
    }
}
