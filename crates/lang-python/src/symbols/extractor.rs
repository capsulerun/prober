//! The symbols of one module: load-time code and top-level functions, classes and variables with their fingerprints, the names each uses, and what the module's names are bound to.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};

use prober_core::files::RepoPath;
use prober_core::symbols::{FallbackCause, LineRange, MODULE_INIT_NAME, SymbolKind};
use ruff_python_ast::comparable::ComparableStmt;
use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{self as ast, Expr, ExprContext, ModModule, Stmt};
use ruff_text_size::{Ranged, TextSize};

use crate::conventions::{is_standard_library, join_directory};
use crate::modules::ModuleIndex;
use crate::pytest::items::PytestItems;
use crate::symbols::{
    Binding, ClassHeader, Decoration, Imported, LocalSymbol, ModuleSymbols, Namespace, Target, Use,
};

const PLAIN_DECORATORS: [&str; 27] = [
    "abstractclassmethod",
    "abstractmethod",
    "abstractproperty",
    "abstractstaticmethod",
    "asynccontextmanager",
    "cache",
    "cached_property",
    "classmethod",
    "contextmanager",
    "dataclass",
    "deleter",
    "deprecated",
    "final",
    "getter",
    "lru_cache",
    "no_type_check",
    "overload",
    "override",
    "property",
    "runtime_checkable",
    "setter",
    "singledispatch",
    "singledispatchmethod",
    "staticmethod",
    "total_ordering",
    "unique",
    "wraps",
];

const MARK_PREFIXES: [&str; 2] = ["pytest.mark.", "mark."];

const NON_STORING_CALLS: [&str; 26] = [
    "callable",
    "cast",
    "debug",
    "error",
    "exception",
    "format",
    "getattr",
    "getdoc",
    "getfullargspec",
    "getmodule",
    "getsource",
    "hasattr",
    "info",
    "isasyncgenfunction",
    "isclass",
    "iscoroutinefunction",
    "isfunction",
    "isgeneratorfunction",
    "isinstance",
    "ismethod",
    "partial",
    "signature",
    "update_wrapper",
    "warn",
    "warning",
    "wraps",
];

const TYPE_CHECKING: &str = "TYPE_CHECKING";

const FUTURE_MODULE: &str = "__future__";

const INIT_SUBCLASS: &str = "__init_subclass__";

const EXECUTING_FUNCTIONS: [&str; 2] = ["exec", "eval"];

const NAMESPACE_FUNCTION: &str = "globals";

const NAMESPACE_CALLS: [&str; 3] = ["globals", "locals", "vars"];

const LAZY_ATTRIBUTE_FUNCTIONS: [&str; 2] = ["__getattr__", "__dir__"];

const RECEIVERS: [&str; 2] = ["self", "cls"];

const CLASS_BODY: &str = "<body>";

pub(crate) fn extract_symbols(
    module: &ModModule,
    source: &str,
    path: &RepoPath,
    index: &ModuleIndex,
    items: &PytestItems,
) -> Result<ModuleSymbols, FallbackCause> {
    if let Some(cause) = fallback_cause(module) {
        return Err(cause);
    }

    let mut extractor = Extractor {
        path,
        index,
        items,
        line_starts: line_starts(source),
        symbols: vec![LocalSymbol {
            name: MODULE_INIT_NAME.to_owned(),
            kind: SymbolKind::ModuleInit,
            lines: Vec::new(),
            hash: 0,
            uses: BTreeSet::new(),
            imports: BTreeMap::new(),
            namespace: Namespace::default(),
            decoration: Decoration::Plain,
            decorators: Vec::new(),
            stores_argument: false,
            calls_at_load: false,
            class: None,
        }],
        names: BTreeMap::new(),
        star_imports: Vec::new(),
        by_name: HashMap::new(),
        init_hasher: DefaultHasher::new(),
        fallback: None,
    };

    for statement in &module.body {
        extractor.top_level(statement);
    }

    if let Some(cause) = extractor.fallback {
        return Err(cause);
    }

    let line_count = extractor.line(TextSize::of(source)).max(1);
    let taken: Vec<LineRange> = extractor.symbols[1..]
        .iter()
        .flat_map(|symbol| symbol.lines.iter().copied())
        .collect();
    let init = &mut extractor.symbols[ModuleSymbols::INIT];

    init.lines = ranges_without(
        LineRange {
            start: 1,
            end: line_count,
        },
        &taken,
    );
    init.hash = extractor.init_hasher.finish();

    Ok(ModuleSymbols {
        symbols: extractor.symbols,
        names: extractor.names,
        star_imports: extractor.star_imports,
    })
}

struct Extractor<'e> {
    path: &'e RepoPath,
    index: &'e ModuleIndex,
    items: &'e PytestItems,
    line_starts: Vec<usize>,
    symbols: Vec<LocalSymbol>,
    names: BTreeMap<String, Vec<Binding>>,
    star_imports: Vec<Target>,
    by_name: HashMap<String, usize>,
    init_hasher: DefaultHasher,
    fallback: Option<FallbackCause>,
}

