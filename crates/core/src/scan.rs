mod hubs;
mod suggestions;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt::Write;

use serde::Serialize;

use crate::files::{FileId, RepoPath};
use crate::graph::{ContentChange, Graph, RootScope, UnresolvedCause};
use crate::ratios::format_percentage;
use crate::rules::{ConfigWarning, RuleEffect, RuleReport};

pub use crate::scan::hubs::{HubFile, SmokeSet};
pub use crate::scan::suggestions::Suggestion;

const SCHEMA_VERSION: u32 = 1;

const LISTED_ROWS: usize = 20;

const UNLINKED_SHOWN: usize = 5;

const REPOSITORY_ROOT: &str = "(root)";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct InferredPattern {
    pub kind: &'static str,
    pub pattern: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredSite {
    pub site: String,
    pub patterns: Vec<InferredPattern>,
    pub links: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedSiteReach {
    pub site: String,
    pub expression: String,
    pub cause: UnresolvedCause,
    pub tests: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingFileSite {
    pub site: String,
    pub expression: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanReport {
    pub inferred_sites: Vec<InferredSite>,
    pub links_by_kind: BTreeMap<&'static str, usize>,
    pub unresolved_sites: Vec<UnresolvedSiteReach>,
    pub missing_file_sites: Vec<MissingFileSite>,
    pub tests_reached_by_unresolved_sites: usize,
    pub unreachable_modules: Vec<RepoPath>,
    pub unreachable_other_files: Vec<RepoPath>,
    pub root_scopes: Vec<RootScope>,
    pub hub_files: Vec<HubFile>,
    pub smoke_set: SmokeSet,
    pub suggestions: Vec<Suggestion>,
    pub rule_effects: Vec<RuleEffect>,
    pub config_warnings: Vec<ConfigWarning>,
    pub test_count: usize,
    pub config_hash: String,
}

impl ScanReport {
    pub fn new(graph: &Graph, repo_files: &[RepoPath], config_hash: String) -> Self {
        let (inferred_sites, links_by_kind) = inferred_sites(graph);
        let (unresolved_sites, tests_reached_by_unresolved_sites) = unresolved_sites(graph);
        let missing_file_sites = missing_file_sites(graph);
        let (unreachable_modules, unreachable_other_files) = unreachable_files(graph, repo_files);
        let (hub_files, smoke_set) = hubs::hubs_and_smoke_set(graph, repo_files);
        let test_count = graph.test_files_by_path().len();
        let suggestions = suggestions::suggestions(
            &unresolved_sites,
            &inferred_sites,
            test_count,
            repo_files.len(),
        );

        Self {
            inferred_sites,
            links_by_kind,
            unresolved_sites,
            missing_file_sites,
            tests_reached_by_unresolved_sites,
            unreachable_modules,
            unreachable_other_files,
            root_scopes: graph.root_scopes().to_vec(),
            hub_files,
            smoke_set,
            suggestions,
            rule_effects: Vec::new(),
            config_warnings: Vec::new(),
            test_count,
            config_hash,
        }
    }

    pub fn with_config(self, rule_report: RuleReport, mut warnings: Vec<ConfigWarning>) -> Self {
        warnings.extend(rule_report.warnings);

        Self {
            rule_effects: rule_report.effects,
            config_warnings: warnings,
            ..self
        }
    }

    pub fn inferred_link_count(&self) -> usize {
        self.links_by_kind.values().sum()
    }

    pub fn unreachable_file_count(&self) -> usize {
        self.unreachable_modules.len() + self.unreachable_other_files.len()
    }

    pub fn to_text(&self) -> String {
        let mut text = String::new();

        self.write_inferred_sites(&mut text);
        text.push('\n');
        self.write_unresolved_sites(&mut text);
        text.push('\n');
        self.write_missing_file_sites(&mut text);
        text.push('\n');
        self.write_unreachable_files(&mut text);
        text.push('\n');
        self.write_root_scopes(&mut text);
        text.push('\n');
        self.write_hub_files(&mut text);
        text.push('\n');
        self.write_suggestions(&mut text);

        if !self.rule_effects.is_empty() || !self.config_warnings.is_empty() {
            text.push('\n');
            self.write_config(&mut text);
        }

        text
    }

    pub fn to_json(&self) -> String {
        let json_scan = JsonScan {
            schema_version: SCHEMA_VERSION,
            inferred: self
                .inferred_sites
                .iter()
                .map(|inferred_site| JsonInferredSite {
                    site: &inferred_site.site,
                    patterns: &inferred_site.patterns,
                    links: inferred_site.links,
                })
                .collect(),
            unresolved: self
                .unresolved_sites
                .iter()
                .map(|unresolved_site| JsonUnresolvedSite {
                    site: &unresolved_site.site,
                    expression: &unresolved_site.expression,
                    cause: unresolved_site.cause.key(),
                    tests: unresolved_site.tests,
                })
                .collect(),
            missing: self
                .missing_file_sites
                .iter()
                .map(|missing_file_site| JsonMissingFileSite {
                    site: &missing_file_site.site,
                    expression: &missing_file_site.expression,
                })
                .collect(),
            unreachable: JsonUnreachableFiles {
                modules: self
                    .unreachable_modules
                    .iter()
                    .map(RepoPath::as_str)
                    .collect(),
                other_files: self
                    .unreachable_other_files
                    .iter()
                    .map(RepoPath::as_str)
                    .collect(),
            },
            root_scopes: self
                .root_scopes
                .iter()
                .map(|root_scope| JsonRootScope {
                    directory: &root_scope.directory,
                    configs: root_scope.configs.iter().map(RepoPath::as_str).collect(),
                    files: root_scope.files.iter().map(RepoPath::as_str).collect(),
                    packages: root_scope.packages,
                    tests: root_scope.tests,
                })
                .collect(),
            hub_files: self
                .hub_files
                .iter()
                .map(|hub_file| JsonHubFile {
                    file: hub_file.path.as_str(),
                    tests: hub_file.tests,
                })
                .collect(),
            smoke_set: JsonSmokeSet {
                tests: self.smoke_set.tests.iter().map(RepoPath::as_str).collect(),
                covered_hub_files: self.smoke_set.covered_hub_files,
            },
            suggestions: self
                .suggestions
                .iter()
                .map(|suggestion| JsonSuggestion {
                    reason: &suggestion.reason,
                    toml: &suggestion.toml,
                })
                .collect(),
            config: JsonConfig {
                rules: self
                    .rule_effects
                    .iter()
                    .map(|rule_effect| JsonRuleEffect {
                        line: rule_effect.line,
                        rule: &rule_effect.rule,
                        links: rule_effect.links,
                        sites: rule_effect.sites,
                        unlinked: &rule_effect.unlinked,
                    })
                    .collect(),
                warnings: self
                    .config_warnings
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            },
            stats: JsonScanStats {
                inferred_sites: self.inferred_sites.len(),
                inferred_links: self.inferred_link_count(),
                links_by_kind: &self.links_by_kind,
                unresolved_sites: self.unresolved_sites.len(),
                tests_reached_by_unresolved_sites: self.tests_reached_by_unresolved_sites,
                missing_file_sites: self.missing_file_sites.len(),
                tests_total: self.test_count,
                unreachable_files: self.unreachable_file_count(),
                config_hash: &self.config_hash,
            },
        };
        let mut json = serde_json::to_string_pretty(&json_scan).expect("scan serializes to JSON");

        json.push('\n');

        json
    }

    pub fn summary_line(&self) -> String {
        format!(
            "prober scan: {} links inferred from {} sites; {} unresolved sites reach {}/{} test files ({}); {} sites load missing files; {} files reach no test; {} suggestions",
            self.inferred_link_count(),
            self.inferred_sites.len(),
            self.unresolved_sites.len(),
            self.tests_reached_by_unresolved_sites,
            self.test_count,
            format_percentage(
                self.tests_reached_by_unresolved_sites as f64,
                self.test_count as f64
            ),
            self.missing_file_sites.len(),
            self.unreachable_file_count(),
            self.suggestions.len()
        )
    }

    fn write_root_scopes(&self, text: &mut String) {
        let _ = writeln!(
            text,
            "Root scope: {} runner projects, the files loaded before their tests",
            self.root_scopes.len()
        );

        let rows: Vec<(usize, String)> = self
            .root_scopes
            .iter()
            .map(|root_scope| {
                let directory = if root_scope.directory.is_empty() {
                    REPOSITORY_ROOT
                } else {
                    &root_scope.directory
                };
                let configs = match root_scope.configs.as_slice() {
                    [config] => config.to_string(),
                    [first, rest @ ..] => format!("{first} and {} more configs", rest.len()),
                    [] => "no config".to_owned(),
                };

                (
                    root_scope.files.len(),
                    format!(
                        "{directory}  {configs}; {} test files, {} packages",
                        root_scope.tests, root_scope.packages
                    ),
                )
            })
            .collect();

        write_rows(text, "files", "project", &rows, "projects");
    }

    fn write_hub_files(&self, text: &mut String) {
        let _ = writeln!(
            text,
            "Hub files: the {} files most test files depend on",
            self.hub_files.len()
        );

        let rows: Vec<(usize, String)> = self
            .hub_files
            .iter()
            .map(|hub_file| (hub_file.tests, hub_file.path.to_string()))
            .collect();

        write_rows(text, "tests", "file", &rows, "files");

        if self.smoke_set.tests.is_empty() {
            return;
        }

        let tests: Vec<String> = self
            .smoke_set
            .tests
            .iter()
            .map(|test| format!("{:?}", test.as_str()))
            .collect();
        let _ = writeln!(
            text,
            "Suggested smoke set: {} test files within two links of {} of these {} files; run them on every change with\n  [always_run]\n  tests = [{}]",
            self.smoke_set.tests.len(),
            self.smoke_set.covered_hub_files,
            self.hub_files.len(),
            tests.join(", ")
        );
    }

    fn write_suggestions(&self, text: &mut String) {
        let _ = writeln!(text, "Suggestions: {}", self.suggestions.len());

        for suggestion in &self.suggestions {
            let _ = writeln!(text, "  # {}", suggestion.reason);

            for toml_line in suggestion.toml.lines() {
                if toml_line.is_empty() {
                    text.push('\n');
                } else {
                    let _ = writeln!(text, "  {toml_line}");
                }
            }
        }
    }

    fn write_config(&self, text: &mut String) {
        let _ = writeln!(
            text,
            "Config rules: {}, {} warnings",
            self.rule_effects.len(),
            self.config_warnings.len()
        );

        for rule_effect in &self.rule_effects {
            let _ = writeln!(
                text,
                "  prober.toml:{}  {}: {} links, {} unresolved sites",
                rule_effect.line, rule_effect.rule, rule_effect.links, rule_effect.sites
            );

            if !rule_effect.unlinked.is_empty() {
                let shown: Vec<&str> = rule_effect
                    .unlinked
                    .iter()
                    .take(UNLINKED_SHOWN)
                    .map(String::as_str)
                    .collect();
                let more = rule_effect.unlinked.len().saturating_sub(UNLINKED_SHOWN);
                let more = if more > 0 {
                    format!(" and {more} more")
                } else {
                    String::new()
                };

                let _ = writeln!(
                    text,
                    "    targets match no file for {}{more}",
                    shown.join("; ")
                );
            }
        }

        for warning in &self.config_warnings {
            let _ = writeln!(text, "  warning: {warning}");
        }
    }

    fn write_inferred_sites(&self, text: &mut String) {
        let _ = writeln!(
            text,
            "Inferred links: {} from {} sites",
            self.inferred_link_count(),
            self.inferred_sites.len()
        );

        for (kind, links) in &self.links_by_kind {
            let _ = writeln!(text, "  {kind}: {links} links");
        }

        let rows: Vec<(usize, String)> = self
            .inferred_sites
            .iter()
            .map(|inferred_site| {
                let patterns: Vec<String> = inferred_site
                    .patterns
                    .iter()
                    .map(|inferred_pattern| {
                        format!("{} {}", inferred_pattern.kind, inferred_pattern.pattern)
                    })
                    .collect();

                (
                    inferred_site.links,
                    format!("{}  {}", inferred_site.site, patterns.join(", ")),
                )
            })
            .collect();

        write_rows(text, "links", "site", &rows, "sites");
    }

    fn write_unresolved_sites(&self, text: &mut String) {
        let _ = writeln!(
            text,
            "Unresolved sites: {}, reaching {} of {} test files ({})",
            self.unresolved_sites.len(),
            self.tests_reached_by_unresolved_sites,
            self.test_count,
            format_percentage(
                self.tests_reached_by_unresolved_sites as f64,
                self.test_count as f64
            )
        );

        let rows: Vec<(usize, String)> = self
            .unresolved_sites
            .iter()
            .map(|unresolved_site| {
                (
                    unresolved_site.tests,
                    format!(
                        "{}  {}: {}",
                        unresolved_site.site,
                        unresolved_site.expression,
                        unresolved_site.cause.description()
                    ),
                )
            })
            .collect();

        write_rows(text, "tests", "site", &rows, "sites");
    }

    fn write_missing_file_sites(&self, text: &mut String) {
        let _ = writeln!(
            text,
            "Loads of missing files: {} sites, linked to the paths they name (build outputs, optional files)",
            self.missing_file_sites.len()
        );

        for missing_file_site in self.missing_file_sites.iter().take(LISTED_ROWS) {
            let _ = writeln!(
                text,
                "  {}  {}",
                missing_file_site.site, missing_file_site.expression
            );
        }

        if self.missing_file_sites.len() > LISTED_ROWS {
            let _ = writeln!(
                text,
                "  … {} more sites, all of them with --format json",
                self.missing_file_sites.len() - LISTED_ROWS
            );
        }
    }

    fn write_unreachable_files(&self, text: &mut String) {
        let _ = writeln!(
            text,
            "Unreachable files: {} ({} modules, {} other files)",
            self.unreachable_file_count(),
            self.unreachable_modules.len(),
            self.unreachable_other_files.len()
        );

        let mut files_by_directory: BTreeMap<&str, usize> = BTreeMap::new();

        for path in self
            .unreachable_modules
            .iter()
            .chain(&self.unreachable_other_files)
        {
            *files_by_directory.entry(top_directory(path)).or_default() += 1;
        }

        let mut rows: Vec<(usize, String)> = files_by_directory
            .into_iter()
            .map(|(directory, files)| (files, directory.to_owned()))
            .collect();

        rows.sort_by_key(|(count, _)| Reverse(*count));
        write_rows(text, "files", "directory", &rows, "directories");
    }
}

fn inferred_sites(graph: &Graph) -> (Vec<InferredSite>, BTreeMap<&'static str, usize>) {
    let mut patterns_by_site: BTreeMap<(&RepoPath, u32), (BTreeSet<InferredPattern>, usize)> =
        BTreeMap::new();
    let mut links_by_kind: BTreeMap<&'static str, usize> = BTreeMap::new();

    for (inferred_edge, inference) in graph.inferred_edges() {
        let site = (graph.path(inferred_edge.dependent), inference.line);
        let (patterns, links) = patterns_by_site.entry(site).or_default();

        patterns.insert(InferredPattern {
            kind: inference.kind,
            pattern: inference.pattern.clone(),
        });
        *links += 1;
        *links_by_kind.entry(inference.kind).or_default() += 1;
    }

    let mut inferred_sites: Vec<InferredSite> = patterns_by_site
        .into_iter()
        .map(|((path, line), (patterns, links))| InferredSite {
            site: format!("{path}:{line}"),
            patterns: patterns.into_iter().collect(),
            links,
        })
        .collect();

    inferred_sites.sort_by_key(|inferred_site| Reverse(inferred_site.links));

    (inferred_sites, links_by_kind)
}

fn unresolved_sites(graph: &Graph) -> (Vec<UnresolvedSiteReach>, usize) {
    let mut tests_by_file: HashMap<FileId, usize> = HashMap::new();
    let mut unresolved_sites: Vec<UnresolvedSiteReach> = graph
        .unresolved_sites()
        .iter()
        .filter(|unresolved_site| unresolved_site.cause.widens())
        .map(|unresolved_site| {
            let tests = *tests_by_file
                .entry(unresolved_site.file)
                .or_insert_with(|| tests_depending_on(graph, [unresolved_site.file]).len());

            UnresolvedSiteReach {
                site: format!(
                    "{}:{}",
                    graph.path(unresolved_site.file),
                    unresolved_site.line
                ),
                expression: unresolved_site.expression.clone(),
                cause: unresolved_site.cause,
                tests,
            }
        })
        .collect();
    let site_files = graph
        .unresolved_sites()
        .iter()
        .filter(|unresolved_site| unresolved_site.cause.widens())
        .map(|unresolved_site| unresolved_site.file);
    let tests_reached = tests_depending_on(graph, site_files).len();

    unresolved_sites.sort_by_key(|unresolved_site| Reverse(unresolved_site.tests));

    (unresolved_sites, tests_reached)
}

fn missing_file_sites(graph: &Graph) -> Vec<MissingFileSite> {
    graph
        .unresolved_sites()
        .iter()
        .filter(|unresolved_site| !unresolved_site.cause.widens())
        .map(|unresolved_site| MissingFileSite {
            site: format!(
                "{}:{}",
                graph.path(unresolved_site.file),
                unresolved_site.line
            ),
            expression: unresolved_site.expression.clone(),
        })
        .collect()
}

pub(crate) fn tests_depending_on(
    graph: &Graph,
    files: impl IntoIterator<Item = FileId>,
) -> HashSet<FileId> {
    let mut reached = HashSet::new();
    let mut files_to_visit = VecDeque::new();

    for file in files {
        if reached.insert(file) {
            files_to_visit.push_back(file);
        }
    }

    while let Some(file) = files_to_visit.pop_front() {
        for edge in graph.dependents(file) {
            let carries_the_change =
                edge.kind.selects_tests() && edge.kind.follows(ContentChange::None);

            if carries_the_change && reached.insert(edge.dependent) {
                files_to_visit.push_back(edge.dependent);
            }
        }
    }

    reached
        .into_iter()
        .filter(|&file| graph.is_test(file))
        .collect()
}

fn unreachable_files(graph: &Graph, repo_files: &[RepoPath]) -> (Vec<RepoPath>, Vec<RepoPath>) {
    let mut needed_content: HashMap<FileId, ContentChange> = HashMap::new();
    let mut files_to_visit = VecDeque::new();

    for test_file in graph.test_files_by_path() {
        needed_content.insert(test_file.file, ContentChange::None);
        files_to_visit.push_back(test_file.file);
    }

    while let Some(file) = files_to_visit.pop_front() {
        let dependent_needs = needed_content[&file];

        for edge in graph.dependencies(file) {
            if !edge.kind.selects_tests() || edge.kind.dependent_content() < dependent_needs {
                continue;
            }

            let Some(dependency_needs) = ContentChange::ALL
                .into_iter()
                .find(|&content| edge.kind.follows(content))
            else {
                continue;
            };
            let is_easier = needed_content
                .get(&edge.dependency)
                .is_none_or(|&needs| dependency_needs < needs);

            if is_easier {
                needed_content.insert(edge.dependency, dependency_needs);
                files_to_visit.push_back(edge.dependency);
            }
        }
    }

    let mut sorted_files: Vec<&RepoPath> = repo_files.iter().collect();
    let mut unreachable_modules = Vec::new();
    let mut unreachable_other_files = Vec::new();

    sorted_files.sort();
    sorted_files.dedup();

    for path in sorted_files {
        let is_reached = graph
            .file_id(path)
            .is_some_and(|file| needed_content.contains_key(&file));

        match (is_reached, graph.is_module(path)) {
            (true, _) => {}
            (false, true) => unreachable_modules.push(path.clone()),
            (false, false) => unreachable_other_files.push(path.clone()),
        }
    }

    (unreachable_modules, unreachable_other_files)
}

fn top_directory(path: &RepoPath) -> &str {
    match path.as_str().split_once('/') {
        Some((directory, _)) => directory,
        None => REPOSITORY_ROOT,
    }
}

fn write_rows(
    text: &mut String,
    count_header: &str,
    label_header: &str,
    rows: &[(usize, String)],
    noun: &str,
) {
    if rows.is_empty() {
        return;
    }

    let count_width = rows
        .iter()
        .map(|(count, _)| count.to_string().len())
        .chain([count_header.len()])
        .max()
        .unwrap_or_default();

    let _ = writeln!(text, "  {count_header:>count_width$}  {label_header}");

    for (count, label) in rows.iter().take(LISTED_ROWS) {
        let _ = writeln!(text, "  {count:>count_width$}  {label}");
    }

    if rows.len() > LISTED_ROWS {
        let _ = writeln!(
            text,
            "  … {} more {noun}, all of them with --format json",
            rows.len() - LISTED_ROWS
        );
    }
}

#[derive(Serialize)]
struct JsonScan<'a> {
    schema_version: u32,
    inferred: Vec<JsonInferredSite<'a>>,
    unresolved: Vec<JsonUnresolvedSite<'a>>,
    missing: Vec<JsonMissingFileSite<'a>>,
    unreachable: JsonUnreachableFiles<'a>,
    root_scopes: Vec<JsonRootScope<'a>>,
    hub_files: Vec<JsonHubFile<'a>>,
    smoke_set: JsonSmokeSet<'a>,
    suggestions: Vec<JsonSuggestion<'a>>,
    config: JsonConfig<'a>,
    stats: JsonScanStats<'a>,
}

#[derive(Serialize)]
struct JsonRootScope<'a> {
    directory: &'a str,
    configs: Vec<&'a str>,
    files: Vec<&'a str>,
    packages: usize,
    tests: usize,
}

#[derive(Serialize)]
struct JsonHubFile<'a> {
    file: &'a str,
    tests: usize,
}

#[derive(Serialize)]
struct JsonSmokeSet<'a> {
    tests: Vec<&'a str>,
    covered_hub_files: usize,
}

