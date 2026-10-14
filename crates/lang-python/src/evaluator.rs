//! Partial evaluation across modules: the strings, paths and modules a summarized expression can be, with parameters followed through every visible caller.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use prober_core::files::RepoPath;

use crate::conventions::{PACKAGE_INIT, join_directory, parent_directory, split_path};
use crate::modules::ModuleIndex;
use crate::summary::{CallSite, Expr, FunctionIndex, ModuleRef, ModuleSummary, Site};

const MAX_ALTERNATIVES: usize = 32;

const MAX_CALL_DEPTH: usize = 6;

const MAX_CALLER_DEPTH: usize = 6;

const MAX_LOOKUP_DEPTH: usize = 6;

const MAX_EXPANSIONS: usize = 50_000;

const MAX_EVALUATION_STEPS: usize = 20_000;

const MAX_SITE_STEPS: usize = 1_000_000;

const RECEIVERS: [&str; 2] = ["self", "cls"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FunctionRef<'s> {
    pub module: &'s RepoPath,
    pub index: FunctionIndex,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Hole {
    Runtime,
    Parameter(RepoPath, FunctionIndex),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Part {
    Root,
    Text(String),
    Hole(Hole),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Pattern(pub Vec<Part>);

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value<'s> {
    Strings(Vec<Pattern>),
    Sequence(Vec<Value<'s>>),
    Module(ModuleRef),
    Function(FunctionRef<'s>),
    BoundMethod(FunctionRef<'s>),
    Builtin(String),
    Method {
        receiver: Box<Value<'s>>,
        name: String,
    },
    Unknown(Hole),
}

struct CallerRef<'s> {
    module: &'s RepoPath,
    call: &'s CallSite,
    receiver_shift: usize,
}

enum Bound<'s> {
    Unbound,
    Values(Vec<Option<Value<'s>>>),
    Call {
        caller: Rc<Frame<'s>>,
        call: &'s CallSite,
        receiver_shift: usize,
    },
}

struct Frame<'s> {
    module: &'s RepoPath,
    function: Option<FunctionIndex>,
    bound: Bound<'s>,
    depth: usize,
}

pub(crate) struct SiteValue<'s> {
    pub reader: &'s RepoPath,
    pub value: Value<'s>,
}

pub(crate) struct Evaluator<'s> {
    summaries: &'s HashMap<RepoPath, ModuleSummary>,
    index: &'s ModuleIndex,
    callers: HashMap<FunctionRef<'s>, Vec<CallerRef<'s>>>,
    escaping: HashSet<FunctionRef<'s>>,
    steps: Cell<usize>,
    site_steps: Cell<usize>,
}

impl<'s> Evaluator<'s> {
    pub fn new(summaries: &'s HashMap<RepoPath, ModuleSummary>, index: &'s ModuleIndex) -> Self {
        let mut evaluator = Self {
            summaries,
            index,
            callers: HashMap::new(),
            escaping: HashSet::new(),
            steps: Cell::new(0),
            site_steps: Cell::new(0),
        };
        let mut callers: HashMap<FunctionRef<'s>, Vec<CallerRef<'s>>> = HashMap::new();
        let mut escaping = HashSet::new();
        let unresolved_attributes: HashSet<&str> = summaries
            .values()
            .flat_map(|summary| &summary.unresolved_attributes)
            .map(String::as_str)
            .collect();
        let referenced_attributes: HashSet<&str> = summaries
            .values()
            .flat_map(|summary| &summary.referenced_attributes)
            .map(String::as_str)
            .collect();
        let mut methods: HashMap<&str, Vec<(FunctionRef<'s>, usize)>> = HashMap::new();

        for (module, summary) in summaries {
            for (index, function) in summary.functions.iter().enumerate() {
                let name = function.name.as_str();
                let reachable_by_attribute = unresolved_attributes.contains(name);
                let escapes = match &function.class {
                    Some(_) => {
                        function.nested || referenced_attributes.contains(name) || is_special(name)
                    }
                    None => reachable_by_attribute,
                };

                if function.escapes || escapes {
                    escaping.insert(FunctionRef { module, index });
                }

                if function.class.is_some() && !function.nested {
                    let receiver_shift = usize::from(
                        function
                            .parameters
                            .first()
                            .is_some_and(|first| RECEIVERS.contains(&first.as_str())),
                    );

                    methods
                        .entry(name)
                        .or_default()
                        .push((FunctionRef { module, index }, receiver_shift));
                }
            }
        }

        for (module, summary) in summaries {
            for call in &summary.calls {
                if let Some((function, receiver_shift)) = evaluator.callee(module, &call.callee) {
                    callers.entry(function).or_default().push(CallerRef {
                        module,
                        call,
                        receiver_shift,
                    });
                }

                if let Expr::Method { name, .. } = &call.callee {
                    for &(method, receiver_shift) in
                        methods.get(name.as_str()).into_iter().flatten()
                    {
                        callers.entry(method).or_default().push(CallerRef {
                            module,
                            call,
                            receiver_shift,
                        });
                    }
                }
            }

            for (name, call) in &summary.method_calls {
                for &(method, receiver_shift) in methods.get(name.as_str()).into_iter().flatten() {
                    callers.entry(method).or_default().push(CallerRef {
                        module,
                        call,
                        receiver_shift,
                    });
                }
            }

            for escape in &summary.escapes {
                if let Some((function, _)) = evaluator.callee(module, escape) {
                    escaping.insert(function);
                }
            }
        }

        evaluator.callers = callers;
        evaluator.escaping = escaping;

        evaluator
    }

    pub fn site_values(&self, module: &'s RepoPath, site: &'s Site) -> Vec<SiteValue<'s>> {
        let mut results = Vec::new();
        let mut expansions = 0;

        self.site_steps.set(0);
        self.expand(module, site, Vec::new(), &mut results, &mut expansions);

        if self.site_steps.get() > MAX_SITE_STEPS {
            return vec![SiteValue {
                reader: module,
                value: Value::Unknown(Hole::Runtime),
            }];
        }

        results
    }

    pub fn evaluate_in(&self, module: &'s RepoPath, expr: &'s Expr) -> Value<'s> {
        self.steps.set(0);

        let frame = Rc::new(Frame {
            module,
            function: None,
            bound: Bound::Unbound,
            depth: 0,
        });

        self.evaluate(expr, &frame)
    }

    fn expand(
        &self,
        module: &'s RepoPath,
        site: &'s Site,
        chain: Vec<&CallerRef<'s>>,
        results: &mut Vec<SiteValue<'s>>,
        expansions: &mut usize,
    ) {
        *expansions += 1;

        let frame = self.site_frame(module, site.unit, &chain);
        let outermost = match chain.last() {
            Some(caller) => caller.call.unit.map(|index| FunctionRef {
                module: caller.module,
                index,
            }),
            None => site.unit.map(|index| FunctionRef { module, index }),
        };
        let reader = chain.last().map_or(module, |caller| caller.module);

        self.steps.set(0);

        let value = self.evaluate(&site.value, &frame);

        self.site_steps
            .set(self.site_steps.get() + self.steps.get());

        if self.site_steps.get() > MAX_SITE_STEPS {
            return;
        }

        let open = outermost.filter(|function| value.depends_on(function));
        let Some(function) = open else {
            results.push(SiteValue { reader, value });

            return;
        };
        let callers = self.callers.get(&function).map_or(&[][..], Vec::as_slice);
        let can_expand = chain.len() < MAX_CALLER_DEPTH
            && !callers.is_empty()
            && *expansions + callers.len() <= MAX_EXPANSIONS;

        if !can_expand || self.escaping.contains(&function) {
            results.push(SiteValue { reader, value });
        }

        if !can_expand {
            return;
        }

        for caller in callers {
            let mut extended = chain.clone();

            extended.push(caller);
            self.expand(module, site, extended, results, expansions);
        }
    }

    fn site_frame(
        &self,
        module: &'s RepoPath,
        unit: Option<FunctionIndex>,
        chain: &[&CallerRef<'s>],
    ) -> Rc<Frame<'s>> {
        let mut outer = None;

        for (position, caller) in chain.iter().enumerate().rev() {
            let bound = match outer.take() {
                Some(previous_caller) => previous_caller,
                None => Rc::new(Frame {
                    module: caller.module,
                    function: caller.call.unit,
                    bound: Bound::Unbound,
                    depth: 0,
                }),
            };
            let callee_module = if position == 0 {
                module
            } else {
                chain[position - 1].module
            };
            let callee_function = if position == 0 {
                unit
            } else {
                chain[position - 1].call.unit
            };

            outer = Some(Rc::new(Frame {
                module: callee_module,
                function: callee_function,
                bound: Bound::Call {
                    caller: bound,
                    call: caller.call,
                    receiver_shift: caller.receiver_shift,
                },
                depth: 0,
            }));
        }

        outer.unwrap_or_else(|| {
            Rc::new(Frame {
                module,
                function: unit,
                bound: Bound::Unbound,
                depth: 0,
            })
        })
    }

    fn callee(&self, module: &'s RepoPath, callee: &Expr) -> Option<(FunctionRef<'s>, usize)> {
        let summary = self.summaries.get(module)?;

        match callee {
            Expr::Function(index) => Some((
                FunctionRef {
                    module: self.module_key(module)?,
                    index: *index,
                },
                0,
            )),
            Expr::Method { class, name } => {
                let index = summary.function_named(name, Some(class))?;

                Some((
                    FunctionRef {
                        module: self.module_key(module)?,
                        index,
                    },
                    1,
                ))
            }
            Expr::Imported {
                module: ModuleRef::Repo(target),
                name,
            } => self
                .exported_function(target, name, 0)
                .map(|function| (function, 0)),
            _ => None,
        }
    }

    fn module_key(&self, module: &RepoPath) -> Option<&'s RepoPath> {
        self.summaries.get_key_value(module).map(|(key, _)| key)
    }

    fn exported_function(
        &self,
        module: &RepoPath,
        name: &str,
        depth: usize,
    ) -> Option<FunctionRef<'s>> {
        let (key, summary) = self.summaries.get_key_value(module)?;

        if let Some(index) = summary.function_named(name, None) {
            return Some(FunctionRef { module: key, index });
        }

        if depth >= MAX_LOOKUP_DEPTH {
            return None;
        }

        match summary.bindings.get(name)? {
            Expr::Imported {
                module: ModuleRef::Repo(target),
                name,
            } => self.exported_function(target, name, depth + 1),
            _ => None,
        }
    }

    fn evaluate(&self, expr: &'s Expr, frame: &Rc<Frame<'s>>) -> Value<'s> {
        let steps = self.steps.get() + 1;

        self.steps.set(steps);

        if steps > MAX_EVALUATION_STEPS {
            return Value::Unknown(Hole::Runtime);
        }

        match expr {
            Expr::Text(text) => Value::text(text),
            Expr::ModuleFile => Value::Strings(vec![Pattern(vec![
                Part::Root,
                Part::Text(frame.module.as_str().to_owned()),
            ])]),
            Expr::ModuleName => match self.index.dotted_name(frame.module.as_str()) {
                Some(name) => Value::text(&name),
                None => Value::Unknown(Hole::Runtime),
            },
            Expr::PackageName => match self.index.package_name(frame.module.as_str()) {
                Some(name) => Value::text(&name),
                None => Value::Unknown(Hole::Runtime),
            },
            Expr::Concat(parts) => {
                let values: Vec<Value> = parts
                    .iter()
                    .map(|part| self.evaluate(part, frame))
                    .collect();

                concatenate(&values)
            }
            Expr::Choice(alternatives) => union(
                alternatives
                    .iter()
                    .map(|alternative| self.evaluate(alternative, frame))
                    .collect(),
            ),
            Expr::Sequence(items) => Value::Sequence(
                items
                    .iter()
                    .map(|item| self.evaluate(item, frame))
                    .collect(),
            ),
            Expr::ElementOf(iterable) => match self.evaluate(iterable, frame) {
                Value::Sequence(items) => union(items),
                Value::Unknown(hole) => Value::Unknown(hole),
                _ => Value::Unknown(Hole::Runtime),
            },
            Expr::ItemOf { iterable, index } => match self.evaluate(iterable, frame) {
                Value::Sequence(items) => union(
                    items
                        .into_iter()
                        .map(|item| match item {
                            Value::Sequence(mut fields) if *index < fields.len() => {
                                fields.swap_remove(*index)
                            }
                            _ => Value::Unknown(Hole::Runtime),
                        })
                        .collect(),
                ),
                Value::Unknown(hole) => Value::Unknown(hole),
                _ => Value::Unknown(Hole::Runtime),
            },
            Expr::Parameter { function, index } => self.parameter(*function, *index, frame),
            Expr::Function(index) => match self.module_key(frame.module) {
                Some(module) => Value::Function(FunctionRef {
                    module,
                    index: *index,
                }),
                None => Value::Unknown(Hole::Runtime),
            },
            Expr::Method { class, name } => {
                let function = self.summaries.get(frame.module).and_then(|summary| {
                    Some(FunctionRef {
                        module: self.module_key(frame.module)?,
                        index: summary.function_named(name, Some(class))?,
                    })
                });

                function.map_or(Value::Unknown(Hole::Runtime), Value::BoundMethod)
            }
            Expr::Module(module) => Value::Module(module.clone()),
            Expr::Imported { module, name } => self.imported(module, name, frame.depth),
            Expr::Attribute { object, name } => {
                let object = self.evaluate(object, frame);

                self.attribute(object, name, frame.depth)
            }
            Expr::Call {
                callee,
                arguments,
                keywords,
            } => {
                let callee = self.evaluate(callee, frame);
                let arguments: Vec<Value> = arguments
                    .iter()
                    .map(|argument| self.evaluate(argument, frame))
                    .collect();
                let keywords: Vec<(&str, Value)> = keywords
                    .iter()
                    .map(|(name, value)| (name.as_str(), self.evaluate(value, frame)))
                    .collect();

                self.call(callee, arguments, keywords, frame)
            }
            Expr::Join(left, right) => {
                let left = self.evaluate(left, frame);
                let right = self.evaluate(right, frame);

                join(&left, &right)
            }
            Expr::Unknown => Value::Unknown(Hole::Runtime),
        }
    }

    fn parameter(&self, function: FunctionIndex, index: usize, frame: &Rc<Frame<'s>>) -> Value<'s> {
        let unbound = || Value::Unknown(Hole::Parameter(frame.module.clone(), function));

        if frame.function != Some(function) {
            return unbound();
        }

        match &frame.bound {
            Bound::Unbound => unbound(),
            Bound::Values(values) => values
                .get(index)
                .cloned()
                .flatten()
                .or_else(|| self.parameter_default(frame, function, index))
                .unwrap_or(Value::Unknown(Hole::Runtime)),
            Bound::Call {
                caller,
                call,
                receiver_shift,
            } => {
                let Some(summary) = self.summaries.get(frame.module) else {
                    return Value::Unknown(Hole::Runtime);
                };
                let Some(definition) = summary.functions.get(function) else {
                    return Value::Unknown(Hole::Runtime);
                };
                let argument = index
                    .checked_sub(*receiver_shift)
                    .and_then(|position| call.arguments.get(position))
                    .or_else(|| {
                        let name = definition.parameters.get(index)?;

                        call.keywords
                            .iter()
                            .find(|(keyword, _)| keyword == name)
                            .map(|(_, value)| value)
                    });

                match argument {
                    Some(argument) => self.evaluate(argument, caller),
                    None => self
                        .parameter_default(frame, function, index)
                        .unwrap_or(Value::Unknown(Hole::Runtime)),
                }
            }
        }
    }

    fn parameter_default(
        &self,
        frame: &Rc<Frame<'s>>,
        function: FunctionIndex,
        index: usize,
    ) -> Option<Value<'s>> {
        let default = self
            .summaries
            .get(frame.module)?
            .functions
            .get(function)?
            .defaults
            .get(index)?
            .as_ref()?;
        let module_frame = Rc::new(Frame {
            module: frame.module,
            function: None,
            bound: Bound::Unbound,
            depth: frame.depth,
        });

        Some(self.evaluate(default, &module_frame))
    }

    fn imported(&self, module: &ModuleRef, name: &str, depth: usize) -> Value<'s> {
        match module {
            ModuleRef::Named(module) => Value::Builtin(format!("{module}.{name}")),
            ModuleRef::Repo(module) => self.module_attribute(module, name, depth),
        }
    }

    fn module_attribute(&self, module: &RepoPath, name: &str, depth: usize) -> Value<'s> {
        let Some((key, summary)) = self.summaries.get_key_value(module) else {
            return Value::Unknown(Hole::Runtime);
        };

        if depth < MAX_LOOKUP_DEPTH
            && let Some(binding) = summary.bindings.get(name)
        {
            let frame = Rc::new(Frame {
                module: key,
                function: None,
                bound: Bound::Unbound,
                depth: depth + 1,
            });

            return self.evaluate(binding, &frame);
        }

        let (directory, file_name) = split_path(module.as_str());

        if file_name == PACKAGE_INIT
            && let Some(resolved) = self.index.resolve_in_directory(directory, name)
        {
            return Value::Module(ModuleRef::Repo(resolved.module));
        }

        Value::Unknown(Hole::Runtime)
    }

    fn attribute(&self, object: Value<'s>, name: &str, depth: usize) -> Value<'s> {
        match object {
            Value::Module(ModuleRef::Repo(module)) => self.module_attribute(&module, name, depth),
            Value::Module(ModuleRef::Named(module)) | Value::Builtin(module) => {
                Value::Builtin(format!("{module}.{name}"))
            }
            Value::Strings(patterns) => match name {
                "parent" => map_patterns(&patterns, dirname),
                "name" => map_patterns(&patterns, basename),
                _ => Value::Method {
                    receiver: Box::new(Value::Strings(patterns)),
                    name: name.to_owned(),
                },
            },
            Value::Unknown(hole) => Value::Unknown(hole),
            _ => Value::Unknown(Hole::Runtime),
        }
    }

    fn call(
        &self,
        callee: Value<'s>,
        arguments: Vec<Value<'s>>,
        keywords: Vec<(&str, Value<'s>)>,
        frame: &Rc<Frame<'s>>,
    ) -> Value<'s> {
        match callee {
            Value::Builtin(name) => self.builtin_call(&name, arguments),
            Value::BoundMethod(function) => {
                self.function_call(function, 1, arguments, keywords, frame)
            }
            Value::Method { receiver, name } => match (*receiver, name.as_str()) {
                (receiver @ Value::Strings(_), "joinpath") => arguments
                    .iter()
                    .fold(receiver, |path, argument| join(&path, argument)),
                (Value::Strings(patterns), "with_suffix") => match arguments.first() {
                    Some(Value::Strings(suffixes)) => {
                        combine(&patterns, suffixes, |path, suffix| {
                            with_suffix(path, suffix)
                        })
                    }
                    _ => Value::Unknown(Hole::Runtime),
                },
                (Value::Strings(patterns), "with_name") => match arguments.first() {
                    Some(name @ Value::Strings(_)) => join(&map_patterns(&patterns, dirname), name),
                    _ => Value::Unknown(Hole::Runtime),
                },
                (receiver @ Value::Strings(_), "resolve" | "absolute" | "expanduser") => receiver,
                (Value::Strings(separators), "join") => match arguments.first() {
                    Some(Value::Sequence(items)) => joined(&separators, items),
                    _ => Value::Unknown(Hole::Runtime),
                },
                _ => Value::Unknown(Hole::Runtime),
            },
            Value::Function(function) => {
                self.function_call(function, 0, arguments, keywords, frame)
            }
            Value::Unknown(hole) => Value::Unknown(hole),
            _ => Value::Unknown(Hole::Runtime),
        }
    }

    fn builtin_call(&self, name: &str, arguments: Vec<Value<'s>>) -> Value<'s> {
        match name {
            "pathlib.Path"
            | "pathlib.PurePath"
            | "pathlib.PosixPath"
            | "pathlib.PurePosixPath"
            | "os.path.join" => match arguments.split_first() {
                Some((first, rest)) => rest
                    .iter()
                    .fold(first.clone(), |path, argument| join(&path, argument)),
                None => Value::text(""),
            },
            "os.path.dirname" => match arguments.first() {
                Some(Value::Strings(patterns)) => map_patterns(patterns, dirname),
                _ => Value::Unknown(Hole::Runtime),
            },
            "os.path.basename" => match arguments.first() {
                Some(Value::Strings(patterns)) => map_patterns(patterns, basename),
                _ => Value::Unknown(Hole::Runtime),
            },
            "os.path.abspath" | "os.path.realpath" | "os.path.normpath" | "os.path.expanduser"
            | "os.fspath" | "builtins.str" => arguments
                .into_iter()
                .next()
                .unwrap_or(Value::Unknown(Hole::Runtime)),
            "importlib.resources.files" | "importlib_resources.files" => match arguments.first() {
                Some(Value::Strings(patterns)) => self.package_directories(patterns),
                Some(Value::Module(ModuleRef::Repo(module))) => {
                    let directory = parent_directory(module.as_str());

                    Value::Strings(vec![Pattern(vec![
                        Part::Root,
                        Part::Text(directory.to_owned()),
                    ])])
                }
                _ => Value::Unknown(Hole::Runtime),
            },
            _ => Value::Unknown(Hole::Runtime),
        }
    }

    fn package_directories(&self, patterns: &[Pattern]) -> Value<'s> {
        let mut directories = Vec::new();

        for pattern in patterns {
            let Some(dotted) = pattern.text() else {
                return Value::Unknown(Hole::Runtime);
            };

            for root in self.index.roots() {
                let directory = join_directory(root, &dotted.replace('.', "/"));

                if self.index.is_directory(&directory) {
                    directories.push(Pattern(vec![Part::Root, Part::Text(directory)]));
                }
            }
        }

        if directories.is_empty() {
            return Value::Unknown(Hole::Runtime);
        }

        Value::Strings(directories)
    }

    fn function_call(
        &self,
        function: FunctionRef<'s>,
        receiver_shift: usize,
        arguments: Vec<Value<'s>>,
        keywords: Vec<(&str, Value<'s>)>,
        frame: &Rc<Frame<'s>>,
    ) -> Value<'s> {
        if frame.depth >= MAX_CALL_DEPTH {
            return Value::Unknown(Hole::Runtime);
        }

        let Some(summary) = self.summaries.get(function.module) else {
            return Value::Unknown(Hole::Runtime);
        };
        let Some(definition) = summary.functions.get(function.index) else {
            return Value::Unknown(Hole::Runtime);
        };
        let mut values: Vec<Option<Value>> = vec![None; definition.parameters.len()];

        for (position, argument) in arguments.into_iter().enumerate() {
            if let Some(slot) = values.get_mut(position + receiver_shift) {
                *slot = Some(argument);
            }
        }

        for (name, value) in keywords {
            if let Some(position) = definition
                .parameters
                .iter()
                .position(|parameter| parameter == name)
            {
                values[position] = Some(value);
            }
        }

        let callee_frame = Rc::new(Frame {
            module: function.module,
            function: Some(function.index),
            bound: Bound::Values(values),
            depth: frame.depth + 1,
        });

        union(
            definition
                .returns
                .iter()
                .map(|returned| self.evaluate(returned, &callee_frame))
                .collect(),
        )
    }
}

