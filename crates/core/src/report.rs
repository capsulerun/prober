use std::cmp::Reverse;
use std::collections::BTreeMap;

use serde::Serialize;

use crate::changes::{FileChange, ResolvedRevisions};
use crate::config::AlwaysAllMatch;
use crate::files::{FileId, RepoPath};
use crate::graph::{Edge, EdgeKind, Graph, TestFile};
use crate::ratios::{format_percentage, rounded_ratio};
use crate::select::{Hop, Link, Reason, SelectedTest};

const SCHEMA_VERSION: u32 = 1;

const UNRESOLVED_DYNAMIC_IMPORT: &str = "unresolved-dynamic-import";

pub(crate) const WIDENED_HOP_PREFIX: &str = "-> widened: ";

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
    UnparsableManifest {
        change: FileChange,
        message: String,
    },
    GlobalField {
        change: FileChange,
        field: String,
    },
    RootScope {
        change: FileChange,
        configures: bool,
    },
    RootScopePackage {
        change: FileChange,
        package: String,
        detail: String,
        other_packages: usize,
    },
    NoImpact {
        changed_file_count: usize,
    },
    Error {
        message: String,
    },
}

impl ModeReason {
    pub fn kind(&self) -> &'static str {
        match self {
            ModeReason::AlwaysAll { .. } => "always_all",
            ModeReason::UnparsableManifest { .. } => "unparsable_manifest",
            ModeReason::GlobalField { .. } => "global_field",
            ModeReason::RootScope { .. } | ModeReason::RootScopePackage { .. } => "root_scope",
            ModeReason::NoImpact { .. } => "no_impact",
            ModeReason::Error { .. } => "error",
        }
    }

    pub fn message(&self) -> String {
        match self {
            ModeReason::AlwaysAll {
                change, pattern, ..
            } => format!("{change} matches always_all {pattern:?}"),
            ModeReason::UnparsableManifest { change, message } => {
                format!("{change} cannot be diffed: {message}")
            }
            ModeReason::GlobalField { change, field } => {
                format!("{change} changes {field:?}, which may affect every test")
            }
            ModeReason::RootScope {
                change,
                configures: true,
            } => format!("{change} configures the test runner of every test"),
            ModeReason::RootScope {
                change,
                configures: false,
            } => format!("{change} is loaded before every test"),
            ModeReason::RootScopePackage {
                change,
                package,
                detail,
                other_packages: 0,
            } => format!("{change} changes package {package} ({detail}), which every test loads"),
            ModeReason::RootScopePackage {
                change,
                package,
                detail,
                other_packages,
            } => format!(
                "{change} changes package {package} ({detail}) and {other_packages} more that every test loads"
            ),
            ModeReason::NoImpact {
                changed_file_count: 0,
            } => "no file changed".to_owned(),
            ModeReason::NoImpact {
                changed_file_count: 1,
            } => "1 changed file reaches no test file".to_owned(),
            ModeReason::NoImpact { changed_file_count } => {
                format!("{changed_file_count} changed files reach no test file")
            }
            ModeReason::Error { message } => message.clone(),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WidenedSite {
    pub site: String,
    pub cause: &'static str,
    pub added: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListSeparator {
    Newline,
    Nul,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub revisions: Option<ResolvedRevisions>,
    pub mode: Mode,
    pub targets: Vec<Target>,
    pub widened: Vec<WidenedSite>,
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
            revisions: None,
            mode,
            targets,
            widened: widened_sites(graph, selected_tests),
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
            revisions: None,
            mode: Mode::All { reasons },
            targets,
            widened: Vec::new(),
            test_count_by_language: count_by_language(&test_files),
            config_hash,
        }
    }

    pub fn with_revisions(self, revisions: ResolvedRevisions) -> Self {
        Self {
            revisions: Some(revisions),
            ..self
        }
    }

    pub fn test_count(&self) -> usize {
        self.test_count_by_language.values().sum()
    }

    pub fn tests_added_by_widening(&self) -> usize {
        self.widened
            .iter()
            .map(|widened_site| widened_site.added)
            .sum()
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
            revisions: self.revisions.as_ref(),
            mode: self.mode.name(),
            mode_reasons,
            targets,
            reasons,
            distances,
            widened: self.widened.iter().map(JsonWidenedSite::new).collect(),
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
            Mode::Selected if self.widened.is_empty() => format!(
                "prober: selected {selected_count}/{test_count} test files ({})",
                format_percentage(selected_count as f64, test_count as f64)
            ),
            Mode::Selected => format!(
                "prober: selected {selected_count}/{test_count} test files ({}), {} added by widening",
                format_percentage(selected_count as f64, test_count as f64),
                self.tests_added_by_widening()
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
                    ratio: rounded_ratio(selected as f64, total as f64),
                };

                (language, language_stats)
            })
            .collect();

        JsonStats {
            tests_total: self.test_count(),
            tests_selected: self.targets.len(),
            selected_ratio: rounded_ratio(self.targets.len() as f64, self.test_count() as f64),
            tests_added_by_widening: self.tests_added_by_widening(),
            by_language,
            config_hash: &self.config_hash,
        }
    }
}

