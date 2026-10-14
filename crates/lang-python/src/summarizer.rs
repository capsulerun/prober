//! From a Python syntax tree to its summary: static imports, scopes and bindings, computed import and path sites, calls to repository functions and the functions that escape.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use prober_core::files::RepoPath;
use prober_core::symbols::FallbackCause;
use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{
    self as ast, ExceptHandler, Expr as AstExpr, InterpolatedStringElement, Operator, PySourceType,
    Stmt, StringLiteralValue,
};
use ruff_python_parser::parse_unchecked_source;
use ruff_text_size::{Ranged, TextSize};

use crate::conventions::join_directory;
use crate::django::setting_lists;
use crate::modules::{ImportTarget, ModuleIndex};
use crate::pytest::items::{PytestItems, pytest_items};
use crate::summary::{
    CallSite, Expr, FunctionIndex, FunctionSummary, ModuleRef, ModuleSummary, NativeImport, Site,
    SiteKind, StaticImport,
};
use crate::symbols::extract_symbols;

const MAX_CONVERSION_DEPTH: usize = 24;

const MAX_STRING_LENGTH: usize = 255;

const MAX_EXPRESSION_LENGTH: usize = 80;

const TYPE_CHECKING: &str = "TYPE_CHECKING";

const PATH_FUNCTIONS: [&str; 12] = [
    "pathlib.Path",
    "pathlib.PurePath",
    "pathlib.PosixPath",
    "pathlib.PurePosixPath",
    "os.path.join",
    "os.path.dirname",
    "os.path.abspath",
    "os.path.realpath",
    "os.path.normpath",
    "os.path.expanduser",
    "importlib.resources.files",
    "importlib_resources.files",
];

const PATH_METHODS: [&str; 6] = [
    "joinpath",
    "with_suffix",
    "with_name",
    "with_stem",
    "resolve",
    "absolute",
];

const PATH_ATTRIBUTES: [&str; 1] = ["parent"];

const OPENING_FUNCTIONS: [&str; 6] = [
    "builtins.open",
    "io.open",
    "codecs.open",
    "runpy.run_path",
    "pkgutil.get_data",
    "os.stat",
];

const LISTING_FUNCTIONS: [&str; 6] = [
    "os.listdir",
    "os.scandir",
    "os.walk",
    "glob.glob",
    "glob.iglob",
    "shutil.copytree",
];

const ATTRIBUTE_FUNCTIONS: [&str; 4] = [
    "builtins.getattr",
    "builtins.hasattr",
    "operator.attrgetter",
    "operator.methodcaller",
];

const IMPORT_FUNCTIONS: [&str; 3] = [
    "importlib.import_module",
    "builtins.__import__",
    "importlib.__import__",
];

const TRANSPARENT_DECORATORS: [&str; 10] = [
    "staticmethod",
    "classmethod",
    "cache",
    "lru_cache",
    "functools.cache",
    "functools.lru_cache",
    "functools.wraps",
    "typing.overload",
    "overload",
    "wraps",
];

const BUILTINS: [&str; 8] = [
    "open",
    "__import__",
    "str",
    "repr",
    "format",
    "print",
    "getattr",
    "hasattr",
];

type ScopeId = usize;

#[derive(Clone)]
enum Binding<'a> {
    Value(&'a AstExpr),
    Element(&'a AstExpr),
    Item(&'a AstExpr, usize),
    Module(ModuleRef),
    Imported(ModuleRef, String),
    Function(FunctionIndex),
    Unknown,
}

struct Scope<'a> {
    parent: Option<ScopeId>,
    function: Option<FunctionIndex>,
    class: Option<String>,
    parameters: HashMap<&'a str, usize>,
    bindings: HashMap<&'a str, Vec<Binding<'a>>>,
}

pub(crate) fn summarize(path: &RepoPath, source: &str, index: &ModuleIndex) -> ModuleSummary {
    let parsed = parse_unchecked_source(source, PySourceType::Python);
    let module = parsed.syntax();
    let mut collector = Collector {
        path,
        index,
        scopes: vec![Scope {
            parent: None,
            function: None,
            class: None,
            parameters: HashMap::new(),
            bindings: HashMap::new(),
        }],
        functions: Vec::new(),
        function_scopes: HashMap::new(),
    };

    collector.collect_body(&module.body, 0, None);

    let summarizer = Summarizer {
        path,
        index,
        source,
        line_starts: line_starts(source),
        scopes: collector.scopes,
        function_scopes: collector.function_scopes,
        converted: RefCell::new(HashMap::new()),
        converting: RefCell::new(HashSet::new()),
    };
    let (pytest, symbols) = if parsed.errors().is_empty() {
        let items = pytest_items(module, source);
        let symbols = extract_symbols(module, source, path, index, &items);

        (items, symbols)
    } else {
        (PytestItems::default(), Err(FallbackCause::ParseError))
    };
    let summary = ModuleSummary {
        functions: collector
            .functions
            .iter()
            .map(|function| summarizer.function_summary(function))
            .collect(),
        bindings: summarizer.module_bindings(),
        setting_lists: setting_lists(module),
        pytest,
        symbols: Some(symbols),
        ..ModuleSummary::default()
    };
    let mut visitor = SummaryVisitor {
        summarizer: &summarizer,
        summary,
        scope: 0,
        type_only: false,
        inside_path: false,
    };

    visitor.visit_body(&module.body);

    visitor.summary
}

struct CollectedFunction<'a> {
    definition: &'a ast::StmtFunctionDef,
    class: Option<String>,
    nested: bool,
    scope: ScopeId,
    escapes: bool,
}

struct Collector<'a, 'i> {
    path: &'i RepoPath,
    index: &'i ModuleIndex,
    scopes: Vec<Scope<'a>>,
    functions: Vec<CollectedFunction<'a>>,
    function_scopes: HashMap<TextSize, (ScopeId, FunctionIndex)>,
}

