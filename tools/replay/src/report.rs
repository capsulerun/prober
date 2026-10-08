use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;
use prober_core::files::RepoPath;
use prober_core::outcomes::{TestOutcomes, TestStatus};
use prober_core::shadow::{RecordedSelection, ShadowResult};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::record::ReplayRecord;

const SHORT_COMMIT_LENGTH: usize = 9;

const RECURRING_FAILURE_CHANGES: usize = 3;

const LISTED_WIDENING_SITES: usize = 10;

#[derive(Debug, Args)]
pub struct ReportArguments {
    #[arg(
        long,
        help = "Name of the benchmarked repository, used in the report title"
    )]
    title: String,

    #[arg(
        long,
        help = "Records of `selections --last N`: selections on merged changes, for savings"
    )]
    merges: Option<PathBuf>,

    #[arg(
        long,
        requires = "failures",
        help = "Records of `selections --changes changes.jsonl`: selections on failed pull request heads"
    )]
    failed_changes: Option<PathBuf>,

    #[arg(
        long,
        requires = "failed_changes",
        help = "failures.jsonl written by `failures`"
    )]
    failures: Option<PathBuf>,

    #[arg(
        long,
        requires = "merges",
        help = "Records of the same merged changes replayed with inference off, compared in the report"
    )]
    merges_without_inference: Option<PathBuf>,

    #[arg(
        long,
        requires = "failed_changes",
        help = "Records of the same failed changes replayed with inference off, compared in the report"
    )]
    failed_changes_without_inference: Option<PathBuf>,

    #[arg(
        long,
        help = "Markdown file the report is written to [default: stdout]"
    )]
    output: Option<PathBuf>,
}

#[derive(Deserialize)]
struct RecordedFailure {
    change: String,
    file: String,
    pre_existing: bool,

    #[serde(default = "in_repository_when_unknown")]
    in_repository: bool,
}

struct Miss<'a> {
    change: &'a str,
    file: RepoPath,
    mode: &'a str,
    selected_test_files: u64,
    failed_on_changes: usize,
}

#[derive(Default)]
struct MissAnalysis<'a> {
    changes: usize,
    unreplayed_changes: Vec<&'a ReplayRecord>,
    counted_failures: usize,
    pre_existing_failures: usize,
    failures_not_in_repository: usize,
    failures_on_unreplayed_changes: usize,
    selected_failures_by_distance: BTreeMap<usize, usize>,
    selected_failures_without_distance: usize,
    misses: Vec<Miss<'a>>,
    caught_only_by_widening: usize,
    selected_ratios: Vec<f64>,
    selected_ratios_without_widening: Vec<f64>,
    failed_change_count_by_file: HashMap<&'a str, usize>,
}

struct SelectionFigures {
    ratios: Vec<f64>,
    ratios_without_widening: Vec<f64>,
}

impl SelectionFigures {
    fn of(records: &[&ReplayRecord]) -> Self {
        Self {
            ratios: sorted(records.iter().filter_map(|record| record.selected_ratio())),
            ratios_without_widening: sorted(
                records
                    .iter()
                    .filter_map(|record| record.selected_ratio_without_widening()),
            ),
        }
    }
}

struct ConfigurationRow {
    label: &'static str,
    ratios: Option<Vec<f64>>,
    misses: Option<usize>,
}

