use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::changes::FileChange;
use crate::files::FileId;
use crate::graph::{ContentChange, EdgeKind, Graph};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Widening {
    Scoped,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    Edge { kind: EdgeKind, dependency: FileId },
    ChangedPart(usize),
    UnresolvedSite(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hop {
    pub dependent: FileId,
    pub link: Link,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    pub change: FileChange,
    pub hops: Vec<Hop>,
}

impl Reason {
    pub fn widening_site(&self) -> Option<usize> {
        match self.hops.first()?.link {
            Link::UnresolvedSite(site_index) => Some(site_index),
            Link::Edge { .. } | Link::ChangedPart(_) => None,
        }
    }
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
struct Discovery<'a> {
    change: &'a FileChange,
    reached_via: Option<Link>,
    content: ContentChange,
}

pub fn select_tests(
    graph: &Graph,
    changes: &[FileChange],
    widening: Widening,
) -> Vec<SelectedTest> {
    let (asset_changes, code_changes): (Vec<FileChange>, Vec<FileChange>) = changes
        .iter()
        .cloned()
        .partition(|change| is_asset_change(graph, change));
    let code_discoveries = discover_dependents(graph, &code_changes, widening);
    let asset_discoveries = discover_dependents(graph, &asset_changes, Widening::Off);
    let mut selected_tests = tests_reached(graph, &code_discoveries, |_| true);
    let selected_files: HashSet<FileId> = selected_tests
        .iter()
        .map(|selected_test| selected_test.file)
        .collect();
    let asset_tests = tests_reached(graph, &asset_discoveries, |test| {
        !graph.is_asset_blind(test) && !selected_files.contains(&test)
    });

    selected_tests.extend(asset_tests);
    selected_tests.sort_by(|left, right| {
        let left_path = graph.path(left.file);
        let right_path = graph.path(right.file);

        left.distance()
            .cmp(&right.distance())
            .then(left_path.cmp(right_path))
    });

    selected_tests
}

fn is_asset_change(graph: &Graph, change: &FileChange) -> bool {
    graph
        .file_id(&change.path)
        .is_some_and(|changed_file| graph.is_asset(changed_file))
}

fn tests_reached(
    graph: &Graph,
    discoveries: &HashMap<FileId, Discovery>,
    keeps: impl Fn(FileId) -> bool,
) -> Vec<SelectedTest> {
    discoveries
        .keys()
        .filter(|&&file| graph.is_test(file) && keeps(file))
        .filter_map(|&test_file| {
            let reason = reason_past_mocks(graph, discoveries, test_file)?;

            Some(SelectedTest {
                file: test_file,
                reason,
            })
        })
        .collect()
}

fn reason_past_mocks(
    graph: &Graph,
    discoveries: &HashMap<FileId, Discovery>,
    test_file: FileId,
) -> Option<Reason> {
    let mocked_modules = graph.mocked_modules(test_file);
    let is_mocked = |file: FileId| {
        mocked_modules.binary_search(&file).is_ok() && !graph.has_changed_contract(file)
    };

    if trace_avoids(test_file, discoveries, &is_mocked) {
        return Some(trace_reason(test_file, discoveries));
    }

    let mut dependents_on_path: HashMap<FileId, (FileId, EdgeKind)> = HashMap::new();
    let mut files_to_visit = VecDeque::from([test_file]);

    while let Some(file) = files_to_visit.pop_front() {
        let test_selecting_edges = graph
            .dependencies(file)
            .iter()
            .filter(|edge| edge.kind.selects_tests());

        for edge in test_selecting_edges {
            let dependency = edge.dependency;
            let carries_the_change = discoveries
                .get(&dependency)
                .is_some_and(|discovery| edge.kind.follows(discovery.content));
            let is_unexplored = dependency != test_file
                && !dependents_on_path.contains_key(&dependency)
                && carries_the_change
                && !is_mocked(dependency);

            if !is_unexplored {
                continue;
            }

            dependents_on_path.insert(dependency, (file, edge.kind));

            if trace_avoids(dependency, discoveries, &is_mocked) {
                return Some(reason_through(dependency, &dependents_on_path, discoveries));
            }

            files_to_visit.push_back(dependency);
        }
    }

    None
}

fn trace_avoids(
    file: FileId,
    discoveries: &HashMap<FileId, Discovery>,
    is_mocked: &impl Fn(FileId) -> bool,
) -> bool {
    let mut file = file;

    loop {
        if is_mocked(file) {
            return false;
        }

        match discoveries[&file].reached_via {
            Some(Link::Edge { dependency, .. }) => file = dependency,
            Some(Link::ChangedPart(_) | Link::UnresolvedSite(_)) | None => return true,
        }
    }
}

fn reason_through(
    file: FileId,
    dependents_on_path: &HashMap<FileId, (FileId, EdgeKind)>,
    discoveries: &HashMap<FileId, Discovery>,
) -> Reason {
    let mut reason = trace_reason(file, discoveries);
    let mut dependency = file;

    while let Some(&(dependent, kind)) = dependents_on_path.get(&dependency) {
        reason.hops.push(Hop {
            dependent,
            link: Link::Edge { kind, dependency },
        });
        dependency = dependent;
    }

    reason
}

fn discover_dependents<'a>(
    graph: &Graph,
    changes: &'a [FileChange],
    widening: Widening,
) -> HashMap<FileId, Discovery<'a>> {
    let mut discoveries = HashMap::new();
    let mut files_to_visit = VecDeque::new();
    let mut changes_by_path: Vec<&FileChange> = changes.iter().collect();

    changes_by_path.sort_by(|left, right| left.path.cmp(&right.path));

    for &change in &changes_by_path {
        let Some(changed_file) = graph.file_id(&change.path) else {
            continue;
        };

        discover(
            &mut discoveries,
            &mut files_to_visit,
            changed_file,
            Discovery {
                change,
                reached_via: None,
                content: ContentChange::Direct,
            },
        );
    }

    for (part_index, changed_part) in graph.changed_parts().iter().enumerate() {
        let Some(&change) = changes_by_path
            .iter()
            .find(|change| change.path == changed_part.change_path)
        else {
            continue;
        };

        discover(
            &mut discoveries,
            &mut files_to_visit,
            changed_part.node,
            Discovery {
                change,
                reached_via: Some(Link::ChangedPart(part_index)),
                content: ContentChange::None,
            },
        );
    }

    visit_dependents(graph, &mut discoveries, &mut files_to_visit);

    if widening == Widening::Off {
        return discoveries;
    }

    let widening_sites = graph
        .unresolved_sites()
        .iter()
        .enumerate()
        .filter(|(_, unresolved_site)| unresolved_site.cause.widens());

    for (site_index, unresolved_site) in widening_sites {
        let loadable_change = changes_by_path.iter().find(|change| {
            change
                .touched_paths()
                .any(|path| graph.can_load(unresolved_site.language, path))
        });

        if let Some(&change) = loadable_change {
            discover(
                &mut discoveries,
                &mut files_to_visit,
                unresolved_site.file,
                Discovery {
                    change,
                    reached_via: Some(Link::UnresolvedSite(site_index)),
                    content: ContentChange::None,
                },
            );
        }
    }

    visit_dependents(graph, &mut discoveries, &mut files_to_visit);

    discoveries
}

