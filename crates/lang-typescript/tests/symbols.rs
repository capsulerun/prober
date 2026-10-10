//! Symbols linked across small TypeScript projects written to disk: barrels, namespaces, module loads, whole-module uses and import cycles.

mod test_project;

use prober_core::graph::Graph;
use prober_core::scan::{ScanReport, SymbolCycle, SymbolCycles};
use prober_core::symbols::{FallbackCause, MODULE_INIT_NAME, SymbolTarget};

use crate::test_project::{TestProject, repo_path};

fn uses_of(graph: &Graph, path: &str, name: &str) -> Vec<String> {
    let file = graph.file_id(&repo_path(path)).unwrap();
    let symbol = graph
        .symbols_of(file)
        .iter()
        .copied()
        .find(|&symbol| graph.symbol(symbol).name == name)
        .unwrap_or_else(|| panic!("{name} is not a symbol of {path}"));
    let mut uses: Vec<String> = graph
        .symbol_dependencies(symbol)
        .iter()
        .map(|edge| match edge.dependency {
            SymbolTarget::Symbol(dependency) => {
                let dependency = graph.symbol(dependency);

                format!("{}#{}", graph.path(dependency.file), dependency.name)
            }
            SymbolTarget::Module { file, cause } => {
                format!("{} ({})", graph.path(file), cause.key())
            }
        })
        .collect();

    uses.sort();

    uses
}

#[test]
fn imported_names_resolve_through_barrels_aliases_and_star_exports() {
    let project = TestProject::new(&[
        (
            "src/app.ts",
            r#"
                import { start, Channel as Line, helper, missing } from "./index";
                import main from "./index";

                export function run() {
                    return [start(), new Line(), helper(), missing, main()];
                }
            "#,
        ),
        (
            "src/index.ts",
            r#"
                export { start } from "./server";
                export * from "./channel";
                export * from "./helpers";
                import main from "./main";
                export default main;
            "#,
        ),
        (
            "src/server.ts",
            "export function start() { return 1; }\nexport function stop() { return 0; }",
        ),
        ("src/channel.ts", "export class Channel {}"),
        ("src/helpers.ts", "export * from \"./deep\";"),
        ("src/deep.ts", "export const helper = () => 1;"),
        (
            "src/main.ts",
            "export default function main() { return 1; }",
        ),
    ]);
    let graph = project.link_symbols();

    assert_eq!(
        uses_of(&graph, "src/app.ts", "run"),
        [
            "src/channel.ts (missing_export)",
            "src/channel.ts#Channel",
            "src/deep.ts (missing_export)",
            "src/deep.ts#helper",
            "src/helpers.ts (missing_export)",
            "src/index.ts (missing_export)",
            "src/main.ts#main",
            "src/server.ts#start",
        ]
    );
    assert_eq!(
        uses_of(&graph, "src/app.ts", MODULE_INIT_NAME),
        ["src/index.ts#<module>"]
    );
    assert_eq!(
        uses_of(&graph, "src/index.ts", MODULE_INIT_NAME),
        [
            "src/channel.ts#<module>",
            "src/helpers.ts#<module>",
            "src/main.ts#<module>",
            "src/server.ts#<module>",
        ]
    );
}

#[test]
fn namespace_imports_narrow_to_the_members_read_even_through_a_barrel() {
    let project = TestProject::new(&[
        (
            "src/app.ts",
            r#"
                import * as utils from "./utils";
                import { tools } from "./barrel";

                export const formatted = () => utils.format();
                export const listed = () => Object.keys(utils);
                export const parsed = () => tools.parse();
            "#,
        ),
        (
            "src/utils.ts",
            "export const format = () => 1;\nexport const parse = () => 2;",
        ),
        ("src/barrel.ts", "export * as tools from \"./utils\";"),
    ]);
    let graph = project.link_symbols();

    assert_eq!(
        uses_of(&graph, "src/app.ts", "formatted"),
        ["src/utils.ts#format"]
    );
    assert_eq!(
        uses_of(&graph, "src/app.ts", "listed"),
        ["src/utils.ts (escaping_namespace)"]
    );
    assert_eq!(
        uses_of(&graph, "src/app.ts", "parsed"),
        ["src/utils.ts#parse"]
    );
}