#[derive(Default)]
struct Collected {
    uses: BTreeSet<Use>,
    imports: BTreeMap<String, Vec<Imported>>,
    namespace: Namespace,
    namespace_escapes: bool,
    globals: BTreeSet<String>,
    bound: BTreeSet<String>,
    calls: bool,
}

#[derive(Default)]
struct CollectScope {
    in_class: bool,
    lazy_attributes: bool,
}

struct Definition {
    name: String,
    kind: SymbolKind,
    lines: Vec<LineRange>,
    hash: u64,
    decoration: Decoration,
    class: Option<ClassHeader>,
    value: bool,
    decorators: Vec<Option<Use>>,
    stores_argument: bool,
}

impl Extractor<'_> {
    fn line(&self, offset: TextSize) -> u32 {
        let offset = offset.to_usize();

        self.line_starts.partition_point(|&start| start <= offset) as u32
    }

    fn lines(&self, node: &impl Ranged) -> LineRange {
        LineRange {
            start: self.line(node.range().start()),
            end: self.line(node.range().end()),
        }
    }

    fn collect(&self, visit: impl FnOnce(&mut Collector<'_, '_>)) -> Collected {
        self.collect_in(CollectScope::default(), visit)
    }

    fn collect_in(
        &self,
        scope: CollectScope,
        visit: impl FnOnce(&mut Collector<'_, '_>),
    ) -> Collected {
        let mut collector = Collector {
            extractor: self,
            collected: Collected::default(),
            in_class: scope.in_class,
            lazy_attributes: scope.lazy_attributes,
            namespace_aliases: BTreeSet::new(),
        };

        visit(&mut collector);

        collector.collected
    }

    fn top_level(&mut self, statement: &Stmt) {
        match statement {
            Stmt::FunctionDef(definition) => {
                let lines = self.lines(statement);
                let fixture = self
                    .items
                    .fixtures
                    .iter()
                    .find(|fixture| fixture.lines.start == lines.start);
                let (decoration, decorators) = decoration(&definition.decorator_list);
                let (name, kind, decoration) = match fixture {
                    Some(fixture) => (
                        fixture.name.clone(),
                        SymbolKind::Fixture,
                        Decoration::Fixture,
                    ),
                    None => (
                        definition.name.as_str().to_owned(),
                        SymbolKind::Function,
                        decoration,
                    ),
                };
                let scope = CollectScope {
                    in_class: false,
                    lazy_attributes: LAZY_ATTRIBUTE_FUNCTIONS.contains(&definition.name.as_str()),
                };
                let collected = self.collect_in(scope, |collector| collector.visit_stmt(statement));

                self.define(
                    Definition {
                        name,
                        kind,
                        lines: vec![lines],
                        hash: statement_hash(statement),
                        decoration,
                        class: None,
                        value: false,
                        decorators,
                        stores_argument: stores_argument(statement),
                    },
                    collected,
                    &[definition.name.as_str()],
                );
            }
            Stmt::ClassDef(definition) => self.define_class(definition, statement),
            Stmt::Assign(assign) if let Some(names) = target_names(&assign.targets) => {
                self.define_variable(&names, statement);
            }
            Stmt::AnnAssign(assign)
                if assign.value.is_some()
                    && let Some(names) =
                        target_names(std::slice::from_ref(assign.target.as_ref())) =>
            {
                self.define_variable(&names, statement);
            }
            Stmt::Import(import) => {
                for (name, imported) in self.import_bindings(import) {
                    self.bind(name, Binding::Import(imported));
                }
            }
            Stmt::ImportFrom(import) => {
                let (bindings, star) = self.import_from_bindings(import);

                for (name, imported) in bindings {
                    self.bind(name, Binding::Import(imported));
                }

                self.star_imports.extend(star);
            }
            Stmt::If(branch)
                if branch.elif_else_clauses.is_empty()
                    && (is_type_checking(&branch.test) || is_main_guard(&branch.test)) => {}
            Stmt::If(branch) if is_runtime_only(branch) => {
                let body = match branch.elif_else_clauses.first() {
                    Some(otherwise) => &otherwise.body,
                    None => &branch.body,
                };

                for statement in body {
                    self.top_level(statement);
                }
            }
            statement => self.init_statement(statement),
        }
    }

    fn define(
        &mut self,
        definition: Definition,
        collected: Collected,
        bound_names: &[&str],
    ) -> usize {
        let Collected {
            uses,
            imports,
            namespace,
            namespace_escapes,
            globals,
            calls,
            ..
        } = collected;

        if namespace_escapes {
            self.fallback.get_or_insert(FallbackCause::Namespace);
        }

        let symbol = LocalSymbol {
            calls_at_load: definition.value && calls,
            name: definition.name,
            kind: definition.kind,
            lines: definition.lines,
            hash: definition.hash,
            uses,
            imports,
            namespace,
            decoration: definition.decoration,
            decorators: definition.decorators,
            stores_argument: definition.stores_argument,
            class: definition.class,
        };
        let index = match self.by_name.get(&symbol.name) {
            Some(&index) => {
                merge(&mut self.symbols[index], symbol);

                index
            }
            None => {
                self.by_name.insert(symbol.name.clone(), self.symbols.len());
                self.symbols.push(symbol);

                self.symbols.len() - 1
            }
        };

        for bound in bound_names
            .iter()
            .copied()
            .chain(globals.iter().map(String::as_str))
        {
            self.bind(bound.to_owned(), Binding::Local(index));
        }

        index
    }

    fn define_variable(&mut self, names: &[String], statement: &Stmt) {
        let bound: Vec<&str> = names.iter().map(String::as_str).collect();
        let collected = self.collect(|collector| collector.visit_stmt(statement));

        self.define(
            Definition {
                name: names[0].clone(),
                kind: SymbolKind::Variable,
                lines: vec![self.lines(statement)],
                hash: statement_hash(statement),
                decoration: Decoration::Plain,
                class: None,
                value: true,
                decorators: Vec::new(),
                stores_argument: false,
            },
            collected,
            &bound,
        );
    }

    fn define_class(&mut self, definition: &ast::StmtClassDef, statement: &Stmt) {
        let class_name = definition.name.as_str();
        let scope = CollectScope {
            in_class: true,
            lazy_attributes: false,
        };
        let collected = self.collect_in(scope, |collector| {
            for decorator in &definition.decorator_list {
                collector.visit_expr(&decorator.expression);
            }

            if let Some(arguments) = &definition.arguments {
                collector.visit_arguments(arguments);
            }

            collector.visit_body(&definition.body);
        });
        let (decoration, decorators) = decoration(&definition.decorator_list);

        self.define(
            Definition {
                name: class_name.to_owned(),
                kind: SymbolKind::Class,
                lines: vec![self.lines(statement)],
                hash: statement_hash(statement),
                decoration,
                class: Some(class_header(definition)),
                value: false,
                decorators,
                stores_argument: false,
            },
            collected,
            &[class_name],
        );

        for member in &definition.body {
            self.define_member(class_name, member);
        }
    }

    fn define_member(&mut self, class_name: &str, statement: &Stmt) {
        let (name, decoration, decorators) = match statement {
            Stmt::FunctionDef(member) => {
                let (decoration, decorators) = decoration(&member.decorator_list);

                (member.name.as_str().to_owned(), decoration, decorators)
            }
            Stmt::ClassDef(member) => {
                let (decoration, decorators) = decoration(&member.decorator_list);

                (member.name.as_str().to_owned(), decoration, decorators)
            }
            Stmt::Assign(assign) if let Some(names) = target_names(&assign.targets) => {
                (names[0].clone(), Decoration::Plain, Vec::new())
            }
            Stmt::AnnAssign(assign)
                if let Some(names) = target_names(std::slice::from_ref(assign.target.as_ref())) =>
            {
                (names[0].clone(), Decoration::Plain, Vec::new())
            }
            _ => (CLASS_BODY.to_owned(), Decoration::Plain, Vec::new()),
        };
        let value = !matches!(statement, Stmt::FunctionDef(_) | Stmt::ClassDef(_));
        let scope = CollectScope {
            in_class: true,
            lazy_attributes: false,
        };
        let collected = self.collect_in(scope, |collector| collector.visit_stmt(statement));

        if decoration == Decoration::Plain && !(value && collected.calls) {
            return;
        }

        self.define(
            Definition {
                name: format!("{class_name}.{name}"),
                kind: SymbolKind::Member,
                lines: vec![self.lines(statement)],
                hash: statement_hash(statement),
                decoration,
                class: None,
                value,
                decorators,
                stores_argument: false,
            },
            collected,
            &[],
        );
    }

    fn init_statement(&mut self, statement: &Stmt) {
        ComparableStmt::from(statement).hash(&mut self.init_hasher);

        let Collected {
            uses,
            imports,
            namespace,
            namespace_escapes,
            globals,
            bound,
            ..
        } = self.collect(|collector| collector.visit_stmt(statement));

        if namespace_escapes {
            self.fallback.get_or_insert(FallbackCause::Namespace);
        }

        let init = &mut self.symbols[ModuleSymbols::INIT];

        init.namespace.reads |= namespace.reads;
        init.namespace.writes |= namespace.writes;

        init.uses.extend(uses);

        for (name, imported) in imports {
            for imported in imported {
                self.bind(name.clone(), Binding::Import(imported));
            }
        }

        for name in bound.into_iter().chain(globals) {
            self.bind(name, Binding::Local(ModuleSymbols::INIT));
        }
    }

    fn bind(&mut self, name: String, binding: Binding) {
        let bindings = self.names.entry(name).or_default();

        if !bindings.contains(&binding) {
            bindings.push(binding);
        }
    }

    fn target(&self, level: u32, dotted: &str) -> Target {
        let index = self.index;

        if level == 0 {
            if dotted.is_empty() {
                return Target::Missing;
            }

            if index.native_module_named(self.path, dotted).is_some() {
                return Target::Native;
            }

            if let Some(resolved) = index.resolve_module(self.path, dotted).into_iter().next() {
                return Target::Repo(resolved.module);
            }

            let top_level = dotted.split('.').next().unwrap_or(dotted).to_owned();

            return if is_standard_library(&top_level) {
                Target::Standard(top_level)
            } else {
                Target::External(top_level)
            };
        }

        let Some(directory) = index.relative_directory(self.path, level) else {
            return Target::Missing;
        };

        if index
            .native_module_at(&join_directory(&directory, &dotted.replace('.', "/")))
            .is_some()
        {
            return Target::Native;
        }

        index
            .resolve_in_directory(&directory, dotted)
            .map_or(Target::Missing, |resolved| Target::Repo(resolved.module))
    }

    fn import_bindings(&self, import: &ast::StmtImport) -> Vec<(String, Imported)> {
        import
            .names
            .iter()
            .map(|alias| {
                let dotted = alias.name.as_str();

                match &alias.asname {
                    Some(asname) => (
                        asname.as_str().to_owned(),
                        Imported::Module(self.target(0, dotted)),
                    ),
                    None => {
                        let top_level = dotted.split('.').next().unwrap_or(dotted);

                        (
                            top_level.to_owned(),
                            Imported::Module(self.target(0, top_level)),
                        )
                    }
                }
            })
            .collect()
    }

    fn import_from_bindings(
        &self,
        import: &ast::StmtImportFrom,
    ) -> (Vec<(String, Imported)>, Option<Target>) {
        let module = import.module.as_ref().map_or("", |module| module.as_str());

        if import.level == 0 && module == FUTURE_MODULE {
            return (Vec::new(), None);
        }

        let module_target = self.target(import.level, module);
        let mut bindings = Vec::new();
        let mut star = None;

        for alias in &import.names {
            let name = alias.name.as_str();

            if name == "*" {
                star = Some(module_target.clone());

                continue;
            }

            let bound = alias.asname.as_ref().map_or(name, |asname| asname.as_str());
            let submodule = if module.is_empty() {
                name.to_owned()
            } else {
                format!("{module}.{name}")
            };

            if module_target != Target::Missing {
                bindings.push((
                    bound.to_owned(),
                    Imported::Name {
                        module: module_target.clone(),
                        name: name.to_owned(),
                    },
                ));
            }

            if let Target::Repo(submodule) = self.target(import.level, &submodule) {
                bindings.push((bound.to_owned(), Imported::Module(Target::Repo(submodule))));
            }
        }

        (bindings, star)
    }
}

