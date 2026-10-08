use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use prober_core::files::RepoPath;

use crate::commands::git;

pub struct RepositoryFiles {
    files: HashSet<String>,
}

impl RepositoryFiles {
    pub fn read(repository: &Path, revision: &str) -> Result<Self> {
        let file_list = git(
            repository,
            &["ls-tree", "-r", "-z", "--name-only", revision],
        )?;

        Ok(Self {
            files: file_list
                .split('\0')
                .filter(|file| !file.is_empty())
                .map(str::to_owned)
                .collect(),
        })
    }

    pub fn contains(&self, file: &RepoPath) -> bool {
        self.files.contains(file.as_str())
    }

    pub fn locate(&self, reported_file: RepoPath) -> RepoPath {
        if self.contains(&reported_file) {
            return reported_file;
        }

        let path_suffix = format!("/{reported_file}");
        let mut candidates = self
            .files
            .iter()
            .filter(|file| file.ends_with(&path_suffix));

        match (candidates.next(), candidates.next()) {
            (Some(only_candidate), None) => {
                RepoPath::new(only_candidate.clone()).unwrap_or(reported_file)
            }
            _ => reported_file,
        }
    }
}
