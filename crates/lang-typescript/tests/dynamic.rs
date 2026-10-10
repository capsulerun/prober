//! Dynamic loading on small TypeScript projects written to disk: inferred links, their evidence, and the sites left unresolved.

mod test_project;

use prober_core::changes::ChangeKind;
use prober_core::config::InferenceSettings;
use prober_core::graph::{EdgeKind, Graph, UnresolvedCause};

use crate::test_project::{TestProject, change, dependents, repo_path, selected_tests};

fn loaded_by(graph: &Graph, dependency: &str) -> Vec<(String, String)> {
    let Some(dependency_file) = graph.file_id(&repo_path(dependency)) else {
        return Vec::new();
    };

    graph
        .dependents(dependency_file)
        .filter(|edge| edge.kind == EdgeKind::Dynamic)
        .map(|edge| {
            let inference = graph
                .inference(&edge)
                .expect("dynamic edges carry their inference");

            (
                format!("{}:{}", graph.path(edge.dependent), inference.line),
                inference.description(),
            )
        })
        .collect()
}

fn loaded(dependent: &str, description: &str) -> (String, String) {
    (dependent.to_owned(), description.to_owned())
}

fn unresolved_sites(graph: &Graph) -> Vec<(String, String, UnresolvedCause)> {
    graph
        .unresolved_sites()
        .iter()
        .map(|site| {
            (
                format!("{}:{}", graph.path(site.file), site.line),
                site.expression.clone(),
                site.cause,
            )
        })
        .collect()
}

fn unresolved(
    site: &str,
    expression: &str,
    cause: UnresolvedCause,
) -> (String, String, UnresolvedCause) {
    (site.to_owned(), expression.to_owned(), cause)
}

#[test]
fn template_literal_links_every_file_of_its_glob() {
    let project = TestProject::new(&[
        (
            "src/loader.ts",
            r#"
                const name = process.env.PLUGIN;

                export const plugin = await import(`./plugins/${name}.js`);
            "#,
        ),
        ("src/plugins/telegram.ts", "export const id = 1;"),
        ("src/plugins/slack.ts", "export const id = 2;"),
        ("src/plugins/nested/deep.ts", "export const id = 3;"),
        (
            "src/loader.test.ts",
            r#"import { plugin } from "./loader";"#,
        ),
    ]);
    let graph = project.link();

    assert_eq!(
        loaded_by(&graph, "src/plugins/telegram.ts"),
        [loaded("src/loader.ts:4", "inferred glob src/plugins/*.js")]
    );
    assert_eq!(loaded_by(&graph, "src/plugins/nested/deep.ts"), []);
    assert_eq!(
        selected_tests(
            &graph,
            &[change("src/plugins/slack.ts", ChangeKind::Modified)]
        ),
        ["src/loader.test.ts"]
    );
    assert_eq!(unresolved_sites(&graph), []);
}

#[test]
fn constants_paths_and_urls_resolve_to_one_file_each() {
    let project = TestProject::new(&[
        (
            "src/entry.ts",
            r#"
                import path from "node:path";
                import { fileURLToPath, pathToFileURL } from "node:url";
                import { RUNTIME } from "./constants";

                const here = path.dirname(fileURLToPath(import.meta.url));

                await import(RUNTIME);
                await import(pathToFileURL(path.join(here, "workers", "pool.js")).href);
                await import(new URL("../node-host.mjs", import.meta.url).href);
                new Worker(new URL("./workers/thread.ts", import.meta.url));
                new Worker("setInterval(() => {}, 1000)", { eval: true });
            "#,
        ),
        (
            "src/constants.ts",
            r#"export const RUNTIME = "./runtime.js";"#,
        ),
        ("src/runtime.ts", "export const runtime = 1;"),
        ("src/workers/pool.ts", "export const pool = 1;"),
        ("src/workers/thread.ts", "export const thread = 1;"),
        ("node-host.mjs", "export const host = 1;"),
    ]);
    let graph = project.link();

    assert_eq!(
        loaded_by(&graph, "src/runtime.ts"),
        [loaded("src/entry.ts:8", "inferred path src/runtime.js")]
    );
    assert_eq!(
        loaded_by(&graph, "src/workers/pool.ts"),
        [loaded(
            "src/entry.ts:9",
            "inferred path src/workers/pool.js"
        )]
    );
    assert_eq!(
        loaded_by(&graph, "node-host.mjs"),
        [loaded("src/entry.ts:10", "inferred path node-host.mjs")]
    );
    assert_eq!(
        loaded_by(&graph, "src/workers/thread.ts"),
        [loaded(
            "src/entry.ts:11",
            "inferred path src/workers/thread.ts"
        )]
    );
    assert_eq!(unresolved_sites(&graph), []);
}