fn merge(symbol: &mut LocalSymbol, other: LocalSymbol) {
    let mut hasher = DefaultHasher::new();

    symbol.hash.hash(&mut hasher);
    other.hash.hash(&mut hasher);
    symbol.hash = hasher.finish();
    symbol.lines.extend(other.lines);
    symbol.uses.extend(other.uses);
    symbol.namespace.reads |= other.namespace.reads;
    symbol.namespace.writes |= other.namespace.writes;

    for (name, imported) in other.imports {
        symbol.imports.entry(name).or_default().extend(imported);
    }

    if other.decoration == Decoration::Registering {
        symbol.decoration = Decoration::Registering;
    }

    symbol.decorators.extend(other.decorators);
    symbol.stores_argument |= other.stores_argument;
    symbol.calls_at_load |= other.calls_at_load;
}

struct Collector<'c, 'e> {
    extractor: &'c Extractor<'e>,
    collected: Collected,
    in_class: bool,
    lazy_attributes: bool,
    namespace_aliases: BTreeSet<String>,
}

impl Collector<'_, '_> {
    fn import(&mut self, bindings: Vec<(String, Imported)>) {
        for (name, imported) in bindings {
            let imports = self.collected.imports.entry(name).or_default();

            if !imports.contains(&imported) {
                imports.push(imported);
            }
        }
    }

