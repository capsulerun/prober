//! Selection from the symbols that changed between the base revision and head, on small TypeScript projects written to disk.

mod test_project;

use crate::test_project::{TestProject, modified, selected};

const HUB: &str = "export function parse(text: string) {\n  return text.trim();\n}\n\nexport function format(value: string) {\n  return `[${value}]`;\n}\n";

const BASE_HUB: &str = "export function parse(text: string) {\n  return text;\n}\n\nexport function format(value: string) {\n  return `[${value}]`;\n}\n";

fn hub_project(hub_path: &str, extra_files: &[(&str, &str)]) -> TestProject {
    let reader = format!(
        "import {{ parse }} from \"{hub_path}\";\n\nexport const read = (text: string) => parse(text);\n"
    );
    let writer = format!(
        "import {{ format }} from \"{hub_path}\";\n\nexport const write = (value: string) => format(value);\n"
    );
    let mut files = vec![
        ("src/hub.ts", HUB),
        ("src/reader.ts", reader.as_str()),
        ("src/writer.ts", writer.as_str()),
        (
            "src/reader.test.ts",
            "import { read } from \"./reader\";\n\ntest(\"read\", () => expect(read(\" a \")).toBe(\"a\"));\n",
        ),
        (
            "src/writer.test.ts",
            "import { write } from \"./writer\";\n\ntest(\"write\", () => expect(write(\"a\")).toBe(\"[a]\"));\n",
        ),
    ];

    files.extend_from_slice(extra_files);

    TestProject::new(&files)
}

#[test]
fn change_to_one_export_of_a_hub_selects_only_the_tests_of_its_users() {
    let project = hub_project("./hub", &[]);
    let changes = [modified("src/hub.ts")];

    assert_eq!(
        project.symbol_outcome_of(&[("src/hub.ts", BASE_HUB)], &changes),
        selected(&[(
            "src/reader.test.ts",
            &[
                "src/hub.ts (modified)",
                "-> parse changed",
                "-> parse used by read in src/reader.ts",
                "-> read used at load by src/reader.test.ts",
            ]
        )])
    );
    assert_eq!(
        project
            .outcome_of(&[("src/hub.ts", BASE_HUB)], &changes)
            .selected_paths(),
        ["src/reader.test.ts", "src/writer.test.ts"]
    );
}

#[test]
fn change_to_comments_or_formatting_selects_nothing() {
    let project = hub_project("./hub", &[]);
    let base_hub = "// Parses and formats values.\nexport function parse(text: string) { return text.trim(); }\n\nexport function format(value: string) {\n  // Brackets.\n  return `[${value}]`;\n}\n";

    assert_eq!(
        project.symbol_outcome_of(&[("src/hub.ts", base_hub)], &[modified("src/hub.ts")]),
        selected(&[])
    );
}

#[test]
fn top_level_side_effect_selects_every_importer() {
    let head_hub = format!("{HUB}\nconsole.log(\"hub loaded\");\n");
    let project = hub_project("./hub", &[("src/hub.ts", &head_hub)]);

    assert_eq!(
        project.symbol_outcome_of(&[("src/hub.ts", HUB)], &[modified("src/hub.ts")]),
        selected(&[
            (
                "src/reader.test.ts",
                &[
                    "src/hub.ts (modified)",
                    "-> code run at load changed",
                    "-> loaded by src/reader.ts",
                    "-> loaded by src/reader.test.ts",
                ]
            ),
            (
                "src/writer.test.ts",
                &[
                    "src/hub.ts (modified)",
                    "-> code run at load changed",
                    "-> loaded by src/writer.ts",
                    "-> loaded by src/writer.test.ts",
                ]
            ),
        ])
    );
}

#[test]
fn barrels_and_namespaces_pass_the_change_only_to_the_names_read() {
    let project = hub_project(
        "./index",
        &[
            ("src/index.ts", "export * from \"./hub\";\n"),
            (
                "src/reader.ts",
                "import * as hub from \"./index\";\n\nexport const read = (text: string) => hub.parse(text);\n",
            ),
        ],
    );

    assert_eq!(
        project
            .symbol_outcome_of(&[("src/hub.ts", BASE_HUB)], &[modified("src/hub.ts")])
            .selected_paths(),
        ["src/reader.test.ts"]
    );
}

