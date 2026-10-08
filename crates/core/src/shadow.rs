use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::changes::ResolvedRevisions;
use crate::files::RepoPath;
use crate::outcomes::{TestFileOutcome, TestOutcomes, TestStatus};
use crate::ratios::{format_percentage, rounded_ratio};

const SCHEMA_VERSION: u32 = 1;

const READABLE_SELECTION_SCHEMA_VERSION: u32 = 1;

const LISTED_UNATTRIBUTED_FAILURES: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    Selected,
    All,
    None,
}

impl SelectionMode {
    pub fn name(self) -> &'static str {
        match self {
            SelectionMode::Selected => "selected",
            SelectionMode::All => "all",
            SelectionMode::None => "none",
        }
    }

    fn parse(name: &str) -> Result<Self> {
        match name {
            "selected" => Ok(SelectionMode::Selected),
            "all" => Ok(SelectionMode::All),
            "none" => Ok(SelectionMode::None),
            unknown => bail!("invalid selection JSON: unknown mode {unknown:?}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ModeReasonRecord {
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedSelection {
    pub revisions: Option<ResolvedRevisions>,
    pub mode: SelectionMode,
    pub mode_reasons: Vec<ModeReasonRecord>,
    pub test_files: BTreeSet<RepoPath>,
    pub distances: BTreeMap<RepoPath, usize>,
}

#[derive(Deserialize)]
struct SelectionJson {
    schema_version: u32,

    #[serde(default)]
    revisions: Option<ResolvedRevisions>,

    mode: String,

    #[serde(default)]
    mode_reasons: Option<Vec<ModeReasonRecord>>,

    #[serde(default)]
    targets: BTreeMap<String, Vec<String>>,

    #[serde(default)]
    distances: BTreeMap<String, usize>,
}

impl RecordedSelection {
    pub fn parse(selection_json: &str) -> Result<Self> {
        let selection: SelectionJson = serde_json::from_str(selection_json)
            .map_err(|error| anyhow!("invalid selection JSON: {error}"))?;

        if selection.schema_version != READABLE_SELECTION_SCHEMA_VERSION {
            bail!(
                "unsupported selection schema_version {}: this prober reads version {READABLE_SELECTION_SCHEMA_VERSION}",
                selection.schema_version
            );
        }

        let test_files = selection
            .targets
            .into_values()
            .flatten()
            .map(RepoPath::new)
            .collect::<Result<BTreeSet<_>>>()?;
        let distances = selection
            .distances
            .into_iter()
            .map(|(path, distance)| Ok((RepoPath::new(path)?, distance)))
            .collect::<Result<BTreeMap<_, _>>>()?;

        Ok(Self {
            revisions: selection.revisions,
            mode: SelectionMode::parse(&selection.mode)?,
            mode_reasons: selection.mode_reasons.unwrap_or_default(),
            test_files,
            distances,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedTestFile {
    pub file: RepoPath,
    pub selected: bool,
    pub distance: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowResult {
    pub revisions: Option<ResolvedRevisions>,
    pub selection_mode: SelectionMode,
    pub mode_reasons: Vec<ModeReasonRecord>,
    pub reported_test_files: usize,
    pub selected_test_files: usize,
    pub selected_but_not_reported: usize,
    pub skipped_test_files: usize,
    pub reported_duration: Duration,
    pub skipped_duration: Duration,
    pub failed_test_files: Vec<FailedTestFile>,
    pub flaky_test_files: Vec<RepoPath>,
    pub unattributed_failures: Vec<String>,
}

impl ShadowResult {
    pub fn compare(selection: &RecordedSelection, outcomes: &TestOutcomes) -> Self {
        let is_selected = |file: &RepoPath| selection.test_files.contains(file);
        let reported_outcomes: Vec<&TestFileOutcome> = outcomes.files().collect();
        let skipped_outcomes: Vec<&TestFileOutcome> = reported_outcomes
            .iter()
            .copied()
            .filter(|outcome| !is_selected(&outcome.file))
            .collect();
        let failed_test_files = reported_outcomes
            .iter()
            .filter(|outcome| outcome.status == TestStatus::Failed)
            .map(|outcome| FailedTestFile {
                file: outcome.file.clone(),
                selected: is_selected(&outcome.file),
                distance: selection.distances.get(&outcome.file).copied(),
            })
            .collect();
        let flaky_test_files = reported_outcomes
            .iter()
            .filter(|outcome| outcome.status == TestStatus::Flaky)
            .map(|outcome| outcome.file.clone())
            .collect();
        let unattributed_failures = outcomes
            .unattributed_test_cases()
            .iter()
            .filter(|test_case| test_case.status == TestStatus::Failed)
            .map(|test_case| test_case.name.clone())
            .collect();

        Self {
            revisions: selection.revisions.clone(),
            selection_mode: selection.mode,
            mode_reasons: selection.mode_reasons.clone(),
            reported_test_files: reported_outcomes.len(),
            selected_test_files: selection.test_files.len(),
            selected_but_not_reported: selection
                .test_files
                .iter()
                .filter(|file| outcomes.outcome(file).is_none())
                .count(),
            skipped_test_files: skipped_outcomes.len(),
            reported_duration: reported_outcomes
                .iter()
                .map(|outcome| outcome.duration)
                .sum(),
            skipped_duration: skipped_outcomes
                .iter()
                .map(|outcome| outcome.duration)
                .sum(),
            failed_test_files,
            flaky_test_files,
            unattributed_failures,
        }
    }

    pub fn misses(&self) -> impl Iterator<Item = &FailedTestFile> {
        self.failed_test_files
            .iter()
            .filter(|failed_test_file| !failed_test_file.selected)
    }

    pub fn to_json(&self) -> String {
        let shadow_json = ShadowJson {
            schema_version: SCHEMA_VERSION,
            revisions: self.revisions.as_ref(),
            selection_mode: self.selection_mode.name(),
            mode_reasons: &self.mode_reasons,
            test_files: TestFilesJson {
                reported: self.reported_test_files,
                selected: self.selected_test_files,
                selected_but_not_reported: self.selected_but_not_reported,
                skipped: self.skipped_test_files,
            },
            savings: SavingsJson {
                test_files_ratio: rounded_ratio(
                    self.skipped_test_files as f64,
                    self.reported_test_files as f64,
                ),
                seconds: rounded_seconds(self.skipped_duration),
                seconds_ratio: rounded_ratio(
                    self.skipped_duration.as_secs_f64(),
                    self.reported_duration.as_secs_f64(),
                ),
            },
            misses: self.misses().map(|miss| miss.file.as_str()).collect(),
            failures: self
                .failed_test_files
                .iter()
                .map(|failed_test_file| FailureJson {
                    file: failed_test_file.file.as_str(),
                    selected: failed_test_file.selected,
                    distance: failed_test_file.distance,
                })
                .collect(),
            flaky: self.flaky_test_files.iter().map(RepoPath::as_str).collect(),
            unattributed_failures: &self.unattributed_failures,
            warnings: self.warnings(),
        };
        let mut json =
            serde_json::to_string_pretty(&shadow_json).expect("shadow result serializes to JSON");

        json.push('\n');

        json
    }

    pub fn to_markdown(&self) -> String {
        let mut markdown = format!(
            "### Prober shadow\n\n**{}** — {}.\n",
            capitalized(&self.headline()),
            self.selection_summary()
        );

        if !self.mode_reasons.is_empty() {
            let mode_reasons: Vec<&str> = self
                .mode_reasons
                .iter()
                .map(|mode_reason| mode_reason.message.as_str())
                .collect();

            markdown.push_str(&format!(
                "\nSelection mode `{}`: {}.\n",
                self.selection_mode.name(),
                mode_reasons.join("; ")
            ));
        }

        if !self.failed_test_files.is_empty() {
            markdown
                .push_str("\n| Failed test file | Selected | Distance |\n| --- | --- | --- |\n");

            for failed_test_file in &self.failed_test_files {
                let selected = if failed_test_file.selected {
                    "yes"
                } else {
                    "**no, miss**"
                };
                let distance = failed_test_file
                    .distance
                    .map_or_else(|| "—".to_owned(), |distance| distance.to_string());

                markdown.push_str(&format!(
                    "| `{}` | {selected} | {distance} |\n",
                    failed_test_file.file
                ));
            }
        }

        if !self.flaky_test_files.is_empty() {
            let flaky_files: Vec<String> = self
                .flaky_test_files
                .iter()
                .map(|file| format!("`{file}`"))
                .collect();

            markdown.push_str(&format!(
                "\nFlaky, not counted as misses: {}.\n",
                flaky_files.join(", ")
            ));
        }

        for warning in self.warnings() {
            markdown.push_str(&format!("\n> **Warning**: {warning}.\n"));
        }

        markdown
    }

    pub fn summary_line(&self) -> String {
        format!(
            "prober shadow: {} — {}",
            self.headline(),
            self.selection_summary()
        )
    }

    fn headline(&self) -> String {
        let miss_count = self.misses().count();
        let unchecked_failure_count = self.unattributed_failures.len();

        if miss_count > 0 {
            return count_of(miss_count, "miss", "misses");
        }

        if unchecked_failure_count > 0 {
            return format!(
                "no miss among checked failures, {} unchecked",
                count_of(unchecked_failure_count, "failure", "failures")
            );
        }

        "no miss".to_owned()
    }

    fn selection_summary(&self) -> String {
        format!(
            "{} of {} reported test files selected, {} skipped ({}, {} of {})",
            self.selected_test_files - self.selected_but_not_reported,
            self.reported_test_files,
            self.skipped_test_files,
            format_percentage(
                self.skipped_test_files as f64,
                self.reported_test_files as f64
            ),
            format_seconds(self.skipped_duration),
            format_seconds(self.reported_duration)
        )
    }

    fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        let nothing_selected_was_reported = self.selected_test_files > 0
            && self.selected_but_not_reported == self.selected_test_files
            && self.reported_test_files > 0;

        if self.reported_test_files == 0 {
            warnings.push("the test reports contain no test file".to_owned());
        }

        if nothing_selected_was_reported {
            warnings.push(
                "none of the selected test files appear in the test reports: report paths must be relative to the repository root or to the runner directory"
                    .to_owned(),
            );
        }

        if !self.unattributed_failures.is_empty() {
            let listed_failures: Vec<String> = self
                .unattributed_failures
                .iter()
                .take(LISTED_UNATTRIBUTED_FAILURES)
                .map(|name| format!("`{name}`"))
                .collect();
            let unlisted_failure_count = self
                .unattributed_failures
                .len()
                .saturating_sub(LISTED_UNATTRIBUTED_FAILURES);
            let and_more = if unlisted_failure_count > 0 {
                format!(" and {unlisted_failure_count} more")
            } else {
                String::new()
            };

            warnings.push(format!(
                "{} cannot be matched to a test file ({}{and_more}): configure the JUnit reporter to write file paths (jest-junit: `addFileAttribute` and `reportTestSuiteErrors`)",
                count_of(
                    self.unattributed_failures.len(),
                    "failed test case",
                    "failed test cases"
                ),
                listed_failures.join(", ")
            ));
        }

        warnings
    }
}

#[derive(Serialize)]
struct ShadowJson<'a> {
    schema_version: u32,
    revisions: Option<&'a ResolvedRevisions>,
    selection_mode: &'static str,
    mode_reasons: &'a [ModeReasonRecord],
    test_files: TestFilesJson,
    savings: SavingsJson,
    misses: Vec<&'a str>,
    failures: Vec<FailureJson<'a>>,
    flaky: Vec<&'a str>,
    unattributed_failures: &'a [String],
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct TestFilesJson {
    reported: usize,
    selected: usize,
    selected_but_not_reported: usize,
    skipped: usize,
}

#[derive(Serialize)]
struct SavingsJson {
    test_files_ratio: f64,
    seconds: f64,
    seconds_ratio: f64,
}

#[derive(Serialize)]
struct FailureJson<'a> {
    file: &'a str,
    selected: bool,
    distance: Option<usize>,
}

fn count_of(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        return format!("1 {singular}");
    }

    format!("{count} {plural}")
}

fn capitalized(text: &str) -> String {
    let mut characters = text.chars();

    match characters.next() {
        Some(first_character) => first_character.to_uppercase().chain(characters).collect(),
        None => String::new(),
    }
}

fn rounded_seconds(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 1_000.0).round() / 1_000.0
}

fn format_seconds(duration: Duration) -> String {
    format!("{:.1} s", duration.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::{ChangeKind, FileChange};
    use crate::graph::{Edge, EdgeKind, GraphBuilder};
    use crate::report::Report;
    use crate::select::select_tests;

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn seconds(count: u64) -> Duration {
        Duration::from_secs(count)
    }

    fn outcomes_of(test_files: &[(&str, TestStatus, u64)]) -> TestOutcomes {
        let mut outcomes = TestOutcomes::default();

        for &(file, status, duration_in_seconds) in test_files {
            outcomes.record(repo_path(file), status, seconds(duration_in_seconds));
        }

        outcomes
    }

    fn selection_of_two_tests() -> RecordedSelection {
        RecordedSelection::parse(
            r#"{
              "schema_version": 1,
              "mode": "selected",
              "mode_reasons": null,
              "targets": { "typescript": ["src/a.test.ts", "src/b.test.ts"] },
              "reasons": {},
              "distances": { "src/a.test.ts": 1, "src/b.test.ts": 2 },
              "stats": { "tests_total": 5 }
            }"#,
        )
        .unwrap()
    }

    fn full_run_with_a_miss() -> TestOutcomes {
        outcomes_of(&[
            ("src/a.test.ts", TestStatus::Failed, 2),
            ("src/b.test.ts", TestStatus::Passed, 3),
            ("src/c.test.ts", TestStatus::Failed, 4),
            ("src/d.test.ts", TestStatus::Passed, 10),
            ("src/e.test.ts", TestStatus::Flaky, 1),
        ])
    }

    #[test]
    fn selected_failures_carry_their_distance_and_unselected_failures_are_misses() {
        let shadow_result =
            ShadowResult::compare(&selection_of_two_tests(), &full_run_with_a_miss());

        assert_eq!(
            shadow_result.to_json(),
            r#"{
  "schema_version": 1,
  "revisions": null,
  "selection_mode": "selected",
  "mode_reasons": [],
  "test_files": {
    "reported": 5,
    "selected": 2,
    "selected_but_not_reported": 0,
    "skipped": 3
  },
  "savings": {
    "test_files_ratio": 0.6,
    "seconds": 15.0,
    "seconds_ratio": 0.75
  },
  "misses": [
    "src/c.test.ts"
  ],
  "failures": [
    {
      "file": "src/a.test.ts",
      "selected": true,
      "distance": 1
    },
    {
      "file": "src/c.test.ts",
      "selected": false,
      "distance": null
    }
  ],
  "flaky": [
    "src/e.test.ts"
  ],
  "unattributed_failures": [],
  "warnings": []
}
"#
        );
    }

    #[test]
    fn markdown_summary_lists_failures_with_their_selection_and_distance() {
        let shadow_result =
            ShadowResult::compare(&selection_of_two_tests(), &full_run_with_a_miss());

        assert_eq!(
            shadow_result.to_markdown(),
            "### Prober shadow

**1 miss** — 2 of 5 reported test files selected, 3 skipped (60.0%, 15.0 s of 20.0 s).

| Failed test file | Selected | Distance |
| --- | --- | --- |
| `src/a.test.ts` | yes | 1 |
| `src/c.test.ts` | **no, miss** | — |

Flaky, not counted as misses: `src/e.test.ts`.
"
        );
        assert_eq!(
            shadow_result.summary_line(),
            "prober shadow: 1 miss — 2 of 5 reported test files selected, 3 skipped (60.0%, 15.0 s of 20.0 s)"
        );
    }

    #[test]
    fn selection_running_every_test_has_no_miss_and_no_savings() {
        let selection = RecordedSelection::parse(
            r#"{
              "schema_version": 1,
              "mode": "all",
              "mode_reasons": [
                { "kind": "always_all", "file": "ci/deploy.sh", "pattern": "ci/**", "message": "ci/deploy.sh (modified) matches always_all \"ci/**\"" }
              ],
              "targets": { "typescript": ["src/a.test.ts", "src/b.test.ts"] },
              "distances": {}
            }"#,
        )
        .unwrap();
        let outcomes = outcomes_of(&[
            ("src/a.test.ts", TestStatus::Failed, 1),
            ("src/b.test.ts", TestStatus::Passed, 1),
        ]);
        let shadow_result = ShadowResult::compare(&selection, &outcomes);

        assert_eq!(
            shadow_result.to_markdown(),
            "### Prober shadow

**No miss** — 2 of 2 reported test files selected, 0 skipped (0.0%, 0.0 s of 2.0 s).

Selection mode `all`: ci/deploy.sh (modified) matches always_all \"ci/**\".

| Failed test file | Selected | Distance |
| --- | --- | --- |
| `src/a.test.ts` | yes | — |
"
        );
    }

    #[test]
    fn selection_of_no_test_misses_every_failure() {
        let selection = RecordedSelection::parse(
            r#"{
              "schema_version": 1,
              "mode": "none",
              "mode_reasons": [{ "kind": "no_impact", "message": "1 changed file reaches no test file" }],
              "targets": {},
              "distances": {}
            }"#,
        )
        .unwrap();
        let outcomes = outcomes_of(&[("src/b.test.ts", TestStatus::Failed, 2)]);
        let shadow_result = ShadowResult::compare(&selection, &outcomes);

        assert_eq!(
            shadow_result.summary_line(),
            "prober shadow: 1 miss — 0 of 1 reported test files selected, 1 skipped (100.0%, 2.0 s of 2.0 s)"
        );
    }

    #[test]
    fn failures_without_a_test_file_are_listed_with_the_reporter_fix() {
        let mut outcomes = outcomes_of(&[("src/a.test.ts", TestStatus::Passed, 1)]);

        for test_case_number in 1..=7 {
            outcomes.record_unattributed(format!("case {test_case_number}"), TestStatus::Failed);
        }

        outcomes.record_unattributed("passing case".to_owned(), TestStatus::Passed);

        let shadow_result = ShadowResult::compare(&selection_of_two_tests(), &outcomes);

        assert_eq!(
            shadow_result.summary_line(),
            "prober shadow: no miss among checked failures, 7 failures unchecked — 1 of 1 reported test files selected, 0 skipped (0.0%, 0.0 s of 1.0 s)"
        );
        assert_eq!(
            shadow_result.warnings(),
            [
                "7 failed test cases cannot be matched to a test file (`case 1`, `case 2`, `case 3`, `case 4`, `case 5` and 2 more): configure the JUnit reporter to write file paths (jest-junit: `addFileAttribute` and `reportTestSuiteErrors`)"
            ]
        );
    }

    #[test]
    fn report_paths_matching_no_selected_test_file_are_flagged() {
        let outcomes = outcomes_of(&[("web/src/a.test.ts", TestStatus::Passed, 1)]);
        let shadow_result = ShadowResult::compare(&selection_of_two_tests(), &outcomes);

        assert_eq!(
            shadow_result.warnings(),
            [
                "none of the selected test files appear in the test reports: report paths must be relative to the repository root or to the runner directory"
            ]
        );
    }

    #[test]
    fn empty_test_reports_are_flagged() {
        let shadow_result =
            ShadowResult::compare(&selection_of_two_tests(), &TestOutcomes::default());

        assert_eq!(
            shadow_result.warnings(),
            ["the test reports contain no test file"]
        );
    }

    #[test]
    fn selection_json_from_another_schema_version_or_malformed_is_rejected() {
        let other_version =
            RecordedSelection::parse(r#"{ "schema_version": 2, "mode": "selected" }"#).unwrap_err();
        let malformed = RecordedSelection::parse(r#"{ "schema_version": 1, "mode": "#).unwrap_err();
        let unknown_mode =
            RecordedSelection::parse(r#"{ "schema_version": 1, "mode": "some" }"#).unwrap_err();

        assert_eq!(
            other_version.to_string(),
            "unsupported selection schema_version 2: this prober reads version 1"
        );
        assert!(
            malformed
                .to_string()
                .starts_with("invalid selection JSON: "),
            "{malformed}"
        );
        assert!(!malformed.to_string().contains('\n'));
        assert_eq!(
            unknown_mode.to_string(),
            "invalid selection JSON: unknown mode \"some\""
        );
    }

    #[test]
    fn selection_written_by_the_report_reads_back_with_its_distances() {
        let mut builder = GraphBuilder::default();
        let test_file = builder.add_test_file(repo_path("src/b.test.ts"), "typescript");
        let source_file = builder.add_file(repo_path("src/b.ts"));

        builder.add_edge(Edge {
            dependent: test_file,
            dependency: source_file,
            kind: EdgeKind::Import,
        });

        let graph = builder.build();
        let changes = [FileChange {
            path: repo_path("src/b.ts"),
            kind: ChangeKind::Modified,
        }];
        let selected_tests = select_tests(&graph, &changes);
        let revisions = ResolvedRevisions {
            base: "1111111".to_owned(),
            head: "2222222".to_owned(),
            merge_base: "1111111".to_owned(),
        };
        let report = Report::from_selection(&graph, &changes, &selected_tests, "hash".to_owned())
            .with_revisions(revisions.clone());
        let selection = RecordedSelection::parse(&report.to_json()).unwrap();

        assert_eq!(selection.revisions, Some(revisions));
        assert_eq!(selection.mode, SelectionMode::Selected);
        assert_eq!(
            selection.test_files,
            BTreeSet::from([repo_path("src/b.test.ts")])
        );
        assert_eq!(
            selection.distances,
            BTreeMap::from([(repo_path("src/b.test.ts"), 1)])
        );
    }
}
