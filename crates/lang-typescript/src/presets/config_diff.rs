//! Runner config changes that only add, remove or edit string literals, such as paths and globs in test lists.

use std::collections::BTreeSet;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    ArrayExpression, ArrayExpressionElement, CallExpression, ExportAllDeclaration,
    ExportFromDeclaration, Expression, ImportDeclaration, ImportExpression, StringLiteral,
    TemplateLiteral,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};
use prober_core::files::RepoPath;
use serde_json::Value as JsonValue;

use crate::conventions::is_source_file;
use crate::manifests::documents::{parse_jsonc, parse_yaml};

const LITERAL_MARK: &str = "\u{0}literal";

const LITERAL_LIST_MARK: &str = "\u{0}list";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigDiff {
    Literals(BTreeSet<String>),
    Structural,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Shape {
    skeleton: String,
    literals: Vec<String>,
    literal_lists: Vec<Vec<String>>,
}

pub(crate) fn config_diff(path: &RepoPath, base_text: &str, head_text: &str) -> ConfigDiff {
    let shapes = if is_source_file(path) {
        source_shape(path, base_text).zip(source_shape(path, head_text))
    } else {
        document_shape(base_text).zip(document_shape(head_text))
    };
    let Some((base_shape, head_shape)) = shapes else {
        return ConfigDiff::Structural;
    };

    if base_shape.skeleton != head_shape.skeleton {
        return ConfigDiff::Structural;
    }

    let mut changed_literals = BTreeSet::new();

    for (base_literal, head_literal) in base_shape.literals.iter().zip(&head_shape.literals) {
        if base_literal != head_literal {
            changed_literals.insert(base_literal.clone());
            changed_literals.insert(head_literal.clone());
        }
    }

    for (base_list, head_list) in base_shape
        .literal_lists
        .iter()
        .zip(&head_shape.literal_lists)
    {
        let base_items: BTreeSet<&String> = base_list.iter().collect();
        let head_items: BTreeSet<&String> = head_list.iter().collect();

        changed_literals.extend(
            base_items
                .symmetric_difference(&head_items)
                .map(|item| (*item).clone()),
        );
    }

    ConfigDiff::Literals(changed_literals)
}

fn source_shape(path: &RepoPath, source_text: &str) -> Option<Shape> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path.as_str()).unwrap_or_default();
    let parsed = Parser::new(&allocator, source_text, source_type).parse();

    if parsed.panicked || parsed.diagnostics.has_errors() {
        return None;
    }

    let mut collector = LiteralCollector::default();

    collector.visit_program(&parsed.program);

    let mut masked_spans: Vec<(Span, &str)> = collector
        .literal_spans
        .iter()
        .map(|span| (*span, LITERAL_MARK))
        .chain(
            collector
                .list_spans
                .iter()
                .map(|span| (*span, LITERAL_LIST_MARK)),
        )
        .chain(
            parsed
                .program
                .comments
                .iter()
                .map(|comment| (comment.span, "")),
        )
        .collect();

    masked_spans.sort_by_key(|(span, _)| span.start);

    let mut skeleton = String::new();
    let mut position = 0;

    for (span, mark) in masked_spans {
        let start = span.start as usize;

        if start < position {
            continue;
        }

        skeleton.push_str(&source_text[position..start]);
        skeleton.push_str(mark);
        position = span.end as usize;
    }

    skeleton.push_str(&source_text[position..]);
    skeleton.retain(|character| !character.is_whitespace());

    Some(Shape {
        skeleton,
        literals: collector.literals,
        literal_lists: collector.literal_lists,
    })
}

#[derive(Default)]
struct LiteralCollector {
    literal_spans: Vec<Span>,
    literals: Vec<String>,
    list_spans: Vec<Span>,
    literal_lists: Vec<Vec<String>>,
}

