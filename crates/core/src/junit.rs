use std::fmt::Display;
use std::iter;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow};
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

use crate::files::RepoPath;
use crate::outcomes::{TestOutcomes, TestStatus};

const TEST_SOURCE_EXTENSIONS: [&str; 10] = [
    "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "py", "rs",
];

pub struct ReportPaths {
    repo_root: PathBuf,
    runner_directory: Option<RepoPath>,
}

impl ReportPaths {
    pub fn new(repo_root: &Path, runner_directory: Option<RepoPath>) -> Self {
        Self {
            repo_root: repo_root.to_path_buf(),
            runner_directory,
        }
    }

    pub fn resolve(&self, reported_path: &str) -> Option<RepoPath> {
        let reported_path = reported_path.trim().replace('\\', "/");
        let path_from_repo_root = match Path::new(&reported_path).strip_prefix(&self.repo_root) {
            Ok(path_inside_repo) => path_inside_repo.to_str()?.replace('\\', "/"),
            Err(_) if reported_path.starts_with('/') => return None,
            Err(_) => match &self.runner_directory {
                Some(runner_directory) => format!("{runner_directory}/{reported_path}"),
                None => reported_path,
            },
        };

        normalize(&path_from_repo_root)
    }
}

pub fn read_junit_report(
    xml: &str,
    report_paths: &ReportPaths,
    outcomes: &mut TestOutcomes,
) -> Result<()> {
    let mut reader = Reader::from_str(xml);
    let mut junit_reader = JunitReader {
        report_paths,
        outcomes,
        open_suites: Vec::new(),
        open_test_case: None,
    };

    loop {
        let event = reader
            .read_event()
            .map_err(|error| invalid_report(reader.error_position(), error))?;
        let element_result = match &event {
            Event::Start(element) => junit_reader.open(element),
            Event::Empty(element) => {
                let opened = junit_reader.open(element);

                junit_reader.close(element.local_name().as_ref());

                opened
            }
            Event::End(element) => {
                junit_reader.close(element.local_name().as_ref());

                Ok(())
            }
            Event::Eof => break,
            _ => Ok(()),
        };

        element_result.map_err(|error| invalid_report(reader.buffer_position(), error))?;
    }

    Ok(())
}

#[derive(Default)]
struct OpenSuite {
    name: Option<String>,
    file: Option<String>,
    time: Option<Duration>,
    test_cases: Vec<OpenTestCase>,
    failed_outside_test_cases: bool,
}

#[derive(Default)]
struct OpenTestCase {
    name: String,
    class_name: Option<String>,
    file: Option<String>,
    time: Duration,
    failed: bool,
    flaky: bool,
    skipped: bool,
}

impl OpenTestCase {
    fn status(&self) -> TestStatus {
        if self.failed {
            return TestStatus::Failed;
        }

        if self.flaky {
            return TestStatus::Flaky;
        }

        if self.skipped {
            return TestStatus::Skipped;
        }

        TestStatus::Passed
    }
}

struct JunitReader<'a> {
    report_paths: &'a ReportPaths,
    outcomes: &'a mut TestOutcomes,
    open_suites: Vec<OpenSuite>,
    open_test_case: Option<OpenTestCase>,
}

