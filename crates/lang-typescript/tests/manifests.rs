//! Manifest, lockfile and tsconfig changes on projects written to disk, against their base revision contents.

mod test_project;

use std::collections::HashMap;

use anyhow::Result;
use prober_core::changes::{BaseRevision, ChangeKind, FileChange};
use prober_core::files::RepoPath;
use prober_core::graph::GraphBuilder;
use prober_core::report::{ModeReason, Report};
use prober_core::select::{Widening, select_tests};
use prober_lang_typescript::{ManifestChanges, TypeScriptLinker};

use crate::test_project::{TestProject, change, repo_path};

struct BaseFiles(HashMap<RepoPath, Vec<u8>>);

impl BaseRevision for BaseFiles {
    fn read_file(&self, path: &RepoPath) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get(path).cloned())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    All(Vec<String>),
    Selected(Vec<(String, Vec<String>)>),
}

fn outcome_of(
    project: &TestProject,
    base_files: &[(&str, &str)],
    changes: &[FileChange],
) -> Outcome {
    let repo_files = project.repo_files();
    let base_files = BaseFiles(
        base_files
            .iter()
            .map(|&(path, content)| (repo_path(path), content.as_bytes().to_vec()))
            .collect(),
    );
    let manifest_changes =
        ManifestChanges::between(project.root(), &repo_files, changes, &base_files).unwrap();
    let global_reasons = manifest_changes.global_reasons();

    if !global_reasons.is_empty() {
        return Outcome::All(global_reasons.iter().map(ModeReason::message).collect());
    }

    let linker = TypeScriptLinker::new(project.root(), &repo_files).unwrap();
    let mut builder = GraphBuilder::default();

    linker
        .link_changes(&repo_files, &manifest_changes, &mut builder)
        .unwrap();

    let graph = builder.build();
    let root_scope_reasons = linker
        .root_scope_reasons(&graph, &repo_files, &manifest_changes)
        .unwrap();

    if !root_scope_reasons.is_empty() {
        return Outcome::All(root_scope_reasons.iter().map(ModeReason::message).collect());
    }

    let selected_tests = select_tests(&graph, changes, Widening::Scoped);
    let report = Report::from_selection(&graph, changes, &selected_tests, String::new());

    Outcome::Selected(
        report
            .targets
            .into_iter()
            .map(|target| (target.path.to_string(), target.impact.unwrap().reason))
            .collect(),
    )
}

fn modified(path: &str) -> FileChange {
    change(path, ChangeKind::Modified)
}

fn selected(tests: &[(&str, &[&str])]) -> Outcome {
    Outcome::Selected(
        tests
            .iter()
            .map(|(test, reason)| {
                let reason = reason.iter().map(|&hop| hop.to_owned()).collect();

                ((*test).to_owned(), reason)
            })
            .collect(),
    )
}

fn all(messages: &[&str]) -> Outcome {
    Outcome::All(messages.iter().map(|&message| message.to_owned()).collect())
}

const PNPM_LOCK: &str = "lockfileVersion: '9.0'

packages:
  debug@4.3.4:
    resolution: {integrity: sha512-debug}
  express@4.19.0:
    resolution: {integrity: sha512-express}
  lodash@4.17.21:
    resolution: {integrity: sha512-lodash}

snapshots:
  debug@4.3.4: {}
  express@4.19.0:
    dependencies:
      debug: 4.3.4
  lodash@4.17.21: {}
";

