//! Partial evaluation across modules: the value of a summarized expression, the loads a call reaches, the files a file access reaches, and whether every call of a loader is visible.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use prober_core::files::RepoPath;

use crate::conventions::is_source_file;
use crate::dynamic::spawn::{Spawned, launch};
use crate::dynamic::summarizer::{SummaryScope, summarize_module};
use crate::dynamic::summary::{
    Access, Action, Comparison, Effect, Expr, FunctionIndex, ImportedName, ModuleSummary, Reach,
    UseKind,
};
use crate::dynamic::value::{
    AccessKind, Builtin, FunctionRef, Part, PathFunction, Pattern, SpawnKind, Unknown, Value,
    alternatives, concatenation, file_url_to_path, is_open_object, merge_config, merge_objects,
    path_dirname, path_join, path_resolve, require_resolve, url,
};
use crate::presets::{VITEST_DEFAULT_EXCLUDE, VITEST_DEFAULT_INCLUDE};
use crate::resolver::{ModuleResolver, Resolution};

const MAX_CALL_DEPTH: usize = 8;

const MAX_NESTING: usize = 32;

const MAX_REEXPORT_DEPTH: usize = 4;

const MAX_EVALUATION_STEPS: usize = 20_000;

const MAX_ACCESS_STEPS: usize = 2_000;

pub(crate) type SharedSummaries = HashMap<RepoPath, Arc<ModuleSummary>>;

const LOADING_BUILTIN_MODULES: [&str; 8] = [
    "path",
    "path/posix",
    "url",
    "module",
    "child_process",
    "execa",
    "cross-spawn",
    "jiti",
];

const NODE_EXECUTABLE: &str = "node";

pub(crate) struct Load {
    pub specifier: Value,
    pub base: LoadBase,
}

pub(crate) enum LoadBase {
    Module(RepoPath),
    Require(Vec<Pattern>),
    WorkingDirectory,
    Process(Vec<Pattern>),
    Program(Vec<Pattern>),
}

impl Load {
    pub fn is_spawn(&self) -> bool {
        matches!(self.base, LoadBase::Process(_) | LoadBase::Program(_))
    }
}

pub(crate) struct FileAccess {
    pub kind: AccessKind,
    pub path: Value,
    pub options: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadScope {
    Modules,
    Processes,
}

#[derive(Clone)]
struct Frame {
    function: FunctionRef,
    arguments: Vec<Value>,
}

#[derive(Clone)]
struct Context {
    module: RepoPath,
    frames: Vec<Frame>,
}

impl Context {
    fn top(module: &RepoPath) -> Self {
        Self {
            module: module.clone(),
            frames: Vec::new(),
        }
    }

    fn enter(&self, function: &FunctionRef, arguments: Vec<Value>) -> Self {
        let mut frames = self.frames.clone();

        frames.push(Frame {
            function: function.clone(),
            arguments,
        });

        Self {
            module: function.module.clone(),
            frames,
        }
    }

    fn is_running(&self, function: &FunctionRef) -> bool {
        self.frames.iter().any(|frame| frame.function == *function)
    }

    fn function(&self, index: FunctionIndex) -> FunctionRef {
        FunctionRef {
            module: self.module.clone(),
            index,
        }
    }
}

pub(crate) struct Evaluator<'e> {
    repo_root: &'e Path,
    resolver: &'e ModuleResolver,
    importers: &'e HashMap<RepoPath, Vec<RepoPath>>,
    summaries: RefCell<HashMap<RepoPath, Option<Arc<ModuleSummary>>>>,
    shared_summaries: Option<&'e SharedSummaries>,
    analysed: RefCell<HashSet<RepoPath>>,
    resolutions: RefCell<HashMap<(RepoPath, String), Resolution>>,
    loaders: RefCell<HashMap<FunctionRef, bool>>,
    parametric_loaders: RefCell<HashMap<FunctionRef, bool>>,
    coverage: RefCell<HashMap<FunctionRef, bool>>,
    nesting: Cell<usize>,
    steps: Cell<usize>,
    evaluates_spreads: Cell<bool>,
    step_limit: Cell<usize>,
    working_directory: RefCell<Value>,
    process_directory: RefCell<Value>,
    summary_scope: SummaryScope,
    load_scope: LoadScope,
}

impl<'e> Evaluator<'e> {
    pub fn new(
        repo_root: &'e Path,
        resolver: &'e ModuleResolver,
        importers: &'e HashMap<RepoPath, Vec<RepoPath>>,
    ) -> Self {
        Self {
            repo_root,
            resolver,
            importers,
            summaries: RefCell::new(HashMap::new()),
            shared_summaries: None,
            analysed: RefCell::new(HashSet::new()),
            resolutions: RefCell::new(HashMap::new()),
            loaders: RefCell::new(HashMap::new()),
            parametric_loaders: RefCell::new(HashMap::new()),
            coverage: RefCell::new(HashMap::new()),
            nesting: Cell::new(0),
            steps: Cell::new(0),
            evaluates_spreads: Cell::new(false),
            step_limit: Cell::new(MAX_EVALUATION_STEPS),
            working_directory: RefCell::new(Value::Unknown(Unknown::working_directory())),
            process_directory: RefCell::new(Value::runtime_value()),
            summary_scope: SummaryScope::Loads,
            load_scope: LoadScope::Modules,
        }
    }

