//! Shadow runs read together: modes and the causes of full runs, savings, misses, failure distances, and the aggressive option values their failures support.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::aggressive::{Aggressive, DEFER, HUB_FANOUT_LIMIT, MAX_DISTANCE};
use crate::changes::ResolvedRevisions;
use crate::patterns::PathPattern;
use crate::ratios::{format_percentage, rounded_ratio};
use crate::shadow::{ModeReasonRecord, capitalized, count_of};

const SCHEMA_VERSION: u32 = 1;

const READABLE_SHADOW_SCHEMA_VERSION: u32 = 1;

pub const MIN_FAILURES: usize = 20;

const SHORT_COMMIT_LENGTH: usize = 7;

#[derive(Debug, Deserialize)]
struct ShadowRun {
    schema_version: u32,

    #[serde(default)]
    revisions: Option<ResolvedRevisions>,

    selection_mode: String,

    #[serde(default)]
    mode_reasons: Vec<ModeReasonRecord>,

    test_files: TestFilesRecord,

    #[serde(default)]
    reported_seconds: Option<f64>,

    savings: SavingsRecord,

    #[serde(default)]
    failures: Vec<FailureRecord>,

    #[serde(default)]
    flaky: Vec<String>,

    #[serde(default)]
    unattributed_failures: Vec<String>,

    #[serde(default)]
    impacted: Vec<ImpactedRecord>,
}

#[derive(Debug, Deserialize)]
struct TestFilesRecord {
    reported: usize,
    skipped: usize,
}

#[derive(Debug, Deserialize)]
struct SavingsRecord {
    seconds: f64,
    seconds_ratio: f64,
}

#[derive(Debug, Deserialize)]
struct FailureRecord {
    file: String,
    selected: bool,

    #[serde(default)]
    distance: Option<usize>,

    #[serde(default)]
    hub_fanout: Option<usize>,

    #[serde(default)]
    dropped: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ImpactedRecord {
    file: String,
    distance: usize,

    #[serde(default)]
    hub_fanout: Option<usize>,

    #[serde(default)]
    seconds: Option<f64>,
}

impl ShadowRun {
    fn reported_seconds(&self) -> f64 {
        match self.reported_seconds {
            Some(reported_seconds) => reported_seconds,
            None if self.savings.seconds_ratio > 0.0 => {
                self.savings.seconds / self.savings.seconds_ratio
            }
            None => 0.0,
        }
    }

