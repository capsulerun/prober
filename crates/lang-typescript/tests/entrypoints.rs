//! Built and spawned entrypoints on projects written to disk: build outputs mapped to their sources, and the scripts, bins and package scripts a process launch runs.

mod test_project;

use prober_core::config::InferenceSettings;
use prober_core::graph::EdgeKind;

use crate::test_project::{TestProject, dependents, modified, repo_path, selected, selected_tests};

fn built_cli() -> TestProject {
    TestProject::new(&[
        (
            "package.json",
            r#"{ "name": "cli", "type": "module", "bin": { "cli": "bin/cli.mjs" }, "main": "dist/index.js" }"#,
        ),
        (
            "bin/cli.mjs",
            r#"
const entry = new URL("../dist/entry.js", import.meta.url);

await import(entry.href);
"#,
        ),
        (
            "src/entry.ts",
            r#"import { addAgent } from "./commands/agents.js";"#,
        ),
        ("src/commands/agents.ts", "export const addAgent = 1;"),
        (
            "test/helpers/run-cli.ts",
            r#"
import { spawnSync } from "node:child_process";
import path from "node:path";

export function runCli(args: string[]) {
    const entry = path.resolve(process.cwd(), "bin/cli.mjs");
    return spawnSync(process.execPath, [entry, ...args], { encoding: "utf8" });
}
"#,
        ),
        (
            "test/agents.e2e.test.ts",
            r#"import { runCli } from "./helpers/run-cli";"#,
        ),
    ])
}

#[test]
fn e2e_test_spawning_the_built_cli_reaches_the_command_sources() {
    let project = built_cli();

    assert_eq!(
        project.outcome_of(&[], &[modified("src/commands/agents.ts")]),
        selected(&[(
            "test/agents.e2e.test.ts",
            &[
                "src/commands/agents.ts (modified)",
                "-> imported by src/entry.ts",
                "-> built into dist/entry.js (inferred build mapping dist/ <- src/ (package.json))",
                "-> loaded by bin/cli.mjs:4 (inferred path dist/entry.js)",
                "-> spawned by test/helpers/run-cli.ts:7 (inferred spawn bin/cli.mjs)",
                "-> imported by test/agents.e2e.test.ts",
            ]
        )])
    );
}

#[test]
fn a_file_dependency_on_a_build_output_resolves_through_its_sources() {
    let project = TestProject::new(&[
        (
            "nodejs/package.json",
            r#"{ "name": "@acme/db", "main": "dist/index.js", "types": "dist/index.d.ts" }"#,
        ),
        (
            "nodejs/tsconfig.json",
            r#"{ "include": ["db"], "compilerOptions": { "outDir": "./dist", "rootDir": "./db" } }"#,
        ),
        (
            "nodejs/db/index.ts",
            r#"export { connect } from "./connection";"#,
        ),
        ("nodejs/db/connection.ts", "export const connect = 1;"),
        ("nodejs/db/embedding/index.ts", "export const registry = 1;"),
        (
            "nodejs/examples/package.json",
            r#"{ "name": "examples", "dependencies": { "@acme/db": "file:../dist" } }"#,
        ),
        (
            "nodejs/examples/basic.test.ts",
            r#"import { connect } from "@acme/db"; connect;"#,
        ),
        (
            "nodejs/examples/embedding.test.ts",
            r#"import { registry } from "@acme/db/embedding"; registry;"#,
        ),
        (
            "test/fixtures/package.json",
            r#"{ "name": "fixture", "dependencies": { "@acme/db": "file:./local" } }"#,
        ),
    ]);

    assert_eq!(
        selected_tests(&project.link(), &[modified("nodejs/db/connection.ts")]),
        ["nodejs/examples/basic.test.ts"]
    );
    assert_eq!(
        selected_tests(&project.link(), &[modified("nodejs/db/embedding/index.ts")]),
        ["nodejs/examples/embedding.test.ts"]
    );
}