    fn is_namespace(&self, expression: &Expr) -> bool {
        match expression {
            Expr::Call(call) => {
                call.arguments.args.is_empty()
                    && matches!(call.func.as_ref(), Expr::Name(name) if name.id.as_str() == NAMESPACE_FUNCTION)
            }
            Expr::Name(name) => self.namespace_aliases.contains(name.id.as_str()),
            _ => false,
        }
    }

    fn namespace_write(&mut self) {
        if !self.lazy_attributes {
            self.collected.namespace.writes = true;
        }
    }
}

impl<'a> Visitor<'a> for Collector<'_, '_> {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::Import(import) => {
                let bindings = self.extractor.import_bindings(import);

                self.import(bindings);
            }
            Stmt::ImportFrom(import) => {
                let (bindings, _) = self.extractor.import_from_bindings(import);

                self.import(bindings);
            }
            Stmt::Global(global) => {
                self.collected
                    .globals
                    .extend(global.names.iter().map(|name| name.as_str().to_owned()));
            }
            Stmt::FunctionDef(definition) => {
                self.collected
                    .bound
                    .insert(definition.name.as_str().to_owned());
                visitor::walk_stmt(self, statement);
            }
            Stmt::ClassDef(definition) => {
                self.collected
                    .bound
                    .insert(definition.name.as_str().to_owned());
                visitor::walk_stmt(self, statement);
            }
            Stmt::Assign(assign) if self.is_namespace(&assign.value) => {
                for target in &assign.targets {
                    match target {
                        Expr::Name(alias) => {
                            self.namespace_aliases.insert(alias.id.as_str().to_owned());
                        }
                        Expr::Attribute(attribute)
                            if self.in_class
                                && matches!(attribute.value.as_ref(), Expr::Name(receiver) if RECEIVERS.contains(&receiver.id.as_str())) =>
                        {
                            self.namespace_write();
                        }
                        _ => self.collected.namespace_escapes = true,
                    }
                }
            }
            _ => visitor::walk_stmt(self, statement),
        }
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        match expression {
            Expr::Subscript(subscript) if self.is_namespace(&subscript.value) => {
                if subscript.ctx == ExprContext::Load {
                    self.collected.namespace.reads = true;
                } else {
                    self.namespace_write();
                }

                self.visit_expr(&subscript.slice);
            }
            Expr::Call(call) if matches!(call.func.as_ref(), Expr::Attribute(method) if self.is_namespace(&method.value)) =>
            {
                let Expr::Attribute(method) = call.func.as_ref() else {
                    return;
                };

                match method.attr.as_str() {
                    "get" | "values" | "items" | "keys" | "copy" | "__contains__"
                    | "__getitem__" => {
                        self.collected.namespace.reads = true;
                    }
                    "update" | "setdefault" | "pop" | "popitem" | "clear" | "__setitem__"
                    | "__delitem__" => self.namespace_write(),
                    _ => self.collected.namespace_escapes = true,
                }

                self.collected.calls = true;

                for argument in &call.arguments.args {
                    self.visit_expr(argument);
                }

                for keyword in &call.arguments.keywords {
                    self.visit_expr(&keyword.value);
                }
            }
            Expr::Compare(compare)
                if compare
                    .operands
                    .iter()
                    .any(|operand| self.is_namespace(operand))
                    && compare
                        .ops
                        .iter()
                        .all(|op| matches!(op, ast::CmpOp::In | ast::CmpOp::NotIn)) =>
            {
                self.collected.namespace.reads = true;

                for operand in &compare.operands {
                    if !self.is_namespace(operand) {
                        self.visit_expr(operand);
                    }
                }
            }
            Expr::Call(_) if self.is_namespace(expression) => {
                self.collected.namespace_escapes = true;
            }
            Expr::Name(name)
                if name.ctx == ExprContext::Load
                    && self.namespace_aliases.contains(name.id.as_str()) =>
            {
                self.collected.namespace_escapes = true;
            }
            Expr::Name(name) => match name.ctx {
                ExprContext::Load => {
                    self.collected.uses.insert(Use {
                        name: name.id.as_str().to_owned(),
                        attributes: Vec::new(),
                    });
                }
                ExprContext::Store => {
                    self.collected.bound.insert(name.id.as_str().to_owned());
                }
                _ => {}
            },
            Expr::Attribute(attribute) => match attribute_chain(expression) {
                Some(chain) if attribute.ctx == ExprContext::Load => {
                    self.collected.uses.insert(chain);
                }
                _ => visitor::walk_expr(self, expression),
            },
            Expr::Call(_) => {
                self.collected.calls = true;
                visitor::walk_expr(self, expression);
            }
            Expr::Await(_) | Expr::Yield(_) | Expr::YieldFrom(_) => {
                self.collected.calls = true;
                visitor::walk_expr(self, expression);
            }
            _ => visitor::walk_expr(self, expression),
        }
    }
}

