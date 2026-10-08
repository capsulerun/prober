//! JUnit reports produced by real test runners (vitest 5.0.3, jest-junit 17.0.0) read into outcomes per test file.

use std::fs;
use std::path::Path;
use std::time::Duration;

use prober_core::files::RepoPath;
use prober_core::junit::{ReportPaths, read_junit_report};
use prober_core::outcomes::{TestOutcomes, TestStatus};

const REPO_ROOT: &str = "/home/runner/work/app/app";

fn fixture(name: &str) -> String {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/junit")
        .join(name);

    fs::read_to_string(fixture_path).unwrap()
}

fn read_reports(reports: &[&str], runner_directory: Option<&str>) -> TestOutcomes {
    let runner_directory = runner_directory.map(|directory| RepoPath::new(directory).unwrap());
    let report_paths = ReportPaths::new(Path::new(REPO_ROOT), runner_directory);
    let mut outcomes = TestOutcomes::default();

    for report in reports {
        read_junit_report(report, &report_paths, &mut outcomes).unwrap();
    }

    outcomes
}

fn statuses(outcomes: &TestOutcomes) -> Vec<(&str, TestStatus)> {
    outcomes
        .files()
        .map(|outcome| (outcome.file.as_str(), outcome.status))
        .collect()
}

#[test]
fn vitest_report_gives_one_outcome_per_test_file_without_retry_information() {
    let outcomes = read_reports(&[&fixture("vitest-5.0.3.xml")], None);
    let math = RepoPath::new("src/math.test.ts").unwrap();

    assert_eq!(
        statuses(&outcomes),
        [
            ("src/broken.test.ts", TestStatus::Failed),
            ("src/flaky.test.ts", TestStatus::Passed),
            ("src/math.test.ts", TestStatus::Failed),
            ("src/nested/strings.test.ts", TestStatus::Passed),
            ("src/skipped.test.ts", TestStatus::Skipped),
        ]
    );
    assert_eq!(
        outcomes.outcome(&math).unwrap().duration,
        Duration::from_secs_f64(0.005014584)
    );
    assert_eq!(outcomes.unattributed_test_cases(), []);
}

#[test]
fn vitest_report_from_a_package_directory_is_prefixed_with_it() {
    let outcomes = read_reports(&[&fixture("vitest-5.0.3.xml")], Some("packages/web"));
    let files: Vec<&str> = statuses(&outcomes)
        .into_iter()
        .map(|(file, _)| file)
        .collect();

    assert_eq!(
        files,
        [
            "packages/web/src/broken.test.ts",
            "packages/web/src/flaky.test.ts",
            "packages/web/src/math.test.ts",
            "packages/web/src/nested/strings.test.ts",
            "packages/web/src/skipped.test.ts",
        ]
    );
}

#[test]
fn jest_junit_default_report_has_no_test_file_paths() {
    let outcomes = read_reports(&[&fixture("jest-junit-17.0.0-default.xml")], None);
    let failed_test_cases = outcomes
        .unattributed_test_cases()
        .iter()
        .filter(|test_case| test_case.status == TestStatus::Failed)
        .count();

    assert_eq!(statuses(&outcomes), []);
    assert_eq!(outcomes.unattributed_test_cases().len(), 6);
    assert_eq!(failed_test_cases, 1);
}

#[test]
fn jest_junit_with_file_attribute_and_suite_errors_reports_every_test_file() {
    let outcomes = read_reports(&[&fixture("jest-junit-17.0.0-recommended.xml")], None);

    assert_eq!(
        statuses(&outcomes),
        [
            ("src/broken.test.js", TestStatus::Failed),
            ("src/flaky.test.js", TestStatus::Passed),
            ("src/math.test.js", TestStatus::Failed),
            ("src/nested/strings.test.js", TestStatus::Passed),
            ("src/skipped.test.js", TestStatus::Skipped),
        ]
    );
}

#[test]
fn sharded_reports_merge_and_a_passing_shard_never_hides_a_failure() {
    let second_shard = r#"
        <testsuites>
          <testsuite name="src/math.test.ts" time="0.01">
            <testcase classname="src/math.test.ts" name="math &gt; subtracts" time="0.01"/>
          </testsuite>
          <testsuite name="src/other.test.ts" time="0.02">
            <testcase classname="src/other.test.ts" name="other" time="0.02"/>
          </testsuite>
        </testsuites>
    "#;
    let outcomes = read_reports(&[&fixture("vitest-5.0.3.xml"), second_shard], None);
    let math = RepoPath::new("src/math.test.ts").unwrap();
    let other = RepoPath::new("src/other.test.ts").unwrap();

    assert_eq!(outcomes.outcome(&math).unwrap().status, TestStatus::Failed);
    assert_eq!(
        outcomes.outcome(&math).unwrap().duration,
        Duration::from_secs_f64(0.005014584) + Duration::from_millis(10)
    );
    assert_eq!(outcomes.outcome(&other).unwrap().status, TestStatus::Passed);
}
