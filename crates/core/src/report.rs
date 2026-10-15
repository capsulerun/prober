use std::cmp::Reverse;
use std::collections::BTreeMap;

use serde::Serialize;

use crate::aggressive::DroppedTest;
use crate::changes::{FileChange, ResolvedRevisions};
use crate::config::AlwaysAllMatch;
use crate::files::{FileId, RepoPath};
use crate::graph::{Edge, EdgeKind, Graph, TargetName, TestFile};
use crate::patterns::describe_captures;
use crate::ratios::{format_percentage, rounded_ratio};
use crate::select::{Hop, Link, Reason, SelectedTest};
use crate::symbols::{
    MODULE_INIT_NAME, SymbolChange, SymbolChangeKind, SymbolEdge, SymbolKind, SymbolTarget,
    WholeModuleCause,
};

const SCHEMA_VERSION: u32 = 1;

const UNRESOLVED_DYNAMIC_IMPORT: &str = "unresolved-dynamic-import";

pub const SELECTION_VARIABLE: &str = "PROBER_SELECTION";

pub const CONFIG_FILE_NAME: &str = "prober.toml";

const LISTED_SUMMARY_ITEMS: usize = 5;

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
    Forced,
    NoImpact {
        changed_file_count: usize,
    },
    OnlyUnhandled {
        changed_file_count: usize,
    },
    Dropped {
        test_count: usize,
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
            ModeReason::Forced => "forced",
            ModeReason::NoImpact { .. } => "no_impact",
            ModeReason::OnlyUnhandled { .. } => "only_unhandled",
            ModeReason::Dropped { .. } => "dropped",
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
            ModeReason::Forced => format!("forced by {SELECTION_VARIABLE}=all"),
            ModeReason::OnlyUnhandled {
                changed_file_count: 1,
            } => "1 changed file is in a language Prober does not analyse".to_owned(),
            ModeReason::OnlyUnhandled { changed_file_count } => format!(
                "{changed_file_count} changed files are in languages Prober does not analyse"
            ),
            ModeReason::Dropped { test_count: 1 } => {
                "the 1 test file the changes reach is dropped by aggressive options".to_owned()
            }
            ModeReason::Dropped { test_count } => format!(
                "the {test_count} test files the changes reach are dropped by aggressive options"
            ),
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
    pub distance: Option<usize>,
    pub hub_fanout: Option<usize>,
    pub reason: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlwaysRunTest {
    pub path: RepoPath,
    pub language: &'static str,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub path: RepoPath,
    pub language: &'static str,
    pub name: Option<TargetName>,
    pub impact: Option<Impact>,
}

impl Target {
    pub fn key(&self) -> &str {
        match &self.name {
            Some(name) => &name.name,
            None => self.path.as_str(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedTarget {
    pub target: Target,
    pub option: &'static str,
    pub description: String,
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
    pub dropped: Vec<DroppedTarget>,
    pub unhandled: Vec<RepoPath>,
    pub always_run_count: usize,
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
            .map(|selected_test| selected_target(graph, selected_test))
            .collect();

        Self {
            revisions: None,
            mode,
            targets,
            widened: widened_sites(graph, selected_tests),
            dropped: Vec::new(),
            unhandled: Vec::new(),
            always_run_count: 0,
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
                name: graph.target_name(test_file.file).cloned(),
                impact: None,
            })
            .collect();

        Self {
            revisions: None,
            mode: Mode::All { reasons },
            targets,
            widened: Vec::new(),
            dropped: Vec::new(),
            unhandled: Vec::new(),
            always_run_count: 0,
            test_count_by_language: count_by_language(&test_files),
            config_hash,
        }
    }

    pub fn with_dropped(mut self, graph: &Graph, dropped_tests: &[DroppedTest]) -> Self {
        if dropped_tests.is_empty() {
            return self;
        }

        if let Mode::None { reasons } = &mut self.mode {
            *reasons = vec![ModeReason::Dropped {
                test_count: dropped_tests.len(),
            }];
        }

        self.dropped = dropped_tests
            .iter()
            .map(|dropped_test| DroppedTarget {
                target: selected_target(graph, &dropped_test.selected_test),
                option: dropped_test.cause.option(),
                description: dropped_test.cause.description(),
            })
            .collect();

        self
    }

    pub fn with_always_run(mut self, always_run_tests: Vec<AlwaysRunTest>) -> Self {
        let all_dropped = matches!(self.mode.reasons(), Some([ModeReason::Dropped { .. }]));

        if self.mode != Mode::Selected && !all_dropped {
            return self;
        }

        for always_run_test in always_run_tests {
            if self
                .targets
                .iter()
                .any(|target| target.path == always_run_test.path)
            {
                continue;
            }

            self.dropped
                .retain(|dropped| dropped.target.path != always_run_test.path);
            self.targets.push(Target {
                path: always_run_test.path,
                language: always_run_test.language,
                name: None,
                impact: Some(Impact {
                    distance: None,
                    hub_fanout: None,
                    reason: vec![always_run_test.reason],
                }),
            });
            self.always_run_count += 1;
            self.mode = Mode::Selected;
        }

        self
    }

    pub fn with_unhandled(mut self, unhandled: Vec<RepoPath>, changed_file_count: usize) -> Self {
        let only_unhandled = !unhandled.is_empty() && unhandled.len() == changed_file_count;

        if let Mode::None { reasons } = &mut self.mode
            && only_unhandled
        {
            *reasons = vec![ModeReason::OnlyUnhandled { changed_file_count }];
        }

        self.unhandled = unhandled;

        self
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
        let mut targets: BTreeMap<&str, Vec<JsonTarget>> = BTreeMap::new();
        let mut reasons = BTreeMap::new();
        let mut distances = BTreeMap::new();
        let mut hub_fanouts = BTreeMap::new();

        for target in &self.targets {
            let path = target.key();
            let json_target = match &target.name {
                Some(name) => JsonTarget::Named(BTreeMap::from([(name.unit, name.name.as_str())])),
                None => JsonTarget::Path(path),
            };

            targets
                .entry(target.language)
                .or_default()
                .push(json_target);

            if let Some(impact) = &target.impact {
                reasons.insert(path, impact.reason.as_slice());

                if let Some(distance) = impact.distance {
                    distances.insert(path, distance);
                }

                if let Some(hub_fanout) = impact.hub_fanout {
                    hub_fanouts.insert(path, hub_fanout);
                }
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
            hub_fanouts,
            widened: self.widened.iter().map(JsonWidenedSite::new).collect(),
            dropped: self.dropped.iter().map(JsonDroppedTarget::new).collect(),
            unhandled: self.unhandled.iter().map(RepoPath::as_str).collect(),
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
            list.push_str(target.key());
            list.push(separator);
        }

        list
    }

    pub fn to_language_list(&self, language: &str) -> String {
        let mut list = String::new();

        for target in self
            .targets
            .iter()
            .filter(|target| target.language == language)
        {
            list.push_str(target.key());
            list.push('\n');
        }

        list
    }

    pub fn to_markdown(&self) -> String {
        let test_count = self.test_count();
        let heading = match &self.mode {
            Mode::Selected => format!(
                "selected {}/{test_count} test files ({})",
                self.targets.len(),
                format_percentage(self.targets.len() as f64, test_count as f64)
            ),
            Mode::All { .. } => format!("all {test_count} test files"),
            Mode::None { .. } => format!("none of {test_count} test files"),
        };
        let mut markdown = format!("### Prober: {heading}\n");

        if let Some(reasons) = self.mode.reasons() {
            markdown.push('\n');

            for reason in reasons {
                markdown.push_str(&format!("- {}\n", reason.message()));
            }
        }

        let mut selected_count_by_language: BTreeMap<&str, usize> = BTreeMap::new();

        for target in &self.targets {
            *selected_count_by_language
                .entry(target.language)
                .or_default() += 1;
        }

        if self.test_count_by_language.len() > 1 {
            markdown.push_str("\n| Language | Selected | Test files |\n| --- | --- | --- |\n");

            for (language, total) in &self.test_count_by_language {
                let selected = selected_count_by_language
                    .get(language)
                    .copied()
                    .unwrap_or_default();

                markdown.push_str(&format!("| {language} | {selected} | {total} |\n"));
            }
        }

        let mut test_count_by_change: BTreeMap<&str, usize> = BTreeMap::new();

        for change in self
            .targets
            .iter()
            .filter_map(|target| target.impact.as_ref()?.reason.first())
        {
            *test_count_by_change.entry(change.as_str()).or_default() += 1;
        }

        if !test_count_by_change.is_empty() {
            let mut changes: Vec<(&str, usize)> = test_count_by_change.into_iter().collect();

            changes.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));
            markdown.push_str("\nChanges reaching the most test files:\n\n");

            for (change, count) in changes.iter().take(LISTED_SUMMARY_ITEMS) {
                markdown.push_str(&format!("- `{change}`: {}\n", test_file_count(*count)));
            }
        }

        if !self.widened.is_empty() {
            markdown.push_str(&format!(
                "\nAdded by widening, through sites that could not be resolved: {}\n\n",
                test_file_count(self.tests_added_by_widening())
            ));

            for widened_site in self.widened.iter().take(LISTED_SUMMARY_ITEMS) {
                markdown.push_str(&format!(
                    "- `{}` ({}): {}\n",
                    widened_site.site,
                    widened_site.cause,
                    test_file_count(widened_site.added)
                ));
            }
        }

        if !self.dropped.is_empty() {
            markdown.push_str(&format!(
                "\nDropped by aggressive options: {}\n\n",
                test_file_count(self.dropped.len())
            ));

            for dropped in self.dropped.iter().take(LISTED_SUMMARY_ITEMS) {
                markdown.push_str(&format!(
                    "- `{}`: {}\n",
                    dropped.target.key(),
                    dropped.description
                ));
            }
        }

        if self.always_run_count > 0 {
            markdown.push_str(&format!(
                "\nAlways run: {}\n",
                test_file_count(self.always_run_count)
            ));
        }

        markdown
    }

    pub fn summary_line(&self) -> String {
        let test_count = self.test_count();
        let selected_count = self.targets.len();

        match &self.mode {
            Mode::Selected => {
                let mut summary_line = format!(
                    "prober: selected {selected_count}/{test_count} test files ({})",
                    format_percentage(selected_count as f64, test_count as f64)
                );

                if !self.widened.is_empty() {
                    summary_line.push_str(&format!(
                        ", {} added by widening",
                        self.tests_added_by_widening()
                    ));
                }

                if !self.dropped.is_empty() {
                    summary_line.push_str(&format!(
                        ", {} dropped by aggressive options",
                        self.dropped.len()
                    ));
                }

                if self.always_run_count > 0 {
                    summary_line.push_str(&format!(", {} always run", self.always_run_count));
                }

                summary_line
            }
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
            tests_always_run: self.always_run_count,
            tests_dropped: self.dropped.len(),
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
    targets: BTreeMap<&'static str, Vec<JsonTarget<'a>>>,
    reasons: BTreeMap<&'a str, &'a [String]>,
    distances: BTreeMap<&'a str, usize>,
    hub_fanouts: BTreeMap<&'a str, usize>,
    widened: Vec<JsonWidenedSite<'a>>,
    dropped: Vec<JsonDroppedTarget<'a>>,
    unhandled: Vec<&'a str>,
    stats: JsonStats<'a>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum JsonTarget<'a> {
    Path(&'a str),
    Named(BTreeMap<&'static str, &'a str>),
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

#[derive(Serialize)]
struct JsonDroppedTarget<'a> {
    test: &'a str,
    option: &'static str,
    distance: Option<usize>,
    hub_fanout: Option<usize>,
}

impl<'a> JsonDroppedTarget<'a> {
    fn new(dropped: &'a DroppedTarget) -> Self {
        let impact = dropped.target.impact.as_ref();

        Self {
            test: dropped.target.key(),
            option: dropped.option,
            distance: impact.and_then(|impact| impact.distance),
            hub_fanout: impact.and_then(|impact| impact.hub_fanout),
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
            ModeReason::Forced
            | ModeReason::NoImpact { .. }
            | ModeReason::OnlyUnhandled { .. }
            | ModeReason::Dropped { .. }
            | ModeReason::Error { .. } => json_mode_reason,
        }
    }
}

#[derive(Serialize)]
struct JsonStats<'a> {
    tests_total: usize,
    tests_selected: usize,
    selected_ratio: f64,
    tests_added_by_widening: usize,
    tests_always_run: usize,
    tests_dropped: usize,
    by_language: BTreeMap<&'static str, JsonLanguageStats>,
    config_hash: &'a str,
}

#[derive(Serialize)]
struct JsonLanguageStats {
    total: usize,
    selected: usize,
    ratio: f64,
}

fn selected_target(graph: &Graph, selected_test: &SelectedTest) -> Target {
    Target {
        path: graph.path(selected_test.file).clone(),
        language: selected_test_language(graph, selected_test.file),
        name: graph.target_name(selected_test.file).cloned(),
        impact: Some(Impact {
            distance: Some(selected_test.distance()),
            hub_fanout: Some(selected_test.hub_crossing().fanout),
            reason: describe_reason(graph, &selected_test.reason),
        }),
    }
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

pub(crate) fn describe_reason(graph: &Graph, reason: &Reason) -> Vec<String> {
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
            dependency,
        } => {
            let edge = Edge {
                dependent: hop.dependent,
                dependency,
                kind: EdgeKind::Import,
            };
            let inferred = graph
                .inference(&edge)
                .map_or_else(String::new, |inference| {
                    format!(":{} ({})", inference.line, inference.description())
                });

            match graph.fallback_of(hop.dependent) {
                Some(cause) => format!(
                    "-> imported by {dependent}{inferred} (analysed at file level: {})",
                    cause.description()
                ),
                None => format!("-> imported by {dependent}{inferred}"),
            }
        }
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
            kind: EdgeKind::Rule,
            dependency,
        } => {
            let edge = Edge {
                dependent: hop.dependent,
                dependency,
                kind: EdgeKind::Rule,
            };

            match graph.rule_link(&edge) {
                Some(rule_link) if rule_link.captures.is_empty() => format!(
                    "-> linked to {dependent} by rule link ({CONFIG_FILE_NAME}:{}): {}",
                    rule_link.line, rule_link.pattern
                ),
                Some(rule_link) => format!(
                    "-> linked to {dependent} by rule link ({CONFIG_FILE_NAME}:{}): {} ({})",
                    rule_link.line,
                    rule_link.pattern,
                    describe_captures(&rule_link.captures)
                ),
                None => format!("-> linked to {dependent} by {CONFIG_FILE_NAME}"),
            }
        }
        Link::Edge {
            kind: EdgeKind::Build,
            dependency,
        } => {
            let edge = Edge {
                dependent: hop.dependent,
                dependency,
                kind: EdgeKind::Build,
            };

            match graph.inference(&edge) {
                Some(inference) => {
                    format!("-> built into {dependent} ({})", inference.description())
                }
                None => format!("-> built into {dependent}"),
            }
        }
        Link::Edge {
            kind: EdgeKind::Binding,
            dependency,
        } => {
            let edge = Edge {
                dependent: hop.dependent,
                dependency,
                kind: EdgeKind::Binding,
            };

            match graph.inference(&edge) {
                Some(inference) if inference.line > 0 => format!(
                    "-> loaded by {dependent}:{} ({})",
                    inference.line,
                    inference.description()
                ),
                Some(inference) => {
                    format!("-> loaded by {dependent} ({})", inference.description())
                }
                None => format!("-> loaded by {dependent}"),
            }
        }
        Link::Edge {
            kind: EdgeKind::Setup,
            ..
        } => format!("-> run before {dependent} as a setup file of its test runner"),
        Link::Edge {
            kind: kind @ (EdgeKind::Reads | EdgeKind::Lists | EdgeKind::Dynamic | EdgeKind::Spawn),
            dependency,
        } => {
            let verb = match kind {
                EdgeKind::Reads => "read",
                EdgeKind::Lists => "listed",
                EdgeKind::Spawn => "spawned",
                _ => "loaded",
            };
            let edge = Edge {
                dependent: hop.dependent,
                dependency,
                kind,
            };

            match graph.inference(&edge) {
                Some(inference) if inference.line > 0 => format!(
                    "-> {verb} by {dependent}:{} ({})",
                    inference.line,
                    inference.description()
                ),
                Some(inference) => {
                    format!("-> {verb} by {dependent} ({})", inference.description())
                }
                None => format!("-> {verb} by {dependent}"),
            }
        }
        Link::Uses(edge) => describe_use(graph, &edge),
        Link::Loads {
            dependency,
            dependent: loading_symbol,
        } => {
            let edge = Edge {
                dependent: hop.dependent,
                dependency,
                kind: EdgeKind::Dynamic,
            };
            let site = match graph.inference(&edge) {
                Some(inference) => format!(
                    "{dependent}:{} ({})",
                    inference.line,
                    inference.description()
                ),
                None => dependent.to_string(),
            };
            let loading_symbol = graph.symbol(loading_symbol);

            if loading_symbol.kind == SymbolKind::ModuleInit {
                format!("-> loaded by {site}")
            } else {
                format!("-> loaded by {} in {site}", loading_symbol.name)
            }
        }
        Link::ChangedSymbol(change_index) => {
            describe_symbol_change(&graph.symbol_changes()[change_index])
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
        Link::SetAsideSite(site_index) => {
            let set_aside_site = &graph.set_aside_sites()[site_index];

            format!(
                "{WIDENED_HOP_PREFIX}unresolved dynamic import at {dependent}:{} ({}), set aside by {CONFIG_FILE_NAME}:{}",
                set_aside_site.site.line, set_aside_site.site.expression, set_aside_site.line
            )
        }
    }
}

fn describe_use(graph: &Graph, edge: &SymbolEdge) -> String {
    let user = graph.symbol(edge.dependent);
    let user_file = graph.path(user.file);
    let at_load = user.kind == SymbolKind::ModuleInit;

    match edge.dependency {
        SymbolTarget::Symbol(used) if graph.is_dispatch(edge) => {
            let used = graph.symbol(used);

            if at_load {
                format!("-> {} read by key at load by {user_file}", used.name)
            } else {
                format!(
                    "-> {} read by key by {} in {user_file}",
                    used.name, user.name
                )
            }
        }
        SymbolTarget::Symbol(used) if graph.symbol(used).kind == SymbolKind::Fixture => {
            let used = graph.symbol(used);

            if at_load {
                format!("-> fixture {} used by {user_file}", used.name)
            } else {
                format!(
                    "-> fixture {} used by {} in {user_file}",
                    used.name, user.name
                )
            }
        }
        SymbolTarget::Symbol(used) => {
            let used = graph.symbol(used);
            let runs_its_own = at_load && used.file == user.file && graph.is_test(user.file);

            if runs_its_own && used.kind != SymbolKind::ModuleInit {
                return format!("-> {} run by {user_file}", used.name);
            }

            match (used.kind == SymbolKind::ModuleInit, at_load) {
                (true, true) => format!("-> loaded by {user_file}"),
                (true, false) => format!("-> loaded by {} in {user_file}", user.name),
                (false, true) => format!("-> {} used at load by {user_file}", used.name),
                (false, false) => {
                    format!("-> {} used by {} in {user_file}", used.name, user.name)
                }
            }
        }
        SymbolTarget::Module { .. } if edge.kind == EdgeKind::Asset => {
            if at_load {
                format!("-> imported as an asset by {user_file}")
            } else {
                format!("-> imported as an asset by {} in {user_file}", user.name)
            }
        }
        SymbolTarget::Module {
            cause: WholeModuleCause::Imported,
            ..
        } if at_load => format!("-> imported by {user_file}"),
        SymbolTarget::Module { file, cause } => {
            let why = match (cause, graph.fallback_of(file)) {
                (WholeModuleCause::NotAnalysed, Some(fallback)) => {
                    format!(" (analysed at file level: {})", fallback.description())
                }
                (WholeModuleCause::NotAnalysed, None) => String::new(),
                (cause, _) => format!(" ({})", cause.description()),
            };

            if at_load {
                format!("-> used whole at load by {user_file}{why}")
            } else {
                format!("-> used whole by {} in {user_file}{why}", user.name)
            }
        }
    }
}

fn describe_symbol_change(symbol_change: &SymbolChange) -> String {
    let name = &symbol_change.name;

    match &symbol_change.kind {
        SymbolChangeKind::Modified if name == MODULE_INIT_NAME => {
            "-> code run at load changed".to_owned()
        }
        SymbolChangeKind::Modified => format!("-> {name} changed"),
        SymbolChangeKind::Added => format!("-> {name} added"),
        SymbolChangeKind::Renamed { from } => format!("-> {name} renamed from {from}"),
        SymbolChangeKind::Removed => format!("-> {name} removed"),
        SymbolChangeKind::Export => format!("-> export {name} changed"),
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

fn test_file_count(count: usize) -> String {
    if count == 1 {
        return "1 test file".to_owned();
    }

    format!("{count} test files")
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
    use crate::graph::{Edge, GraphBuilder, Inference, RuleLink, UnresolvedCause, UnresolvedSite};
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
  "hub_fanouts": {
    "src/a.test.ts": 0,
    "src/b.test.ts": 2
  },
  "widened": [],
  "dropped": [],
  "unhandled": [],
  "stats": {
    "tests_total": 3,
    "tests_selected": 2,
    "selected_ratio": 0.6667,
    "tests_added_by_widening": 0,
    "tests_always_run": 0,
    "tests_dropped": 0,
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
    fn spawn_and_build_hops_name_the_process_launch_and_the_build_output() {
        let mut builder = GraphBuilder::default();
        let cli_test = builder.add_test_file(repo_path("test/cli.e2e.test.ts"), "typescript");
        let launcher = builder.add_file(repo_path("bin/cli.mjs"));
        let output = builder.add_file(repo_path("dist/entry.js"));
        let entry = builder.add_file(repo_path("src/entry.ts"));

        builder.add_inferred_edge(
            Edge {
                dependent: cli_test,
                dependency: launcher,
                kind: EdgeKind::Spawn,
            },
            Inference {
                line: 12,
                kind: "spawn",
                pattern: "bin/cli.mjs".to_owned(),
                detail: None,
            },
        );
        builder.add_inferred_edge(
            Edge {
                dependent: launcher,
                dependency: output,
                kind: EdgeKind::Dynamic,
            },
            Inference {
                line: 3,
                kind: "path",
                pattern: "dist/entry.js".to_owned(),
                detail: None,
            },
        );
        builder.add_inferred_edge(
            Edge {
                dependent: output,
                dependency: entry,
                kind: EdgeKind::Build,
            },
            Inference {
                line: 0,
                kind: "build mapping",
                pattern: "dist/ <- src/".to_owned(),
                detail: Some("tsconfig.json".to_owned()),
            },
        );

        let graph = builder.build();
        let report = report_for_changes(&graph, &[modified("src/entry.ts")]);
        let json_report: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();

        assert_eq!(
            json_report["reasons"]["test/cli.e2e.test.ts"],
            serde_json::json!([
                "src/entry.ts (modified)",
                "-> built into dist/entry.js (inferred build mapping dist/ <- src/ (tsconfig.json))",
                "-> loaded by bin/cli.mjs:3 (inferred path dist/entry.js)",
                "-> spawned by test/cli.e2e.test.ts:12 (inferred spawn bin/cli.mjs)",
            ])
        );
    }

    #[test]
    fn binding_hops_name_the_module_and_its_declaration() {
        let mut builder = GraphBuilder::default();
        let python_test = builder.add_test_file(repo_path("tests/test_sum.py"), "python");
        let importer = builder.add_file(repo_path("app/native.py"));
        let node_test = builder.add_test_file(repo_path("test/sum.test.ts"), "typescript");
        let loader = builder.add_file(repo_path("binding/index.js"));
        let manifest = builder.add_test_file(repo_path("core/Cargo.toml"), "rust");
        let source = builder.add_file(repo_path("core/src/lib.rs"));

        builder.add_edge(Edge {
            dependent: manifest,
            dependency: source,
            kind: EdgeKind::Build,
        });
        builder.add_inferred_edge(
            Edge {
                dependent: importer,
                dependency: manifest,
                kind: EdgeKind::Binding,
            },
            Inference {
                line: 4,
                kind: "Python binding",
                pattern: "app._core".to_owned(),
                detail: Some("pyproject.toml".to_owned()),
            },
        );
        builder.add_inferred_edge(
            Edge {
                dependent: loader,
                dependency: manifest,
                kind: EdgeKind::Binding,
            },
            Inference {
                line: 0,
                kind: "Node binding",
                pattern: "core".to_owned(),
                detail: Some("binding/package.json".to_owned()),
            },
        );

        for (dependent, dependency) in [(python_test, importer), (node_test, loader)] {
            builder.add_edge(Edge {
                dependent,
                dependency,
                kind: EdgeKind::Import,
            });
        }

        let graph = builder.build();
        let report = report_for_changes(&graph, &[modified("core/src/lib.rs")]);
        let json_report: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();

        assert_eq!(
            json_report["reasons"]["tests/test_sum.py"],
            serde_json::json!([
                "core/src/lib.rs (modified)",
                "-> built into core/Cargo.toml",
                "-> loaded by app/native.py:4 (inferred Python binding app._core (pyproject.toml))",
                "-> imported by tests/test_sum.py",
            ])
        );
        assert_eq!(
            json_report["reasons"]["test/sum.test.ts"],
            serde_json::json!([
                "core/src/lib.rs (modified)",
                "-> built into core/Cargo.toml",
                "-> loaded by binding/index.js (inferred Node binding core (binding/package.json))",
                "-> imported by test/sum.test.ts",
            ])
        );
    }

    #[test]
    fn always_run_tests_join_a_selection_without_a_distance_but_never_an_empty_one() {
        let graph = polyglot_graph();
        let smoke_test = AlwaysRunTest {
            path: repo_path("tests/test_api.py"),
            language: "python",
            reason: "always run by prober.toml:2 (\"tests/**\")".to_owned(),
        };
        let selected = report_for_changes(&graph, &[modified("src/b.ts")])
            .with_always_run(vec![smoke_test.clone()]);
        let json_report: serde_json::Value = serde_json::from_str(&selected.to_json()).unwrap();
        let none = report_for_changes(&graph, &[modified("docs/guide.md")])
            .with_always_run(vec![smoke_test]);

        assert_eq!(
            json_report["targets"],
            serde_json::json!({ "python": ["tests/test_api.py"], "typescript": ["src/b.test.ts"] })
        );
        assert_eq!(
            json_report["reasons"]["tests/test_api.py"],
            serde_json::json!(["always run by prober.toml:2 (\"tests/**\")"])
        );
        assert_eq!(
            json_report["distances"],
            serde_json::json!({ "src/b.test.ts": 1 })
        );
        assert_eq!(json_report["stats"]["tests_always_run"], 1);
        assert_eq!(
            selected.summary_line(),
            "prober: selected 2/3 test files (66.7%), 1 always run"
        );
        assert_eq!(none.targets, []);
    }

    #[test]
    fn only_unhandled_changes_select_none_with_their_own_reason() {
        let graph = polyglot_graph();
        let changes = [modified("apps/ios/ChatView.swift")];
        let report = report_for_changes(&graph, &changes)
            .with_unhandled(vec![repo_path("apps/ios/ChatView.swift")], changes.len());
        let json_report: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();

        assert_eq!(
            json_report["mode_reasons"],
            serde_json::json!([{
                "kind": "only_unhandled",
                "message": "1 changed file is in a language Prober does not analyse",
            }])
        );
        assert_eq!(
            json_report["unhandled"],
            serde_json::json!(["apps/ios/ChatView.swift"])
        );
    }

    #[test]
    fn forced_selection_runs_every_test() {
        let report = Report::all(&polyglot_graph(), vec![ModeReason::Forced], String::new());

        assert_eq!(
            report.summary_line(),
            "prober: all 3 test files — forced by PROBER_SELECTION=all"
        );
        assert_eq!(ModeReason::Forced.kind(), "forced");
    }

    #[test]
    fn config_link_hop_names_the_rule_line_and_its_captures() {
        let mut builder = GraphBuilder::default();
        let plugins_test =
            builder.add_test_file(repo_path("test/plugins.e2e.test.ts"), "typescript");
        let telegram = builder.add_file(repo_path("extensions/telegram/send.ts"));

        builder.add_rule_link(RuleLink {
            line: 12,
            pattern: "extensions/{id}/**".to_owned(),
            captures: vec![("id".to_owned(), "telegram".to_owned())],
            from: vec![telegram],
            to: vec![plugins_test],
        });

        let graph = builder.build();
        let report = report_for_changes(&graph, &[modified("extensions/telegram/send.ts")]);

        assert_eq!(
            report.targets[0].impact.as_ref().unwrap().reason,
            [
                "extensions/telegram/send.ts (modified)",
                "-> linked to test/plugins.e2e.test.ts by rule link (prober.toml:12): extensions/{id}/** (id=telegram)",
            ]
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
  "hub_fanouts": {},
  "widened": [],
  "dropped": [],
  "unhandled": [],
  "stats": {
    "tests_total": 3,
    "tests_selected": 3,
    "selected_ratio": 1.0,
    "tests_added_by_widening": 0,
    "tests_always_run": 0,
    "tests_dropped": 0,
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
  "hub_fanouts": {},
  "widened": [],
  "dropped": [],
  "unhandled": [],
  "stats": {
    "tests_total": 3,
    "tests_selected": 0,
    "selected_ratio": 0.0,
    "tests_added_by_widening": 0,
    "tests_always_run": 0,
    "tests_dropped": 0,
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
    fn language_lists_hold_the_targets_of_one_language_in_every_mode() {
        let graph = polyglot_graph();
        let selected_report = report_for_changes(&graph, &[modified("src/a.ts")]);
        let all_report = report_running_all(&graph);
        let none_report = report_for_changes(&graph, &[modified("README.md")]);

        assert_eq!(
            selected_report.to_language_list("typescript"),
            "src/a.test.ts\nsrc/b.test.ts\n"
        );
        assert_eq!(selected_report.to_language_list("python"), "");
        assert_eq!(all_report.to_language_list("python"), "tests/test_api.py\n");
        assert_eq!(none_report.to_language_list("typescript"), "");
    }

    #[test]
    fn markdown_summary_gives_the_mode_the_languages_and_the_main_changes_or_reasons() {
        let graph = polyglot_graph();
        let selected_report = report_for_changes(&graph, &[modified("src/a.ts")]);
        let all_report = report_running_all(&graph);

        assert_eq!(
            selected_report.to_markdown(),
            "### Prober: selected 2/3 test files (66.7%)

| Language | Selected | Test files |
| --- | --- | --- |
| python | 0 | 1 |
| typescript | 2 | 2 |

Changes reaching the most test files:

- `src/a.ts (modified)`: 2 test files
"
        );
        assert_eq!(
            all_report.to_markdown(),
            "### Prober: all 3 test files

- ci/deploy.sh (modified) matches always_all \"ci/**\"
- ci/lint.sh (modified) matches always_all \"ci/**\"

| Language | Selected | Test files |
| --- | --- | --- |
| python | 1 | 1 |
| typescript | 2 | 2 |
"
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
                "message": "invalid prober.toml at line 1, column 1: unknown field `anchors`, expected one of `presets`, `always_all`, `always_run`, `link`, `ignore`, `inference`, `aggressive`, `defer`, `protected`",
            }])
        );
        assert_eq!(
            report.summary_line(),
            "prober: all 3 test files — invalid prober.toml at line 1, column 1: unknown field `anchors`, expected one of `presets`, `always_all`, `always_run`, `link`, `ignore`, `inference`, `aggressive`, `defer`, `protected`"
        );
    }

    fn report_with_options(config_text: &str, always_run: Vec<AlwaysRunTest>) -> Report {
        let graph = polyglot_graph();
        let config = Config::parse(config_text).unwrap();
        let changes = [modified("src/a.ts")];
        let selected_tests = select_tests(&graph, &changes, Widening::Scoped);
        let (kept_tests, dropped_tests) = config.aggressive.split(&graph, selected_tests);

        Report::from_selection(&graph, &changes, &kept_tests, CONFIG_HASH.to_owned())
            .with_dropped(&graph, &dropped_tests)
            .with_always_run(always_run)
    }

    #[test]
    fn dropped_tests_leave_the_targets_and_are_listed_with_their_option() {
        let report = report_with_options("[aggressive]\nmax_distance = 1\n", Vec::new());
        let json_report: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();

        assert_eq!(report.to_list(ListSeparator::Newline), "src/a.test.ts\n");
        assert_eq!(
            json_report["dropped"],
            serde_json::json!([{
                "test": "src/b.test.ts",
                "option": "max_distance",
                "distance": 2,
                "hub_fanout": 2,
            }])
        );
        assert_eq!(
            json_report["hub_fanouts"],
            serde_json::json!({ "src/a.test.ts": 0 })
        );
        assert_eq!(json_report["stats"]["tests_dropped"], 1);
        assert_eq!(
            report.summary_line(),
            "prober: selected 1/3 test files (33.3%), 1 dropped by aggressive options"
        );
        assert!(
            report.to_markdown().ends_with(
                "\nDropped by aggressive options: 1 test file\n\n- `src/b.test.ts`: [aggressive] max_distance = 1 (prober.toml:2)\n"
            ),
            "{}",
            report.to_markdown()
        );
    }

    #[test]
    fn every_impacted_test_dropped_runs_none_unless_tests_always_run() {
        let report = report_with_options("[aggressive]\nmax_distance = 0\n", Vec::new());
        let always_run = AlwaysRunTest {
            path: repo_path("src/b.test.ts"),
            language: "typescript",
            reason: "always run by prober.toml:5 (\"src/b.test.ts\")".to_owned(),
        };
        let with_always_run = report_with_options(
            "[aggressive]\nmax_distance = 0\n\n[always_run]\ntests = [\"src/b.test.ts\"]\n",
            vec![always_run],
        );

        assert_eq!(
            report.mode,
            Mode::None {
                reasons: vec![ModeReason::Dropped { test_count: 2 }]
            }
        );
        assert_eq!(
            report.summary_line(),
            "prober: none of 3 test files — the 2 test files the changes reach are dropped by aggressive options"
        );
        assert_eq!(with_always_run.mode, Mode::Selected);
        assert_eq!(
            with_always_run.to_list(ListSeparator::Newline),
            "src/b.test.ts\n"
        );
        assert_eq!(
            with_always_run
                .dropped
                .iter()
                .map(|dropped| dropped.target.path.as_str())
                .collect::<Vec<_>>(),
            ["src/a.test.ts"]
        );
    }
}