fn fallback_cause(module: &ModModule) -> Option<FallbackCause> {
    let mut finder = FallbackFinder::default();

    finder.visit_body(&module.body);

    finder.cause
}

#[derive(Default)]
struct FallbackFinder {
    cause: Option<FallbackCause>,
}

impl<'a> Visitor<'a> for FallbackFinder {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        if let Stmt::Assign(assign) = statement
            && assign.targets.iter().any(replaces_a_module)
        {
            self.cause.get_or_insert(FallbackCause::Namespace);
        }

        visitor::walk_stmt(self, statement);
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        if let Expr::Call(call) = expression
            && let Expr::Name(callee) = call.func.as_ref()
        {
            let callee = callee.id.as_str();

            let own_namespace = call.arguments.args.get(1).is_none_or(|namespace| {
                matches!(namespace, Expr::Call(inner) if matches!(inner.func.as_ref(), Expr::Name(name) if NAMESPACE_CALLS.contains(&name.id.as_str())))
            });

            if EXECUTING_FUNCTIONS.contains(&callee) && own_namespace {
                self.cause.get_or_insert(FallbackCause::Exec);
            }
        }

        visitor::walk_expr(self, expression);
    }
}

fn replaces_a_module(target: &Expr) -> bool {
    let Expr::Subscript(subscript) = target else {
        return false;
    };

    dotted_name(&subscript.value) == "sys.modules"
}

