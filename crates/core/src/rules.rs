//! The rules a team writes in prober.toml: links the code does not show, ignores of inferred links and unresolved sites, the tests every selection runs, and warnings for rules that match nothing or almost everything.

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use anyhow::{Result, bail};
use serde::Deserialize;
use toml::Spanned;

use crate::changes::FileChange;
use crate::files::{FileId, RepoPath};
use crate::graph::{Graph, GraphBuilder, RuleLink, SetAside};
use crate::patterns::{Captures, PathPattern, describe_captures};
use crate::report::{AlwaysRunTest, CONFIG_FILE_NAME};

const LITERAL_TARGET: &str = "literal:";

const ALWAYS_RUN_KEY: &str = "always_run";

const LINK_HEADER: &str = "[[link]]";

const IGNORE_HEADER: &str = "[[ignore]]";

const MIN_BROAD_CANDIDATES: usize = 10;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LinkTable {
    from: String,
    to: Targets,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Targets {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IgnoreTable {
    from: Option<String>,
    to: Option<String>,
    site: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AlwaysRunSection {
    #[serde(default)]
    tests: Vec<String>,
}

#[derive(Debug)]
pub struct LinkRule {
    pub line: usize,
    pub from: PathPattern,
    pub to: Vec<PathPattern>,
}

#[derive(Debug)]
pub enum IgnoreTarget {
    Links {
        from: PathPattern,
        to: PathPattern,
    },
    Sites {
        file: PathPattern,
        line: Option<u32>,
    },
}

#[derive(Debug)]
pub struct IgnoreRule {
    pub line: usize,
    pub target: IgnoreTarget,
}

#[derive(Debug, Default)]
pub struct Rules {
    pub links: Vec<LinkRule>,
    pub ignores: Vec<IgnoreRule>,
    pub always_run: Vec<PathPattern>,
    always_run_line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning {
    pub line: Option<usize>,
    pub message: String,
}

impl fmt::Display for ConfigWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(formatter, "{CONFIG_FILE_NAME}:{line}: {}", self.message),
            None => write!(formatter, "{CONFIG_FILE_NAME}: {}", self.message),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleEffect {
    pub line: usize,
    pub rule: String,
    pub links: usize,
    pub sites: usize,
    pub unlinked: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleReport {
    pub effects: Vec<RuleEffect>,
    pub warnings: Vec<ConfigWarning>,
}

impl Rules {
    pub(crate) fn parse(
        config_text: &str,
        link_tables: Vec<Spanned<LinkTable>>,
        ignore_tables: Vec<Spanned<IgnoreTable>>,
        always_run: AlwaysRunSection,
    ) -> Result<Self> {
        let links = link_tables
            .into_iter()
            .map(|link_table| {
                let line = table_line(config_text, &link_table, LINK_HEADER);
                let link_table = link_table.into_inner();

                link_rule(line, &link_table)
            })
            .collect::<Result<Vec<LinkRule>>>()?;
        let ignores = ignore_tables
            .into_iter()
            .map(|ignore_table| {
                let line = table_line(config_text, &ignore_table, IGNORE_HEADER);
                let ignore_table = ignore_table.into_inner();

                ignore_rule(line, &ignore_table)
            })
            .collect::<Result<Vec<IgnoreRule>>>()?;
        let always_run_patterns = always_run
            .tests
            .iter()
            .map(|test| PathPattern::new(test).map_err(rule_error(None, "[always_run]")))
            .collect::<Result<Vec<PathPattern>>>()?;

        Ok(Self {
            links,
            ignores,
            always_run: always_run_patterns,
            always_run_line: config_text
                .find(ALWAYS_RUN_KEY)
                .map_or(0, |offset| line_of(config_text, offset)),
        })
    }

    pub fn apply(
        &self,
        builder: &mut GraphBuilder,
        repo_files: &[RepoPath],
        changes: &[FileChange],
    ) -> RuleReport {
        let mut rule_report = RuleReport::default();
        let present: HashSet<&RepoPath> = repo_files.iter().collect();
        let removed: Vec<&RepoPath> = changes
            .iter()
            .flat_map(FileChange::touched_paths)
            .filter(|path| !present.contains(path))
            .collect();

        for link in &self.links {
            apply_link(link, builder, repo_files, &removed, &mut rule_report);
        }

        for ignore in &self.ignores {
            apply_ignore(ignore, builder, repo_files, &mut rule_report);
        }

        self.check_always_run(builder, &mut rule_report);
        rule_report
            .warnings
            .sort_by_key(|warning| warning.line.unwrap_or_default());

        rule_report
    }

    pub fn always_run_tests(&self, graph: &Graph) -> Vec<AlwaysRunTest> {
        if self.always_run.is_empty() {
            return Vec::new();
        }

        graph
            .test_files_by_path()
            .into_iter()
            .filter_map(|test_file| {
                let path = graph.path(test_file.file);
                let pattern = self
                    .always_run
                    .iter()
                    .find(|pattern| pattern.is_match(path.as_str()))?;

                Some(AlwaysRunTest {
                    path: path.clone(),
                    language: test_file.language,
                    reason: format!(
                        "always run by {CONFIG_FILE_NAME}:{} ({:?})",
                        self.always_run_line,
                        pattern.text()
                    ),
                })
            })
            .collect()
    }

    fn check_always_run(&self, builder: &GraphBuilder, rule_report: &mut RuleReport) {
        let tests: Vec<&RepoPath> = builder
            .files()
            .filter(|&(file, _)| builder.is_test(file))
            .map(|(_, path)| path)
            .collect();

        for pattern in &self.always_run {
            let matched = tests
                .iter()
                .filter(|path| pattern.is_match(path.as_str()))
                .count();

            if matched == 0 {
                rule_report.warnings.push(ConfigWarning {
                    line: Some(self.always_run_line),
                    message: format!(
                        "[always_run] test {:?} matches no test file",
                        pattern.text()
                    ),
                });
            } else if is_broad(matched, tests.len()) {
                rule_report.warnings.push(ConfigWarning {
                    line: Some(self.always_run_line),
                    message: format!(
                        "[always_run] test {:?} matches {matched} of {} test files, which every selection then runs",
                        pattern.text(),
                        tests.len()
                    ),
                });
            }
        }
    }
}

fn apply_link(
    link: &LinkRule,
    builder: &mut GraphBuilder,
    repo_files: &[RepoPath],
    removed: &[&RepoPath],
    rule_report: &mut RuleReport,
) {
    let from_groups = capture_groups(&link.from, repo_files.iter().chain(removed.iter().copied()));
    let to_groups: Vec<BTreeMap<Captures, Vec<&RepoPath>>> = link
        .to
        .iter()
        .map(|target| capture_groups(target, repo_files))
        .collect();
    let targets: Vec<String> = link
        .to
        .iter()
        .map(|target| format!("{:?}", target.text()))
        .collect();
    let rule = format!(
        "[[link]] from {:?} to {}",
        link.from.text(),
        targets.join(", ")
    );

    lint_matches(
        link.line,
        "from",
        &link.from,
        from_groups.values(),
        repo_files,
        rule_report,
    );

    for (target, groups) in link.to.iter().zip(&to_groups) {
        lint_matches(
            link.line,
            "to",
            target,
            groups.values(),
            repo_files,
            rule_report,
        );
    }

    let mut linked_paths: HashSet<&RepoPath> = HashSet::new();
    let mut unlinked = Vec::new();
    let mut links = 0;

    for (from_captures, from_paths) in &from_groups {
        let to_paths: Vec<&RepoPath> = link
            .to
            .iter()
            .zip(&to_groups)
            .filter_map(|(target, groups)| {
                let key: Captures = target
                    .capture_names()
                    .iter()
                    .filter_map(|name| {
                        from_captures
                            .iter()
                            .find(|(captured_name, _)| captured_name == name)
                            .cloned()
                    })
                    .collect();

                groups.get(&key)
            })
            .flatten()
            .copied()
            .collect();

        if to_paths.is_empty() {
            if !from_captures.is_empty() {
                unlinked.push(describe_captures(from_captures));
            }

            continue;
        }

        let from: Vec<FileId> = from_paths
            .iter()
            .map(|&path| builder.add_file(path.clone()))
            .collect();
        let to: Vec<FileId> = to_paths
            .iter()
            .map(|&path| builder.add_file(path.clone()))
            .collect();
        let group_links = builder.add_rule_link(RuleLink {
            line: link.line,
            pattern: link.from.text().to_owned(),
            captures: from_captures.clone(),
            from,
            to,
        });

        if group_links > 0 {
            linked_paths.extend(to_paths);
            links += group_links;
        }
    }

    let sites = builder.set_aside_sites(SetAside::Resolved { line: link.line }, |_, path| {
        linked_paths.contains(path)
    });

    rule_report.effects.push(RuleEffect {
        line: link.line,
        rule,
        links,
        sites,
        unlinked,
    });
}

fn capture_groups<'p>(
    pattern: &PathPattern,
    paths: impl IntoIterator<Item = &'p RepoPath>,
) -> BTreeMap<Captures, Vec<&'p RepoPath>> {
    let mut groups: BTreeMap<Captures, Vec<&RepoPath>> = BTreeMap::new();

    for path in paths {
        if let Some(captures) = pattern.captures(path.as_str()) {
            groups.entry(captures).or_default().push(path);
        }
    }

    groups
}

fn apply_ignore(
    ignore: &IgnoreRule,
    builder: &mut GraphBuilder,
    repo_files: &[RepoPath],
    rule_report: &mut RuleReport,
) {
    match &ignore.target {
        IgnoreTarget::Links { from, to } => {
            let matching = |pattern: &PathPattern| -> HashSet<FileId> {
                builder
                    .files()
                    .filter(|(_, path)| pattern.is_match(path.as_str()))
                    .map(|(file, _)| file)
                    .collect()
            };
            let from_files = matching(from);
            let to_files = matching(to);
            let rule = format!("[[ignore]] from {:?} to {:?}", from.text(), to.text());
            let matched = |pattern: &PathPattern, nodes: &HashSet<FileId>| {
                repo_files
                    .iter()
                    .filter(|path| pattern.is_match(path.as_str()))
                    .count()
                    .max(nodes.len())
            };
            let from_matched = matched(from, &from_files);
            let to_matched = matched(to, &to_files);

            lint_pattern(
                ignore.line,
                "from",
                from,
                from_matched,
                repo_files,
                rule_report,
            );
            lint_pattern(ignore.line, "to", to, to_matched, repo_files, rule_report);

            let ignored = builder.ignore_links(ignore.line, from_files, to_files);

            if ignored.links == 0 && from_matched > 0 && to_matched > 0 {
                rule_report.warnings.push(ConfigWarning {
                    line: Some(ignore.line),
                    message: format!("{rule} matches no link"),
                });
            }

            if ignored.imports > 0 {
                rule_report.warnings.push(ConfigWarning {
                    line: Some(ignore.line),
                    message: format!(
                        "{rule} removes {} import edges written in the code: tests reached only through them are no longer selected",
                        ignored.imports
                    ),
                });
            }

            rule_report.effects.push(RuleEffect {
                line: ignore.line,
                rule,
                links: ignored.links,
                sites: 0,
                unlinked: Vec::new(),
            });
        }
        IgnoreTarget::Sites { file, line } => {
            let rule = match line {
                Some(line) => format!("[[ignore]] site \"{}:{line}\"", file.text()),
                None => format!("[[ignore]] site {:?}", file.text()),
            };
            let sites =
                builder.set_aside_sites(SetAside::Ignored { line: ignore.line }, |site, path| {
                    file.is_match(path.as_str()) && line.is_none_or(|line| site.line == line)
                });

            if sites == 0 {
                rule_report.warnings.push(ConfigWarning {
                    line: Some(ignore.line),
                    message: format!("{rule} matches no unresolved site"),
                });
            }

            rule_report.effects.push(RuleEffect {
                line: ignore.line,
                rule,
                links: 0,
                sites,
                unlinked: Vec::new(),
            });
        }
    }
}

fn lint_matches<'m>(
    line: usize,
    side: &str,
    pattern: &PathPattern,
    groups: impl Iterator<Item = &'m Vec<&'m RepoPath>>,
    repo_files: &[RepoPath],
    rule_report: &mut RuleReport,
) {
    let matched = groups.map(Vec::len).sum();

    lint_pattern(line, side, pattern, matched, repo_files, rule_report);
}

fn lint_pattern(
    line: usize,
    side: &str,
    pattern: &PathPattern,
    matched: usize,
    repo_files: &[RepoPath],
    rule_report: &mut RuleReport,
) {
    if matched == 0 {
        rule_report.warnings.push(ConfigWarning {
            line: Some(line),
            message: format!("{side} {:?} matches no file", pattern.text()),
        });
    } else if is_broad(matched, repo_files.len()) {
        rule_report.warnings.push(ConfigWarning {
            line: Some(line),
            message: format!(
                "{side} {:?} matches {matched} of {} files",
                pattern.text(),
                repo_files.len()
            ),
        });
    }
}

pub(crate) fn is_broad(matched: usize, candidates: usize) -> bool {
    candidates >= MIN_BROAD_CANDIDATES && matched * 2 > candidates
}

fn link_rule(line: usize, link_table: &LinkTable) -> Result<LinkRule> {
    let from = PathPattern::new(&link_table.from).map_err(rule_error(Some(line), "[[link]]"))?;
    let targets = match &link_table.to {
        Targets::One(target) => std::slice::from_ref(target),
        Targets::Many(targets) => targets.as_slice(),
    };

    if targets.is_empty() {
        bail!("invalid {CONFIG_FILE_NAME} at line {line}: [[link]] to lists no target");
    }

    let mut to = Vec::new();

    for target in targets {
        if target.starts_with(LITERAL_TARGET) {
            bail!(
                "invalid {CONFIG_FILE_NAME} at line {line}: [[link]] {LITERAL_TARGET} targets are not supported yet, they come with string anchors"
            );
        }

        let target = PathPattern::new(target).map_err(rule_error(Some(line), "[[link]]"))?;

        if let Some(name) = target
            .capture_names()
            .iter()
            .find(|name| !from.capture_names().contains(name))
        {
            bail!(
                "invalid {CONFIG_FILE_NAME} at line {line}: [[link]] to uses {{{name}}}, which from does not capture"
            );
        }

        to.push(target);
    }

    Ok(LinkRule { line, from, to })
}

fn ignore_rule(line: usize, ignore_table: &IgnoreTable) -> Result<IgnoreRule> {
    let target = match (&ignore_table.from, &ignore_table.to, &ignore_table.site) {
        (Some(from), Some(to), None) => IgnoreTarget::Links {
            from: PathPattern::new(from).map_err(rule_error(Some(line), "[[ignore]]"))?,
            to: PathPattern::new(to).map_err(rule_error(Some(line), "[[ignore]]"))?,
        },
        (None, None, Some(site)) => {
            let (file, site_line) = match site.rsplit_once(':') {
                Some((file, site_line))
                    if site_line.chars().all(|digit| digit.is_ascii_digit()) =>
                {
                    (file, site_line.parse().ok())
                }
                _ => (site.as_str(), None),
            };

            IgnoreTarget::Sites {
                file: PathPattern::new(file).map_err(rule_error(Some(line), "[[ignore]]"))?,
                line: site_line,
            }
        }
        _ => bail!(
            "invalid {CONFIG_FILE_NAME} at line {line}: [[ignore]] needs either from and to, or site"
        ),
    };

    Ok(IgnoreRule { line, target })
}

fn rule_error(line: Option<usize>, rule: &'static str) -> impl Fn(anyhow::Error) -> anyhow::Error {
    move |pattern_error| match line {
        Some(line) => {
            anyhow::anyhow!("invalid {CONFIG_FILE_NAME} at line {line}: {rule} {pattern_error}")
        }
        None => anyhow::anyhow!("invalid {CONFIG_FILE_NAME}: {rule} {pattern_error}"),
    }
}

fn table_line<T>(config_text: &str, table: &Spanned<T>, header: &str) -> usize {
    let span = table.span();
    let end = span.end.min(config_text.len());

    config_text
        .get(..end)
        .and_then(|before| before.rfind(header))
        .map_or_else(
            || line_of(config_text, span.start),
            |offset| line_of(config_text, offset),
        )
}

fn line_of(text: &str, byte_offset: usize) -> usize {
    text.get(..byte_offset)
        .unwrap_or(text)
        .matches('\n')
        .count()
        + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::ChangeKind;
    use crate::config::Config;
    use crate::graph::{Edge, EdgeKind, Inference, UnresolvedCause, UnresolvedSite};

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn repo_files(paths: &[&str]) -> Vec<RepoPath> {
        paths.iter().map(|path| repo_path(path)).collect()
    }

    fn rules(config_text: &str) -> Rules {
        Config::parse(config_text).unwrap().rules
    }

    fn dependents(graph: &Graph, dependency: &str) -> Vec<(String, EdgeKind)> {
        let Some(file) = graph.file_id(&repo_path(dependency)) else {
            return Vec::new();
        };

        let mut dependents: Vec<(String, EdgeKind)> = graph
            .dependents(file)
            .map(|edge| (graph.path(edge.dependent).to_string(), edge.kind))
            .collect();

        dependents.sort();

        dependents
    }

    fn plugin_project() -> (GraphBuilder, Vec<RepoPath>) {
        let files = repo_files(&[
            "src/loader.ts",
            "src/loader.test.ts",
            "extensions/telegram/index.ts",
            "extensions/slack/index.ts",
            "test/plugins/telegram.test.ts",
            "test/plugins/slack.test.ts",
            "fixtures/golden/a.json",
        ]);
        let mut builder = GraphBuilder::default();
        let loader = builder.add_file(repo_path("src/loader.ts"));
        let loader_test = builder.add_test_file(repo_path("src/loader.test.ts"), "typescript");
        let telegram = builder.add_file(repo_path("extensions/telegram/index.ts"));

        builder.add_test_file(repo_path("test/plugins/telegram.test.ts"), "typescript");
        builder.add_test_file(repo_path("test/plugins/slack.test.ts"), "typescript");
        builder.add_edge(Edge {
            dependent: loader_test,
            dependency: loader,
            kind: EdgeKind::Import,
        });
        builder.add_inferred_edge(
            Edge {
                dependent: loader_test,
                dependency: telegram,
                kind: EdgeKind::Reads,
            },
            Inference {
                line: 3,
                kind: "file path",
                pattern: "extensions/telegram/index.ts".to_owned(),
                detail: None,
            },
        );
        builder.add_unresolved_site(UnresolvedSite {
            file: loader,
            line: 12,
            expression: "import(entry)".to_owned(),
            cause: UnresolvedCause::Parameter,
            language: "typescript",
        });

        (builder, files)
    }

    #[test]
    fn rules_parse_with_the_line_of_their_table() {
        let rules = rules(
            "[always_run]\ntests = [\"test/smoke/**\"]\n\n[[link]]\nfrom = \"fixtures/**\"\nto = \"test/golden.test.ts\"\n\n[[ignore]]\nsite = \"src/loader.ts:12\"\n",
        );

        assert_eq!(rules.links[0].line, 4);
        assert_eq!(rules.links[0].from.text(), "fixtures/**");
        assert_eq!(rules.ignores[0].line, 8);
        assert!(matches!(
            &rules.ignores[0].target,
            IgnoreTarget::Sites { file, line: Some(12) } if file.text() == "src/loader.ts"
        ));
        assert_eq!(rules.always_run[0].text(), "test/smoke/**");
    }

    #[test]
    fn malformed_rules_are_errors_with_their_line() {
        let invalid_configs = [
            (
                "[[link]]\nfrom = \"a/**\"\nto = \"test/{id}.test.ts\"\n",
                "invalid prober.toml at line 1: [[link]] to uses {id}, which from does not capture",
            ),
            (
                "\n[[ignore]]\nfrom = \"a/**\"\n",
                "invalid prober.toml at line 2: [[ignore]] needs either from and to, or site",
            ),
            (
                "[[link]]\nfrom = \"src/[a\"\nto = \"b\"\n",
                "invalid prober.toml at line 1: [[link]] invalid pattern \"src/[a\": unclosed character class; missing ']'",
            ),
            (
                "[[link]]\nfrom = \"a\"\n",
                "invalid prober.toml at line 1, column 1: missing field `to`",
            ),
            (
                "[[link]]\nfrom = \"a/**\"\nto = []\n",
                "invalid prober.toml at line 1: [[link]] to lists no target",
            ),
            (
                "[[link]]\nfrom = \"extensions/{id}/**\"\nto = [\"test/a.test.ts\", \"literal:pluginId: \\\"{id}\\\"\"]\n",
                "invalid prober.toml at line 1: [[link]] literal: targets are not supported yet, they come with string anchors",
            ),
        ];

        for (config_text, expected_message) in invalid_configs {
            assert_eq!(
                Config::parse(config_text).unwrap_err().to_string(),
                expected_message
            );
        }
    }

    #[test]
    fn links_connect_captured_files_and_resolve_the_unresolved_sites_they_declare() {
        let (mut builder, files) = plugin_project();
        let rules = rules(
            "[[link]]\nfrom = \"extensions/{id}/**\"\nto = \"test/plugins/{id}.test.ts\"\n\n[[link]]\nfrom = \"extensions/*/index.ts\"\nto = \"src/loader.ts\"\n",
        );
        let rule_report = rules.apply(&mut builder, &files, &[]);
        let graph = builder.build();

        assert_eq!(
            dependents(&graph, "extensions/slack/index.ts"),
            [
                ("src/loader.ts".to_owned(), EdgeKind::Rule),
                ("test/plugins/slack.test.ts".to_owned(), EdgeKind::Rule),
            ]
        );
        assert_eq!(graph.unresolved_sites(), []);
        assert_eq!(
            graph.set_aside_sites()[0].set_aside,
            SetAside::Resolved { line: 5 }
        );
        assert_eq!(
            rule_report.effects,
            [
                RuleEffect {
                    line: 1,
                    rule: "[[link]] from \"extensions/{id}/**\" to \"test/plugins/{id}.test.ts\""
                        .to_owned(),
                    links: 2,
                    sites: 0,
                    unlinked: Vec::new(),
                },
                RuleEffect {
                    line: 5,
                    rule: "[[link]] from \"extensions/*/index.ts\" to \"src/loader.ts\"".to_owned(),
                    links: 2,
                    sites: 1,
                    unlinked: Vec::new(),
                },
            ]
        );
        assert_eq!(rule_report.warnings, []);
    }

    #[test]
    fn links_reach_files_removed_by_the_changes() {
        let (mut builder, files) = plugin_project();
        let rules = rules("[[link]]\nfrom = \"fixtures/golden/**\"\nto = \"src/loader.test.ts\"\n");
        let removed = FileChange {
            path: repo_path("fixtures/golden/b.json"),
            kind: ChangeKind::Deleted,
        };

        rules.apply(&mut builder, &files, &[removed]);

        let graph = builder.build();

        assert_eq!(
            dependents(&graph, "fixtures/golden/b.json"),
            [("src/loader.test.ts".to_owned(), EdgeKind::Rule)]
        );
    }

    #[test]
    fn a_link_to_several_targets_connects_each_capture_to_all_of_them() {
        let (mut builder, mut files) = plugin_project();

        files.push(repo_path("extensions/zalo/index.ts"));

        let rules = rules(
            "[[link]]\nfrom = \"extensions/{id}/**\"\nto = [\"test/plugins/{id}.test.ts\", \"src/loader.test.ts\"]\n",
        );
        let rule_report = rules.apply(&mut builder, &files, &[]);
        let graph = builder.build();

        assert_eq!(
            dependents(&graph, "extensions/slack/index.ts"),
            [
                ("src/loader.test.ts".to_owned(), EdgeKind::Rule),
                ("test/plugins/slack.test.ts".to_owned(), EdgeKind::Rule),
            ]
        );
        assert_eq!(
            dependents(&graph, "extensions/zalo/index.ts"),
            [("src/loader.test.ts".to_owned(), EdgeKind::Rule)]
        );
        assert_eq!(rule_report.effects[0].links, 5);
        assert_eq!(rule_report.effects[0].unlinked, Vec::<String>::new());
    }

    #[test]
    fn capture_values_whose_targets_match_nothing_are_reported() {
        let (mut builder, mut files) = plugin_project();

        files.push(repo_path("extensions/zalo/index.ts"));

        let rule_report =
            rules("[[link]]\nfrom = \"extensions/{id}/**\"\nto = \"test/plugins/{id}.test.ts\"\n")
                .apply(&mut builder, &files, &[]);

        assert_eq!(rule_report.effects[0].links, 2);
        assert_eq!(rule_report.effects[0].unlinked, ["id=zalo"]);
        assert_eq!(dependents(&builder.build(), "extensions/zalo/index.ts"), []);
    }

    #[test]
    fn ignores_set_aside_links_of_every_kind_and_unresolved_sites() {
        let (mut builder, files) = plugin_project();
        let rules = rules(
            "[[link]]\nfrom = \"extensions/{id}/**\"\nto = \"test/plugins/{id}.test.ts\"\n\n[[ignore]]\nfrom = \"**\"\nto = \"src/loader.test.ts\"\n\n[[ignore]]\nsite = \"src/loader.ts\"\n\n[[ignore]]\nfrom = \"extensions/slack/**\"\nto = \"test/**\"\n",
        );
        let rule_report = rules.apply(&mut builder, &files, &[]);
        let graph = builder.build();
        let telegram = graph
            .file_id(&repo_path("extensions/telegram/index.ts"))
            .unwrap();
        let slack = graph
            .file_id(&repo_path("extensions/slack/index.ts"))
            .unwrap();

        assert_eq!(
            dependents(&graph, "extensions/telegram/index.ts"),
            [("test/plugins/telegram.test.ts".to_owned(), EdgeKind::Rule)]
        );
        assert_eq!(dependents(&graph, "src/loader.ts"), []);
        assert_eq!(dependents(&graph, "extensions/slack/index.ts"), []);
        assert_eq!(
            graph
                .ignored_dependents(telegram)
                .map(|ignored_edge| ignored_edge.line)
                .collect::<Vec<_>>(),
            [5]
        );
        assert_eq!(
            graph
                .ignored_dependents(slack)
                .map(|ignored_edge| (ignored_edge.edge.kind, ignored_edge.line))
                .collect::<Vec<_>>(),
            [(EdgeKind::Rule, 12)]
        );
        assert_eq!(graph.unresolved_sites(), []);
        assert_eq!(
            graph.set_aside_sites()[0].set_aside,
            SetAside::Ignored { line: 9 }
        );
        assert_eq!(
            rule_report
                .effects
                .iter()
                .map(|effect| (effect.links, effect.sites))
                .collect::<Vec<_>>(),
            [(2, 0), (2, 0), (0, 1), (1, 0)]
        );
        assert_eq!(
            rule_report
                .warnings
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "prober.toml:5: [[ignore]] from \"**\" to \"src/loader.test.ts\" removes 1 import edges written in the code: tests reached only through them are no longer selected"
            ]
        );
    }

    #[test]
    fn rules_matching_nothing_or_almost_everything_are_warnings() {
        let (mut builder, mut files) = plugin_project();

        files.extend(repo_files(&["a.md", "b.md", "c.md", "d.md"]));

        let rules = rules(
            "[[link]]\nfrom = \"fixtures/missing/**\"\nto = \"src/loader.test.ts\"\n\n[[ignore]]\nfrom = \"fixtures/**\"\nto = \"src/loader.ts\"\n\n[[ignore]]\nsite = \"src/other.ts\"\n\n[always_run]\ntests = [\"test/smoke/**\", \"**\"]\n",
        );
        let messages: Vec<String> = rules
            .apply(&mut builder, &files, &[])
            .warnings
            .iter()
            .map(ToString::to_string)
            .collect();

        assert_eq!(
            messages,
            [
                "prober.toml:1: from \"fixtures/missing/**\" matches no file",
                "prober.toml:5: [[ignore]] from \"fixtures/**\" to \"src/loader.ts\" matches no link",
                "prober.toml:9: [[ignore]] site \"src/other.ts\" matches no unresolved site",
                "prober.toml:12: [always_run] test \"test/smoke/**\" matches no test file",
            ]
        );
    }

    #[test]
    fn always_run_tests_carry_the_pattern_that_names_them() {
        let (builder, _) = plugin_project();
        let graph = builder.build();
        let always_run_tests = rules("\n[always_run]\ntests = [\"test/plugins/*.test.ts\"]\n")
            .always_run_tests(&graph);

        assert_eq!(
            always_run_tests
                .iter()
                .map(|always_run_test| (
                    always_run_test.path.as_str(),
                    always_run_test.reason.as_str()
                ))
                .collect::<Vec<_>>(),
            [
                (
                    "test/plugins/slack.test.ts",
                    "always run by prober.toml:2 (\"test/plugins/*.test.ts\")"
                ),
                (
                    "test/plugins/telegram.test.ts",
                    "always run by prober.toml:2 (\"test/plugins/*.test.ts\")"
                ),
            ]
        );
    }
}
