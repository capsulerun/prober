use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

pub fn git(directory: &Path, arguments: &[&str]) -> Result<String> {
    run("git", Some(directory), arguments)
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
