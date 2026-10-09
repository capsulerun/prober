//! Why a test file is selected or not: its path from the changes, or the rule that blocks every path to it.

use std::collections::BTreeSet;

use crate::changes::FileChange;
use crate::files::{FileId, RepoPath};
use crate::graph::{Edge, Graph};
use crate::report::{AlwaysRunTest, CONFIG_FILE_NAME, describe_reason};
use crate::select::{Hop, Link, Reason, Relaxation, SelectedTest, Widening, select_relaxed};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    WideningOff,
    Ignored { lines: Vec<usize> },
    Mocked { modules: Vec<RepoPath> },
    Content,
    TypeOnly,
    AssetBlind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Explanation {
    NotATest,
    Selected {
        reason: Vec<String>,
    },
    AlwaysRun {
        reason: String,
    },
    Blocked {
        blocker: Blocker,
        reason: Vec<String>,
    },
    NoPath {
        changed_file_count: usize,
    },
}

impl Explanation {
    pub fn is_selected(&self) -> bool {
        matches!(
            self,
            Explanation::Selected { .. } | Explanation::AlwaysRun { .. }
        )
    }

    pub fn to_text(&self, test: &RepoPath) -> String {
        let (headline, reason) = match self {
            Explanation::NotATest => (format!("{test} is not a test file"), &[][..]),
            Explanation::Selected { reason } => (format!("{test} is selected:"), reason.as_slice()),
            Explanation::AlwaysRun { reason } => (format!("{test} is selected: {reason}"), &[][..]),
            Explanation::Blocked { blocker, reason } => (
                format!("{test} is not selected: {}:", blocker.description()),
                reason.as_slice(),
            ),
            Explanation::NoPath {
                changed_file_count: 1,
            } => (
                format!("{test} is not selected: no path links it to the changed file"),
                &[][..],
            ),
            Explanation::NoPath { changed_file_count } => (
                format!(
                    "{test} is not selected: no path links it to the {changed_file_count} changed files"
                ),
                &[][..],
            ),
        };
        let mut text = headline;

        text.push('\n');

        for hop in reason {
            text.push_str("  ");
            text.push_str(hop);
            text.push('\n');
        }

        text
    }
}

impl Blocker {
    fn description(&self) -> String {
        match self {
            Blocker::WideningOff => {
                "it is reached only through an unresolved site, and widening is off ([inference] widening = false)".to_owned()
            }
            Blocker::Ignored { lines } => {
                let lines: Vec<String> = lines
                    .iter()
                    .map(|line| format!("{CONFIG_FILE_NAME}:{line}"))
                    .collect();

                format!("every path goes through what {} ignores", lines.join(", "))
            }
            Blocker::Mocked { modules } => {
                let modules: Vec<&str> = modules.iter().map(RepoPath::as_str).collect();

                format!(
                    "every path goes through modules it mocks with a closed factory ({}), whose exports did not change",
                    modules.join(", ")
                )
            }
            Blocker::Content => {
                "it reaches the changes only through files it reads, lists or is named after, and their content did not change".to_owned()
            }
            Blocker::TypeOnly => {
                "it reaches the changes only through type-only imports, which never select tests"
                    .to_owned()
            }
            Blocker::AssetBlind => {
                "the changed assets reach it, but it runs in a project that renders no assets"
                    .to_owned()
            }
        }
    }
}

pub fn explain(
    graph: &Graph,
    changes: &[FileChange],
    test: &RepoPath,
    widening: Widening,
    always_run: &[AlwaysRunTest],
) -> Explanation {
    let Some(test_file) = graph.file_id(test).filter(|&file| graph.is_test(file)) else {
        return Explanation::NotATest;
    };
    let selected = select_relaxed(graph, changes, widening, None);

    if let Some(selected_test) = find(&selected, test_file) {
        return Explanation::Selected {
            reason: describe_reason(graph, &selected_test.reason),
        };
    }

    if !selected.is_empty()
        && let Some(always_run_test) = always_run
            .iter()
            .find(|always_run_test| always_run_test.path == *test)
    {
        return Explanation::AlwaysRun {
            reason: always_run_test.reason.clone(),
        };
    }

    if widening == Widening::Off {
        let widened = select_relaxed(graph, changes, Widening::Scoped, None);

        if let Some(selected_test) = find(&widened, test_file) {
            return blocked(graph, Blocker::WideningOff, &selected_test.reason);
        }
    }

    let relaxations = [
        Relaxation::Ignores,
        Relaxation::Mocks,
        Relaxation::Content,
        Relaxation::TypeOnly,
        Relaxation::Assets,
    ];

    for relaxation in relaxations {
        let relaxed = select_relaxed(graph, changes, widening, Some(relaxation));
        let Some(selected_test) = find(&relaxed, test_file) else {
            continue;
        };
        let blocker = match relaxation {
            Relaxation::Ignores => Blocker::Ignored {
                lines: ignoring_lines(graph, &selected_test.reason),
            },
            Relaxation::Mocks => Blocker::Mocked {
                modules: mocked_modules(graph, test_file, &selected_test.reason),
            },
            Relaxation::Content => Blocker::Content,
            Relaxation::TypeOnly => Blocker::TypeOnly,
            Relaxation::Assets => Blocker::AssetBlind,
        };

        let mut reason = describe_reason(graph, &selected_test.reason);

        if relaxation == Relaxation::Ignores {
            mark_ignored_hops(graph, &selected_test.reason, &mut reason);
        }

        return Explanation::Blocked { blocker, reason };
    }

    Explanation::NoPath {
        changed_file_count: changes.len(),
    }
}