impl JunitReader<'_> {
    fn open(&mut self, element: &BytesStart) -> Result<()> {
        match element.local_name().as_ref() {
            "testsuite" => self.open_suites.push(OpenSuite {
                name: attribute(element, "name")?,
                file: attribute(element, "file")?.or(attribute(element, "filepath")?),
                time: seconds(attribute(element, "time")?),
                ..OpenSuite::default()
            }),
            "testcase" => {
                self.open_test_case = Some(OpenTestCase {
                    name: attribute(element, "name")?.unwrap_or_default(),
                    class_name: attribute(element, "classname")?,
                    file: attribute(element, "file")?,
                    time: seconds(attribute(element, "time")?).unwrap_or_default(),
                    ..OpenTestCase::default()
                })
            }
            "failure" | "error" | "rerunFailure" | "rerunError" => self.mark_failed(),
            "flakyFailure" | "flakyError" => {
                if let Some(test_case) = &mut self.open_test_case {
                    test_case.flaky = true;
                }
            }
            "skipped" => {
                if let Some(test_case) = &mut self.open_test_case {
                    test_case.skipped = true;
                }
            }
            _ => {}
        }

        Ok(())
    }

    fn close(&mut self, element_name: &str) {
        match element_name {
            "testcase" => {
                let Some(test_case) = self.open_test_case.take() else {
                    return;
                };

                match self.open_suites.last_mut() {
                    Some(suite) => suite.test_cases.push(test_case),
                    None => {
                        let file = test_case_file(&test_case, &[], self.report_paths);

                        record_test_case(self.outcomes, &test_case, file);
                    }
                }
            }
            "testsuite" => {
                if let Some(suite) = self.open_suites.pop() {
                    self.record_suite(suite);
                }
            }
            _ => {}
        }
    }

    fn mark_failed(&mut self) {
        match (&mut self.open_test_case, self.open_suites.last_mut()) {
            (Some(test_case), _) => test_case.failed = true,
            (None, Some(suite)) => suite.failed_outside_test_cases = true,
            (None, None) => {}
        }
    }

    fn record_suite(&mut self, suite: OpenSuite) {
        let outer_suites: Vec<&OpenSuite> = self.open_suites.iter().rev().collect();
        let suites_inner_first: Vec<&OpenSuite> = iter::once(&suite).chain(outer_suites).collect();
        let suite_file = suite_file(&suites_inner_first, self.report_paths);

        if suite.failed_outside_test_cases {
            match &suite_file {
                Some(file) => {
                    self.outcomes
                        .record(file.clone(), TestStatus::Failed, Duration::ZERO)
                }
                None => self.outcomes.record_unattributed(
                    suite.name.clone().unwrap_or_default(),
                    TestStatus::Failed,
                ),
            }
        }

        let attributed_files: Vec<Option<RepoPath>> = suite
            .test_cases
            .iter()
            .map(|test_case| test_case_file(test_case, &suites_inner_first, self.report_paths))
            .collect();
        match (only_file(&attributed_files), suite.time) {
            (Some(file), Some(suite_time)) => {
                let combined_status = suite
                    .test_cases
                    .iter()
                    .fold(TestStatus::Skipped, |status, test_case| {
                        status.combined_with(test_case.status())
                    });

                self.outcomes.record(file, combined_status, suite_time);
            }
            _ => {
                for (test_case, file) in suite.test_cases.iter().zip(attributed_files) {
                    record_test_case(self.outcomes, test_case, file);
                }
            }
        }
    }
}

fn only_file(attributed_files: &[Option<RepoPath>]) -> Option<RepoPath> {
    let first_file = attributed_files.first()?.as_ref()?;
    let every_test_case_in_first_file = attributed_files
        .iter()
        .all(|file| file.as_ref() == Some(first_file));

    every_test_case_in_first_file.then(|| first_file.clone())
}

fn record_test_case(outcomes: &mut TestOutcomes, test_case: &OpenTestCase, file: Option<RepoPath>) {
    match file {
        Some(file) => outcomes.record(file, test_case.status(), test_case.time),
        None => outcomes.record_unattributed(test_case.name.trim().to_owned(), test_case.status()),
    }
}

fn suite_file(suites_inner_first: &[&OpenSuite], report_paths: &ReportPaths) -> Option<RepoPath> {
    let explicit_files = suites_inner_first
        .iter()
        .filter_map(|suite| suite.file.as_deref());
    let path_like_names = suites_inner_first
        .iter()
        .filter_map(|suite| suite.name.as_deref())
        .filter(|name| looks_like_test_file(name));

    explicit_files
        .chain(path_like_names)
        .find_map(|candidate| report_paths.resolve(candidate))
}