impl<'a> Visit<'a> for LiteralCollector {
    fn visit_import_declaration(&mut self, _: &ImportDeclaration<'a>) {}

    fn visit_export_all_declaration(&mut self, _: &ExportAllDeclaration<'a>) {}

    fn visit_import_expression(&mut self, _: &ImportExpression<'a>) {}

    fn visit_export_from_declaration(&mut self, _: &ExportFromDeclaration<'a>) {}

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        let is_require =
            matches!(&call.callee, Expression::Identifier(callee) if callee.name == "require");

        if !is_require {
            walk::walk_call_expression(self, call);
        }
    }

    fn visit_array_expression(&mut self, array: &ArrayExpression<'a>) {
        let items: Option<Vec<String>> = array
            .elements
            .iter()
            .map(|element| match element {
                ArrayExpressionElement::StringLiteral(literal) => Some(literal.value.to_string()),
                _ => None,
            })
            .collect();

        match items {
            Some(items) if !items.is_empty() => {
                self.list_spans.push(array.span);
                self.literal_lists.push(items);
            }
            _ => walk::walk_array_expression(self, array),
        }
    }

    fn visit_string_literal(&mut self, literal: &StringLiteral<'a>) {
        self.literal_spans.push(literal.span);
        self.literals.push(literal.value.to_string());
    }

    fn visit_template_literal(&mut self, template: &TemplateLiteral<'a>) {
        match (template.expressions.is_empty(), template.quasis.first()) {
            (true, Some(quasi)) => {
                self.literal_spans.push(template.span);
                self.literals
                    .push(quasi.value.cooked.as_deref().unwrap_or_default().to_owned());
            }
            _ => walk::walk_template_literal(self, template),
        }
    }
}

fn document_shape(text: &str) -> Option<Shape> {
    let document = parse_jsonc(text).or_else(|_| parse_yaml(text)).ok()?;
    let mut shape = Shape::default();

    describe_document(&document, &mut shape);

    Some(shape)
}

fn describe_document(value: &JsonValue, shape: &mut Shape) {
    match value {
        JsonValue::String(text) => {
            shape.skeleton.push_str(LITERAL_MARK);
            shape.literals.push(text.clone());
        }
        JsonValue::Array(items) if !items.is_empty() && items.iter().all(JsonValue::is_string) => {
            shape.skeleton.push_str(LITERAL_LIST_MARK);
            shape.literal_lists.push(
                items
                    .iter()
                    .filter_map(JsonValue::as_str)
                    .map(str::to_owned)
                    .collect(),
            );
        }
        JsonValue::Array(items) => {
            shape.skeleton.push('[');

            for item in items {
                describe_document(item, shape);
                shape.skeleton.push(',');
            }

            shape.skeleton.push(']');
        }
        JsonValue::Object(properties) => {
            shape.skeleton.push('{');

            for (key, value) in properties {
                shape.skeleton.push_str(key);
                shape.skeleton.push(':');
                describe_document(value, shape);
                shape.skeleton.push(',');
            }

            shape.skeleton.push('}');
        }
        other => shape.skeleton.push_str(&other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(path: &str, base_text: &str, head_text: &str) -> ConfigDiff {
        config_diff(&RepoPath::new(path).unwrap(), base_text, head_text)
    }

    fn literals(literals: &[&str]) -> ConfigDiff {
        ConfigDiff::Literals(literals.iter().map(|&literal| literal.to_owned()).collect())
    }

    #[test]
    fn edits_of_literal_lists_and_values_are_literal_changes() {
        let base = r#"
            // Unit fast lane
            export const unitFastPaths = ["src/a.test.ts", "src/b.test.ts"];
            export default { include: "src/**/*.test.ts", pool: "forks" };
        "#;
        let head = r#"
            export const unitFastPaths = [
                "src/b.test.ts",
                "src/c.test.ts",
            ];
            export default { include: "src/cron/**/*.test.ts", pool: "forks" };
        "#;

        assert_eq!(
            diff("test/vitest/vitest.unit-fast-paths.mjs", base, head),
            literals(&[
                "src/**/*.test.ts",
                "src/a.test.ts",
                "src/c.test.ts",
                "src/cron/**/*.test.ts"
            ])
        );
    }

    #[test]
    fn code_changes_are_structural() {
        let base = r#"export default defineConfig({ test: { include: ["a"] } });"#;
        let changes = [
            r#"export default defineConfig({ test: { include: ["a"], isolate: false } });"#,
            r#"export default defineConfig({ test: { include: [...shared, "a"] } });"#,
            r#"export default mergeConfig(base, { test: { include: ["a"] } });"#,
            "export default defineConfig({ test: { include: [\"a\"] }",
        ];

        for head in changes {
            assert_eq!(
                diff("vitest.config.ts", base, head),
                ConfigDiff::Structural,
                "{head}"
            );
        }
    }

    #[test]
    fn module_specifiers_are_code_not_data() {
        let base = r#"import { a } from "./paths-a.mjs"; export default a;"#;
        let head = r#"import { a } from "./paths-b.mjs"; export default a;"#;

        assert_eq!(diff("vitest.config.ts", base, head), ConfigDiff::Structural);
    }

    #[test]
    fn json_and_yaml_configs_compare_their_values() {
        assert_eq!(
            diff(
                "jest.config.json",
                r#"{ "testMatch": ["**/a.test.js"], "bail": 1 }"#,
                r#"{ "testMatch": ["**/a.test.js", "**/b.test.js"], "bail": 1 }"#
            ),
            literals(&["**/b.test.js"])
        );
        assert_eq!(
            diff(
                ".mocharc.yml",
                "spec: test/a.js\nbail: true\n",
                "spec: test/a.js\nbail: false\n"
            ),
            ConfigDiff::Structural
        );
    }
}
