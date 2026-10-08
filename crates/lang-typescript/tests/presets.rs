//! Runner presets on projects written to disk: test discovery, root scope, config literal changes, mock contracts, assets, manual mocks, snapshots and glob imports.

mod test_project;

use prober_core::changes::ChangeKind;
use prober_core::graph::EdgeKind;

use crate::test_project::{TestProject, all, change, dependents, modified, selected};

fn test_paths(project: &TestProject) -> Vec<String> {
    let graph = project.link();

    graph
        .test_files_by_path()
        .iter()
        .map(|test_file| graph.path(test_file.file).as_str().to_owned())
        .collect()
}

#[test]
fn runner_configs_add_the_tests_their_patterns_include() {
    let project = TestProject::new(&[
        (
            "package.json",
            r#"{ "devDependencies": { "vitest": "^3", "jest": "^29", "cypress": "^13", "mocha": "^10" } }"#,
        ),
        (
            "vitest.config.ts",
            r#"
                import { defineConfig, configDefaults } from "vitest/config";

                export default defineConfig({
                    test: {
                        include: ["tests/**/*.ts", "src/**/*.test.ts"],
                        exclude: [...configDefaults.exclude, "tests/helpers/**"],
                    },
                });
            "#,
        ),
        ("tests/smoke.ts", ""),
        ("tests/helpers/server.ts", ""),
        ("src/a.test.ts", ""),
        ("src/a.ts", ""),
        (
            "packages/legacy/jest.config.js",
            "module.exports = { testPathIgnorePatterns: ['/node_modules/', '/fixtures/'] };",
        ),
        ("packages/legacy/__tests__/render.js", ""),
        ("packages/legacy/__tests__/fixtures/input.js", ""),
        ("cypress/e2e/login.cy.ts", ""),
        ("test/mocha-suite.js", ""),
        ("test/nested/not-run.js", ""),
    ]);

    assert_eq!(
        test_paths(&project),
        [
            "cypress/e2e/login.cy.ts",
            "packages/legacy/__tests__/render.js",
            "src/a.test.ts",
            "test/mocha-suite.js",
            "tests/smoke.ts",
        ]
    );
}