pub fn run(arguments: &ReportArguments) -> Result<String> {
    if arguments.merges.is_none() && arguments.failed_changes.is_none() {
        bail!("--merges or --failed-changes is required");
    }

    let mut markdown = format!("# Prober benchmark: {}\n", arguments.title);
    let mut summary_parts = Vec::new();
    let merge_records = read_optional_json_lines::<ReplayRecord>(arguments.merges.as_deref())?;
    let merge_records_without_inference =
        read_optional_json_lines::<ReplayRecord>(arguments.merges_without_inference.as_deref())?;
    let failed_change_records =
        read_optional_json_lines::<ReplayRecord>(arguments.failed_changes.as_deref())?;
    let failed_change_records_without_inference = read_optional_json_lines::<ReplayRecord>(
        arguments.failed_changes_without_inference.as_deref(),
    )?;
    let failures = read_optional_json_lines::<RecordedFailure>(arguments.failures.as_deref())?;
    let miss_analysis = failed_change_records
        .as_deref()
        .zip(failures.as_deref())
        .map(|(records, failures)| MissAnalysis::analyze(records, failures))
        .transpose()?;
    let miss_analysis_without_inference = failed_change_records_without_inference
        .as_deref()
        .zip(failures.as_deref())
        .map(|(records, failures)| MissAnalysis::analyze(records, failures))
        .transpose()?;

    if let Some(merge_records) = &merge_records {
        markdown.push_str(&savings_section(merge_records));
        summary_parts.push(format!("{} merged changes", merge_records.len()));
    }

    if let Some(miss_analysis) = &miss_analysis {
        markdown.push_str(&miss_analysis.section());
        summary_parts.push(format!(
            "{} failed changes with {} misses",
            miss_analysis.changes,
            miss_analysis.misses.len()
        ));
    }

    markdown.push_str(&configurations_section(
        merge_records.as_deref(),
        merge_records_without_inference.as_deref(),
        miss_analysis.as_ref(),
        miss_analysis_without_inference.as_ref(),
    ));

    match &arguments.output {
        Some(output_path) => fs::write(output_path, &markdown)
            .with_context(|| format!("cannot write {}", output_path.display()))?,
        None => print!("{markdown}"),
    }

    Ok(format!(
        "prober-replay: report for {}",
        summary_parts.join(" and ")
    ))
}

fn savings_section(records: &[ReplayRecord]) -> String {
    let replayed_records: Vec<&ReplayRecord> = records
        .iter()
        .filter(|record| record.selection.is_some())
        .collect();
    let count_of_mode = |mode: &str| {
        replayed_records
            .iter()
            .filter(|record| record.mode() == Some(mode))
            .count()
    };
    let selection_figures = SelectionFigures::of(&replayed_records);
    let replay_seconds = sorted(replayed_records.iter().filter_map(|record| record.seconds));
    let mut section = format!(
        "\n## Savings on {} merged changes\n\n| | |\n| --- | --- |\n| Replayed | {} ({} failed to replay) |\n| Mode | {} selected, {} all, {} none |\n| Test files selected | {} |\n| Without widening | {} |\n| Widening | {} |\n| Time per change | median {}, 90th percentile {} |\n",
        records.len(),
        replayed_records.len(),
        records.len() - replayed_records.len(),
        count_of_mode("selected"),
        count_of_mode("all"),
        count_of_mode("none"),
        format_distribution(&selection_figures.ratios),
        format_distribution(&selection_figures.ratios_without_widening),
        widening_summary(&replayed_records),
        format_seconds(percentile(&replay_seconds, 50.0)),
        format_seconds(percentile(&replay_seconds, 90.0)),
    );
    let widening_sites = widening_sites_table(&replayed_records);

    section.push_str(&widening_sites);
    let full_run_causes = full_run_causes(&replayed_records);

    if !full_run_causes.is_empty() {
        section.push_str("\n### Why every test ran\n\n| Cause | Changes |\n| --- | --- |\n");

        for (cause, change_count) in &full_run_causes {
            section.push_str(&format!("| {cause} | {change_count} |\n"));
        }
    }

    section
}

fn widening_summary(records: &[&ReplayRecord]) -> String {
    let widened_records: Vec<&&ReplayRecord> = records
        .iter()
        .filter(|record| record.tests_added_by_widening() > 0)
        .collect();

    if widened_records.is_empty() {
        return format!("no change widened, of {}", records.len());
    }

    let added_test_files = sorted(
        widened_records
            .iter()
            .map(|record| record.tests_added_by_widening() as f64),
    );
    let shares_of_selection = sorted(widened_records.iter().map(|record| {
        record.tests_added_by_widening() as f64 / record.tests_selected().max(1) as f64
    }));

    format!(
        "{} of {} changes widened, adding median {} test files (mean {:.0}), {} of their selection",
        widened_records.len(),
        records.len(),
        percentile(&added_test_files, 50.0).unwrap_or_default(),
        mean(&added_test_files).unwrap_or_default(),
        format_ratio(percentile(&shares_of_selection, 50.0)),
    )
}

