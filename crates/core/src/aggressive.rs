//! Opt-in options that drop impacted tests for savings (far tests, tests reached only past hubs, deferred suites), and the tests they never drop.

use anyhow::{Result, bail};
use serde::Deserialize;
use toml::Spanned;

use crate::files::{FileId, RepoPath};
use crate::graph::Graph;
use crate::patterns::PathPattern;
use crate::report::CONFIG_FILE_NAME;
use crate::rules::{ConfigWarning, Targets, line_of, rule_error, table_line};
use crate::select::SelectedTest;

const DEFER_HEADER: &str = "[[defer]]";

const PROTECTED_HEADER: &str = "[protected]";

pub const MAX_DISTANCE: &str = "max_distance";

pub const HUB_FANOUT_LIMIT: &str = "hub_fanout_limit";

pub const DEFER: &str = "defer";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AggressiveSection {
    max_distance: Option<Spanned<usize>>,
    hub_fanout_limit: Option<Spanned<usize>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeferTable {
    tests: Targets,
    max_distance: usize,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProtectedSection {
    #[serde(default)]
    tests: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub value: usize,
    pub line: usize,
}

#[derive(Debug)]
pub struct Defer {
    pub line: usize,
    pub tests: Vec<PathPattern>,
    pub max_distance: usize,
}

#[derive(Debug, Default)]
pub struct Aggressive {
    pub max_distance: Option<Limit>,
    pub hub_fanout_limit: Option<Limit>,
    pub defers: Vec<Defer>,
    pub protected: Vec<PathPattern>,
    protected_line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropCause {
    MaxDistance {
        limit: Limit,
    },
    HubFanoutLimit {
        limit: Limit,
        hub: RepoPath,
        fanout: usize,
    },
    Defer {
        line: usize,
        pattern: String,
        max_distance: usize,
    },
}

impl DropCause {
    pub fn option(&self) -> &'static str {
        match self {
            DropCause::MaxDistance { .. } => MAX_DISTANCE,
            DropCause::HubFanoutLimit { .. } => HUB_FANOUT_LIMIT,
            DropCause::Defer { .. } => DEFER,
        }
    }

    pub fn description(&self) -> String {
        match self {
            DropCause::MaxDistance { limit } => format!(
                "[aggressive] max_distance = {} ({CONFIG_FILE_NAME}:{})",
                limit.value, limit.line
            ),
            DropCause::HubFanoutLimit { limit, hub, fanout } => format!(
                "[aggressive] hub_fanout_limit = {} ({CONFIG_FILE_NAME}:{}): every path to it goes past the direct dependents of {hub}, which {fanout} files depend on",
                limit.value, limit.line
            ),
            DropCause::Defer {
                line,
                pattern,
                max_distance,
            } => format!(
                "[[defer]] {pattern:?} with max_distance = {max_distance} ({CONFIG_FILE_NAME}:{line})"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedTest {
    pub selected_test: SelectedTest,
    pub cause: DropCause,
}

impl Aggressive {
    pub(crate) fn parse(
        config_text: &str,
        section: AggressiveSection,
        defer_tables: Vec<Spanned<DeferTable>>,
        protected: ProtectedSection,
    ) -> Result<Self> {
        let limit = |setting: Spanned<usize>| Limit {
            value: *setting.get_ref(),
            line: line_of(config_text, setting.span().start),
        };
        let defers = defer_tables
            .into_iter()
            .map(|defer_table| {
                let line = table_line(config_text, &defer_table, DEFER_HEADER);
                let defer_table = defer_table.into_inner();

                defer(line, &defer_table)
            })
            .collect::<Result<Vec<Defer>>>()?;
        let protected_patterns = protected
            .tests
            .iter()
            .map(|test| PathPattern::new(test).map_err(rule_error(None, PROTECTED_HEADER)))
            .collect::<Result<Vec<PathPattern>>>()?;

        Ok(Self {
            max_distance: section.max_distance.map(limit),
            hub_fanout_limit: section.hub_fanout_limit.map(limit),
            defers,
            protected: protected_patterns,
            protected_line: config_text
                .find(PROTECTED_HEADER)
                .map_or(0, |offset| line_of(config_text, offset)),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.max_distance.is_some() || self.hub_fanout_limit.is_some() || !self.defers.is_empty()
    }

    pub fn split(
        &self,
        graph: &Graph,
        selected_tests: Vec<SelectedTest>,
    ) -> (Vec<SelectedTest>, Vec<DroppedTest>) {
        let mut kept = Vec::new();
        let mut dropped = Vec::new();

        for selected_test in selected_tests {
            match self.drop_cause(graph, &selected_test) {
                Some(cause) => dropped.push(DroppedTest {
                    selected_test,
                    cause,
                }),
                None => kept.push(selected_test),
            }
        }

        (kept, dropped)
    }

    pub fn drop_cause(&self, graph: &Graph, selected_test: &SelectedTest) -> Option<DropCause> {
        if !self.is_enabled() || self.is_protected(graph, selected_test.file) {
            return None;
        }

        let path = graph.path(selected_test.file).as_str();
        let distance = selected_test.distance();

        for defer in &self.defers {
            let Some(pattern) = defer.tests.iter().find(|pattern| pattern.is_match(path)) else {
                continue;
            };

            if distance > defer.max_distance {
                return Some(DropCause::Defer {
                    line: defer.line,
                    pattern: pattern.text().to_owned(),
                    max_distance: defer.max_distance,
                });
            }
        }

        if let Some(limit) = self.max_distance
            && distance > limit.value
        {
            return Some(DropCause::MaxDistance { limit });
        }

        let hub_crossing = selected_test.hub_crossing();

        if let Some(limit) = self.hub_fanout_limit
            && hub_crossing.fanout > limit.value
            && let Some(hub) = hub_crossing.hub
        {
            return Some(DropCause::HubFanoutLimit {
                limit,
                hub: graph.path(hub).clone(),
                fanout: hub_crossing.fanout,
            });
        }

        None
    }

    pub fn warnings(&self, graph: &Graph) -> Vec<ConfigWarning> {
        let tests: Vec<&str> = graph
            .test_files_by_path()
            .into_iter()
            .map(|test_file| graph.path(test_file.file).as_str())
            .collect();
        let matches_no_test =
            |pattern: &PathPattern| !tests.iter().any(|test| pattern.is_match(test));
        let mut warnings = Vec::new();

        for defer in &self.defers {
            for pattern in defer
                .tests
                .iter()
                .filter(|pattern| matches_no_test(pattern))
            {
                warnings.push(ConfigWarning {
                    line: Some(defer.line),
                    message: format!(
                        "{DEFER_HEADER} tests {:?} matches no test file",
                        pattern.text()
                    ),
                });
            }
        }

        for pattern in self
            .protected
            .iter()
            .filter(|pattern| matches_no_test(pattern))
        {
            warnings.push(ConfigWarning {
                line: Some(self.protected_line),
                message: format!(
                    "{PROTECTED_HEADER} test {:?} matches no test file",
                    pattern.text()
                ),
            });
        }

        warnings
    }

    fn is_protected(&self, graph: &Graph, test: FileId) -> bool {
        let path = graph.path(test).as_str();
        let mocks_a_changed_contract = graph
            .mocked_modules(test)
            .iter()
            .any(|&module| graph.has_changed_contract(module));

        mocks_a_changed_contract || self.protected.iter().any(|pattern| pattern.is_match(path))
    }
}

fn defer(line: usize, defer_table: &DeferTable) -> Result<Defer> {
    let tests = defer_table.tests.as_slice();

    if tests.is_empty() {
        bail!("invalid {CONFIG_FILE_NAME} at line {line}: {DEFER_HEADER} tests lists no test");
    }

    let tests = tests
        .iter()
        .map(|test| PathPattern::new(test).map_err(rule_error(Some(line), DEFER_HEADER)))
        .collect::<Result<Vec<PathPattern>>>()?;

    Ok(Defer {
        line,
        tests,
        max_distance: defer_table.max_distance,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::{ChangeKind, FileChange};
    use crate::config::Config;
    use crate::graph::EdgeKind::Import;
    use crate::graph::{Edge, GraphBuilder};
    use crate::select::{Widening, select_tests};

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn modified(path: &str) -> FileChange {
        FileChange {
            path: repo_path(path),
            kind: ChangeKind::Modified,
        }
    }

    fn aggressive(config_text: &str) -> Aggressive {
        Config::parse(config_text).unwrap().aggressive
    }

    fn hub_graph() -> GraphBuilder {
        let mut builder = GraphBuilder::default();
        let test_files = [
            "src/hub.test.ts",
            "src/use.test.ts",
            "src/far.test.ts",
            "test/api.integration.test.ts",
        ];
        let edges = [
            ("src/hub.test.ts", "src/hub.ts"),
            ("src/use.ts", "src/hub.ts"),
            ("src/other.ts", "src/hub.ts"),
            ("src/use.test.ts", "src/use.ts"),
            ("src/far.ts", "src/use.ts"),
            ("src/far.test.ts", "src/far.ts"),
            ("test/api.integration.test.ts", "src/far.ts"),
        ];

        for test_file in test_files {
            builder.add_test_file(repo_path(test_file), "typescript");
        }

        for (dependent_path, dependency_path) in edges {
            let dependent = builder.add_file(repo_path(dependent_path));
            let dependency = builder.add_file(repo_path(dependency_path));

            builder.add_edge(Edge {
                dependent,
                dependency,
                kind: Import,
            });
        }

        builder
    }

    fn split<'g>(
        graph: &'g Graph,
        aggressive: &Aggressive,
    ) -> (Vec<&'g str>, Vec<(&'g str, String)>) {
        let selected_tests = select_tests(graph, &[modified("src/hub.ts")], Widening::Scoped);
        let (kept, dropped) = aggressive.split(graph, selected_tests);

        (
            kept.iter()
                .map(|selected_test| graph.path(selected_test.file).as_str())
                .collect(),
            dropped
                .iter()
                .map(|dropped_test| {
                    (
                        graph.path(dropped_test.selected_test.file).as_str(),
                        dropped_test.cause.description(),
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn options_are_off_unless_configured() {
        let graph = hub_graph().build();
        let options = aggressive("");

        assert!(!options.is_enabled());
        assert_eq!(
            split(&graph, &options),
            (
                vec![
                    "src/hub.test.ts",
                    "src/use.test.ts",
                    "src/far.test.ts",
                    "test/api.integration.test.ts"
                ],
                vec![]
            )
        );
    }

    #[test]
    fn max_distance_drops_the_tests_farther_from_the_changes() {
        let graph = hub_graph().build();
        let options = aggressive("[aggressive]\nmax_distance = 2\n");

        assert_eq!(
            split(&graph, &options),
            (
                vec!["src/hub.test.ts", "src/use.test.ts"],
                vec![
                    (
                        "src/far.test.ts",
                        "[aggressive] max_distance = 2 (prober.toml:2)".to_owned()
                    ),
                    (
                        "test/api.integration.test.ts",
                        "[aggressive] max_distance = 2 (prober.toml:2)".to_owned()
                    )
                ]
            )
        );
    }

    #[test]
    fn hub_fanout_limit_keeps_only_the_direct_dependents_of_a_hub() {
        let graph = hub_graph().build();
        let options = aggressive("[aggressive]\nhub_fanout_limit = 2\n");
        let past_the_hub = "[aggressive] hub_fanout_limit = 2 (prober.toml:2): every path to it goes past the direct dependents of src/hub.ts, which 3 files depend on".to_owned();

        assert_eq!(
            split(&graph, &options),
            (
                vec!["src/hub.test.ts"],
                vec![
                    ("src/use.test.ts", past_the_hub.clone()),
                    ("src/far.test.ts", past_the_hub.clone()),
                    ("test/api.integration.test.ts", past_the_hub)
                ]
            )
        );
        assert_eq!(
            split(&graph, &aggressive("[aggressive]\nhub_fanout_limit = 3\n")).1,
            []
        );
    }

    #[test]
    fn deferred_tests_run_only_within_their_own_distance() {
        let graph = hub_graph().build();
        let options = aggressive(
            "[aggressive]\nmax_distance = 4\n\n[[defer]]\ntests = \"test/**/*.integration.test.ts\"\nmax_distance = 2\n",
        );

        assert_eq!(
            split(&graph, &options),
            (
                vec!["src/hub.test.ts", "src/use.test.ts", "src/far.test.ts"],
                vec![(
                    "test/api.integration.test.ts",
                    "[[defer]] \"test/**/*.integration.test.ts\" with max_distance = 2 (prober.toml:4)"
                        .to_owned()
                )]
            )
        );
    }

    #[test]
    fn protected_tests_and_mock_contract_consumers_are_never_dropped() {
        let mut builder = hub_graph();
        let far_test = builder.add_file(repo_path("src/far.test.ts"));
        let hub = builder.add_file(repo_path("src/hub.ts"));

        builder.add_mock(far_test, hub);
        builder.mark_changed_contract(hub);

        let graph = builder.build();
        let options =
            aggressive("[aggressive]\nmax_distance = 1\n\n[protected]\ntests = [\"test/**\"]\n");

        assert_eq!(
            split(&graph, &options).0,
            [
                "src/hub.test.ts",
                "src/far.test.ts",
                "test/api.integration.test.ts"
            ]
        );
    }

    #[test]
    fn patterns_matching_no_test_are_reported() {
        let graph = hub_graph().build();
        let options = aggressive(
            "[[defer]]\ntests = [\"test/**\", \"e2e/**\"]\nmax_distance = 1\n\n[protected]\ntests = [\"src/gone.test.ts\"]\n",
        );
        let warnings: Vec<String> = options
            .warnings(&graph)
            .iter()
            .map(ToString::to_string)
            .collect();

        assert_eq!(
            warnings,
            [
                "prober.toml:1: [[defer]] tests \"e2e/**\" matches no test file",
                "prober.toml:5: [protected] test \"src/gone.test.ts\" matches no test file"
            ]
        );
    }

    #[test]
    fn every_aggressive_config_error_is_one_line() {
        let invalid_configs = [
            (
                "[aggressive]\nmax_hops = 3\n",
                "invalid prober.toml at line 2, column 1: unknown field `max_hops`, expected `max_distance` or `hub_fanout_limit`",
            ),
            (
                "[aggressive]\nmax_distance = -1\n",
                "invalid prober.toml at line 2, column 16: invalid value: integer `-1`, expected usize",
            ),
            (
                "[[defer]]\ntests = \"test/**\"\n",
                "invalid prober.toml at line 1, column 1: missing field `max_distance`",
            ),
            (
                "[[defer]]\ntests = []\nmax_distance = 1\n",
                "invalid prober.toml at line 1: [[defer]] tests lists no test",
            ),
            (
                "\n[[defer]]\ntests = \"test/[unclosed\"\nmax_distance = 1\n",
                "invalid prober.toml at line 2: [[defer]] invalid pattern \"test/[unclosed\": unclosed character class; missing ']'",
            ),
        ];

        for (invalid_config, expected_message) in invalid_configs {
            assert_eq!(
                Config::parse(invalid_config).unwrap_err().to_string(),
                expected_message
            );
        }
    }
}
