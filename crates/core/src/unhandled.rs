//! Changed files in languages Prober has no frontend for: listed so a CI can route them, never analysed, never widening other languages.

use crate::changes::FileChange;
use crate::files::RepoPath;
use crate::graph::Graph;

const OTHER_LANGUAGE_EXTENSIONS: &[&str] = &[
    "c", "cc", "clj", "cpp", "cs", "cxx", "dart", "erl", "ex", "exs", "fs", "go", "groovy", "h",
    "hpp", "hs", "java", "kt", "kts", "lua", "m", "mm", "php", "pl", "py", "rb", "rs", "scala",
    "swift", "zig",
];

pub fn unhandled_changes(graph: Option<&Graph>, changes: &[FileChange]) -> Vec<RepoPath> {
    let mut unhandled: Vec<RepoPath> = changes
        .iter()
        .filter(|change| is_unhandled(graph, &change.path))
        .map(|change| change.path.clone())
        .collect();

    unhandled.sort();
    unhandled.dedup();

    unhandled
}

fn is_unhandled(graph: Option<&Graph>, path: &RepoPath) -> bool {
    let is_other_language = path
        .as_str()
        .rsplit('/')
        .next()
        .and_then(|file_name| file_name.rsplit_once('.'))
        .is_some_and(|(stem, extension)| {
            !stem.is_empty() && OTHER_LANGUAGE_EXTENSIONS.contains(&extension)
        });

    if !is_other_language {
        return false;
    }

    let Some(graph) = graph else {
        return true;
    };

    !graph.is_module(path)
        && graph
            .file_id(path)
            .is_none_or(|file| !graph.is_test(file) && graph.dependents(file).is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::ChangeKind;
    use crate::graph::{Edge, EdgeKind, GraphBuilder};

    fn modified(path: &str) -> FileChange {
        FileChange {
            path: RepoPath::new(path).unwrap(),
            kind: ChangeKind::Modified,
        }
    }

    #[test]
    fn other_language_files_are_unhandled_unless_something_links_them() {
        let mut builder = GraphBuilder::default();
        let contract_test = builder.add_test_file(
            RepoPath::new("test/contract.test.ts").unwrap(),
            "typescript",
        );
        let contract = builder.add_file(RepoPath::new("tools/contract.py").unwrap());

        builder.add_language("typescript", |path| path.as_str().ends_with(".ts"));
        builder.add_edge(Edge {
            dependent: contract_test,
            dependency: contract,
            kind: EdgeKind::Reads,
        });

        let graph = builder.build();
        let changes = [
            modified("apps/ios/ChatView.swift"),
            modified("tools/contract.py"),
            modified("docs/guide.md"),
            modified("src/a.ts"),
        ];

        assert_eq!(
            unhandled_changes(Some(&graph), &changes),
            [RepoPath::new("apps/ios/ChatView.swift").unwrap()]
        );
        assert_eq!(unhandled_changes(None, &changes).len(), 2);
    }
}
