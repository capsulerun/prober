//! What a function does with each parameter while it is called: runs it, or only keeps it in what it returns, possibly by passing it to another function that keeps it.

use std::collections::{HashMap, HashSet};

use oxc_ast::AstKind;
use oxc_ast::ast::{
    ArrowFunctionBody, BindingPattern, CallExpression, Expression, FormalParameters, Statement,
};
use oxc_semantic::SymbolId;
use oxc_span::{GetSpan, Span};

use crate::symbols::{Parameter, Use};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Position {
    Run,
    Kept,
    Passed(Use, usize),
    Through(SymbolId),
}

pub(crate) struct FunctionFacts {
    span: Span,
    parameters: Vec<Option<SymbolId>>,
    watched: HashSet<SymbolId>,
    positions: HashMap<SymbolId, Vec<Position>>,
}

impl FunctionFacts {
    pub(crate) fn new(span: Span, parameters: &FormalParameters, body: &[Statement]) -> Self {
        let parameters: Vec<Option<SymbolId>> = parameters
            .items
            .iter()
            .map(|parameter| match &parameter.pattern {
                BindingPattern::BindingIdentifier(identifier) => identifier.symbol_id.get(),
                _ => None,
            })
            .collect();
        let mut watched: HashSet<SymbolId> = parameters.iter().flatten().copied().collect();

        for statement in body {
            match statement {
                Statement::VariableDeclaration(variables) => {
                    for declarator in &variables.declarations {
                        if let (BindingPattern::BindingIdentifier(identifier), Some(init)) =
                            (&declarator.id, &declarator.init)
                            && holds_functions(init)
                        {
                            watched.extend(identifier.symbol_id.get());
                        }
                    }
                }
                Statement::FunctionDeclaration(function) => {
                    watched.extend(function.id.as_ref().and_then(|id| id.symbol_id.get()));
                }
                _ => {}
            }
        }

        Self {
            span,
            parameters,
            watched,
            positions: HashMap::new(),
        }
    }

    pub(crate) fn span(&self) -> Span {
        self.span
    }

    pub(crate) fn watches(&self, symbol: SymbolId) -> bool {
        self.watched.contains(&symbol)
    }

    pub(crate) fn record(&mut self, symbol: SymbolId, position: Position) {
        self.positions.entry(symbol).or_default().push(position);
    }

    pub(crate) fn into_parameters(self) -> Vec<Parameter> {
        self.parameters
            .iter()
            .map(|symbol| {
                let mut passed = Vec::new();
                let mut visiting = HashSet::new();
                let kept =
                    symbol.is_some_and(|symbol| self.kept(symbol, &mut passed, &mut visiting));

                Parameter {
                    kept,
                    passed: if kept { passed } else { Vec::new() },
                }
            })
            .collect()
    }

    fn kept(
        &self,
        symbol: SymbolId,
        passed: &mut Vec<(Use, usize)>,
        visiting: &mut HashSet<SymbolId>,
    ) -> bool {
        if !visiting.insert(symbol) {
            return true;
        }

        self.positions
            .get(&symbol)
            .into_iter()
            .flatten()
            .all(|position| match position {
                Position::Run => false,
                Position::Kept => true,
                Position::Passed(callee, argument) => {
                    passed.push((callee.clone(), *argument));

                    true
                }
                Position::Through(local) => {
                    self.watched.contains(local) && self.kept(*local, passed, visiting)
                }
            })
    }
}

pub(crate) fn position(
    ancestors: &[AstKind],
    function_span: Span,
    reference_span: Span,
    callee_use: &dyn Fn(&CallExpression) -> Option<Use>,
) -> Position {
    let Some(function_index) = ancestors
        .iter()
        .rposition(|kind| is_function(kind) && kind.span() == function_span)
    else {
        return Position::Run;
    };
    let nested = ancestors[function_index + 1..]
        .iter()
        .position(is_function)
        .map(|offset| function_index + 1 + offset);
    let (mut index, mut span) = match nested {
        Some(nested) => (nested, ancestors[nested].span()),
        None => (ancestors.len(), reference_span),
    };

    while index > function_index + 1 {
        index -= 1;

        match ancestors[index] {
            AstKind::ParenthesizedExpression(_)
            | AstKind::TSAsExpression(_)
            | AstKind::TSSatisfiesExpression(_)
            | AstKind::TSNonNullExpression(_)
            | AstKind::ObjectProperty(_)
            | AstKind::ObjectExpression(_)
            | AstKind::ArrayExpression(_)
            | AstKind::ConditionalExpression(_)
            | AstKind::LogicalExpression(_) => span = ancestors[index].span(),
            AstKind::ReturnStatement(_) => return Position::Kept,
            AstKind::CallExpression(call) => {
                let argument = call
                    .arguments
                    .iter()
                    .position(|argument| argument.span() == span);

                return match (argument, callee_use(call)) {
                    (Some(argument), Some(callee)) => Position::Passed(callee, argument),
                    _ => Position::Run,
                };
            }
            AstKind::VariableDeclarator(declarator) => {
                return match (&declarator.id, &declarator.init) {
                    (BindingPattern::BindingIdentifier(identifier), Some(init))
                        if init.span() == span =>
                    {
                        identifier
                            .symbol_id
                            .get()
                            .map_or(Position::Run, Position::Through)
                    }
                    _ => Position::Run,
                };
            }
            AstKind::FunctionBody(_) => {
                return match ancestors.get(index + 1) {
                    Some(AstKind::Function(function)) => function
                        .id
                        .as_ref()
                        .and_then(|id| id.symbol_id.get())
                        .map_or(Position::Run, Position::Through),
                    _ => Position::Run,
                };
            }
            _ => return Position::Run,
        }
    }

    match ancestors[function_index] {
        AstKind::ArrowFunctionExpression(arrow) => match &arrow.body {
            ArrowFunctionBody::FunctionBody(_) => Position::Run,
            body => {
                if body.to_expression().span() == span {
                    Position::Kept
                } else {
                    Position::Run
                }
            }
        },
        _ => Position::Run,
    }
}

fn is_function(kind: &AstKind) -> bool {
    matches!(
        kind,
        AstKind::Function(_) | AstKind::ArrowFunctionExpression(_)
    )
}

fn holds_functions(expression: &Expression) -> bool {
    matches!(
        expression.get_inner_expression(),
        Expression::ArrowFunctionExpression(_)
            | Expression::FunctionExpression(_)
            | Expression::ObjectExpression(_)
            | Expression::ArrayExpression(_)
    )
}