fn attribute_chain(expression: &Expr) -> Option<Use> {
    let mut attributes = Vec::new();
    let mut current = expression;

    loop {
        match current {
            Expr::Attribute(attribute) => {
                attributes.push(attribute.attr.as_str().to_owned());
                current = &attribute.value;
            }
            Expr::Name(name) => {
                attributes.reverse();

                return Some(Use {
                    name: name.id.as_str().to_owned(),
                    attributes,
                });
            }
            _ => return None,
        }
    }
}

fn base_use(base: &Expr) -> Option<Use> {
    match base {
        Expr::Subscript(subscript) => attribute_chain(&subscript.value),
        base => attribute_chain(base),
    }
}

fn class_header(definition: &ast::StmtClassDef) -> ClassHeader {
    let (bases, keywords) = match &definition.arguments {
        Some(arguments) => (
            arguments.args.iter().map(base_use).collect(),
            !arguments.keywords.is_empty(),
        ),
        None => (Vec::new(), false),
    };
    let init_subclass = definition.body.iter().any(|statement| {
        matches!(statement, Stmt::FunctionDef(method) if method.name.as_str() == INIT_SUBCLASS)
    });

    ClassHeader {
        bases,
        keywords,
        init_subclass,
    }
}

fn decoration(decorators: &[ast::Decorator]) -> (Decoration, Vec<Option<Use>>) {
    let unknown: Vec<Option<Use>> = decorators
        .iter()
        .filter_map(|decorator| {
            let callee = match &decorator.expression {
                Expr::Call(call) => call.func.as_ref(),
                expression => expression,
            };
            let dotted = dotted_name(callee);
            let last = dotted.rsplit('.').next().unwrap_or_default();
            let plain = MARK_PREFIXES
                .iter()
                .any(|prefix| dotted.starts_with(prefix))
                || PLAIN_DECORATORS.contains(&last);

            (!plain).then(|| attribute_chain(callee))
        })
        .collect();

    if unknown.is_empty() {
        (Decoration::Plain, unknown)
    } else {
        (Decoration::Registering, unknown)
    }
}

fn stores_argument(statement: &Stmt) -> bool {
    let mut finder = StoreFinder::default();

    finder.visit_stmt(statement);

    finder.found
}

#[derive(Default)]
struct StoreFinder {
    parameters: BTreeSet<String>,
    found: bool,
}

impl StoreFinder {
    fn is_parameter(&self, expression: &Expr) -> bool {
        matches!(expression, Expr::Name(name) if self.parameters.contains(name.id.as_str()))
    }
}

impl<'a> Visitor<'a> for StoreFinder {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::FunctionDef(definition) => {
                let first = definition
                    .parameters
                    .posonlyargs
                    .iter()
                    .chain(&definition.parameters.args)
                    .next();

                self.parameters
                    .extend(first.map(|parameter| parameter.parameter.name.as_str().to_owned()));
            }
            Stmt::Assign(assign)
                if self.is_parameter(&assign.value)
                    && assign.targets.iter().any(|target| {
                        matches!(target, Expr::Subscript(_) | Expr::Attribute(_))
                    }) =>
            {
                self.found = true;
            }
            _ => {}
        }

        visitor::walk_stmt(self, statement);
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        if let Expr::Call(call) = expression {
            let callee = dotted_name(&call.func);
            let last = callee.rsplit('.').next().unwrap_or_default();
            let passes_parameter = call
                .arguments
                .args
                .iter()
                .chain(call.arguments.keywords.iter().map(|keyword| &keyword.value))
                .any(|argument| self.is_parameter(argument));
            let stores = match call.func.as_ref() {
                Expr::Attribute(_) => passes_parameter && !NON_STORING_CALLS.contains(&last),
                _ => {
                    last == "setattr"
                        && call
                            .arguments
                            .args
                            .get(2)
                            .is_some_and(|value| self.is_parameter(value))
                }
            };

            self.found |= stores;
        }

        visitor::walk_expr(self, expression);
    }
}

fn target_names(targets: &[Expr]) -> Option<Vec<String>> {
    let mut names = Vec::new();

    for target in targets {
        collect_target_names(target, &mut names)?;
    }

    (!names.is_empty()).then_some(names)
}

