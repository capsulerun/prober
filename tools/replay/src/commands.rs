use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

pub fn git(directory: &Path, arguments: &[&str]) -> Result<String> {
    run("git", Some(directory), arguments)
}

pub fn has_commit(repository: &Path, commit: &str) -> bool {
    git(
        repository,
        &["cat-file", "-e", &format!("{commit}^{{commit}}")],
    )
    .is_ok()
}

pub fn fetch_missing_commits<'c>(repository: &Path, commits: impl IntoIterator<Item = &'c str>) {
    let mut missing_commits: Vec<&str> = commits
        .into_iter()
        .filter(|commit| !has_commit(repository, commit))
        .collect();

    missing_commits.sort_unstable();
    missing_commits.dedup();

    if missing_commits.is_empty() {
        return;
    }

    let mut fetch_arguments = vec!["fetch", "--quiet", "origin"];

    fetch_arguments.extend(&missing_commits);

    if git(repository, &fetch_arguments).is_ok() {
        return;
    }

    for commit in missing_commits {
        let _ = git(repository, &["fetch", "--quiet", "origin", commit]);
    }
}

pub fn gh(arguments: &[&str]) -> Result<String> {
    run("gh", None, arguments)
}

fn run(program: &str, directory: Option<&Path>, arguments: &[&str]) -> Result<String> {
    let mut command = Command::new(program);

    command.args(arguments);

    if let Some(directory) = directory {
        command.current_dir(directory);
    }

    let output = command
        .output()
        .with_context(|| format!("cannot run {program}"))?;

    if !output.status.success() {
        bail!(
            "{program} {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