fn server_project(package_json: &str, pnpm_lock: &str) -> TestProject {
    TestProject::new(&[
        ("package.json", package_json),
        ("pnpm-lock.yaml", pnpm_lock),
        ("src/server.ts", r#"import express from "express";"#),
        ("src/server.test.ts", r#"import "./server";"#),
        ("src/log.test.ts", r#"import debug from "debug";"#),
        (
            "src/types.test.ts",
            r#"import type { Request } from "express";"#,
        ),
        (
            "src/version.ts",
            r#"import manifest from "../package.json";"#,
        ),
        ("src/version.test.ts", r#"import "./version";"#),
        ("src/util.test.ts", r#"import { chunk } from "lodash";"#),
    ])
}

#[test]
fn metadata_change_selects_only_the_tests_that_import_the_manifest() {
    let project = server_project(
        r#"{ "version": "1.1.0", "scripts": { "build": "tsc -b" } }"#,
        PNPM_LOCK,
    );
    let base_manifest = r#"{ "version": "1.0.0", "scripts": { "build": "tsc" } }"#;

    assert_eq!(
        outcome_of(
            &project,
            &[("package.json", base_manifest)],
            &[modified("package.json")]
        ),
        selected(&[(
            "src/version.test.ts",
            &[
                "package.json (modified)",
                "-> imported by src/version.ts",
                "-> imported by src/version.test.ts",
            ],
        )])
    );
}

#[test]
fn lockfile_bump_selects_the_importers_of_the_package_and_of_its_dependents() {
    let head_lock = PNPM_LOCK
        .replace("debug@4.3.4", "debug@4.3.5")
        .replace("debug: 4.3.4", "debug: 4.3.5");
    let project = server_project("{}", &head_lock);

    assert_eq!(
        outcome_of(
            &project,
            &[("pnpm-lock.yaml", PNPM_LOCK)],
            &[modified("pnpm-lock.yaml")]
        ),
        selected(&[
            (
                "src/log.test.ts",
                &[
                    "pnpm-lock.yaml (modified)",
                    "-> package debug changed (4.3.4 -> 4.3.5)",
                    "-> imported by src/log.test.ts",
                ],
            ),
            (
                "src/server.test.ts",
                &[
                    "pnpm-lock.yaml (modified)",
                    "-> package express changed (4.19.0, dependencies changed)",
                    "-> imported by src/server.ts",
                    "-> imported by src/server.test.ts",
                ],
            ),
        ])
    );
}

#[test]
fn dependency_bump_without_a_lockfile_change_selects_the_package_its_dependents_and_manifest_readers()
 {
    let project = server_project(r#"{ "dependencies": { "debug": "^4.3.5" } }"#, PNPM_LOCK);
    let base_manifest = r#"{ "dependencies": { "debug": "^4.3.4" } }"#;

    assert_eq!(
        outcome_of(
            &project,
            &[("package.json", base_manifest)],
            &[modified("package.json")]
        ),
        selected(&[
            (
                "src/log.test.ts",
                &[
                    "package.json (modified)",
                    "-> package debug changed (^4.3.4 -> ^4.3.5)",
                    "-> imported by src/log.test.ts",
                ],
            ),
            (
                "src/version.test.ts",
                &[
                    "package.json (modified)",
                    "-> imported by src/version.ts",
                    "-> imported by src/version.test.ts",
                ],
            ),
            (
                "src/server.test.ts",
                &[
                    "package.json (modified)",
                    "-> package express depends on debug, which changed (^4.3.4 -> ^4.3.5)",
                    "-> imported by src/server.ts",
                    "-> imported by src/server.test.ts",
                ],
            ),
        ])
    );
}

#[test]
fn patch_change_selects_the_importers_of_the_patched_package() {
    let project = server_project("{}", PNPM_LOCK);

    project.write(
        "patches/lodash@4.17.21.patch",
        "--- a/index.js\n+++ b/index.js\n",
    );

    assert_eq!(
        outcome_of(
            &project,
            &[],
            &[change("patches/lodash@4.17.21.patch", ChangeKind::Added)]
        ),
        selected(&[(
            "src/util.test.ts",
            &[
                "patches/lodash@4.17.21.patch (added)",
                "-> package lodash changed (patched)",
                "-> imported by src/util.test.ts",
            ],
        )])
    );
}

fn workspace_project(lib_manifest: &str) -> TestProject {
    TestProject::new(&[
        ("package.json", r#"{ "name": "acme", "private": true }"#),
        ("pnpm-workspace.yaml", "packages:\n  - packages/*\n"),
        ("packages/lib/package.json", lib_manifest),
        ("packages/lib/src/index.ts", "export const lib = 1;"),
        ("packages/lib/src/utils.ts", "export const utils = 1;"),
        ("packages/lib/src/utils-v2.ts", "export const utils = 2;"),
        (
            "packages/lib/src/index.test.ts",
            r#"import { lib } from "./index";"#,
        ),
        ("packages/web/package.json", r#"{ "name": "@acme/web" }"#),
        (
            "packages/web/src/a.ts",
            r#"import { utils } from "@acme/lib/utils";"#,
        ),
        ("packages/web/src/a.test.ts", r#"import "./a";"#),
        (
            "packages/web/src/b.ts",
            r#"import { lib } from "@acme/lib";"#,
        ),
        ("packages/web/src/b.test.ts", r#"import "./b";"#),
    ])
}

const LIB_MANIFEST: &str = r#"{
    "name": "@acme/lib",
    "exports": { ".": "./src/index.ts", "./utils": "./src/utils.ts" }
}"#;

#[test]
fn exports_change_selects_the_files_whose_imports_resolve_differently() {
    let project = workspace_project(&LIB_MANIFEST.replace("./src/utils.ts", "./src/utils-v2.ts"));

    assert_eq!(
        outcome_of(
            &project,
            &[("packages/lib/package.json", LIB_MANIFEST)],
            &[modified("packages/lib/package.json")]
        ),
        selected(&[(
            "packages/web/src/a.test.ts",
            &[
                "packages/lib/package.json (modified)",
                "-> packages/web/src/a.ts resolves \"@acme/lib/utils\" differently",
                "-> imported by packages/web/src/a.test.ts",
            ],
        )])
    );
}

#[test]
fn type_change_selects_the_tests_of_every_module_of_the_package() {
    let project =
        workspace_project(&LIB_MANIFEST.replace("\"name\"", "\"type\": \"module\", \"name\""));
    let outcome = outcome_of(
        &project,
        &[("packages/lib/package.json", LIB_MANIFEST)],
        &[modified("packages/lib/package.json")],
    );
    let Outcome::Selected(selected_tests) = outcome else {
        panic!("expected a selection, got {outcome:?}");
    };
    let selected_paths: Vec<&str> = selected_tests
        .iter()
        .map(|(test, _)| test.as_str())
        .collect();

    assert_eq!(
        selected_paths,
        [
            "packages/lib/src/index.test.ts",
            "packages/web/src/a.test.ts",
            "packages/web/src/b.test.ts",
        ]
    );
    assert_eq!(
        selected_tests[0].1,
        [
            "packages/lib/package.json (modified)",
            "-> packages/lib/src/index.test.ts is affected by field \"type\"",
        ]
    );
}

#[test]
fn renamed_workspace_package_changes_the_resolution_of_its_importers() {
    let project = workspace_project(&LIB_MANIFEST.replace("@acme/lib", "@acme/core"));

    let outcome = outcome_of(
        &project,
        &[("packages/lib/package.json", LIB_MANIFEST)],
        &[modified("packages/lib/package.json")],
    );
    let Outcome::Selected(selected_tests) = outcome else {
        panic!("expected a selection, got {outcome:?}");
    };
    let selected_paths: Vec<&str> = selected_tests
        .iter()
        .map(|(test, _)| test.as_str())
        .collect();

    assert_eq!(
        selected_paths,
        ["packages/web/src/a.test.ts", "packages/web/src/b.test.ts"]
    );
}

#[test]
fn tsconfig_paths_change_selects_the_files_whose_aliases_resolve_differently() {
    let base_tsconfig =
        r#"{ "compilerOptions": { "strict": false, "paths": { "@app/*": ["./src/*"] } } }"#;
    let project = TestProject::new(&[
        (
            "tsconfig.json",
            r#"{ "compilerOptions": { "strict": true, "paths": { "@app/*": ["./lib/*"] } } }"#,
        ),
        ("src/util.ts", "export const util = 1;"),
        ("lib/util.ts", "export const util = 2;"),
        ("src/main.ts", r#"import { util } from "@app/util";"#),
        ("src/main.test.ts", r#"import "./main";"#),
        ("src/other.test.ts", r#"import "./util";"#),
    ]);

    assert_eq!(
        outcome_of(
            &project,
            &[("tsconfig.json", base_tsconfig)],
            &[modified("tsconfig.json")]
        ),
        selected(&[(
            "src/main.test.ts",
            &[
                "tsconfig.json (modified)",
                "-> src/main.ts resolves \"@app/util\" differently",
                "-> imported by src/main.test.ts",
            ],
        )])
    );
}

#[test]
fn runner_packages_and_the_packages_of_runner_configs_and_setup_files_run_every_test() {
    let base_lock = "lockfileVersion: '9.0'
packages:
  '@vitejs/plugin-vue@5.0.0': {resolution: {integrity: a}}
  fake-indexeddb@6.0.0: {resolution: {integrity: b}}
  lodash@4.17.21: {resolution: {integrity: c}}
  vitest@3.2.0: {resolution: {integrity: d}}
";
    let project = TestProject::new(&[
        ("pnpm-lock.yaml", base_lock),
        (
            "vitest.config.ts",
            r#"
                import vue from "@vitejs/plugin-vue";

                export default { plugins: [vue()], test: { setupFiles: ["test/setup.ts"] } };
            "#,
        ),
        ("test/setup.ts", r#"import "fake-indexeddb/auto";"#),
        ("src/a.test.ts", r#"import { chunk } from "lodash";"#),
    ]);
    let bump = |package: &str, from: &str, to: &str| {
        let head_lock = base_lock.replace(&format!("{package}@{from}"), &format!("{package}@{to}"));

        project.write("pnpm-lock.yaml", &head_lock);

        outcome_of(
            &project,
            &[("pnpm-lock.yaml", base_lock)],
            &[modified("pnpm-lock.yaml")],
        )
    };

    assert_eq!(
        bump("vitest", "3.2.0", "3.2.1"),
        all(&[
            "pnpm-lock.yaml (modified) changes package vitest (3.2.0 -> 3.2.1), which every test loads"
        ])
    );
    assert_eq!(
        bump("'@vitejs/plugin-vue", "5.0.0", "5.1.0"),
        all(&[
            "pnpm-lock.yaml (modified) changes package @vitejs/plugin-vue (5.0.0 -> 5.1.0), which every test loads"
        ])
    );
    assert_eq!(
        bump("fake-indexeddb", "6.0.0", "6.1.0"),
        all(&[
            "pnpm-lock.yaml (modified) changes package fake-indexeddb (6.0.0 -> 6.1.0), which every test loads"
        ])
    );
    assert_eq!(
        bump("lodash", "4.17.21", "4.17.22"),
        selected(&[(
            "src/a.test.ts",
            &[
                "pnpm-lock.yaml (modified)",
                "-> package lodash changed (4.17.21 -> 4.17.22)",
                "-> imported by src/a.test.ts",
            ],
        )])
    );
}

#[test]
fn unknown_fields_global_compiler_options_and_unparsable_files_run_every_test() {
    let project = TestProject::new(&[
        ("package.json", r#"{ "browserslist": ["defaults"] }"#),
        (
            "tsconfig.json",
            r#"{ "compilerOptions": { "target": "es2022" } }"#,
        ),
        ("pnpm-lock.yaml", "lockfileVersion: 5.4\n"),
        ("src/a.test.ts", ""),
    ]);
    let changes = [
        modified("package.json"),
        modified("pnpm-lock.yaml"),
        modified("tsconfig.json"),
    ];
    let base_files = [
        ("package.json", "{}"),
        (
            "tsconfig.json",
            r#"{ "compilerOptions": { "target": "es2020" } }"#,
        ),
        ("pnpm-lock.yaml", "lockfileVersion: 5.4\n"),
    ];

    assert_eq!(
        outcome_of(&project, &base_files, &changes),
        all(&[
            "package.json (modified) changes \"browserslist\", which may affect every test",
            "pnpm-lock.yaml (modified) cannot be diffed: lockfileVersion 5.4 is not supported",
            "tsconfig.json (modified) changes \"compilerOptions.target\", which may affect every test",
        ])
    );
}

#[test]
fn added_and_deleted_manifests_are_diffed_against_an_empty_one() {
    let project = server_project("{}", PNPM_LOCK);

    project.write(
        "packages/new/package.json",
        r#"{ "name": "@acme/new", "version": "0.1.0" }"#,
    );

    assert_eq!(
        outcome_of(
            &project,
            &[("packages/old/package.json", r#"{ "name": "@acme/old" }"#)],
            &[
                change("packages/new/package.json", ChangeKind::Added),
                change("packages/old/package.json", ChangeKind::Deleted),
            ]
        ),
        selected(&[])
    );
}

#[test]
fn workspace_dependencies_and_template_manifests_change_no_installed_package() {
    let project = workspace_project(LIB_MANIFEST);
    let template_manifest = r#"{ "devDependencies": { "vite": "^8.2.0" } }"#;

    project.write(
        "packages/web/package.json",
        r#"{ "name": "@acme/web", "dependencies": { "@acme/lib": "workspace:^" } }"#,
    );
    project.write("packages/web/template/package.json", template_manifest);

    assert_eq!(
        outcome_of(
            &project,
            &[
                (
                    "packages/web/package.json",
                    r#"{ "name": "@acme/web", "dependencies": { "@acme/lib": "workspace:*" } }"#,
                ),
                (
                    "packages/web/template/package.json",
                    r#"{ "devDependencies": { "vite": "^8.1.4" } }"#,
                ),
            ],
            &[
                modified("packages/web/package.json"),
                modified("packages/web/template/package.json"),
            ]
        ),
        selected(&[])
    );
}

#[test]
fn changed_lockfile_overrides_manifest_specifiers_and_follows_exact_versions() {
    let base_lock = "lockfileVersion: '9.0'
packages:
  eslint-plugin@1.0.0: {resolution: {integrity: a}}
  tinyexec@0.3.2: {resolution: {integrity: b}}
  vitest@3.2.0: {resolution: {integrity: c}}
snapshots:
  eslint-plugin@1.0.0:
    dependencies:
      tinyexec: 0.3.2
  tinyexec@0.3.2: {}
  vitest@3.2.0:
    dependencies:
      tinyexec: 0.3.2
";
    let head_lock = base_lock
        .replace("eslint-plugin@1.0.0", "eslint-plugin@1.1.0")
        .replace(
            "  tinyexec@0.3.2: {resolution: {integrity: b}}",
            "  tinyexec@0.3.2: {resolution: {integrity: b}}\n  tinyexec@1.0.1: {resolution: {integrity: d}}",
        )
        .replace(
            "  eslint-plugin@1.1.0:\n    dependencies:\n      tinyexec: 0.3.2",
            "  eslint-plugin@1.1.0:\n    dependencies:\n      tinyexec: 1.0.1\n  tinyexec@1.0.1: {}",
        );
    let project = TestProject::new(&[
        (
            "package.json",
            r#"{ "devDependencies": { "eslint-plugin": "^1.1.0", "vitest": "^3.2.0" } }"#,
        ),
        ("pnpm-lock.yaml", &head_lock),
        ("src/exec.test.ts", r#"import { x } from "tinyexec";"#),
        ("src/other.test.ts", ""),
    ]);

    assert_eq!(
        outcome_of(
            &project,
            &[
                (
                    "package.json",
                    r#"{ "devDependencies": { "eslint-plugin": "^1.0.0", "vitest": "^3.2.0" } }"#,
                ),
                ("pnpm-lock.yaml", base_lock),
            ],
            &[modified("package.json"), modified("pnpm-lock.yaml")]
        ),
        selected(&[(
            "src/exec.test.ts",
            &[
                "pnpm-lock.yaml (modified)",
                "-> package tinyexec changed (added 1.0.1)",
                "-> imported by src/exec.test.ts",
            ],
        )])
    );
}
