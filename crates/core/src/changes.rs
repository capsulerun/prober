use std::fmt;

use serde::{Deserialize, Serialize};

use crate::files::RepoPath;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed { from: RepoPath },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRevisions {
    pub base: String,
    pub head: String,
    pub merge_base: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: RepoPath,
    pub kind: ChangeKind,
}

impl FileChange {
    pub fn touched_paths(&self) -> impl Iterator<Item = &RepoPath> {
        let renamed_from = match &self.kind {
            ChangeKind::Renamed { from } => Some(from),
            ChangeKind::Added | ChangeKind::Modified | ChangeKind::Deleted => None,
        };

        std::iter::once(&self.path).chain(renamed_from)
    }
}

impl fmt::Display for FileChange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = &self.path;

        match &self.kind {
            ChangeKind::Added => write!(formatter, "{path} (added)"),
            ChangeKind::Modified => write!(formatter, "{path} (modified)"),
            ChangeKind::Deleted => write!(formatter, "{path} (deleted)"),
            ChangeKind::Renamed { from } => write!(formatter, "{path} (renamed from {from})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    #[test]
    fn renamed_file_touches_its_old_and_new_paths() {
        let renamed = FileChange {
            path: repo_path("src/http/client.ts"),
            kind: ChangeKind::Renamed {
                from: repo_path("src/client.ts"),
            },
        };
        let touched_paths: Vec<&str> = renamed.touched_paths().map(RepoPath::as_str).collect();

        assert_eq!(touched_paths, ["src/http/client.ts", "src/client.ts"]);
    }

    #[test]
    fn change_is_described_by_its_path_and_kind() {
        let modified = FileChange {
            path: repo_path("src/a.ts"),
            kind: ChangeKind::Modified,
        };
        let renamed = FileChange {
            path: repo_path("src/b.ts"),
            kind: ChangeKind::Renamed {
                from: repo_path("src/old_b.ts"),
            },
        };

        assert_eq!(modified.to_string(), "src/a.ts (modified)");
        assert_eq!(renamed.to_string(), "src/b.ts (renamed from src/old_b.ts)");
    }
}
