//! Where a value goes once the code analysed evaluates it: stored in what that code declares or returns, dropped, read to decide what runs, held in a local, handed to functions that may only keep it, or run; and whether only a value derived from it goes there.

use oxc_ast::AstKind;
use oxc_ast::ast::{
    Argument, AssignmentTarget, BindingPattern, CallExpression, Expression, IdentifierReference,
    SimpleAssignmentTarget, UnaryOperator,
};
use oxc_semantic::SymbolId;
use oxc_span::{GetSpan, Span};

use crate::symbols::{Keeping, Use};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Boundary {
    Declaration,
    Function,
    Constructor,
    Nested,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum End {
    Stored,
    Dropped,
    Read,
    Through(Vec<SymbolId>),
    Run,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Placement {
    pub keeping: Vec<Keeping>,
    pub end: End,
    pub derived: bool,
    pub read: bool,
}

pub(crate) trait Bindings {
    fn callee_use(&self, callee: &Expression) -> Option<Use>;

    fn is_local(&self, reference: &IdentifierReference) -> bool;
}

enum Step {
    Same,
    Derived,
    Tested,
    End(End),
}

pub(crate) fn placement(
    ancestors: &[AstKind],
    (boundary_index, boundary): (usize, Boundary),
    span: Span,
    bindings: &dyn Bindings,
) -> Placement {
    let mut span = span;
    let mut placement = Placement {
        keeping: Vec::new(),
        end: if boundary == Boundary::Declaration {
            End::Stored
        } else {
            End::Run
        },
        derived: false,
        read: false,
    };

    for index in (boundary_index..ancestors.len()).rev() {
        let parent = ancestors[index];
        let step = match parent {
            AstKind::ParenthesizedExpression(_)
            | AstKind::TSAsExpression(_)
            | AstKind::TSSatisfiesExpression(_)
            | AstKind::TSNonNullExpression(_)
            | AstKind::TSTypeAssertion(_)
            | AstKind::TSInstantiationExpression(_)
            | AstKind::ChainExpression(_)
            | AstKind::AwaitExpression(_)
            | AstKind::ObjectExpression(_)
            | AstKind::ArrayExpression(_)
            | AstKind::SpreadElement(_)
            | AstKind::ClassBody(_) => Step::Same,
            AstKind::BinaryExpression(_)
            | AstKind::PrivateInExpression(_)
            | AstKind::TemplateLiteral(_) => Step::Derived,
            AstKind::UnaryExpression(unary) => match unary.operator {
                UnaryOperator::Delete => Step::End(End::Run),
                UnaryOperator::Void => Step::End(End::Dropped),
                _ => Step::Derived,
            },
            AstKind::ObjectProperty(property) => same_if(property.value.span() == span),
            AstKind::MethodDefinition(method) => same_if(method.value.span == span),
            AstKind::StaticMemberExpression(member) => same_if(member.object.span() == span),
            AstKind::PrivateFieldExpression(field) => same_if(field.object.span() == span),
            AstKind::ComputedMemberExpression(member) => {
                if member.object.span() == span {
                    Step::Same
                } else {
                    Step::Tested
                }
            }
            AstKind::LogicalExpression(logical) => {
                if logical.left.span() == span {
                    placement.read = true;
                }

                Step::Same
            }
            AstKind::ConditionalExpression(conditional) => {
                if conditional.test.span() == span {
                    Step::Tested
                } else {
                    Step::Same
                }
            }
            AstKind::SequenceExpression(sequence) => {
                let last = sequence.expressions.last().map(GetSpan::span);

                if last == Some(span) {
                    Step::Same
                } else {
                    Step::End(End::Dropped)
                }
            }
            AstKind::ExpressionStatement(_) => Step::End(End::Dropped),
            AstKind::IfStatement(_)
            | AstKind::WhileStatement(_)
            | AstKind::DoWhileStatement(_)
            | AstKind::SwitchStatement(_)
            | AstKind::SwitchCase(_) => Step::End(End::Read),
            AstKind::ForStatement(statement) => {
                if statement
                    .test
                    .as_ref()
                    .is_some_and(|test| test.span() == span)
                {
                    Step::End(End::Read)
                } else {
                    Step::End(End::Dropped)
                }
            }
            AstKind::AssignmentExpression(assignment) => Step::End(
                if boundary == Boundary::Constructor
                    && assignment.right.span() == span
                    && assigns_this(&assignment.left)
                {
                    End::Stored
                } else {
                    End::Run
                },
            ),
            AstKind::ReturnStatement(_) | AstKind::ArrowFunctionExpression(_) => {
                Step::End(returned(boundary))
            }
            AstKind::ExportDefaultDeclaration(_) => Step::End(End::Stored),
            AstKind::PropertyDefinition(property) => {
                Step::End(stored_value(property.value.as_ref(), span))
            }
            AstKind::AccessorProperty(property) => {
                Step::End(stored_value(property.value.as_ref(), span))
            }
            AstKind::VariableDeclarator(declarator) => {
                let initialises = declarator
                    .init
                    .as_ref()
                    .is_some_and(|init| init.span() == span);

                Step::End(if !initialises {
                    End::Run
                } else if declares(boundary, index, boundary_index) {
                    End::Stored
                } else {
                    through(bound_symbols(&declarator.id))
                })
            }
            AstKind::Class(class) if class.is_declaration() => Step::End(match &class.id {
                _ if declares(boundary, index, boundary_index) => End::Stored,
                Some(id) => through(id.symbol_id.get().into_iter().collect()),
                None => End::Run,
            }),
            AstKind::Class(_) => Step::Same,
            AstKind::CallExpression(call) => {
                let argument = call
                    .arguments
                    .iter()
                    .position(|argument| argument.span() == span);

                match argument {
                    Some(_) if call.pure => Step::Same,
                    Some(0) if passes_through(&call.callee) => Step::Same,
                    Some(index) if is_object_assign(&call.callee) => {
                        if index > 0 && !has_own_target(call, bindings) {
                            Step::End(End::Run)
                        } else {
                            Step::Same
                        }
                    }
                    Some(index) => match bindings.callee_use(&call.callee) {
                        Some(callee) => {
                            placement.keeping.push(Keeping {
                                callee,
                                index,
                                fresh_target: has_own_target(call, bindings),
                            });

                            Step::Same
                        }
                        None => Step::End(End::Run),
                    },
                    None => Step::End(End::Run),
                }
            }
            AstKind::NewExpression(new) if new.callee.span() != span => {
                let argument = new
                    .arguments
                    .iter()
                    .position(|argument| argument.span() == span);

                match (argument, bindings.callee_use(&new.callee)) {
                    _ if new.pure => Step::Same,
                    (Some(index), Some(callee)) => {
                        placement.keeping.push(Keeping {
                            callee,
                            index,
                            fresh_target: false,
                        });

                        Step::Same
                    }
                    _ => Step::End(End::Run),
                }
            }
            _ => Step::End(End::Run),
        };

        match step {
            Step::Same => {}
            Step::Derived => placement.derived = true,
            Step::Tested => {
                placement.derived = true;
                placement.read = true;
            }
            Step::End(end) => {
                placement.end = end;

                return placement;
            }
        }

        span = parent.span();
    }

    placement
}

fn through(symbols: Vec<SymbolId>) -> End {
    if symbols.is_empty() {
        End::Dropped
    } else {
        End::Through(symbols)
    }
}

pub(crate) fn bound_symbols(pattern: &BindingPattern) -> Vec<SymbolId> {
    let mut symbols = Vec::new();

    collect_bound(pattern, &mut symbols);

    symbols
}

fn collect_bound(pattern: &BindingPattern, symbols: &mut Vec<SymbolId>) {
    match pattern {
        BindingPattern::BindingIdentifier(identifier) => symbols.extend(identifier.symbol_id.get()),
        BindingPattern::ObjectPattern(object) => {
            for property in &object.properties {
                collect_bound(&property.value, symbols);
            }

            if let Some(rest) = &object.rest {
                collect_bound(&rest.argument, symbols);
            }
        }
        BindingPattern::ArrayPattern(array) => {
            for element in array.elements.iter().flatten() {
                collect_bound(element, symbols);
            }

            if let Some(rest) = &array.rest {
                collect_bound(&rest.argument, symbols);
            }
        }
        BindingPattern::AssignmentPattern(assignment) => collect_bound(&assignment.left, symbols),
    }
}

fn same_if(same: bool) -> Step {
    if same { Step::Same } else { Step::Derived }
}

fn declares(boundary: Boundary, index: usize, boundary_index: usize) -> bool {
    boundary == Boundary::Declaration && index == boundary_index
}

fn assigns_this(target: &AssignmentTarget) -> bool {
    target
        .as_simple_assignment_target()
        .is_some_and(|target| match target {
            SimpleAssignmentTarget::StaticMemberExpression(member) => {
                matches!(member.object, Expression::ThisExpression(_))
            }
            SimpleAssignmentTarget::PrivateFieldExpression(field) => {
                matches!(field.object, Expression::ThisExpression(_))
            }
            _ => false,
        })
}

fn stored_value(value: Option<&Expression>, span: Span) -> End {
    if value.is_some_and(|value| value.span() == span) {
        End::Stored
    } else {
        End::Dropped
    }
}

fn returned(boundary: Boundary) -> End {
    match boundary {
        Boundary::Function | Boundary::Constructor => End::Stored,
        Boundary::Declaration | Boundary::Nested => End::Run,
    }
}

pub(crate) fn is_object_assign(callee: &Expression) -> bool {
    is_object_method(callee, &["assign"])
}

fn passes_through(callee: &Expression) -> bool {
    is_object_method(callee, &["freeze", "seal", "preventExtensions"])
}

fn is_object_method(callee: &Expression, names: &[&str]) -> bool {
    matches!(
        callee.get_inner_expression(),
        Expression::StaticMemberExpression(member)
            if names.contains(&member.property.name.as_str())
                && matches!(&member.object, Expression::Identifier(object) if object.name == "Object")
    )
}

fn has_own_target(call: &CallExpression, bindings: &dyn Bindings) -> bool {
    call.arguments
        .first()
        .and_then(Argument::as_expression)
        .is_some_and(|target| is_own(target, bindings))
}

fn is_own(expression: &Expression, bindings: &dyn Bindings) -> bool {
    match expression.get_inner_expression() {
        Expression::ObjectExpression(_)
        | Expression::ArrayExpression(_)
        | Expression::ArrowFunctionExpression(_)
        | Expression::FunctionExpression(_)
        | Expression::ClassExpression(_) => true,
        Expression::Identifier(reference) => bindings.is_local(reference),
        Expression::StaticMemberExpression(member) => is_own(&member.object, bindings),
        Expression::CallExpression(call) => is_object_method(&call.callee, &["create"]),
        _ => false,
    }
}