fn find(selected_tests: &[SelectedTest], test_file: FileId) -> Option<&SelectedTest> {
    selected_tests
        .iter()
        .find(|selected_test| selected_test.file == test_file)
}

fn blocked(graph: &Graph, blocker: Blocker, reason: &Reason) -> Explanation {
    Explanation::Blocked {
        blocker,
        reason: describe_reason(graph, reason),
    }
}

fn ignoring_lines(graph: &Graph, reason: &Reason) -> Vec<usize> {
    let mut lines = BTreeSet::new();

    for hop in &reason.hops {
        match hop.link {
            Link::Edge { .. } => lines.extend(ignored_edge_line(graph, hop)),
            Link::SetAsideSite(site_index) => {
                lines.insert(graph.set_aside_sites()[site_index].set_aside.line());
            }
            Link::ChangedPart(_) | Link::UnresolvedSite(_) => {}
        }
    }

    lines.into_iter().collect()
}

fn mark_ignored_hops(graph: &Graph, reason: &Reason, described_hops: &mut [String]) {
    for (hop, described_hop) in reason.hops.iter().zip(described_hops.iter_mut().skip(1)) {
        if let Some(line) = ignored_edge_line(graph, hop) {
            described_hop.push_str(&format!(", ignored by {CONFIG_FILE_NAME}:{line}"));
        }
    }
}

fn ignored_edge_line(graph: &Graph, hop: &Hop) -> Option<usize> {
    let Link::Edge { kind, dependency } = hop.link else {
        return None;
    };
    let edge = Edge {
        dependent: hop.dependent,
        dependency,
        kind,
    };

    graph
        .ignored_edge(&edge)
        .map(|ignored_edge| ignored_edge.line)
}