#[test]
fn files_importing_node_test_are_tests_when_scripts_run_node_test() {
    let files = [
        ("test/api.mjs", r#"import test from "node:test";"#),
        ("test/helpers.mjs", r#"export const helper = 1;"#),
    ];
    let with_script = TestProject::new(&files);
    let without_script = TestProject::new(&files);

    with_script.write(
        "package.json",
        r#"{ "scripts": { "test": "node --test test/" } }"#,
    );

    assert_eq!(test_paths(&with_script), ["test/api.mjs"]);
    assert_eq!(test_paths(&without_script), Vec::<String>::new());
}

fn vitest_project() -> TestProject {
    TestProject::new(&[
        (
            "package.json",
            r#"{ "devDependencies": { "vitest": "^3" } }"#,
        ),
        (
            "vitest.config.ts",
            r#"
                import { defineConfig } from "vitest/config";
                import { unitPaths } from "./test/unit-paths.mjs";

                export default defineConfig({
                    test: { setupFiles: ["./test/setup.ts"], include: [...unitPaths, "src/**/*.test.ts"] },
                });
            "#,
        ),
        (
            "test/unit-paths.mjs",
            r#"export const unitPaths = ["src/fast/a.test.ts", "src/fast/b.test.ts"];"#,
        ),
        (
            "test/setup.ts",
            r#"
                import { installEnv } from "./env";
                import { warn } from "../src/log";
            "#,
        ),
        ("test/env.ts", "export const installEnv = () => {};"),
        ("src/log.ts", "export const warn = () => {};"),
        ("src/log.test.ts", r#"import { warn } from "./log";"#),
        ("src/fast/a.test.ts", ""),
        ("src/fast/b.test.ts", ""),
        ("src/fast/c.test.ts", ""),
    ])
}

#[test]
fn setup_files_and_what_only_they_reach_run_every_test() {
    let project = vitest_project();

    for setup_file in ["test/setup.ts", "test/env.ts"] {
        assert_eq!(
            project.outcome_of(&[], &[modified(setup_file)]),
            all(&[&format!(
                "{setup_file} (modified) is loaded before every test"
            )])
        );
    }
}

#[test]
fn shared_module_imported_by_a_setup_file_selects_only_its_importers() {
    let project = vitest_project();

    assert_eq!(
        project.outcome_of(&[], &[modified("src/log.ts")]),
        selected(&[(
            "src/log.test.ts",
            &["src/log.ts (modified)", "-> imported by src/log.test.ts"],
        )])
    );
}

#[test]
fn path_list_edit_selects_only_the_tests_it_adds_or_removes() {
    let project = vitest_project();
    let base_list = r#"export const unitPaths = ["src/fast/a.test.ts", "src/fast/b.test.ts"];"#;

    project.write(
        "test/unit-paths.mjs",
        r#"export const unitPaths = ["src/fast/b.test.ts", "src/fast/c.test.ts"];"#,
    );

    assert_eq!(
        project.outcome_of(
            &[("test/unit-paths.mjs", base_list)],
            &[modified("test/unit-paths.mjs")]
        ),
        selected(&[
            (
                "src/fast/a.test.ts",
                &[
                    "test/unit-paths.mjs (modified)",
                    "-> src/fast/a.test.ts matches \"src/fast/a.test.ts\", which changed",
                ],
            ),
            (
                "src/fast/c.test.ts",
                &[
                    "test/unit-paths.mjs (modified)",
                    "-> src/fast/c.test.ts matches \"src/fast/c.test.ts\", which changed",
                ],
            ),
        ])
    );
}

#[test]
fn config_code_changes_and_new_setup_files_run_every_test() {
    let project = vitest_project();
    let base_config = std::fs::read_to_string(project.root().join("vitest.config.ts")).unwrap();
    let edits = [
        base_config.replace("setupFiles", "isolate: false, setupFiles"),
        base_config.replace("./test/setup.ts", "./test/env.ts"),
    ];

    for edited_config in edits {
        project.write("vitest.config.ts", &edited_config);

        assert_eq!(
            project.outcome_of(
                &[("vitest.config.ts", &base_config)],
                &[modified("vitest.config.ts")]
            ),
            all(&["vitest.config.ts (modified) configures the test runner of every test"]),
            "{edited_config}"
        );
    }
}

#[test]
fn package_runner_config_scopes_its_setup_to_the_tests_of_its_package() {
    let project = TestProject::new(&[
        (
            "package.json",
            r#"{ "devDependencies": { "vitest": "^3" } }"#,
        ),
        ("vitest.config.ts", "export default { test: {} };"),
        (
            "packages/ui/vite.config.ts",
            r#"export default { test: { setupFiles: ["./test/dom-setup.ts"] } };"#,
        ),
        ("packages/ui/package.json", r#"{ "name": "@acme/ui" }"#),
        ("packages/ui/test/dom-setup.ts", ""),
        ("packages/ui/src/button.test.ts", ""),
        ("src/a.test.ts", ""),
    ]);

    assert_eq!(
        project.outcome_of(&[], &[modified("packages/ui/test/dom-setup.ts")]),
        selected(&[(
            "packages/ui/src/button.test.ts",
            &[
                "packages/ui/test/dom-setup.ts (modified)",
                "-> loaded before packages/ui/src/button.test.ts",
            ],
        )])
    );
}

#[test]
fn runner_config_outside_a_package_scopes_its_setup_to_every_test() {
    let project = TestProject::new(&[
        (
            "package.json",
            r#"{ "devDependencies": { "vitest": "^3" } }"#,
        ),
        (
            "test/vitest/vitest.e2e.config.ts",
            r#"export default { test: { setupFiles: ["test/e2e-setup.ts"] } };"#,
        ),
        ("test/e2e-setup.ts", ""),
        ("src/a.e2e.test.ts", ""),
    ]);

    assert_eq!(
        project.outcome_of(&[], &[modified("test/e2e-setup.ts")]),
        all(&["test/e2e-setup.ts (modified) is loaded before every test"])
    );
}

fn mocking_project(mock: &str) -> TestProject {
    TestProject::new(&[
        ("src/db.ts", "export function query() { return 1; }"),
        (
            "src/service.ts",
            r#"import { query } from "./db"; export const run = () => query();"#,
        ),
        (
            "src/service.test.ts",
            &format!("{mock}\nimport {{ run }} from \"./service\";"),
        ),
        ("src/db.test.ts", r#"import { query } from "./db";"#),
    ])
}

const DB_BODY_CHANGE: (&str, &str) = ("src/db.ts", "export function query() { return 0; }");

#[test]
fn closed_mock_factory_keeps_the_test_out_of_body_changes() {
    let project = mocking_project(r#"vi.mock("./db", () => ({ query: vi.fn() }));"#);

    project.write("src/db.ts", "export function query() { return 2; }");

    assert_eq!(
        project
            .outcome_of(&[DB_BODY_CHANGE], &[modified("src/db.ts")])
            .selected_paths(),
        ["src/db.test.ts"]
    );
}

#[test]
fn new_export_or_partial_mock_still_selects_the_mocking_test() {
    let closed_mock = mocking_project(r#"vi.mock("./db", () => ({ query: vi.fn() }));"#);
    let partial_mock = mocking_project(
        r#"vi.mock("./db", async (importOriginal) => ({ ...(await importOriginal()) }));"#,
    );

    closed_mock.write(
        "src/db.ts",
        "export function query() { return 2; }\nexport function close() {}",
    );
    partial_mock.write("src/db.ts", "export function query() { return 2; }");

    for project in [closed_mock, partial_mock] {
        assert_eq!(
            project
                .outcome_of(&[DB_BODY_CHANGE], &[modified("src/db.ts")])
                .selected_paths(),
            ["src/db.test.ts", "src/service.test.ts"]
        );
    }
}

#[test]
fn stylesheet_change_reaches_only_browser_tests_when_projects_are_known() {
    let project = TestProject::new(&[
        (
            "package.json",
            r#"{ "devDependencies": { "vitest": "^3" } }"#,
        ),
        (
            "vitest.config.ts",
            r#"
                export default {
                    test: {
                        projects: [
                            { test: { name: "unit", include: ["src/**/*.test.ts"] } },
                            { test: { name: "browser", include: ["src/**/*.spec.ts"], browser: { enabled: true } } },
                        ],
                    },
                };
            "#,
        ),
        ("src/button.css", ".button {}"),
        ("src/button.ts", r#"import "./button.css";"#),
        ("src/button.test.ts", r#"import "./button";"#),
        ("src/button.spec.ts", r#"import "./button";"#),
    ]);

    assert_eq!(
        dependents(&project.link(), "src/button.css"),
        [("src/button.ts", EdgeKind::Asset)]
    );
    assert_eq!(
        project
            .outcome_of(&[], &[modified("src/button.css")])
            .selected_paths(),
        ["src/button.spec.ts"]
    );
    assert_eq!(
        project
            .outcome_of(&[], &[modified("src/button.ts")])
            .selected_paths(),
        ["src/button.spec.ts", "src/button.test.ts"]
    );
}

#[test]
fn snapshots_and_manual_mocks_select_the_tests_using_them() {
    let project = TestProject::new(&[
        (
            "src/render.test.ts",
            r#"vi.mock("./clock"); import "./render";"#,
        ),
        ("src/render.ts", ""),
        ("src/clock.ts", "export const now = () => 1;"),
        ("src/__mocks__/clock.ts", "export const now = () => 0;"),
        (
            "src/__snapshots__/render.test.ts.snap",
            "exports[`a`] = `1`;",
        ),
        ("src/request.ts", r#"import axios from "axios";"#),
        ("src/request.test.ts", r#"import "./request";"#),
        ("__mocks__/axios.ts", "export default { get: () => null };"),
    ]);

    assert_eq!(
        project
            .outcome_of(&[], &[modified("src/__snapshots__/render.test.ts.snap")])
            .selected_paths(),
        ["src/render.test.ts"]
    );
    assert_eq!(
        project
            .outcome_of(&[], &[modified("src/__mocks__/clock.ts")])
            .selected_paths(),
        ["src/render.test.ts"]
    );
    assert_eq!(
        project
            .outcome_of(&[], &[modified("__mocks__/axios.ts")])
            .selected_paths(),
        ["src/request.test.ts"]
    );
}

#[test]
fn glob_imports_link_the_files_their_patterns_match() {
    let project = TestProject::new(&[
        ("package.json", r#"{ "devDependencies": { "vite": "^6" } }"#),
        (
            "src/pages.ts",
            r#"export const pages = import.meta.glob(["./pages/*.ts", "!./pages/draft.ts"]);"#,
        ),
        ("src/pages.test.ts", r#"import { pages } from "./pages";"#),
        ("src/pages/home.ts", ""),
        ("src/pages/draft.ts", ""),
    ]);

    assert_eq!(
        project
            .outcome_of(&[], &[change("src/pages/about.ts", ChangeKind::Added)])
            .selected_paths(),
        Vec::<&str>::new()
    );

    project.write("src/pages/about.ts", "");

    assert_eq!(
        project
            .outcome_of(&[], &[change("src/pages/about.ts", ChangeKind::Added)])
            .selected_paths(),
        ["src/pages.test.ts"]
    );
    assert_eq!(
        project
            .outcome_of(&[], &[modified("src/pages/draft.ts")])
            .selected_paths(),
        Vec::<&str>::new()
    );
}