fn discover<'a>(
    discoveries: &mut HashMap<FileId, Discovery<'a>>,
    files_to_visit: &mut VecDeque<FileId>,
    file: FileId,
    discovery: Discovery<'a>,
) {
    match discoveries.entry(file) {
        Entry::Vacant(entry) => {
            entry.insert(discovery);
            files_to_visit.push_back(file);
        }
        Entry::Occupied(mut entry) if discovery.content > entry.get().content => {
            entry.insert(discovery);
            files_to_visit.push_back(file);
        }
        Entry::Occupied(_) => {}
    }
}

fn visit_dependents<'a>(
    graph: &Graph,
    discoveries: &mut HashMap<FileId, Discovery<'a>>,
    files_to_visit: &mut VecDeque<FileId>,
) {
    while let Some(file) = files_to_visit.pop_front() {
        let discovery = discoveries[&file];
        let test_selecting_edges = graph
            .dependents(file)
            .iter()
            .filter(|edge| edge.kind.selects_tests() && edge.kind.follows(discovery.content));

        for edge in test_selecting_edges {
            let reached_via = Link::Edge {
                kind: edge.kind,
                dependency: file,
            };

            discover(
                discoveries,
                files_to_visit,
                edge.dependent,
                Discovery {
                    change: discovery.change,
                    reached_via: Some(reached_via),
                    content: edge.kind.dependent_content(),
                },
            );
        }
    }
}

