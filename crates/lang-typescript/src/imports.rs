use oxc_allocator::Allocator;
use std::collections::HashSet;

use oxc_ast::ast::{
    Argument, ArrayExpressionElement, CallExpression, ExportAllDeclaration, ExportFromDeclaration,
    Expression, ImportDeclaration, ImportExpression, ImportOrExportKind, NewExpression, Program,
    Statement, StringLiteral, TSImportEqualsDeclaration, TSModuleReference, TemplateLiteral,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};
use prober_core::files::RepoPath;
use prober_core::graph::EdgeKind;

const MODULE_LOADING_METHODS: [(&str, &str); 8] = [
    ("vi", "mock"),
    ("vi", "doMock"),
    ("vi", "importActual"),
    ("vi", "importMock"),
    ("jest", "mock"),
    ("jest", "doMock"),
    ("jest", "requireActual"),
    ("jest", "requireMock"),
];

const WORKER_CONSTRUCTORS: [&str; 2] = ["Worker", "SharedWorker"];

const LOADER_MODULES: [&str; 1] = ["jiti"];

const MOCKING_METHODS: [(&str, &str); 4] = [
    ("vi", "mock"),
    ("vi", "doMock"),
    ("jest", "mock"),
    ("jest", "doMock"),
];

const HOISTED_MOCKING_METHOD: &str = "mock";

const ORIGINAL_MODULE_LOADERS: [&str; 5] = [
    "importActual",
    "importOriginal",
    "importMock",
    "requireActual",
    "requireMock",
];

const GLOB_IMPORT_METHODS: [&str; 2] = ["glob", "globEager"];

const FILE_ACCESS_MODULES: [&str; 10] = [
    "fs",
    "node:fs",
    "fs/promises",
    "node:fs/promises",
    "fs-extra",
    "graceful-fs",
    "glob",
    "fast-glob",
    "tinyglobby",
    "globby",
];

const SPAWN_MODULES: [&str; 5] = [
    "child_process",
    "node:child_process",
    "execa",
    "cross-spawn",
    "nano-spawn",
];

const MAX_PATH_LITERAL_LENGTH: usize = 255;