#[derive(Serialize)]
struct JsonReport<'a> {
    schema_version: u32,
    revisions: Option<&'a ResolvedRevisions>,
    mode: &'static str,
    mode_reasons: Option<Vec<JsonModeReason<'a>>>,
    targets: BTreeMap<&'static str, Vec<&'a str>>,
    reasons: BTreeMap<&'a str, &'a [String]>,
    distances: BTreeMap<&'a str, usize>,
    widened: Vec<JsonWidenedSite<'a>>,
    stats: JsonStats<'a>,
}

#[derive(Serialize)]
struct JsonWidenedSite<'a> {
    site: &'a str,
    kind: &'static str,
    cause: &'static str,
    added: usize,
}

impl<'a> JsonWidenedSite<'a> {
    fn new(widened_site: &'a WidenedSite) -> Self {
        Self {
            site: &widened_site.site,
            kind: UNRESOLVED_DYNAMIC_IMPORT,
            cause: widened_site.cause,
            added: widened_site.added,
        }
    }
}

#[derive(Default, Serialize)]
struct JsonModeReason<'a> {
    kind: &'static str,

    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<&'a str>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pattern: Option<&'a str>,

    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<&'a str>,

    #[serde(skip_serializing_if = "Option::is_none")]
    package: Option<&'a str>,

    message: String,
}

impl<'a> JsonModeReason<'a> {
    fn new(mode_reason: &'a ModeReason) -> Self {
        let json_mode_reason = Self {
            kind: mode_reason.kind(),
            message: mode_reason.message(),
            ..Self::default()
        };

        match mode_reason {
            ModeReason::AlwaysAll {
                matched_path,
                pattern,
                ..
            } => Self {
                file: Some(matched_path.as_str()),
                pattern: Some(pattern),
                ..json_mode_reason
            },
            ModeReason::UnparsableManifest { change, .. }
            | ModeReason::RootScope { change, .. } => Self {
                file: Some(change.path.as_str()),
                ..json_mode_reason
            },
            ModeReason::GlobalField { change, field } => Self {
                file: Some(change.path.as_str()),
                field: Some(field),
                ..json_mode_reason
            },
            ModeReason::RootScopePackage {
                change, package, ..
            } => Self {
                file: Some(change.path.as_str()),
                package: Some(package),
                ..json_mode_reason
            },
            ModeReason::NoImpact { .. } | ModeReason::Error { .. } => json_mode_reason,
        }
    }
}

#[derive(Serialize)]
struct JsonStats<'a> {
    tests_total: usize,
    tests_selected: usize,
    selected_ratio: f64,
    tests_added_by_widening: usize,
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

    match hop.link {
        Link::Edge {
            kind: EdgeKind::Import,
            ..
        } => format!("-> imported by {dependent}"),
        Link::Edge {
            kind: EdgeKind::TypeOnly,
            ..
        } => format!("-> type-imported by {dependent}"),
        Link::Edge {
            kind: EdgeKind::Contains,
            ..
        } => format!("-> in folder {dependent}/"),
        Link::Edge {
            kind: EdgeKind::Asset,
            ..
        } => format!("-> imported as an asset by {dependent}"),
        Link::Edge {
            kind: EdgeKind::Family,
            ..
        } => format!("-> paired by name with {dependent}"),
        Link::Edge {
            kind: kind @ (EdgeKind::Reads | EdgeKind::Lists | EdgeKind::Dynamic),
            dependency,
        } => {
            let verb = match kind {
                EdgeKind::Reads => "read",
                EdgeKind::Lists => "listed",
                _ => "loaded",
            };
            let edge = Edge {
                dependent: hop.dependent,
                dependency,
                kind,
            };

            match graph.inference(&edge) {
                Some(inference) => format!(
                    "-> {verb} by {dependent}:{} ({})",
                    inference.line,
                    inference.description()
                ),
                None => format!("-> {verb} by {dependent}"),
            }
        }
        Link::ChangedPart(part_index) => {
            format!("-> {}", graph.changed_parts()[part_index].description)
        }
        Link::UnresolvedSite(site_index) => {
            let unresolved_site = &graph.unresolved_sites()[site_index];

            format!(
                "{WIDENED_HOP_PREFIX}unresolved dynamic import at {dependent}:{} ({})",
                unresolved_site.line, unresolved_site.expression
            )
        }
    }
}

