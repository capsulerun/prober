use std::collections::{HashMap, HashSet};

use prober_core::files::RepoPath;

pub(crate) struct RepoIndex<'r> {
    files: HashMap<&'r str, &'r RepoPath>,
    directories: HashSet<&'r str>,
    sorted_files: Vec<&'r RepoPath>,
}

impl<'r> RepoIndex<'r> {
    pub fn new(repo_files: &'r [RepoPath]) -> Self {
        let directories = repo_files
            .iter()
            .flat_map(|path| {
                path.as_str()
                    .match_indices('/')
                    .map(|(offset, _)| &path.as_str()[..offset])
            })
            .collect();
        let mut sorted_files: Vec<&RepoPath> = repo_files.iter().collect();

        sorted_files.sort();

        Self {
            files: repo_files
                .iter()
                .map(|path| (path.as_str(), path))
                .collect(),
            directories,
            sorted_files,
        }
    }

    pub fn file(&self, path: &str) -> Option<&'r RepoPath> {
        self.files.get(path).copied()
    }

    pub fn is_directory(&self, path: &str) -> bool {
        path.is_empty() || self.directories.contains(path)
    }

    pub fn directories(&self) -> impl Iterator<Item = &'r str> + '_ {
        self.directories.iter().copied()
    }

    pub fn files(&self) -> &[&'r RepoPath] {
        &self.sorted_files
    }

    pub fn files_under(&self, directory: &str) -> &[&'r RepoPath] {
        if directory.is_empty() {
            return &self.sorted_files;
        }

        let prefix = format!("{directory}/");
        let start = self
            .sorted_files
            .partition_point(|path| path.as_str() < prefix.as_str());
        let length =
            self.sorted_files[start..].partition_point(|path| path.as_str().starts_with(&prefix));

        &self.sorted_files[start..start + length]
    }
}