    pub fn with_load_scope(self, load_scope: LoadScope) -> Self {
        Self { load_scope, ..self }
    }

    pub fn with_shared_summaries(self, shared_summaries: &'e SharedSummaries) -> Self {
        Self {
            shared_summaries: Some(shared_summaries),
            ..self
        }
    }

    pub fn analysed_summaries(&self) -> SharedSummaries {
        let summaries = self.summaries.borrow();

        self.analysed
            .borrow()
            .iter()
            .filter_map(|module| Some((module.clone(), summaries.get(module)?.clone()?)))
            .collect()
    }

    pub fn with_summary_scope(self, summary_scope: SummaryScope) -> Self {
        Self {
            summary_scope,
            ..self
        }
    }

    pub fn set_process_directory(&self, process_directory: Value) {
        self.process_directory.replace(process_directory);
    }

    pub fn add_analysed(&self, module: RepoPath, summary: ModuleSummary) {
        self.add_summary(module.clone(), summary);
        self.analysed.borrow_mut().insert(module);
    }

    pub fn add_summary(&self, module: RepoPath, summary: ModuleSummary) {
        self.summaries
            .borrow_mut()
            .insert(module, Some(Arc::new(summary)));
    }

    pub fn is_analysed(&self, module: &RepoPath) -> bool {
        self.shared_summaries
            .is_some_and(|shared_summaries| shared_summaries.contains_key(module))
            || self.analysed.borrow().contains(module)
    }

    pub fn summary(&self, module: &RepoPath) -> Option<Arc<ModuleSummary>> {
        if let Some(summary) = self
            .shared_summaries
            .and_then(|shared_summaries| shared_summaries.get(module))
        {
            return Some(Arc::clone(summary));
        }

        if let Some(summary) = self.summaries.borrow().get(module) {
            return summary.clone();
        }

        let summary = is_source_file(module)
            .then(|| fs::read(self.repo_root.join(module.as_str())).ok())
            .flatten()
            .map(|source_bytes| {
                Arc::new(summarize_module(
                    module,
                    &String::from_utf8_lossy(&source_bytes),
                    self.summary_scope,
                ))
            });

        self.summaries
            .borrow_mut()
            .insert(module.clone(), summary.clone());

        summary
    }

    pub fn evaluate_config(&self, module: &RepoPath) -> Value {
        self.steps.set(0);

        let context = Context::top(module);

        match self.export(module, "default", 0) {
            Value::Function(function) => {
                self.call_function(&context, &function, vec![Value::runtime_value()])
            }
            config => self.call(
                &context,
                Value::Builtin(Builtin::DefineConfig),
                vec![config],
            ),
        }
    }

    pub fn file_access(
        &self,
        module: &RepoPath,
        access: &Access,
        working_directory: Value,
    ) -> Option<FileAccess> {
        self.steps.set(0);

        let context = Context::top(module);
        let kind = match self.evaluate_callee(&context, &access.callee) {
            Value::Builtin(Builtin::FileAccess(kind)) => kind,
            Value::Builtin(Builtin::GlobModule) => AccessKind::Glob,
            _ => return None,
        };
        let outer_directory = self.working_directory.replace(working_directory);
        let outer_limit = self.step_limit.replace(MAX_ACCESS_STEPS);
        let mut arguments = access
            .arguments
            .iter()
            .map(|argument| self.evaluate(&context, argument));
        let path = arguments.next().unwrap_or_else(Value::runtime_value);
        let options = arguments.next().unwrap_or_else(Value::runtime_value);

        self.step_limit.set(outer_limit);
        self.working_directory.replace(outer_directory);

        Some(FileAccess {
            kind,
            path,
            options,
        })
    }

    pub fn resolve(&self, importer: &RepoPath, specifier: &str) -> Resolution {
        let key = (importer.clone(), specifier.to_owned());

        if let Some(resolution) = self.resolutions.borrow().get(&key) {
            return resolution.clone();
        }

        let resolution = self.resolver.resolve_specifier(importer, specifier);

        self.resolutions
            .borrow_mut()
            .insert(key, resolution.clone());

        resolution
    }

    pub fn effect_loads(&self, module: &RepoPath, effect: &Effect) -> Vec<Load> {
        self.steps.set(0);

        self.action_loads(&Context::top(module), &effect.action)
    }

    pub fn is_load(&self, module: &RepoPath, effect: &Effect) -> bool {
        match &effect.action {
            Action::Import(_) | Action::Worker(_) => true,
            Action::Call { callee, .. } => {
                is_require(&self.evaluate_callee(&Context::top(module), callee))
            }
        }
    }

    pub fn exports_parametric_loader(&self, module: &RepoPath) -> bool {
        let Some(summary) = self.summary(module) else {
            return false;
        };

        summary.exports.iter().any(|(_, exported)| {
            match self.evaluate(&Context::top(module), exported) {
                Value::Function(function) => self.is_parametric_loader(&function),
                _ => false,
            }
        })
    }

