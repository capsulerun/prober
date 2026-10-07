//! Repo-relative file paths and the interned ids the graph refers to them by.

use std::collections::HashMap;
use std::fmt;

use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoPath(String);

impl RepoPath {
    pub fn new(path: impl Into<String>) -> Result<Self> {
        let path = path.into();
        let has_invalid_component = path
            .split('/')
            .any(|component| matches!(component, "" | "." | ".."));

        if has_invalid_component {
            bail!(
                "invalid repo path {path:?}: expected a relative path without empty, `.` or `..` components"
            );
        }

        Ok(Self(path))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(u32);

impl FileId {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Debug, Default)]
pub(crate) struct FileTable {
    paths: Vec<RepoPath>,
    ids_by_path: HashMap<RepoPath, FileId>,
}

impl FileTable {
    pub(crate) fn intern(&mut self, path: RepoPath) -> FileId {
        if let Some(&existing_id) = self.ids_by_path.get(&path) {
            return existing_id;
        }

        let new_id = FileId(u32::try_from(self.paths.len()).expect("file count exceeds u32::MAX"));

        self.paths.push(path.clone());
        self.ids_by_path.insert(path, new_id);

        new_id
    }

    pub(crate) fn id(&self, path: &RepoPath) -> Option<FileId> {
        self.ids_by_path.get(path).copied()
    }

    pub(crate) fn path(&self, id: FileId) -> &RepoPath {
        &self.paths[id.index()]
    }

    pub(crate) fn len(&self) -> usize {
        self.paths.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_path_accepts_relative_paths() {
        let repo_path = RepoPath::new("src/plugins/loader.ts").unwrap();

        assert_eq!(repo_path.as_str(), "src/plugins/loader.ts");
    }

    #[test]
    fn repo_path_rejects_absolute_empty_and_dot_components() {
        let invalid_paths = [
            "",
            "/src/a.ts",
            "src//a.ts",
            "src/",
            "./src/a.ts",
            "src/../a.ts",
        ];

        for invalid_path in invalid_paths {
            assert!(
                RepoPath::new(invalid_path).is_err(),
                "{invalid_path:?} should be rejected"
            );
        }
    }

    #[test]
    fn interning_the_same_path_twice_returns_the_same_id() {
        let mut file_table = FileTable::default();
        let first_id = file_table.intern(RepoPath::new("src/a.ts").unwrap());
        let second_id = file_table.intern(RepoPath::new("src/a.ts").unwrap());
        let other_id = file_table.intern(RepoPath::new("src/b.ts").unwrap());

        assert_eq!(first_id, second_id);
        assert_ne!(first_id, other_id);
        assert_eq!(file_table.len(), 2);
        assert_eq!(file_table.path(other_id).as_str(), "src/b.ts");
    }
}
