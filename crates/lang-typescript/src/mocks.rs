//! Mock contracts: the runtime export names a mocking factory has to provide, compared between revisions.

use std::collections::BTreeSet;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    AssignmentTarget, BindingPattern, Declaration, Expression, ImportOrExportKind, Statement,
};
use oxc_parser::Parser;
use oxc_span::SourceType;
use prober_core::files::RepoPath;

pub(crate) fn export_signature(path: &RepoPath, source_text: &str) -> Option<BTreeSet<String>> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path.as_str()).unwrap_or_default();
    let parsed = Parser::new(&allocator, source_text, source_type).parse();

    if parsed.panicked || parsed.diagnostics.has_errors() {
        return None;
    }

    let mut names = BTreeSet::new();

    for statement in &parsed.program.body {
        match statement {
            Statement::ExportDeclaration(export) => declared_names(&export.declaration, &mut names),
            Statement::ExportNamedDeclaration(export) => {
                if export.export_kind == ImportOrExportKind::Type {
                    continue;
                }

                names.extend(
                    export
                        .specifiers
                        .iter()
                        .filter(|specifier| specifier.export_kind == ImportOrExportKind::Value)
                        .map(|specifier| specifier.exported.name().to_string()),
                );
            }
            Statement::ExportFromDeclaration(export) => {
                if export.export_kind == ImportOrExportKind::Type {
                    continue;
                }

                names.extend(
                    export
                        .specifiers
                        .iter()
                        .filter(|specifier| specifier.export_kind == ImportOrExportKind::Value)
                        .map(|specifier| specifier.exported.name().to_string()),
                );
            }
            Statement::ExportDefaultDeclaration(_) => {
                names.insert("default".to_owned());
            }
            Statement::ExportAllDeclaration(export) => {
                if export.export_kind == ImportOrExportKind::Type {
                    continue;
                }

                let exported = match &export.exported {
                    Some(exported) => exported.name().to_string(),
                    None => format!("* from {}", export.source.value),
                };

                names.insert(exported);
            }
            Statement::TSExportAssignment(_) => return None,
            Statement::ExpressionStatement(expression_statement) => {
                if let Expression::AssignmentExpression(assignment) =
                    &expression_statement.expression
                    && assigns_commonjs_exports(&assignment.left)
                {
                    return None;
                }
            }
            _ => {}
        }
    }

    Some(names)
}

fn declared_names(declaration: &Declaration, names: &mut BTreeSet<String>) {
    match declaration {
        Declaration::VariableDeclaration(variables) if !variables.declare => {
            for declarator in &variables.declarations {
                binding_names(&declarator.id, names);
            }
        }
        Declaration::FunctionDeclaration(function) if !function.declare => {
            names.extend(function.id.as_ref().map(|id| id.name.to_string()));
        }
        Declaration::ClassDeclaration(class) if !class.declare => {
            names.extend(class.id.as_ref().map(|id| id.name.to_string()));
        }
        Declaration::TSEnumDeclaration(enumeration) if !enumeration.declare => {
            names.insert(enumeration.id.name.to_string());
        }
        Declaration::TSNamespaceDeclaration(namespace) if !namespace.declare => {
            names.insert(namespace.id.name.to_string());
        }
        _ => {}
    }
}

fn binding_names(pattern: &BindingPattern, names: &mut BTreeSet<String>) {
    match pattern {
        BindingPattern::BindingIdentifier(identifier) => {
            names.insert(identifier.name.to_string());
        }
        BindingPattern::ObjectPattern(object) => {
            for property in &object.properties {
                binding_names(&property.value, names);
            }

            if let Some(rest) = &object.rest {
                binding_names(&rest.argument, names);
            }
        }
        BindingPattern::ArrayPattern(array) => {
            for element in array.elements.iter().flatten() {
                binding_names(element, names);
            }

            if let Some(rest) = &array.rest {
                binding_names(&rest.argument, names);
            }
        }
        BindingPattern::AssignmentPattern(assignment) => binding_names(&assignment.left, names),
    }
}

fn assigns_commonjs_exports(target: &AssignmentTarget) -> bool {
    match target {
        AssignmentTarget::StaticMemberExpression(member) => match &member.object {
            Expression::Identifier(object) => {
                object.name == "exports"
                    || (object.name == "module" && member.property.name == "exports")
            }
            Expression::StaticMemberExpression(inner) => {
                matches!(&inner.object, Expression::Identifier(object) if object.name == "module")
                    && inner.property.name == "exports"
            }
            _ => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signature(source_text: &str) -> Option<Vec<String>> {
        export_signature(&RepoPath::new("src/db.ts").unwrap(), source_text)
            .map(|names| names.into_iter().collect())
    }

    #[test]
    fn signature_lists_runtime_export_names_only() {
        let source_text = r#"
            export const query = () => 1, { pool, ...rest } = config;
            export function connect() {}
            export class Client {}
            export enum Mode { Read }
            export type Row = { id: string };
            export interface Options {}
            export declare const ambient: number;
            export { helper as close, type Kind };
            export * from "./errors";
            export * as dialects from "./dialects";
            export default connect;
        "#;

        assert_eq!(
            signature(source_text).unwrap(),
            [
                "* from ./errors",
                "Client",
                "Mode",
                "close",
                "connect",
                "default",
                "dialects",
                "pool",
                "query",
                "rest"
            ]
        );
    }

    #[test]
    fn body_changes_keep_the_signature() {
        let before = "export function query(sql: string) { return run(sql); }";
        let after = "export function query(sql: string) { return run(sql.trim()); }";

        assert_eq!(signature(before), signature(after));
    }

    #[test]
    fn commonjs_and_unparsable_modules_have_no_known_signature() {
        assert_eq!(signature("module.exports = { query };"), None);
        assert_eq!(signature("exports.query = query;"), None);
        assert_eq!(signature("module.exports.query = query;"), None);
        assert_eq!(signature("export const = ;"), None);
    }
}