fn widening_sites_table(records: &[&ReplayRecord]) -> String {
    let mut added_by_site: BTreeMap<String, (String, Vec<f64>)> = BTreeMap::new();

    for record in records {
        for widened_site in record.widened_sites() {
            let (_, added) = added_by_site
                .entry(widened_site.site)
                .or_insert_with(|| (widened_site.cause, Vec::new()));

            added.push(widened_site.added as f64);
        }
    }

    if added_by_site.is_empty() {
        return String::new();
    }

    let mut sites: Vec<(String, String, Vec<f64>)> = added_by_site
        .into_iter()
        .map(|(site, (cause, added))| (site, cause, sorted(added.into_iter())))
        .collect();

    sites.sort_by(|left, right| {
        let total_added = |added: &[f64]| added.iter().sum::<f64>();

        total_added(&right.2).total_cmp(&total_added(&left.2))
    });

    let mut table = String::from(
        "\n### Sites that add the most tests by widening\n\n| Site | Cause | Changes widened | Test files added (median, total) |\n| --- | --- | --- | --- |\n",
    );

    for (site, cause, added) in sites.iter().take(LISTED_WIDENING_SITES) {
        table.push_str(&format!(
            "| `{site}` | {cause} | {} | {}, {} |\n",
            added.len(),
            percentile(added, 50.0).unwrap_or_default(),
            added.iter().sum::<f64>()
        ));
    }

    if sites.len() > LISTED_WIDENING_SITES {
        table.push_str(&format!(
            "\n{} more sites widened at least one change.\n",
            sites.len() - LISTED_WIDENING_SITES
        ));
    }

    table
}

fn configurations_section(
    merge_records: Option<&[ReplayRecord]>,
    merge_records_without_inference: Option<&[ReplayRecord]>,
    miss_analysis: Option<&MissAnalysis>,
    miss_analysis_without_inference: Option<&MissAnalysis>,
) -> String {
    let figures_of = |records: Option<&[ReplayRecord]>| {
        records.map(|records| {
            let replayed_records: Vec<&ReplayRecord> = records
                .iter()
                .filter(|record| record.selection.is_some())
                .collect();

            SelectionFigures::of(&replayed_records)
        })
    };
    let figures = figures_of(merge_records);
    let figures_without_inference = figures_of(merge_records_without_inference);
    let has_inference_off =
        figures_without_inference.is_some() || miss_analysis_without_inference.is_some();
    let mut rows = Vec::new();

    if has_inference_off {
        rows.push(ConfigurationRow {
            label: "Inference off, widening off (phase 1)",
            ratios: figures_without_inference
                .as_ref()
                .map(|figures| figures.ratios_without_widening.clone()),
            misses: miss_analysis_without_inference.map(MissAnalysis::misses_without_widening),
        });
        rows.push(ConfigurationRow {
            label: "Inference off, widening on",
            ratios: figures_without_inference
                .as_ref()
                .map(|figures| figures.ratios.clone()),
            misses: miss_analysis_without_inference.map(|analysis| analysis.misses.len()),
        });
    }

    rows.push(ConfigurationRow {
        label: "Inference on, widening off",
        ratios: figures
            .as_ref()
            .map(|figures| figures.ratios_without_widening.clone()),
        misses: miss_analysis.map(MissAnalysis::misses_without_widening),
    });
    rows.push(ConfigurationRow {
        label: "**Inference on, widening on (default)**",
        ratios: figures.as_ref().map(|figures| figures.ratios.clone()),
        misses: miss_analysis.map(|analysis| analysis.misses.len()),
    });

    let mut section = String::from(
        "\n## Inference and widening\n\n| Configuration | Test files selected on merged changes | Misses on failed changes |\n| --- | --- | --- |\n",
    );

    for row in rows {
        section.push_str(&format!(
            "| {} | {} | {} |\n",
            row.label,
            row.ratios
                .as_deref()
                .map_or_else(|| "n/a".to_owned(), format_median_and_mean),
            row.misses
                .map_or_else(|| "n/a".to_owned(), |misses| misses.to_string()),
        ));
    }

    section
}