impl<'a> Collector<'a, '_> {
    fn bind(&mut self, scope: ScopeId, name: &'a str, binding: Binding<'a>) {
        self.scopes[scope]
            .bindings
            .entry(name)
            .or_default()
            .push(binding);
    }

    fn collect_body(&mut self, body: &'a [Stmt], scope: ScopeId, class: Option<&str>) {
        for statement in body {
            self.collect_statement(statement, scope, class);
        }
    }

    fn collect_statement(&mut self, statement: &'a Stmt, scope: ScopeId, class: Option<&str>) {
        match statement {
            Stmt::FunctionDef(definition) => {
                let function = self.collect_function(definition, scope, class);

                if class.is_none() {
                    self.bind(scope, definition.name.as_str(), Binding::Function(function));
                }
            }
            Stmt::ClassDef(definition) => {
                self.collect_body(&definition.body, scope, Some(definition.name.as_str()));

                if class.is_none() {
                    self.bind(scope, definition.name.as_str(), Binding::Unknown);
                }
            }
            _ if class.is_some() => self.collect_nested_bodies(statement, scope, class),
            Stmt::Assign(assign) => {
                for target in &assign.targets {
                    self.bind_target(scope, target, &assign.value);
                }
            }
            Stmt::AnnAssign(assign) => {
                if let Some(value) = &assign.value {
                    self.bind_target(scope, &assign.target, value);
                }
            }
            Stmt::AugAssign(assign) => {
                if let AstExpr::Name(name) = assign.target.as_ref() {
                    self.bind(scope, name.id.as_str(), Binding::Unknown);
                }
            }
            Stmt::For(for_statement) => {
                self.bind_elements(scope, &for_statement.target, &for_statement.iter);
                self.collect_body(&for_statement.body, scope, class);
                self.collect_body(&for_statement.orelse, scope, class);
            }
            Stmt::With(with) => {
                for item in &with.items {
                    if let Some(variables) = &item.optional_vars {
                        self.bind_unknown(scope, variables);
                    }
                }

                self.collect_body(&with.body, scope, class);
            }
            Stmt::Import(import) => {
                for alias in &import.names {
                    let dotted = alias.name.as_str();
                    let (name, module) = match &alias.asname {
                        Some(asname) => (asname.as_str(), dotted),
                        None => {
                            let top_level = dotted.split('.').next().unwrap_or(dotted);

                            (top_level, top_level)
                        }
                    };
                    let module = self.module_ref(module);

                    self.bind(scope, name, Binding::Module(module));
                }
            }
            Stmt::ImportFrom(import) => {
                for alias in &import.names {
                    if alias.name.as_str() == "*" {
                        continue;
                    }

                    let name = alias
                        .asname
                        .as_ref()
                        .map_or(alias.name.as_str(), |asname| asname.as_str());
                    let binding = self.imported_binding(import, alias.name.as_str());

                    self.bind(scope, name, binding);
                }
            }
            Stmt::Try(try_statement) => {
                self.collect_body(&try_statement.body, scope, class);

                for ExceptHandler::ExceptHandler(handler) in &try_statement.handlers {
                    if let Some(name) = &handler.name {
                        self.bind(scope, name.as_str(), Binding::Unknown);
                    }

                    self.collect_body(&handler.body, scope, class);
                }

                self.collect_body(&try_statement.orelse, scope, class);
                self.collect_body(&try_statement.finalbody, scope, class);
            }
            _ => self.collect_nested_bodies(statement, scope, class),
        }
    }

    fn collect_nested_bodies(&mut self, statement: &'a Stmt, scope: ScopeId, class: Option<&str>) {
        match statement {
            Stmt::If(if_statement) => {
                self.collect_body(&if_statement.body, scope, class);

                for clause in &if_statement.elif_else_clauses {
                    self.collect_body(&clause.body, scope, class);
                }
            }
            Stmt::While(while_statement) => {
                self.collect_body(&while_statement.body, scope, class);
                self.collect_body(&while_statement.orelse, scope, class);
            }
            Stmt::Match(match_statement) => {
                for case in &match_statement.cases {
                    self.collect_body(&case.body, scope, class);
                }
            }
            Stmt::Try(try_statement) if class.is_some() => {
                self.collect_body(&try_statement.body, scope, class);
                self.collect_body(&try_statement.orelse, scope, class);
                self.collect_body(&try_statement.finalbody, scope, class);
            }
            Stmt::For(for_statement) if class.is_some() => {
                self.collect_body(&for_statement.body, scope, class);
            }
            Stmt::With(with) if class.is_some() => {
                self.collect_body(&with.body, scope, class);
            }
            _ => {}
        }
    }

    fn collect_function(
        &mut self,
        definition: &'a ast::StmtFunctionDef,
        scope: ScopeId,
        class: Option<&str>,
    ) -> FunctionIndex {
        let function = self.functions.len();
        let parameters: HashMap<&'a str, usize> = parameter_names(&definition.parameters)
            .into_iter()
            .enumerate()
            .map(|(index, name)| (name, index))
            .collect();
        let function_scope = self.scopes.len();
        let escapes = definition.decorator_list.iter().any(|decorator| {
            !TRANSPARENT_DECORATORS.contains(&decorator_name(&decorator.expression).as_str())
        });

        self.scopes.push(Scope {
            parent: Some(scope),
            function: Some(function),
            class: class.map(str::to_owned),
            parameters,
            bindings: HashMap::new(),
        });
        self.functions.push(CollectedFunction {
            definition,
            class: class.map(str::to_owned),
            nested: self.scopes[scope].function.is_some(),
            scope: function_scope,
            escapes,
        });
        self.function_scopes
            .insert(definition.range().start(), (function_scope, function));
        self.collect_body(&definition.body, function_scope, None);

        function
    }

    fn bind_target(&mut self, scope: ScopeId, target: &'a AstExpr, value: &'a AstExpr) {
        match (target, value) {
            (AstExpr::Name(name), value) => {
                self.bind(scope, name.id.as_str(), Binding::Value(value))
            }
            (AstExpr::Tuple(targets), AstExpr::Tuple(values))
                if targets.elts.len() == values.elts.len() =>
            {
                for (target, value) in targets.elts.iter().zip(&values.elts) {
                    self.bind_target(scope, target, value);
                }
            }
            (target, _) => self.bind_unknown(scope, target),
        }
    }

    fn bind_elements(&mut self, scope: ScopeId, target: &'a AstExpr, iterable: &'a AstExpr) {
        let items = match target {
            AstExpr::Tuple(tuple) => Some(&tuple.elts),
            AstExpr::List(list) => Some(&list.elts),
            _ => None,
        };

        match (target, items) {
            (AstExpr::Name(name), _) => {
                self.bind(scope, name.id.as_str(), Binding::Element(iterable));
            }
            (_, Some(items)) => {
                for (index, item) in items.iter().enumerate() {
                    match item {
                        AstExpr::Name(name) => {
                            self.bind(scope, name.id.as_str(), Binding::Item(iterable, index));
                        }
                        item => self.bind_unknown(scope, item),
                    }
                }
            }
            (target, None) => self.bind_unknown(scope, target),
        }
    }

    fn bind_unknown(&mut self, scope: ScopeId, target: &'a AstExpr) {
        match target {
            AstExpr::Name(name) => self.bind(scope, name.id.as_str(), Binding::Unknown),
            AstExpr::Tuple(targets) => {
                for target in &targets.elts {
                    self.bind_unknown(scope, target);
                }
            }
            AstExpr::List(targets) => {
                for target in &targets.elts {
                    self.bind_unknown(scope, target);
                }
            }
            AstExpr::Starred(starred) => self.bind_unknown(scope, &starred.value),
            _ => {}
        }
    }

    fn module_ref(&self, dotted: &str) -> ModuleRef {
        match self
            .index
            .resolve_module(self.path, dotted)
            .into_iter()
            .next()
        {
            Some(resolved) => ModuleRef::Repo(resolved.module),
            None => ModuleRef::Named(dotted.to_owned()),
        }
    }

    fn imported_binding(&self, import: &ast::StmtImportFrom, name: &str) -> Binding<'a> {
        let module = import.module.as_ref().map_or("", |module| module.as_str());

        if import.level == 0 {
            let submodule = format!("{module}.{name}");

            return match self
                .index
                .resolve_module(self.path, &submodule)
                .into_iter()
                .next()
            {
                Some(resolved) => Binding::Module(ModuleRef::Repo(resolved.module)),
                None => Binding::Imported(self.module_ref(module), name.to_owned()),
            };
        }

        let Some(directory) = self.index.relative_directory(self.path, import.level) else {
            return Binding::Unknown;
        };
        let submodule = if module.is_empty() {
            name.to_owned()
        } else {
            format!("{module}.{name}")
        };

        if let Some(resolved) = self.index.resolve_in_directory(&directory, &submodule) {
            return Binding::Module(ModuleRef::Repo(resolved.module));
        }

        match self.index.resolve_in_directory(&directory, module) {
            Some(resolved) => Binding::Imported(ModuleRef::Repo(resolved.module), name.to_owned()),
            None => Binding::Unknown,
        }
    }
}