fn collect_target_names(target: &Expr, names: &mut Vec<String>) -> Option<()> {
    match target {
        Expr::Name(name) => names.push(name.id.as_str().to_owned()),
        Expr::Tuple(tuple) => {
            for element in &tuple.elts {
                collect_target_names(element, names)?;
            }
        }
        Expr::List(list) => {
            for element in &list.elts {
                collect_target_names(element, names)?;
            }
        }
        Expr::Starred(starred) => collect_target_names(&starred.value, names)?,
        _ => return None,
    }

    Some(())
}

fn is_type_checking(test: &Expr) -> bool {
    let dotted = dotted_name(test);

    dotted == TYPE_CHECKING || dotted.ends_with(&format!(".{TYPE_CHECKING}"))
}

fn is_runtime_only(branch: &ast::StmtIf) -> bool {
    let not_type_checking = matches!(
        branch.test.as_ref(),
        Expr::UnaryOp(unary) if unary.op == ast::UnaryOp::Not && is_type_checking(&unary.operand)
    );

    match branch.elif_else_clauses.as_slice() {
        [] => not_type_checking,
        [otherwise] => otherwise.test.is_none() && is_type_checking(&branch.test),
        _ => false,
    }
}

fn is_main_guard(test: &Expr) -> bool {
    let Expr::Compare(compare) = test else {
        return false;
    };
    let sides = &compare.operands;

    sides.len() == 2
        && sides
            .iter()
            .any(|side| matches!(side, Expr::Name(name) if name.id.as_str() == "__name__"))
        && sides.iter().any(|side| {
            matches!(side, Expr::StringLiteral(literal) if literal.value.to_str() == "__main__")
        })
}

fn dotted_name(expression: &Expr) -> String {
    match expression {
        Expr::Name(name) => name.id.as_str().to_owned(),
        Expr::Attribute(attribute) => {
            let value = dotted_name(&attribute.value);

            if value.is_empty() {
                String::new()
            } else {
                format!("{value}.{}", attribute.attr.as_str())
            }
        }
        _ => String::new(),
    }
}

