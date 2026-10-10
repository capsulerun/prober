//! What a function does while it is called with each parameter and each local it declares: runs it, reads it, or only keeps it in what it returns, possibly by handing it to functions that keep it.

use std::collections::{HashMap, HashSet};

use oxc_ast::ast::{BindingPattern, FormalParameters, Statement};
use oxc_semantic::SymbolId;
use oxc_span::Span;

use crate::symbols::placement::{End, Placement, bound_symbols};
use crate::symbols::{Keeping, Parameter, Use};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Question {
    Runs,
    Matters,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Flow {
    Run,
    Kept,
    Read,
    Through(Vec<SymbolId>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fate {
    pub flow: Flow,
    pub keeping: Vec<Keeping>,
    pub derived: bool,
    pub read: bool,
}

impl Fate {
    pub(crate) fn kept(keeping: Vec<Keeping>) -> Self {
        Self {
            flow: Flow::Kept,
            keeping,
            derived: false,
            read: false,
        }
    }

    pub(crate) fn run() -> Self {
        Self {
            flow: Flow::Run,
            ..Self::kept(Vec::new())
        }
    }

    pub(crate) fn through(locals: Vec<SymbolId>, keeping: Vec<Keeping>) -> Self {
        Self {
            flow: Flow::Through(locals),
            ..Self::kept(keeping)
        }
    }

    pub(crate) fn of(placement: Placement) -> Self {
        let flow = match placement.end {
            End::Stored | End::Dropped => Flow::Kept,
            End::Read => Flow::Read,
            End::Through(locals) => Flow::Through(locals),
            End::Run => Flow::Run,
        };

        Self {
            flow,
            keeping: placement.keeping,
            derived: placement.derived,
            read: placement.read,
        }
    }

    pub(crate) fn step(&self, question: Question) -> Step {
        let counts = match question {
            Question::Runs => !self.derived && self.flow == Flow::Run,
            Question::Matters => self.read || matches!(self.flow, Flow::Run | Flow::Read),
        };

        match &self.flow {
            _ if counts => Step::Counts,
            Flow::Through(locals) if question == Question::Matters || !self.derived => {
                Step::Through(locals.clone())
            }
            _ => Step::Kept,
        }
    }
}

pub(crate) enum Step {
    Counts,
    Kept,
    Through(Vec<SymbolId>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    Counts,
    Held(Vec<Keeping>),
    Kept,
}

impl Verdict {
    pub(crate) fn of(counts: bool, kept_by: Vec<Keeping>) -> Self {
        if counts {
            Verdict::Counts
        } else if kept_by.is_empty() {
            Verdict::Kept
        } else {
            Verdict::Held(kept_by)
        }
    }
}

pub(crate) struct Pending {
    pub locals: Vec<SymbolId>,
    pub question: Question,
    pub kept_by: Vec<Keeping>,
    pub uses: Vec<Use>,
    pub hole: Option<Span>,
}

pub(crate) struct FunctionFacts {
    parameters: Vec<(Vec<SymbolId>, bool)>,
    watched: HashSet<SymbolId>,
    fates: HashMap<SymbolId, Vec<Fate>>,
    pending: Vec<Pending>,
}

impl FunctionFacts {
    pub(crate) fn new(parameters: &FormalParameters, body: &[Statement]) -> Self {
        let mut fates: HashMap<SymbolId, Vec<Fate>> = HashMap::new();
        let mut positional: Vec<(Vec<SymbolId>, bool)> = parameters
            .items
            .iter()
            .map(|parameter| {
                let bound = bound_symbols(&parameter.pattern);

                if parameter.accessibility.is_some() || parameter.readonly || parameter.r#override {
                    for &symbol in &bound {
                        fates
                            .entry(symbol)
                            .or_default()
                            .push(Fate::kept(Vec::new()));
                    }
                }

                (bound, false)
            })
            .collect();

        if let Some(rest) = &parameters.rest {
            match &rest.rest.argument {
                BindingPattern::ArrayPattern(array) => {
                    positional.extend(array.elements.iter().map(|element| {
                        (
                            element.as_ref().map(bound_symbols).unwrap_or_default(),
                            false,
                        )
                    }));

                    if let Some(rest) = &array.rest {
                        positional.push((bound_symbols(&rest.argument), true));
                    }
                }
                pattern => positional.push((bound_symbols(pattern), true)),
            }
        }

        let parameters = positional;
        let mut watched: HashSet<SymbolId> = parameters
            .iter()
            .flat_map(|(bound, _)| bound)
            .copied()
            .collect();

        for statement in body {
            match statement {
                Statement::VariableDeclaration(variables) => {
                    for declarator in &variables.declarations {
                        if declarator.init.is_some() {
                            watched.extend(bound_symbols(&declarator.id));
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
            parameters,
            watched,
            fates,
            pending: Vec::new(),
        }
    }

    pub(crate) fn watches(&self, locals: &[SymbolId]) -> bool {
        locals.iter().all(|local| self.watched.contains(local))
    }

    pub(crate) fn record(&mut self, symbol: SymbolId, fate: Fate) {
        self.fates.entry(symbol).or_default().push(fate);
    }

    pub(crate) fn defer(&mut self, pending: Pending) {
        self.pending.push(pending);
    }

    pub(crate) fn finish(self) -> (Vec<Parameter>, Vec<(Verdict, Pending)>) {
        let parameters = self
            .parameters
            .iter()
            .map(|(bound, rest)| {
                let (kept, passed) = match self.verdict(bound, Question::Runs, &[]) {
                    Verdict::Counts => (false, Vec::new()),
                    Verdict::Held(passed) => (true, passed),
                    Verdict::Kept => (true, Vec::new()),
                };

                Parameter {
                    kept,
                    passed,
                    rest: *rest,
                }
            })
            .collect();
        let verdicts = self
            .pending
            .iter()
            .map(|pending| self.verdict(&pending.locals, pending.question, &pending.kept_by))
            .collect::<Vec<_>>();

        (parameters, verdicts.into_iter().zip(self.pending).collect())
    }

    fn verdict(&self, locals: &[SymbolId], question: Question, kept_by: &[Keeping]) -> Verdict {
        let mut passed = kept_by.to_vec();
        let mut visiting = HashSet::new();
        let counts = locals
            .iter()
            .any(|&local| self.counts(local, question, &mut passed, &mut visiting));

        Verdict::of(counts, if counts { kept_by.to_vec() } else { passed })
    }

    fn counts(
        &self,
        symbol: SymbolId,
        question: Question,
        passed: &mut Vec<Keeping>,
        visiting: &mut HashSet<SymbolId>,
    ) -> bool {
        if !visiting.insert(symbol) {
            return false;
        }

        self.fates.get(&symbol).into_iter().flatten().any(|fate| {
            passed.extend(fate.keeping.iter().cloned());

            match fate.step(question) {
                Step::Counts => true,
                Step::Kept => false,
                Step::Through(locals) => locals.into_iter().any(|local| {
                    !self.watched.contains(&local) || self.counts(local, question, passed, visiting)
                }),
            }
        })
    }
}