#[test]
fn modules_outside_the_analysis_and_dynamic_loads_are_used_whole() {
    let project = TestProject::new(&[
        (
            "src/app.ts",
            r#"
                import legacy from "./legacy.cjs";
                import { z } from "zod";
                import data from "./data.json";
                import unused from "./unused.json";

                export const fromLegacy = () => legacy.x;
                export const schema = () => z.string();
                export const read = () => data;
                export async function load() { return import("./lazy"); }
            "#,
        ),
        ("src/legacy.cjs", "module.exports = { x: 1 };"),
        ("src/data.json", "{}"),
        ("src/unused.json", "{}"),
        ("src/lazy.ts", "export const lazy = 1;"),
    ]);
    let graph = project.link_symbols();
    let legacy = graph.file_id(&repo_path("src/legacy.cjs")).unwrap();

    assert_eq!(
        uses_of(&graph, "src/app.ts", "fromLegacy"),
        ["src/legacy.cjs (not_analysed)"]
    );
    assert_eq!(
        uses_of(&graph, "src/app.ts", "schema"),
        ["node_modules/zod (not_analysed)"]
    );
    assert_eq!(
        uses_of(&graph, "src/app.ts", "read"),
        ["src/data.json (not_analysed)"]
    );
    assert_eq!(
        uses_of(&graph, "src/app.ts", "load"),
        ["src/lazy.ts (dynamic_load)"]
    );
    assert_eq!(
        uses_of(&graph, "src/app.ts", MODULE_INIT_NAME),
        [
            "node_modules/zod (not_analysed)",
            "src/legacy.cjs (not_analysed)",
            "src/unused.json (unattributed)",
        ]
    );
    assert_eq!(graph.symbols_of(legacy), []);
    assert_eq!(
        graph
            .fallbacks()
            .iter()
            .map(|fallback| (graph.path(fallback.file).as_str(), fallback.cause))
            .collect::<Vec<_>>(),
        [("src/legacy.cjs", FallbackCause::CommonJs)]
    );
}

#[test]
fn symbols_are_analysed_only_when_asked() {
    let project = TestProject::new(&[("src/a.ts", "export const a = 1;")]);

    assert_eq!(project.link().symbol_count(), 0);
    assert_eq!(project.link_symbols().symbol_count(), 2);
}

fn symbol_cycles(project: &TestProject) -> Vec<(Vec<String>, Option<SymbolCycles>)> {
    let graph = project.link_symbols();
    let scan_report = ScanReport::new(&graph, &project.repo_files(), String::new());

    scan_report
        .import_cycles
        .into_iter()
        .map(|import_cycle| {
            let modules = import_cycle
                .modules
                .iter()
                .map(|module| module.as_str().to_owned())
                .collect();

            (modules, import_cycle.symbols)
        })
        .collect()
}

#[test]
fn a_file_level_cycle_without_symbol_cycle_keeps_only_its_module_loads() {
    let project = TestProject::new(&[
        (
            "src/a.ts",
            "import { b } from \"./b\";\nexport function a() { return b(); }\nexport function helper() { return 1; }",
        ),
        (
            "src/b.ts",
            "import { helper } from \"./a\";\nexport function b() { return 2; }\nexport function viaA() { return helper(); }",
        ),
    ]);

    assert_eq!(
        symbol_cycles(&project),
        [(
            vec!["src/a.ts".to_owned(), "src/b.ts".to_owned()],
            Some(SymbolCycles {
                symbols: 6,
                cycles: vec![SymbolCycle {
                    symbols: 2,
                    modules: 2,
                    tests: 0,
                    leading_in: 2,
                }],
                file_level: Vec::new(),
            })
        )]
    );
}

#[test]
fn functions_calling_each_other_and_load_time_calls_keep_the_cycle() {
    let project = TestProject::new(&[
        (
            "src/x.ts",
            "import { y } from \"./y\";\nexport function x() { return y(); }\nx();",
        ),
        (
            "src/y.ts",
            "import { x } from \"./x\";\nexport function y() { return x(); }",
        ),
    ]);

    assert_eq!(
        symbol_cycles(&project),
        [(
            vec!["src/x.ts".to_owned(), "src/y.ts".to_owned()],
            Some(SymbolCycles {
                symbols: 6,
                cycles: vec![
                    SymbolCycle {
                        symbols: 2,
                        modules: 2,
                        tests: 0,
                        leading_in: 4,
                    },
                    SymbolCycle {
                        symbols: 2,
                        modules: 2,
                        tests: 0,
                        leading_in: 2,
                    },
                    SymbolCycle {
                        symbols: 2,
                        modules: 2,
                        tests: 0,
                        leading_in: 2,
                    },
                ],
                file_level: Vec::new(),
            })
        )]
    );
}

#[test]
fn a_commonjs_module_in_a_cycle_stays_at_file_level_and_keeps_its_importers_whole() {
    let project = TestProject::new(&[
        (
            "src/p.ts",
            "import legacy from \"./legacy.cjs\";\nexport function p() { return legacy(); }\nexport function q() { return 1; }",
        ),
        (
            "src/legacy.cjs",
            "const { q } = require(\"./p\");\nmodule.exports = () => q();",
        ),
    ]);

    assert_eq!(
        symbol_cycles(&project),
        [(
            vec!["src/legacy.cjs".to_owned(), "src/p.ts".to_owned()],
            Some(SymbolCycles {
                symbols: 3,
                cycles: vec![SymbolCycle {
                    symbols: 2,
                    modules: 2,
                    tests: 0,
                    leading_in: 3,
                }],
                file_level: vec![(repo_path("src/legacy.cjs"), FallbackCause::CommonJs)],
            })
        )]
    );
}