impl<'s> Value<'s> {
    pub fn text(text: &str) -> Self {
        Value::Strings(vec![Pattern(vec![Part::Text(text.to_owned())])])
    }

    pub fn patterns(&self) -> Vec<Pattern> {
        match self {
            Value::Strings(patterns) => patterns.clone(),
            Value::Sequence(items) => items.iter().flat_map(Value::patterns).collect(),
            Value::Unknown(hole) => vec![Pattern(vec![Part::Hole(hole.clone())])],
            _ => vec![Pattern(vec![Part::Hole(Hole::Runtime)])],
        }
    }

    fn depends_on(&self, function: &FunctionRef) -> bool {
        let is_open = |part: &Part| {
            matches!(part, Part::Hole(Hole::Parameter(module, index))
                if module == function.module && *index == function.index)
        };

        match self {
            Value::Strings(patterns) => {
                patterns.iter().any(|pattern| pattern.0.iter().any(is_open))
            }
            Value::Sequence(items) => items.iter().any(|item| item.depends_on(function)),
            Value::Unknown(hole) => is_open(&Part::Hole(hole.clone())),
            Value::Method { receiver, .. } => receiver.depends_on(function),
            _ => false,
        }
    }
}

impl Pattern {
    pub fn text(&self) -> Option<String> {
        let mut text = String::new();

        for part in &self.0 {
            match part {
                Part::Text(part) => text.push_str(part),
                Part::Root | Part::Hole(_) => return None,
            }
        }

        Some(text)
    }