#[test]
fn removed_symbol_reaches_the_users_left_behind_through_any_barrel() {
    let base_hub = format!("{HUB}\nexport function legacy() {{\n  return 0;\n}}\n");
    let project = hub_project(
        "./hub",
        &[
            ("src/index.ts", "export * from \"./hub\";\n"),
            (
                "src/old.ts",
                "import { legacy } from \"./index\";\n\nexport const runOld = () => legacy();\n",
            ),
            (
                "src/old.test.ts",
                "import { runOld } from \"./old\";\n\ntest(\"old\", () => runOld());\n",
            ),
        ],
    );

    assert_eq!(
        project.symbol_outcome_of(&[("src/hub.ts", &base_hub)], &[modified("src/hub.ts")]),
        selected(&[(
            "src/old.test.ts",
            &[
                "src/hub.ts (modified)",
                "-> used whole by runOld in src/old.ts (imported name not found)",
                "-> runOld used at load by src/old.test.ts",
            ]
        )])
    );
}

#[test]
fn renamed_symbol_reaches_its_users_at_head_and_those_left_behind() {
    let base_hub = HUB.replace("function parse(", "function parseText(");
    let base_reader = "import { parseText } from \"./hub\";\n\nexport const read = (text: string) => parseText(text);\n";
    let project = hub_project(
        "./hub",
        &[
            (
                "src/old.ts",
                "import { parseText } from \"./hub\";\n\nexport const runOld = (text: string) => parseText(text);\n",
            ),
            (
                "src/old.test.ts",
                "import { runOld } from \"./old\";\n\ntest(\"old\", () => runOld(\"a\"));\n",
            ),
        ],
    );
    let outcome = project.symbol_outcome_of(
        &[("src/hub.ts", &base_hub), ("src/reader.ts", base_reader)],
        &[modified("src/hub.ts"), modified("src/reader.ts")],
    );

    assert_eq!(
        outcome,
        selected(&[
            (
                "src/reader.test.ts",
                &[
                    "src/reader.ts (modified)",
                    "-> read changed",
                    "-> read used at load by src/reader.test.ts",
                ]
            ),
            (
                "src/old.test.ts",
                &[
                    "src/hub.ts (modified)",
                    "-> used whole by runOld in src/old.ts (imported name not found)",
                    "-> runOld used at load by src/old.test.ts",
                ]
            ),
        ])
    );
    assert_eq!(
        project
            .symbol_outcome_of(&[("src/hub.ts", &base_hub)], &[modified("src/hub.ts")])
            .selected_paths(),
        ["src/old.test.ts", "src/reader.test.ts"]
    );
}

#[test]
fn changed_re_export_reaches_the_users_of_the_name_it_now_points_to() {
    let base_index =
        "export { parse, format } from \"./hub\";\nexport { strictParse } from \"./strict\";\n";
    let project = hub_project(
        "./index",
        &[
            (
                "src/index.ts",
                "export { format } from \"./hub\";\nexport { parse, strictParse } from \"./strict\";\n",
            ),
            (
                "src/strict.ts",
                "export function parse(text: string) {\n  return text.trim().toLowerCase();\n}\n\nexport const strictParse = parse;\n",
            ),
        ],
    );

    assert_eq!(
        project.symbol_outcome_of(&[("src/index.ts", base_index)], &[modified("src/index.ts")]),
        selected(&[(
            "src/reader.test.ts",
            &[
                "src/index.ts (modified)",
                "-> export parse changed",
                "-> parse used by read in src/reader.ts",
                "-> read used at load by src/reader.test.ts",
            ]
        )])
    );
}

