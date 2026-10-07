//! Traversal from changed files to the test files that depend on them, ordered by distance with a reason each.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};

use crate::changes::FileChange;
use crate::files::FileId;
use crate::graph::{EdgeKind, Graph};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hop {
    pub dependent: FileId,
    pub edge: EdgeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    pub change: FileChange,
    pub hops: Vec<Hop>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedTest {
    pub file: FileId,
    pub reason: Reason,
}

impl SelectedTest {
    pub fn distance(&self) -> usize {
        self.reason.hops.len()
    }
}

#[derive(Clone, Copy)]
struct ReachedVia {
    dependency: FileId,
    edge: EdgeKind,
}

#[derive(Clone, Copy)]
struct Discovery<'a> {
    change: &'a FileChange,
    reached_via: Option<ReachedVia>,
}

pub fn select_tests(graph: &Graph, changes: &[FileChange]) -> Vec<SelectedTest> {
    let discoveries = discover_dependents(graph, changes);
    let mut selected_tests: Vec<SelectedTest> = discoveries
        .keys()
        .filter(|&&file| graph.is_test(file))
        .map(|&test_file| SelectedTest {
            file: test_file,
            reason: trace_reason(test_file, &discoveries),
        })
        .collect();

    selected_tests.sort_by(|left, right| {
        let left_path = graph.path(left.file);
        let right_path = graph.path(right.file);

        left.distance()
            .cmp(&right.distance())
            .then(left_path.cmp(right_path))
    });

    selected_tests
}

fn discover_dependents<'a>(
    graph: &Graph,
    changes: &'a [FileChange],
) -> HashMap<FileId, Discovery<'a>> {
    let mut discoveries = HashMap::new();
    let mut files_to_visit = VecDeque::new();
    let mut changes_by_path: Vec<&FileChange> = changes.iter().collect();

    changes_by_path.sort_by(|left, right| left.path.cmp(&right.path));

    for change in changes_by_path {
        let Some(changed_file) = graph.file_id(&change.path) else {
            continue;
        };

        if let Entry::Vacant(entry) = discoveries.entry(changed_file) {
            entry.insert(Discovery {
                change,
                reached_via: None,
            });
            files_to_visit.push_back(changed_file);
        }
    }

    while let Some(file) = files_to_visit.pop_front() {
        let change = discoveries[&file].change;
        let test_selecting_edges = graph
            .dependents(file)
            .iter()
            .filter(|edge| edge.kind.selects_tests());

        for edge in test_selecting_edges {
            if let Entry::Vacant(entry) = discoveries.entry(edge.dependent) {
                entry.insert(Discovery {
                    change,
                    reached_via: Some(ReachedVia {
                        dependency: file,
                        edge: edge.kind,
                    }),
                });
                files_to_visit.push_back(edge.dependent);
            }
        }
    }

    discoveries
}

