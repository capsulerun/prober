//! Linking small TypeScript projects written to disk: resolved edges, edge kinds, test files.

mod test_project;

use prober_core::changes::ChangeKind;
use prober_core::graph::{EdgeKind, GraphBuilder};
use prober_lang_typescript::TypeScriptLinker;

use crate::test_project::{TestProject, change, dependents, repo_path, selected_tests};

#[test]
fn relative_imports_resolve_omitted_extensions_index_files_js_specifiers_and_json() {
    let project = TestProject::new(&[
        (
            "src/app.ts",
            r#"
                import { b } from "./b";
                import { lib } from "./lib";
                import { c } from "./c.js";
                import data from "./data.json";
            "#,
        ),
        ("src/b.ts", "export const b = 1;"),
        ("src/lib/index.ts", "export const lib = 1;"),
        ("src/c.ts", "export const c = 1;"),
        ("src/data.json", "{}"),
    ]);
    let graph = project.link();

    for dependency in ["src/b.ts", "src/lib/index.ts", "src/c.ts", "src/data.json"] {
        assert_eq!(
            dependents(&graph, dependency),
            [("src/app.ts", EdgeKind::Import)],
            "{dependency}"
        );
    }
}

#[test]
fn type_only_import_links_without_selecting_the_test() {
    let project = TestProject::new(&[
        (
            "src/a.test.ts",
            r#"
                import type { Config } from "./config";
                import { run } from "./run";
            "#,
        ),
        (
            "src/config.ts",
            "export type Config = { verbose: boolean };",
        ),
        ("src/run.ts", "export const run = () => 1;"),
    ]);
    let graph = project.link();

    assert_eq!(
        dependents(&graph, "src/config.ts"),
        [("src/a.test.ts", EdgeKind::TypeOnly)]
    );
    assert_eq!(
        selected_tests(&graph, &[change("src/config.ts", ChangeKind::Modified)]),
        Vec::<String>::new()
    );
    assert_eq!(
        selected_tests(&graph, &[change("src/run.ts", ChangeKind::Modified)]),
        ["src/a.test.ts"]
    );
}