#[test]
fn module_outside_the_analysis_is_changed_whole() {
    let project = hub_project(
        "./hub",
        &[
            (
                "src/legacy.js",
                "module.exports = { trim: (text) => text.trim() };\n",
            ),
            (
                "src/reader.ts",
                "import { trim } from \"./legacy.js\";\n\nexport const read = (text: string) => trim(text);\n",
            ),
        ],
    );

    assert_eq!(
        project.symbol_outcome_of(&[], &[modified("src/legacy.js")]),
        selected(&[(
            "src/reader.test.ts",
            &[
                "src/legacy.js (modified)",
                "-> used whole at load by src/reader.ts (analysed at file level: CommonJS exports)",
                "-> loaded by src/reader.test.ts",
            ]
        )])
    );
}

#[test]
fn import_cycle_between_files_does_not_carry_a_change_to_the_symbols_outside_it() {
    let project = TestProject::new(&[
        (
            "src/a.ts",
            "import { b } from \"./b\";\n\nexport function useB() {\n  return b();\n}\n\nexport function standalone() {\n  return 1;\n}\n",
        ),
        (
            "src/b.ts",
            "import { standalone } from \"./a\";\n\nexport function b() {\n  return 2;\n}\n\nexport function useA() {\n  return standalone();\n}\n",
        ),
        (
            "src/use-b.test.ts",
            "import { useB } from \"./a\";\n\ntest(\"useB\", () => useB());\n",
        ),
        (
            "src/standalone.test.ts",
            "import { standalone } from \"./a\";\n\ntest(\"standalone\", () => standalone());\n",
        ),
    ]);
    let base_b = "import { standalone } from \"./a\";\n\nexport function b() {\n  return 1;\n}\n\nexport function useA() {\n  return standalone();\n}\n";
    let changes = [modified("src/b.ts")];

    assert_eq!(
        project
            .symbol_outcome_of(&[("src/b.ts", base_b)], &changes)
            .selected_paths(),
        ["src/use-b.test.ts"]
    );
    assert_eq!(
        project
            .outcome_of(&[("src/b.ts", base_b)], &changes)
            .selected_paths(),
        ["src/standalone.test.ts", "src/use-b.test.ts"]
    );
}

#[test]
fn widening_reaches_only_the_users_of_the_function_holding_the_unresolved_site() {
    let project = TestProject::new(&[
        (
            "src/plugins.ts",
            "export async function loadPlugin() {\n  return import(process.env.PLUGIN!);\n}\n\nexport function count() {\n  return 1;\n}\n",
        ),
        (
            "src/load.test.ts",
            "import { loadPlugin } from \"./plugins\";\n\ntest(\"load\", () => loadPlugin());\n",
        ),
        (
            "src/count.test.ts",
            "import { count } from \"./plugins\";\n\ntest(\"count\", () => count());\n",
        ),
        ("src/telegram.ts", "export const id = 1;\n"),
    ]);
    let base_files = [("src/telegram.ts", "export const id = 0;\n")];
    let changes = [modified("src/telegram.ts")];

    assert_eq!(
        project.symbol_outcome_of(&base_files, &changes),
        selected(&[(
            "src/load.test.ts",
            &[
                "src/telegram.ts (modified)",
                "-> widened: unresolved dynamic import at src/plugins.ts:2 (process.env.PLUGIN!)",
                "-> loadPlugin used at load by src/load.test.ts",
            ]
        )])
    );
    assert_eq!(
        project.outcome_of(&base_files, &changes).selected_paths(),
        ["src/count.test.ts", "src/load.test.ts"]
    );
}

#[test]
fn closed_mock_of_a_barrel_cuts_the_symbols_resolved_through_it() {
    let project = hub_project(
        "./index",
        &[
            ("src/index.ts", "export * from \"./hub\";\n"),
            (
                "src/reader.mocked.test.ts",
                "import { read } from \"./reader\";\n\nvi.mock(\"./index\", () => ({ parse: (text: string) => text }));\n\ntest(\"read\", () => read(\"a\"));\n",
            ),
        ],
    );

    assert_eq!(
        project
            .symbol_outcome_of(&[("src/hub.ts", BASE_HUB)], &[modified("src/hub.ts")])
            .selected_paths(),
        ["src/reader.test.ts"]
    );
}

const FACTORY: &str = "export function createHandlers(prefix: string) {\n  return {\n    greet: (name: string) => `${prefix} ${name}`,\n    shout: (name: string) => `${prefix} ${name}!`,\n  };\n}\n";

