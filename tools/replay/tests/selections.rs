#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

const STUB_PROBER: &str = r#"#!/bin/sh
head_commit=$(git rev-parse HEAD)
echo "$head_commit" >> "$PROBER_CALLS"
printf '{"schema_version":1,"revisions":{"base":"%s","head":"%s"},"mode":"selected","stats":{"selected_ratio":0.25}}\n' "$2" "$head_commit"
echo "prober: selected 1/4 test files (25.0%)" >&2
"#;

const CONFIG_READING_PROBER: &str = r#"#!/bin/sh
cat prober.toml >> "$PROBER_CALLS"
printf '{"schema_version":1,"mode":"none","stats":{"selected_ratio":0.0}}\n'
"#;

const FAILING_PROBER: &str = r#"#!/bin/sh
echo "prober: unknown revision \"$2\"" >&2
exit 1
"#;

struct ReplayFixture {
    directory: TempDir,
    commits: Vec<String>,
}

struct ReplayRun {
    stderr: String,
    exit_code: i32,
}

impl ReplayFixture {
    fn with_commits(count: usize) -> Self {
        let directory = TempDir::new().unwrap();

        fs::create_dir(directory.path().join("repo")).unwrap();

        let fixture = Self {
            directory,
            commits: Vec::new(),
        };

        fixture.git(&["init", "--quiet", "--initial-branch=main"]);

        let commits = (1..=count)
            .map(|commit_number| {
                fs::write(
                    fixture
                        .repository()
                        .join(format!("file-{commit_number}.ts")),
                    "export {};",
                )
                .unwrap();
                fixture.git(&["add", "--all"]);
                fixture.git(&[
                    "commit",
                    "--quiet",
                    "--message",
                    &format!("commit {commit_number}"),
                ]);

                fixture.git(&["rev-parse", "HEAD"]).trim().to_owned()
            })
            .collect();

        fixture.write_script("prober", STUB_PROBER);
        fixture.write_script("failing-prober", FAILING_PROBER);
        fixture.write_script("config-reading-prober", CONFIG_READING_PROBER);

        Self { commits, ..fixture }
    }