fn full_run_causes(records: &[&ReplayRecord]) -> BTreeMap<String, usize> {
    let mut change_count_by_cause = BTreeMap::new();

    for record in records.iter().filter(|record| record.mode() == Some("all")) {
        let causes: BTreeSet<String> = mode_reasons(record).iter().map(cause_of).collect();

        for cause in causes {
            *change_count_by_cause.entry(cause).or_default() += 1;
        }
    }

    change_count_by_cause
}

fn mode_reasons(record: &ReplayRecord) -> Vec<Value> {
    record
        .selection
        .as_ref()
        .and_then(|selection| selection.get("mode_reasons"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn cause_of(mode_reason: &Value) -> String {
    let kind = mode_reason
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let detail = ["pattern", "field", "package", "file"]
        .iter()
        .find_map(|key| mode_reason.get(*key)?.as_str());

    match detail {
        Some(detail) => format!("`{kind}` `{detail}`"),
        None => format!("`{kind}`"),
    }
}

impl<'a> MissAnalysis<'a> {
    fn analyze(records: &'a [ReplayRecord], failures: &'a [RecordedFailure]) -> Result<Self> {
        let records_by_change: HashMap<&str, &ReplayRecord> = records
            .iter()
            .map(|record| (record.id.as_str(), record))
            .collect();
        let mut failures_by_change: BTreeMap<&str, Vec<&RecordedFailure>> = BTreeMap::new();
        let mut miss_analysis = MissAnalysis::default();

        for failure in failures {
            failures_by_change
                .entry(failure.change.as_str())
                .or_default()
                .push(failure);
        }

        miss_analysis.changes = failures_by_change.len();

        for change_failures in failures_by_change.values() {
            let failed_files: BTreeSet<&str> = change_failures
                .iter()
                .map(|failure| failure.file.as_str())
                .collect();

            for failed_file in failed_files {
                *miss_analysis
                    .failed_change_count_by_file
                    .entry(failed_file)
                    .or_default() += 1;
            }
        }

        for (change, change_failures) in failures_by_change {
            let counted_failures: Vec<&RecordedFailure> = change_failures
                .iter()
                .copied()
                .filter(|failure| !failure.pre_existing)
                .collect();
            let record = records_by_change.get(change).copied();

            miss_analysis.pre_existing_failures += change_failures
                .iter()
                .filter(|failure| failure.pre_existing)
                .count();

            let Some((record, selection_value)) =
                record.and_then(|record| Some((record, record.selection.as_ref()?)))
            else {
                miss_analysis.failures_on_unreplayed_changes += counted_failures.len();
                miss_analysis.unreplayed_changes.extend(record);

                continue;
            };

            miss_analysis
                .selected_ratios
                .extend(record.selected_ratio());
            miss_analysis
                .selected_ratios_without_widening
                .extend(record.selected_ratio_without_widening());
            miss_analysis.compare(change, record, selection_value, &counted_failures)?;
        }

        miss_analysis.selected_ratios = sorted(miss_analysis.selected_ratios.iter().copied());
        miss_analysis.selected_ratios_without_widening = sorted(
            miss_analysis
                .selected_ratios_without_widening
                .iter()
                .copied(),
        );

        Ok(miss_analysis)
    }

    fn misses_without_widening(&self) -> usize {
        self.misses.len() + self.caught_only_by_widening
    }

    fn compare(
        &mut self,
        change: &'a str,
        record: &'a ReplayRecord,
        selection_value: &Value,
        counted_failures: &[&RecordedFailure],
    ) -> Result<()> {
        let selection = RecordedSelection::parse(&selection_value.to_string())
            .with_context(|| format!("selection of change {change}"))?;
        let mut outcomes = TestOutcomes::default();

        for failure in counted_failures {
            outcomes.record(
                RepoPath::new(failure.file.clone())?,
                TestStatus::Failed,
                Duration::ZERO,
            );
        }

        let shadow_result = ShadowResult::compare(&selection, &outcomes);
        let files_in_repository: HashMap<&str, bool> = counted_failures
            .iter()
            .map(|failure| (failure.file.as_str(), failure.in_repository))
            .collect();

        for failed_test_file in shadow_result.failed_test_files {
            let in_repository = files_in_repository
                .get(failed_test_file.file.as_str())
                .copied()
                .unwrap_or(true);

            if !failed_test_file.selected && !in_repository {
                self.failures_not_in_repository += 1;

                continue;
            }

            self.counted_failures += 1;

            if failed_test_file.selected && failed_test_file.widened {
                self.caught_only_by_widening += 1;
            }

            match (failed_test_file.selected, failed_test_file.distance) {
                (true, Some(distance)) => {
                    *self
                        .selected_failures_by_distance
                        .entry(distance)
                        .or_default() += 1;
                }
                (true, None) => self.selected_failures_without_distance += 1,
                (false, _) => self.misses.push(Miss {
                    change,
                    failed_on_changes: self
                        .failed_change_count_by_file
                        .get(failed_test_file.file.as_str())
                        .copied()
                        .unwrap_or(1),
                    file: failed_test_file.file,
                    mode: record.mode().unwrap_or("unknown"),
                    selected_test_files: selection_value
                        .pointer("/stats/tests_selected")
                        .and_then(Value::as_u64)
                        .unwrap_or_default(),
                }),
            }
        }

        Ok(())
    }

    fn section(&self) -> String {
        let replayed_changes = self.changes - self.unreplayed_changes.len();
        let selected_failures = self.counted_failures - self.misses.len();
        let mut section = format!(
            "\n## Misses on {} failed pull request changes\n\n| | |\n| --- | --- |\n| Replayed | {replayed_changes} ({} could not be replayed) |\n| Failed test files | {} counted, {} pre-existing on the base branch (not counted), {} not found in the repository (not counted), {} on changes that could not be replayed |\n| Selected | {selected_failures} ({} only through widening) |\n| **Misses** | **{}** ({} on tests that failed on {RECURRING_FAILURE_CHANGES} or more changes, likely flaky); {} without widening |\n| Test files selected | {}; without widening {} |\n",
            self.changes,
            self.unreplayed_changes.len(),
            self.counted_failures,
            self.pre_existing_failures,
            self.failures_not_in_repository,
            self.failures_on_unreplayed_changes,
            self.caught_only_by_widening,
            self.misses.len(),
            self.misses
                .iter()
                .filter(|miss| miss.failed_on_changes >= RECURRING_FAILURE_CHANGES)
                .count(),
            self.misses_without_widening(),
            format_median_and_mean(&self.selected_ratios),
            format_median_and_mean(&self.selected_ratios_without_widening),
        );

        if selected_failures > 0 {
            section.push_str(
                "\n### Distance of selected failures\n\n| Distance | Failed test files |\n| --- | --- |\n",
            );

            for (distance, failure_count) in &self.selected_failures_by_distance {
                section.push_str(&format!("| {distance} | {failure_count} |\n"));
            }

            if self.selected_failures_without_distance > 0 {
                section.push_str(&format!(
                    "| every test ran | {} |\n",
                    self.selected_failures_without_distance
                ));
            }
        }

        if !self.misses.is_empty() {
            section.push_str(
                "\n### Misses\n\n| Change | Test file | Selection | Same test failed on |\n| --- | --- | --- | --- |\n",
            );

            for miss in &self.misses {
                section.push_str(&format!(
                    "| `{}` | `{}` | {}, {} | {} |\n",
                    short_commit(miss.change),
                    miss.file,
                    miss.mode,
                    test_file_count(miss.selected_test_files),
                    change_count(miss.failed_on_changes)
                ));
            }
        }

        if !self.unreplayed_changes.is_empty() {
            section.push_str(
                "\n### Changes that could not be replayed\n\n| Change | Error |\n| --- | --- |\n",
            );

            for record in &self.unreplayed_changes {
                let error = record
                    .error
                    .clone()
                    .unwrap_or_else(|| record.stderr.trim().to_owned());

                section.push_str(&format!(
                    "| `{}` | {} |\n",
                    short_commit(&record.id),
                    error.replace('|', "\\|")
                ));
            }
        }

        section
    }
}

fn in_repository_when_unknown() -> bool {
    true
}

fn read_optional_json_lines<T: DeserializeOwned>(path: Option<&Path>) -> Result<Option<Vec<T>>> {
    path.map(read_json_lines).transpose()
}

fn read_json_lines<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let text =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;

    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(line_index, line)| {
            serde_json::from_str(line)
                .with_context(|| format!("{} line {}", path.display(), line_index + 1))
        })
        .collect()
}

fn sorted(values: impl Iterator<Item = f64>) -> Vec<f64> {
    let mut sorted_values: Vec<f64> = values.collect();

    sorted_values.sort_by(f64::total_cmp);

    sorted_values
}

fn percentile(sorted_values: &[f64], percentile: f64) -> Option<f64> {
    if sorted_values.is_empty() {
        return None;
    }

    let nearest_rank = (percentile / 100.0 * sorted_values.len() as f64).ceil() as usize;

    Some(sorted_values[nearest_rank.clamp(1, sorted_values.len()) - 1])
}

fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }

    Some(values.iter().sum::<f64>() / values.len() as f64)
}