fn factory_project() -> TestProject {
    TestProject::new(&[
        ("src/factory.ts", FACTORY),
        (
            "src/handlers.ts",
            "import { createHandlers } from \"./factory\";\n\nexport const handlers = createHandlers(\"hello\");\nexport const version = 1;\n",
        ),
        (
            "src/greet.test.ts",
            "import { handlers } from \"./handlers\";\n\ntest(\"greet\", () => expect(handlers.greet(\"a\")).toBe(\"hello a\"));\n",
        ),
        (
            "src/version.test.ts",
            "import { version } from \"./handlers\";\n\ntest(\"version\", () => expect(version).toBe(1));\n",
        ),
    ])
}

#[test]
fn change_inside_what_a_factory_called_at_load_creates_selects_only_the_users_of_its_result() {
    let project = factory_project();
    let base_factory = FACTORY.replace("`${prefix} ${name}`", "`${prefix}, ${name}`");

    assert_eq!(
        project.symbol_outcome_of(
            &[("src/factory.ts", &base_factory)],
            &[modified("src/factory.ts")]
        ),
        selected(&[(
            "src/greet.test.ts",
            &[
                "src/factory.ts (modified)",
                "-> createHandlers changed",
                "-> createHandlers used by handlers in src/handlers.ts",
                "-> handlers used at load by src/greet.test.ts",
            ]
        )])
    );
}

#[test]
fn change_to_what_a_factory_runs_when_called_at_load_selects_every_test_loading_the_caller() {
    let project = factory_project();
    let base_factory = FACTORY.replace("{\n  return", "{\n  console.log(prefix);\n  return");

    assert_eq!(
        project
            .symbol_outcome_of(
                &[("src/factory.ts", &base_factory)],
                &[modified("src/factory.ts")]
            )
            .selected_paths(),
        ["src/greet.test.ts", "src/version.test.ts"]
    );
}

const STORE: &str = "export class Store {\n  private items: string[] = [];\n\n  add(item: string) {\n    this.items.push(item);\n    return this.count();\n  }\n\n  count() {\n    return this.items.length;\n  }\n\n  clear() {\n    this.items = [];\n  }\n}\n";

fn store_test(method: &str, call: &str) -> String {
    format!(
        "import {{ Store }} from \"./store\";\n\nconst store = new Store();\n\ntest(\"{method}\", () => expect(store.{call}).toBeDefined());\n"
    )
}

#[test]
fn change_to_one_method_selects_the_tests_reaching_it_through_instances_and_this() {
    let add_test = store_test("add", "add(\"a\")");
    let count_test = store_test("count", "count()");
    let clear_test = store_test("clear", "clear()");
    let project = TestProject::new(&[
        ("src/store.ts", STORE),
        ("src/add.test.ts", &add_test),
        ("src/count.test.ts", &count_test),
        ("src/clear.test.ts", &clear_test),
    ]);
    let base_store = STORE.replace("return this.items.length;", "return this.items.length + 0;");
    let base_clear = STORE.replace("this.items = [];", "this.items = [\"a\"];");

    assert_eq!(
        project.symbol_outcome_of(
            &[("src/store.ts", &base_store)],
            &[modified("src/store.ts")]
        ),
        selected(&[
            (
                "src/add.test.ts",
                &[
                    "src/store.ts (modified)",
                    "-> Store.count changed",
                    "-> Store.count used by Store.add in src/store.ts",
                    "-> Store.add used at load by src/add.test.ts",
                ]
            ),
            (
                "src/count.test.ts",
                &[
                    "src/store.ts (modified)",
                    "-> Store.count changed",
                    "-> Store.count used at load by src/count.test.ts",
                ]
            ),
        ])
    );
    assert_eq!(
        project
            .symbol_outcome_of(
                &[("src/store.ts", &base_clear)],
                &[modified("src/store.ts")]
            )
            .selected_paths(),
        ["src/add.test.ts", "src/clear.test.ts", "src/count.test.ts"]
    );
}

