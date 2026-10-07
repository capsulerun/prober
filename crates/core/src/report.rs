//! The result of a run (mode, ordered targets, reasons, stats) and its JSON, list and summary renderings.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::changes::FileChange;
use crate::config::AlwaysAllMatch;
use crate::files::{FileId, RepoPath};
use crate::graph::{EdgeKind, Graph, TestFile};
use crate::select::{Hop, Reason, SelectedTest};

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Selected,
    All { reasons: Vec<ModeReason> },
    None { reasons: Vec<ModeReason> },
}

impl Mode {
    pub fn name(&self) -> &'static str {
        match self {
            Mode::Selected => "selected",
            Mode::All { .. } => "all",
            Mode::None { .. } => "none",
        }
    }

    pub fn reasons(&self) -> Option<&[ModeReason]> {
        match self {
            Mode::Selected => None,
            Mode::All { reasons } | Mode::None { reasons } => Some(reasons),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeReason {
    AlwaysAll {
        change: FileChange,
        matched_path: RepoPath,
        pattern: String,
    },
    NoImpact {
        changed_file_count: usize,
    },
}

impl ModeReason {
    pub fn kind(&self) -> &'static str {
        match self {
            ModeReason::AlwaysAll { .. } => "always_all",
            ModeReason::NoImpact { .. } => "no_impact",
        }
    }

    pub fn message(&self) -> String {
        match self {
            ModeReason::AlwaysAll {
                change, pattern, ..
            } => format!("{change} matches always_all {pattern:?}"),
            ModeReason::NoImpact {
                changed_file_count: 0,
            } => "no file changed".to_owned(),
            ModeReason::NoImpact {
                changed_file_count: 1,
            } => "1 changed file reaches no test file".to_owned(),
            ModeReason::NoImpact { changed_file_count } => {
                format!("{changed_file_count} changed files reach no test file")
            }
        }
    }
}

impl From<AlwaysAllMatch<'_>> for ModeReason {
    fn from(always_all_match: AlwaysAllMatch<'_>) -> Self {
        ModeReason::AlwaysAll {
            change: always_all_match.change.clone(),
            matched_path: always_all_match.matched_path.clone(),
            pattern: always_all_match.pattern.to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Impact {
    pub distance: usize,
    pub reason: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub path: RepoPath,
    pub language: &'static str,
    pub impact: Option<Impact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListSeparator {
    Newline,
    Nul,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub mode: Mode,
    pub targets: Vec<Target>,
    pub test_count_by_language: BTreeMap<&'static str, usize>,
    pub config_hash: String,
}

impl Report {
    pub fn from_selection(
        graph: &Graph,
        changes: &[FileChange],
        selected_tests: &[SelectedTest],
        config_hash: String,
    ) -> Self {
        let mode = if selected_tests.is_empty() {
            Mode::None {
                reasons: vec![ModeReason::NoImpact {
                    changed_file_count: changes.len(),
                }],
            }
        } else {
            Mode::Selected
        };
        let targets = selected_tests
            .iter()
            .map(|selected_test| Target {
                path: graph.path(selected_test.file).clone(),
                language: selected_test_language(graph, selected_test.file),
                impact: Some(Impact {
                    distance: selected_test.distance(),
                    reason: describe_reason(graph, &selected_test.reason),
                }),
            })
            .collect();

        Self {
            mode,
            targets,
            test_count_by_language: count_by_language(&graph.test_files_by_path()),
            config_hash,
        }
    }

    pub fn all(graph: &Graph, reasons: Vec<ModeReason>, config_hash: String) -> Self {
        let test_files = graph.test_files_by_path();
        let targets = test_files
            .iter()
            .map(|test_file| Target {
                path: graph.path(test_file.file).clone(),
                language: test_file.language,
                impact: None,
            })
            .collect();

        Self {
            mode: Mode::All { reasons },
            targets,
            test_count_by_language: count_by_language(&test_files),
            config_hash,
        }
    }

    pub fn test_count(&self) -> usize {
        self.test_count_by_language.values().sum()
    }

    pub fn to_json(&self) -> String {
        let mut targets: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        let mut reasons = BTreeMap::new();
        let mut distances = BTreeMap::new();

        for target in &self.targets {
            let path = target.path.as_str();

            targets.entry(target.language).or_default().push(path);

            if let Some(impact) = &target.impact {
                reasons.insert(path, impact.reason.as_slice());
                distances.insert(path, impact.distance);
            }
        }

        let mode_reasons = self
            .mode
            .reasons()
            .map(|reasons| reasons.iter().map(JsonModeReason::new).collect());
        let json_report = JsonReport {
            schema_version: SCHEMA_VERSION,
            mode: self.mode.name(),
            mode_reasons,
            targets,
            reasons,
            distances,
            stats: self.json_stats(),
        };
        let mut json =
            serde_json::to_string_pretty(&json_report).expect("report serializes to JSON");

        json.push('\n');

        json
    }

    pub fn to_list(&self, separator: ListSeparator) -> String {
        let separator = match separator {
            ListSeparator::Newline => '\n',
            ListSeparator::Nul => '\0',
        };
        let mut list = String::new();

        for target in &self.targets {
            list.push_str(target.path.as_str());
            list.push(separator);
        }

        list
    }

    pub fn summary_line(&self) -> String {
        let test_count = self.test_count();
        let selected_count = self.targets.len();

        match &self.mode {
            Mode::Selected => format!(
                "prober: selected {selected_count}/{test_count} test files ({})",
                format_percentage(selected_count, test_count)
            ),
            Mode::All { reasons } => format!(
                "prober: all {test_count} test files — {}",
                join_messages(reasons)
            ),
            Mode::None { reasons } => format!(
                "prober: none of {test_count} test files — {}",
                join_messages(reasons)
            ),
        }
    }

    fn json_stats(&self) -> JsonStats<'_> {
        let mut selected_count_by_language: BTreeMap<&str, usize> = BTreeMap::new();

        for target in &self.targets {
            *selected_count_by_language
                .entry(target.language)
                .or_default() += 1;
        }

        let by_language = self
            .test_count_by_language
            .iter()
            .map(|(&language, &total)| {
                let selected = selected_count_by_language
                    .get(language)
                    .copied()
                    .unwrap_or_default();
                let language_stats = JsonLanguageStats {
                    total,
                    selected,
                    ratio: rounded_ratio(selected, total),
                };

                (language, language_stats)
            })
            .collect();

        JsonStats {
            tests_total: self.test_count(),
            tests_selected: self.targets.len(),
            selected_ratio: rounded_ratio(self.targets.len(), self.test_count()),
            by_language,
            config_hash: &self.config_hash,
        }
    }
}

#[derive(Serialize)]
struct JsonReport<'a> {
    schema_version: u32,
    mode: &'static str,
    mode_reasons: Option<Vec<JsonModeReason<'a>>>,
    targets: BTreeMap<&'static str, Vec<&'a str>>,
    reasons: BTreeMap<&'a str, &'a [String]>,
    distances: BTreeMap<&'a str, usize>,
    stats: JsonStats<'a>,
}

#[derive(Serialize)]
struct JsonModeReason<'a> {
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pattern: Option<&'a str>,
    message: String,
}

impl<'a> JsonModeReason<'a> {
    fn new(mode_reason: &'a ModeReason) -> Self {
        let (file, pattern) = match mode_reason {
            ModeReason::AlwaysAll {
                matched_path,
                pattern,
                ..
            } => (Some(matched_path.as_str()), Some(pattern.as_str())),
            ModeReason::NoImpact { .. } => (None, None),
        };

        Self {
            kind: mode_reason.kind(),
            file,
            pattern,
            message: mode_reason.message(),
        }
    }
}

#[derive(Serialize)]
struct JsonStats<'a> {
    tests_total: usize,
    tests_selected: usize,
    selected_ratio: f64,
    by_language: BTreeMap<&'static str, JsonLanguageStats>,
    config_hash: &'a str,
}