fn mocked_modules(graph: &Graph, test_file: FileId, reason: &Reason) -> Vec<RepoPath> {
    let mocked = graph.mocked_modules(test_file);
    let mut modules = BTreeSet::new();
    let path_files = graph
        .file_id(&reason.change.path)
        .into_iter()
        .chain(reason.hops.iter().map(|hop| hop.dependent));

    for file in path_files {
        if mocked.binary_search(&file).is_ok() {
            modules.insert(graph.path(file).clone());
        }
    }

    modules.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::ChangeKind;
    use crate::config::Config;
    use crate::graph::{EdgeKind, GraphBuilder, Inference, UnresolvedCause, UnresolvedSite};

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn modified(path: &str) -> FileChange {
        FileChange {
            path: repo_path(path),
            kind: ChangeKind::Modified,
        }
    }

    fn project(config_text: &str) -> Graph {
        let mut builder = GraphBuilder::default();
        let edges = [
            ("test/a.test.ts", EdgeKind::Import, "src/a.ts"),
            ("test/types.test.ts", EdgeKind::TypeOnly, "src/a.ts"),
            ("test/prompts.test.ts", EdgeKind::Reads, "src/sample.ts"),
            ("src/sample.ts", EdgeKind::Import, "src/a.ts"),
            ("test/mocked.test.ts", EdgeKind::Import, "src/a.ts"),
            ("test/loader.test.ts", EdgeKind::Import, "src/loader.ts"),
            ("test/button.test.ts", EdgeKind::Import, "src/button.ts"),
            ("src/button.ts", EdgeKind::Asset, "src/button.css"),
        ];

        builder.add_language("typescript", |path| path.as_str().ends_with(".ts"));

        for test in [
            "test/a.test.ts",
            "test/types.test.ts",
            "test/prompts.test.ts",
            "test/mocked.test.ts",
            "test/loader.test.ts",
            "test/button.test.ts",
            "test/other.test.ts",
            "test/ignored.test.ts",
        ] {
            builder.add_test_file(repo_path(test), "typescript");
        }

        for (dependent, kind, dependency) in edges {
            let dependent = builder.add_file(repo_path(dependent));
            let dependency = builder.add_file(repo_path(dependency));

            builder.add_edge(Edge {
                dependent,
                dependency,
                kind,
            });
        }

        let ignored_test = builder.add_file(repo_path("test/ignored.test.ts"));
        let changed = builder.add_file(repo_path("src/a.ts"));
        let mocked_test = builder.add_file(repo_path("test/mocked.test.ts"));
        let button_test = builder.add_file(repo_path("test/button.test.ts"));
        let loader = builder.add_file(repo_path("src/loader.ts"));

        builder.add_inferred_edge(
            Edge {
                dependent: ignored_test,
                dependency: changed,
                kind: EdgeKind::Dynamic,
            },
            Inference {
                line: 4,
                kind: "glob",
                pattern: "src/*.ts".to_owned(),
                detail: None,
            },
        );
        builder.add_mock(mocked_test, changed);
        builder.mark_asset_blind(button_test);
        builder.add_unresolved_site(UnresolvedSite {
            file: loader,
            line: 7,
            expression: "import(name)".to_owned(),
            cause: UnresolvedCause::Parameter,
            language: "typescript",
        });

        let files: Vec<RepoPath> = builder.files().map(|(_, path)| path.clone()).collect();

        Config::parse(config_text)
            .unwrap()
            .rules
            .apply(&mut builder, &files, &[]);

        builder.build()
    }

    fn explained(test: &str, widening: Widening, change: &str) -> String {
        let graph = project("[[ignore]]\nfrom = \"src/**\"\nto = \"test/ignored.test.ts\"\n");

        explain(&graph, &[modified(change)], &repo_path(test), widening, &[])
            .to_text(&repo_path(test))
    }

    #[test]
    fn a_selected_test_shows_its_path_from_the_change() {
        assert_eq!(
            explained("test/a.test.ts", Widening::Scoped, "src/a.ts"),
            "test/a.test.ts is selected:\n  src/a.ts (modified)\n  -> imported by test/a.test.ts\n"
        );
    }

    #[test]
    fn a_test_left_out_names_the_rule_blocking_every_path() {
        let blocked = [
            (
                "test/types.test.ts",
                "src/a.ts",
                "test/types.test.ts is not selected: it reaches the changes only through type-only imports, which never select tests:\n  src/a.ts (modified)\n  -> type-imported by test/types.test.ts\n",
            ),
            (
                "test/prompts.test.ts",
                "src/a.ts",
                "test/prompts.test.ts is not selected: it reaches the changes only through files it reads, lists or is named after, and their content did not change:\n  src/a.ts (modified)\n  -> imported by src/sample.ts\n  -> read by test/prompts.test.ts\n",
            ),
            (
                "test/mocked.test.ts",
                "src/a.ts",
                "test/mocked.test.ts is not selected: every path goes through modules it mocks with a closed factory (src/a.ts), whose exports did not change:\n  src/a.ts (modified)\n  -> imported by test/mocked.test.ts\n",
            ),
            (
                "test/ignored.test.ts",
                "src/a.ts",
                "test/ignored.test.ts is not selected: every path goes through what prober.toml:1 ignores:\n  src/a.ts (modified)\n  -> loaded by test/ignored.test.ts:4 (inferred glob src/*.ts), ignored by prober.toml:1\n",
            ),
            (
                "test/button.test.ts",
                "src/button.css",
                "test/button.test.ts is not selected: the changed assets reach it, but it runs in a project that renders no assets:\n  src/button.css (modified)\n  -> imported as an asset by src/button.ts\n  -> imported by test/button.test.ts\n",
            ),
            (
                "test/other.test.ts",
                "src/a.ts",
                "test/other.test.ts is not selected: no path links it to the changed file\n",
            ),
        ];

        for (test, change, expected_text) in blocked {
            assert_eq!(explained(test, Widening::Scoped, change), expected_text);
        }
    }

    #[test]
    fn widening_switched_off_is_named_when_only_widening_reaches_the_test() {
        assert_eq!(
            explained("test/loader.test.ts", Widening::Off, "src/a.ts"),
            "test/loader.test.ts is not selected: it is reached only through an unresolved site, and widening is off ([inference] widening = false):\n  src/a.ts (modified)\n  -> widened: unresolved dynamic import at src/loader.ts:7 (import(name))\n  -> imported by test/loader.test.ts\n"
        );
    }

    #[test]
    fn a_test_run_by_always_run_or_unknown_says_so() {
        let graph = project("");
        let always_run = [AlwaysRunTest {
            path: repo_path("test/other.test.ts"),
            language: "typescript",
            reason: "always run by prober.toml:2 (\"test/other.test.ts\")".to_owned(),
        }];
        let changes = [modified("src/a.ts")];

        assert_eq!(
            explain(
                &graph,
                &changes,
                &repo_path("test/other.test.ts"),
                Widening::Scoped,
                &always_run
            ),
            Explanation::AlwaysRun {
                reason: "always run by prober.toml:2 (\"test/other.test.ts\")".to_owned()
            }
        );
        assert_eq!(
            explain(
                &graph,
                &changes,
                &repo_path("src/a.ts"),
                Widening::Scoped,
                &[]
            ),
            Explanation::NotATest
        );
    }
}