#[test]
fn imports_of_build_outputs_resolve_through_their_sources() {
    let project = TestProject::new(&[
        (
            "package.json",
            r#"{ "name": "root", "private": true, "workspaces": ["packages/*"] }"#,
        ),
        (
            "packages/ui/package.json",
            r#"{ "name": "@acme/ui", "exports": { ".": { "types": "./dist/index.d.mts", "import": "./dist/index.mjs" } } }"#,
        ),
        (
            "packages/ui/src/index.ts",
            r#"export { Button } from "./button";"#,
        ),
        ("packages/ui/src/button.ts", "export const Button = 1;"),
        (
            "packages/app/package.json",
            r#"{ "name": "@acme/app", "dependencies": { "@acme/ui": "workspace:*" } }"#,
        ),
        (
            "packages/app/src/page.test.ts",
            r#"import { Button } from "@acme/ui";"#,
        ),
        (
            "packages/app/src/bundle.test.ts",
            r#"import { Button } from "../../ui/dist/index.mjs";"#,
        ),
    ]);
    let changes = [modified("packages/ui/src/button.ts")];

    assert_eq!(
        project.outcome_of(&[], &changes),
        selected(&[
            (
                "packages/app/src/bundle.test.ts",
                &[
                    "packages/ui/src/button.ts (modified)",
                    "-> imported by packages/ui/src/index.ts",
                    "-> built into packages/ui/dist/index.mjs (inferred build mapping packages/ui/dist/ <- packages/ui/src/ (packages/ui/package.json))",
                    "-> imported by packages/app/src/bundle.test.ts",
                ]
            ),
            (
                "packages/app/src/page.test.ts",
                &[
                    "packages/ui/src/button.ts (modified)",
                    "-> imported by packages/ui/src/index.ts",
                    "-> built into packages/ui/dist/index.mjs (inferred build mapping packages/ui/dist/ <- packages/ui/src/ (packages/ui/package.json))",
                    "-> imported by packages/app/src/page.test.ts",
                ]
            ),
        ])
    );

    let without_build_mapping = project.link_with(InferenceSettings {
        build_mapping: false,
        ..InferenceSettings::default()
    });

    assert_eq!(
        selected_tests(&without_build_mapping, &changes),
        Vec::<String>::new()
    );
}

