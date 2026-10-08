use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

const CONFIG: &str = r#"
repository = "acme/app"
workflow = "CI"
base_branch = "main"
test_jobs = ["unit-*"]
pre_existing_window_hours = 24
"#;

const RUNS: &[(u64, &str, &str, &str, &str)] = &[
    (101, "pull_request", "aaa", "fix/a", "2026-10-08T10:00:00Z"),
    (102, "pull_request", "aaa", "fix/a", "2026-10-08T11:00:00Z"),
    (103, "pull_request", "bbb", "fix/b", "2026-10-08T12:00:00Z"),
    (
        104,
        "pull_request",
        "ccc",
        "chore/lint",
        "2026-10-08T12:30:00Z",
    ),
    (105, "push", "ddd", "main", "2026-10-08T13:00:00Z"),
    (106, "push", "eee", "feature-x", "2026-10-08T13:30:00Z"),
    (107, "schedule", "fff", "main", "2026-10-01T00:00:00Z"),
];

const JOB_LOGS: &[(u64, u64, &str)] = &[
    (101, 1, "2026-10-08T10:05:00Z  FAIL  src/a.test.ts > adds\n"),
    (102, 2, "2026-10-08T11:05:00Z  FAIL  src/a.test.ts > adds\n"),
    (
        103,
        3,
        "2026-10-08T12:05:00Z  FAIL  src/b.test.ts > parses\n",
    ),
    (
        105,
        5,
        "2026-10-08T13:05:00Z  FAIL  src/b.test.ts > parses\n",
    ),
    (106, 6, "2026-10-08T13:35:00Z  FAIL  src/a.test.ts > adds\n"),
    (107, 7, "2026-10-01T00:05:00Z  FAIL  src/a.test.ts > adds\n"),
];

const JUNIT_REPORT_OF_RUN_103: &str = r#"<testsuites>
  <testsuite name="unit" time="1.0">
    <testcase name="c works" file="/home/runner/work/app/app/src/c.test.ts" time="1.0"><failure/></testcase>
  </testsuite>
</testsuites>"#;

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn snapshot_with_failed_runs(directory: &Path) {
    let runs_lines: String = RUNS
        .iter()
        .map(|(run_id, event, head_sha, head_branch, created_at)| {
            format!(
                "{{\"databaseId\":{run_id},\"event\":\"{event}\",\"headSha\":\"{head_sha}\",\"headBranch\":\"{head_branch}\",\"createdAt\":\"{created_at}\",\"url\":\"https://github.com/acme/app/actions/runs/{run_id}\"}}\n"
            )
        })
        .collect();

    write(&directory.join("config.toml"), CONFIG);
    write(&directory.join("snapshot/runs.jsonl"), &runs_lines);

    for (run_id, job_id, job_log) in JOB_LOGS {
        write(
            &directory.join(format!("snapshot/logs/{run_id}/{job_id}.log")),
            job_log,
        );
    }

    write(
        &directory.join("snapshot/junit/103/unit-report/report.xml"),
        JUNIT_REPORT_OF_RUN_103,
    );
}

#[test]
fn failed_pull_request_runs_become_changes_with_their_failed_test_files() {
    let directory = TempDir::new().unwrap();

    snapshot_with_failed_runs(directory.path());

    let output = Command::new(env!("CARGO_BIN_EXE_prober-replay"))
        .args(["failures", "--config"])
        .arg(directory.path().join("config.toml"))
        .arg("--snapshot")
        .arg(directory.path().join("snapshot"))
        .output()
        .unwrap();
    let changes = fs::read_to_string(directory.path().join("snapshot/changes.jsonl")).unwrap();
    let failures = fs::read_to_string(directory.path().join("snapshot/failures.jsonl")).unwrap();

    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "prober-replay: 4 failed runs on pull requests and 2 on main: 2 changes with 3 failed test files (1 pre-existing); 1 failed runs without a failed test file\n"
    );
    assert_eq!(
        changes,
        "{\"id\":\"aaa\",\"base\":\"main\",\"head\":\"aaa\"}\n{\"id\":\"bbb\",\"base\":\"main\",\"head\":\"bbb\"}\n"
    );
    assert_eq!(
        failures,
        concat!(
            "{\"change\":\"aaa\",\"file\":\"src/a.test.ts\",\"runs\":[101,102],\"pre_existing\":false}\n",
            "{\"change\":\"bbb\",\"file\":\"src/b.test.ts\",\"runs\":[103],\"pre_existing\":true}\n",
            "{\"change\":\"bbb\",\"file\":\"src/c.test.ts\",\"runs\":[103],\"pre_existing\":false}\n",
        )
    );
}