    pub fn is_reached_through_calls(
        &self,
        module: &RepoPath,
        effect: &Effect,
        load: &Load,
    ) -> bool {
        let Some(unit) = effect.unit else {
            return false;
        };
        let unit = FunctionRef {
            module: module.clone(),
            index: unit,
        };
        let unknowns = load.specifier.unknowns();

        unknowns
            .iter()
            .any(|unknown| unknown.parameter_of.as_ref() == Some(&unit))
            && self.is_covered(&unit)
    }

    fn is_parametric_loader(&self, function: &FunctionRef) -> bool {
        if let Some(&parametric) = self.parametric_loaders.borrow().get(function) {
            return parametric;
        }

        self.parametric_loaders
            .borrow_mut()
            .insert(function.clone(), false);

        let site_steps = self.steps.replace(0);
        let parametric = self.summary(&function.module).is_some_and(|summary| {
            let context = Context::top(&function.module);

            unit_effects(&summary, function.index)
                .flat_map(|effect| self.action_loads(&context, &effect.action))
                .any(|load| {
                    load.specifier
                        .unknowns()
                        .iter()
                        .any(|unknown| unknown.parameter_of.as_ref() == Some(function))
                })
        });

        self.steps.set(site_steps);
        self.parametric_loaders
            .borrow_mut()
            .insert(function.clone(), parametric);

        parametric
    }

    fn is_covered(&self, function: &FunctionRef) -> bool {
        if let Some(&covered) = self.coverage.borrow().get(function) {
            return covered;
        }

        self.coverage.borrow_mut().insert(function.clone(), false);

        let covered = self.summary(&function.module).is_some_and(|summary| {
            match &summary.functions[function.index].reach {
                Reach::Named {
                    exported_as,
                    escapes,
                } => {
                    !escapes
                        && exported_as.iter().all(|exported_name| {
                            self.importers_only_call(&function.module, exported_name, 0)
                        })
                }
                Reach::ParameterDefault {
                    function: owner,
                    escapes,
                    ..
                } => {
                    !escapes
                        && self.is_covered(&FunctionRef {
                            module: function.module.clone(),
                            index: *owner,
                        })
                }
                Reach::Nested => false,
            }
        });

        self.coverage.borrow_mut().insert(function.clone(), covered);

        covered
    }

    fn importers_only_call(&self, module: &RepoPath, exported_name: &str, depth: usize) -> bool {
        if depth > MAX_REEXPORT_DEPTH {
            return false;
        }

        let Some(importers) = self.importers.get(module) else {
            return true;
        };

        importers.iter().all(|importer| {
            let Some(summary) = self
                .is_analysed(importer)
                .then(|| self.summary(importer))
                .flatten()
            else {
                return false;
            };

            summary
                .import_uses
                .iter()
                .filter(|import_use| {
                    self.resolve(importer, &import_use.specifier)
                        == Resolution::RepoFile(module.clone())
                })
                .all(|import_use| match (&import_use.name, &import_use.kind) {
                    (ImportedName::Namespace, UseKind::ReexportAll) => {
                        self.importers_only_call(importer, exported_name, depth + 1)
                    }
                    (ImportedName::Namespace, _) => false,
                    (used_name, kind) if names_export(used_name, exported_name) => match kind {
                        UseKind::Call => true,
                        UseKind::Exported(alias) => {
                            self.importers_only_call(importer, alias, depth + 1)
                        }
                        UseKind::ReexportAll | UseKind::Escape => false,
                    },
                    _ => true,
                })
        })
    }

    fn action_loads(&self, context: &Context, action: &Action) -> Vec<Load> {
        match action {
            Action::Import(_) | Action::Worker(_) if self.load_scope == LoadScope::Processes => {
                Vec::new()
            }
            Action::Import(specifier) => vec![Load {
                specifier: self.evaluate(context, specifier),
                base: LoadBase::Module(context.module.clone()),
            }],
            Action::Worker(specifier) => vec![Load {
                specifier: self.evaluate(context, specifier),
                base: LoadBase::WorkingDirectory,
            }],
            Action::Call { callee, .. } if !self.may_reach_a_loader(context, callee) => Vec::new(),
            Action::Call { callee, arguments } => {
                self.callee_loads(context, self.evaluate_callee(context, callee), arguments)
            }
        }
    }

    fn callee_loads(&self, context: &Context, callee: Value, arguments: &[Expr]) -> Vec<Load> {
        match callee {
            Value::Builtin(Builtin::Spawn(kind)) if self.load_scope == LoadScope::Processes => {
                self.spawn_loads(context, kind, arguments)
            }
            Value::Require(bases) if self.load_scope == LoadScope::Modules => vec![Load {
                specifier: arguments
                    .first()
                    .map_or_else(Value::runtime_value, |specifier| {
                        self.evaluate(context, specifier)
                    }),
                base: LoadBase::Require(bases),
            }],
            Value::Function(function) => self.function_loads(context, &function, arguments),
            Value::Alternatives(callees) => callees
                .into_iter()
                .flat_map(|callee| self.callee_loads(context, callee, arguments))
                .collect(),
            _ => Vec::new(),
        }
    }