    pub fn holes(&self) -> impl Iterator<Item = &Hole> {
        self.0.iter().filter_map(|part| match part {
            Part::Hole(hole) => Some(hole),
            _ => None,
        })
    }

    fn push(&mut self, part: Part) {
        match (self.0.last_mut(), part) {
            (Some(Part::Text(last)), Part::Text(text)) => last.push_str(&text),
            (Some(Part::Hole(_)), Part::Hole(_)) => {}
            (_, Part::Text(text)) if text.is_empty() => {}
            (_, part) => self.0.push(part),
        }
    }

    fn concatenated(&self, other: &Pattern) -> Pattern {
        let mut pattern = self.clone();

        for part in &other.0 {
            pattern.push(part.clone());
        }

        pattern
    }
}

fn concatenate<'s>(values: &[Value<'s>]) -> Value<'s> {
    let mut patterns = vec![Pattern(Vec::new())];

    for value in values {
        let alternatives = value.patterns();
        let mut next = Vec::new();

        for pattern in &patterns {
            for alternative in &alternatives {
                next.push(pattern.concatenated(alternative));
            }
        }

        next.dedup();

        if next.len() > MAX_ALTERNATIVES {
            return Value::Unknown(Hole::Runtime);
        }

        patterns = next;
    }

    Value::Strings(patterns)
}