#[test]
fn path_under_an_unknown_root_is_matched_by_its_literal_tail() {
    let project = TestProject::new(&[
        (
            "scripts/run.mts",
            r#"
                import path from "node:path";
                import { pathToFileURL } from "node:url";

                const root = process.cwd();

                await import(pathToFileURL(path.join(root, "src/cli/profile.ts")).href);
            "#,
        ),
        ("src/cli/profile.ts", "export const profile = 1;"),
    ]);
    let graph = project.link();

    assert_eq!(
        loaded_by(&graph, "src/cli/profile.ts"),
        [loaded(
            "scripts/run.mts:7",
            "inferred glob **/src/cli/profile.ts"
        )]
    );
}

#[test]
fn literals_iterated_from_a_constant_array_replace_the_wildcard() {
    let project = TestProject::new(&[
        (
            "src/providers.ts",
            r#"
                const PROVIDERS = ["openai", "anthropic"] as const;

                for (const id of PROVIDERS) {
                    await import(`./providers/${id}.js`);
                }

                export const adapters = PROVIDERS.map((id) => import(`./adapters/${id}.js`));
            "#,
        ),
        ("src/providers/openai.ts", ""),
        ("src/providers/anthropic.ts", ""),
        ("src/providers/mistral.ts", ""),
        ("src/adapters/openai.ts", ""),
        ("src/adapters/mistral.ts", ""),
    ]);
    let graph = project.link();

    assert_eq!(
        loaded_by(&graph, "src/providers/openai.ts"),
        [loaded(
            "src/providers.ts:5",
            "inferred path src/providers/openai.js"
        )]
    );
    assert_eq!(
        loaded_by(&graph, "src/providers/anthropic.ts"),
        [loaded(
            "src/providers.ts:5",
            "inferred path src/providers/anthropic.js"
        )]
    );
    assert_eq!(loaded_by(&graph, "src/providers/mistral.ts"), []);
    assert_eq!(
        loaded_by(&graph, "src/adapters/openai.ts"),
        [loaded(
            "src/providers.ts:8",
            "inferred path src/adapters/openai.js"
        )]
    );
    assert_eq!(loaded_by(&graph, "src/adapters/mistral.ts"), []);
    assert_eq!(
        unresolved_sites(&graph),
        [unresolved(
            "src/providers.ts:8",
            "`./adapters/${id}.js`",
            UnresolvedCause::NoMatchingFile
        )]
    );
}

#[test]
fn spreads_and_destructured_callback_parameters_are_enumerated() {
    let project = TestProject::new(&[
        (
            "src/channels.test.ts",
            r#"
                const CHANNELS = [{ channel: "matrix" }, { channel: "telegram" }];

                export const plugins = [...CHANNELS.map(({ channel }) => channel), "discord"].map(
                    (channel) => import(`../extensions/${channel}/api.js`),
                );
            "#,
        ),
        ("extensions/matrix/api.ts", ""),
        ("extensions/telegram/api.ts", ""),
        ("extensions/discord/api.ts", ""),
        ("extensions/slack/api.ts", ""),
    ]);
    let graph = project.link();

    assert_eq!(
        loaded_by(&graph, "extensions/telegram/api.ts"),
        [loaded(
            "src/channels.test.ts:5",
            "inferred path extensions/telegram/api.js"
        )]
    );
    assert_eq!(
        loaded_by(&graph, "extensions/discord/api.ts"),
        [loaded(
            "src/channels.test.ts:5",
            "inferred path extensions/discord/api.js"
        )]
    );
    assert_eq!(loaded_by(&graph, "extensions/slack/api.ts"), []);
}