fn widened_sites(graph: &Graph, selected_tests: &[SelectedTest]) -> Vec<WidenedSite> {
    let mut added_by_site: BTreeMap<usize, usize> = BTreeMap::new();

    for site_index in selected_tests
        .iter()
        .filter_map(|selected_test| selected_test.reason.widening_site())
    {
        *added_by_site.entry(site_index).or_default() += 1;
    }

    let mut widened_sites: Vec<WidenedSite> = added_by_site
        .into_iter()
        .map(|(site_index, added)| {
            let unresolved_site = &graph.unresolved_sites()[site_index];

            WidenedSite {
                site: format!(
                    "{}:{}",
                    graph.path(unresolved_site.file),
                    unresolved_site.line
                ),
                cause: unresolved_site.cause.key(),
                added,
            }
        })
        .collect();

    widened_sites.sort_by_key(|widened_site| Reverse(widened_site.added));

    widened_sites
}

fn join_messages(mode_reasons: &[ModeReason]) -> String {
    let messages: Vec<String> = mode_reasons.iter().map(ModeReason::message).collect();

    messages.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::ChangeKind;
    use crate::config::Config;
    use crate::graph::{Edge, GraphBuilder, Inference, UnresolvedCause, UnresolvedSite};
    use crate::select::{Widening, select_tests};

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

    fn plugin_loader_graph() -> Graph {
        let mut builder = GraphBuilder::default();
        let loader_test = builder.add_test_file(repo_path("src/loader.test.ts"), "typescript");
        let loader = builder.add_file(repo_path("src/loader.ts"));
        let plugin = builder.add_file(repo_path("src/plugins/a.ts"));

        builder.add_edge(Edge {
            dependent: loader_test,
            dependency: loader,
            kind: EdgeKind::Import,
        });
        builder.add_inferred_edge(
            Edge {
                dependent: loader,
                dependency: plugin,
                kind: EdgeKind::Dynamic,
            },
            Inference {
                line: 7,
                kind: "glob",
                pattern: "src/plugins/*.ts".to_owned(),
                detail: None,
            },
        );

        builder.build()
    }

    fn report_for_changes(graph: &Graph, changes: &[FileChange]) -> Report {
        let selected_tests = select_tests(graph, changes, Widening::Scoped);

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
  "revisions": null,
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
  "widened": [],
  "stats": {
    "tests_total": 3,
    "tests_selected": 2,
    "selected_ratio": 0.6667,
    "tests_added_by_widening": 0,
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
    fn dynamic_hop_names_the_loading_site_and_its_inference() {
        let graph = plugin_loader_graph();
        let report = report_for_changes(&graph, &[modified("src/plugins/a.ts")]);
        let json_report: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();

        assert_eq!(
            json_report["reasons"]["src/loader.test.ts"],
            serde_json::json!([
                "src/plugins/a.ts (modified)",
                "-> loaded by src/loader.ts:7 (inferred glob src/plugins/*.ts)",
                "-> imported by src/loader.test.ts",
            ])
        );
        assert_eq!(json_report["distances"]["src/loader.test.ts"], 2);
    }

    #[test]
    fn read_listed_and_named_hops_say_how_the_file_is_used() {
        let mut builder = GraphBuilder::default();
        let discovery_test =
            builder.add_test_file(repo_path("src/discovery.test.ts"), "typescript");
        let index_e2e_test =
            builder.add_test_file(repo_path("extensions/a/index.e2e.test.ts"), "typescript");
        let discovery = builder.add_file(repo_path("src/discovery.ts"));
        let manifest = builder.add_file(repo_path("extensions/a/plugin.json"));
        let index = builder.add_file(repo_path("extensions/a/index.ts"));
        let inference = |line: u32, kind: &'static str, pattern: &str| Inference {
            line,
            kind,
            pattern: pattern.to_owned(),
            detail: None,
        };

        builder.add_edge(Edge {
            dependent: discovery_test,
            dependency: discovery,
            kind: EdgeKind::Import,
        });
        builder.add_edge(Edge {
            dependent: index_e2e_test,
            dependency: index,
            kind: EdgeKind::Family,
        });
        builder.add_inferred_edge(
            Edge {
                dependent: discovery,
                dependency: manifest,
                kind: EdgeKind::Reads,
            },
            inference(9, "file read", "extensions/*/plugin.json"),
        );
        builder.add_inferred_edge(
            Edge {
                dependent: manifest,
                dependency: index,
                kind: EdgeKind::Lists,
            },
            inference(1, "listed path", "./index.ts"),
        );

        let graph = builder.build();
        let report = report_for_changes(&graph, &[modified("extensions/a/index.ts")]);
        let json_report: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();

        assert_eq!(
            json_report["reasons"]["src/discovery.test.ts"],
            serde_json::json!([
                "extensions/a/index.ts (modified)",
                "-> listed by extensions/a/plugin.json:1 (inferred listed path ./index.ts)",
                "-> read by src/discovery.ts:9 (inferred file read extensions/*/plugin.json)",
                "-> imported by src/discovery.test.ts",
            ])
        );
        assert_eq!(
            json_report["reasons"]["extensions/a/index.e2e.test.ts"],
            serde_json::json!([
                "extensions/a/index.ts (modified)",
                "-> paired by name with extensions/a/index.e2e.test.ts",
            ])
        );
    }

    #[test]
    fn changed_part_hop_says_what_changed_in_the_file() {
        let mut builder = GraphBuilder::default();
        let a_test = builder.add_test_file(repo_path("src/a.test.ts"), "typescript");
        let lodash = builder.add_file(repo_path("node_modules/lodash"));

        builder.add_edge(Edge {
            dependent: a_test,
            dependency: lodash,
            kind: EdgeKind::Import,
        });
        builder.add_changed_part(
            repo_path("pnpm-lock.yaml"),
            repo_path("node_modules/lodash"),
            "package lodash changed (4.17.20 -> 4.17.21)".to_owned(),
        );

        let graph = builder.build();
        let report = report_for_changes(&graph, &[modified("pnpm-lock.yaml")]);
        let json_report: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();

        assert_eq!(
            json_report["reasons"]["src/a.test.ts"],
            serde_json::json!([
                "pnpm-lock.yaml (modified)",
                "-> package lodash changed (4.17.20 -> 4.17.21)",
                "-> imported by src/a.test.ts",
            ])
        );
        assert_eq!(json_report["distances"]["src/a.test.ts"], 2);
    }

    #[test]
    fn asset_hop_says_the_file_is_imported_as_an_asset() {
        let mut builder = GraphBuilder::default();
        let styles = builder.add_file(repo_path("src/button.css"));
        let button = builder.add_file(repo_path("src/button.ts"));
        let button_test = builder.add_test_file(repo_path("src/button.test.ts"), "typescript");
        let edges = [
            (button, styles, EdgeKind::Asset),
            (button_test, button, EdgeKind::Import),
        ];

        for (dependent, dependency, kind) in edges {
            builder.add_edge(Edge {
                dependent,
                dependency,
                kind,
            });
        }

        let graph = builder.build();
        let report = report_for_changes(&graph, &[modified("src/button.css")]);
        let json_report: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();

        assert_eq!(
            json_report["reasons"]["src/button.test.ts"],
            serde_json::json!([
                "src/button.css (modified)",
                "-> imported as an asset by src/button.ts",
                "-> imported by src/button.test.ts",
            ])
        );
    }

    #[test]
    fn manifest_reasons_name_the_file_and_the_field_or_package_at_fault() {
        let graph = polyglot_graph();
        let manifest_reasons = vec![
            ModeReason::UnparsableManifest {
                change: modified("pnpm-lock.yaml"),
                message: "lockfileVersion 5.4 is not supported".to_owned(),
            },
            ModeReason::GlobalField {
                change: modified("tsconfig.json"),
                field: "compilerOptions.target".to_owned(),
            },
            ModeReason::RootScope {
                change: modified("test/setup.ts"),
                configures: false,
            },
            ModeReason::RootScope {
                change: modified("vitest.config.ts"),
                configures: true,
            },
            ModeReason::RootScopePackage {
                change: modified("package.json"),
                package: "vitest".to_owned(),
                detail: "^3.2.0 -> ^3.2.1".to_owned(),
                other_packages: 0,
            },
            ModeReason::RootScopePackage {
                change: modified("yarn.lock"),
                package: "vite".to_owned(),
                detail: "5.0.0 -> 5.0.1".to_owned(),
                other_packages: 2,
            },
        ];
        let report = Report::all(&graph, manifest_reasons, CONFIG_HASH.to_owned());
        let json_report: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();

        assert_eq!(
            json_report["mode_reasons"],
            serde_json::json!([
                {
                    "kind": "unparsable_manifest",
                    "file": "pnpm-lock.yaml",
                    "message": "pnpm-lock.yaml (modified) cannot be diffed: lockfileVersion 5.4 is not supported",
                },
                {
                    "kind": "global_field",
                    "file": "tsconfig.json",
                    "field": "compilerOptions.target",
                    "message": "tsconfig.json (modified) changes \"compilerOptions.target\", which may affect every test",
                },
                {
                    "kind": "root_scope",
                    "file": "test/setup.ts",
                    "message": "test/setup.ts (modified) is loaded before every test",
                },
                {
                    "kind": "root_scope",
                    "file": "vitest.config.ts",
                    "message": "vitest.config.ts (modified) configures the test runner of every test",
                },
                {
                    "kind": "root_scope",
                    "file": "package.json",
                    "package": "vitest",
                    "message": "package.json (modified) changes package vitest (^3.2.0 -> ^3.2.1), which every test loads",
                },
                {
                    "kind": "root_scope",
                    "file": "yarn.lock",
                    "package": "vite",
                    "message": "yarn.lock (modified) changes package vite (5.0.0 -> 5.0.1) and 2 more that every test loads",
                },
            ])
        );
    }

    #[test]
    fn widened_tests_are_counted_per_unresolved_site_and_explained() {
        let mut builder = GraphBuilder::default();
        let worker_test = builder.add_test_file(repo_path("src/worker.test.ts"), "typescript");
        let pool_test = builder.add_test_file(repo_path("src/pool.test.ts"), "typescript");
        let worker = builder.add_file(repo_path("src/worker.ts"));

        builder.add_language("typescript", |path| path.as_str().ends_with(".ts"));
        builder.add_edge(Edge {
            dependent: worker_test,
            dependency: worker,
            kind: EdgeKind::Import,
        });
        builder.add_edge(Edge {
            dependent: pool_test,
            dependency: worker,
            kind: EdgeKind::Import,
        });
        builder.add_unresolved_site(UnresolvedSite {
            file: worker,
            line: 313,
            expression: "request.moduleUrl".to_owned(),
            cause: UnresolvedCause::Parameter,
            language: "typescript",
        });

        let graph = builder.build();
        let report = report_for_changes(&graph, &[modified("src/a.ts")]);
        let json_report: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();

        assert_eq!(
            json_report["widened"],
            serde_json::json!([{
                "site": "src/worker.ts:313",
                "kind": "unresolved-dynamic-import",
                "cause": "parameter",
                "added": 2,
            }])
        );
        assert_eq!(json_report["stats"]["tests_added_by_widening"], 2);
        assert_eq!(
            json_report["reasons"]["src/pool.test.ts"],
            serde_json::json!([
                "src/a.ts (modified)",
                "-> widened: unresolved dynamic import at src/worker.ts:313 (request.moduleUrl)",
                "-> imported by src/pool.test.ts",
            ])
        );
        assert_eq!(json_report["distances"]["src/pool.test.ts"], 2);
        assert_eq!(
            report.summary_line(),
            "prober: selected 2/2 test files (100.0%), 2 added by widening"
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
  "revisions": null,
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
  "widened": [],
  "stats": {
    "tests_total": 3,
    "tests_selected": 3,
    "selected_ratio": 1.0,
    "tests_added_by_widening": 0,
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
  "revisions": null,
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
  "widened": [],
  "stats": {
    "tests_total": 3,
    "tests_selected": 0,
    "selected_ratio": 0.0,
    "tests_added_by_widening": 0,
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
    fn config_error_runs_all_tests_with_an_error_reason() {
        let graph = polyglot_graph();
        let config_error = Config::parse("anchors = true\n").unwrap_err();
        let error_reason = ModeReason::Error {
            message: config_error.to_string(),
        };
        let report = Report::all(&graph, vec![error_reason], CONFIG_HASH.to_owned());
        let json_report: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();

        assert_eq!(json_report["mode"], "all");
        assert_eq!(
            json_report["mode_reasons"],
            serde_json::json!([{
                "kind": "error",
                "message": "invalid prober.toml at line 1, column 1: unknown field `anchors`, expected one of `presets`, `always_all`, `inference`",
            }])
        );
        assert_eq!(
            report.summary_line(),
            "prober: all 3 test files — invalid prober.toml at line 1, column 1: unknown field `anchors`, expected one of `presets`, `always_all`, `inference`"
        );
    }
}
