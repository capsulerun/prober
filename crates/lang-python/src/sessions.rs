//! Test modules whose load-time code writes the process environment: pytest imports every test module of a session in one process, so such a write reaches every test collected with it.

use std::collections::BTreeSet;

use anyhow::Result;
use prober_core::changes::{BaseRevision, ChangeKind, FileChange};
use prober_core::files::RepoPath;

use crate::manifests::{ProjectChange, project_of};

const ENVIRONMENT_WRITES: [&str; 9] = [
    "os.environ[",
    "os.environ.update(",
    "os.environ.setdefault(",
    "os.environ.pop(",
    "os.environ.clear(",
    "os.putenv(",
    "os.unsetenv(",
    "environ[",
    "load_dotenv(",
];

pub(crate) fn session_changes(
    read_head: &dyn Fn(&RepoPath) -> Option<String>,
    changes: &[FileChange],
    base: &dyn BaseRevision,
    project_directories: &BTreeSet<String>,
    is_test: &dyn Fn(&RepoPath) -> bool,
) -> Result<Vec<ProjectChange>> {
    let mut session_changes = Vec::new();

    for change in changes.iter().filter(|change| is_test(&change.path)) {
        let base_path = match &change.kind {
            ChangeKind::Renamed { from } => from,
            _ => &change.path,
        };
        let head_writes = match change.kind {
            ChangeKind::Deleted => Vec::new(),
            _ => environment_writes(&read_head(&change.path).unwrap_or_default()),
        };
        let base_writes = match change.kind {
            ChangeKind::Added => Vec::new(),
            _ => base
                .read_file(base_path)?
                .map(|bytes| environment_writes(&String::from_utf8_lossy(&bytes)))
                .unwrap_or_default(),
        };

        if head_writes == base_writes {
            continue;
        }

        let directory = project_of(&change.path, project_directories).unwrap_or_default();
        let session = if directory.is_empty() {
            "the repository".to_owned()
        } else {
            directory.to_owned()
        };

        session_changes.push(ProjectChange {
            change_path: change.path.clone(),
            directory: directory.to_owned(),
            description: format!(
                "changes the environment its load sets for every test of the session in {session}"
            ),
        });
    }

    Ok(session_changes)
}

fn environment_writes(source: &str) -> Vec<String> {
    source
        .lines()
        .filter(|line| !line.starts_with([' ', '\t', '#']))
        .filter(|line| {
            ENVIRONMENT_WRITES
                .iter()
                .any(|write| line.contains(write) && writes_through(line, write))
        })
        .map(|line| line.trim_end().to_owned())
        .collect()
}

fn writes_through(line: &str, write: &str) -> bool {
    match write.strip_suffix('[') {
        Some(_) => line.split_once(write).is_some_and(|(_, rest)| {
            rest.split_once(']').is_some_and(|(_, after)| {
                after.trim_start().starts_with('=') && !after.trim_start().starts_with("==")
            })
        }),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_environment_writes_are_read_and_reads_are_not() {
        let source = "import os\n\nos.environ[\"OPENAI_API_KEY\"] = \"foo\"\nKEY = os.environ[\"HOME\"]\nif os.environ[\"CI\"] == \"1\":\n    os.environ[\"X\"] = \"1\"\nos.environ.setdefault(\"TZ\", \"UTC\")\n";

        assert_eq!(
            environment_writes(source),
            [
                "os.environ[\"OPENAI_API_KEY\"] = \"foo\"",
                "os.environ.setdefault(\"TZ\", \"UTC\")"
            ]
        );
    }
}