    fn spawn_loads(&self, context: &Context, kind: SpawnKind, arguments: &[Expr]) -> Vec<Load> {
        let arguments = self.evaluate_all(context, arguments);
        let launch = launch(kind, &arguments, &self.process_directory.borrow());

        launch
            .spawned
            .into_iter()
            .map(|spawned| match spawned {
                Spawned::Script(script) => Load {
                    specifier: script,
                    base: LoadBase::Process(launch.directories.clone()),
                },
                Spawned::Program(name) => Load {
                    specifier: Value::text(&name),
                    base: LoadBase::Program(launch.directories.clone()),
                },
            })
            .collect()
    }

    fn may_load(&self, function: &FunctionRef) -> bool {
        if let Some(&may_load) = self.loaders.borrow().get(function) {
            return may_load;
        }

        self.loaders.borrow_mut().insert(function.clone(), false);

        let site_steps = self.steps.replace(0);
        let context = Context::top(&function.module);
        let may_load = self.summary(&function.module).is_some_and(|summary| {
            unit_effects(&summary, function.index).any(|effect| match &effect.action {
                Action::Import(_) | Action::Worker(_) => self.load_scope == LoadScope::Modules,
                Action::Call { callee, .. } => {
                    self.may_reach_a_loader(&context, callee)
                        && self.callee_may_load(self.evaluate_callee(&context, callee))
                }
            })
        });

        self.steps.set(site_steps);
        self.loaders.borrow_mut().insert(function.clone(), may_load);

        may_load
    }

    fn callee_may_load(&self, callee: Value) -> bool {
        match callee {
            Value::Require(_) => self.load_scope == LoadScope::Modules,
            Value::Builtin(Builtin::Spawn(_)) => self.load_scope == LoadScope::Processes,
            Value::Function(function) => {
                self.is_analysed(&function.module) && self.may_load(&function)
            }
            Value::Alternatives(callees) => callees
                .into_iter()
                .any(|callee| self.callee_may_load(callee)),
            _ => false,
        }
    }

    fn may_reach_a_loader(&self, context: &Context, callee: &Expr) -> bool {
        let imported_specifier = match callee {
            Expr::Imported { specifier, .. } => specifier,
            Expr::Member { object, .. } => match object.as_ref() {
                Expr::Imported { specifier, .. } => specifier,
                _ => return true,
            },
            _ => return true,
        };
        let builtin_module = imported_specifier
            .strip_prefix("node:")
            .unwrap_or(imported_specifier);

        if LOADING_BUILTIN_MODULES.contains(&builtin_module) {
            return true;
        }

        match self.resolve(&context.module, imported_specifier) {
            Resolution::RepoFile(module) => self.is_analysed(&module),
            Resolution::Outside | Resolution::Missing => false,
        }
    }

    fn function_loads(
        &self,
        context: &Context,
        function: &FunctionRef,
        arguments: &[Expr],
    ) -> Vec<Load> {
        if !self.is_analysed(&function.module)
            || context.is_running(function)
            || !self.may_load(function)
        {
            return Vec::new();
        }

        if self.load_scope == LoadScope::Processes && !self.is_parametric_loader(function) {
            return Vec::new();
        }

        let Some(summary) = self.summary(&function.module) else {
            return Vec::new();
        };

        if context.frames.len() >= MAX_CALL_DEPTH {
            return (self.load_scope == LoadScope::Modules)
                .then(|| Load {
                    specifier: Value::runtime_value(),
                    base: LoadBase::Module(function.module.clone()),
                })
                .into_iter()
                .collect();
        }

        let argument_values = arguments
            .iter()
            .map(|argument| self.evaluate(context, argument))
            .collect();
        let callee_context = context.enter(function, argument_values);

        unit_effects(&summary, function.index)
            .flat_map(|effect| self.action_loads(&callee_context, &effect.action))
            .collect()
    }

