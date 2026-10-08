use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::commands::git;

pub struct BaseBranches {
    repository: PathBuf,
    remote_branches: Vec<String>,
}

impl BaseBranches {
    pub fn fetch(repository: &Path, branch_names: &[String]) -> Result<Self> {
        for branch_name in branch_names {
            git(
                repository,
                &[
                    "fetch",
                    "--quiet",
                    "origin",
                    &format!("+{branch_name}:refs/remotes/origin/{branch_name}"),
                ],
            )?;
        }

        Ok(Self {
            repository: repository.to_path_buf(),
            remote_branches: branch_names
                .iter()
                .map(|branch_name| format!("origin/{branch_name}"))
                .collect(),
        })
    }

    pub fn nearest_to(&self, head: &str) -> String {
        let primary_branch = self.remote_branches[0].clone();

        if !self.has_commit(head)
            && git(&self.repository, &["fetch", "--quiet", "origin", head]).is_err()
        {
            return primary_branch;
        }

        self.remote_branches
            .iter()
            .filter_map(|branch| Some((self.commits_since_fork(branch, head)?, branch)))
            .min_by_key(|(commits_since_fork, _)| *commits_since_fork)
            .map_or(primary_branch, |(_, branch)| branch.clone())
    }

    fn has_commit(&self, commit: &str) -> bool {
        git(
            &self.repository,
            &["cat-file", "-e", &format!("{commit}^{{commit}}")],
        )
        .is_ok()
    }

    fn commits_since_fork(&self, branch: &str, head: &str) -> Option<usize> {
        let merge_base = git(&self.repository, &["merge-base", branch, head]).ok()?;
        let commit_count = git(
            &self.repository,
            &[
                "rev-list",
                "--count",
                &format!("{}..{head}", merge_base.trim()),
            ],
        )
        .ok()?;

        commit_count.trim().parse().ok()
    }
}