fn statement_hash(statement: &Stmt) -> u64 {
    let mut hasher = DefaultHasher::new();

    ComparableStmt::from(statement).hash(&mut hasher);

    hasher.finish()
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

fn ranges_without(range: LineRange, taken: &[LineRange]) -> Vec<LineRange> {
    let mut ranges = Vec::new();
    let mut start = range.start;
    let mut taken = taken.to_vec();

    taken.sort_by_key(|taken| taken.start);

    for taken in taken {
        if taken.start > start {
            ranges.push(LineRange {
                start,
                end: taken.start - 1,
            });
        }

        start = start.max(taken.end + 1);
    }

    if start <= range.end {
        ranges.push(LineRange {
            start,
            end: range.end,
        });
    }

    ranges
}

#[cfg(test)]
mod tests {
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use super::*;
    use crate::pytest::items::pytest_items;

    fn extract(path: &str, source: &str, others: &[&str]) -> Result<ModuleSymbols, FallbackCause> {
        let path = RepoPath::new(path).unwrap();
        let mut repo_files: Vec<RepoPath> = others
            .iter()
            .map(|other| RepoPath::new(*other).unwrap())
            .collect();

        repo_files.push(path.clone());

        let index = ModuleIndex::new(&repo_files, &|_| None);
        let parsed = parse_unchecked_source(source, PySourceType::Python);
        let items = pytest_items(parsed.syntax(), source);

        extract_symbols(parsed.syntax(), source, &path, &index, &items)
    }

    fn symbol<'m>(module: &'m ModuleSymbols, name: &str) -> &'m LocalSymbol {
        module
            .symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| panic!("no symbol {name}"))
    }

    #[test]
    fn load_time_code_and_top_level_definitions_become_symbols() {
        let module = extract(
            "app/shapes.py",
            "import os\nfrom typing import TYPE_CHECKING\n\nif TYPE_CHECKING:\n    from app.other import Other\n\nLIMIT = 3\nCACHE = os.getenv(\"CACHE\")\n\n\ndef area(side):\n    return side * LIMIT\n\n\nclass Square:\n    sides = 4\n\n    def perimeter(self):\n        return self.side * self.sides\n\n\nprint(area(2))\n\nif __name__ == \"__main__\":\n    area(1)\n",
            &["app/__init__.py", "app/other.py"],
        )
        .unwrap();
        let kinds: Vec<(&str, SymbolKind)> = module
            .symbols
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.kind))
            .collect();

        assert_eq!(
            kinds,
            [
                (MODULE_INIT_NAME, SymbolKind::ModuleInit),
                ("LIMIT", SymbolKind::Variable),
                ("CACHE", SymbolKind::Variable),
                ("area", SymbolKind::Function),
                ("Square", SymbolKind::Class),
            ]
        );
        assert!(!symbol(&module, "LIMIT").calls_at_load);
        assert!(symbol(&module, "CACHE").calls_at_load);
        assert_eq!(
            symbol(&module, "area").lines,
            [LineRange { start: 11, end: 12 }]
        );
        assert!(
            symbol(&module, MODULE_INIT_NAME)
                .lines
                .iter()
                .all(|lines| !lines.contains(12))
        );
        assert!(symbol(&module, MODULE_INIT_NAME).uses.contains(&Use {
            name: "area".to_owned(),
            attributes: Vec::new(),
        }));
        assert!(!module.names.contains_key("Other"));
        assert_eq!(
            symbol(&module, "Square").lines,
            [LineRange { start: 15, end: 19 }]
        );
    }

    #[test]
    fn class_members_acting_at_load_become_symbols_of_their_own() {
        let module = extract(
            "app/views.py",
            "from app.web import named, route\n\n\nclass Views:\n    \"\"\"Pages.\"\"\"\n\n    title = named(\"views\")\n    size = 3\n\n    @route(\"/home\")\n    def home(self):\n        return \"home\"\n\n    @property\n    def helper(self):\n        return 1\n\n    named(title)\n",
            &["app/__init__.py", "app/web.py"],
        )
        .unwrap();
        let kinds: Vec<(&str, SymbolKind)> = module
            .symbols
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.kind))
            .collect();

        assert_eq!(
            kinds,
            [
                (MODULE_INIT_NAME, SymbolKind::ModuleInit),
                ("Views", SymbolKind::Class),
                ("Views.title", SymbolKind::Member),
                ("Views.home", SymbolKind::Member),
                ("Views.<body>", SymbolKind::Member),
            ]
        );
        assert!(symbol(&module, "Views.title").calls_at_load);
        assert_eq!(
            symbol(&module, "Views.home").decoration,
            Decoration::Registering
        );
        assert!(!module.names.contains_key("Views.home"));
        assert_eq!(module.symbol_at(12), 1);
    }

    #[test]
    fn names_are_bound_to_definitions_imports_submodules_and_star_sources() {
        let module = extract(
            "pkg/api.py",
            "import pkg.core\nfrom pkg.core import run as start\nfrom . import tools\nfrom pkg.extra import *\n\n\ndef run():\n    return start()\n",
            &[
                "pkg/__init__.py",
                "pkg/core.py",
                "pkg/tools.py",
                "pkg/extra.py",
            ],
        )
        .unwrap();
        let core = Target::Repo(RepoPath::new("pkg/core.py").unwrap());
        let package = Target::Repo(RepoPath::new("pkg/__init__.py").unwrap());

        assert_eq!(
            module.names["pkg"],
            [Binding::Import(Imported::Module(package.clone()))]
        );
        assert_eq!(
            module.names["start"],
            [Binding::Import(Imported::Name {
                module: core,
                name: "run".to_owned(),
            })]
        );
        assert_eq!(
            module.names["tools"],
            [
                Binding::Import(Imported::Name {
                    module: package,
                    name: "tools".to_owned(),
                }),
                Binding::Import(Imported::Module(Target::Repo(
                    RepoPath::new("pkg/tools.py").unwrap()
                ))),
            ]
        );
        assert_eq!(
            module.star_imports,
            [Target::Repo(RepoPath::new("pkg/extra.py").unwrap())]
        );
        assert_eq!(module.names["run"], [Binding::Local(1)]);
    }

    #[test]
    fn dynamic_namespaces_are_tracked_or_fall_back() {
        let analysed = |source: &str| extract("names.py", source, &[]);

        assert_eq!(
            analysed("exec(\"x = 1\")\n").unwrap_err(),
            FallbackCause::Exec
        );
        assert!(analysed("exec(\"x = 1\", {})\n").is_ok());
        assert_eq!(
            analysed("from registry import fill\n\nfill(globals())\n").unwrap_err(),
            FallbackCause::Namespace
        );
        assert_eq!(
            analysed("import sys\n\nsys.modules[__name__] = object()\n").unwrap_err(),
            FallbackCause::Namespace
        );

        let writes = analysed("def make(name):\n    globals()[name] = 1\n").unwrap();
        let reads = analysed("def names():\n    return list(globals().values())\n").unwrap();

        assert!(symbol(&writes, "make").namespace.writes);
        assert!(symbol(&reads, "names").namespace.reads);
    }

    #[test]
    fn decorators_storing_their_function_are_told_apart_from_wrappers() {
        let module = extract(
            "decorators.py",
            "import functools\n\nREGISTRY = {}\n\n\ndef register(name):\n    def decorate(function):\n        REGISTRY[name] = function\n        return function\n\n    return decorate\n\n\ndef logged(function):\n    @functools.wraps(function)\n    def wrapper(*args):\n        return function(*args)\n\n    wrapper.__logged__ = True\n    return wrapper\n",
            &[],
        )
        .unwrap();

        assert!(symbol(&module, "register").stores_argument);
        assert!(!symbol(&module, "logged").stores_argument);
    }
}