struct Summarizer<'a, 'i> {
    path: &'i RepoPath,
    index: &'i ModuleIndex,
    source: &'i str,
    line_starts: Vec<usize>,
    scopes: Vec<Scope<'a>>,
    function_scopes: HashMap<TextSize, (ScopeId, FunctionIndex)>,
    converted: RefCell<HashMap<(ScopeId, &'a str), Expr>>,
    converting: RefCell<HashSet<(ScopeId, &'a str)>>,
}

impl<'a> Summarizer<'a, '_> {
    fn line(&self, offset: TextSize) -> u32 {
        let offset = offset.to_usize();

        self.line_starts.partition_point(|&start| start <= offset) as u32
    }

    fn text(&self, node: &impl Ranged) -> String {
        let text = self
            .source
            .get(node.range().to_std_range())
            .unwrap_or_default();
        let mut text: String = text.split_whitespace().collect::<Vec<_>>().join(" ");

        if text.len() > MAX_EXPRESSION_LENGTH {
            let mut end = MAX_EXPRESSION_LENGTH;

            while !text.is_char_boundary(end) {
                end -= 1;
            }

            text.truncate(end);
            text.push('…');
        }

        text
    }

    fn function_summary(&self, function: &CollectedFunction<'a>) -> FunctionSummary {
        let definition = function.definition;
        let parameters = all_parameters(&definition.parameters);
        let defaults = parameters
            .iter()
            .map(|(_, default)| {
                default.map(|default| {
                    let scope = self.scopes[function.scope].parent.unwrap_or_default();

                    self.convert(default, scope, 0)
                })
            })
            .collect();
        let mut returns = Vec::new();

        collect_returns(&definition.body, &mut returns);

        FunctionSummary {
            name: definition.name.as_str().to_owned(),
            class: function.class.clone(),
            nested: function.nested,
            parameters: parameters
                .iter()
                .map(|(name, _)| (*name).to_owned())
                .collect(),
            defaults,
            returns: returns
                .into_iter()
                .map(|value| self.convert(value, function.scope, 0))
                .collect(),
            escapes: function.escapes,
        }
    }

    fn module_bindings(&self) -> HashMap<String, Expr> {
        self.scopes[0]
            .bindings
            .keys()
            .map(|&name| (name.to_owned(), self.lookup(name, 0, 0)))
            .collect()
    }

    fn lookup(&self, name: &'a str, scope: ScopeId, depth: usize) -> Expr {
        let mut current = Some(scope);

        while let Some(scope_id) = current {
            let scope = &self.scopes[scope_id];

            if let (Some(&index), Some(function)) = (scope.parameters.get(name), scope.function) {
                return Expr::Parameter { function, index };
            }

            if scope.bindings.contains_key(name) {
                return self.binding_value(name, scope_id, depth);
            }

            current = scope.parent;
        }

        match name {
            "__file__" => Expr::ModuleFile,
            "__name__" => Expr::ModuleName,
            "__package__" => Expr::PackageName,
            name if BUILTINS.contains(&name) => Expr::Imported {
                module: ModuleRef::Named("builtins".to_owned()),
                name: name.to_owned(),
            },
            _ => Expr::Unknown,
        }
    }

    fn binding_value(&self, name: &'a str, scope: ScopeId, depth: usize) -> Expr {
        let key = (scope, name);

        if let Some(value) = self.converted.borrow().get(&key) {
            return value.clone();
        }

        if !self.converting.borrow_mut().insert(key) {
            return Expr::Unknown;
        }

        let values: Vec<Expr> = self.scopes[scope].bindings[name]
            .clone()
            .into_iter()
            .map(|binding| match binding {
                Binding::Value(value) => self.convert(value, scope, depth + 1),
                Binding::Element(iterable) => {
                    Expr::ElementOf(Box::new(self.convert(iterable, scope, depth + 1)))
                }
                Binding::Item(iterable, index) => Expr::ItemOf {
                    iterable: Box::new(self.convert(iterable, scope, depth + 1)),
                    index,
                },
                Binding::Module(module) => Expr::Module(module),
                Binding::Imported(module, name) => Expr::Imported { module, name },
                Binding::Function(function) => Expr::Function(function),
                Binding::Unknown => Expr::Unknown,
            })
            .collect();
        let value = choice(values);

        self.converting.borrow_mut().remove(&key);
        self.converted.borrow_mut().insert(key, value.clone());

        value
    }

    fn convert(&self, expr: &'a AstExpr, scope: ScopeId, depth: usize) -> Expr {
        if depth > MAX_CONVERSION_DEPTH {
            return Expr::Unknown;
        }

        let depth = depth + 1;

        match expr {
            AstExpr::StringLiteral(literal) => Expr::Text(literal.value.to_str().to_owned()),
            AstExpr::FString(fstring) => {
                let mut parts = Vec::new();

                for part in fstring.value.iter() {
                    match part {
                        ast::FStringPartRef::Literal(literal) => {
                            parts.push(Expr::Text(literal.value.to_string()));
                        }
                        ast::FStringPartRef::FString(fstring) => {
                            for element in &fstring.elements {
                                match element {
                                    InterpolatedStringElement::Literal(literal) => {
                                        parts.push(Expr::Text(literal.value.to_string()));
                                    }
                                    InterpolatedStringElement::Interpolation(interpolation) => {
                                        parts.push(self.convert(
                                            &interpolation.expression,
                                            scope,
                                            depth,
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }

                Expr::Concat(parts)
            }
            AstExpr::Name(name) => self.lookup(name.id.as_str(), scope, depth),
            AstExpr::Attribute(attribute) => {
                if let AstExpr::Name(object) = attribute.value.as_ref()
                    && let Some(class) = self.method_receiver(object.id.as_str(), scope)
                {
                    return Expr::Method {
                        class,
                        name: attribute.attr.as_str().to_owned(),
                    };
                }

                Expr::Attribute {
                    object: Box::new(self.convert(&attribute.value, scope, depth)),
                    name: attribute.attr.as_str().to_owned(),
                }
            }
            AstExpr::BinOp(binary) => {
                let left = self.convert(&binary.left, scope, depth);

                match binary.op {
                    Operator::Add => {
                        Expr::Concat(vec![left, self.convert(&binary.right, scope, depth)])
                    }
                    Operator::Div => Expr::Join(
                        Box::new(left),
                        Box::new(self.convert(&binary.right, scope, depth)),
                    ),
                    Operator::Mod => match binary.left.as_ref() {
                        AstExpr::StringLiteral(template) => {
                            let arguments = match binary.right.as_ref() {
                                AstExpr::Tuple(tuple) => tuple
                                    .elts
                                    .iter()
                                    .map(|element| self.convert(element, scope, depth))
                                    .collect(),
                                right => vec![self.convert(right, scope, depth)],
                            };

                            printf_format(template.value.to_str(), arguments)
                        }
                        _ => Expr::Unknown,
                    },
                    _ => Expr::Unknown,
                }
            }
            AstExpr::Call(call) => {
                if let AstExpr::Attribute(attribute) = call.func.as_ref()
                    && attribute.attr.as_str() == "format"
                    && let AstExpr::StringLiteral(template) = attribute.value.as_ref()
                {
                    let arguments = call
                        .arguments
                        .args
                        .iter()
                        .map(|argument| self.convert(argument, scope, depth))
                        .collect();
                    let keywords = self.keywords(&call.arguments.keywords, scope, depth);

                    return brace_format(template.value.to_str(), arguments, &keywords);
                }

                Expr::Call {
                    callee: Box::new(self.convert(&call.func, scope, depth)),
                    arguments: call
                        .arguments
                        .args
                        .iter()
                        .map(|argument| self.convert(argument, scope, depth))
                        .collect(),
                    keywords: self.keywords(&call.arguments.keywords, scope, depth),
                }
            }
            AstExpr::Tuple(tuple) => self.sequence(&tuple.elts, scope, depth),
            AstExpr::List(list) => self.sequence(&list.elts, scope, depth),
            AstExpr::Set(set) => self.sequence(&set.elts, scope, depth),
            AstExpr::If(conditional) => choice(vec![
                self.convert(&conditional.body, scope, depth),
                self.convert(&conditional.orelse, scope, depth),
            ]),
            AstExpr::BoolOp(boolean) => choice(
                boolean
                    .values
                    .iter()
                    .map(|value| self.convert(value, scope, depth))
                    .collect(),
            ),
            AstExpr::Named(named) => self.convert(&named.value, scope, depth),
            AstExpr::Await(awaited) => self.convert(&awaited.value, scope, depth),
            _ => Expr::Unknown,
        }
    }

    fn sequence(&self, elements: &'a [AstExpr], scope: ScopeId, depth: usize) -> Expr {
        Expr::Sequence(
            elements
                .iter()
                .map(|element| match element {
                    AstExpr::Starred(starred) => {
                        Expr::ElementOf(Box::new(self.convert(&starred.value, scope, depth)))
                    }
                    element => self.convert(element, scope, depth),
                })
                .collect(),
        )
    }

    fn keywords(
        &self,
        keywords: &'a [ast::Keyword],
        scope: ScopeId,
        depth: usize,
    ) -> Vec<(String, Expr)> {
        keywords
            .iter()
            .filter_map(|keyword| {
                let name = keyword.arg.as_ref()?.as_str().to_owned();

                Some((name, self.convert(&keyword.value, scope, depth)))
            })
            .collect()
    }

    fn method_receiver(&self, name: &str, scope: ScopeId) -> Option<String> {
        let scope = &self.scopes[scope];
        let class = scope.class.as_ref()?;

        (scope.parameters.get(name) == Some(&0) && !scope.bindings.contains_key(name))
            .then(|| class.clone())
    }

    fn qualified_name(&self, expr: &'a AstExpr, scope: ScopeId) -> Option<String> {
        match expr {
            AstExpr::Name(name) => match self.shallow(name.id.as_str(), scope) {
                Shallow::Reference(reference) => qualified(&reference),
                Shallow::Unbound if BUILTINS.contains(&name.id.as_str()) => {
                    Some(format!("builtins.{}", name.id.as_str()))
                }
                Shallow::Unbound | Shallow::Opaque => None,
            },
            AstExpr::Attribute(attribute) => Some(format!(
                "{}.{}",
                self.qualified_name(&attribute.value, scope)?,
                attribute.attr.as_str()
            )),
            _ => None,
        }
    }

    fn function_reference(&self, expr: &'a AstExpr, scope: ScopeId) -> Option<Expr> {
        let reference = match expr {
            AstExpr::Name(name) => self.shallow(name.id.as_str(), scope).reference()?,
            AstExpr::Attribute(attribute) => match attribute.value.as_ref() {
                AstExpr::Name(object) => {
                    if let Some(class) = self.method_receiver(object.id.as_str(), scope) {
                        Expr::Method {
                            class,
                            name: attribute.attr.as_str().to_owned(),
                        }
                    } else {
                        match self.shallow(object.id.as_str(), scope).reference()? {
                            Expr::Module(ModuleRef::Repo(module)) => Expr::Imported {
                                module: ModuleRef::Repo(module),
                                name: attribute.attr.as_str().to_owned(),
                            },
                            _ => return None,
                        }
                    }
                }
                _ => return None,
            },
            _ => return None,
        };

        matches!(
            reference,
            Expr::Function(_)
                | Expr::Method { .. }
                | Expr::Imported {
                    module: ModuleRef::Repo(_),
                    ..
                }
        )
        .then_some(reference)
    }

    fn shallow(&self, name: &str, scope: ScopeId) -> Shallow {
        let mut current = Some(scope);

        while let Some(scope_id) = current {
            let scope = &self.scopes[scope_id];

            if scope.parameters.contains_key(name) {
                return Shallow::Opaque;
            }

            if let Some(bindings) = scope.bindings.get(name) {
                return match bindings.as_slice() {
                    [Binding::Function(function)] => Shallow::Reference(Expr::Function(*function)),
                    [Binding::Module(module)] => Shallow::Reference(Expr::Module(module.clone())),
                    [Binding::Imported(module, name)] => Shallow::Reference(Expr::Imported {
                        module: module.clone(),
                        name: name.clone(),
                    }),
                    _ => Shallow::Opaque,
                };
            }

            current = scope.parent;
        }

        Shallow::Unbound
    }
}

enum Shallow {
    Unbound,
    Opaque,
    Reference(Expr),
}

impl Shallow {
    fn reference(self) -> Option<Expr> {
        match self {
            Shallow::Reference(reference) => Some(reference),
            Shallow::Unbound | Shallow::Opaque => None,
        }
    }
}

struct SummaryVisitor<'s, 'a, 'i> {
    summarizer: &'s Summarizer<'a, 'i>,
    summary: ModuleSummary,
    scope: ScopeId,
    type_only: bool,
    inside_path: bool,
}

impl<'a> SummaryVisitor<'_, 'a, '_> {
    fn unit(&self) -> Option<FunctionIndex> {
        self.summarizer.scopes[self.scope].function
    }

    fn add_site(&mut self, node: &impl Ranged, kind: SiteKind, value: Expr, context: Option<Expr>) {
        self.summary.sites.push(Site {
            line: self.summarizer.line(node.range().start()),
            unit: self.unit(),
            expression: self.summarizer.text(node),
            kind,
            value,
            context,
        });
    }

    fn add_import(&mut self, statement: &impl Ranged, target: ImportTarget) {
        self.summary.imports.push(StaticImport {
            line: self.summarizer.line(statement.range().start()),
            target,
            type_only: self.type_only,
        });
    }

    fn add_native_import(&mut self, statement: &impl Ranged, module: Option<usize>) {
        let Some(module) = module.filter(|_| !self.type_only) else {
            return;
        };
        let line = self.summarizer.line(statement.range().start());
        let native_import = NativeImport { line, module };

        if !self.summary.native_imports.contains(&native_import) {
            self.summary.native_imports.push(native_import);
        }
    }

    fn import_from(&mut self, import: &ast::StmtImportFrom) {
        let index = self.summarizer.index;
        let path = self.summarizer.path;
        let module = import.module.as_ref().map_or("", |module| module.as_str());
        let names: Vec<&str> = import
            .names
            .iter()
            .map(|alias| alias.name.as_str())
            .filter(|name| *name != "*")
            .collect();

        if import.level == 0 {
            let mut target = index.resolve_absolute(path, module);

            self.add_native_import(import, index.native_module_named(path, module));

            for name in &names {
                let native = index.native_module_named(path, &format!("{module}.{name}"));

                self.add_native_import(import, native);
            }

            if let ImportTarget::Repo(files) = &mut target {
                for name in names {
                    for resolved in index.resolve_module(path, &format!("{module}.{name}")) {
                        files.extend(resolved.files());
                    }
                }

                files.sort();
                files.dedup();
            }

            self.add_import(import, target);

            return;
        }

        let Some(directory) = index.relative_directory(path, import.level) else {
            return;
        };
        let mut files = Vec::new();

        if let Some(resolved) = index.resolve_in_directory(&directory, module) {
            files.extend(resolved.files());
        }

        if !module.is_empty() {
            let location = join_directory(&directory, &module.replace('.', "/"));

            self.add_native_import(import, index.native_module_at(&location));
        }

        for name in names {
            let submodule = if module.is_empty() {
                name.to_owned()
            } else {
                format!("{module}.{name}")
            };
            let location = join_directory(&directory, &submodule.replace('.', "/"));

            self.add_native_import(import, index.native_module_at(&location));

            if let Some(resolved) = index.resolve_in_directory(&directory, &submodule) {
                files.extend(resolved.files());
            }
        }

        files.sort();
        files.dedup();

        if !files.is_empty() {
            self.add_import(import, ImportTarget::Repo(files));
        }
    }

    fn visit_call(&mut self, call: &'a ast::ExprCall) {
        let summarizer = self.summarizer;
        let scope = self.scope;
        let qualified_name = summarizer.qualified_name(&call.func, scope);
        let arguments = &call.arguments;
        let first_argument = arguments.args.first().or_else(|| {
            arguments
                .keywords
                .iter()
                .find(|keyword| {
                    keyword
                        .arg
                        .as_ref()
                        .is_some_and(|arg| arg.as_str() == "name")
                })
                .map(|keyword| &keyword.value)
        });

        match qualified_name.as_deref() {
            Some(name) if IMPORT_FUNCTIONS.contains(&name) => {
                if let Some(module) = first_argument {
                    let package = arguments
                        .keywords
                        .iter()
                        .find(|keyword| {
                            keyword
                                .arg
                                .as_ref()
                                .is_some_and(|arg| arg.as_str() == "package")
                        })
                        .map(|keyword| &keyword.value)
                        .or_else(|| {
                            (name == "importlib.import_module")
                                .then(|| arguments.args.get(1))
                                .flatten()
                        })
                        .map(|package| summarizer.convert(package, scope, 0));
                    let value = summarizer.convert(module, scope, 0);

                    self.add_site(call, SiteKind::Import, value, package);
                }
            }
            Some(name) if OPENING_FUNCTIONS.contains(&name) => {
                if let Some(path) = first_argument
                    && !self.inside_path
                    && !is_path_expression(summarizer, path, scope)
                {
                    let value = if name == "pkgutil.get_data" {
                        resource_path(summarizer, arguments, scope)
                    } else {
                        summarizer.convert(path, scope, 0)
                    };

                    self.add_site(call, SiteKind::Path, value, None);
                }
            }
            Some(name) if LISTING_FUNCTIONS.contains(&name) => {
                if let Some(path) = first_argument {
                    let value = summarizer.convert(path, scope, 0);

                    self.add_site(call, SiteKind::Listing, value, None);
                }
            }
            Some(name) if ATTRIBUTE_FUNCTIONS.contains(&name) => {
                let position = usize::from(matches!(name, "builtins.getattr" | "builtins.hasattr"));

                if let Some(attribute) = arguments.args.get(position)
                    && !matches!(attribute, AstExpr::StringLiteral(_))
                {
                    let value = summarizer.convert(attribute, scope, 0);

                    self.add_site(call, SiteKind::Attribute, value, None);
                }
            }
            _ => {}
        }

        if let AstExpr::Attribute(attribute) = call.func.as_ref() {
            let method = attribute.attr.as_str();
            let receiver = summarizer.convert(&attribute.value, scope, 0);
            let pattern = arguments
                .args
                .first()
                .map(|pattern| summarizer.convert(pattern, scope, 0));

            match (method, pattern) {
                ("iterdir", _) => self.add_site(call, SiteKind::Listing, receiver, None),
                ("glob", Some(pattern)) => {
                    let value = Expr::Join(Box::new(receiver), Box::new(pattern));

                    self.add_site(call, SiteKind::Listing, value, None);
                }
                ("rglob", Some(pattern)) => {
                    let recursive =
                        Expr::Join(Box::new(Expr::Text("**".to_owned())), Box::new(pattern));
                    let value = Expr::Join(Box::new(receiver), Box::new(recursive));

                    self.add_site(call, SiteKind::Listing, value, None);
                }
                _ => {}
            }
        }

        match summarizer.function_reference(&call.func, scope) {
            Some(callee) => self.add_call(call, callee, &arguments.args, &arguments.keywords),
            None => {
                self.add_unresolved_attribute(&call.func);

                if let AstExpr::Attribute(attribute) = call.func.as_ref() {
                    let method_call =
                        self.call_site(call, Expr::Unknown, &arguments.args, &arguments.keywords);

                    self.summary
                        .method_calls
                        .push((attribute.attr.as_str().to_owned(), method_call));
                }
            }
        }

        for (position, argument) in arguments.args.iter().enumerate() {
            let Some(callee) = summarizer.function_reference(argument, scope) else {
                continue;
            };
            let rest = &arguments.args[position + 1..];

            if rest.is_empty() {
                self.summary.escapes.push(callee);
            } else {
                self.add_call(call, callee, rest, &[]);
            }
        }
    }

    fn add_call(
        &mut self,
        call: &'a ast::ExprCall,
        callee: Expr,
        arguments: &'a [AstExpr],
        keywords: &'a [ast::Keyword],
    ) {
        let call_site = self.call_site(call, callee, arguments, keywords);

        self.summary.calls.push(call_site);
    }

    fn call_site(
        &self,
        call: &'a ast::ExprCall,
        callee: Expr,
        arguments: &'a [AstExpr],
        keywords: &'a [ast::Keyword],
    ) -> CallSite {
        let summarizer = self.summarizer;
        let scope = self.scope;

        CallSite {
            line: summarizer.line(call.range().start()),
            unit: self.unit(),
            callee,
            arguments: arguments
                .iter()
                .map(|argument| match argument {
                    AstExpr::Starred(_) => Expr::Unknown,
                    argument => summarizer.convert(argument, scope, 0),
                })
                .collect(),
            keywords: summarizer.keywords(keywords, scope, 0),
        }
    }
}

impl<'a> Visitor<'a> for SummaryVisitor<'_, 'a, '_> {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::Import(import) => {
                for alias in &import.names {
                    let index = self.summarizer.index;
                    let target = index.resolve_absolute(self.summarizer.path, alias.name.as_str());
                    let native =
                        index.native_module_named(self.summarizer.path, alias.name.as_str());

                    self.add_import(import, target);
                    self.add_native_import(import, native);
                }
            }
            Stmt::ImportFrom(import) => self.import_from(import),
            Stmt::FunctionDef(definition) => {
                for decorator in &definition.decorator_list {
                    self.visit_decorator(decorator);
                }

                for parameter in definition.parameters.iter_non_variadic_params() {
                    if let Some(default) = &parameter.default {
                        self.visit_expr(default);
                    }
                }

                let Some(&(function_scope, _)) = self
                    .summarizer
                    .function_scopes
                    .get(&definition.range().start())
                else {
                    return;
                };
                let outer_scope = self.scope;

                self.scope = function_scope;
                self.visit_body(&definition.body);
                self.scope = outer_scope;
            }
            Stmt::If(if_statement) if names_type_checking(&if_statement.test) => {
                let outer = self.type_only;

                self.type_only = true;
                self.visit_body(&if_statement.body);
                self.type_only = outer;

                for clause in &if_statement.elif_else_clauses {
                    self.visit_body(&clause.body);
                }
            }
            statement => visitor::walk_stmt(self, statement),
        }
    }

    fn visit_expr(&mut self, expr: &'a AstExpr) {
        let summarizer = self.summarizer;

        match expr {
            AstExpr::StringLiteral(literal) => {
                self.add_string(literal.range().start(), &literal.value)
            }
            AstExpr::Name(_) | AstExpr::Attribute(_) => {
                match summarizer.function_reference(expr, self.scope) {
                    Some(reference) => self.summary.escapes.push(reference),
                    None => self.add_unresolved_attribute(expr),
                }

                if let AstExpr::Attribute(attribute) = expr {
                    self.summary
                        .referenced_attributes
                        .insert(attribute.attr.as_str().to_owned());
                }
            }
            _ => {}
        }

        let starts_path = !self.inside_path && is_path_expression(summarizer, expr, self.scope);

        if starts_path {
            let value = summarizer.convert(expr, self.scope, 0);

            self.add_site(expr, SiteKind::Path, value, None);
            self.inside_path = true;
        }

        match expr {
            AstExpr::Call(call) => {
                self.visit_call(call);

                match call.func.as_ref() {
                    AstExpr::Attribute(attribute) => self.visit_expr(&attribute.value),
                    AstExpr::Name(_) => {}
                    callee => self.visit_expr(callee),
                }

                for argument in &call.arguments.args {
                    if summarizer
                        .function_reference(argument, self.scope)
                        .is_some()
                    {
                        continue;
                    }

                    self.visit_expr(argument);
                }

                for keyword in &call.arguments.keywords {
                    self.visit_expr(&keyword.value);
                }
            }
            expr => visitor::walk_expr(self, expr),
        }

        if starts_path {
            self.inside_path = false;
        }
    }
}

impl SummaryVisitor<'_, '_, '_> {
    fn add_unresolved_attribute(&mut self, expr: &AstExpr) {
        if let AstExpr::Attribute(attribute) = expr {
            self.summary
                .unresolved_attributes
                .insert(attribute.attr.as_str().to_owned());
        }
    }

    fn add_string(&mut self, offset: TextSize, value: &StringLiteralValue) {
        let text = value.to_str();

        if text.len() <= MAX_STRING_LENGTH
            && text.contains(['.', '/'])
            && !text.contains(char::is_whitespace)
        {
            let line = self.summarizer.line(offset);

            self.summary.strings.push((line, text.to_owned()));
        }
    }
}

fn is_path_expression<'a>(
    summarizer: &Summarizer<'a, '_>,
    expr: &'a AstExpr,
    scope: ScopeId,
) -> bool {
    match expr {
        AstExpr::BinOp(binary) if binary.op == Operator::Div => ![&binary.left, &binary.right]
            .iter()
            .any(|operand| matches!(operand.as_ref(), AstExpr::NumberLiteral(_))),
        AstExpr::Call(call) => match call.func.as_ref() {
            AstExpr::Attribute(attribute) if PATH_METHODS.contains(&attribute.attr.as_str()) => {
                true
            }
            func => summarizer
                .qualified_name(func, scope)
                .is_some_and(|name| PATH_FUNCTIONS.contains(&name.as_str())),
        },
        AstExpr::Attribute(attribute) => PATH_ATTRIBUTES.contains(&attribute.attr.as_str()),
        AstExpr::Name(name) => name.id.as_str() == "__file__",
        _ => false,
    }
}

fn resource_path<'a>(
    summarizer: &Summarizer<'a, '_>,
    arguments: &'a ast::Arguments,
    scope: ScopeId,
) -> Expr {
    let [package, resource, ..] = &arguments.args[..] else {
        return Expr::Unknown;
    };
    let package = summarizer.convert(package, scope, 0);
    let resource = summarizer.convert(resource, scope, 0);
    let directory = Expr::Call {
        callee: Box::new(Expr::Imported {
            module: ModuleRef::Named("importlib.resources".to_owned()),
            name: "files".to_owned(),
        }),
        arguments: vec![package],
        keywords: Vec::new(),
    };

    Expr::Join(Box::new(directory), Box::new(resource))
}