    fn head(&self) -> Option<&str> {
        self.revisions
            .as_ref()
            .map(|revisions| &revisions.head[..revisions.head.len().min(SHORT_COMMIT_LENGTH)])
    }
}

#[derive(Debug, Default)]
pub struct ShadowRuns {
    runs: Vec<ShadowRun>,
    seen: HashSet<(String, String)>,
    duplicate_runs: usize,
}

impl ShadowRuns {
    pub fn add(&mut self, shadow_json: &str) -> Result<()> {
        let run: ShadowRun = serde_json::from_str(shadow_json)
            .map_err(|error| anyhow!("invalid shadow JSON: {error}"))?;

        if run.schema_version != READABLE_SHADOW_SCHEMA_VERSION {
            bail!(
                "unsupported shadow schema_version {}: this prober reads version {READABLE_SHADOW_SCHEMA_VERSION}",
                run.schema_version
            );
        }

        if let Some(revisions) = &run.revisions
            && !self
                .seen
                .insert((revisions.base.clone(), revisions.head.clone()))
        {
            self.duplicate_runs += 1;

            return Ok(());
        }

        self.runs.push(run);

        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    pub fn aggregate(&self, aggressive: &Aggressive) -> Aggregate {
        let mut aggregate = Aggregate {
            runs: self.runs.len(),
            duplicate_runs: self.duplicate_runs,
            ..Aggregate::default()
        };

        for run in &self.runs {
            aggregate.add(run);
        }

        aggregate.recommendations = Recommender {
            runs: &self.runs,
            protected: &aggressive.protected,
        }
        .recommendations(aggressive);

        aggregate
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Miss {
    pub file: String,
    pub head: Option<String>,
    pub dropped: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Recommended {
        value: usize,
        failures: usize,
        dropped_test_files: usize,
        dropped_seconds: f64,
        impacted_test_files: usize,
        impacted_seconds: f64,
    },
    NoSaving {
        value: usize,
        failures: usize,
    },
    NotEnoughFailures {
        failures: usize,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Recommendation {
    pub option: &'static str,
    pub tests: Vec<String>,
    pub verdict: Verdict,
}

impl Recommendation {
    fn label(&self) -> String {
        match self.option {
            DEFER => format!("[[defer]] {}", quoted_list(&self.tests)),
            option => option.to_owned(),
        }
    }

    fn describe(&self) -> String {
        let label = self.label();

        match &self.verdict {
            Verdict::Recommended {
                value,
                failures,
                dropped_test_files,
                dropped_seconds,
                impacted_test_files,
                impacted_seconds,
            } => format!(
                "`{label}`: `{} = {value}` keeps every one of the {} it rests on, and would drop {dropped_test_files} of {impacted_test_files} impacted test files ({} of {})",
                self.value_key(),
                count_of(*failures, "failure", "failures"),
                format_duration(*dropped_seconds),
                format_duration(*impacted_seconds)
            ),
            Verdict::NoSaving { value, failures } => format!(
                "`{label}`: `{} = {value}` keeps every one of the {}, but would drop no test file: keep it off",
                self.value_key(),
                count_of(*failures, "failure", "failures")
            ),
            Verdict::NotEnoughFailures { failures } => format!(
                "`{label}`: not enough failures yet, {failures} of the {MIN_FAILURES} needed"
            ),
        }
    }

    fn value_key(&self) -> &'static str {
        match self.option {
            HUB_FANOUT_LIMIT => HUB_FANOUT_LIMIT,
            _ => MAX_DISTANCE,
        }
    }

    fn recommended_value(&self) -> Option<usize> {
        match self.verdict {
            Verdict::Recommended { value, .. } => Some(value),
            Verdict::NoSaving { .. } | Verdict::NotEnoughFailures { .. } => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct Aggregate {
    pub runs: usize,
    pub duplicate_runs: usize,
    pub runs_by_mode: BTreeMap<String, usize>,
    pub full_run_causes: BTreeMap<String, usize>,
    pub reported_test_files: usize,
    pub skipped_test_files: usize,
    pub reported_seconds: f64,
    pub skipped_seconds: f64,
    pub failed_test_files: usize,
    pub failures_by_distance: BTreeMap<usize, usize>,
    pub failures_in_full_runs: usize,
    pub misses: Vec<Miss>,
    pub flaky_test_files: usize,
    pub unattributed_failures: usize,
    pub recommendations: Vec<Recommendation>,
}

impl Aggregate {
    fn add(&mut self, run: &ShadowRun) {
        *self
            .runs_by_mode
            .entry(run.selection_mode.clone())
            .or_default() += 1;

        if run.selection_mode == "all" {
            let kinds: HashSet<&str> = run
                .mode_reasons
                .iter()
                .map(|mode_reason| mode_reason.kind.as_str())
                .collect();

            for kind in kinds {
                *self.full_run_causes.entry(kind.to_owned()).or_default() += 1;
            }
        }

        self.reported_test_files += run.test_files.reported;
        self.skipped_test_files += run.test_files.skipped;
        self.reported_seconds += run.reported_seconds();
        self.skipped_seconds += run.savings.seconds;
        self.failed_test_files += run.failures.len();
        self.flaky_test_files += run.flaky.len();
        self.unattributed_failures += run.unattributed_failures.len();

        for failure in &run.failures {
            if !failure.selected {
                self.misses.push(Miss {
                    file: failure.file.clone(),
                    head: run.head().map(str::to_owned),
                    dropped: failure.dropped.clone(),
                });
            }

            match failure.distance {
                Some(distance) => *self.failures_by_distance.entry(distance).or_default() += 1,
                None if failure.selected => self.failures_in_full_runs += 1,
                None => {}
            }
        }
    }

    pub fn summary_line(&self) -> String {
        format!(
            "prober shadow: {}, {} — {}",
            count_of(self.runs, "run", "runs"),
            self.headline(),
            self.selection_summary()
        )
    }

    pub fn to_markdown(&self) -> String {
        let mut markdown = format!(
            "### Prober shadow: {}\n\n**{}** — {}.\n",
            count_of(self.runs, "run", "runs"),
            capitalized(&self.headline()),
            self.selection_summary()
        );
        let modes: Vec<String> = ["selected", "none", "all"]
            .iter()
            .filter_map(|&mode| {
                let count = self.runs_by_mode.get(mode)?;

                Some(format!("{count} {mode}"))
            })
            .collect();

        markdown.push_str(&format!("\nRuns by mode: {}.\n", modes.join(", ")));

        if !self.full_run_causes.is_empty() {
            let mut causes: Vec<(&String, &usize)> = self.full_run_causes.iter().collect();

            causes.sort_by(|left, right| right.1.cmp(left.1).then(left.0.cmp(right.0)));

            let causes: Vec<String> = causes
                .iter()
                .map(|(kind, count)| format!("`{kind}` in {}", count_of(**count, "run", "runs")))
                .collect();

            markdown.push_str(&format!(
                "Every test ran because of {}.\n",
                causes.join(", ")
            ));
        }

        if self.duplicate_runs > 0 {
            markdown.push_str(&format!(
                "{} with the same base and head as another read once.\n",
                count_of(self.duplicate_runs, "run", "runs")
            ));
        }

        markdown.push_str(&format!(
            "\nFailed test files: {}, {} missed",
            self.failed_test_files,
            self.misses.len()
        ));

        if self.flaky_test_files > 0 {
            markdown.push_str(&format!("; {} flaky, not counted", self.flaky_test_files));
        }

        if self.unattributed_failures > 0 {
            markdown.push_str(&format!(
                "; {} without a test file",
                count_of(
                    self.unattributed_failures,
                    "failed test case",
                    "failed test cases"
                )
            ));
        }

        markdown.push_str(".\n");

        if !self.failures_by_distance.is_empty() || self.failures_in_full_runs > 0 {
            markdown.push_str("\n| Distance | Failed test files |\n| --- | --- |\n");

            for (distance, count) in &self.failures_by_distance {
                markdown.push_str(&format!("| {distance} | {count} |\n"));
            }

            if self.failures_in_full_runs > 0 {
                markdown.push_str(&format!(
                    "| every test ran | {} |\n",
                    self.failures_in_full_runs
                ));
            }
        }

        if !self.misses.is_empty() {
            markdown.push_str("\n#### Misses\n\n| Test file | Head | Why |\n| --- | --- | --- |\n");

            for miss in &self.misses {
                let why = match &miss.dropped {
                    Some(option) => format!("dropped by `{option}`"),
                    None => "not selected".to_owned(),
                };

                markdown.push_str(&format!(
                    "| `{}` | {} | {why} |\n",
                    miss.file,
                    miss.head.as_deref().unwrap_or("—")
                ));
            }
        }

        markdown.push_str("\n#### Aggressive options\n\n");

        for recommendation in &self.recommendations {
            markdown.push_str(&format!("- {}.\n", recommendation.describe()));
        }

        if let Some(snippet) = self.config_snippet() {
            markdown.push_str(&format!(
                "\nWith a periodic full run on the main branch to catch what they let through:\n\n```toml\n{snippet}```\n"
            ));
        }

        markdown
    }

    pub fn to_json(&self) -> String {
        let aggregate_json = AggregateJson {
            schema_version: SCHEMA_VERSION,
            runs: self.runs,
            duplicate_runs: self.duplicate_runs,
            modes: &self.runs_by_mode,
            full_run_causes: &self.full_run_causes,
            test_files: TestFilesJson {
                reported: self.reported_test_files,
                skipped: self.skipped_test_files,
                skipped_ratio: rounded_ratio(
                    self.skipped_test_files as f64,
                    self.reported_test_files as f64,
                ),
            },
            seconds: SecondsJson {
                reported: rounded(self.reported_seconds),
                skipped: rounded(self.skipped_seconds),
                skipped_ratio: rounded_ratio(self.skipped_seconds, self.reported_seconds),
            },
            failures: FailuresJson {
                total: self.failed_test_files,
                missed: self.misses.len(),
                flaky: self.flaky_test_files,
                unattributed: self.unattributed_failures,
                by_distance: &self.failures_by_distance,
                in_full_runs: self.failures_in_full_runs,
            },
            misses: self
                .misses
                .iter()
                .map(|miss| MissJson {
                    file: &miss.file,
                    head: miss.head.as_deref(),
                    dropped: miss.dropped.as_deref(),
                })
                .collect(),
            recommendations: self
                .recommendations
                .iter()
                .map(RecommendationJson::new)
                .collect(),
        };
        let mut json =
            serde_json::to_string_pretty(&aggregate_json).expect("aggregate serializes to JSON");

        json.push('\n');

        json
    }

    fn headline(&self) -> String {
        match self.misses.len() {
            0 => "no miss".to_owned(),
            miss_count => count_of(miss_count, "miss", "misses"),
        }
    }

    fn selection_summary(&self) -> String {
        format!(
            "{} of {} reported test files selected ({}), {} of {} skipped",
            self.reported_test_files - self.skipped_test_files,
            self.reported_test_files,
            format_percentage(
                (self.reported_test_files - self.skipped_test_files) as f64,
                self.reported_test_files as f64
            ),
            format_duration(self.skipped_seconds),
            format_duration(self.reported_seconds)
        )
    }

    fn config_snippet(&self) -> Option<String> {
        let mut aggressive = String::new();
        let mut defers = String::new();

        for recommendation in &self.recommendations {
            let Some(value) = recommendation.recommended_value() else {
                continue;
            };

            match recommendation.option {
                DEFER => defers.push_str(&format!(
                    "\n[[defer]]\ntests = {}\nmax_distance = {value}\n",
                    toml_patterns(&recommendation.tests)
                )),
                option => aggressive.push_str(&format!("{option} = {value}\n")),
            }
        }

        if aggressive.is_empty() && defers.is_empty() {
            return None;
        }

        let aggressive = if aggressive.is_empty() {
            String::new()
        } else {
            format!("[aggressive]\n{aggressive}")
        };

        Some(format!("{aggressive}{defers}").trim_start().to_owned())
    }
}

struct Recommender<'a> {
    runs: &'a [ShadowRun],
    protected: &'a [PathPattern],
}

impl Recommender<'_> {
    fn recommendations(&self, aggressive: &Aggressive) -> Vec<Recommendation> {
        let mut recommendations = vec![
            self.recommend(
                MAX_DISTANCE,
                Vec::new(),
                |_| true,
                |fact| Some(fact.distance),
            ),
            self.recommend(
                HUB_FANOUT_LIMIT,
                Vec::new(),
                |_| true,
                |fact| fact.hub_fanout,
            ),
        ];

        for defer in &aggressive.defers {
            let tests = defer
                .tests
                .iter()
                .map(|pattern| pattern.text().to_owned())
                .collect();
            let matches = |file: &str| defer.tests.iter().any(|pattern| pattern.is_match(file));

            recommendations.push(self.recommend(DEFER, tests, matches, |fact| Some(fact.distance)));
        }

        recommendations
    }

    fn recommend(
        &self,
        option: &'static str,
        tests: Vec<String>,
        applies_to: impl Fn(&str) -> bool,
        value_of: impl Fn(Fact) -> Option<usize>,
    ) -> Recommendation {
        let concerned = |file: &str| applies_to(file) && !self.is_protected(file);
        let failure_values: Vec<usize> = self
            .runs
            .iter()
            .flat_map(|run| &run.failures)
            .filter(|failure| concerned(&failure.file))
            .filter_map(|failure| {
                value_of(Fact {
                    distance: failure.distance?,
                    hub_fanout: failure.hub_fanout,
                })
            })
            .collect();
        let failures = failure_values.len();
        let verdict = match failure_values.iter().max() {
            Some(&value) if failures >= MIN_FAILURES => {
                let mut impacted_test_files = 0;
                let mut impacted_seconds = 0.0;
                let mut dropped_test_files = 0;
                let mut dropped_seconds = 0.0;

                for impacted in self.runs.iter().flat_map(|run| &run.impacted) {
                    let seconds = impacted.seconds.unwrap_or_default();

                    impacted_test_files += 1;
                    impacted_seconds += seconds;

                    let fact = Fact {
                        distance: impacted.distance,
                        hub_fanout: impacted.hub_fanout,
                    };

                    if concerned(&impacted.file) && value_of(fact).is_some_and(|fact| fact > value)
                    {
                        dropped_test_files += 1;
                        dropped_seconds += seconds;
                    }
                }

                if dropped_test_files == 0 {
                    Verdict::NoSaving { value, failures }
                } else {
                    Verdict::Recommended {
                        value,
                        failures,
                        dropped_test_files,
                        dropped_seconds,
                        impacted_test_files,
                        impacted_seconds,
                    }
                }
            }
            _ => Verdict::NotEnoughFailures { failures },
        };

        Recommendation {
            option,
            tests,
            verdict,
        }
    }

    fn is_protected(&self, file: &str) -> bool {
        self.protected.iter().any(|pattern| pattern.is_match(file))
    }
}

#[derive(Clone, Copy)]
struct Fact {
    distance: usize,
    hub_fanout: Option<usize>,
}

#[derive(Serialize)]
struct AggregateJson<'a> {
    schema_version: u32,
    runs: usize,
    duplicate_runs: usize,
    modes: &'a BTreeMap<String, usize>,
    full_run_causes: &'a BTreeMap<String, usize>,
    test_files: TestFilesJson,
    seconds: SecondsJson,
    failures: FailuresJson<'a>,
    misses: Vec<MissJson<'a>>,
    recommendations: Vec<RecommendationJson<'a>>,
}

#[derive(Serialize)]
struct TestFilesJson {
    reported: usize,
    skipped: usize,
    skipped_ratio: f64,
}

#[derive(Serialize)]
struct SecondsJson {
    reported: f64,
    skipped: f64,
    skipped_ratio: f64,
}

#[derive(Serialize)]
struct FailuresJson<'a> {
    total: usize,
    missed: usize,
    flaky: usize,
    unattributed: usize,
    by_distance: &'a BTreeMap<usize, usize>,
    in_full_runs: usize,
}

#[derive(Serialize)]
struct MissJson<'a> {
    file: &'a str,
    head: Option<&'a str>,
    dropped: Option<&'a str>,
}

#[derive(Serialize)]
struct RecommendationJson<'a> {
    option: &'static str,

    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    tests: &'a [String],

    value: Option<usize>,
    failures: usize,
    failures_needed: usize,
    dropped_test_files: usize,
    dropped_seconds: f64,
    impacted_test_files: usize,
    impacted_seconds: f64,
}

impl<'a> RecommendationJson<'a> {
    fn new(recommendation: &'a Recommendation) -> Self {
        let empty = Self {
            option: recommendation.option,
            tests: &recommendation.tests,
            value: None,
            failures: 0,
            failures_needed: MIN_FAILURES,
            dropped_test_files: 0,
            dropped_seconds: 0.0,
            impacted_test_files: 0,
            impacted_seconds: 0.0,
        };

        match recommendation.verdict {
            Verdict::Recommended {
                value,
                failures,
                dropped_test_files,
                dropped_seconds,
                impacted_test_files,
                impacted_seconds,
            } => Self {
                value: Some(value),
                failures,
                dropped_test_files,
                dropped_seconds: rounded(dropped_seconds),
                impacted_test_files,
                impacted_seconds: rounded(impacted_seconds),
                ..empty
            },
            Verdict::NoSaving { failures, .. } | Verdict::NotEnoughFailures { failures } => {
                Self { failures, ..empty }
            }
        }
    }
}

fn quoted_list(texts: &[String]) -> String {
    let quoted: Vec<String> = texts.iter().map(|text| format!("{text:?}")).collect();

    quoted.join(", ")
}

fn toml_patterns(patterns: &[String]) -> String {
    match patterns {
        [pattern] => format!("{pattern:?}"),
        patterns => format!("[{}]", quoted_list(patterns)),
    }
}

fn rounded(seconds: f64) -> f64 {
    (seconds * 1_000.0).round() / 1_000.0
}

fn format_duration(seconds: f64) -> String {
    if seconds < 60.0 {
        format!("{seconds:.1} s")
    } else if seconds < 3_600.0 {
        format!("{:.1} min", seconds / 60.0)
    } else {
        format!("{:.1} h", seconds / 3_600.0)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::config::Config;

    fn shadow_json(head: &str, mode: &str, failures: Value, impacted: Value) -> String {
        json!({
            "schema_version": 1,
            "revisions": { "base": "main", "head": head, "merge_base": "0000000" },
            "selection_mode": mode,
            "mode_reasons": [],
            "test_files": { "reported": 10, "selected": 4, "selected_but_not_reported": 0, "skipped": 6 },
            "reported_seconds": 100.0,
            "savings": { "test_files_ratio": 0.6, "seconds": 60.0, "seconds_ratio": 0.6 },
            "widening": { "test_files": 0, "seconds": 0.0, "caught_failures": [] },
            "misses": [],
            "failures": failures,
            "flaky": [],
            "unattributed_failures": [],
            "impacted": impacted,
            "warnings": []
        })
        .to_string()
    }

    fn failure(file: &str, distance: usize, hub_fanout: usize) -> Value {
        json!({
            "file": file,
            "selected": true,
            "widened": false,
            "distance": distance,
            "hub_fanout": hub_fanout,
            "dropped": null
        })
    }

    fn impacted(file: &str, distance: usize, hub_fanout: usize, seconds: f64) -> Value {
        json!({
            "file": file,
            "distance": distance,
            "hub_fanout": hub_fanout,
            "seconds": seconds,
            "dropped": null
        })
    }

    fn runs(shadow_jsons: &[String]) -> ShadowRuns {
        let mut shadow_runs = ShadowRuns::default();

        for shadow_json in shadow_jsons {
            shadow_runs.add(shadow_json).unwrap();
        }

        shadow_runs
    }

    fn runs_with_failures(failure_count: usize) -> ShadowRuns {
        let shadow_jsons: Vec<String> = (0..failure_count)
            .map(|index| {
                shadow_json(
                    &format!("head{index}"),
                    "selected",
                    json!([failure("src/a.test.ts", 1 + index % 3, 10 + index % 5)]),
                    json!([
                        impacted("src/a.test.ts", 1 + index % 3, 10 + index % 5, 2.0),
                        impacted("test/far.integration.test.ts", 6, 80, 30.0),
                    ]),
                )
            })
            .collect();

        runs(&shadow_jsons)
    }

    fn aggregate_with(shadow_runs: &ShadowRuns, config_text: &str) -> Aggregate {
        shadow_runs.aggregate(&Config::parse(config_text).unwrap().aggressive)
    }

    #[test]
    fn runs_are_summed_with_modes_full_run_causes_misses_and_failure_distances() {
        let mut full_run: Value = serde_json::from_str(&shadow_json(
            "bbbbbbbbbb",
            "all",
            json!([{ "file": "src/c.test.ts", "selected": true, "widened": false, "distance": null }]),
            json!([]),
        ))
        .unwrap();

        full_run["mode_reasons"] = json!([
            { "kind": "always_all", "message": "ci/deploy.sh (modified) matches always_all \"ci/**\"" },
            { "kind": "always_all", "message": "ci/lint.sh (modified) matches always_all \"ci/**\"" }
        ]);

        let selected_run = shadow_json(
            "aaaaaaaaaa",
            "selected",
            json!([
                failure("src/a.test.ts", 1, 2),
                {
                    "file": "src/b.test.ts",
                    "selected": false,
                    "widened": false,
                    "distance": 4,
                    "hub_fanout": 9,
                    "dropped": "max_distance"
                }
            ]),
            json!([]),
        );
        let aggregate = aggregate_with(
            &runs(&[selected_run.clone(), full_run.to_string(), selected_run]),
            "",
        );

        assert_eq!(aggregate.runs, 2);
        assert_eq!(aggregate.duplicate_runs, 1);
        assert_eq!(
            aggregate.summary_line(),
            "prober shadow: 2 runs, 1 miss — 8 of 20 reported test files selected (40.0%), 2.0 min of 3.3 min skipped"
        );
        assert_eq!(
            aggregate.to_markdown(),
            "### Prober shadow: 2 runs

**1 miss** — 8 of 20 reported test files selected (40.0%), 2.0 min of 3.3 min skipped.

Runs by mode: 1 selected, 1 all.
Every test ran because of `always_all` in 1 run.
1 run with the same base and head as another read once.

Failed test files: 3, 1 missed.

| Distance | Failed test files |
| --- | --- |
| 1 | 1 |
| 4 | 1 |
| every test ran | 1 |

#### Misses

| Test file | Head | Why |
| --- | --- | --- |
| `src/b.test.ts` | aaaaaaa | dropped by `max_distance` |

#### Aggressive options

- `max_distance`: not enough failures yet, 2 of the 20 needed.
- `hub_fanout_limit`: not enough failures yet, 2 of the 20 needed.
"
        );
    }

    #[test]
    fn options_are_recommended_at_the_largest_failure_value_with_what_they_drop() {
        let aggregate = aggregate_with(&runs_with_failures(MIN_FAILURES), "");

        assert_eq!(
            aggregate.recommendations[0].describe(),
            "`max_distance`: `max_distance = 3` keeps every one of the 20 failures it rests on, and would drop 20 of 40 impacted test files (10.0 min of 10.7 min)"
        );
        assert_eq!(
            aggregate.recommendations[1].describe(),
            "`hub_fanout_limit`: `hub_fanout_limit = 14` keeps every one of the 20 failures it rests on, and would drop 20 of 40 impacted test files (10.0 min of 10.7 min)"
        );
        assert!(
            aggregate.to_markdown().ends_with(
                "\n```toml\n[aggressive]\nmax_distance = 3\nhub_fanout_limit = 14\n```\n"
            ),
            "{}",
            aggregate.to_markdown()
        );
    }

    #[test]
    fn too_few_failures_recommend_nothing() {
        let aggregate = aggregate_with(&runs_with_failures(MIN_FAILURES - 1), "");

        assert_eq!(
            aggregate.recommendations[0].verdict,
            Verdict::NotEnoughFailures {
                failures: MIN_FAILURES - 1
            }
        );
        assert!(!aggregate.to_markdown().contains("```toml"));
    }

    #[test]
    fn protected_tests_neither_bound_nor_count_and_each_defer_gets_its_own_distance() {
        let aggregate = aggregate_with(
            &runs_with_failures(MIN_FAILURES),
            "[protected]\ntests = [\"test/**\"]\n\n[[defer]]\ntests = [\"src/**/*.test.ts\", \"e2e/**\"]\nmax_distance = 1\n",
        );

        assert_eq!(
            aggregate.recommendations[0].verdict,
            Verdict::NoSaving {
                value: 3,
                failures: MIN_FAILURES
            }
        );
        assert_eq!(
            aggregate.recommendations[2].describe(),
            "`[[defer]] \"src/**/*.test.ts\", \"e2e/**\"`: `max_distance = 3` keeps every one of the 20 failures, but would drop no test file: keep it off"
        );
    }

    #[test]
    fn recommendations_read_as_json() {
        let aggregate = aggregate_with(&runs_with_failures(MIN_FAILURES), "");
        let json_aggregate: Value = serde_json::from_str(&aggregate.to_json()).unwrap();

        assert_eq!(json_aggregate["runs"], 20);
        assert_eq!(
            json_aggregate["failures"]["by_distance"],
            json!({ "1": 7, "2": 7, "3": 6 })
        );
        assert_eq!(
            json_aggregate["recommendations"][0],
            json!({
                "option": "max_distance",
                "value": 3,
                "failures": 20,
                "failures_needed": 20,
                "dropped_test_files": 20,
                "dropped_seconds": 600.0,
                "impacted_test_files": 40,
                "impacted_seconds": 640.0
            })
        );
    }

    #[test]
    fn shadow_json_of_another_schema_version_or_malformed_is_rejected() {
        let mut shadow_runs = ShadowRuns::default();
        let other_version = shadow_json("a", "none", json!([]), json!([]))
            .replace("\"schema_version\":1", "\"schema_version\":2");

        assert_eq!(
            shadow_runs.add(&other_version).unwrap_err().to_string(),
            "unsupported shadow schema_version 2: this prober reads version 1"
        );
        assert!(
            shadow_runs
                .add("{\"schema_version\": 1}")
                .unwrap_err()
                .to_string()
                .starts_with("invalid shadow JSON: missing field `selection_mode`")
        );
    }
}