fn union<'s>(values: Vec<Value<'s>>) -> Value<'s> {
    if values.len() == 1 {
        return values
            .into_iter()
            .next()
            .unwrap_or(Value::Unknown(Hole::Runtime));
    }

    let all_strings = values
        .iter()
        .all(|value| matches!(value, Value::Strings(_) | Value::Unknown(_)));

    if !all_strings {
        return values
            .into_iter()
            .find(|value| !matches!(value, Value::Unknown(_)))
            .unwrap_or(Value::Unknown(Hole::Runtime));
    }

    let mut patterns: Vec<Pattern> = Vec::new();

    for value in &values {
        for pattern in value.patterns() {
            if !patterns.contains(&pattern) {
                patterns.push(pattern);
            }
        }
    }

    if patterns.len() > MAX_ALTERNATIVES {
        return Value::Unknown(Hole::Runtime);
    }

    Value::Strings(patterns)
}

fn join<'s>(left: &Value<'s>, right: &Value<'s>) -> Value<'s> {
    let (Value::Strings(_) | Value::Unknown(_), Value::Strings(_) | Value::Unknown(_)) =
        (left, right)
    else {
        return Value::Unknown(Hole::Runtime);
    };

    combine(&left.patterns(), &right.patterns(), |left, right| {
        if matches!(right.0.first(), Some(Part::Root)) {
            return right.clone();
        }

        if let Some(Part::Text(text)) = right.0.first()
            && text.starts_with('/')
        {
            return Pattern(vec![Part::Hole(Hole::Runtime)]);
        }

        let mut joined = left.clone();
        let separator = match left.0.last() {
            None => "",
            Some(Part::Root) => "",
            Some(Part::Text(text)) if text.ends_with('/') || text.is_empty() => "",
            _ => "/",
        };

        joined.push(Part::Text(separator.to_owned()));

        joined.concatenated(right)
    })
}