fn names_type_checking(test: &AstExpr) -> bool {
    match test {
        AstExpr::Name(name) => name.id.as_str() == TYPE_CHECKING,
        AstExpr::Attribute(attribute) => attribute.attr.as_str() == TYPE_CHECKING,
        _ => false,
    }
}

pub(crate) fn qualified(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Module(ModuleRef::Named(module)) => Some(module.clone()),
        Expr::Imported {
            module: ModuleRef::Named(module),
            name,
        } => Some(format!("{module}.{name}")),
        Expr::Attribute { object, name } => Some(format!("{}.{name}", qualified(object)?)),
        _ => None,
    }
}

fn decorator_name(expr: &AstExpr) -> String {
    match expr {
        AstExpr::Name(name) => name.id.as_str().to_owned(),
        AstExpr::Attribute(attribute) => {
            format!(
                "{}.{}",
                decorator_name(&attribute.value),
                attribute.attr.as_str()
            )
        }
        AstExpr::Call(call) => decorator_name(&call.func),
        _ => String::new(),
    }
}

fn parameter_names(parameters: &ast::Parameters) -> Vec<&str> {
    all_parameters(parameters)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

fn all_parameters(parameters: &ast::Parameters) -> Vec<(&str, Option<&AstExpr>)> {
    let mut names: Vec<(&str, Option<&AstExpr>)> = parameters
        .posonlyargs
        .iter()
        .chain(&parameters.args)
        .map(|parameter| {
            (
                parameter.parameter.name.as_str(),
                parameter.default.as_deref(),
            )
        })
        .collect();

    if let Some(vararg) = &parameters.vararg {
        names.push((vararg.name.as_str(), None));
    }

    names.extend(parameters.kwonlyargs.iter().map(|parameter| {
        (
            parameter.parameter.name.as_str(),
            parameter.default.as_deref(),
        )
    }));

    if let Some(kwarg) = &parameters.kwarg {
        names.push((kwarg.name.as_str(), None));
    }

    names
}

fn collect_returns<'a>(body: &'a [Stmt], returns: &mut Vec<&'a AstExpr>) {
    for statement in body {
        match statement {
            Stmt::Return(ast::StmtReturn {
                value: Some(value), ..
            }) => returns.push(value),
            Stmt::If(if_statement) => {
                collect_returns(&if_statement.body, returns);

                for clause in &if_statement.elif_else_clauses {
                    collect_returns(&clause.body, returns);
                }
            }
            Stmt::For(for_statement) => {
                collect_returns(&for_statement.body, returns);
                collect_returns(&for_statement.orelse, returns);
            }
            Stmt::While(while_statement) => {
                collect_returns(&while_statement.body, returns);
                collect_returns(&while_statement.orelse, returns);
            }
            Stmt::With(with) => collect_returns(&with.body, returns),
            Stmt::Try(try_statement) => {
                collect_returns(&try_statement.body, returns);

                for ExceptHandler::ExceptHandler(handler) in &try_statement.handlers {
                    collect_returns(&handler.body, returns);
                }

                collect_returns(&try_statement.orelse, returns);
                collect_returns(&try_statement.finalbody, returns);
            }
            Stmt::Match(match_statement) => {
                for case in &match_statement.cases {
                    collect_returns(&case.body, returns);
                }
            }
            _ => {}
        }
    }
}