fn format_ratio(ratio: Option<f64>) -> String {
    ratio.map_or_else(
        || "n/a".to_owned(),
        |ratio| format!("{:.1}%", ratio * 100.0),
    )
}

fn format_distribution(sorted_ratios: &[f64]) -> String {
    format!(
        "median {}, mean {}, 90th percentile {}",
        format_ratio(percentile(sorted_ratios, 50.0)),
        format_ratio(mean(sorted_ratios)),
        format_ratio(percentile(sorted_ratios, 90.0)),
    )
}

fn format_median_and_mean(sorted_ratios: &[f64]) -> String {
    format!(
        "median {}, mean {}",
        format_ratio(percentile(sorted_ratios, 50.0)),
        format_ratio(mean(sorted_ratios)),
    )
}

fn format_seconds(seconds: Option<f64>) -> String {
    seconds.map_or_else(|| "n/a".to_owned(), |seconds| format!("{seconds:.1} s"))
}

fn test_file_count(count: u64) -> String {
    if count == 1 {
        return "1 test file".to_owned();
    }

    format!("{count} test files")
}

fn change_count(count: usize) -> String {
    if count == 1 {
        return "1 change".to_owned();
    }

    format!("{count} changes")
}

fn short_commit(change: &str) -> &str {
    &change[..change.len().min(SHORT_COMMIT_LENGTH)]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn full_run_cause_names_the_pattern_field_package_or_file_at_fault() {
        let causes = [
            (
                json!({ "kind": "always_all", "file": "ci/a.sh", "pattern": "ci/**" }),
                "`always_all` `ci/**`",
            ),
            (
                json!({ "kind": "global_field", "file": "tsconfig.json", "field": "compilerOptions.target" }),
                "`global_field` `compilerOptions.target`",
            ),
            (
                json!({ "kind": "root_scope", "file": "pnpm-lock.yaml", "package": "vitest" }),
                "`root_scope` `vitest`",
            ),
            (
                json!({ "kind": "unparsable_manifest", "file": "yarn.lock" }),
                "`unparsable_manifest` `yarn.lock`",
            ),
            (json!({ "kind": "error", "message": "boom" }), "`error`"),
        ];

        for (mode_reason, expected_cause) in causes {
            assert_eq!(cause_of(&mode_reason), expected_cause);
        }
    }
}
