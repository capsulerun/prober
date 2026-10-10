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
        Self::with_files_on_main(&[])
    }

    fn with_files_on_main(extra_files: &[(&str, &str)]) -> Self {
        let test_repository = Self {
            directory: TempDir::new().unwrap(),
        };

        test_repository.git(&["init", "--quiet", "--initial-branch=main"]);
        test_repository.write("package.json", r#"{ "name": "app" }"#);
        test_repository.write("src/a.ts", "export const a = 1;");
        test_repository.write(
            "src/a.test.ts",
            "import { a } from \"./a\";\n\ntest(\"a\", () => expect(a).toBe(1));\n",
        );
        test_repository.write(
            "src/b.ts",
            r#"import { a } from "./a"; export const b = a;"#,
        );
        test_repository.write(
            "src/b.test.ts",
            "import { b } from \"./b\";\n\ntest(\"b\", () => expect(b).toBe(1));\n",
        );
        test_repository.write("docs/guide.md", "# Guide");

        for &(path, content) in extra_files {
            test_repository.write(path, content);
        }

        test_repository.commit_all("project");
        test_repository.git(&["switch", "--quiet", "--create", "feature"]);

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
        run_prober(self.path(), arguments, None, &[])
    }

    fn prober_with_environment(
        &self,
        arguments: &[&str],
        environment: &[(&str, &str)],
    ) -> ProberRun {
        run_prober(self.path(), arguments, None, environment)
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

fn run_prober(
    directory: &Path,
    arguments: &[&str],
    job_summary: Option<&Path>,
    environment: &[(&str, &str)],
) -> ProberRun {
    let mut command = Command::new(env!("CARGO_BIN_EXE_prober"));

    command
        .args(arguments)
        .current_dir(directory)
        .env_remove("GITHUB_STEP_SUMMARY")
        .env_remove("PROBER_SELECTION")
        .envs(environment.iter().copied());

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

    repository.change_and_commit(
        "src/b.ts",
        r#"import { a } from "./a"; export const b = a + 1;"#,
    );

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
        json!({ "src/b.test.ts": [
            "src/b.ts (modified)",
            "-> b changed",
            "-> b used at load by src/b.test.ts",
        ] })
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
fn module_loaded_by_a_computed_import_selects_the_tests_of_its_loader_unless_widening_is_off() {
    let repository = TestRepository::with_project_on_main();

    repository.write(
        "src/loader.ts",
        "const name = process.env.PLUGIN;\nexport const plugin = await import(`./plugins/${name}.js`);\n",
    );
    repository.write("src/plugins/telegram.ts", "export const id = 1;");
    repository.write(
        "src/loader.test.ts",
        "import { plugin } from \"./loader\";\n\ntest(\"plugin\", () => expect(plugin).toBeDefined());\n",
    );
    repository.commit_all("plugin loader");
    repository.change_and_commit("src/plugins/telegram.ts", "export const id = 2;");

    let report = repository
        .prober(&["--base", "HEAD~1", "--format", "json"])
        .json();

    assert_eq!(
        report["reasons"],
        json!({ "src/loader.test.ts": [
            "src/plugins/telegram.ts (modified)",
            "-> loaded by src/loader.ts:2 (inferred glob src/plugins/*.js)",
            "-> loaded by src/loader.test.ts",
        ] })
    );

    repository.write("prober.toml", "[inference]\ndynamic_imports = false\n");

    let without_inference = repository.prober(&["--base", "HEAD~1", "--format", "json"]);

    assert_eq!(
        without_inference.json()["reasons"],
        json!({ "src/loader.test.ts": [
            "src/plugins/telegram.ts (modified)",
            "-> widened: unresolved dynamic import at src/loader.ts:2 (`./plugins/${name}.js`)",
            "-> loaded by src/loader.test.ts",
        ] })
    );
    assert_eq!(
        without_inference.stderr,
        "prober: selected 1/3 test files (33.3%), 1 added by widening\n"
    );

    repository.write(
        "prober.toml",
        "[inference]\ndynamic_imports = false\nwidening = false\n",
    );

    let without_widening = repository.prober(&["--base", "HEAD~1"]);

    assert_eq!(without_widening.stdout, "");
}

#[test]
fn change_to_one_function_selects_the_tests_using_it_unless_symbols_are_switched_off() {
    let repository = TestRepository::with_files_on_main(&[
        (
            "src/hub.ts",
            "export function parse(text: string) {\n  return text;\n}\n\nexport function format(value: string) {\n  return value;\n}\n",
        ),
        (
            "src/parse.test.ts",
            "import { parse } from \"./hub\";\n\ntest(\"parse\", () => parse(\"a\"));\n",
        ),
        (
            "src/format.test.ts",
            "import { format } from \"./hub\";\n\ntest(\"format\", () => format(\"a\"));\n",
        ),
    ]);

    repository.change_and_commit(
        "src/hub.ts",
        "export function parse(text: string) {\n  return text.trim();\n}\n\nexport function format(value: string) {\n  return value;\n}\n",
    );

    let symbol_level = repository.prober(&["--base", "main"]);
    let why_not = repository.prober(&["why", "src/format.test.ts", "--base", "main"]);

    repository.write("prober.toml", "[inference]\nsymbols = false\n");

    let file_level = repository.prober(&["--base", "main"]);

    assert_eq!(symbol_level.stdout, "src/parse.test.ts\n");
    assert_eq!(
        why_not.stdout,
        "src/format.test.ts is not selected: it reaches the changed files, but none of the symbols it uses changed ([inference] symbols = false follows files):\n  src/hub.ts (modified)\n  -> imported by src/format.test.ts\n"
    );
    assert_eq!(file_level.stdout, "src/format.test.ts\nsrc/parse.test.ts\n");
}

#[test]
fn comment_change_selects_nothing_and_top_level_code_selects_every_importer() {
    let repository = TestRepository::with_project_on_main();

    repository.change_and_commit("src/a.ts", "// The first value.\nexport const a = 1;\n");

    let comment = repository.prober(&["--base", "main"]);

    repository.change_and_commit("src/a.ts", "export const a = 1;\nconsole.log(a);\n");

    let top_level = repository
        .prober(&["--base", "HEAD~1", "--format", "json"])
        .json();

    assert_eq!(comment.stdout, "");
    assert_eq!(
        comment.stderr,
        "prober: none of 2 test files — 1 changed file reaches no test file\n"
    );
    assert_eq!(
        top_level["reasons"],
        json!({
            "src/a.test.ts": [
                "src/a.ts (modified)",
                "-> code run at load changed",
                "-> loaded by src/a.test.ts",
            ],
            "src/b.test.ts": [
                "src/a.ts (modified)",
                "-> code run at load changed",
                "-> loaded by src/b.ts",
                "-> loaded by src/b.test.ts",
            ],
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

    repository.change_and_commit(".npmrc", "registry=https://registry.example.com/\n");

    let run = repository.prober(&["--base", "main", "--format", "json"]);
    let report = run.json();

    assert_eq!(report["mode"], "all");
    assert_eq!(
        report["mode_reasons"],
        json!([{
            "kind": "always_all",
            "file": ".npmrc",
            "pattern": "**/.npmrc",
            "message": ".npmrc (added) matches always_all \"**/.npmrc\"",
        }])
    );
    assert_eq!(
        report["targets"],
        json!({ "typescript": ["src/a.test.ts", "src/b.test.ts"] })
    );
}

const LOCKFILE: &str = "lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      lodash:
        specifier: ^4.17.20
        version: 4.17.20
    devDependencies:
      eslint:
        specifier: ^9.0.0
        version: 9.0.0
      vitest:
        specifier: ^3.2.0
        version: 3.2.0

packages:
  eslint@9.0.0:
    resolution: {integrity: sha512-eslint}
  lodash@4.17.20:
    resolution: {integrity: sha512-lodash}
  vitest@3.2.0:
    resolution: {integrity: sha512-vitest}

snapshots:
  eslint@9.0.0: {}
  lodash@4.17.20: {}
  vitest@3.2.0: {}
";

const MANIFEST: &str = r#"{
  "name": "app",
  "version": "1.0.0",
  "dependencies": { "lodash": "^4.17.20" },
  "devDependencies": { "eslint": "^9.0.0", "vitest": "^3.2.0" }
}"#;

fn repository_with_dependencies() -> TestRepository {
    TestRepository::with_files_on_main(&[
        ("package.json", MANIFEST),
        ("pnpm-lock.yaml", LOCKFILE),
        (
            "src/a.ts",
            r#"import { chunk } from "lodash"; export const a = chunk;"#,
        ),
    ])
}

fn bump(repository: &TestRepository, package: &str, from: &str, to: &str) {
    let manifest = fs::read_to_string(repository.path().join("package.json")).unwrap();
    let lockfile = fs::read_to_string(repository.path().join("pnpm-lock.yaml")).unwrap();

    repository.write(
        "package.json",
        &manifest.replace(&format!("\"^{from}\""), &format!("\"^{to}\"")),
    );
    repository.write(
        "pnpm-lock.yaml",
        &lockfile
            .replace(&format!("{package}@{from}"), &format!("{package}@{to}"))
            .replace(&format!("^{from}"), &format!("^{to}"))
            .replace(&format!("version: {from}"), &format!("version: {to}")),
    );
    repository.commit_all(&format!("bump {package}"));
}

#[test]
fn release_version_bump_selects_no_test() {
    let repository = repository_with_dependencies();

    repository.change_and_commit("package.json", &MANIFEST.replace("1.0.0", "1.1.0"));

    let run = repository.prober(&["--base", "main"]);

    assert_eq!(run.stdout, "");
    assert_eq!(
        run.stderr,
        "prober: none of 2 test files — 1 changed file reaches no test file\n"
    );
}

#[test]
fn tooling_bump_selects_no_test() {
    let repository = repository_with_dependencies();

    bump(&repository, "eslint", "9.0.0", "9.1.0");

    let run = repository.prober(&["--base", "main"]);

    assert_eq!(run.stdout, "");
    assert_eq!(
        run.stderr,
        "prober: none of 2 test files — 2 changed files reach no test file\n"
    );
}

#[test]
fn runtime_dependency_bump_selects_the_tests_reaching_its_importers() {
    let repository = repository_with_dependencies();

    bump(&repository, "lodash", "4.17.20", "4.17.21");

    let report = repository
        .prober(&["--base", "main", "--format", "json"])
        .json();

    assert_eq!(report["mode"], "selected");
    assert_eq!(
        report["targets"],
        json!({ "typescript": ["src/a.test.ts", "src/b.test.ts"] })
    );
    assert_eq!(
        report["reasons"]["src/a.test.ts"],
        json!([
            "pnpm-lock.yaml (modified)",
            "-> package lodash changed (4.17.20 -> 4.17.21)",
            "-> used whole at load by src/a.ts",
            "-> loaded by src/a.test.ts",
        ])
    );
}

#[test]
fn runner_bump_runs_every_test_with_a_root_scope_reason() {
    let repository = repository_with_dependencies();

    bump(&repository, "vitest", "3.2.0", "3.2.1");

    let report = repository
        .prober(&["--base", "main", "--format", "json"])
        .json();

    assert_eq!(report["mode"], "all");
    assert_eq!(
        report["mode_reasons"],
        json!([{
            "kind": "root_scope",
            "file": "pnpm-lock.yaml",
            "package": "vitest",
            "message": "pnpm-lock.yaml (modified) changes package vitest (3.2.0 -> 3.2.1), which every test loads",
        }])
    );
}

#[test]
fn exports_change_selects_the_tests_whose_imports_resolve_elsewhere() {
    let lib_manifest = r#"{ "name": "@acme/lib", "exports": { ".": "./src/index.ts" } }"#;
    let repository = TestRepository::with_files_on_main(&[
        ("pnpm-workspace.yaml", "packages:\n  - packages/*\n"),
        ("packages/lib/package.json", lib_manifest),
        ("packages/lib/src/index.ts", "export const lib = 1;"),
        ("packages/lib/src/next.ts", "export const lib = 2;"),
        (
            "src/a.ts",
            r#"import { lib } from "@acme/lib"; export const a = lib;"#,
        ),
    ]);

    repository.change_and_commit(
        "packages/lib/package.json",
        &lib_manifest.replace("./src/index.ts", "./src/next.ts"),
    );

    let report = repository
        .prober(&["--base", "main", "--format", "json"])
        .json();

    assert_eq!(
        report["reasons"]["src/a.test.ts"],
        json!([
            "packages/lib/package.json (modified)",
            "-> src/a.ts resolves \"@acme/lib\" differently",
            "-> a used at load by src/a.test.ts",
        ])
    );
    assert_eq!(
        report["targets"],
        json!({ "typescript": ["src/a.test.ts", "src/b.test.ts"] })
    );
}

fn repository_with_vitest() -> TestRepository {
    TestRepository::with_files_on_main(&[
        (
            "package.json",
            r#"{ "name": "app", "devDependencies": { "vitest": "^3.2.0" } }"#,
        ),
        (
            "vitest.config.ts",
            r#"
                import { defineConfig } from "vitest/config";
                import { slowTests } from "./test/slow-tests.mjs";

                export default defineConfig({
                    test: { setupFiles: ["test/setup.ts"], exclude: [...slowTests, "**/node_modules/**"] },
                });
            "#,
        ),
        (
            "test/slow-tests.mjs",
            r#"export const slowTests = ["src/a.test.ts"];"#,
        ),
        ("test/setup.ts", "globalThis.ready = true;"),
    ])
}

#[test]
fn setup_file_change_runs_every_test_with_a_root_scope_reason() {
    let repository = repository_with_vitest();

    repository.change_and_commit("test/setup.ts", "globalThis.ready = false;");

    let report = repository
        .prober(&["--base", "main", "--format", "json"])
        .json();

    assert_eq!(report["mode"], "all");
    assert_eq!(
        report["mode_reasons"],
        json!([{
            "kind": "root_scope",
            "file": "test/setup.ts",
            "message": "test/setup.ts (modified) is loaded before every test",
        }])
    );
}

#[test]
fn test_list_edit_in_a_runner_config_selects_the_tests_it_moves() {
    let repository = repository_with_vitest();

    repository.change_and_commit(
        "test/slow-tests.mjs",
        r#"export const slowTests = ["src/b.test.ts"];"#,
    );

    let report = repository
        .prober(&["--base", "main", "--format", "json"])
        .json();

    assert_eq!(
        report["targets"],
        json!({ "typescript": ["src/a.test.ts", "src/b.test.ts"] })
    );
    assert_eq!(
        report["reasons"]["src/b.test.ts"],
        json!([
            "test/slow-tests.mjs (modified)",
            "-> src/b.test.ts matches \"src/b.test.ts\", which changed",
        ])
    );
}

#[test]
fn runner_config_change_is_root_scope_with_presets_and_global_without() {
    let with_presets = repository_with_vitest();
    let without_presets = repository_with_vitest();

    without_presets.write("prober.toml", "presets = []\n");

    for repository in [&with_presets, &without_presets] {
        let config = fs::read_to_string(repository.path().join("vitest.config.ts")).unwrap();

        repository.change_and_commit(
            "vitest.config.ts",
            &config.replace("setupFiles", "isolate: false, setupFiles"),
        );
    }

    assert_eq!(
        with_presets.prober(&["--base", "main"]).stderr,
        "prober: all 2 test files — vitest.config.ts (modified) configures the test runner of every test\n"
    );
    assert_eq!(
        without_presets.prober(&["--base", "main"]).stderr,
        "prober: all 2 test files — vitest.config.ts (modified) matches always_all \"**/vitest.*\"\n"
    );
}

#[test]
fn closed_mock_keeps_its_test_out_of_body_changes_until_the_exports_change() {
    let repository = TestRepository::with_files_on_main(&[
        ("src/db.ts", "export function query() { return 1; }"),
        (
            "src/a.ts",
            r#"import { query } from "./db"; export const a = query();"#,
        ),
        (
            "src/a.test.ts",
            r#"vi.mock("./db", () => ({ query: () => 0 })); import { a } from "./a";"#,
        ),
    ]);

    repository.change_and_commit("src/db.ts", "export function query() { return 2; }");

    let body_change = repository.prober(&["--base", "main"]);

    repository.change_and_commit(
        "src/db.ts",
        "export function query() { return 2; }\nexport function close() {}",
    );

    let export_change = repository.prober(&["--base", "main"]);

    assert_eq!(body_change.stdout, "src/b.test.ts\n");
    assert_eq!(export_change.stdout, "src/a.test.ts\nsrc/b.test.ts\n");
}

#[test]
fn fixture_read_by_a_test_selects_it_when_changed_or_deleted_unless_switched_off() {
    let repository = TestRepository::with_files_on_main(&[
        (
            "src/parser.test.ts",
            r#"import { readFileSync } from "node:fs";
const input = readFileSync(new URL("./fixtures/input.json", import.meta.url), "utf8");"#,
        ),
        ("src/fixtures/input.json", r#"{ "items": [] }"#),
    ]);

    repository.change_and_commit("src/fixtures/input.json", r#"{ "items": [1] }"#);

    let changed = repository
        .prober(&["--base", "main", "--format", "json"])
        .json();

    repository.git(&["rm", "--quiet", "src/fixtures/input.json"]);
    repository.commit_all("delete the fixture");

    let deleted = repository
        .prober(&["--base", "main", "--format", "json"])
        .json();

    repository.write("prober.toml", "[inference]\nfiles = false\n");

    let switched_off = repository.prober(&["--base", "main"]);

    assert_eq!(
        changed["reasons"]["src/parser.test.ts"],
        json!([
            "src/fixtures/input.json (modified)",
            "-> read by src/parser.test.ts:2 (inferred file path ./fixtures/input.json)",
        ])
    );
    assert_eq!(
        deleted["reasons"]["src/parser.test.ts"],
        json!([
            "src/fixtures/input.json (deleted)",
            "-> read by src/parser.test.ts:2 (inferred file path ./fixtures/input.json)",
        ])
    );
    assert_eq!(switched_off.stdout, "");
}

#[test]
fn command_change_selects_the_e2e_test_spawning_the_built_cli_unless_switched_off() {
    let repository = TestRepository::with_files_on_main(&[
        (
            "package.json",
            r#"{ "name": "cli", "type": "module", "bin": { "cli": "bin/cli.mjs" } }"#,
        ),
        ("bin/cli.mjs", r#"await import("../dist/entry.js");"#),
        (
            "src/entry.ts",
            "import { addAgent } from \"./commands/agents.js\";\n\nregister(addAgent);\n",
        ),
        ("src/commands/agents.ts", "export const addAgent = 1;"),
        (
            "test/agents.e2e.test.ts",
            r#"import { execFileSync } from "node:child_process";
const output = execFileSync("npx", ["cli", "agents", "add"], { encoding: "utf8" });"#,
        ),
    ]);

    repository.change_and_commit("src/commands/agents.ts", "export const addAgent = 2;");

    let selected = repository
        .prober(&["--base", "main", "--format", "json"])
        .json();

    repository.write("prober.toml", "[inference]\nspawned_entrypoints = false\n");

    let switched_off = repository.prober(&["--base", "main"]);

    assert_eq!(
        selected["reasons"]["test/agents.e2e.test.ts"],
        json!([
            "src/commands/agents.ts (modified)",
            "-> addAgent changed",
            "-> addAgent used at load by src/entry.ts",
            "-> built into dist/entry.js (inferred build mapping dist/ <- src/)",
            "-> used whole at load by bin/cli.mjs (loaded with import() or require())",
            "-> spawned by test/agents.e2e.test.ts:2 (inferred spawn cli (bin of package.json))",
        ])
    );
    assert_eq!(switched_off.stdout, "");
}

#[test]
fn kill_switch_runs_every_test_with_a_forced_reason() {
    let repository = TestRepository::with_project_on_main();

    repository.change_and_commit("docs/guide.md", "# Guide v2");

    let forced = repository.prober_with_environment(
        &["--base", "main", "--format", "json"],
        &[("PROBER_SELECTION", "all")],
    );
    let misspelled =
        repository.prober_with_environment(&["--base", "main"], &[("PROBER_SELECTION", "al")]);

    assert_eq!(forced.json()["mode"], "all");
    assert_eq!(
        forced.json()["mode_reasons"],
        json!([{ "kind": "forced", "message": "forced by PROBER_SELECTION=all" }])
    );
    assert_eq!(
        forced.stderr,
        "prober: all 2 test files — forced by PROBER_SELECTION=all\n"
    );
    assert_eq!(misspelled.stdout, "src/a.test.ts\nsrc/b.test.ts\n");
    assert_eq!(
        misspelled.stderr,
        "prober: all 2 test files — PROBER_SELECTION must be \"all\" or unset, not \"al\"\n"
    );
}

#[test]
fn changes_in_languages_without_a_frontend_are_listed_as_unhandled() {
    let repository = TestRepository::with_project_on_main();

    repository.change_and_commit("apps/ios/ChatView.swift", "struct ChatView {}");

    let report = repository
        .prober(&["--base", "main", "--format", "json"])
        .json();

    assert_eq!(report["mode"], "none");
    assert_eq!(
        report["mode_reasons"],
        json!([{
            "kind": "only_unhandled",
            "message": "1 changed file is in a language Prober does not analyse",
        }])
    );
    assert_eq!(report["unhandled"], json!(["apps/ios/ChatView.swift"]));
}

fn repository_with_rules() -> TestRepository {
    let repository = TestRepository::with_files_on_main(&[
        ("fixtures/golden/expected.txt", "v1"),
        ("test/output.e2e.test.ts", "export {};"),
        ("fixtures/data.json", "{}"),
        (
            "src/data.test.ts",
            r#"import { readFileSync } from "node:fs";
const data = readFileSync(new URL("../fixtures/data.json", import.meta.url), "utf8");"#,
        ),
        (
            "prober.toml",
            r#"[[link]]
from = "fixtures/golden/**"
to = "test/output.e2e.test.ts"

[always_run]
tests = ["src/b.test.ts"]

[[ignore]]
from = "fixtures/data.json"
to = "src/data.test.ts"

[[ignore]]
from = "src/missing/**"
to = "src/a.test.ts"
"#,
        ),
    ]);

    repository.write("fixtures/golden/expected.txt", "v2");
    repository.write("fixtures/data.json", r#"{ "items": [] }"#);
    repository.commit_all("change the fixtures");

    repository
}

#[test]
fn config_links_ignores_and_smoke_tests_shape_the_selection() {
    let repository = repository_with_rules();
    let run = repository.prober(&["--base", "main", "--format", "json"]);
    let report = run.json();

    assert_eq!(
        report["reasons"],
        json!({
            "test/output.e2e.test.ts": [
                "fixtures/golden/expected.txt (modified)",
                "-> linked to test/output.e2e.test.ts by rule link (prober.toml:1): fixtures/golden/**",
            ],
            "src/b.test.ts": ["always run by prober.toml:5 (\"src/b.test.ts\")"],
        })
    );
    assert_eq!(
        run.stderr,
        "prober: warning: prober.toml:12: from \"src/missing/**\" matches no file\nprober: selected 2/4 test files (50.0%), 1 always run\n"
    );
}

#[test]
fn why_explains_a_selected_test_and_the_rule_leaving_another_out() {
    let repository = repository_with_rules();
    let why = |test: &str| repository.prober(&["why", test, "--base", "main"]);
    let not_a_test = why("src/a.ts");

    assert_eq!(
        why("test/output.e2e.test.ts").stdout,
        "test/output.e2e.test.ts is selected:\n  fixtures/golden/expected.txt (modified)\n  -> linked to test/output.e2e.test.ts by rule link (prober.toml:1): fixtures/golden/**\n"
    );
    assert_eq!(
        why("./src/b.test.ts").stdout,
        "src/b.test.ts is selected: always run by prober.toml:5 (\"src/b.test.ts\")\n"
    );
    assert_eq!(
        why("src/data.test.ts").stdout,
        "src/data.test.ts is not selected: every path goes through what prober.toml:8 ignores:\n  fixtures/data.json (modified)\n  -> read by src/data.test.ts:2 (inferred file path ../fixtures/data.json), ignored by prober.toml:8\n"
    );
    assert_eq!(
        why("src/a.test.ts").stdout,
        "src/a.test.ts is not selected: no path links it to the 2 changed files\n"
    );
    assert_eq!(not_a_test.exit_code, 1);
    assert!(
        not_a_test
            .stderr
            .ends_with("prober why: src/a.ts is not a test file\n"),
        "{}",
        not_a_test.stderr
    );
}

#[test]
fn scan_suggests_declaring_what_a_loader_loads_and_setting_its_site_aside() {
    let mut files = vec![
        (
            "src/loader.ts".to_owned(),
            "export const plugin = await import(process.env.PLUGIN_ENTRY!);\n".to_owned(),
        ),
        (
            "extensions/telegram/index.ts".to_owned(),
            "export const id = 1;".to_owned(),
        ),
    ];

    for index in 0..10 {
        files.push((
            format!("src/loader-{index}.test.ts"),
            r#"import { plugin } from "./loader";"#.to_owned(),
        ));
    }

    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let repository = TestRepository::with_files_on_main(&files);

    repository.change_and_commit("src/a.ts", "export const a = 2;");

    let scan = repository.prober(&["scan"]);
    let widened = repository.prober(&["--base", "main"]);

    repository.write(
        "prober.toml",
        "[[link]]\nfrom = \"extensions/*/index.ts\"\nto = \"src/loader.ts\"\n\n[[ignore]]\nsite = \"src/loader.ts\"\n",
    );

    let linked = repository.prober(&["--base", "main"]);
    let linked_scan = repository.prober(&["scan", "--format", "json"]).json();

    assert!(
        scan.stdout.contains(
            "Suggestions: 1\n  # src/loader.ts:1 widens to 10 test files: process.env.PLUGIN_ENTRY! is computed at runtime; declare what src/loader.ts loads, then set its unresolved sites aside\n  [[link]]\n  from = \"path/to/what/it/loads/**\"\n  to = \"src/loader.ts\"\n\n  [[ignore]]\n  site = \"src/loader.ts\"\n"
        ),
        "{}",
        scan.stdout
    );
    assert_eq!(
        widened.stderr,
        "prober: selected 12/12 test files (100.0%), 10 added by widening\n"
    );
    assert_eq!(linked.stdout, "src/a.test.ts\nsrc/b.test.ts\n");
    assert_eq!(linked_scan["unresolved"], json!([]));
    assert_eq!(
        linked_scan["config"]["rules"],
        json!([
            {
                "line": 1,
                "rule": "[[link]] from \"extensions/*/index.ts\" to \"src/loader.ts\"",
                "links": 1,
                "sites": 0,
                "unlinked": [],
            },
            {
                "line": 5,
                "rule": "[[ignore]] site \"src/loader.ts\"",
                "links": 0,
                "sites": 1,
                "unlinked": [],
            },
        ])
    );
}

#[test]
fn manifests_switched_off_keep_package_json_global() {
    let repository = TestRepository::with_project_on_main();

    repository.write("prober.toml", "[inference]\nmanifests = false\n");
    repository.change_and_commit("package.json", r#"{ "name": "app", "private": true }"#);

    let run = repository.prober(&["--base", "main"]);

    assert_eq!(
        run.stderr,
        "prober: all 2 test files — package.json (modified) matches always_all \"**/package.json\"\n"
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

    repository.change_and_commit("prober.toml", "anchors = true\n");

    let report = repository
        .prober(&["--base", "main", "--format", "json"])
        .json();

    assert_eq!(report["mode"], "all");
    assert_eq!(
        report["mode_reasons"],
        json!([{
            "kind": "error",
            "message": "invalid prober.toml at line 1, column 1: unknown field `anchors`, expected one of `presets`, `always_all`, `always_run`, `link`, `ignore`, `inference`",
        }])
    );
    assert_eq!(report["stats"]["tests_selected"], 2);
}

#[test]
fn on_error_fail_exits_with_the_error() {
    let repository = TestRepository::with_project_on_main();

    repository.change_and_commit("prober.toml", "anchors = true\n");

    let run = repository.prober(&["--base", "main", "--on-error", "fail"]);

    assert_eq!(run.exit_code, 1);
    assert_eq!(run.stdout, "");
    assert_eq!(
        run.stderr,
        "prober: invalid prober.toml at line 1, column 1: unknown field `anchors`, expected one of `presets`, `always_all`, `always_run`, `link`, `ignore`, `inference`\n"
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
    let run = run_prober(directory.path(), &["--base", "main"], None, &[]);

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
fn scan_reports_inferred_links_unresolved_sites_and_unreachable_files() {
    let repository = TestRepository::with_project_on_main();

    repository.write(
        "src/loader.ts",
        "const name = process.env.PLUGIN;\nexport const plugin = await import(`./plugins/${name}.js`);\n",
    );
    repository.write("src/plugins/telegram.ts", "export const id = 1;");
    repository.write(
        "src/worker.ts",
        "onMessage((request: { moduleUrl: string }) => import(request.moduleUrl));\nexport const ready = true;\n",
    );
    repository.write("src/worker.test.ts", r#"import { ready } from "./worker";"#);
    repository.commit_all("loaders");

    let run = repository.prober(&["scan", "--format", "json"]);
    let scan = run.json();

    assert_eq!(run.exit_code, 0);
    assert_eq!(
        scan["inferred"],
        json!([{
            "site": "src/loader.ts:2",
            "patterns": [{ "kind": "glob", "pattern": "src/plugins/*.js" }],
            "links": 1,
        }])
    );
    assert_eq!(
        scan["unresolved"],
        json!([{
            "site": "src/worker.ts:1",
            "expression": "request.moduleUrl",
            "cause": "parameter",
            "tests": 1,
        }])
    );
    assert_eq!(
        scan["unreachable"],
        json!({
            "modules": ["package.json", "src/loader.ts", "src/plugins/telegram.ts"],
            "other_files": ["docs/guide.md"],
        })
    );
    assert_eq!(
        run.stderr,
        "prober scan: 1 links inferred from 1 sites; 1 unresolved sites reach 1/3 test files (33.3%); 0 sites load missing files; 4 files reach no test; 0 import cycles; 0 suggestions\n"
    );

    let text_run = repository.prober(&["scan"]);

    assert!(
        text_run
            .stdout
            .starts_with("Inferred links: 1 from 1 sites\n"),
        "{}",
        text_run.stdout
    );
}

#[test]
fn scan_reports_import_cycles_and_what_is_left_of_them_at_symbol_level() {
    let repository = TestRepository::with_project_on_main();

    repository.write(
        "src/x.ts",
        "import { y } from \"./y\";\nexport function x() { return y(); }\nexport function helper() { return 1; }\n",
    );
    repository.write(
        "src/y.ts",
        "import { helper } from \"./x\";\nexport function y() { return 2; }\nexport function viaX() { return helper(); }\n",
    );
    repository.write("src/x.test.ts", "import { x } from \"./x\";\nx();\n");
    repository.commit_all("cycle");

    let run = repository.prober(&["scan", "--format", "json"]);
    let scan = run.json();

    assert_eq!(run.exit_code, 0);
    assert_eq!(
        scan["cycles"],
        json!([{
            "modules": ["src/x.ts", "src/y.ts"],
            "tests": 1,
            "symbols": {
                "symbols": 7,
                "cycles": [{ "symbols": 2, "modules": 2, "tests": 1, "leading_in": 2 }],
                "file_level": [],
            },
        }])
    );
    assert_eq!(scan["stats"]["import_cycles"], 1);
    assert_eq!(scan["stats"]["modules_in_import_cycles"], 2);
    assert_eq!(scan["symbols"]["members"], 0);
    assert_eq!(scan["symbols"]["calls"], 1);

    let text_run = repository.prober(&["scan"]);

    assert!(
        text_run.stdout.contains(
            "Import cycles: 1 with 2 or more modules, 2 modules in them
  modules  tests  directories
        2      1  src 2
                  symbol level: 7 symbols, 2 symbols in 2 modules still form a cycle pulling in 1 tests, 2 symbols lead into it (28.6%)
"
        ),
        "{}",
        text_run.stdout
    );
}

#[test]
fn scan_reports_objects_read_by_key_and_commands_without_narrowing() {
    let repository = TestRepository::with_project_on_main();

    repository.write(
        "src/handlers.ts",
        "export const handlers = { list: () => 1, add: () => 2 };\n",
    );
    repository.write(
        "src/dispatch.ts",
        "import { handlers } from \"./handlers\";\nexport const dispatch = (method: \"list\" | \"add\") => handlers[method]();\n",
    );
    repository.write(
        "src/cli.ts",
        "import type { Command } from \"commander\";\nexport function register(program: Command) {\n  program.command(\"agents\").command(\"add\").action(() => 1);\n}\n",
    );
    repository.commit_all("anchors");

    let run = repository.prober(&["scan", "--format", "json"]);

    assert_eq!(run.exit_code, 0);
    assert_eq!(
        run.json()["anchors"],
        json!({
            "registries": [{
                "registry": "handlers",
                "file": "src/handlers.ts",
                "keys": 2,
                "readers": ["dispatch in src/dispatch.ts"],
            }],
            "commands": [{ "file": "src/cli.ts", "commands": ["agents", "agents add"] }],
        })
    );

    let text_run = repository.prober(&["scan"]);

    assert!(
        text_run.stdout.contains(
            "String anchors: 1 objects read by a runtime key (2 keys), 2 commands in 1 files; reported, not used to narrow: the keys a test dispatches and the commands it launches can come from helpers, clients and data Prober cannot read
  keys  object
     2  handlers in src/handlers.ts, read by key by dispatch in src/dispatch.ts
  commands  file
         2  src/cli.ts
"
        ),
        "{}",
        text_run.stdout
    );
}

#[test]
fn scan_outside_a_repository_fails() {
    let directory = TempDir::new().unwrap();
    let run = run_prober(directory.path(), &["scan"], None, &[]);

    assert_eq!(run.exit_code, 1);
    assert_eq!(run.stdout, "");
    assert!(
        run.stderr
            .starts_with("prober scan: no git repository found at "),
        "{}",
        run.stderr
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
        &[],
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