fn trace_reason(test_file: FileId, discoveries: &HashMap<FileId, Discovery>) -> Reason {
    let mut hops = Vec::new();
    let mut file = test_file;

    while let Some(reached_via) = discoveries[&file].reached_via {
        hops.push(Hop {
            dependent: file,
            edge: reached_via.edge,
        });
        file = reached_via.dependency;
    }

    hops.reverse();

    Reason {
        change: discoveries[&test_file].change.clone(),
        hops,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::ChangeKind;
    use crate::files::RepoPath;
    use crate::graph::EdgeKind::{Import, TypeOnly};
    use crate::graph::{Edge, GraphBuilder};

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn build_graph(test_files: &[&str], edges: &[(&str, EdgeKind, &str)]) -> Graph {
        let mut builder = GraphBuilder::default();

        for test_file in test_files {
            builder.add_test_file(repo_path(test_file), "typescript");
        }

        for &(dependent_path, kind, dependency_path) in edges {
            let dependent = builder.add_file(repo_path(dependent_path));
            let dependency = builder.add_file(repo_path(dependency_path));

            builder.add_edge(Edge {
                dependent,
                dependency,
                kind,
            });
        }

        builder.build()
    }

    fn modified(path: &str) -> FileChange {
        FileChange {
            path: repo_path(path),
            kind: ChangeKind::Modified,
        }
    }

    fn selected_with_distances<'g>(
        graph: &'g Graph,
        changes: &[FileChange],
    ) -> Vec<(&'g str, usize)> {
        select_tests(graph, changes)
            .iter()
            .map(|selected_test| {
                (
                    graph.path(selected_test.file).as_str(),
                    selected_test.distance(),
                )
            })
            .collect()
    }

    fn reason_files<'g>(graph: &'g Graph, selected_test: &SelectedTest) -> Vec<&'g str> {
        let changed_file = graph.file_id(&selected_test.reason.change.path).unwrap();
        let dependents = selected_test.reason.hops.iter().map(|hop| hop.dependent);

        std::iter::once(changed_file)
            .chain(dependents)
            .map(|file| graph.path(file).as_str())
            .collect()
    }

    #[test]
    fn changed_file_selects_the_tests_importing_it() {
        let graph = build_graph(
            &["src/a.test.ts", "src/unrelated.test.ts"],
            &[
                ("src/a.test.ts", Import, "src/a.ts"),
                ("src/unrelated.test.ts", Import, "src/unrelated.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/a.ts")]),
            [("src/a.test.ts", 1)]
        );
    }

    #[test]
    fn transitive_dependents_are_selected_with_their_distance() {
        let graph = build_graph(
            &["src/api.test.ts"],
            &[
                ("src/store.ts", Import, "src/db.ts"),
                ("src/api.ts", Import, "src/store.ts"),
                ("src/api.test.ts", Import, "src/api.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/db.ts")]),
            [("src/api.test.ts", 3)]
        );
    }

    #[test]
    fn reason_lists_every_file_from_the_change_to_the_test() {
        let graph = build_graph(
            &["src/api.test.ts"],
            &[
                ("src/store.ts", Import, "src/db.ts"),
                ("src/api.ts", Import, "src/store.ts"),
                ("src/api.test.ts", Import, "src/api.ts"),
            ],
        );
        let selected_tests = select_tests(&graph, &[modified("src/db.ts")]);

        assert_eq!(selected_tests[0].reason.change, modified("src/db.ts"));
        assert_eq!(
            reason_files(&graph, &selected_tests[0]),
            ["src/db.ts", "src/store.ts", "src/api.ts", "src/api.test.ts"]
        );
    }

    #[test]
    fn distance_is_the_shortest_path_to_a_change() {
        let graph = build_graph(
            &["src/api.test.ts"],
            &[
                ("src/store.ts", Import, "src/db.ts"),
                ("src/api.ts", Import, "src/store.ts"),
                ("src/api.test.ts", Import, "src/api.ts"),
                ("src/api.test.ts", Import, "src/db.ts"),
            ],
        );
        let selected_tests = select_tests(&graph, &[modified("src/db.ts")]);

        assert_eq!(
            reason_files(&graph, &selected_tests[0]),
            ["src/db.ts", "src/api.test.ts"]
        );
    }

    #[test]
    fn type_only_import_never_selects_a_test() {
        let graph = build_graph(
            &["src/a.test.ts", "src/b.test.ts"],
            &[
                ("src/a.test.ts", TypeOnly, "src/types.ts"),
                ("src/b.ts", TypeOnly, "src/types.ts"),
                ("src/b.test.ts", Import, "src/b.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/types.ts")]),
            []
        );
    }

    #[test]
    fn test_with_both_type_only_and_runtime_paths_is_selected_through_the_runtime_path() {
        let graph = build_graph(
            &["src/a.test.ts"],
            &[
                ("src/a.test.ts", TypeOnly, "src/model.ts"),
                ("src/a.test.ts", Import, "src/a.ts"),
                ("src/a.ts", Import, "src/model.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/model.ts")]),
            [("src/a.test.ts", 2)]
        );
    }

    #[test]
    fn changed_test_file_is_always_selected_at_distance_zero() {
        let graph = build_graph(&["src/standalone.test.ts"], &[]);
        let added_test = FileChange {
            path: repo_path("src/standalone.test.ts"),
            kind: ChangeKind::Added,
        };

        assert_eq!(
            selected_with_distances(&graph, &[added_test]),
            [("src/standalone.test.ts", 0)]
        );
    }

    #[test]
    fn renamed_file_is_traced_from_its_new_path() {
        let graph = build_graph(
            &["src/client.test.ts"],
            &[("src/client.test.ts", Import, "src/http/client.ts")],
        );
        let renamed = FileChange {
            path: repo_path("src/http/client.ts"),
            kind: ChangeKind::Renamed {
                from: repo_path("src/client.ts"),
            },
        };
        let selected_tests = select_tests(&graph, std::slice::from_ref(&renamed));

        assert_eq!(selected_tests.len(), 1);
        assert_eq!(selected_tests[0].reason.change, renamed);
    }

    #[test]
    fn changes_outside_the_graph_select_nothing() {
        let graph = build_graph(&["src/a.test.ts"], &[("src/a.test.ts", Import, "src/a.ts")]);
        let deleted = FileChange {
            path: repo_path("src/removed.ts"),
            kind: ChangeKind::Deleted,
        };

        assert_eq!(
            selected_with_distances(&graph, &[modified("README.md"), deleted]),
            []
        );
    }

    #[test]
    fn tests_are_ordered_by_distance_then_path() {
        let graph = build_graph(
            &[
                "src/z.test.ts",
                "src/b.test.ts",
                "src/a.test.ts",
                "src/far.test.ts",
            ],
            &[
                ("src/z.test.ts", Import, "src/core.ts"),
                ("src/b.test.ts", Import, "src/core.ts"),
                ("src/feature.ts", Import, "src/core.ts"),
                ("src/a.test.ts", Import, "src/feature.ts"),
                ("src/page.ts", Import, "src/feature.ts"),
                ("src/far.test.ts", Import, "src/page.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/core.ts")]),
            [
                ("src/b.test.ts", 1),
                ("src/z.test.ts", 1),
                ("src/a.test.ts", 2),
                ("src/far.test.ts", 3),
            ]
        );
    }

    #[test]
    fn import_cycles_are_traversed_once() {
        let graph = build_graph(
            &["src/a.test.ts"],
            &[
                ("src/a.ts", Import, "src/b.ts"),
                ("src/b.ts", Import, "src/a.ts"),
                ("src/a.test.ts", Import, "src/a.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/b.ts")]),
            [("src/a.test.ts", 2)]
        );
    }

    #[test]
    fn traversal_continues_through_test_files() {
        let graph = build_graph(
            &["src/base.test.ts", "src/extended.test.ts"],
            &[
                ("src/base.test.ts", Import, "src/a.ts"),
                ("src/extended.test.ts", Import, "src/base.test.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/a.ts")]),
            [("src/base.test.ts", 1), ("src/extended.test.ts", 2)]
        );
    }

    #[test]
    fn reason_ties_are_broken_by_change_path_whatever_the_change_order() {
        let graph = build_graph(
            &["src/ab.test.ts"],
            &[
                ("src/ab.test.ts", Import, "src/a.ts"),
                ("src/ab.test.ts", Import, "src/b.ts"),
            ],
        );
        let selected_tests = select_tests(&graph, &[modified("src/b.ts"), modified("src/a.ts")]);

        assert_eq!(selected_tests[0].reason.change, modified("src/a.ts"));
    }
}