#[test]
fn covered_wrapper_leaves_its_runtime_root_to_its_call_sites() {
    let project = TestProject::new(&[
        (
            "scripts/generate.mjs",
            r#"
                import path from "node:path";
                import { pathToFileURL } from "node:url";

                const [checkout] = process.argv.slice(2);
                const load = (name) => import(pathToFileURL(path.join(checkout, "src", name)).href);

                await load("config/sessions.ts");
            "#,
        ),
        ("src/config/sessions.ts", ""),
        ("src/config/other.ts", ""),
    ]);
    let graph = project.link();

    assert_eq!(
        loaded_by(&graph, "src/config/sessions.ts"),
        [loaded(
            "scripts/generate.mjs:8",
            "inferred glob **/src/config/sessions.ts"
        )]
    );
    assert_eq!(loaded_by(&graph, "src/config/other.ts"), []);
    assert_eq!(unresolved_sites(&graph), []);
}

#[test]
fn loader_wrapper_resolves_at_each_call_site_through_a_barrel() {
    let project = TestProject::new(&[
        (
            "src/shared/import-specifier.ts",
            r#"
                import { pathToFileURL } from "node:url";

                export function toSafeImportPath(specifier: string): string {
                    if (/^[A-Za-z]:[\\/]/.test(specifier)) {
                        return pathToFileURL(specifier).href;
                    }

                    return specifier;
                }
            "#,
        ),
        (
            "src/shared/runtime-import.ts",
            r#"
                import { toSafeImportPath } from "./import-specifier.js";

                export async function importRuntimeModule<T>(
                    baseUrl: string,
                    parts: readonly string[],
                    importModule: (specifier: string) => Promise<unknown> = (specifier) => import(specifier),
                ): Promise<T> {
                    const joined = parts.join("");
                    const safeJoined = toSafeImportPath(joined);
                    const specifier =
                        safeJoined !== joined ? safeJoined : new URL(joined, toSafeImportPath(baseUrl)).href;

                    return (await importModule(specifier)) as T;
                }
            "#,
        ),
        (
            "src/shared/index.ts",
            r#"export { importRuntimeModule } from "./runtime-import.js";"#,
        ),
        (
            "src/agents/registry.ts",
            r#"
                import { importRuntimeModule } from "../shared/index.js";

                export const loadRuntime = () =>
                    importRuntimeModule(import.meta.url, ["./registry.runtime", ".js"]);
            "#,
        ),
        (
            "src/agents/registry.runtime.ts",
            "export const runtime = 1;",
        ),
        (
            "src/agents/registry.test.ts",
            r#"import { loadRuntime } from "./registry";"#,
        ),
        (
            "src/shared/runtime-import.test.ts",
            r#"
                import { importRuntimeModule } from "./runtime-import";

                const importModule = vi.fn(async () => ({}));

                await importRuntimeModule("file:///repo/src/a.js", ["./b", ".js"], importModule);
            "#,
        ),
    ]);
    let graph = project.link();

    assert_eq!(
        loaded_by(&graph, "src/agents/registry.runtime.ts"),
        [loaded(
            "src/agents/registry.ts:5",
            "inferred path src/agents/registry.runtime.js"
        )]
    );
    assert_eq!(
        selected_tests(
            &graph,
            &[change(
                "src/agents/registry.runtime.ts",
                ChangeKind::Modified
            )]
        ),
        ["src/agents/registry.test.ts"]
    );
    assert_eq!(unresolved_sites(&graph), []);
}