    fn repository(&self) -> PathBuf {
        self.directory.path().join("repo")
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    fn git(&self, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(self.repository())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Prober Tests")
            .env("GIT_AUTHOR_EMAIL", "tests@prober.invalid")
            .env("GIT_COMMITTER_NAME", "Prober Tests")
            .env("GIT_COMMITTER_EMAIL", "tests@prober.invalid")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8(output.stdout).unwrap()
    }

    fn write_script(&self, name: &str, content: &str) {
        let script_path = self.path(name);

        fs::write(&script_path, content).unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn replay(&self, arguments: &[&str]) -> ReplayRun {
        self.replay_with(&self.repository(), "prober", arguments)
    }

    fn replay_with_prober(&self, prober_script: &str, arguments: &[&str]) -> ReplayRun {
        self.replay_with(&self.repository(), prober_script, arguments)
    }

    fn replay_with(&self, repository: &Path, prober_script: &str, arguments: &[&str]) -> ReplayRun {
        let output = self.path("records.jsonl");
        let worktree = self.path("worktree");
        let output = Command::new(env!("CARGO_BIN_EXE_prober-replay"))
            .args([
                "selections",
                "--repo",
                repository.to_str().unwrap(),
                "--output",
                output.to_str().unwrap(),
                "--worktree",
                worktree.to_str().unwrap(),
                "--prober",
                self.path(prober_script).to_str().unwrap(),
            ])
            .args(arguments)
            .env("PROBER_CALLS", self.path("prober-calls.txt"))
            .output()
            .unwrap();

        ReplayRun {
            stderr: String::from_utf8(output.stderr).unwrap(),
            exit_code: output.status.code().unwrap(),
        }
    }

    fn records(&self) -> Vec<Value> {
        fs::read_to_string(self.path("records.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn prober_calls(&self) -> usize {
        fs::read_to_string(self.path("prober-calls.txt")).map_or(0, |calls| calls.lines().count())
    }
}

fn record_fields(record: &Value) -> (&str, &str, &str) {
    (
        record["id"].as_str().unwrap(),
        record["base"].as_str().unwrap(),
        record["selection"]["revisions"]["head"].as_str().unwrap(),
    )
}

#[test]
fn last_commits_are_replayed_oldest_first_against_their_first_parent() {
    let fixture = ReplayFixture::with_commits(4);
    let run = fixture.replay(&["--last", "2"]);
    let records = fixture.records();
    let commits = &fixture.commits;

    assert_eq!(run.exit_code, 0, "{}", run.stderr);
    assert_eq!(
        records.iter().map(record_fields).collect::<Vec<_>>(),
        [
            (
                commits[2].as_str(),
                commits[1].as_str(),
                commits[2].as_str()
            ),
            (
                commits[3].as_str(),
                commits[2].as_str(),
                commits[3].as_str()
            ),
        ]
    );
    assert_eq!(records[0]["exit_code"], 0);
    assert_eq!(
        records[0]["stderr"],
        "prober: selected 1/4 test files (25.0%)\n"
    );
    assert!(
        run.stderr.starts_with(
            "prober-replay: replayed 2 changes, 0 already recorded, 0 failed — 2 selected, 0 all, 0 none; median 25.0% of test files selected; "
        ),
        "{}",
        run.stderr
    );
}

#[test]
fn root_commit_has_no_parent_to_replay_against() {
    let fixture = ReplayFixture::with_commits(3);

    fixture.replay(&["--last", "10"]);

    assert_eq!(fixture.records().len(), 2);
}

#[test]
fn changes_from_a_file_are_replayed_with_their_id() {
    let fixture = ReplayFixture::with_commits(3);
    let commits = &fixture.commits;
    let changes_path = fixture.path("changes.jsonl");

    fs::write(
        &changes_path,
        format!(
            "{{\"id\": \"pr-7\", \"base\": \"{}\", \"head\": \"{}\"}}\n\n{{\"base\": \"{}\", \"head\": \"{}\"}}\n",
            commits[0], commits[2], commits[0], commits[1]
        ),
    )
    .unwrap();
    fixture.replay(&["--changes", changes_path.to_str().unwrap()]);

    assert_eq!(
        fixture
            .records()
            .iter()
            .map(record_fields)
            .collect::<Vec<_>>(),
        [
            ("pr-7", commits[0].as_str(), commits[2].as_str()),
            (
                commits[1].as_str(),
                commits[0].as_str(),
                commits[1].as_str()
            ),
        ]
    );
}

#[test]
fn replay_resumes_by_skipping_changes_already_recorded() {
    let fixture = ReplayFixture::with_commits(4);

    fixture.replay(&["--last", "2"]);

    let second_run = fixture.replay(&["--last", "3"]);
    let third_run = fixture.replay(&["--last", "3"]);

    assert_eq!(fixture.records().len(), 3);
    assert_eq!(fixture.prober_calls(), 3);
    assert!(
        second_run
            .stderr
            .starts_with("prober-replay: replayed 1 changes, 2 already recorded"),
        "{}",
        second_run.stderr
    );
    assert_eq!(
        third_run.stderr,
        "prober-replay: nothing to replay, 3 changes already recorded\n"
    );
}

#[test]
fn failing_prober_run_is_recorded_without_a_selection() {
    let fixture = ReplayFixture::with_commits(2);
    let run = fixture.replay_with_prober("failing-prober", &["--last", "1"]);
    let records = fixture.records();

    assert_eq!(run.exit_code, 0);
    assert_eq!(records[0]["exit_code"], 1);
    assert_eq!(records[0]["selection"], Value::Null);
    assert!(
        records[0]["stderr"]
            .as_str()
            .unwrap()
            .starts_with("prober: unknown revision")
    );
    assert!(run.stderr.contains(", 1 failed — "), "{}", run.stderr);
}

#[test]
fn unknown_head_commit_is_recorded_as_an_error() {
    let fixture = ReplayFixture::with_commits(2);
    let changes_path = fixture.path("changes.jsonl");

    fs::write(
        &changes_path,
        format!(
            "{{\"id\": \"gone\", \"base\": \"{}\", \"head\": \"0000000000000000000000000000000000000000\"}}\n{{\"id\": \"present\", \"base\": \"{}\", \"head\": \"{}\"}}\n",
            fixture.commits[0], fixture.commits[0], fixture.commits[1]
        ),
    )
    .unwrap();
    fixture.replay(&["--changes", changes_path.to_str().unwrap()]);

    let records = fixture.records();

    assert!(
        records[0]["error"].as_str().unwrap().starts_with(
            "git checkout --quiet --force --detach 0000000000000000000000000000000000000000 failed"
        ),
        "{}",
        records[0]["error"]
    );
    assert_eq!(records[1]["id"], "present");
    assert_eq!(records[1]["error"], Value::Null);
}

#[test]
fn prober_config_is_placed_in_the_worktree_for_each_change_and_removed_after() {
    let fixture = ReplayFixture::with_commits(3);
    let config_path = fixture.path("inference-off.toml");

    fs::write(&config_path, "[inference]\ndynamic_imports = false\n").unwrap();

    let run = fixture.replay_with_prober(
        "config-reading-prober",
        &[
            "--last",
            "2",
            "--prober-config",
            config_path.to_str().unwrap(),
        ],
    );

    assert_eq!(run.exit_code, 0, "{}", run.stderr);
    assert_eq!(
        fs::read_to_string(fixture.path("prober-calls.txt")).unwrap(),
        "[inference]\ndynamic_imports = false\n[inference]\ndynamic_imports = false\n"
    );
    assert!(!fixture.path("worktree").join("prober.toml").exists());
    assert!(
        fixture
            .records()
            .iter()
            .all(|record| record["error"].is_null())
    );
}

#[test]
fn prober_config_never_replaces_a_tracked_one() {
    let fixture = ReplayFixture::with_commits(1);
    let config_path = fixture.path("inference-off.toml");

    fs::write(fixture.repository().join("prober.toml"), "").unwrap();
    fixture.git(&["add", "--all"]);
    fixture.git(&["commit", "--quiet", "--message", "add prober.toml"]);
    fs::write(&config_path, "[inference]\ndynamic_imports = false\n").unwrap();

    fixture.replay_with_prober(
        "config-reading-prober",
        &[
            "--last",
            "1",
            "--prober-config",
            config_path.to_str().unwrap(),
        ],
    );

    assert_eq!(
        fixture.records()[0]["error"],
        "the repository has its own prober.toml, which --prober-config would replace"
    );
    assert_eq!(fixture.prober_calls(), 0);
}

#[test]
fn setup_command_runs_once_when_the_worktree_is_created() {
    let fixture = ReplayFixture::with_commits(4);
    let setup_marker = fixture.path("worktree").join("setup-marker");

    fixture.replay(&["--last", "1", "--setup", "touch setup-marker"]);

    let created_on_first_run = setup_marker.exists();

    fs::remove_file(&setup_marker).unwrap();
    fixture.replay(&["--last", "2", "--setup", "touch setup-marker"]);

    assert!(created_on_first_run);
    assert!(!Path::new(&setup_marker).exists());
    assert_eq!(fixture.records().len(), 2);
}

#[test]
fn missing_heads_are_fetched_from_origin_when_asked() {
    let fixture = ReplayFixture::with_commits(2);
    let clone = fixture.path("clone");
    let changes_path = fixture.path("changes.jsonl");

    fixture.git(&[
        "clone",
        "--quiet",
        fixture.repository().to_str().unwrap(),
        clone.to_str().unwrap(),
    ]);
    fs::write(fixture.repository().join("late.ts"), "export {};").unwrap();
    fixture.git(&["add", "--all"]);
    fixture.git(&["commit", "--quiet", "--message", "late commit"]);

    let late_commit = fixture.git(&["rev-parse", "HEAD"]).trim().to_owned();

    fs::write(
        &changes_path,
        format!(
            "{{\"base\": \"{}\", \"head\": \"{late_commit}\"}}\n",
            fixture.commits[0]
        ),
    )
    .unwrap();
    fixture.replay_with(
        &clone,
        "prober",
        &["--changes", changes_path.to_str().unwrap(), "--fetch"],
    );

    let records = fixture.records();

    assert_eq!(records[0]["error"], Value::Null);
    assert_eq!(records[0]["selection"]["revisions"]["head"], late_commit);
}