    fn evaluate(&self, context: &Context, expr: &Expr) -> Value {
        if self.steps.get() >= self.step_limit.get() {
            return Value::runtime_value();
        }

        self.steps.set(self.steps.get() + 1);

        match expr {
            Expr::Text(text) => Value::text(text),
            Expr::Boolean(boolean) => Value::Boolean(*boolean),
            Expr::ModuleFile => module_file(&context.module),
            Expr::ModuleDirectory => path_dirname(&module_file(&context.module)),
            Expr::ModuleResolve => Value::Builtin(Builtin::RequireResolve(
                module_file(&context.module).patterns(),
            )),
            Expr::Concat(parts) => concatenation(&self.evaluate_all(context, parts)),
            Expr::Choice(choices) => alternatives(self.evaluate_all(context, choices)),
            Expr::Conditional {
                comparison,
                consequent,
                alternate,
            } => match comparison
                .as_ref()
                .and_then(|comparison| self.decide(context, comparison))
            {
                Some(true) => self.evaluate(context, consequent),
                Some(false) => self.evaluate(context, alternate),
                None => alternatives(vec![
                    self.evaluate(context, consequent),
                    self.evaluate(context, alternate),
                ]),
            },
            Expr::Array(items) => Value::Array(self.array_items(context, items)),
            Expr::Spread(_) => Value::runtime_value(),
            Expr::Object(properties) => Value::Object(
                properties
                    .iter()
                    .map(|(key, property)| (key.clone(), self.evaluate(context, property)))
                    .collect(),
            ),
            Expr::Merge(parts) if self.evaluates_spreads.get() => {
                merge_objects(self.evaluate_all(context, parts))
            }
            Expr::Merge(_) => Value::runtime_value(),
            Expr::ElementOf(array) => element_of(self.evaluate(context, array)),
            Expr::Parameter { function, index } => {
                self.parameter(context, *function, *index, false)
            }
            Expr::Function(index) => Value::Function(context.function(*index)),
            Expr::Imported { specifier, name } => self.imported(context, specifier, name),
            Expr::Global(name) => global(name, &context.module),
            Expr::Member { object, property } => {
                self.member(self.evaluate(context, object), property)
            }
            Expr::Call { callee, arguments } => {
                let callee = self.evaluate_callee(context, callee);
                let arguments = self.evaluate_all(context, arguments);

                self.call(context, callee, arguments)
            }
            Expr::New { callee, arguments } => match self.evaluate(context, callee) {
                Value::Builtin(Builtin::UrlConstructor) => {
                    let arguments = self.evaluate_all(context, arguments);

                    match arguments.first() {
                        Some(input) => url(input, arguments.get(1)),
                        None => Value::runtime_value(),
                    }
                }
                _ => Value::runtime_value(),
            },
            Expr::Unknown => Value::runtime_value(),
        }
    }

    fn array_items(&self, context: &Context, items: &[Expr]) -> Vec<Value> {
        let mut values = Vec::new();

        for item in items {
            match item {
                Expr::Spread(spread) => match self.evaluate(context, spread) {
                    Value::Array(spread_items) => values.extend(spread_items),
                    spread_value => values.push(element_of(spread_value)),
                },
                item => values.push(self.evaluate(context, item)),
            }
        }

        values
    }

    fn evaluate_all(&self, context: &Context, exprs: &[Expr]) -> Vec<Value> {
        exprs
            .iter()
            .map(|expr| self.evaluate(context, expr))
            .collect()
    }

    fn evaluate_callee(&self, context: &Context, callee: &Expr) -> Value {
        match callee {
            Expr::Parameter { function, index } => self.parameter(context, *function, *index, true),
            callee => self.evaluate(context, callee),
        }
    }

    fn decide(&self, context: &Context, comparison: &Comparison) -> Option<bool> {
        let left = self.evaluate(context, &comparison.left).single_literal()?;
        let right = self.evaluate(context, &comparison.right).single_literal()?;

        Some((left == right) == comparison.equal)
    }

    fn parameter(
        &self,
        context: &Context,
        function: FunctionIndex,
        index: usize,
        as_callee: bool,
    ) -> Value {
        let function = context.function(function);
        let frame = context
            .frames
            .iter()
            .rev()
            .find(|frame| frame.function == function);

        if let Some(frame) = frame {
            return frame
                .arguments
                .get(index)
                .cloned()
                .or_else(|| self.parameter_default(context, &function, index))
                .unwrap_or_else(Value::runtime_value);
        }

        if as_callee && let Some(default) = self.parameter_default(context, &function, index) {
            return default;
        }

        Value::Unknown(Unknown::parameter_of(function))
    }

    fn parameter_default(
        &self,
        context: &Context,
        function: &FunctionRef,
        index: usize,
    ) -> Option<Value> {
        let summary = self.summary(&function.module)?;
        let default = summary.functions[function.index]
            .parameter_defaults
            .get(index)?
            .as_ref()?;

        Some(self.evaluate(context, default))
    }

    fn imported(&self, context: &Context, specifier: &str, name: &ImportedName) -> Value {
        if let Some(builtin) = builtin_import(specifier, name) {
            return builtin;
        }

        match self.resolve(&context.module, specifier) {
            Resolution::RepoFile(module) => match name {
                ImportedName::Namespace => Value::Module(module),
                ImportedName::Default => self.export(&module, "default", 0),
                ImportedName::Named(exported_name) => self.export(&module, exported_name, 0),
            },
            Resolution::Outside | Resolution::Missing => Value::runtime_value(),
        }
    }

    fn export(&self, module: &RepoPath, exported_name: &str, depth: usize) -> Value {
        if depth > MAX_REEXPORT_DEPTH {
            return Value::runtime_value();
        }

        let Some(summary) = self.summary(module) else {
            return Value::runtime_value();
        };

        if let Some(exported) = summary.export(exported_name) {
            return self.nested(|| self.evaluate(&Context::top(module), exported));
        }

        for star_specifier in &summary.star_exports {
            if let Resolution::RepoFile(target) = self.resolve(module, star_specifier) {
                let value = self.export(&target, exported_name, depth + 1);

                if !matches!(value, Value::Unknown(_)) {
                    return value;
                }
            }
        }

        Value::runtime_value()
    }

