use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, CallExpression, ExportAllDeclaration, ExportFromDeclaration, Expression,
    ImportDeclaration, ImportExpression, ImportOrExportKind, NewExpression,
    TSImportEqualsDeclaration, TSModuleReference,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::SourceType;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRequest {
    pub specifier: String,
    pub kind: EdgeKind,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParsedImports {
    pub import_requests: Vec<ImportRequest>,
    pub has_dynamic_sites: bool,
}

pub fn parse_imports(path: &RepoPath, source_text: &str) -> ParsedImports {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path.as_str()).unwrap_or_default();
    let parsed = Parser::new(&allocator, source_text, source_type).parse();
    let mut import_collector = ImportCollector::default();

    import_collector.visit_program(&parsed.program);

    import_collector.parsed_imports
}

#[derive(Default)]
struct ImportCollector {
    parsed_imports: ParsedImports,
}

impl ImportCollector {
    fn record(&mut self, specifier: &str, kind: EdgeKind) {
        self.parsed_imports.import_requests.push(ImportRequest {
            specifier: specifier.to_owned(),
            kind,
        });
    }
}

impl<'a> Visit<'a> for ImportCollector {
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

        walk::walk_call_expression(self, call);
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
        ];
        let static_sources = [
            "await import(\"./plugins/telegram.js\");",
            "require(\"./config.json\");",
            "require.resolve(modulePath);",
            "new Map(entries);",
        ];

        for dynamic_source in dynamic_sources {
            assert!(has_dynamic_sites(dynamic_source), "{dynamic_source}");
        }

        for static_source in static_sources {
            assert!(!has_dynamic_sites(static_source), "{static_source}");
        }
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
