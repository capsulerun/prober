//! Links through files on projects written to disk: directory scans, paths listed in data files, literal paths, removed files and naming families.

mod test_project;

use prober_core::changes::ChangeKind;
use prober_core::config::InferenceSettings;
use prober_core::graph::EdgeKind;

use crate::test_project::{TestProject, change, dependents, modified, selected};

fn plugin_host() -> TestProject {
    TestProject::new(&[
        (
            "src/plugins/discovery.ts",
            r#"
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

const extensionsDirectory = join(import.meta.dirname, "../../extensions");

export function discoverPlugins() {
    return readdirSync(extensionsDirectory).map((name) =>
        JSON.parse(readFileSync(join(extensionsDirectory, name, "plugin.json"), "utf8")),
    );
}
"#,
        ),
        (
            "src/plugins/registry.test.ts",
            r#"import { discoverPlugins } from "./discovery";"#,
        ),
        (
            "extensions/telegram/plugin.json",
            r#"{ "id": "telegram", "entries": { "runtime": "./index.ts" } }"#,
        ),
        (
            "extensions/telegram/index.ts",
            r#"import { send } from "./src/send";"#,
        ),
        ("extensions/telegram/src/send.ts", "export const send = 1;"),
        (
            "extensions/telegram/src/send.test.ts",
            r#"import { send } from "./send";"#,
        ),
    ])
}

#[test]
fn plugin_entries_listed_by_scanned_manifests_reach_the_discovery_tests() {
    let project = plugin_host();

    assert_eq!(
        project.outcome_of(&[], &[modified("extensions/telegram/index.ts")]),
        selected(&[(
            "src/plugins/registry.test.ts",
            &[
                "extensions/telegram/index.ts (modified)",
                "-> listed by extensions/telegram/plugin.json:1 (inferred listed path ./index.ts)",
                "-> read by src/plugins/discovery.ts:9 (inferred file read extensions/*/plugin.json)",
                "-> imported by src/plugins/registry.test.ts",
            ],
        )])
    );
    assert_eq!(
        project
            .outcome_of(&[], &[modified("extensions/telegram/src/send.ts")])
            .selected_paths(),
        ["extensions/telegram/src/send.test.ts"]
    );
}

#[test]
fn added_and_removed_plugin_folders_reach_the_module_listing_them() {
    let project = plugin_host();

    project.write(
        "extensions/slack/plugin.json",
        r#"{ "id": "slack", "entries": { "runtime": "./index.ts" } }"#,
    );
    project.write("extensions/slack/index.ts", "export const slack = 1;");

    assert_eq!(
        project.outcome_of(
            &[],
            &[
                change("extensions/slack/index.ts", ChangeKind::Added),
                change("extensions/slack/plugin.json", ChangeKind::Added),
            ]
        ),
        selected(&[(
            "src/plugins/registry.test.ts",
            &[
                "extensions/slack/index.ts (added)",
                "-> read by src/plugins/discovery.ts:8 (inferred directory listing extensions/)",
                "-> imported by src/plugins/registry.test.ts",
            ],
        )])
    );
    assert_eq!(
        project.outcome_of(
            &[],
            &[
                change("extensions/discord/index.ts", ChangeKind::Deleted),
                change("extensions/discord/plugin.json", ChangeKind::Deleted),
            ]
        ),
        selected(&[(
            "src/plugins/registry.test.ts",
            &[
                "extensions/discord/index.ts (deleted)",
                "-> read by src/plugins/discovery.ts:8 (inferred directory listing extensions/)",
                "-> imported by src/plugins/registry.test.ts",
            ],
        )])
    );
}

#[test]
fn paths_listed_in_a_data_file_reach_whoever_reads_or_copies_it() {
    let project = TestProject::new(&[
        (
            "test/helpers/pr-wrapper.ts",
            r#"
import { cpSync, readFileSync } from "node:fs";
import { join } from "node:path";

export function copyWrapperSources(destination: string) {
    const components = [
        "scripts/pr",
        "scripts/pr-lib",
        ...readFileSync("scripts/pr-lib/components.txt", "utf8").trim().split("\n"),
    ];

    for (const component of components) {
        cpSync(component, join(destination, component), { recursive: true });
    }
}
"#,
        ),
        (
            "test/scripts/wrapper-sources.test.ts",
            r#"import { copyWrapperSources } from "../helpers/pr-wrapper";"#,
        ),
        (
            "scripts/pr",
            "#!/usr/bin/env bash\nsource scripts/pr-lib/common.sh\n",
        ),
        ("scripts/pr-lib/common.sh", "set -euo pipefail\n"),
        (
            "scripts/pr-lib/components.txt",
            "packages/core/src/redact.ts\nconfig/timeouts.json\n",
        ),
        (
            "packages/core/src/redact.ts",
            r#"import { mask } from "./mask";"#,
        ),
        ("packages/core/src/mask.ts", "export const mask = 1;"),
        ("config/timeouts.json", "{}"),
    ]);

    assert_eq!(
        project.outcome_of(&[], &[modified("packages/core/src/redact.ts")]),
        selected(&[(
            "test/scripts/wrapper-sources.test.ts",
            &[
                "packages/core/src/redact.ts (modified)",
                "-> listed by scripts/pr-lib/components.txt:1 (inferred listed path packages/core/src/redact.ts)",
                "-> read by test/helpers/pr-wrapper.ts:9 (inferred file path scripts/pr-lib/components.txt)",
                "-> imported by test/scripts/wrapper-sources.test.ts",
            ],
        )])
    );
    assert_eq!(
        project.outcome_of(&[], &[modified("scripts/pr-lib/common.sh")]),
        selected(&[(
            "test/scripts/wrapper-sources.test.ts",
            &[
                "scripts/pr-lib/common.sh (modified)",
                "-> read by test/helpers/pr-wrapper.ts:13 (inferred file copy scripts/pr-lib/)",
                "-> imported by test/scripts/wrapper-sources.test.ts",
            ],
        )])
    );
}