#[derive(Serialize)]
struct JsonSuggestion<'a> {
    reason: &'a str,
    toml: &'a str,
}

#[derive(Serialize)]
struct JsonConfig<'a> {
    rules: Vec<JsonRuleEffect<'a>>,
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct JsonRuleEffect<'a> {
    line: usize,
    rule: &'a str,
    links: usize,
    sites: usize,
    unlinked: &'a [String],
}

#[derive(Serialize)]
struct JsonInferredSite<'a> {
    site: &'a str,
    patterns: &'a [InferredPattern],
    links: usize,
}

#[derive(Serialize)]
struct JsonUnresolvedSite<'a> {
    site: &'a str,
    expression: &'a str,
    cause: &'static str,
    tests: usize,
}

#[derive(Serialize)]
struct JsonMissingFileSite<'a> {
    site: &'a str,
    expression: &'a str,
}

#[derive(Serialize)]
struct JsonUnreachableFiles<'a> {
    modules: Vec<&'a str>,
    other_files: Vec<&'a str>,
}

#[derive(Serialize)]
struct JsonScanStats<'a> {
    inferred_sites: usize,
    inferred_links: usize,
    links_by_kind: &'a BTreeMap<&'static str, usize>,
    unresolved_sites: usize,
    tests_reached_by_unresolved_sites: usize,
    missing_file_sites: usize,
    tests_total: usize,
    unreachable_files: usize,
    config_hash: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Edge, EdgeKind, GraphBuilder, Inference, UnresolvedSite};

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn scanned_project() -> ScanReport {
        let mut builder = GraphBuilder::default();
        let loader_test = builder.add_test_file(repo_path("src/loader.test.ts"), "typescript");
        let worker_test = builder.add_test_file(repo_path("src/worker.test.ts"), "typescript");
        let loader = builder.add_file(repo_path("src/loader.ts"));
        let worker = builder.add_file(repo_path("src/worker.ts"));
        let telegram = builder.add_file(repo_path("src/plugins/telegram.ts"));
        let slack = builder.add_file(repo_path("src/plugins/slack.ts"));
        let imports = [(loader_test, loader), (worker_test, worker)];

        builder.add_language("typescript", |path| path.as_str().ends_with(".ts"));

        for (dependent, dependency) in imports {
            builder.add_edge(Edge {
                dependent,
                dependency,
                kind: EdgeKind::Import,
            });
        }

        for plugin in [telegram, slack] {
            builder.add_inferred_edge(
                Edge {
                    dependent: loader,
                    dependency: plugin,
                    kind: EdgeKind::Dynamic,
                },
                Inference {
                    line: 4,
                    kind: "glob",
                    pattern: "src/plugins/*.js".to_owned(),
                    detail: None,
                },
            );
        }

        for (file, line, cause) in [
            (worker, 5, UnresolvedCause::Parameter),
            (loader, 9, UnresolvedCause::NoMatchingFile),
        ] {
            builder.add_unresolved_site(UnresolvedSite {
                file,
                line,
                expression: "request.moduleUrl".to_owned(),
                cause,
                language: "typescript",
            });
        }

        let graph = builder.build();
        let repo_files: Vec<RepoPath> = [
            "src/loader.ts",
            "src/loader.test.ts",
            "src/worker.ts",
            "src/worker.test.ts",
            "src/plugins/telegram.ts",
            "src/plugins/slack.ts",
            "src/unused.ts",
            "docs/guide.md",
            "README.md",
        ]
        .into_iter()
        .map(repo_path)
        .collect();

        ScanReport::new(&graph, &repo_files, "0123456789abcdef".to_owned())
    }

    #[test]
    fn scan_groups_inferred_links_by_site_and_counts_the_tests_each_unresolved_site_reaches() {
        let scan = scanned_project();

        assert_eq!(
            scan.inferred_sites,
            [InferredSite {
                site: "src/loader.ts:4".to_owned(),
                patterns: vec![InferredPattern {
                    kind: "glob",
                    pattern: "src/plugins/*.js".to_owned(),
                }],
                links: 2,
            }]
        );
        assert_eq!(
            scan.unresolved_sites
                .iter()
                .map(|unresolved_site| (unresolved_site.site.as_str(), unresolved_site.tests))
                .collect::<Vec<_>>(),
            [("src/worker.ts:5", 1)]
        );
        assert_eq!(scan.tests_reached_by_unresolved_sites, 1);
        assert_eq!(
            scan.missing_file_sites,
            [MissingFileSite {
                site: "src/loader.ts:9".to_owned(),
                expression: "request.moduleUrl".to_owned(),
            }]
        );
        assert_eq!(scan.unreachable_modules, [repo_path("src/unused.ts")]);
        assert_eq!(
            scan.unreachable_other_files,
            [repo_path("README.md"), repo_path("docs/guide.md")]
        );
    }

    #[test]
    fn scan_text_lists_sites_and_unreachable_directories() {
        assert_eq!(
            scanned_project().to_text(),
            "Inferred links: 2 from 1 sites
  glob: 2 links
  links  site
      2  src/loader.ts:4  glob src/plugins/*.js

Unresolved sites: 1, reaching 1 of 2 test files (50.0%)
  tests  site
      1  src/worker.ts:5  request.moduleUrl: comes from a function parameter

Loads of missing files: 1 sites, linked to the paths they name (build outputs, optional files)
  src/loader.ts:9  request.moduleUrl

Unreachable files: 3 (1 modules, 2 other files)
  files  directory
      1  (root)
      1  docs
      1  src

Root scope: 0 runner projects, the files loaded before their tests

Hub files: the 4 files most test files depend on
  tests  file
      1  src/loader.ts
      1  src/plugins/slack.ts
      1  src/plugins/telegram.ts
      1  src/worker.ts
Suggested smoke set: 2 test files within two links of 4 of these 4 files; run them on every change with
  [always_run]
  tests = [\"src/loader.test.ts\", \"src/worker.test.ts\"]

Suggestions: 0
"
        );
    }

    #[test]
    fn files_imported_behind_a_read_or_a_listing_reach_no_test() {
        let mut builder = GraphBuilder::default();
        let edges = [
            ("src/prompts.test.ts", EdgeKind::Reads, "src/sample.ts"),
            ("src/sample.ts", EdgeKind::Import, "src/helper.ts"),
            ("src/prompts.test.ts", EdgeKind::Reads, "data/list.json"),
            ("data/list.json", EdgeKind::Lists, "src/entry.ts"),
            ("src/entry.ts", EdgeKind::Import, "src/transport.ts"),
        ];

        builder.add_test_file(repo_path("src/prompts.test.ts"), "typescript");
        builder.add_language("typescript", |path| path.as_str().ends_with(".ts"));

        for (dependent_path, kind, dependency_path) in edges {
            let dependent = builder.add_file(repo_path(dependent_path));
            let dependency = builder.add_file(repo_path(dependency_path));

            builder.add_edge(Edge {
                dependent,
                dependency,
                kind,
            });
        }

        let graph = builder.build();
        let repo_files: Vec<RepoPath> = [
            "src/prompts.test.ts",
            "src/sample.ts",
            "src/helper.ts",
            "data/list.json",
            "src/entry.ts",
            "src/transport.ts",
        ]
        .into_iter()
        .map(repo_path)
        .collect();
        let scan_report = ScanReport::new(&graph, &repo_files, String::new());

        assert_eq!(
            scan_report.unreachable_modules,
            [repo_path("src/helper.ts"), repo_path("src/transport.ts")]
        );
        assert_eq!(scan_report.unreachable_other_files, []);
    }

    #[test]
    fn scan_summary_line_counts_links_unresolved_sites_and_unreachable_files() {
        assert_eq!(
            scanned_project().summary_line(),
            "prober scan: 2 links inferred from 1 sites; 1 unresolved sites reach 1/2 test files (50.0%); 1 sites load missing files; 3 files reach no test; 0 suggestions"
        );
    }

    #[test]
    fn scan_json_carries_every_site_and_file() {
        let json_scan: serde_json::Value =
            serde_json::from_str(&scanned_project().to_json()).unwrap();

        assert_eq!(
            json_scan["inferred"],
            serde_json::json!([{
                "site": "src/loader.ts:4",
                "patterns": [{ "kind": "glob", "pattern": "src/plugins/*.js" }],
                "links": 2,
            }])
        );
        assert_eq!(
            json_scan["missing"],
            serde_json::json!([{ "site": "src/loader.ts:9", "expression": "request.moduleUrl" }])
        );
        assert_eq!(
            json_scan["unresolved"][0],
            serde_json::json!({
                "site": "src/worker.ts:5",
                "expression": "request.moduleUrl",
                "cause": "parameter",
                "tests": 1,
            })
        );
        assert_eq!(
            json_scan["unreachable"],
            serde_json::json!({
                "modules": ["src/unused.ts"],
                "other_files": ["README.md", "docs/guide.md"],
            })
        );
        assert_eq!(
            json_scan["stats"],
            serde_json::json!({
                "inferred_sites": 1,
                "inferred_links": 2,
                "links_by_kind": { "glob": 2 },
                "unresolved_sites": 1,
                "tests_reached_by_unresolved_sites": 1,
                "missing_file_sites": 1,
                "tests_total": 2,
                "unreachable_files": 3,
                "config_hash": "0123456789abcdef",
            })
        );
    }
}