const MAX_EXTENSION_LENGTH: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRequest {
    pub specifier: String,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockRequest {
    pub specifier: String,
    pub has_factory: bool,
    pub replaces_module: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobImport {
    pub line: u32,
    pub patterns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathLiteral {
    pub line: u32,
    pub text: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParsedImports {
    pub import_requests: Vec<ImportRequest>,
    pub has_dynamic_sites: bool,
    pub has_file_accesses: bool,
    pub has_spawn_sites: bool,
    pub mocks: Vec<MockRequest>,
    pub glob_imports: Vec<GlobImport>,
    pub path_literals: Vec<PathLiteral>,
}

pub(crate) fn collect_imports(program: &Program, source_text: &str) -> ParsedImports {
    let top_level_calls = program
        .body
        .iter()
        .filter_map(|statement| match statement {
            Statement::ExpressionStatement(statement) => {
                Some(statement.expression.get_inner_expression().span().start)
            }
            _ => None,
        })
        .collect();
    let mut import_collector = ImportCollector {
        source_text,
        line_starts: line_starts(source_text),
        top_level_calls,
        parsed_imports: ParsedImports::default(),
    };

    import_collector.visit_program(program);

    import_collector.parsed_imports
}

pub fn string_literals(path: &RepoPath, source_text: &str) -> Vec<String> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path.as_str()).unwrap_or_default();
    let parsed = Parser::new(&allocator, source_text, source_type).parse();
    let mut literal_collector = LiteralCollector::default();

    literal_collector.visit_program(&parsed.program);

    literal_collector.literals
}

#[derive(Default)]
struct LiteralCollector {
    literals: Vec<String>,
}

impl<'a> Visit<'a> for LiteralCollector {
    fn visit_string_literal(&mut self, string_literal: &StringLiteral<'a>) {
        self.literals.push(string_literal.value.to_string());
    }

    fn visit_template_literal(&mut self, template_literal: &TemplateLiteral<'a>) {
        if template_literal.expressions.is_empty()
            && let Some(text) = template_literal
                .quasis
                .first()
                .and_then(|quasi| quasi.value.cooked.as_deref())
        {
            self.literals.push(text.to_owned());
        }

        walk::walk_template_literal(self, template_literal);
    }
}

struct ImportCollector<'s> {
    source_text: &'s str,
    line_starts: Vec<usize>,
    top_level_calls: HashSet<u32>,
    parsed_imports: ParsedImports,
}

impl ImportCollector<'_> {
    fn record(&mut self, specifier: &str, kind: EdgeKind) {
        if FILE_ACCESS_MODULES.contains(&specifier) {
            self.parsed_imports.has_file_accesses = true;
        }

        if kind == EdgeKind::Import && SPAWN_MODULES.contains(&specifier) {
            self.parsed_imports.has_spawn_sites = true;
        }

        if kind == EdgeKind::Import && LOADER_MODULES.contains(&specifier) {
            self.parsed_imports.has_dynamic_sites = true;
        }

        self.parsed_imports.import_requests.push(ImportRequest {
            specifier: specifier.to_owned(),
            kind,
        });
    }

    fn record_path_literal(&mut self, text: &str, offset: u32) {
        if !is_path_like(text) {
            return;
        }

        let line = self.line_of(offset);

        self.parsed_imports.path_literals.push(PathLiteral {
            line,
            text: text.to_owned(),
        });
    }

    fn line_of(&self, offset: u32) -> u32 {
        let line_index = self
            .line_starts
            .partition_point(|&line_start| line_start <= offset as usize);

        u32::try_from(line_index).unwrap_or(u32::MAX)
    }

    fn record_mock(&mut self, call: &CallExpression, mocking_method: &str) {
        let Some(specifier) = call.arguments.first().and_then(mocked_specifier) else {
            return;
        };
        let factory = call.arguments.get(1).and_then(Argument::as_expression);
        let is_hoisted = mocking_method == HOISTED_MOCKING_METHOD
            && self.top_level_calls.contains(&call.span.start);
        let replaces_module =
            is_hoisted && factory.is_some_and(|factory| self.is_closed_factory(factory));

        self.parsed_imports.mocks.push(MockRequest {
            specifier: specifier.to_owned(),
            has_factory: factory.is_some_and(is_function),
            replaces_module,
        });
    }

    fn is_closed_factory(&self, factory: &Expression) -> bool {
        let has_parameters = match factory.get_inner_expression() {
            Expression::ArrowFunctionExpression(arrow) => !arrow.params.items.is_empty(),
            Expression::FunctionExpression(function) => !function.params.items.is_empty(),
            _ => return false,
        };
        let factory_text = factory.span().source_text(self.source_text);

        !has_parameters
            && !ORIGINAL_MODULE_LOADERS
                .iter()
                .any(|loader| factory_text.contains(loader))
    }

    fn record_glob_import(&mut self, call: &CallExpression) {
        let patterns = match call.arguments.first().and_then(Argument::as_expression) {
            Some(Expression::ArrayExpression(array)) => array
                .elements
                .iter()
                .map(|element| match element {
                    ArrayExpressionElement::StringLiteral(pattern) => {
                        Some(pattern.value.to_string())
                    }
                    _ => None,
                })
                .collect::<Option<Vec<String>>>(),
            Some(expression) => literal_text(expression).map(|pattern| vec![pattern.to_owned()]),
            None => None,
        };

        match patterns {
            Some(patterns) => {
                let line = self.line_of(call.span.start);

                self.parsed_imports
                    .glob_imports
                    .push(GlobImport { line, patterns });
            }
            None => self.parsed_imports.has_dynamic_sites = true,
        }
    }
}