#[test]
fn override_reaches_the_callers_through_this_and_an_inherited_method_the_subclass_users() {
    let shape = "export class Shape {\n  describe() {\n    return `area ${this.area()}`;\n  }\n\n  area() {\n    return 0;\n  }\n}\n";
    let square = "import { Shape } from \"./shape\";\n\nexport class Square extends Shape {\n  area() {\n    return 4;\n  }\n}\n";
    let project = TestProject::new(&[
        ("src/shape.ts", shape),
        ("src/square.ts", square),
        (
            "src/shape.test.ts",
            "import { Shape } from \"./shape\";\n\nconst shape = new Shape();\n\ntest(\"shape\", () => expect(shape.describe()).toBe(\"area 0\"));\n",
        ),
        (
            "src/square.test.ts",
            "import { Square } from \"./square\";\n\nconst square = new Square();\n\ntest(\"square\", () => expect(square.area()).toBe(4));\n",
        ),
    ]);
    let base_square = square.replace("return 4;", "return 2 * 2;");
    let base_shape = shape.replace("`area ${this.area()}`", "`area: ${this.area()}`");

    assert_eq!(
        project
            .symbol_outcome_of(
                &[("src/square.ts", &base_square)],
                &[modified("src/square.ts")]
            )
            .selected_paths(),
        ["src/square.test.ts", "src/shape.test.ts"]
    );
    assert_eq!(
        project
            .symbol_outcome_of(
                &[("src/shape.ts", &base_shape)],
                &[modified("src/shape.ts")]
            )
            .selected_paths(),
        ["src/shape.test.ts"]
    );
}

#[test]
fn change_to_one_property_of_an_object_selects_only_the_tests_reading_it() {
    let handlers = "export const handlers = {\n  greet: (name: string) => `hi ${name}`,\n  shout: (name: string) => `HI ${name}`,\n};\n";
    let project = TestProject::new(&[
        ("src/handlers.ts", handlers),
        (
            "src/greet.test.ts",
            "import { handlers } from \"./handlers\";\n\ntest(\"greet\", () => expect(handlers.greet(\"a\")).toBe(\"hi a\"));\n",
        ),
        (
            "src/shout.test.ts",
            "import { handlers } from \"./handlers\";\n\ntest(\"shout\", () => expect(handlers[\"shout\"](\"a\")).toBe(\"HI a\"));\n",
        ),
    ]);
    let base_handlers = handlers.replace("`HI ${name}`", "`HI, ${name}`");

    assert_eq!(
        project.symbol_outcome_of(
            &[("src/handlers.ts", &base_handlers)],
            &[modified("src/handlers.ts")]
        ),
        selected(&[(
            "src/shout.test.ts",
            &[
                "src/handlers.ts (modified)",
                "-> handlers.shout changed",
                "-> handlers.shout used at load by src/shout.test.ts",
            ]
        )])
    );
}

#[test]
fn a_change_read_through_a_runtime_key_names_the_read_in_the_reason() {
    let handlers = "export const handlers = {\n  greet: (name: string) => `hi ${name}`,\n  shout: (name: string) => `HI ${name}`,\n};\n";
    let project = TestProject::new(&[
        ("src/handlers.ts", handlers),
        (
            "src/dispatch.ts",
            "import { handlers } from \"./handlers\";\n\nexport const dispatch = (method: \"greet\" | \"shout\", name: string) => handlers[method](name);\n",
        ),
        (
            "src/dispatch.test.ts",
            "import { dispatch } from \"./dispatch\";\n\ntest(\"greet\", () => expect(dispatch(\"greet\", \"a\")).toBe(\"hi a\"));\n",
        ),
    ]);
    let base_handlers = handlers.replace("`HI ${name}`", "`HI, ${name}`");

    assert_eq!(
        project.symbol_outcome_of(
            &[("src/handlers.ts", &base_handlers)],
            &[modified("src/handlers.ts")]
        ),
        selected(&[(
            "src/dispatch.test.ts",
            &[
                "src/handlers.ts (modified)",
                "-> handlers.shout changed",
                "-> handlers.shout used by handlers in src/handlers.ts",
                "-> handlers read by key by dispatch in src/dispatch.ts",
                "-> dispatch used at load by src/dispatch.test.ts",
            ]
        )])
    );
}