    fn member(&self, object: Value, property: &str) -> Value {
        match object {
            Value::Strings(_) if matches!(property, "href" | "pathname") => object,
            Value::Strings(_) if property == "toString" => Value::Method {
                receiver: Box::new(object),
                name: property.to_owned(),
            },
            Value::Builtin(Builtin::PathModule) => path_member(property),
            Value::Builtin(Builtin::UrlModule) => url_member(property),
            Value::Builtin(Builtin::ModuleModule) => module_member(property),
            Value::Builtin(Builtin::FsModule) => fs_member(property),
            Value::Builtin(Builtin::GlobModule) => glob_member(property),
            Value::Builtin(Builtin::Process) => match property {
                "cwd" => Value::Builtin(Builtin::ProcessCwd),
                "execPath" | "argv0" => Value::text(NODE_EXECUTABLE),
                _ => Value::runtime_value(),
            },
            Value::Builtin(Builtin::ChildProcessModule) => child_process_member(property),
            Value::Builtin(Builtin::Spawn(kind)) if property == "sync" => {
                Value::Builtin(Builtin::Spawn(kind))
            }
            Value::Builtin(Builtin::ObjectConstructor) => match property {
                "keys" => Value::Builtin(Builtin::ObjectKeys),
                "values" => Value::Builtin(Builtin::ObjectValues),
                _ => Value::runtime_value(),
            },
            Value::Require(bases) if matches!(property, "resolve" | "esmResolve") => {
                Value::Builtin(Builtin::RequireResolve(bases))
            }
            Value::Require(bases) if property == "import" => Value::Require(bases),
            Value::Module(module) => self.export(&module, property, 0),
            Value::Object(properties) => properties
                .into_iter()
                .find(|(key, _)| key == property)
                .map_or_else(Value::runtime_value, |(_, value)| value),
            Value::Array(_) => Value::Method {
                receiver: Box::new(object),
                name: property.to_owned(),
            },
            Value::Alternatives(values) => alternatives(
                values
                    .into_iter()
                    .map(|value| self.member(value, property))
                    .collect(),
            ),
            Value::Unknown(unknown) => Value::Unknown(unknown),
            _ => Value::runtime_value(),
        }
    }

    fn call(&self, context: &Context, callee: Value, arguments: Vec<Value>) -> Value {
        let first_argument = arguments
            .first()
            .cloned()
            .unwrap_or_else(Value::runtime_value);

        match callee {
            Value::Builtin(Builtin::Path(PathFunction::Join)) => path_join(&arguments),
            Value::Builtin(Builtin::Path(PathFunction::Resolve)) => {
                path_resolve(&arguments, &self.working_directory.borrow())
            }
            Value::Builtin(Builtin::ProcessCwd) => self.working_directory.borrow().clone(),
            Value::Builtin(Builtin::Path(PathFunction::Dirname)) => path_dirname(&first_argument),
            Value::Builtin(Builtin::Path(PathFunction::Normalize)) => path_join(&[first_argument]),
            Value::Builtin(Builtin::PathToFileUrl | Builtin::StringFunction) => first_argument,
            Value::Builtin(Builtin::FileUrlToPath) => file_url_to_path(&first_argument),
            Value::Builtin(Builtin::CreateRequire) => {
                Value::Require(file_url_to_path(&first_argument).patterns())
            }
            Value::Builtin(Builtin::RequireResolve(bases)) => {
                require_resolve(&first_argument, &bases)
            }
            Value::Builtin(Builtin::ObjectKeys | Builtin::ObjectValues)
                if is_open_object(&first_argument) =>
            {
                Value::runtime_value()
            }
            Value::Builtin(Builtin::DefineConfig) => match first_argument {
                Value::Function(function) => {
                    self.call_function(context, &function, vec![Value::runtime_value()])
                }
                config => config,
            },
            Value::Builtin(Builtin::MergeConfig) => {
                let overrides = arguments
                    .get(1)
                    .cloned()
                    .unwrap_or_else(Value::runtime_value);

                merge_config(first_argument, overrides)
            }
            Value::Builtin(Builtin::ObjectKeys) => match first_argument {
                Value::Object(properties) => {
                    Value::Array(properties.iter().map(|(key, _)| Value::text(key)).collect())
                }
                _ => Value::runtime_value(),
            },
            Value::Builtin(Builtin::ObjectValues) => match first_argument {
                Value::Object(properties) => {
                    Value::Array(properties.into_iter().map(|(_, value)| value).collect())
                }
                _ => Value::runtime_value(),
            },
            Value::Method { receiver, name } => match (*receiver, name.as_str()) {
                (Value::Array(items), "join") => {
                    let separator = arguments
                        .first()
                        .and_then(Value::single_literal)
                        .unwrap_or_else(|| ",".to_owned());
                    let mut joined = Vec::new();

                    for (position, item) in items.into_iter().enumerate() {
                        if position > 0 {
                            joined.push(Value::text(&separator));
                        }

                        joined.push(item);
                    }

                    concatenation(&joined)
                }
                (Value::Array(items), "map") => match first_argument {
                    callback @ (Value::Function(_) | Value::Alternatives(_)) => Value::Array(
                        items
                            .into_iter()
                            .map(|item| self.call(context, callback.clone(), vec![item]))
                            .collect(),
                    ),
                    _ => Value::runtime_value(),
                },
                (Value::Array(items), "filter") => Value::Array(items),
                (Value::Array(mut items), "concat") => {
                    for argument in arguments {
                        match argument {
                            Value::Array(more_items) => items.extend(more_items),
                            argument => items.push(argument),
                        }
                    }

                    Value::Array(items)
                }
                (receiver @ Value::Strings(_), "toString") => receiver,
                _ => Value::runtime_value(),
            },
            Value::Function(function) => self.call_function(context, &function, arguments),
            Value::Require(bases) => self.required_module(&bases, &first_argument),
            Value::Alternatives(callees) => alternatives(
                callees
                    .into_iter()
                    .filter(|callee| !matches!(callee, Value::Unknown(_) | Value::Strings(_)))
                    .map(|callee| self.call(context, callee, arguments.clone()))
                    .collect(),
            ),
            Value::Unknown(unknown) => Value::Unknown(unknown),
            _ => Value::runtime_value(),
        }
    }

