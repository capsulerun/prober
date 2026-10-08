//! The prober binary (selection and shadow) run end to end on small git repositories holding a TypeScript project.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

struct TestRepository {
    directory: TempDir,
}

struct ProberRun {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

impl ProberRun {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap()
    }
}

impl TestRepository {
    fn with_project_on_main() -> Self {
        let test_repository = Self {
            directory: TempDir::new().unwrap(),
        };

        test_repository.git(&["init", "--quiet", "--initial-branch=main"]);
        test_repository.write("package.json", r#"{ "name": "app" }"#);
        test_repository.write("src/a.ts", "export const a = 1;");
        test_repository.write("src/a.test.ts", r#"import { a } from "./a";"#);
        test_repository.write(
            "src/b.ts",
            r#"import { a } from "./a"; export const b = a;"#,
        );
        test_repository.write("src/b.test.ts", r#"import { b } from "./b";"#);
        test_repository.write("docs/guide.md", "# Guide");
        test_repository.commit_all("project");
        test_repository.git(&["switch", "--quiet", "--create", "feature"]);

        fs::create_dir(test_repository.path().join("node_modules")).unwrap();

        test_repository
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }

    fn git(&self, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(self.path())
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

    fn commit_id(&self, revision: &str) -> String {
        self.git(&["rev-parse", revision]).trim().to_owned()
    }

    fn write(&self, path: &str, content: &str) {
        let file_path = self.path().join(path);

        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(file_path, content).unwrap();
    }

    fn commit_all(&self, message: &str) {
        self.git(&["add", "--all"]);
        self.git(&["commit", "--quiet", "--message", message]);
    }

    fn change_and_commit(&self, path: &str, content: &str) {
        self.write(path, content);
        self.commit_all(&format!("change {path}"));
    }

    fn prober(&self, arguments: &[&str]) -> ProberRun {
        run_prober(self.path(), arguments, None)
    }

    fn shadow_inputs(&self, full_run_report: &str) -> ShadowInputs {
        let directory = TempDir::new().unwrap();
        let selection = directory.path().join("prober.json");
        let report = directory.path().join("report.xml");
        let selection_run = self.prober(&["--base", "main", "--format", "json"]);

        fs::write(&selection, selection_run.stdout).unwrap();
        fs::write(&report, full_run_report).unwrap();

        ShadowInputs {
            directory,
            selection,
            report,
        }
    }
}

struct ShadowInputs {
    directory: TempDir,
    selection: PathBuf,
    report: PathBuf,
}

impl ShadowInputs {
    fn arguments(&self) -> Vec<&str> {
        vec![
            "shadow",
            "--selection",
            self.selection.to_str().unwrap(),
            "--report",
            self.report.to_str().unwrap(),
        ]
    }

    fn write(&self, name: &str, content: &str) -> PathBuf {
        let path = self.directory.path().join(name);

        fs::write(&path, content).unwrap();

        path
    }
}

const FULL_RUN_REPORT: &str = r#"<testsuites>
  <testsuite name="src/a.test.ts" time="2.0">
    <testcase classname="src/a.test.ts" name="a" time="2.0"><failure message="broken"/></testcase>
  </testsuite>
  <testsuite name="src/b.test.ts" time="1.0">
    <testcase classname="src/b.test.ts" name="b" time="1.0"/>
  </testsuite>
</testsuites>"#;

fn run_prober(directory: &Path, arguments: &[&str], job_summary: Option<&Path>) -> ProberRun {
    let mut command = Command::new(env!("CARGO_BIN_EXE_prober"));

    command
        .args(arguments)
        .current_dir(directory)
        .env_remove("GITHUB_STEP_SUMMARY");

    if let Some(job_summary) = job_summary {
        command.env("GITHUB_STEP_SUMMARY", job_summary);
    }

    let output: Output = command.output().unwrap();

    ProberRun {
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
        exit_code: output.status.code().unwrap(),
    }
}

#[test]
fn lists_the_tests_impacted_by_the_branch_closest_first() {
    let repository = TestRepository::with_project_on_main();

    repository.change_and_commit("src/a.ts", "export const a = 2;");

    let run = repository.prober(&["--base", "main"]);

    assert_eq!(run.exit_code, 0);
    assert_eq!(run.stdout, "src/a.test.ts\nsrc/b.test.ts\n");
    assert_eq!(run.stderr, "prober: selected 2/2 test files (100.0%)\n");
}

#[test]
fn json_report_explains_each_selected_test() {
    let repository = TestRepository::with_project_on_main();

    repository.change_and_commit("src/b.ts", "export const b = 2;");

    let report = repository
        .prober(&["--base", "main", "--format", "json"])
        .json();

    assert_eq!(report["mode"], "selected");
    assert_eq!(report["mode_reasons"], Value::Null);
    assert_eq!(
        report["targets"],
        json!({ "typescript": ["src/b.test.ts"] })
    );
    assert_eq!(
        report["reasons"],
        json!({ "src/b.test.ts": ["src/b.ts (modified)", "-> imported by src/b.test.ts"] })
    );
    assert_eq!(report["distances"], json!({ "src/b.test.ts": 1 }));
    assert_eq!(report["stats"]["tests_total"], 2);
    assert_eq!(
        report["revisions"],
        json!({
            "base": repository.commit_id("main"),
            "head": repository.commit_id("HEAD"),
            "merge_base": repository.commit_id("main"),
        })
    );
}

#[test]
fn docs_only_change_selects_no_test() {
    let repository = TestRepository::with_project_on_main();

    repository.change_and_commit("docs/guide.md", "# Guide, revised");

    let run = repository.prober(&["--base", "main"]);

    assert_eq!(run.exit_code, 0);
    assert_eq!(run.stdout, "");
    assert_eq!(
        run.stderr,
        "prober: none of 2 test files — 1 changed file reaches no test file\n"
    );
}

#[test]
fn built_in_global_file_change_runs_every_test() {
    let repository = TestRepository::with_project_on_main();

    repository.change_and_commit("package.json", r#"{ "name": "app", "private": true }"#);

    let run = repository.prober(&["--base", "main", "--format", "json"]);
    let report = run.json();

    assert_eq!(report["mode"], "all");
    assert_eq!(
        report["mode_reasons"],
        json!([{
            "kind": "always_all",
            "file": "package.json",
            "pattern": "**/package.json",
            "message": "package.json (modified) matches always_all \"**/package.json\"",
        }])
    );
    assert_eq!(
        report["targets"],
        json!({ "typescript": ["src/a.test.ts", "src/b.test.ts"] })
    );
}

#[test]
fn configured_global_file_change_runs_every_test() {
    let repository = TestRepository::with_project_on_main();

    repository.write("prober.toml", "[always_all]\nfiles = [\"ci/**\"]\n");
    repository.change_and_commit("ci/deploy.sh", "echo deploy");

    let run = repository.prober(&["--base", "main"]);

    assert_eq!(run.stdout, "src/a.test.ts\nsrc/b.test.ts\n");
    assert_eq!(
        run.stderr,
        "prober: all 2 test files — ci/deploy.sh (added) matches always_all \"ci/**\"\n"
    );
}

#[test]
fn invalid_config_runs_every_test_with_an_error_reason() {
    let repository = TestRepository::with_project_on_main();

    repository.change_and_commit("prober.toml", "presets = \"auto\"\n");

    let report = repository
        .prober(&["--base", "main", "--format", "json"])
        .json();

    assert_eq!(report["mode"], "all");
    assert_eq!(
        report["mode_reasons"],
        json!([{
            "kind": "error",
            "message": "invalid prober.toml at line 1, column 1: unknown field `presets`, expected `always_all`",
        }])
    );
    assert_eq!(report["stats"]["tests_selected"], 2);
}

#[test]
fn on_error_fail_exits_with_the_error() {
    let repository = TestRepository::with_project_on_main();

    repository.change_and_commit("prober.toml", "presets = \"auto\"\n");

    let run = repository.prober(&["--base", "main", "--on-error", "fail"]);

    assert_eq!(run.exit_code, 1);
    assert_eq!(run.stdout, "");
    assert_eq!(
        run.stderr,
        "prober: invalid prober.toml at line 1, column 1: unknown field `presets`, expected `always_all`\n"
    );
}

#[test]
fn unknown_base_runs_every_test_with_an_error_reason() {
    let repository = TestRepository::with_project_on_main();

    repository.change_and_commit("src/a.ts", "export const a = 2;");

    let run = repository.prober(&["--base", "origin/main"]);

    assert_eq!(run.exit_code, 0);
    assert_eq!(run.stdout, "src/a.test.ts\nsrc/b.test.ts\n");
    assert_eq!(
        run.stderr,
        "prober: all 2 test files — unknown revision \"origin/main\"\n"
    );
}

#[test]
fn head_other_than_the_checked_out_commit_runs_every_test() {
    let repository = TestRepository::with_project_on_main();

    repository.change_and_commit("src/a.ts", "export const a = 2;");

    let report = repository
        .prober(&["--base", "main", "--head", "main", "--format", "json"])
        .json();

    assert_eq!(report["mode"], "all");
    assert_eq!(
        report["mode_reasons"][0]["message"],
        "--head \"main\" is not the checked-out commit: Prober reads files from the working tree, check it out first"
    );
}

#[test]
fn nul_separated_list_ends_each_path_with_nul() {
    let repository = TestRepository::with_project_on_main();

    repository.change_and_commit("src/a.ts", "export const a = 2;");

    let run = repository.prober(&["--base", "main", "-0"]);

    assert_eq!(run.stdout, "src/a.test.ts\0src/b.test.ts\0");
}

#[test]
fn nul_separator_is_rejected_with_json() {
    let repository = TestRepository::with_project_on_main();
    let run = repository.prober(&["--base", "main", "--format", "json", "-0"]);

    assert_eq!(run.exit_code, 2);
    assert!(run.stderr.contains("-0 only applies to --format list"));
}

#[test]
fn outside_a_repository_fails_even_with_on_error_all() {
    let directory = TempDir::new().unwrap();
    let run = run_prober(directory.path(), &["--base", "main"], None);

    assert_eq!(run.exit_code, 1);
    assert_eq!(run.stdout, "");
    assert!(
        run.stderr
            .starts_with("prober: no git repository found at "),
        "{}",
        run.stderr
    );
}

#[test]
fn missing_node_modules_is_warned_about_before_the_summary() {
    let repository = TestRepository::with_project_on_main();

    fs::remove_dir(repository.path().join("node_modules")).unwrap();
    repository.change_and_commit("src/a.ts", "export const a = 2;");

    let run = repository.prober(&["--base", "main"]);

    assert_eq!(
        run.stderr,
        "prober: warning: node_modules not found: imports between workspace packages cannot be resolved, run prober after installing dependencies\nprober: selected 2/2 test files (100.0%)\n"
    );
}

#[test]
fn shadow_reports_a_failed_test_that_was_not_selected_as_a_miss() {
    let repository = TestRepository::with_project_on_main();

    repository.change_and_commit("src/b.ts", "export const b = 2;");

    let shadow_inputs = repository.shadow_inputs(FULL_RUN_REPORT);
    let run = repository.prober(&shadow_inputs.arguments());
    let shadow_result = run.json();

    assert_eq!(run.exit_code, 0);
    assert_eq!(
        run.stderr,
        "prober shadow: 1 miss — 1 of 2 reported test files selected, 1 skipped (50.0%, 2.0 s of 3.0 s)\n"
    );
    assert_eq!(shadow_result["misses"], json!(["src/a.test.ts"]));
    assert_eq!(
        shadow_result["revisions"]["head"],
        repository.commit_id("HEAD")
    );
}

#[test]
fn shadow_appends_its_markdown_to_the_github_job_summary() {
    let repository = TestRepository::with_project_on_main();

    repository.change_and_commit("src/b.ts", "export const b = 2;");

    let shadow_inputs = repository.shadow_inputs(FULL_RUN_REPORT);
    let job_summary = shadow_inputs.write("summary.md", "Earlier step\n");
    let run = run_prober(
        repository.path(),
        &shadow_inputs.arguments(),
        Some(&job_summary),
    );
    let job_summary_content = fs::read_to_string(&job_summary).unwrap();

    assert_eq!(run.exit_code, 0);
    assert!(
        job_summary_content.starts_with("Earlier step\n### Prober shadow\n\n**1 miss**"),
        "{job_summary_content}"
    );
}

#[test]
fn sharded_reports_relative_to_the_runner_directory_are_merged() {
    let repository = TestRepository::with_project_on_main();

    repository.change_and_commit("src/b.ts", "export const b = 2;");

    let shadow_inputs = repository.shadow_inputs("<testsuites/>");
    let first_shard = shadow_inputs.write(
        "shard-1.xml",
        r#"<testsuite name="a.test.ts" time="2.0"><testcase classname="a.test.ts" name="a" time="2.0"/></testsuite>"#,
    );
    let second_shard = shadow_inputs.write(
        "shard-2.xml",
        r#"<testsuite name="b.test.ts" time="1.0"><testcase classname="b.test.ts" name="b" time="1.0"/></testsuite>"#,
    );
    let run = repository.prober(&[
        "shadow",
        "--selection",
        shadow_inputs.selection.to_str().unwrap(),
        "--report",
        first_shard.to_str().unwrap(),
        "--report",
        second_shard.to_str().unwrap(),
        "--runner-directory",
        "src",
    ]);

    assert_eq!(
        run.stderr,
        "prober shadow: no miss — 1 of 2 reported test files selected, 1 skipped (50.0%, 2.0 s of 3.0 s)\n"
    );
}

#[test]
fn shadow_errors_name_the_file_at_fault() {
    let repository = TestRepository::with_project_on_main();
    let shadow_inputs = repository.shadow_inputs("<testsuites><testsuite></testsuites>");
    let missing_selection = repository.prober(&[
        "shadow",
        "--selection",
        "missing.json",
        "--report",
        shadow_inputs.report.to_str().unwrap(),
    ]);
    let malformed_report = repository.prober(&shadow_inputs.arguments());

    assert_eq!(missing_selection.exit_code, 1);
    assert!(
        missing_selection
            .stderr
            .starts_with("prober shadow: cannot read missing.json: "),
        "{}",
        missing_selection.stderr
    );
    assert_eq!(malformed_report.exit_code, 1);
    assert!(
        malformed_report.stderr.starts_with(&format!(
            "prober shadow: {}: invalid JUnit report at byte ",
            shadow_inputs.report.display()
        )),
        "{}",
        malformed_report.stderr
    );
}

#[test]
fn shadow_requires_a_report() {
    let repository = TestRepository::with_project_on_main();
    let run = repository.prober(&["shadow", "--selection", "prober.json"]);

    assert_eq!(run.exit_code, 2);
    assert!(run.stderr.contains("--report <REPORT>"), "{}", run.stderr);
}