fn choice(mut values: Vec<Expr>) -> Expr {
    values.dedup();

    if values.len() == 1 {
        values.pop().unwrap_or(Expr::Unknown)
    } else {
        Expr::Choice(values)
    }
}

fn printf_format(template: &str, arguments: Vec<Expr>) -> Expr {
    let mut parts = Vec::new();
    let mut arguments = arguments.into_iter();
    let mut rest = template;

    while let Some(position) = rest.find('%') {
        parts.push(Expr::Text(rest[..position].to_owned()));

        let after = &rest[position + 1..];

        if let Some(after_percent) = after.strip_prefix('%') {
            parts.push(Expr::Text("%".to_owned()));
            rest = after_percent;

            continue;
        }

        let specifier_end = after
            .find(|character: char| character.is_ascii_alphabetic())
            .map_or(after.len(), |end| end + 1);

        parts.push(arguments.next().unwrap_or(Expr::Unknown));
        rest = &after[specifier_end..];
    }

    parts.push(Expr::Text(rest.to_owned()));

    Expr::Concat(parts)
}

fn brace_format(template: &str, arguments: Vec<Expr>, keywords: &[(String, Expr)]) -> Expr {
    let mut parts = Vec::new();
    let mut next_argument = 0;
    let mut rest = template;

    while let Some(open) = rest.find('{') {
        if rest[open..].starts_with("{{") {
            parts.push(Expr::Text(rest[..=open].to_owned()));
            rest = &rest[open + 2..];

            continue;
        }

        let Some(close) = rest[open..].find('}') else {
            break;
        };
        let field = rest[open + 1..open + close]
            .split([':', '!'])
            .next()
            .unwrap_or_default();
        let value = if field.is_empty() {
            next_argument += 1;

            arguments.get(next_argument - 1).cloned()
        } else if let Ok(position) = field.parse::<usize>() {
            arguments.get(position).cloned()
        } else {
            keywords
                .iter()
                .find(|(name, _)| name == field)
                .map(|(_, value)| value.clone())
        };

        parts.push(Expr::Text(rest[..open].to_owned()));
        parts.push(value.unwrap_or(Expr::Unknown));
        rest = &rest[open + close + 1..];
    }

    parts.push(Expr::Text(rest.replace("}}", "}")));

    Expr::Concat(parts)
}

fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];

    starts.extend(
        source
            .bytes()
            .enumerate()
            .filter(|(_, byte)| *byte == b'\n')
            .map(|(offset, _)| offset + 1),
    );

    starts
}