#[derive(Serialize)]
struct JsonLanguageStats {
    total: usize,
    selected: usize,
    ratio: f64,
}

fn selected_test_language(graph: &Graph, selected_test: FileId) -> &'static str {
    graph
        .test_language(selected_test)
        .expect("selected files are test files")
}

fn count_by_language(test_files: &[TestFile]) -> BTreeMap<&'static str, usize> {
    let mut test_count_by_language = BTreeMap::new();

    for test_file in test_files {
        *test_count_by_language
            .entry(test_file.language)
            .or_default() += 1;
    }

    test_count_by_language
}

fn describe_reason(graph: &Graph, reason: &Reason) -> Vec<String> {
    let hop_descriptions = reason.hops.iter().map(|hop| describe_hop(graph, hop));

    std::iter::once(reason.change.to_string())
        .chain(hop_descriptions)
        .collect()
}

fn describe_hop(graph: &Graph, hop: &Hop) -> String {
    let dependent = graph.path(hop.dependent);

    match hop.edge {
        EdgeKind::Import => format!("-> imported by {dependent}"),
        EdgeKind::TypeOnly => format!("-> type-imported by {dependent}"),
    }
}

fn join_messages(mode_reasons: &[ModeReason]) -> String {
    let messages: Vec<String> = mode_reasons.iter().map(ModeReason::message).collect();

    messages.join("; ")
}