fn test_case_file(
    test_case: &OpenTestCase,
    suites_inner_first: &[&OpenSuite],
    report_paths: &ReportPaths,
) -> Option<RepoPath> {
    let explicit_files = iter::once(test_case.file.as_deref())
        .chain(suites_inner_first.iter().map(|suite| suite.file.as_deref()))
        .flatten();
    let path_like_names = iter::once(test_case.class_name.as_deref())
        .chain(suites_inner_first.iter().map(|suite| suite.name.as_deref()))
        .chain(iter::once(Some(test_case.name.as_str())))
        .flatten()
        .filter(|name| looks_like_test_file(name));

    explicit_files
        .chain(path_like_names)
        .find_map(|candidate| report_paths.resolve(candidate))
}

pub fn looks_like_test_file(name: &str) -> bool {
    let has_whitespace = name.chars().any(char::is_whitespace);
    let extension = name
        .rsplit('/')
        .next()
        .and_then(|file_name| file_name.rsplit_once('.'))
        .map(|(_, extension)| extension);

    !has_whitespace
        && extension.is_some_and(|extension| TEST_SOURCE_EXTENSIONS.contains(&extension))
}

fn invalid_report(position: u64, error: impl Display) -> anyhow::Error {
    anyhow!("invalid JUnit report at byte {position}: {error}")
}

fn attribute(element: &BytesStart, name: &str) -> Result<Option<String>> {
    let Some(attribute) = element.try_get_attribute(name)? else {
        return Ok(None);
    };
    let value = attribute.normalized_value(XmlVersion::Implicit1_0)?;

    Ok(Some(value.into_owned()))
}

fn seconds(value: Option<String>) -> Option<Duration> {
    let seconds: f64 = value?.trim().parse().ok()?;

    Duration::try_from_secs_f64(seconds).ok()
}