#[test]
fn bundler_entries_map_renamed_outputs() {
    let project = TestProject::new(&[
        ("package.json", r#"{ "name": "cli" }"#),
        (
            "tsdown.config.ts",
            r#"
import { defineConfig } from "tsdown";

export default defineConfig({
    entry: { "cli/daemon": "src/cli/daemon-cli.ts" },
    outDir: "build",
});
"#,
        ),
        ("src/cli/daemon-cli.ts", "export const daemon = 1;"),
        (
            "test/daemon.e2e.test.ts",
            r#"import { daemon } from "../build/cli/daemon.js";"#,
        ),
    ]);
    let graph = project.link();
    let daemon_output = graph.file_id(&repo_path("build/cli/daemon.js")).unwrap();
    let build_edge = graph
        .dependencies(daemon_output)
        .find(|edge| edge.kind == EdgeKind::Build)
        .unwrap();

    assert_eq!(
        graph.path(build_edge.dependency).as_str(),
        "src/cli/daemon-cli.ts"
    );
    assert_eq!(
        graph.inference(&build_edge).unwrap().description(),
        "inferred build mapping build/cli/daemon <- src/cli/daemon-cli.ts (tsdown.config.ts)"
    );
}

#[test]
fn wrapped_spawns_link_the_scripts_their_callers_pass() {
    let project = TestProject::new(&[
        ("package.json", r#"{ "name": "tools" }"#),
        (
            "test/helpers/scripts.ts",
            r#"
import { execFileSync } from "node:child_process";

export function runScript(script: string, args: string[] = []) {
    return execFileSync("node", [script, ...args], { encoding: "utf8" });
}
"#,
        ),
        (
            "test/release.test.ts",
            r#"
import { runScript } from "./helpers/scripts";

runScript("scripts/release.mjs", ["--dry-run"]);
"#,
        ),
        (
            "scripts/release.mjs",
            r#"import { nextVersion } from "./lib/version.mjs";"#,
        ),
        ("scripts/lib/version.mjs", "export const nextVersion = 1;"),
    ]);

    assert_eq!(
        project.outcome_of(&[], &[modified("scripts/lib/version.mjs")]),
        selected(&[(
            "test/release.test.ts",
            &[
                "scripts/lib/version.mjs (modified)",
                "-> imported by scripts/release.mjs",
                "-> spawned by test/release.test.ts:4 (inferred spawn scripts/release.mjs)",
            ]
        )])
    );
}

#[test]
fn package_scripts_and_bins_run_what_they_name() {
    let project = TestProject::new(&[
        (
            "package.json",
            r#"{ "name": "tools", "scripts": { "build": "node --import ./register.mjs scripts/build.mjs" }, "workspaces": ["packages/*"] }"#,
        ),
        ("register.mjs", ""),
        ("scripts/build.mjs", ""),
        (
            "packages/cli/package.json",
            r#"{ "name": "@tools/cli", "bin": "./bin/run.js" }"#,
        ),
        ("packages/cli/bin/run.js", ""),
        (
            "test/build.test.ts",
            r#"
import { execSync } from "node:child_process";

execSync("pnpm run build");
execSync(`npx cli --version`);
execSync("git status");
"#,
        ),
    ]);
    let graph = project.link();
    let spawned_by = |script: &str| -> Vec<(&str, EdgeKind)> { dependents(&graph, script) };

    assert_eq!(
        spawned_by("scripts/build.mjs"),
        [("test/build.test.ts", EdgeKind::Spawn)]
    );
    assert_eq!(
        spawned_by("register.mjs"),
        [("test/build.test.ts", EdgeKind::Spawn)]
    );
    assert_eq!(
        spawned_by("packages/cli/bin/run.js"),
        [("test/build.test.ts", EdgeKind::Spawn)]
    );
}

#[test]
fn shell_scripts_and_jiti_loads_are_entrypoints_too() {
    let project = TestProject::new(&[
        (
            "test/install.test.ts",
            r#"
import { spawnSync } from "child_process";
import { createJiti } from "jiti";

spawnSync("bash", ["scripts/install.sh", "--prefix", "/tmp/x"]);

const jiti = createJiti(import.meta.url);
await jiti.import("../config/defaults.ts");
"#,
        ),
        ("scripts/install.sh", "echo install"),
        ("config/defaults.ts", "export default {};"),
    ]);
    let graph = project.link();

    assert!(
        dependents(&graph, "scripts/install.sh")
            .contains(&("test/install.test.ts", EdgeKind::Spawn))
    );
    assert!(
        dependents(&graph, "config/defaults.ts")
            .contains(&("test/install.test.ts", EdgeKind::Dynamic))
    );
}

#[test]
fn spawned_entrypoints_switch_off_on_their_own() {
    let project = built_cli();
    let changes = [modified("src/commands/agents.ts")];
    let graph = project.link_with(InferenceSettings {
        spawned_entrypoints: false,
        ..InferenceSettings::default()
    });

    assert_eq!(selected_tests(&graph, &changes), Vec::<String>::new());
    assert_eq!(
        dependents(&graph, "src/entry.ts"),
        [("dist/entry.js", EdgeKind::Build)]
    );
}

#[test]
fn a_component_change_reaches_the_tests_of_the_components_using_it() {
    let base_input = "<template>\n  <textarea placeholder=\"Ask\" />\n</template>\n<script setup lang=\"ts\">\nconst props = defineProps<{ value: string }>();\n</script>\n";
    let project = TestProject::new(&[
        (
            "package.json",
            r#"{ "devDependencies": { "vitest": "^3" } }"#,
        ),
        ("src/components/ChatInput/ChatInput.vue", base_input),
        (
            "src/components/ChatInput/index.ts",
            "export { default } from './ChatInput.vue';",
        ),
        (
            "src/components/AskAssistantChat/AskAssistantChat.vue",
            "<template>\n  <ChatInput />\n</template>\n<script setup lang=\"ts\">\nimport ChatInput from '../ChatInput';\n</script>\n",
        ),
        (
            "src/components/AskAssistantChat/AskAssistantChat.test.ts",
            "import AskAssistantChat from './AskAssistantChat.vue';\n",
        ),
        ("src/components/Other/Other.test.ts", "export {};"),
    ]);

    project.write(
        "src/components/ChatInput/ChatInput.vue",
        &base_input.replace("Ask", "Ask a question"),
    );

    assert_eq!(
        project
            .symbol_outcome_of(
                &[("src/components/ChatInput/ChatInput.vue", base_input)],
                &[modified("src/components/ChatInput/ChatInput.vue")]
            )
            .selected_paths(),
        ["src/components/AskAssistantChat/AskAssistantChat.test.ts"]
    );
}