fn trace_reason(reached_file: FileId, discoveries: &HashMap<FileId, Discovery>) -> Reason {
    let mut hops = Vec::new();
    let mut file = reached_file;

    while let Some(link) = discoveries[&file].reached_via {
        hops.push(Hop {
            dependent: file,
            link,
        });

        match link {
            Link::Edge { dependency, .. } => file = dependency,
            Link::ChangedPart(_) | Link::UnresolvedSite(_) => break,
        }
    }

    hops.reverse();

    Reason {
        change: discoveries[&reached_file].change.clone(),
        hops,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::ChangeKind;
    use crate::files::RepoPath;
    use crate::graph::EdgeKind::{Asset, Family, Import, Lists, Reads, TypeOnly};
    use crate::graph::{Edge, GraphBuilder, UnresolvedCause, UnresolvedSite};

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

    fn graph_with_unresolved_sites(
        test_files: &[&str],
        edges: &[(&str, EdgeKind, &str)],
        unresolved_sites: &[(&str, u32)],
    ) -> Graph {
        let mut builder = GraphBuilder::default();

        builder.add_language("typescript", |path| {
            path.as_str().ends_with(".ts") && !path.as_str().ends_with(".test.ts")
        });

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

        for &(site_path, line) in unresolved_sites {
            let file = builder.add_file(repo_path(site_path));

            builder.add_unresolved_site(UnresolvedSite {
                file,
                line,
                expression: "request.moduleUrl".to_owned(),
                cause: UnresolvedCause::Parameter,
                language: "typescript",
            });
        }

        builder.build()
    }

    fn widening_sites(graph: &Graph, changes: &[FileChange]) -> Vec<(String, Option<usize>)> {
        select_tests(graph, changes, Widening::Scoped)
            .iter()
            .map(|selected_test| {
                (
                    graph.path(selected_test.file).as_str().to_owned(),
                    selected_test.reason.widening_site(),
                )
            })
            .collect()
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
        select_tests(graph, changes, Widening::Scoped)
            .iter()
            .map(|selected_test| {
                (
                    graph.path(selected_test.file).as_str(),
                    selected_test.distance(),
                )
            })
            .collect()
    }

    fn reason_files<'g>(graph: &'g Graph, selected_test: &'g SelectedTest) -> Vec<&'g str> {
        let dependents = selected_test
            .reason
            .hops
            .iter()
            .map(|hop| graph.path(hop.dependent).as_str());

        std::iter::once(selected_test.reason.change.path.as_str())
            .chain(dependents)
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
        let selected_tests = select_tests(&graph, &[modified("src/db.ts")], Widening::Scoped);

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
        let selected_tests = select_tests(&graph, &[modified("src/db.ts")], Widening::Scoped);

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
        let selected_tests = select_tests(&graph, std::slice::from_ref(&renamed), Widening::Scoped);

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
    fn module_change_widens_to_the_dependents_of_each_unresolved_site() {
        let graph = graph_with_unresolved_sites(
            &["src/worker.test.ts", "src/pool.test.ts", "src/a.test.ts"],
            &[
                ("src/worker.test.ts", Import, "src/worker.ts"),
                ("src/pool.ts", Import, "src/worker.ts"),
                ("src/pool.test.ts", Import, "src/pool.ts"),
                ("src/a.test.ts", Import, "src/a.ts"),
            ],
            &[("src/worker.ts", 313)],
        );
        let selected_tests = select_tests(&graph, &[modified("src/a.ts")], Widening::Scoped);

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/a.ts")]),
            [
                ("src/a.test.ts", 1),
                ("src/worker.test.ts", 2),
                ("src/pool.test.ts", 3),
            ]
        );
        assert_eq!(
            selected_tests[1].reason.hops[0],
            Hop {
                dependent: graph.file_id(&repo_path("src/worker.ts")).unwrap(),
                link: Link::UnresolvedSite(0),
            }
        );
        assert_eq!(
            reason_files(&graph, &selected_tests[2]),
            [
                "src/a.ts",
                "src/worker.ts",
                "src/pool.ts",
                "src/pool.test.ts"
            ]
        );
    }

    #[test]
    fn changes_the_language_cannot_load_never_widen() {
        let graph = graph_with_unresolved_sites(
            &["src/worker.test.ts", "src/a.test.ts"],
            &[("src/worker.test.ts", Import, "src/worker.ts")],
            &[("src/worker.ts", 313)],
        );
        let docs_and_tests = [modified("docs/guide.md"), modified("src/a.test.ts")];

        assert_eq!(
            widening_sites(&graph, &docs_and_tests),
            [("src/a.test.ts".to_owned(), None)]
        );
    }

    #[test]
    fn deleted_and_renamed_modules_widen_too() {
        let graph = graph_with_unresolved_sites(
            &["src/worker.test.ts"],
            &[("src/worker.test.ts", Import, "src/worker.ts")],
            &[("src/worker.ts", 313)],
        );
        let deleted = FileChange {
            path: repo_path("src/removed.ts"),
            kind: ChangeKind::Deleted,
        };
        let renamed_to_markdown = FileChange {
            path: repo_path("docs/notes.md"),
            kind: ChangeKind::Renamed {
                from: repo_path("src/notes.ts"),
            },
        };

        assert_eq!(
            widening_sites(&graph, &[deleted]),
            [("src/worker.test.ts".to_owned(), Some(0))]
        );
        assert_eq!(
            widening_sites(&graph, &[renamed_to_markdown]),
            [("src/worker.test.ts".to_owned(), Some(0))]
        );
    }

    #[test]
    fn widening_off_selects_only_through_the_graph() {
        let graph = graph_with_unresolved_sites(
            &["src/worker.test.ts", "src/a.test.ts"],
            &[
                ("src/worker.test.ts", Import, "src/worker.ts"),
                ("src/a.test.ts", Import, "src/a.ts"),
            ],
            &[("src/worker.ts", 313)],
        );
        let selected_tests = select_tests(&graph, &[modified("src/a.ts")], Widening::Off);

        assert_eq!(selected_tests.len(), 1);
        assert_eq!(graph.path(selected_tests[0].file).as_str(), "src/a.test.ts");
    }

    #[test]
    fn tests_already_selected_keep_their_ordinary_reason() {
        let graph = graph_with_unresolved_sites(
            &["src/worker.test.ts"],
            &[
                ("src/worker.ts", Import, "src/a.ts"),
                ("src/worker.test.ts", Import, "src/worker.ts"),
            ],
            &[("src/worker.ts", 313)],
        );

        assert_eq!(
            widening_sites(&graph, &[modified("src/a.ts")]),
            [("src/worker.test.ts".to_owned(), None)]
        );
    }

    #[test]
    fn unresolved_site_in_a_test_file_widens_to_that_test() {
        let graph = graph_with_unresolved_sites(
            &["src/fresh.test.ts", "src/a.test.ts"],
            &[("src/a.test.ts", Import, "src/a.ts")],
            &[("src/fresh.test.ts", 12), ("src/fresh.test.ts", 30)],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/a.ts")]),
            [("src/a.test.ts", 1), ("src/fresh.test.ts", 1)]
        );
        assert_eq!(
            widening_sites(&graph, &[modified("src/a.ts")])[1],
            ("src/fresh.test.ts".to_owned(), Some(0))
        );
    }

    #[test]
    fn changed_part_selects_through_its_node_instead_of_the_whole_file() {
        let mut builder = GraphBuilder::default();
        let lodash = builder.add_file(repo_path("node_modules/lodash"));
        let a = builder.add_file(repo_path("src/a.ts"));
        let a_test = builder.add_test_file(repo_path("src/a.test.ts"), "typescript");
        let b = builder.add_file(repo_path("src/b.ts"));
        let b_test = builder.add_test_file(repo_path("src/b.test.ts"), "typescript");
        let import_edges = [(a, lodash), (a_test, a), (b_test, b)];

        for (dependent, dependency) in import_edges {
            builder.add_edge(Edge {
                dependent,
                dependency,
                kind: Import,
            });
        }

        builder.add_changed_part(
            repo_path("pnpm-lock.yaml"),
            repo_path("node_modules/lodash"),
            "package lodash changed (4.17.20 -> 4.17.21)".to_owned(),
        );

        let graph = builder.build();
        let selected_tests = select_tests(&graph, &[modified("pnpm-lock.yaml")], Widening::Scoped);

        assert_eq!(selected_tests.len(), 1);
        assert_eq!(
            reason_files(&graph, &selected_tests[0]),
            [
                "pnpm-lock.yaml",
                "node_modules/lodash",
                "src/a.ts",
                "src/a.test.ts"
            ]
        );
        assert_eq!(
            selected_tests[0].reason.hops[0],
            Hop {
                dependent: lodash,
                link: Link::ChangedPart(0),
            }
        );
        assert_eq!(selected_tests[0].reason.widening_site(), None);
    }

    #[test]
    fn changed_part_of_a_file_outside_the_diff_selects_nothing() {
        let mut builder = GraphBuilder::default();
        let a = builder.add_file(repo_path("src/a.ts"));
        let a_test = builder.add_test_file(repo_path("src/a.test.ts"), "typescript");

        builder.add_edge(Edge {
            dependent: a_test,
            dependency: a,
            kind: Import,
        });
        builder.add_changed_part(
            repo_path("package.json"),
            repo_path("src/a.ts"),
            "src/a.ts is affected by field \"type\"".to_owned(),
        );

        let graph = builder.build();

        assert_eq!(
            selected_with_distances(&graph, &[modified("pnpm-lock.yaml")]),
            []
        );
        assert_eq!(
            selected_with_distances(&graph, &[modified("package.json")]),
            [("src/a.test.ts", 2)]
        );
    }

    fn graph_with_mocks(
        edges: &[(&str, EdgeKind, &str)],
        mocks: &[(&str, &str)],
        changed_contracts: &[&str],
    ) -> Graph {
        let mut builder = GraphBuilder::default();

        for &(dependent_path, kind, dependency_path) in edges {
            let dependent = if dependent_path.ends_with(".test.ts") {
                builder.add_test_file(repo_path(dependent_path), "typescript")
            } else {
                builder.add_file(repo_path(dependent_path))
            };
            let dependency = builder.add_file(repo_path(dependency_path));

            builder.add_edge(Edge {
                dependent,
                dependency,
                kind,
            });
        }

        for &(test, mocked_module) in mocks {
            let test = builder.add_file(repo_path(test));
            let mocked_module = builder.add_file(repo_path(mocked_module));

            builder.add_mock(test, mocked_module);
        }

        for &module in changed_contracts {
            let module = builder.add_file(repo_path(module));

            builder.mark_changed_contract(module);
        }

        builder.build()
    }

    const SERVICE_WITH_MOCKED_DB: &[(&str, EdgeKind, &str)] = &[
        ("src/db.ts", Import, "src/sql.ts"),
        ("src/service.ts", Import, "src/db.ts"),
        ("src/service.test.ts", Import, "src/service.ts"),
        ("src/service.test.ts", Import, "src/db.ts"),
        ("src/db.test.ts", Import, "src/db.ts"),
    ];

    #[test]
    fn test_mocking_a_module_is_not_selected_by_changes_behind_the_mock() {
        let graph = graph_with_mocks(
            SERVICE_WITH_MOCKED_DB,
            &[("src/service.test.ts", "src/db.ts")],
            &[],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/db.ts")]),
            [("src/db.test.ts", 1)]
        );
        assert_eq!(
            selected_with_distances(&graph, &[modified("src/sql.ts")]),
            [("src/db.test.ts", 2)]
        );
        assert_eq!(
            selected_with_distances(&graph, &[modified("src/service.ts")]),
            [("src/service.test.ts", 1)]
        );
    }

    #[test]
    fn changed_export_signature_of_a_mocked_module_selects_its_mockers() {
        let graph = graph_with_mocks(
            SERVICE_WITH_MOCKED_DB,
            &[("src/service.test.ts", "src/db.ts")],
            &["src/db.ts"],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/db.ts")]),
            [("src/db.test.ts", 1), ("src/service.test.ts", 1)]
        );
        assert_eq!(
            selected_with_distances(&graph, &[modified("src/sql.ts")]),
            [("src/db.test.ts", 2), ("src/service.test.ts", 2)]
        );
    }

    #[test]
    fn path_around_the_mock_still_selects_the_test_with_that_path_as_reason() {
        let edges = [
            ("src/db.ts", Import, "src/config.ts"),
            ("src/service.ts", Import, "src/db.ts"),
            ("src/logger.ts", Import, "src/config.ts"),
            ("src/service.ts", Import, "src/logger.ts"),
            ("src/service.test.ts", Import, "src/service.ts"),
        ];
        let graph = graph_with_mocks(&edges, &[("src/service.test.ts", "src/db.ts")], &[]);
        let selected_tests = select_tests(&graph, &[modified("src/config.ts")], Widening::Scoped);

        assert_eq!(selected_tests.len(), 1);
        assert_eq!(
            reason_files(&graph, &selected_tests[0]),
            [
                "src/config.ts",
                "src/logger.ts",
                "src/service.ts",
                "src/service.test.ts"
            ]
        );
    }

    #[test]
    fn asset_change_reaches_only_tests_that_render_assets() {
        let mut builder = GraphBuilder::default();
        let styles = builder.add_file(repo_path("src/button.css"));
        let button = builder.add_file(repo_path("src/button.ts"));
        let unit_test = builder.add_test_file(repo_path("src/button.test.ts"), "typescript");
        let browser_test =
            builder.add_test_file(repo_path("src/button.browser.test.ts"), "typescript");
        let edges = [
            (button, styles, Asset),
            (unit_test, button, Import),
            (browser_test, button, Import),
        ];

        for (dependent, dependency, kind) in edges {
            builder.add_edge(Edge {
                dependent,
                dependency,
                kind,
            });
        }

        builder.mark_asset_blind(unit_test);

        let graph = builder.build();

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/button.css")]),
            [("src/button.browser.test.ts", 2)]
        );
        assert_eq!(
            selected_with_distances(
                &graph,
                &[modified("src/button.css"), modified("src/button.ts")]
            ),
            [("src/button.browser.test.ts", 1), ("src/button.test.ts", 1)]
        );
        assert_eq!(
            reason_files(
                &graph,
                &select_tests(&graph, &[modified("src/button.css")], Widening::Scoped)[0]
            ),
            [
                "src/button.css",
                "src/button.ts",
                "src/button.browser.test.ts"
            ]
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
        let selected_tests = select_tests(
            &graph,
            &[modified("src/b.ts"), modified("src/a.ts")],
            Widening::Scoped,
        );

        assert_eq!(selected_tests[0].reason.change, modified("src/a.ts"));
    }

    #[test]
    fn a_read_selects_through_the_content_of_the_file_it_reads_not_through_its_imports() {
        let graph = build_graph(
            &["src/agent.test.ts"],
            &[
                ("src/agent.test.ts", Import, "src/prompts.ts"),
                ("src/prompts.ts", Reads, "src/sample.ts"),
                ("src/sample.ts", Import, "src/helper.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/sample.ts")]),
            [("src/agent.test.ts", 2)]
        );
        assert_eq!(
            selected_with_distances(&graph, &[modified("src/helper.ts")]),
            []
        );
    }

    #[test]
    fn a_listing_passes_the_changes_of_the_listed_files_on_to_its_readers() {
        let graph = build_graph(
            &["src/discovery.test.ts"],
            &[
                ("src/discovery.test.ts", Import, "src/discovery.ts"),
                ("src/discovery.ts", Reads, "extensions/a/plugin.json"),
                ("extensions/a/plugin.json", Lists, "extensions/a/index.ts"),
                ("extensions/a/index.ts", Import, "extensions/a/send.ts"),
            ],
        );
        let selected_tests = select_tests(
            &graph,
            &[modified("extensions/a/index.ts")],
            Widening::Scoped,
        );

        assert_eq!(
            reason_files(&graph, &selected_tests[0]),
            [
                "extensions/a/index.ts",
                "extensions/a/plugin.json",
                "src/discovery.ts",
                "src/discovery.test.ts"
            ]
        );
        assert_eq!(
            selected_with_distances(&graph, &[modified("extensions/a/send.ts")]),
            []
        );
    }

    #[test]
    fn a_file_reached_first_through_behaviour_is_revisited_when_a_listing_reaches_it() {
        let graph = build_graph(
            &["src/reader.test.ts"],
            &[
                ("src/reader.test.ts", Import, "src/reader.ts"),
                ("src/reader.ts", Reads, "data/list.json"),
                ("data/list.json", Import, "data/a.ts"),
                ("data/list.json", Lists, "data/z.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("data/a.ts"), modified("data/z.ts")]),
            [("src/reader.test.ts", 3)]
        );
    }

    #[test]
    fn a_naming_family_selects_its_test_only_when_the_named_file_changes() {
        let graph = build_graph(
            &["src/channel.e2e.test.ts"],
            &[
                ("src/channel.e2e.test.ts", Family, "src/channel.ts"),
                ("src/channel.ts", Import, "src/transport.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/channel.ts")]),
            [("src/channel.e2e.test.ts", 1)]
        );
        assert_eq!(
            selected_with_distances(&graph, &[modified("src/transport.ts")]),
            []
        );
    }
}
