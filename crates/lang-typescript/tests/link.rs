//! Linking small TypeScript projects written to disk: resolved edges, edge kinds, test files.

use std::fs;
use std::path::Path;

use prober_core::changes::{ChangeKind, FileChange};
use prober_core::files::RepoPath;
use prober_core::graph::{EdgeKind, Graph, GraphBuilder};
use prober_core::select::select_tests;
use prober_lang_typescript::TypeScriptLinker;
use tempfile::TempDir;

struct TestProject {
    directory: TempDir,
}

impl TestProject {
    fn new(files: &[(&str, &str)]) -> Self {
        let test_project = Self {
            directory: TempDir::new().unwrap(),
        };

        for &(path, content) in files {
            test_project.write(path, content);
        }

        test_project
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn write(&self, path: &str, content: &str) {
        let file_path = self.root().join(path);

        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(file_path, content).unwrap();
    }

    #[cfg(unix)]
    fn symlink(&self, link: &str, target: &str) {
        let link_path = self.root().join(link);

        fs::create_dir_all(link_path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(target, link_path).unwrap();
    }

    fn repo_files(&self) -> Vec<RepoPath> {
        let mut repo_files = Vec::new();

        collect_repo_files(self.root(), "", &mut repo_files);
        repo_files.sort();

        repo_files
    }

    fn link(&self) -> Graph {
        let mut builder = GraphBuilder::default();

        TypeScriptLinker::new(self.root())
            .unwrap()
            .link(&self.repo_files(), &mut builder)
            .unwrap();

        builder.build()
    }
}

fn collect_repo_files(directory: &Path, prefix: &str, repo_files: &mut Vec<RepoPath>) {
    for entry in fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        let repo_relative_path = format!("{prefix}{name}");

        if name == "node_modules" {
            continue;
        }

        if entry.file_type().unwrap().is_dir() {
            collect_repo_files(&entry.path(), &format!("{repo_relative_path}/"), repo_files);
        } else {
            repo_files.push(repo_path(&repo_relative_path));
        }
    }
}

fn repo_path(path: &str) -> RepoPath {
    RepoPath::new(path).unwrap()
}

fn dependents<'g>(graph: &'g Graph, dependency: &str) -> Vec<(&'g str, EdgeKind)> {
    let Some(dependency_file) = graph.file_id(&repo_path(dependency)) else {
        return Vec::new();
    };

    graph
        .dependents(dependency_file)
        .iter()
        .map(|edge| (graph.path(edge.dependent).as_str(), edge.kind))
        .collect()
}

fn selected_tests(graph: &Graph, changes: &[FileChange]) -> Vec<String> {
    select_tests(graph, changes)
        .iter()
        .map(|selected_test| graph.path(selected_test.file).as_str().to_owned())
        .collect()
}

fn change(path: &str, kind: ChangeKind) -> FileChange {
    FileChange {
        path: repo_path(path),
        kind,
    }
}

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
fn installed_packages_and_node_builtins_add_no_edges() {
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
                import { readFile } from "node:fs";
                import path from "path";
                import missing from "not-installed";
            "#,
        ),
    ]);
    let graph = project.link();

    assert_eq!(
        graph.file_id(&repo_path("node_modules/lodash/index.js")),
        None
    );
    assert_eq!(dependents(&graph, "not-installed"), []);
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
    let error = TypeScriptLinker::new(project.root())
        .unwrap()
        .link(&[repo_path("src/gone.ts")], &mut builder)
        .unwrap_err();

    assert_eq!(error.to_string(), "cannot read src/gone.ts");
}

#[test]
fn missing_node_modules_next_to_a_package_manifest_is_warned_about() {
    let project = TestProject::new(&[("package.json", r#"{ "name": "app" }"#)]);
    let linker = TypeScriptLinker::new(project.root()).unwrap();
    let warning_without_dependencies = linker.missing_dependencies_warning();

    fs::create_dir(project.root().join("node_modules")).unwrap();

    assert!(
        warning_without_dependencies
            .unwrap()
            .starts_with("node_modules not found")
    );
    assert_eq!(linker.missing_dependencies_warning(), None);
}

#[test]
fn repository_without_package_manifest_needs_no_node_modules() {
    let project = TestProject::new(&[("src/a.ts", "export const a = 1;")]);
    let linker = TypeScriptLinker::new(project.root()).unwrap();

    assert_eq!(linker.missing_dependencies_warning(), None);
}
