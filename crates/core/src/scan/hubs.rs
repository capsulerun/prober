//! Hub files, the source files most test files depend on, and the few tests close to most of them that could run on every change.

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap, HashSet};

use crate::files::{FileId, RepoPath};
use crate::graph::{ContentChange, Graph};
use crate::scan::tests_depending_on;

const HUB_CANDIDATES: usize = 200;

const HUB_FILES: usize = 20;

const SMOKE_TESTS: usize = 5;

const SMOKE_TEST_DISTANCE: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubFile {
    pub path: RepoPath,
    pub tests: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SmokeSet {
    pub tests: Vec<RepoPath>,
    pub covered_hub_files: usize,
}

pub(crate) fn hubs_and_smoke_set(
    graph: &Graph,
    repo_files: &[RepoPath],
) -> (Vec<HubFile>, SmokeSet) {
    let mut candidates: Vec<(usize, &RepoPath, FileId)> = repo_files
        .iter()
        .filter_map(|path| {
            let file = graph.file_id(path).filter(|&file| !graph.is_test(file))?;
            let direct_dependents = graph
                .dependents(file)
                .filter(|edge| edge.kind.selects_tests())
                .count();

            (direct_dependents > 0).then_some((direct_dependents, path, file))
        })
        .collect();

    candidates.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(right.1)));
    candidates.truncate(HUB_CANDIDATES);

    let mut hubs: Vec<(&RepoPath, HashSet<FileId>)> = candidates
        .into_iter()
        .map(|(_, path, file)| (path, tests_depending_on(graph, [file])))
        .filter(|(_, tests)| !tests.is_empty())
        .collect();

    hubs.sort_by(|left, right| right.1.len().cmp(&left.1.len()).then(left.0.cmp(right.0)));
    hubs.truncate(HUB_FILES);

    let close_tests: Vec<HashSet<FileId>> = hubs
        .iter()
        .map(|(path, _)| close_tests(graph, path))
        .collect();
    let smoke_set = smoke_set(graph, &close_tests);
    let hub_files = hubs
        .iter()
        .map(|(path, tests)| HubFile {
            path: (*path).clone(),
            tests: tests.len(),
        })
        .collect();

    (hub_files, smoke_set)
}

fn close_tests(graph: &Graph, hub: &RepoPath) -> HashSet<FileId> {
    let Some(hub_file) = graph.file_id(hub) else {
        return HashSet::new();
    };
    let mut reached = HashSet::from([hub_file]);
    let mut frontier = vec![hub_file];

    for _ in 0..SMOKE_TEST_DISTANCE {
        frontier = frontier
            .iter()
            .flat_map(|&file| graph.dependents(file))
            .filter(|edge| edge.kind.selects_tests() && edge.kind.follows(ContentChange::None))
            .map(|edge| edge.dependent)
            .filter(|&dependent| reached.insert(dependent))
            .collect();
    }

    reached
        .into_iter()
        .filter(|&file| graph.is_test(file))
        .collect()
}

fn smoke_set(graph: &Graph, hubs: &[HashSet<FileId>]) -> SmokeSet {
    let mut hubs_by_test: HashMap<FileId, BTreeSet<usize>> = HashMap::new();

    for (hub_index, tests) in hubs.iter().enumerate() {
        for &test in tests {
            hubs_by_test.entry(test).or_default().insert(hub_index);
        }
    }

    let mut uncovered: BTreeSet<usize> = (0..hubs.len()).collect();
    let mut smoke_set = SmokeSet::default();

    while smoke_set.tests.len() < SMOKE_TESTS && !uncovered.is_empty() {
        let best = hubs_by_test
            .iter()
            .map(|(&test, covered)| (covered.intersection(&uncovered).count(), test))
            .filter(|&(gain, _)| gain > 0)
            .min_by_key(|&(gain, test)| (Reverse(gain), graph.path(test)));
        let Some((_, test)) = best else {
            break;
        };

        for hub_index in &hubs_by_test[&test] {
            uncovered.remove(hub_index);
        }

        smoke_set.tests.push(graph.path(test).clone());
    }

    smoke_set.covered_hub_files = hubs.len() - uncovered.len();
    smoke_set.tests.sort();

    smoke_set
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Edge, EdgeKind, GraphBuilder};

    #[test]
    fn the_smoke_set_greedily_covers_the_files_most_tests_depend_on() {
        let mut builder = GraphBuilder::default();
        let edges = [
            ("test/app.test.ts", "src/app.ts"),
            ("src/app.ts", "src/logger.ts"),
            ("src/app.ts", "src/config.ts"),
            ("test/logger.test.ts", "src/logger.ts"),
            ("test/config.test.ts", "src/config.ts"),
            ("test/cli.test.ts", "src/cli.ts"),
        ];
        let repo_path = |path: &str| RepoPath::new(path).unwrap();

        for test in [
            "test/app.test.ts",
            "test/logger.test.ts",
            "test/config.test.ts",
            "test/cli.test.ts",
        ] {
            builder.add_test_file(repo_path(test), "typescript");
        }

        for (dependent, dependency) in edges {
            let dependent = builder.add_file(repo_path(dependent));
            let dependency = builder.add_file(repo_path(dependency));

            builder.add_edge(Edge {
                dependent,
                dependency,
                kind: EdgeKind::Import,
            });
        }

        let graph = builder.build();
        let repo_files: Vec<RepoPath> =
            ["src/app.ts", "src/logger.ts", "src/config.ts", "src/cli.ts"]
                .into_iter()
                .map(repo_path)
                .collect();
        let (hub_files, smoke_set) = hubs_and_smoke_set(&graph, &repo_files);

        assert_eq!(
            hub_files
                .iter()
                .map(|hub_file| (hub_file.path.as_str(), hub_file.tests))
                .collect::<Vec<_>>(),
            [
                ("src/config.ts", 2),
                ("src/logger.ts", 2),
                ("src/app.ts", 1),
                ("src/cli.ts", 1),
            ]
        );
        assert_eq!(
            smoke_set,
            SmokeSet {
                tests: vec![repo_path("test/app.test.ts"), repo_path("test/cli.test.ts")],
                covered_hub_files: 4,
            }
        );
    }
}
