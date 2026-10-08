use std::path::Path;

use anyhow::{Context, Result, anyhow};
use gix::bstr::{BStr, ByteSlice};
use gix::diff::Rewrites;
use gix::object::tree::diff::ChangeDetached;
use prober_core::changes::{ChangeKind, FileChange, ResolvedRevisions};
use prober_core::files::RepoPath;

pub struct GitRepository {
    repository: gix::Repository,
}

struct CommitRange {
    base: gix::ObjectId,
    head: gix::ObjectId,
    merge_base: gix::ObjectId,
}

impl GitRepository {
    pub fn discover(directory: &Path) -> Result<Self> {
        let repository = gix::discover(directory)
            .with_context(|| format!("no git repository found at {}", directory.display()))?;

        Ok(Self { repository })
    }

    pub fn work_dir(&self) -> Result<&Path> {
        self.repository
            .workdir()
            .ok_or_else(|| anyhow!("the repository has no working tree"))
    }

    pub fn is_checked_out(&self, revision: &str) -> Result<bool> {
        let revision_commit = self.resolve_commit(revision)?;
        let checked_out_commit = self.resolve_commit("HEAD")?;

        Ok(revision_commit == checked_out_commit)
    }

    pub fn files_at(&self, revision: &str) -> Result<Vec<RepoPath>> {
        let commit = self.resolve_commit(revision)?;
        let tree = self.repository.find_commit(commit)?.tree()?;
        let mut recorder = gix::traverse::tree::Recorder::default();

        tree.traverse()
            .breadthfirst(&mut recorder)
            .with_context(|| format!("cannot list the files of {revision:?}"))?;

        let mut files = recorder
            .records
            .into_iter()
            .filter(|entry| entry.mode.is_blob_or_symlink())
            .map(|entry| repo_path(entry.filepath.as_bstr()))
            .collect::<Result<Vec<_>>>()?;

        files.sort();

        Ok(files)
    }

    pub fn resolve_revisions(&self, base: &str, head: &str) -> Result<ResolvedRevisions> {
        let commit_range = self.commit_range(base, head)?;

        Ok(ResolvedRevisions {
            base: commit_range.base.to_string(),
            head: commit_range.head.to_string(),
            merge_base: commit_range.merge_base.to_string(),
        })
    }

    pub fn changes_between(&self, base: &str, head: &str) -> Result<Vec<FileChange>> {
        let commit_range = self.commit_range(base, head)?;
        let merge_base_tree = self
            .repository
            .find_commit(commit_range.merge_base)?
            .tree()?;
        let head_tree = self.repository.find_commit(commit_range.head)?.tree()?;
        let rename_tracking =
            gix::diff::Options::default().with_rewrites(Some(Rewrites::default()));
        let tree_changes =
            self.repository
                .diff_tree_to_tree(&merge_base_tree, &head_tree, rename_tracking)?;
        let mut file_changes = tree_changes
            .into_iter()
            .filter(|tree_change| !tree_change.entry_mode().is_tree())
            .map(to_file_change)
            .collect::<Result<Vec<_>>>()?;

        file_changes.sort_by(|left, right| left.path.cmp(&right.path));

        Ok(file_changes)
    }

    fn commit_range(&self, base: &str, head: &str) -> Result<CommitRange> {
        let base_commit = self.resolve_commit(base)?;
        let head_commit = self.resolve_commit(head)?;
        let merge_base = self
            .repository
            .merge_base(base_commit, head_commit)
            .map_err(|_| {
                anyhow!(self.with_shallow_clone_hint(format!(
                    "no merge base between {base:?} and {head:?}"
                )))
            })?;

        Ok(CommitRange {
            base: base_commit,
            head: head_commit,
            merge_base: merge_base.detach(),
        })
    }

    fn resolve_commit(&self, revision: &str) -> Result<gix::ObjectId> {
        let object = self
            .repository
            .rev_parse_single(revision)
            .map_err(|_| {
                anyhow!(self.with_shallow_clone_hint(format!("unknown revision {revision:?}")))
            })?
            .object()
            .with_context(|| format!("cannot read revision {revision:?}"))?;
        let commit = object
            .peel_to_commit()
            .with_context(|| format!("revision {revision:?} is not a commit"))?;

        Ok(commit.id)
    }

    fn with_shallow_clone_hint(&self, message: String) -> String {
        if !self.repository.is_shallow() {
            return message;
        }

        format!(
            "{message} (shallow clone: fetch the full history, e.g. actions/checkout with fetch-depth: 0)"
        )
    }
}

fn to_file_change(tree_change: ChangeDetached) -> Result<FileChange> {
    let file_change = match tree_change {
        ChangeDetached::Addition { location, .. }
        | ChangeDetached::Rewrite {
            location,
            copy: true,
            ..
        } => FileChange {
            path: repo_path(location.as_bstr())?,
            kind: ChangeKind::Added,
        },
        ChangeDetached::Deletion { location, .. } => FileChange {
            path: repo_path(location.as_bstr())?,
            kind: ChangeKind::Deleted,
        },
        ChangeDetached::Modification { location, .. } => FileChange {
            path: repo_path(location.as_bstr())?,
            kind: ChangeKind::Modified,
        },
        ChangeDetached::Rewrite {
            source_location,
            location,
            copy: false,
            ..
        } => FileChange {
            path: repo_path(location.as_bstr())?,
            kind: ChangeKind::Renamed {
                from: repo_path(source_location.as_bstr())?,
            },
        },
    };

    Ok(file_change)
}

fn repo_path(location: &BStr) -> Result<RepoPath> {
    let path = location
        .to_str()
        .map_err(|_| anyhow!("changed path {location:?} is not valid UTF-8"))?;

    RepoPath::new(path)
}
