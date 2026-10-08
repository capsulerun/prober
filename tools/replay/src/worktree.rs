use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use crate::git::git;

pub struct Worktree {
    path: PathBuf,
}

impl Worktree {
    pub fn open(repository: &Path, path: &Path, setup_command: Option<&str>) -> Result<Self> {
        let worktree = Self {
            path: path.to_path_buf(),
        };

        if path.join(".git").exists() {
            return Ok(worktree);
        }

        git(
            repository,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                worktree.path_text()?,
                "HEAD",
            ],
        )?;

        if let Some(setup_command) = setup_command {
            worktree.run_setup(setup_command)?;
        }

        Ok(worktree)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn check_out(&self, commit: &str) -> Result<()> {
        git(
            &self.path,
            &["checkout", "--quiet", "--force", "--detach", commit],
        )?;

        Ok(())
    }

    fn run_setup(&self, setup_command: &str) -> Result<()> {
        let status = Command::new("sh")
            .args(["-c", setup_command])
            .current_dir(&self.path)
            .status()
            .with_context(|| format!("cannot run setup command {setup_command:?}"))?;

        if !status.success() {
            bail!("setup command {setup_command:?} failed with {status}");
        }

        Ok(())
    }

    fn path_text(&self) -> Result<&str> {
        self.path
            .to_str()
            .ok_or_else(|| anyhow!("worktree path {} is not valid UTF-8", self.path.display()))
    }
}