#[test]
fn loader_wrapper_used_as_a_value_leaves_its_site_unresolved() {
    let project = TestProject::new(&[
        (
            "src/load.ts",
            r#"
                export function load(specifier: string) {
                    return import(specifier);
                }

                export const loaders = [load];
            "#,
        ),
        (
            "src/app.ts",
            r#"
                import { load } from "./load";

                await load("./plugins/telegram.js");
            "#,
        ),
        ("src/plugins/telegram.ts", ""),
    ]);
    let graph = project.link();

    assert_eq!(
        unresolved_sites(&graph),
        [unresolved(
            "src/load.ts:3",
            "specifier",
            UnresolvedCause::Parameter
        )]
    );
}

#[test]
fn require_factories_resolve_from_their_base() {
    let project = TestProject::new(&[
        (
            "scripts/build.mjs",
            r#"
                import path from "node:path";
                import { createRequire } from "node:module";

                const localRequire = createRequire(import.meta.url);
                const rootRequire = createRequire(path.join(process.cwd(), "package.json"));

                export const config = localRequire("./config.json");
                export const plan = rootRequire("./scripts/lib/plan.cjs");
            "#,
        ),
        ("scripts/config.json", "{}"),
        ("scripts/lib/plan.cjs", "module.exports = {};"),
    ]);
    let graph = project.link();

    assert_eq!(
        loaded_by(&graph, "scripts/config.json"),
        [loaded(
            "scripts/build.mjs:8",
            "inferred path scripts/config.json"
        )]
    );
    assert_eq!(
        loaded_by(&graph, "scripts/lib/plan.cjs"),
        [loaded(
            "scripts/build.mjs:9",
            "inferred glob **/scripts/lib/plan.cjs"
        )]
    );
}

#[test]
fn plugin_folders_with_a_manifest_link_every_file_of_the_folder() {
    let project = TestProject::new(&[
        (
            "src/plugins.ts",
            r#"
                const id = process.env.PLUGIN;

                export const plugin = await import(`../extensions/${id}/index.js`);
            "#,
        ),
        ("extensions/telegram/index.ts", "export const channel = 1;"),
        ("extensions/telegram/package.json", "{}"),
        ("extensions/telegram/prompts/system.md", "You are a bot."),
        ("extensions/telegram/channel.test.ts", ""),
        ("extensions/slack/index.ts", "export const channel = 2;"),
        ("extensions/slack/slack.plugin.json", "{}"),
        (
            "src/plugins.test.ts",
            r#"import { plugin } from "./plugins";"#,
        ),
    ]);
    let graph = project.link();

    assert_eq!(
        loaded_by(&graph, "extensions/telegram"),
        [loaded(
            "src/plugins.ts:4",
            "inferred plugin folder extensions/{id}/ (id=telegram)"
        )]
    );
    assert_eq!(
        loaded_by(&graph, "extensions/slack"),
        [loaded(
            "src/plugins.ts:4",
            "inferred plugin folder extensions/{id}/ (id=slack)"
        )]
    );
    assert_eq!(
        dependents(&graph, "extensions/telegram/prompts/system.md"),
        [("extensions/telegram", EdgeKind::Contains)]
    );
    assert_eq!(
        dependents(&graph, "extensions/telegram/channel.test.ts"),
        []
    );
    assert_eq!(
        selected_tests(
            &graph,
            &[change(
                "extensions/telegram/prompts/system.md",
                ChangeKind::Modified
            )]
        ),
        ["src/plugins.test.ts"]
    );

    let without_plugin_folders = project.link_with(InferenceSettings {
        plugin_folders: false,
        ..InferenceSettings::default()
    });

    assert_eq!(
        loaded_by(&without_plugin_folders, "extensions/telegram"),
        []
    );
    assert_eq!(
        dependents(
            &without_plugin_folders,
            "extensions/telegram/prompts/system.md"
        ),
        []
    );
    assert_eq!(
        loaded_by(&without_plugin_folders, "extensions/telegram/index.ts"),
        [loaded(
            "src/plugins.ts:4",
            "inferred glob extensions/*/index.js"
        )]
    );
}