#[test]
fn tsconfig_paths_resolve_with_the_nearest_tsconfig() {
    let project = TestProject::new(&[
        (
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@app/*": ["./src/*"] } } }"#,
        ),
        ("src/main.ts", r#"import { util } from "@app/util";"#),
        ("src/util.ts", "export const util = 1;"),
        (
            "packages/web/tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@web/*": ["./lib/*"] } } }"#,
        ),
        (
            "packages/web/src/page.ts",
            r#"import { button } from "@web/button";"#,
        ),
        ("packages/web/lib/button.ts", "export const button = 1;"),
    ]);
    let graph = project.link();

    assert_eq!(
        dependents(&graph, "src/util.ts"),
        [("src/main.ts", EdgeKind::Import)]
    );
    assert_eq!(
        dependents(&graph, "packages/web/lib/button.ts"),
        [("packages/web/src/page.ts", EdgeKind::Import)]
    );
}

#[cfg(unix)]
#[test]
fn workspace_package_exports_resolve_through_node_modules() {
    let project = TestProject::new(&[
        (
            "packages/lib/package.json",
            r#"{
                "name": "@acme/lib",
                "exports": {
                    ".": { "types": "./dist/index.d.ts", "import": "./src/index.ts" },
                    "./utils": "./src/utils.ts"
                }
            }"#,
        ),
        ("packages/lib/src/index.ts", "export const lib = 1;"),
        ("packages/lib/src/utils.ts", "export const utils = 1;"),
        (
            "apps/web/src/a.ts",
            r#"
                import { lib } from "@acme/lib";
                import { utils } from "@acme/lib/utils";
            "#,
        ),
    ]);

    project.symlink("node_modules/@acme/lib", "../../packages/lib");

    let graph = project.link();

    assert_eq!(
        dependents(&graph, "packages/lib/src/index.ts"),
        [("apps/web/src/a.ts", EdgeKind::Import)]
    );
    assert_eq!(
        dependents(&graph, "packages/lib/src/utils.ts"),
        [("apps/web/src/a.ts", EdgeKind::Import)]
    );
}

#[test]
fn bare_imports_link_to_a_package_node_whether_installed_or_not() {
    let project = TestProject::new(&[
        (
            "node_modules/lodash/package.json",
            r#"{ "name": "lodash", "main": "index.js" }"#,
        ),
        ("node_modules/lodash/index.js", "module.exports = {};"),
        (
            "src/a.ts",
            r#"
                import lodash from "lodash";
                import map from "lodash/fp/map";
                import { readFile } from "node:fs";
                import path from "path";
                import missing from "@acme/not-installed/sub";
                import type { Options } from "options-types";
            "#,
        ),
    ]);
    let graph = project.link();

    assert_eq!(
        graph.file_id(&repo_path("node_modules/lodash/index.js")),
        None
    );
    assert_eq!(
        dependents(&graph, "node_modules/lodash"),
        [("src/a.ts", EdgeKind::Import)]
    );
    assert_eq!(
        dependents(&graph, "node_modules/@acme/not-installed"),
        [("src/a.ts", EdgeKind::Import)]
    );
    assert_eq!(
        dependents(&graph, "node_modules/options-types"),
        [("src/a.ts", EdgeKind::TypeOnly)]
    );
    assert_eq!(dependents(&graph, "node_modules/path"), []);
    assert_eq!(dependents(&graph, "node_modules/node:fs"), []);
}

#[test]
fn workspace_packages_resolve_through_their_exports_without_node_modules() {
    let project = TestProject::new(&[
        (
            "pnpm-workspace.yaml",
            "packages:\n  - packages/*\n  - apps/*\n",
        ),
        (
            "packages/lib/package.json",
            r#"{
                "name": "@acme/lib",
                "exports": {
                    ".": { "types": "./dist/index.d.ts", "import": "./src/index.ts" },
                    "./utils": "./src/utils.ts"
                }
            }"#,
        ),
        ("packages/lib/src/index.ts", "export const lib = 1;"),
        ("packages/lib/src/utils.ts", "export const utils = 1;"),
        ("apps/web/package.json", r#"{ "name": "@acme/web" }"#),
        (
            "apps/web/src/a.ts",
            r#"
                import { lib } from "@acme/lib";
                import { utils } from "@acme/lib/utils";
                import { missing } from "@acme/lib/missing";
            "#,
        ),
    ]);
    let graph = project.link();

    assert_eq!(
        dependents(&graph, "packages/lib/src/index.ts"),
        [("apps/web/src/a.ts", EdgeKind::Import)]
    );
    assert_eq!(
        dependents(&graph, "packages/lib/src/utils.ts"),
        [("apps/web/src/a.ts", EdgeKind::Import)]
    );
    assert_eq!(dependents(&graph, "node_modules/@acme/lib"), []);
}

#[test]
fn test_still_importing_a_deleted_file_is_selected() {
    let project = TestProject::new(&[("src/util.test.ts", r#"import { util } from "./util";"#)]);
    let graph = project.link();

    assert_eq!(
        dependents(&graph, "src/util.ts"),
        [("src/util.test.ts", EdgeKind::Import)]
    );
    assert_eq!(
        selected_tests(&graph, &[change("src/util.ts", ChangeKind::Deleted)]),
        ["src/util.test.ts"]
    );
}

#[test]
fn test_files_are_registered_with_their_language() {
    let project = TestProject::new(&[
        ("src/a.ts", "export const a = 1;"),
        ("src/a.test.ts", "import { a } from './a';"),
        ("src/a.test-support.ts", "export const fixture = 1;"),
        ("README.md", "import { a } from './src/a';"),
    ]);
    let graph = project.link();
    let test_files: Vec<(&str, &str)> = graph
        .test_files_by_path()
        .iter()
        .map(|test_file| (graph.path(test_file.file).as_str(), test_file.language))
        .collect();

    assert_eq!(test_files, [("src/a.test.ts", "typescript")]);
    assert_eq!(
        dependents(&graph, "src/a.ts"),
        [("src/a.test.ts", EdgeKind::Import)]
    );
}

#[test]
fn listed_file_missing_on_disk_is_an_error() {
    let project = TestProject::new(&[]);
    let mut builder = GraphBuilder::default();
    let error = TypeScriptLinker::new(project.root(), &[])
        .unwrap()
        .link(&[repo_path("src/gone.ts")], &mut builder)
        .unwrap_err();

    assert_eq!(error.to_string(), "cannot read src/gone.ts");
}

#[test]
fn tsconfig_extending_an_uninstalled_package_keeps_its_own_paths() {
    let project = TestProject::new(&[
        (
            "tsconfig.json",
            r#"{
                // shared base installed from npm
                "extends": ["@tsconfig/node20/tsconfig.json", "./tsconfig.paths.json"],
            }"#,
        ),
        (
            "tsconfig.paths.json",
            r#"{ "compilerOptions": { "paths": { "@app/*": ["./src/*"] } } }"#,
        ),
        (
            "src/a.ts",
            r#"
                import { b } from "./b";
                import { c } from "@app/c";
            "#,
        ),
        ("src/b.ts", "export const b = 1;"),
        ("src/c.ts", "export const c = 1;"),
    ]);
    let graph = project.link();

    assert_eq!(
        dependents(&graph, "src/b.ts"),
        [("src/a.ts", EdgeKind::Import)]
    );
    assert_eq!(
        dependents(&graph, "src/c.ts"),
        [("src/a.ts", EdgeKind::Import)]
    );
}