#[test]
fn callbacks_handed_to_functions_that_only_keep_them_stay_out_of_module_initialisation() {
    let project = TestProject::new(&[
        (
            "src/lazy.ts",
            r#"
                import { remember } from "./memory";

                export function createLazyRuntimeModule(load: () => Promise<unknown>) {
                    return remember(() => load());
                }

                export function eager(create: () => unknown) {
                    return create();
                }
            "#,
        ),
        (
            "src/memory.ts",
            r#"
                export function remember<T>(create: () => T) {
                    let cached: T | undefined;
                    return () => (cached ??= create());
                }
            "#,
        ),
        (
            "src/app.ts",
            r#"
                import { createLazyRuntimeModule, eager } from "./lazy";

                export const loadRuntime = createLazyRuntimeModule(() => import("./runtime"));
                export const now = eager(() => import("./other"));
            "#,
        ),
        ("src/runtime.ts", "export const runtime = 1;"),
        ("src/other.ts", "export const other = 1;"),
    ]);
    let graph = project.link_symbols();

    assert_eq!(
        uses_of(&graph, "src/app.ts", MODULE_INIT_NAME),
        [
            "src/lazy.ts#<module>",
            "src/lazy.ts#createLazyRuntimeModule()",
            "src/lazy.ts#eager",
            "src/other.ts (dynamic_load)",
        ]
    );
    assert_eq!(
        uses_of(&graph, "src/app.ts", "loadRuntime"),
        [
            "src/lazy.ts#createLazyRuntimeModule",
            "src/runtime.ts (dynamic_load)",
        ]
    );
}

#[test]
fn members_resolve_through_imports_instances_bases_and_overrides() {
    let project = TestProject::new(&[
        (
            "src/base.ts",
            r#"
                export class Base {
                    describe() { return this.label(); }
                    label() { return "base"; }
                    static create() { return new Base(); }
                }
            "#,
        ),
        (
            "src/square.ts",
            r#"
                import { Base } from "./base";

                export class Square extends Base {
                    label() { return "square"; }
                    area() { return super.label(); }
                }
            "#,
        ),
        (
            "src/app.ts",
            r#"
                import { Base } from "./base";
                import { Square } from "./square";

                const square = new Square();

                export const describe = () => square.describe();
                export const area = () => square.area();
                export const created = () => Base.create();
            "#,
        ),
    ]);
    let graph = project.link_symbols();

    assert_eq!(
        uses_of(&graph, "src/app.ts", "describe"),
        ["src/base.ts#Base.describe"]
    );
    assert_eq!(
        uses_of(&graph, "src/app.ts", "area"),
        ["src/square.ts#Square.area"]
    );
    assert_eq!(
        uses_of(&graph, "src/app.ts", "created"),
        ["src/base.ts#static Base.create"]
    );
    assert_eq!(
        uses_of(&graph, "src/base.ts", "Base.describe"),
        ["src/base.ts#Base.label", "src/square.ts#Square.label"]
    );
    assert_eq!(
        uses_of(&graph, "src/square.ts", "Square.area"),
        ["src/base.ts#Base.label"]
    );
    assert_eq!(
        uses_of(&graph, "src/square.ts", "Square"),
        [
            "src/base.ts#Base",
            "src/square.ts#Square.area",
            "src/square.ts#Square.label"
        ]
    );
    assert_eq!(
        uses_of(&graph, "src/app.ts", MODULE_INIT_NAME),
        [
            "src/base.ts#<module>",
            "src/square.ts#<module>",
            "src/square.ts#new Square()"
        ]
    );
    assert_eq!(
        uses_of(&graph, "src/square.ts", "new Square()"),
        ["src/base.ts#new Base()"]
    );
}

#[test]
fn options_handed_to_factories_and_constructors_that_keep_them_stay_out_of_load() {
    let project = TestProject::new(&[
        (
            "src/runtime.ts",
            r#"
                export function createRuntime({ load, label }: { load: () => Promise<unknown>; label: string }) {
                    const { retries } = { retries: label.length };
                    return { start: async () => (await load(), retries) };
                }

                export class Controller {
                    constructor(readonly options: { run: () => Promise<unknown> }) {}
                    start() { return this.options.run(); }
                }
            "#,
        ),
        (
            "src/app.ts",
            r#"
                import { createRuntime, Controller } from "./runtime";

                export const runtime = createRuntime({ load: () => import("./plugin"), label: "app" });
                export const controller = new Controller({ run: () => import("./worker") });
            "#,
        ),
        ("src/plugin.ts", "export const plugin = 1;"),
        ("src/worker.ts", "export const worker = 1;"),
    ]);
    let graph = project.link_symbols();

    assert_eq!(
        uses_of(&graph, "src/app.ts", MODULE_INIT_NAME),
        [
            "src/runtime.ts#<module>",
            "src/runtime.ts#createRuntime()",
            "src/runtime.ts#new Controller()"
        ]
    );
    assert_eq!(
        uses_of(&graph, "src/app.ts", "runtime"),
        [
            "src/plugin.ts (dynamic_load)",
            "src/runtime.ts#createRuntime"
        ]
    );
}