#[test]
fn folders_without_a_manifest_link_only_the_loaded_files() {
    let project = TestProject::new(&[
        (
            "src/plugins.ts",
            r#"
                const id = process.env.PLUGIN;

                export const plugin = await import(`../extensions/${id}/index.js`);
            "#,
        ),
        ("extensions/telegram/index.ts", ""),
        ("extensions/telegram/package.json", "{}"),
        ("extensions/telegram/README.md", ""),
        ("extensions/slack/index.ts", ""),
    ]);
    let graph = project.link();

    assert_eq!(loaded_by(&graph, "extensions/telegram/README.md"), []);
    assert_eq!(
        loaded_by(&graph, "extensions/slack/index.ts"),
        [loaded(
            "src/plugins.ts:4",
            "inferred glob extensions/*/index.js"
        )]
    );
}

#[test]
fn loads_that_cannot_be_known_are_unresolved_with_their_cause() {
    let project = TestProject::new(&[(
        "src/worker.ts",
        r#"
                import path from "node:path";

                export async function handle(request: { moduleUrl: string }) {
                    return import(request.moduleUrl);
                }

                parentPort.on("message", handle);

                const fetched = await fetchModuleUrl();

                await import(fetched);
                await import(path.join(__dirname, "../dist/index.js"));
                await import(`${process.cwd()}/${process.env.NAME}.js`);
                await import("sqlite-vec");
                await import(process.env.OPTIONAL ? "sqlite-vec" : "data:text/javascript,1");
            "#,
    )]);
    let graph = project.link();

    assert_eq!(
        unresolved_sites(&graph),
        [
            unresolved(
                "src/worker.ts:5",
                "request.moduleUrl",
                UnresolvedCause::Parameter
            ),
            unresolved("src/worker.ts:12", "fetched", UnresolvedCause::RuntimeValue),
            unresolved(
                "src/worker.ts:13",
                r#"path.join(__dirname, "../dist/index.js")"#,
                UnresolvedCause::NoMatchingFile
            ),
            unresolved(
                "src/worker.ts:14",
                "`${process.cwd()}/${process.env.NAME}.js`",
                UnresolvedCause::TooWide
            ),
        ]
    );
}

#[test]
fn test_support_triggers_only_the_unresolved_sites_of_test_code() {
    let project = TestProject::new(&[
        (
            "src/loader.ts",
            "export const plugin = await import(process.env.PLUGIN_ENTRY!);\n",
        ),
        (
            "src/loader.test.ts",
            "import { plugin } from \"./loader\";\n",
        ),
        ("src/a.ts", "export const a = 1;\n"),
        (
            "src/a.test-support.ts",
            "import { env } from \"../test/helpers/env\";\n\nexport const expected = env;\n",
        ),
        ("test/helpers/env.ts", "export const env = 1;\n"),
        (
            "src/a.test.ts",
            "import { a } from \"./a\";\nimport { expected } from \"./a.test-support\";\n",
        ),
        ("src/shared.test-support.ts", "export const shared = 1;\n"),
        (
            "src/b.ts",
            "import { shared } from \"./shared.test-support\";\n\nexport const b = shared;\n",
        ),
        (
            "test/fixtures/plugin/index.ts",
            "export const plugin = 1;\n",
        ),
        (
            "src/fixture.test.ts",
            "import { plugin } from \"../test/fixtures/plugin/index\";\n",
        ),
        ("extensions/telegram/index.ts", "export const id = 1;\n"),
        (
            "extensions/telegram/index.test.ts",
            "import { id } from \"./index\";\n",
        ),
        (
            "test/helpers/load-fixture.ts",
            "export const fixture = await import(process.env.FIXTURE!);\n",
        ),
        (
            "src/fixture-loader.test.ts",
            "import { fixture } from \"../test/helpers/load-fixture\";\n",
        ),
    ]);
    let graph = project.link();
    let mut test_support: Vec<&str> = graph
        .test_support_files()
        .map(|file| graph.path(file).as_str())
        .collect();
    let selected = |path: &str| selected_tests(&graph, &[change(path, ChangeKind::Modified)]);

    test_support.sort_unstable();

    assert_eq!(
        test_support,
        [
            "src/a.test-support.ts",
            "test/helpers/env.ts",
            "test/helpers/load-fixture.ts"
        ]
    );
    assert_eq!(
        selected("test/helpers/env.ts"),
        ["src/a.test.ts", "src/fixture-loader.test.ts"]
    );
    assert_eq!(
        selected("src/shared.test-support.ts"),
        ["src/fixture-loader.test.ts", "src/loader.test.ts"]
    );
    assert_eq!(
        selected("test/fixtures/plugin/index.ts"),
        [
            "src/fixture.test.ts",
            "src/fixture-loader.test.ts",
            "src/loader.test.ts"
        ]
    );
    assert_eq!(
        selected("extensions/telegram/index.ts"),
        [
            "extensions/telegram/index.test.ts",
            "src/fixture-loader.test.ts",
            "src/loader.test.ts"
        ]
    );

    let without_test_support = project.link_with(InferenceSettings {
        test_support: false,
        ..InferenceSettings::default()
    });

    assert_eq!(without_test_support.test_support_files().count(), 0);
    assert_eq!(
        selected_tests(
            &without_test_support,
            &[change("test/helpers/env.ts", ChangeKind::Modified)]
        ),
        [
            "src/a.test.ts",
            "src/fixture-loader.test.ts",
            "src/loader.test.ts"
        ]
    );
}