fn normalize(path: &str) -> Option<RepoPath> {
    let mut components = Vec::new();

    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            name => components.push(name),
        }
    }

    RepoPath::new(components.join("/")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO_ROOT: &str = "/home/runner/work/app/app";

    fn read(xml: &str, runner_directory: Option<&str>) -> TestOutcomes {
        let runner_directory = runner_directory.map(|directory| RepoPath::new(directory).unwrap());
        let report_paths = ReportPaths::new(Path::new(REPO_ROOT), runner_directory);
        let mut outcomes = TestOutcomes::default();

        read_junit_report(xml, &report_paths, &mut outcomes).unwrap();

        outcomes
    }

    fn statuses(outcomes: &TestOutcomes) -> Vec<(&str, TestStatus)> {
        outcomes
            .files()
            .map(|outcome| (outcome.file.as_str(), outcome.status))
            .collect()
    }

    fn durations(outcomes: &TestOutcomes) -> Vec<(&str, Duration)> {
        outcomes
            .files()
            .map(|outcome| (outcome.file.as_str(), outcome.duration))
            .collect()
    }

    fn unattributed_names(outcomes: &TestOutcomes) -> Vec<&str> {
        outcomes
            .unattributed_test_cases()
            .iter()
            .map(|test_case| test_case.name.as_str())
            .collect()
    }

    #[test]
    fn surefire_reruns_mark_a_test_case_flaky_or_keep_it_failed() {
        let xml = r#"
            <testsuites>
              <testsuite name="src/a.test.ts">
                <testcase classname="src/a.test.ts" name="passes after a rerun"><flakyFailure message="timeout"/></testcase>
              </testsuite>
              <testsuite name="src/b.test.ts">
                <testcase classname="src/b.test.ts" name="fails every rerun"><failure/><rerunFailure/></testcase>
              </testsuite>
            </testsuites>
        "#;

        assert_eq!(
            statuses(&read(xml, None)),
            [
                ("src/a.test.ts", TestStatus::Flaky),
                ("src/b.test.ts", TestStatus::Failed),
            ]
        );
    }

    #[test]
    fn failure_outside_test_cases_fails_the_suite_file() {
        let xml = r#"<testsuite name="src/a.test.ts" time="0.1"><error message="cannot load"/></testsuite>"#;

        assert_eq!(
            statuses(&read(xml, None)),
            [("src/a.test.ts", TestStatus::Failed)]
        );
    }

    #[test]
    fn nested_suites_attribute_test_cases_to_the_closest_file() {
        let xml = r#"
            <testsuites>
              <testsuite name="unit" file="packages/a/src/a.test.ts">
                <testsuite name="describe block">
                  <testcase name="works" time="0.01"/>
                </testsuite>
              </testsuite>
            </testsuites>
        "#;

        assert_eq!(
            statuses(&read(xml, None)),
            [("packages/a/src/a.test.ts", TestStatus::Passed)]
        );
    }

    #[test]
    fn suite_time_counts_when_every_test_case_belongs_to_one_file() {
        let xml = r#"
            <testsuite name="src/a.test.ts" time="0.5">
              <testcase classname="src/a.test.ts" name="first" time="0.1"/>
              <testcase classname="src/a.test.ts" name="second" time="0.1"/>
            </testsuite>
        "#;

        assert_eq!(
            durations(&read(xml, None)),
            [("src/a.test.ts", Duration::from_millis(500))]
        );
    }

    #[test]
    fn test_case_times_are_summed_when_a_suite_spans_several_files() {
        let xml = r#"
            <testsuite name="unit" time="0.9">
              <testcase file="src/a.test.ts" name="first" time="0.2"/>
              <testcase file="src/b.test.ts" name="second" time="0.3"/>
              <testcase file="src/a.test.ts" name="third" time="0.1"/>
            </testsuite>
        "#;

        assert_eq!(
            durations(&read(xml, None)),
            [
                ("src/a.test.ts", Duration::from_millis(300)),
                ("src/b.test.ts", Duration::from_millis(300)),
            ]
        );
    }

    #[test]
    fn reported_paths_resolve_to_repo_paths_or_stay_unattributed() {
        let xml = r#"
            <testsuite name="unit">
              <testcase name="absolute" file="/home/runner/work/app/app/src/absolute.test.ts"/>
              <testcase name="dotted" file="./src/dotted.test.ts"/>
              <testcase name="windows" file="src\windows.test.ts"/>
              <testcase name="parent" file="src/nested/../parent.test.ts"/>
              <testcase name="outside the repository" file="/elsewhere/outside.test.ts"/>
              <testcase name="escaping the repository" file="../escaping.test.ts"/>
            </testsuite>
        "#;
        let outcomes = read(xml, None);
        let files: Vec<&str> = statuses(&outcomes)
            .into_iter()
            .map(|(file, _)| file)
            .collect();

        assert_eq!(
            files,
            [
                "src/absolute.test.ts",
                "src/dotted.test.ts",
                "src/parent.test.ts",
                "src/windows.test.ts",
            ]
        );
        assert_eq!(
            unattributed_names(&outcomes),
            ["outside the repository", "escaping the repository"]
        );
    }

    #[test]
    fn relative_paths_start_from_the_runner_directory() {
        let xml = r#"
            <testsuite name="unit">
              <testcase name="own" file="src/own.test.ts"/>
              <testcase name="sibling" file="../shared/sibling.test.ts"/>
            </testsuite>
        "#;
        let outcomes = read(xml, Some("packages/web"));
        let files: Vec<&str> = statuses(&outcomes)
            .into_iter()
            .map(|(file, _)| file)
            .collect();

        assert_eq!(
            files,
            [
                "packages/shared/sibling.test.ts",
                "packages/web/src/own.test.ts"
            ]
        );
    }

    #[test]
    fn titles_and_dotted_class_names_are_not_test_files() {
        let test_files = [
            "src/a.test.ts",
            "tests/test_api.py",
            "crates/core/tests/select.rs",
        ];
        let other_names = [
            "math adds",
            "tests.test_api",
            "undefined",
            "src/a.test.ts > adds",
        ];

        for test_file in test_files {
            assert!(looks_like_test_file(test_file), "{test_file}");
        }

        for other_name in other_names {
            assert!(!looks_like_test_file(other_name), "{other_name}");
        }
    }

    #[test]
    fn malformed_xml_is_an_error_with_its_position() {
        let report_paths = ReportPaths::new(Path::new(REPO_ROOT), None);
        let mut outcomes = TestOutcomes::default();
        let error = read_junit_report(
            r#"<testsuites><testsuite name="a"></testsuites>"#,
            &report_paths,
            &mut outcomes,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .starts_with("invalid JUnit report at byte "),
            "{error}"
        );
    }
}