    fn call_function(
        &self,
        context: &Context,
        function: &FunctionRef,
        arguments: Vec<Value>,
    ) -> Value {
        if context.frames.len() >= MAX_CALL_DEPTH || context.is_running(function) {
            return Value::runtime_value();
        }

        let Some(summary) = self.summary(&function.module) else {
            return Value::runtime_value();
        };
        let callee_context = context.enter(function, arguments);
        let returns = &summary.functions[function.index].returns;

        if returns.is_empty() {
            return Value::runtime_value();
        }

        self.nested(|| alternatives(self.evaluate_all(&callee_context, returns)))
    }

    fn required_module(&self, bases: &[Pattern], specifier: &Value) -> Value {
        if let Some(builtin) = specifier
            .single_literal()
            .and_then(|specifier| builtin_import(&specifier, &ImportedName::Namespace))
        {
            return builtin;
        }

        let base_module = match bases {
            [base] => repo_module(base),
            _ => None,
        };

        match (base_module, specifier.single_literal()) {
            (Some(base_module), Some(specifier)) => match self.resolve(&base_module, &specifier) {
                Resolution::RepoFile(module) => Value::Module(module),
                Resolution::Outside | Resolution::Missing => Value::runtime_value(),
            },
            _ => Value::runtime_value(),
        }
    }

    fn nested(&self, evaluate: impl FnOnce() -> Value) -> Value {
        if self.nesting.get() >= MAX_NESTING {
            return Value::runtime_value();
        }

        self.nesting.set(self.nesting.get() + 1);

        let value = evaluate();

        self.nesting.set(self.nesting.get() - 1);

        value
    }
}

fn is_require(callee: &Value) -> bool {
    match callee {
        Value::Require(_) => true,
        Value::Alternatives(callees) => callees.iter().any(is_require),
        _ => false,
    }
}

fn element_of(array: Value) -> Value {
    match array {
        Value::Array(items) => alternatives(items),
        Value::Alternatives(arrays) => alternatives(arrays.into_iter().map(element_of).collect()),
        Value::Unknown(unknown) => Value::Unknown(unknown),
        _ => Value::runtime_value(),
    }
}

fn unit_effects(summary: &ModuleSummary, unit: FunctionIndex) -> impl Iterator<Item = &Effect> {
    summary
        .effects
        .iter()
        .filter(move |effect| effect.unit == Some(unit))
}

fn names_export(used_name: &ImportedName, exported_name: &str) -> bool {
    match used_name {
        ImportedName::Named(name) => name == exported_name,
        ImportedName::Default => exported_name == "default",
        ImportedName::Namespace => false,
    }
}

fn module_file(module: &RepoPath) -> Value {
    Value::Strings(vec![Pattern::repo_path(module.as_str())])
}

pub(crate) fn repo_module(pattern: &Pattern) -> Option<RepoPath> {
    match pattern.parts() {
        [Part::RepoRoot, Part::Text(path)] => RepoPath::new(path.trim_start_matches('/')).ok(),
        _ => None,
    }
}

fn global(name: &str, module: &RepoPath) -> Value {
    match name {
        "URL" => Value::Builtin(Builtin::UrlConstructor),
        "require" => Value::Require(module_file(module).patterns()),
        "String" => Value::Builtin(Builtin::StringFunction),
        "Object" => Value::Builtin(Builtin::ObjectConstructor),
        "process" => Value::Builtin(Builtin::Process),
        _ => Value::runtime_value(),
    }
}