fn combine<'s>(
    left: &[Pattern],
    right: &[Pattern],
    operation: impl Fn(&Pattern, &Pattern) -> Pattern,
) -> Value<'s> {
    let mut patterns = Vec::new();

    for left in left {
        for right in right {
            let pattern = operation(left, right);

            if !patterns.contains(&pattern) {
                patterns.push(pattern);
            }
        }
    }

    if patterns.len() > MAX_ALTERNATIVES {
        return Value::Unknown(Hole::Runtime);
    }

    Value::Strings(patterns)
}

fn map_patterns<'s>(patterns: &[Pattern], operation: fn(&Pattern) -> Pattern) -> Value<'s> {
    let mut mapped = Vec::new();

    for pattern in patterns {
        let pattern = operation(pattern);

        if !mapped.contains(&pattern) {
            mapped.push(pattern);
        }
    }

    Value::Strings(mapped)
}

fn joined<'s>(separators: &[Pattern], items: &[Value<'s>]) -> Value<'s> {
    let Some(separator) = separators.first().and_then(Pattern::text) else {
        return Value::Unknown(Hole::Runtime);
    };
    let mut values = Vec::new();

    for (position, item) in items.iter().enumerate() {
        if position > 0 {
            values.push(Value::text(&separator));
        }

        values.push(item.clone());
    }

    concatenate(&values)
}