fn rounded_ratio(selected: usize, total: usize) -> f64 {
    if total == 0 {
        return 0.0;
    }

    let exact_ratio = selected as f64 / total as f64;

    (exact_ratio * 10_000.0).round() / 10_000.0
}

fn format_percentage(selected: usize, total: usize) -> String {
    let percentage = if total == 0 {
        0.0
    } else {
        selected as f64 * 100.0 / total as f64
    };

    format!("{percentage:.1}%")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::ChangeKind;
    use crate::config::Config;
    use crate::graph::{Edge, GraphBuilder};
    use crate::select::select_tests;

    const CONFIG_HASH: &str = "0123456789abcdef";

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn modified(path: &str) -> FileChange {
        FileChange {
            path: repo_path(path),
            kind: ChangeKind::Modified,
        }
    }

    fn polyglot_graph() -> Graph {
        let mut builder = GraphBuilder::default();
        let a_test = builder.add_test_file(repo_path("src/a.test.ts"), "typescript");
        let b_test = builder.add_test_file(repo_path("src/b.test.ts"), "typescript");
        let a = builder.add_file(repo_path("src/a.ts"));
        let b = builder.add_file(repo_path("src/b.ts"));
        let imports = [(a_test, a), (b, a), (b_test, b)];

        builder.add_test_file(repo_path("tests/test_api.py"), "python");

        for (dependent, dependency) in imports {
            builder.add_edge(Edge {
                dependent,
                dependency,
                kind: EdgeKind::Import,
            });
        }

        builder.build()
    }

    fn report_for_changes(graph: &Graph, changes: &[FileChange]) -> Report {
        let selected_tests = select_tests(graph, changes);

        Report::from_selection(graph, changes, &selected_tests, CONFIG_HASH.to_owned())
    }

    fn report_running_all(graph: &Graph) -> Report {
        let config = Config::parse("[always_all]\nfiles = [\"ci/**\"]\n").unwrap();
        let changes = [
            modified("ci/lint.sh"),
            modified("src/a.ts"),
            modified("ci/deploy.sh"),
        ];
        let mode_reasons = config
            .always_all
            .matching_changes(&changes)
            .into_iter()
            .map(ModeReason::from)
            .collect();

        Report::all(graph, mode_reasons, CONFIG_HASH.to_owned())
    }

    #[test]
    fn selected_report_renders_targets_by_language_with_reasons_distances_and_stats() {
        let graph = polyglot_graph();
        let report = report_for_changes(&graph, &[modified("src/a.ts")]);

        assert_eq!(
            report.to_json(),
            r#"{
  "schema_version": 1,
  "mode": "selected",
  "mode_reasons": null,
  "targets": {
    "typescript": [
      "src/a.test.ts",
      "src/b.test.ts"
    ]
  },
  "reasons": {
    "src/a.test.ts": [
      "src/a.ts (modified)",
      "-> imported by src/a.test.ts"
    ],
    "src/b.test.ts": [
      "src/a.ts (modified)",
      "-> imported by src/b.ts",
      "-> imported by src/b.test.ts"
    ]
  },
  "distances": {
    "src/a.test.ts": 1,
    "src/b.test.ts": 2
  },
  "stats": {
    "tests_total": 3,
    "tests_selected": 2,
    "selected_ratio": 0.6667,
    "by_language": {
      "python": {
        "total": 1,
        "selected": 0,
        "ratio": 0.0
      },
      "typescript": {
        "total": 2,
        "selected": 2,
        "ratio": 1.0
      }
    },
    "config_hash": "0123456789abcdef"
  }
}
"#
        );
    }

    #[test]
    fn all_report_lists_every_test_file_and_every_always_all_cause() {
        let graph = polyglot_graph();
        let report = report_running_all(&graph);

        assert_eq!(
            report.to_json(),
            r#"{
  "schema_version": 1,
  "mode": "all",
  "mode_reasons": [
    {
      "kind": "always_all",
      "file": "ci/deploy.sh",
      "pattern": "ci/**",
      "message": "ci/deploy.sh (modified) matches always_all \"ci/**\""
    },
    {
      "kind": "always_all",
      "file": "ci/lint.sh",
      "pattern": "ci/**",
      "message": "ci/lint.sh (modified) matches always_all \"ci/**\""
    }
  ],
  "targets": {
    "python": [
      "tests/test_api.py"
    ],
    "typescript": [
      "src/a.test.ts",
      "src/b.test.ts"
    ]
  },
  "reasons": {},
  "distances": {},
  "stats": {
    "tests_total": 3,
    "tests_selected": 3,
    "selected_ratio": 1.0,
    "by_language": {
      "python": {
        "total": 1,
        "selected": 1,
        "ratio": 1.0
      },
      "typescript": {
        "total": 2,
        "selected": 2,
        "ratio": 1.0
      }
    },
    "config_hash": "0123456789abcdef"
  }
}
"#
        );
    }

    #[test]
    fn report_without_impacted_tests_is_in_mode_none_with_a_no_impact_reason() {
        let graph = polyglot_graph();
        let report = report_for_changes(&graph, &[modified("README.md")]);

        assert_eq!(
            report.to_json(),
            r#"{
  "schema_version": 1,
  "mode": "none",
  "mode_reasons": [
    {
      "kind": "no_impact",
      "message": "1 changed file reaches no test file"
    }
  ],
  "targets": {},
  "reasons": {},
  "distances": {},
  "stats": {
    "tests_total": 3,
    "tests_selected": 0,
    "selected_ratio": 0.0,
    "by_language": {
      "python": {
        "total": 1,
        "selected": 0,
        "ratio": 0.0
      },
      "typescript": {
        "total": 2,
        "selected": 0,
        "ratio": 0.0
      }
    },
    "config_hash": "0123456789abcdef"
  }
}
"#
        );
    }

    #[test]
    fn no_impact_message_counts_the_changed_files() {
        let messages: Vec<String> = [0, 1, 3]
            .into_iter()
            .map(|changed_file_count| ModeReason::NoImpact { changed_file_count }.message())
            .collect();

        assert_eq!(
            messages,
            [
                "no file changed",
                "1 changed file reaches no test file",
                "3 changed files reach no test file",
            ]
        );
    }

    #[test]
    fn list_contains_exactly_the_tests_to_run_in_every_mode() {
        let graph = polyglot_graph();
        let selected_report = report_for_changes(&graph, &[modified("src/b.ts")]);
        let all_report = report_running_all(&graph);
        let none_report = report_for_changes(&graph, &[modified("README.md")]);

        assert_eq!(
            selected_report.to_list(ListSeparator::Newline),
            "src/b.test.ts\n"
        );
        assert_eq!(
            all_report.to_list(ListSeparator::Newline),
            "src/a.test.ts\nsrc/b.test.ts\ntests/test_api.py\n"
        );
        assert_eq!(none_report.to_list(ListSeparator::Newline), "");
    }

    #[test]
    fn list_follows_distance_order() {
        let graph = polyglot_graph();
        let report = report_for_changes(&graph, &[modified("src/a.ts"), modified("src/b.test.ts")]);

        assert_eq!(
            report.to_list(ListSeparator::Newline),
            "src/b.test.ts\nsrc/a.test.ts\n"
        );
    }

    #[test]
    fn nul_separated_list_terminates_each_path_with_nul() {
        let graph = polyglot_graph();
        let report = report_running_all(&graph);

        assert_eq!(
            report.to_list(ListSeparator::Nul),
            "src/a.test.ts\0src/b.test.ts\0tests/test_api.py\0"
        );
    }

    #[test]
    fn summary_line_states_the_mode_with_its_reasons_or_the_selected_share() {
        let graph = polyglot_graph();
        let selected_report = report_for_changes(&graph, &[modified("src/b.ts")]);
        let all_report = report_running_all(&graph);
        let none_report = report_for_changes(&graph, &[modified("README.md")]);

        assert_eq!(
            selected_report.summary_line(),
            "prober: selected 1/3 test files (33.3%)"
        );
        assert_eq!(
            all_report.summary_line(),
            r#"prober: all 3 test files — ci/deploy.sh (modified) matches always_all "ci/**"; ci/lint.sh (modified) matches always_all "ci/**""#
        );
        assert_eq!(
            none_report.summary_line(),
            "prober: none of 3 test files — 1 changed file reaches no test file"
        );
    }

    #[test]
    fn ratios_are_rounded_to_four_decimals_and_zero_without_tests() {
        assert_eq!(rounded_ratio(246, 19_104), 0.0129);
        assert_eq!(rounded_ratio(0, 0), 0.0);
    }
}