#[test]
fn reads_depend_on_the_content_of_the_file_not_on_its_imports() {
    let project = TestProject::new(&[
        (
            "src/prompts.ts",
            r#"
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

export const system = readFileSync(new URL("../prompts/system.md", import.meta.url), "utf8");
export const sample = readFileSync(path.join(path.dirname(fileURLToPath(import.meta.url)), "sample.ts"), "utf8");
"#,
        ),
        (
            "src/agent.test.ts",
            r#"import { system } from "./prompts";"#,
        ),
        ("prompts/system.md", "You are a helpful agent."),
        ("src/sample.ts", r#"import { helper } from "./helper";"#),
        ("src/helper.ts", "export const helper = 1;"),
    ]);

    assert_eq!(
        project.outcome_of(&[], &[modified("prompts/system.md")]),
        selected(&[(
            "src/agent.test.ts",
            &[
                "prompts/system.md (modified)",
                "-> read by src/prompts.ts:6 (inferred file path ../prompts/system.md)",
                "-> imported by src/agent.test.ts",
            ],
        )])
    );
    assert_eq!(
        project
            .outcome_of(&[], &[modified("src/sample.ts")])
            .selected_paths(),
        ["src/agent.test.ts"]
    );
    assert_eq!(
        project
            .outcome_of(&[], &[modified("src/helper.ts")])
            .selected_paths(),
        Vec::<&str>::new()
    );
}

#[test]
fn literal_paths_link_fixtures_and_scripts_to_the_tests_naming_them() {
    let project = TestProject::new(&[
        (
            "test/scripts/upgrade-baseline.test.ts",
            r#"
import { spawnSync } from "node:child_process";

const result = spawnSync(process.execPath, ["scripts/plan-docker-lanes.mjs", "--check-baselines"]);
"#,
        ),
        (
            "scripts/plan-docker-lanes.mjs",
            r#"import { lanes } from "./lib/lanes.mjs";"#,
        ),
        ("scripts/lib/lanes.mjs", "export const lanes = [];"),
        (
            "src/config/load.test.ts",
            r#"const fixture = "./fixtures/legacy.json";"#,
        ),
        ("src/config/fixtures/legacy.json", "{}"),
        (
            "test/inventory.ts",
            r#"export const slowTests = ["src/config/load.test.ts"];"#,
        ),
        (
            "test/inventory.test.ts",
            r#"import { slowTests } from "./inventory";"#,
        ),
    ]);

    assert_eq!(
        project.outcome_of(&[], &[modified("scripts/plan-docker-lanes.mjs")]),
        selected(&[(
            "test/scripts/upgrade-baseline.test.ts",
            &[
                "scripts/plan-docker-lanes.mjs (modified)",
                "-> read by test/scripts/upgrade-baseline.test.ts:4 (inferred file path scripts/plan-docker-lanes.mjs)",
            ],
        )])
    );
    assert_eq!(
        project
            .outcome_of(&[], &[modified("src/config/fixtures/legacy.json")])
            .selected_paths(),
        ["src/config/load.test.ts"]
    );
    assert_eq!(
        project
            .outcome_of(&[], &[modified("src/config/load.test.ts")])
            .selected_paths(),
        ["src/config/load.test.ts"]
    );
    project.remove("src/config/fixtures/legacy.json");

    assert_eq!(
        project.outcome_of(
            &[],
            &[change(
                "src/config/fixtures/legacy.json",
                ChangeKind::Deleted
            )]
        ),
        selected(&[(
            "src/config/load.test.ts",
            &[
                "src/config/fixtures/legacy.json (deleted)",
                "-> read by src/config/load.test.ts:1 (inferred file path ./fixtures/legacy.json)",
            ],
        )])
    );
}

#[test]
fn files_of_other_languages_pass_on_the_paths_they_name() {
    let project = TestProject::new(&[
        (
            "test/contracts.test.ts",
            r#"
import { execFileSync } from "node:child_process";

execFileSync("python3", ["scripts/check_contract.py"]);
"#,
        ),
        (
            "scripts/check_contract.py",
            "import json\n\nwith open(\"contracts/session.json\") as contract:\n    json.load(contract)\n\nprint(open(\"package.json\").read())\n",
        ),
        ("package.json", r#"{ "name": "app" }"#),
        ("contracts/session.json", r#"{ "type": "object" }"#),
        (
            "apps/android/src/test/SessionContractTest.kt",
            "val contract = File(\"../../../../contracts/session.json\").readText()\n",
        ),
    ]);

    assert_eq!(
        project.outcome_of(&[], &[modified("contracts/session.json")]),
        selected(&[(
            "test/contracts.test.ts",
            &[
                "contracts/session.json (modified)",
                "-> listed by scripts/check_contract.py:3 (inferred listed path contracts/session.json)",
                "-> read by test/contracts.test.ts:4 (inferred file path scripts/check_contract.py)",
            ],
        )])
    );
    assert_eq!(dependents(&project.link(), "package.json"), []);
}

#[test]
fn tests_are_paired_with_the_files_and_fixtures_named_after_them() {
    let project = TestProject::new(&[
        ("src/channel.ts", "export const channel = 1;"),
        ("src/channel.e2e.test.ts", ""),
        ("src/channel-send.test.ts", ""),
        ("src/__tests__/channel.test.ts", ""),
        ("scripts/release.mjs", ""),
        ("test/scripts/release.test.ts", ""),
        ("test/fixtures/basic/input.json", "{}"),
        ("test/fixtures/basic.json", "{}"),
        ("test/parser/basic.test.ts", ""),
        ("src/styles/button.css", ".button {}"),
        ("src/styles/button.test.ts", ""),
    ]);
    let graph = project.link();

    assert_eq!(
        dependents(&graph, "src/channel.ts"),
        [
            ("src/__tests__/channel.test.ts", EdgeKind::Family),
            ("src/channel-send.test.ts", EdgeKind::Family),
            ("src/channel.e2e.test.ts", EdgeKind::Family),
        ]
    );
    assert_eq!(
        dependents(&graph, "scripts/release.mjs"),
        [("test/scripts/release.test.ts", EdgeKind::Family)]
    );
    assert_eq!(
        dependents(&graph, "test/fixtures/basic/input.json"),
        [("test/parser/basic.test.ts", EdgeKind::Family)]
    );
    assert_eq!(
        dependents(&graph, "test/fixtures/basic.json"),
        [("test/parser/basic.test.ts", EdgeKind::Family)]
    );
    assert_eq!(dependents(&graph, "src/styles/button.css"), []);
    assert_eq!(
        project.outcome_of(&[], &[modified("scripts/release.mjs")]),
        selected(&[(
            "test/scripts/release.test.ts",
            &[
                "scripts/release.mjs (modified)",
                "-> paired by name with test/scripts/release.test.ts",
            ],
        )])
    );
}

#[test]
fn stylesheets_keep_their_assets_narrowed_to_tests_that_render_them() {
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
        (
            "src/app.css",
            "body { background: url(./assets/logo.png); }",
        ),
        ("src/assets/logo.png", ""),
        ("src/app.ts", r#"import "./app.css";"#),
        ("src/app.test.ts", r#"import "./app";"#),
        ("src/app.spec.ts", r#"import "./app";"#),
    ]);

    assert_eq!(
        dependents(&project.link(), "src/assets/logo.png"),
        [("src/app.css", EdgeKind::Asset)]
    );
    assert_eq!(
        project
            .outcome_of(&[], &[modified("src/assets/logo.png")])
            .selected_paths(),
        ["src/app.spec.ts"]
    );
}

#[test]
fn file_system_calls_are_followed_through_their_module_aliases() {
    let project = TestProject::new(&[
        (
            "scripts/build.cjs",
            r#"
const fs = require("node:fs");
const fg = require("fast-glob");
const path = require("node:path");

fs.promises.readFile(path.resolve("config", "defaults.json"));
fg.sync(["locales/*.json", "!locales/draft.json"], { cwd: path.join(__dirname, "..") });
fs.readdirSync(path.join(process.cwd(), "templates"), { recursive: true });
"#,
        ),
        ("config/defaults.json", "{}"),
        ("locales/en.json", "{}"),
        ("locales/draft.json", "{}"),
        ("templates/email/welcome.html", ""),
    ]);
    let graph = project.link();

    assert_eq!(
        dependents(&graph, "config/defaults.json"),
        [("scripts/build.cjs", EdgeKind::Reads)]
    );
    assert_eq!(
        dependents(&graph, "locales/en.json"),
        [("scripts/build.cjs", EdgeKind::Reads)]
    );
    assert_eq!(
        dependents(&graph, "templates/email/welcome.html"),
        [("scripts/build.cjs", EdgeKind::Reads)]
    );
}

#[test]
fn file_inference_and_naming_families_switch_off_on_their_own() {
    let project = plugin_host();
    let graph = project.link_with(InferenceSettings {
        files: false,
        naming_families: false,
        ..InferenceSettings::default()
    });

    assert_eq!(dependents(&graph, "extensions/telegram/plugin.json"), []);
    assert_eq!(
        dependents(&graph, "extensions/telegram/src/send.ts"),
        [
            ("extensions/telegram/index.ts", EdgeKind::Import),
            ("extensions/telegram/src/send.test.ts", EdgeKind::Import)
        ]
    );
}