fn dirname(pattern: &Pattern) -> Pattern {
    let mut parts = pattern.0.clone();

    while let Some(Part::Text(text)) = parts.last_mut() {
        let trimmed = text.trim_end_matches('/');

        match trimmed.rfind('/') {
            Some(position) => {
                text.truncate(position);

                return Pattern(parts);
            }
            None if trimmed.is_empty() => {
                parts.pop();
            }
            None => {
                parts.pop();

                return Pattern(parts);
            }
        }
    }

    match parts.last() {
        Some(Part::Root) | None => Pattern(parts),
        Some(_) => Pattern(vec![Part::Hole(Hole::Runtime)]),
    }
}

fn basename(pattern: &Pattern) -> Pattern {
    match pattern.0.last() {
        Some(Part::Text(text)) => {
            let name = text
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or_default();

            if text.trim_end_matches('/').contains('/') || pattern.0.len() == 1 {
                Pattern(vec![Part::Text(name.to_owned())])
            } else {
                Pattern(vec![Part::Hole(Hole::Runtime), Part::Text(name.to_owned())])
            }
        }
        _ => Pattern(vec![Part::Hole(Hole::Runtime)]),
    }
}

fn with_suffix(path: &Pattern, suffix: &Pattern) -> Pattern {
    let mut parts = path.0.clone();

    if let Some(Part::Text(text)) = parts.last_mut() {
        let (directory, name) = split_path(text);

        if let Some((stem, _)) = name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()) {
            let replaced = join_directory(directory, stem);

            *text = if text.starts_with('/') && !replaced.starts_with('/') {
                format!("/{replaced}")
            } else {
                replaced
            };
        }
    }

    Pattern(parts).concatenated(suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(path: &str) -> Pattern {
        Pattern(vec![Part::Root, Part::Text(path.to_owned())])
    }

    fn strings<'s>(patterns: Vec<Pattern>) -> Value<'s> {
        Value::Strings(patterns)
    }

    #[test]
    fn joins_follow_pathlib() {
        let directory = strings(vec![root("tests")]);
        let joined = join(&directory, &Value::text("fixtures/a.json"));

        assert_eq!(joined, strings(vec![root("tests/fixtures/a.json")]));
        assert_eq!(
            join(&Value::text("data"), &strings(vec![root("tests")])),
            strings(vec![root("tests")])
        );
    }

    #[test]
    fn dirname_climbs_one_directory() {
        assert_eq!(dirname(&root("tests/common.py")), root("tests"));
        assert_eq!(dirname(&root("conftest.py")), Pattern(vec![Part::Root]));
        assert_eq!(
            dirname(&Pattern(vec![Part::Hole(Hole::Runtime)])),
            Pattern(vec![Part::Hole(Hole::Runtime)])
        );
    }

    #[test]
    fn concatenation_keeps_one_hole_between_texts() {
        let value = concatenate(&[
            Value::text("pkg.plugins."),
            Value::Unknown(Hole::Runtime),
            Value::Unknown(Hole::Runtime),
        ]);

        assert_eq!(
            value,
            strings(vec![Pattern(vec![
                Part::Text("pkg.plugins.".to_owned()),
                Part::Hole(Hole::Runtime)
            ])])
        );
    }

    #[test]
    fn with_suffix_replaces_the_extension() {
        assert_eq!(
            with_suffix(
                &root("tests/test_a.py"),
                &Pattern(vec![Part::Text(".json".to_owned())])
            ),
            root("tests/test_a.json")
        );
    }
}

fn is_special(name: &str) -> bool {
    name.len() > 4 && name.starts_with("__") && name.ends_with("__")
}
