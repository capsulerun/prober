//! Files changed between the base and head revisions.

use crate::files::RepoPath;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed { from: RepoPath },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: RepoPath,
    pub kind: ChangeKind,
}
