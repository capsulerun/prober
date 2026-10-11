//! The interface every language frontend implements, so one run selects the tests of every language of the repository.

use anyhow::Result;

use crate::changes::{BaseRevision, FileChange};
use crate::files::RepoPath;
use crate::graph::GraphBuilder;
use crate::report::ModeReason;

pub struct ChangeSet<'c> {
    pub repo_files: &'c [RepoPath],
    pub changes: &'c [FileChange],
    pub base: &'c dyn BaseRevision,
}

struct NoBase;

impl BaseRevision for NoBase {
    fn read_file(&self, _: &RepoPath) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

impl<'c> ChangeSet<'c> {
    pub fn unchanged(repo_files: &'c [RepoPath]) -> Self {
        Self {
            repo_files,
            changes: &[],
            base: &NoBase,
        }
    }
}

pub trait Frontend: Send + Sync {
    fn language(&self) -> &'static str;

    fn always_all_patterns(&self) -> Vec<&'static str>;

    fn test_files(&self, repo_files: &[RepoPath]) -> Vec<RepoPath>;

    fn target_name(&self, _test: &RepoPath) -> Option<(&'static str, String)> {
        None
    }

    fn diff_manifests(&mut self, change_set: &ChangeSet) -> Result<Vec<ModeReason>>;

    fn add_links(
        &self,
        change_set: &ChangeSet,
        graph: &mut GraphBuilder,
    ) -> Result<Vec<ModeReason>>;
}