impl<'a> Visit<'a> for ImportCollector<'_> {
    fn visit_import_declaration(&mut self, declaration: &ImportDeclaration<'a>) {
        self.record(
            &declaration.source.value,
            edge_kind(declaration.import_kind),
        );
    }

    fn visit_export_from_declaration(&mut self, declaration: &ExportFromDeclaration<'a>) {
        self.record(
            &declaration.source.value,
            edge_kind(declaration.export_kind),
        );
    }

    fn visit_export_all_declaration(&mut self, declaration: &ExportAllDeclaration<'a>) {
        self.record(
            &declaration.source.value,
            edge_kind(declaration.export_kind),
        );
    }

    fn visit_ts_import_equals_declaration(&mut self, declaration: &TSImportEqualsDeclaration<'a>) {
        if let TSModuleReference::ExternalModuleReference(module_reference) =
            &declaration.module_reference
        {
            self.record(
                &module_reference.expression.value,
                edge_kind(declaration.import_kind),
            );
        }
    }

    fn visit_import_expression(&mut self, import_expression: &ImportExpression<'a>) {
        match literal_text(&import_expression.source) {
            Some(specifier) => self.record(specifier, EdgeKind::Import),
            None => self.parsed_imports.has_dynamic_sites = true,
        }

        walk::walk_import_expression(self, import_expression);
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        let literal_specifier = call
            .arguments
            .first()
            .and_then(Argument::as_expression)
            .and_then(literal_text);

        match literal_specifier {
            Some(specifier) if loads_module(&call.callee) => {
                self.record(specifier, EdgeKind::Import)
            }
            None if is_require(&call.callee) && !call.arguments.is_empty() => {
                self.parsed_imports.has_dynamic_sites = true;
            }
            _ => {}
        }

        if creates_require(&call.callee) {
            self.parsed_imports.has_dynamic_sites = true;
        }

        if let Some(mocking_method) = mocking_method(&call.callee) {
            self.record_mock(call, mocking_method);
        }

        if is_glob_import(&call.callee) {
            self.record_glob_import(call);
        }

        walk::walk_call_expression(self, call);
    }

    fn visit_string_literal(&mut self, string_literal: &StringLiteral<'a>) {
        self.record_path_literal(&string_literal.value, string_literal.span.start);
    }

    fn visit_template_literal(&mut self, template_literal: &TemplateLiteral<'a>) {
        if template_literal.expressions.is_empty()
            && let Some(text) = template_literal
                .quasis
                .first()
                .and_then(|quasi| quasi.value.cooked.as_deref())
        {
            self.record_path_literal(text, template_literal.span.start);
        }

        walk::walk_template_literal(self, template_literal);
    }

    fn visit_new_expression(&mut self, new_expression: &NewExpression<'a>) {
        if let Expression::Identifier(constructor) = new_expression.callee.get_inner_expression()
            && WORKER_CONSTRUCTORS.contains(&constructor.name.as_str())
        {
            self.parsed_imports.has_dynamic_sites = true;
        }

        walk::walk_new_expression(self, new_expression);
    }
}

fn edge_kind(import_or_export_kind: ImportOrExportKind) -> EdgeKind {
    match import_or_export_kind {
        ImportOrExportKind::Value => EdgeKind::Import,
        ImportOrExportKind::Type => EdgeKind::TypeOnly,
    }
}

fn literal_text<'s>(expression: &'s Expression) -> Option<&'s str> {
    match expression {
        Expression::StringLiteral(string_literal) => Some(&string_literal.value),
        Expression::TemplateLiteral(template_literal)
            if template_literal.expressions.is_empty() =>
        {
            template_literal.quasis.first()?.value.cooked.as_deref()
        }
        _ => None,
    }
}

fn mocking_method<'e>(callee: &'e Expression) -> Option<&'e str> {
    let Expression::StaticMemberExpression(method) = callee.get_inner_expression() else {
        return None;
    };
    let Expression::Identifier(object) = &method.object else {
        return None;
    };
    let method_name = method.property.name.as_str();

    MOCKING_METHODS
        .contains(&(object.name.as_str(), method_name))
        .then_some(method_name)
}

fn mocked_specifier<'a>(argument: &'a Argument) -> Option<&'a str> {
    match argument.as_expression()?.get_inner_expression() {
        Expression::ImportExpression(import_expression) => literal_text(&import_expression.source),
        expression => literal_text(expression),
    }
}

fn is_function(expression: &Expression) -> bool {
    matches!(
        expression.get_inner_expression(),
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_)
    )
}

fn is_glob_import(callee: &Expression) -> bool {
    let Expression::StaticMemberExpression(method) = callee.get_inner_expression() else {
        return false;
    };

    matches!(&method.object, Expression::ImportMeta(_))
        && GLOB_IMPORT_METHODS.contains(&method.property.name.as_str())
}