fn builtin_import(specifier: &str, name: &ImportedName) -> Option<Value> {
    let builtin_module = specifier.strip_prefix("node:").unwrap_or(specifier);
    let member = match name {
        ImportedName::Named(member) => Some(member.as_str()),
        ImportedName::Default | ImportedName::Namespace => None,
    };

    match (builtin_module, member) {
        ("path" | "path/posix", None) => Some(Value::Builtin(Builtin::PathModule)),
        ("path" | "path/posix", Some(member)) => Some(path_member(member)),
        ("url", None) => Some(Value::Builtin(Builtin::UrlModule)),
        ("url", Some(member)) => Some(url_member(member)),
        ("module", None) => Some(Value::Builtin(Builtin::ModuleModule)),
        ("module", Some(member)) => Some(module_member(member)),
        ("fs" | "fs/promises" | "fs-extra" | "graceful-fs", None) => {
            Some(Value::Builtin(Builtin::FsModule))
        }
        ("fs" | "fs/promises" | "fs-extra" | "graceful-fs", Some(member)) => {
            Some(fs_member(member))
        }
        ("glob" | "fast-glob" | "tinyglobby" | "globby", None) => {
            Some(Value::Builtin(Builtin::GlobModule))
        }
        ("glob" | "fast-glob" | "tinyglobby" | "globby", Some(member)) => Some(glob_member(member)),
        ("child_process", None) => Some(Value::Builtin(Builtin::ChildProcessModule)),
        ("child_process", Some(member)) => Some(child_process_member(member)),
        ("execa", None) | ("cross-spawn", _) => {
            Some(Value::Builtin(Builtin::Spawn(SpawnKind::Command)))
        }
        ("execa", Some(member)) => Some(execa_member(member)),
        ("jiti", None | Some("createJiti" | "default")) => {
            Some(Value::Builtin(Builtin::CreateRequire))
        }
        (
            "vitest/config" | "vitest" | "vite" | "vitest/node" | "@playwright/test" | "cypress",
            Some(member),
        ) => runner_config_member(member),
        _ => None,
    }
}

fn runner_config_member(member: &str) -> Option<Value> {
    let texts = |texts: &[&str]| Value::Array(texts.iter().map(|text| Value::text(text)).collect());

    match member {
        "defineConfig" | "defineProject" | "defineWorkspace" => {
            Some(Value::Builtin(Builtin::DefineConfig))
        }
        "mergeConfig" => Some(Value::Builtin(Builtin::MergeConfig)),
        "defaultInclude" => Some(texts(VITEST_DEFAULT_INCLUDE)),
        "defaultExclude" => Some(texts(VITEST_DEFAULT_EXCLUDE)),
        "configDefaults" => Some(Value::Object(vec![
            ("include".to_owned(), texts(VITEST_DEFAULT_INCLUDE)),
            ("exclude".to_owned(), texts(VITEST_DEFAULT_EXCLUDE)),
        ])),
        _ => None,
    }
}

fn path_member(member: &str) -> Value {
    match member {
        "join" => Value::Builtin(Builtin::Path(PathFunction::Join)),
        "resolve" => Value::Builtin(Builtin::Path(PathFunction::Resolve)),
        "dirname" => Value::Builtin(Builtin::Path(PathFunction::Dirname)),
        "normalize" => Value::Builtin(Builtin::Path(PathFunction::Normalize)),
        "posix" | "default" => Value::Builtin(Builtin::PathModule),
        _ => Value::runtime_value(),
    }
}

fn url_member(member: &str) -> Value {
    match member {
        "pathToFileURL" => Value::Builtin(Builtin::PathToFileUrl),
        "fileURLToPath" => Value::Builtin(Builtin::FileUrlToPath),
        "URL" => Value::Builtin(Builtin::UrlConstructor),
        _ => Value::runtime_value(),
    }
}

fn fs_member(member: &str) -> Value {
    let kind = match member {
        "promises" | "default" => return Value::Builtin(Builtin::FsModule),
        "readFile" | "readFileSync" | "readJson" | "readJsonSync" | "readJSON" | "readJSONSync"
        | "createReadStream" | "exists" | "existsSync" | "stat" | "statSync" | "lstat"
        | "lstatSync" | "access" | "accessSync" | "copyFile" | "copyFileSync" => AccessKind::Read,
        "readdir" | "readdirSync" | "opendir" | "opendirSync" => AccessKind::List,
        "cp" | "cpSync" | "copy" | "copySync" => AccessKind::Copy,
        "glob" | "globSync" => AccessKind::Glob,
        _ => return Value::runtime_value(),
    };

    Value::Builtin(Builtin::FileAccess(kind))
}

fn glob_member(member: &str) -> Value {
    match member {
        "default" => Value::Builtin(Builtin::GlobModule),
        "glob" | "globSync" | "sync" | "async" | "globby" | "globbySync" | "globStream"
        | "stream" => Value::Builtin(Builtin::FileAccess(AccessKind::Glob)),
        _ => Value::runtime_value(),
    }
}

fn child_process_member(member: &str) -> Value {
    let kind = match member {
        "default" => return Value::Builtin(Builtin::ChildProcessModule),
        "spawn" | "spawnSync" | "execFile" | "execFileSync" => SpawnKind::Command,
        "exec" | "execSync" => SpawnKind::Shell,
        "fork" => SpawnKind::Node,
        _ => return Value::runtime_value(),
    };

    Value::Builtin(Builtin::Spawn(kind))
}

fn execa_member(member: &str) -> Value {
    let kind = match member {
        "execa" | "execaSync" | "default" => SpawnKind::Command,
        "execaCommand" | "execaCommandSync" => SpawnKind::Shell,
        "execaNode" => SpawnKind::Node,
        _ => return Value::runtime_value(),
    };

    Value::Builtin(Builtin::Spawn(kind))
}

fn module_member(member: &str) -> Value {
    match member {
        "createRequire" => Value::Builtin(Builtin::CreateRequire),
        _ => Value::runtime_value(),
    }
}
