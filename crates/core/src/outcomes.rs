//! Test outcomes per test file, merged from the test cases of one or several test reports.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::files::RepoPath;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestStatus {
    Skipped,
    Passed,
    Flaky,
    Failed,
}

impl TestStatus {
    pub fn combined_with(self, other: TestStatus) -> TestStatus {
        if other.severity() > self.severity() {
            return other;
        }

        self
    }

    fn severity(self) -> u8 {
        match self {
            TestStatus::Skipped => 0,
            TestStatus::Passed => 1,
            TestStatus::Flaky => 2,
            TestStatus::Failed => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestFileOutcome {
    pub file: RepoPath,
    pub status: TestStatus,
    pub duration: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnattributedTestCase {
    pub name: String,
    pub status: TestStatus,
}

#[derive(Debug, Default)]
pub struct TestOutcomes {
    outcomes_by_file: BTreeMap<RepoPath, TestFileOutcome>,
    unattributed_test_cases: Vec<UnattributedTestCase>,
}

impl TestOutcomes {
    pub fn record(&mut self, file: RepoPath, status: TestStatus, duration: Duration) {
        let outcome = self
            .outcomes_by_file
            .entry(file.clone())
            .or_insert_with(|| TestFileOutcome {
                file,
                status,
                duration: Duration::ZERO,
            });

        outcome.status = outcome.status.combined_with(status);
        outcome.duration += duration;
    }

    pub fn record_unattributed(&mut self, name: String, status: TestStatus) {
        self.unattributed_test_cases
            .push(UnattributedTestCase { name, status });
    }

    pub fn files(&self) -> impl Iterator<Item = &TestFileOutcome> {
        self.outcomes_by_file.values()
    }

    pub fn outcome(&self, file: &RepoPath) -> Option<&TestFileOutcome> {
        self.outcomes_by_file.get(file)
    }

    pub fn unattributed_test_cases(&self) -> &[UnattributedTestCase] {
        &self.unattributed_test_cases
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn milliseconds(count: u64) -> Duration {
        Duration::from_millis(count)
    }

    #[test]
    fn a_file_takes_the_most_severe_status_of_its_test_cases() {
        let mut outcomes = TestOutcomes::default();
        let file = repo_path("src/a.test.ts");

        outcomes.record(file.clone(), TestStatus::Skipped, milliseconds(0));
        outcomes.record(file.clone(), TestStatus::Passed, milliseconds(3));

        assert_eq!(outcomes.outcome(&file).unwrap().status, TestStatus::Passed);

        outcomes.record(file.clone(), TestStatus::Flaky, milliseconds(5));

        assert_eq!(outcomes.outcome(&file).unwrap().status, TestStatus::Flaky);

        outcomes.record(file.clone(), TestStatus::Failed, milliseconds(7));
        outcomes.record(file.clone(), TestStatus::Passed, milliseconds(11));

        assert_eq!(
            outcomes.outcome(&file),
            Some(&TestFileOutcome {
                file,
                status: TestStatus::Failed,
                duration: milliseconds(26),
            })
        );
    }

    #[test]
    fn files_are_listed_by_path() {
        let mut outcomes = TestOutcomes::default();

        outcomes.record(
            repo_path("src/z.test.ts"),
            TestStatus::Passed,
            milliseconds(1),
        );
        outcomes.record(
            repo_path("src/a.test.ts"),
            TestStatus::Failed,
            milliseconds(1),
        );

        let files: Vec<&str> = outcomes
            .files()
            .map(|outcome| outcome.file.as_str())
            .collect();

        assert_eq!(files, ["src/a.test.ts", "src/z.test.ts"]);
    }
}