#[test]
fn load_of_a_missing_file_links_the_paths_it_names_without_widening() {
    let project = TestProject::new(&[
        (
            "src/loader.ts",
            r#"
                import path from "node:path";

                export const runtime = await import(path.join(__dirname, "runtime.js"));
            "#,
        ),
        (
            "src/loader.test.ts",
            r#"import { runtime } from "./loader";"#,
        ),
        ("src/other.ts", ""),
    ]);
    let graph = project.link();

    assert_eq!(
        loaded_by(&graph, "src/runtime.ts"),
        [loaded("src/loader.ts:4", "inferred path src/runtime.js")]
    );
    assert_eq!(
        selected_tests(&graph, &[change("src/runtime.ts", ChangeKind::Deleted)]),
        ["src/loader.test.ts"]
    );
    assert_eq!(
        selected_tests(&graph, &[change("src/other.ts", ChangeKind::Modified)]),
        Vec::<String>::new()
    );
    assert_eq!(
        unresolved_sites(&graph),
        [unresolved(
            "src/loader.ts:4",
            r#"path.join(__dirname, "runtime.js")"#,
            UnresolvedCause::NoMatchingFile
        )]
    );
}

#[test]
fn disabled_inference_leaves_every_dynamic_load_unresolved() {
    let project = TestProject::new(&[
        (
            "src/loader.ts",
            r#"
                import { helper } from "./helper";

                const name = process.env.PLUGIN;

                await import(`./plugins/${name}.js`);
                new Worker(new URL("./thread.ts", import.meta.url));
                helper(name);
            "#,
        ),
        (
            "src/helper.ts",
            "export const helper = (value: string) => value;",
        ),
        ("src/plugins/telegram.ts", ""),
        ("src/thread.ts", ""),
    ]);
    let graph = project.link_with(InferenceSettings {
        dynamic_imports: false,
        ..InferenceSettings::default()
    });

    assert_eq!(loaded_by(&graph, "src/plugins/telegram.ts"), []);
    assert_eq!(
        unresolved_sites(&graph),
        [
            unresolved(
                "src/loader.ts:6",
                "`./plugins/${name}.js`",
                UnresolvedCause::InferenceDisabled
            ),
            unresolved(
                "src/loader.ts:7",
                r#"new URL("./thread.ts", import.meta.url)"#,
                UnresolvedCause::InferenceDisabled
            ),
        ]
    );
    assert_eq!(
        graph
            .dependents(graph.file_id(&repo_path("src/helper.ts")).unwrap())
            .count(),
        1
    );
}