pub(crate) fn is_path_like(text: &str) -> bool {
    let has_path_characters = !text.is_empty()
        && text.len() <= MAX_PATH_LITERAL_LENGTH
        && !text.chars().any(|character| {
            character.is_whitespace()
                || character.is_control()
                || "*?{}[]<>|:\\$#\"'`".contains(character)
        });
    let file_name = text.rsplit('/').next().unwrap_or(text);
    let has_extension = file_name.rsplit_once('.').is_some_and(|(_, extension)| {
        !extension.is_empty()
            && extension.len() <= MAX_EXTENSION_LENGTH
            && extension
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
    });

    has_path_characters && (text.contains('/') || has_extension)
}

fn line_starts(source_text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(
            source_text
                .match_indices('\n')
                .map(|(newline_offset, _)| newline_offset + 1),
        )
        .collect()
}

fn is_require(callee: &Expression) -> bool {
    matches!(callee.get_inner_expression(), Expression::Identifier(function) if function.name == "require")
}

fn creates_require(callee: &Expression) -> bool {
    match callee.get_inner_expression() {
        Expression::Identifier(function) => function.name == "createRequire",
        Expression::StaticMemberExpression(method) => method.property.name == "createRequire",
        _ => false,
    }
}

fn loads_module(callee: &Expression) -> bool {
    match callee {
        Expression::Identifier(function) => function.name == "require",
        Expression::StaticMemberExpression(method) => match &method.object {
            Expression::Identifier(object) => MODULE_LOADING_METHODS
                .contains(&(object.name.as_str(), method.property.name.as_str())),
            _ => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_imports(path: &RepoPath, source_text: &str) -> ParsedImports {
        let allocator = Allocator::default();
        let source_type = SourceType::from_path(path.as_str()).unwrap_or_default();
        let parsed = Parser::new(&allocator, source_text, source_type).parse();

        collect_imports(&parsed.program, source_text)
    }

    fn imports_of(path: &str, source_text: &str) -> Vec<(String, EdgeKind)> {
        parse_imports(&RepoPath::new(path).unwrap(), source_text)
            .import_requests
            .into_iter()
            .map(|import_request| (import_request.specifier, import_request.kind))
            .collect()
    }

    fn has_dynamic_sites(source_text: &str) -> bool {
        parse_imports(&RepoPath::new("src/a.ts").unwrap(), source_text).has_dynamic_sites
    }

    fn runtime(specifier: &str) -> (String, EdgeKind) {
        (specifier.to_owned(), EdgeKind::Import)
    }

    fn type_only(specifier: &str) -> (String, EdgeKind) {
        (specifier.to_owned(), EdgeKind::TypeOnly)
    }

    #[test]
    fn static_imports_load_their_module_at_runtime() {
        let source_text = r#"
            import defaultExport from "./default";
            import { named } from "./named";
            import * as namespace from "./namespace";
            import "./side-effect";
        "#;

        assert_eq!(
            imports_of("src/a.ts", source_text),
            [
                runtime("./default"),
                runtime("./named"),
                runtime("./namespace"),
                runtime("./side-effect"),
            ]
        );
    }

    #[test]
    fn import_type_is_type_only_but_inline_type_specifiers_stay_runtime() {
        let source_text = r#"
            import type { Config } from "./config";
            import { type Options } from "./options";
        "#;

        assert_eq!(
            imports_of("src/a.ts", source_text),
            [type_only("./config"), runtime("./options")]
        );
    }

    #[test]
    fn re_exports_load_their_module_unless_type_only() {
        let source_text = r#"
            export * from "./all";
            export { named } from "./named";
            export type { Config } from "./config";
            export type * from "./types";
            export const local = 1;
        "#;

        assert_eq!(
            imports_of("src/index.ts", source_text),
            [
                runtime("./all"),
                runtime("./named"),
                type_only("./config"),
                type_only("./types"),
            ]
        );
    }

    #[test]
    fn import_equals_require_follows_its_type_modifier() {
        let source_text = r#"
            import fs = require("./fs-wrapper");
            import type Types = require("./types");
        "#;

        assert_eq!(
            imports_of("src/a.ts", source_text),
            [runtime("./fs-wrapper"), type_only("./types")]
        );
    }

    #[test]
    fn literal_dynamic_imports_and_requires_are_runtime_imports() {
        let source_text = r#"
            export async function load(name: string) {
                const plugin = await import("./plugins/telegram");
                const template = await import(`./template`);
                const config = require("./config.json");
                const computed = await import(`./plugins/${name}`);
                const required = require(name);
                const resolvedPath = require.resolve("./not-loaded");

                return [plugin, template, config, computed, required, resolvedPath];
            }
        "#;

        assert_eq!(
            imports_of("src/a.ts", source_text),
            [
                runtime("./plugins/telegram"),
                runtime("./template"),
                runtime("./config.json"),
            ]
        );
    }

    #[test]
    fn computed_loads_workers_and_require_factories_are_dynamic_sites() {
        let dynamic_sources = [
            "await import(`./plugins/${name}.js`);",
            "require(modulePath);",
            "const load = createRequire(import.meta.url);",
            "const load = module.createRequire(root);",
            "new Worker(new URL(\"./a.worker.ts\", import.meta.url));",
            "import { createJiti } from \"jiti\";",
        ];
        let static_sources = [
            "await import(\"./plugins/telegram.js\");",
            "require(\"./config.json\");",
            "require.resolve(modulePath);",
            "new Map(entries);",
            "import type { JitiOptions } from \"jiti\";",
            "const loader = createJiti(source);",
        ];

        for dynamic_source in dynamic_sources {
            assert!(has_dynamic_sites(dynamic_source), "{dynamic_source}");
        }

        for static_source in static_sources {
            assert!(!has_dynamic_sites(static_source), "{static_source}");
        }
    }

    #[test]
    fn runtime_imports_of_process_modules_are_spawn_sites() {
        let has_spawn_sites = |source_text: &str| {
            parse_imports(&RepoPath::new("test/a.test.ts").unwrap(), source_text).has_spawn_sites
        };

        assert!(has_spawn_sites(
            "import { spawnSync } from \"node:child_process\";"
        ));
        assert!(has_spawn_sites("const { execa } = require(\"execa\");"));
        assert!(!has_spawn_sites(
            "import type { ChildProcess } from \"node:child_process\";"
        ));
        assert!(!has_spawn_sites("import { readFile } from \"node:fs\";"));
    }

    #[test]
    fn mocked_and_actual_modules_in_tests_are_runtime_imports() {
        let source_text = r#"
            vi.mock("../db", async () => ({ ...(await vi.importActual("../db")) }));
            jest.mock("./logger");
            const actual = jest.requireActual("./clock");
        "#;

        assert_eq!(
            imports_of("src/a.test.ts", source_text),
            [
                runtime("../db"),
                runtime("../db"),
                runtime("./logger"),
                runtime("./clock")
            ]
        );
    }

    #[test]
    fn string_literals_include_plain_templates_but_not_interpolated_ones() {
        let source_text = r#"
            export default defineConfig({
                test: { setupFiles: ["./test/setup.ts", `scripts/setup-vitest.ts`], name: `unit-${id}` },
            });
        "#;

        assert_eq!(
            string_literals(&RepoPath::new("vitest.config.ts").unwrap(), source_text),
            ["./test/setup.ts", "scripts/setup-vitest.ts"]
        );
    }

    fn mocks_of(source_text: &str) -> Vec<(String, bool, bool)> {
        parse_imports(&RepoPath::new("src/a.test.ts").unwrap(), source_text)
            .mocks
            .into_iter()
            .map(|mock| (mock.specifier, mock.has_factory, mock.replaces_module))
            .collect()
    }

    fn mock(specifier: &str, has_factory: bool, replaces_module: bool) -> (String, bool, bool) {
        (specifier.to_owned(), has_factory, replaces_module)
    }

    #[test]
    fn hoisted_mocks_with_a_closed_factory_replace_the_module() {
        let source_text = r#"
            vi.mock("./db", () => ({ query: vi.fn() }));
            jest.mock("./clock", function () { return { now: () => 0 }; });
            vi.mock(import("./typed"), () => ({ typed: 1 }));
            vi.mock("./partial", async (importOriginal) => ({ ...(await importOriginal()), x: 1 }));
            vi.mock("./actual", async () => ({ ...(await vi.importActual("./actual")) }));
            jest.mock("./required", () => ({ ...jest.requireActual("./required") }));
            vi.mock("./automocked");
            vi.mock("./spied", { spy: true });
            vi.doMock("./later", () => ({ later: 1 }));

            describe("nested", () => {
                vi.mock("./nested", () => ({ nested: 1 }));
            });
        "#;

        assert_eq!(
            mocks_of(source_text),
            [
                mock("./db", true, true),
                mock("./clock", true, true),
                mock("./typed", true, true),
                mock("./partial", true, false),
                mock("./actual", true, false),
                mock("./required", true, false),
                mock("./automocked", false, false),
                mock("./spied", false, false),
                mock("./later", true, false),
                mock("./nested", true, false),
            ]
        );
    }

    #[test]
    fn glob_imports_keep_their_literal_patterns_and_line() {
        let source_text = r#"
            const pages = import.meta.glob("./pages/*.ts");
            const locales = import.meta.glob(["./locales/*.json", "!./locales/draft.json"], { eager: true });
            const computed = import.meta.glob(pattern);
        "#;
        let parsed = parse_imports(&RepoPath::new("src/a.ts").unwrap(), source_text);

        assert_eq!(
            parsed.glob_imports,
            [
                GlobImport {
                    line: 2,
                    patterns: vec!["./pages/*.ts".to_owned()],
                },
                GlobImport {
                    line: 3,
                    patterns: vec![
                        "./locales/*.json".to_owned(),
                        "!./locales/draft.json".to_owned()
                    ],
                },
            ]
        );
        assert!(parsed.has_dynamic_sites);
    }

    #[test]
    fn path_like_literals_are_kept_with_their_line() {
        let source_text = r#"
            const prompt = readFileSync("prompts/system.md", "utf8");
            const fixture = `./fixtures/legacy.json`;
            const label = "Hello world";
            const pattern = "src/**/*.ts";
            const url = "https://example.com/a.json";
            const name = `${prefix}.json`;
            const manifest = "package.json";
        "#;
        let literals: Vec<(u32, String)> =
            parse_imports(&RepoPath::new("src/a.ts").unwrap(), source_text)
                .path_literals
                .into_iter()
                .map(|path_literal| (path_literal.line, path_literal.text))
                .collect();

        assert_eq!(
            literals,
            [
                (2, "prompts/system.md".to_owned()),
                (3, "./fixtures/legacy.json".to_owned()),
                (8, "package.json".to_owned()),
            ]
        );
    }

    #[test]
    fn importing_the_file_system_or_a_glob_library_marks_file_accesses() {
        let has_file_accesses = |source_text: &str| {
            parse_imports(&RepoPath::new("src/a.ts").unwrap(), source_text).has_file_accesses
        };

        assert!(has_file_accesses(
            r#"import { readFileSync } from "node:fs";"#
        ));
        assert!(has_file_accesses(r#"import fs from "fs/promises";"#));
        assert!(has_file_accesses(r#"const fg = require("fast-glob");"#));
        assert!(!has_file_accesses(r#"import { join } from "node:path";"#));
    }

    #[test]
    fn import_types_in_type_positions_are_ignored() {
        let source_text = r#"
            let options: typeof import("./options");
            type Config = import("./config").Config;
        "#;

        assert_eq!(imports_of("src/a.ts", source_text), []);
    }

    #[test]
    fn jsx_files_are_parsed_with_jsx_enabled() {
        let source_text = r#"
            import { Button } from "./Button";

            export const App = () => <Button label="ok" />;
        "#;

        assert_eq!(imports_of("ui/App.tsx", source_text), [runtime("./Button")]);
        assert_eq!(imports_of("ui/App.jsx", source_text), [runtime("./Button")]);
    }

    #[test]
    fn commonjs_files_are_parsed_as_scripts() {
        let source_text = r#"
            const helper = require("./helper.cjs");

            module.exports = { helper };
        "#;

        assert_eq!(
            imports_of("scripts/build.cjs", source_text),
            [runtime("./helper.cjs")]
        );
    }
}
