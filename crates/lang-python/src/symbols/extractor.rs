//! The symbols of one module: load-time code, top-level functions, classes and variables with their fingerprints, the names each uses, and what the module's names are bound to.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};

use prober_core::files::RepoPath;
use prober_core::symbols::{FallbackCause, LineRange, MODULE_INIT_NAME, SymbolKind};
use ruff_python_ast::comparable::{ComparableExpr, ComparableStmt};
use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{self as ast, Expr, ExprContext, ModModule, Stmt};
use ruff_text_size::{Ranged, TextSize};

use crate::conventions::{is_standard_library, join_directory};
use crate::modules::ModuleIndex;
use crate::pytest::items::PytestItems;
use crate::symbols::{
    Attributes, Binding, ClassHeader, Decoration, Imported, LocalSymbol, Member, ModuleSymbols,
    Target, Use,
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

const LAZY_ATTRIBUTE_FUNCTIONS: [&str; 2] = ["__getattr__", "__dir__"];

const PROXY_METHODS: [&str; 2] = ["__getattr__", "__getattribute__"];

const RECEIVERS: [&str; 2] = ["self", "cls"];

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
            attributes: Attributes::default(),
            decoration: Decoration::Plain,
            decorators: Vec::new(),
            stores_argument: false,
            calls_at_load: false,
            class: None,
            member: None,
        }],
        names: BTreeMap::new(),
        star_imports: Vec::new(),
        by_name: HashMap::new(),
        init_hasher: DefaultHasher::new(),
    };

    for statement in &module.body {
        extractor.top_level(statement);
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
}

#[derive(Default)]
struct Collected {
    uses: BTreeSet<Use>,
    imports: BTreeMap<String, Vec<Imported>>,
    attributes: Attributes,
    globals: BTreeSet<String>,
    bound: BTreeSet<String>,
    calls: bool,
}

struct Definition {
    name: String,
    kind: SymbolKind,
    lines: Vec<LineRange>,
    hash: u64,
    decoration: Decoration,
    class: Option<ClassHeader>,
    member: Option<Member>,
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
        self.collect_in(None, visit)
    }

    fn collect_in(
        &self,
        class_name: Option<&str>,
        visit: impl FnOnce(&mut Collector<'_, '_>),
    ) -> Collected {
        let mut collector = Collector {
            extractor: self,
            collected: Collected::default(),
            forwarded: Vec::new(),
            self_like: RECEIVERS
                .iter()
                .map(|receiver| (*receiver).to_owned())
                .collect(),
            class_name: class_name.map(str::to_owned),
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
                let collected = self.collect(|collector| collector.visit_stmt(statement));

                self.define(
                    Definition {
                        name,
                        kind,
                        lines: vec![lines],
                        hash: statement_hash(statement),
                        decoration,
                        class: None,
                        member: None,
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
            attributes,
            globals,
            calls,
            ..
        } = collected;
        let symbol = LocalSymbol {
            calls_at_load: definition.value && calls,
            name: definition.name,
            kind: definition.kind,
            lines: definition.lines,
            hash: definition.hash,
            uses,
            imports,
            attributes,
            decoration: definition.decoration,
            decorators: definition.decorators,
            stores_argument: definition.stores_argument,
            class: definition.class,
            member: definition.member,
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
                member: None,
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
        let (members, content): (Vec<&Stmt>, Vec<&Stmt>) = definition
            .body
            .iter()
            .partition(|member| member_keys(member).is_some());
        let collected = self.collect(|collector| {
            for decorator in &definition.decorator_list {
                collector.visit_expr(&decorator.expression);
            }

            if let Some(arguments) = &definition.arguments {
                collector.visit_arguments(arguments);
            }

            for statement in &content {
                collector.visit_stmt(statement);
            }
        });
        let mut hasher = DefaultHasher::new();

        class_name.hash(&mut hasher);

        for decorator in &definition.decorator_list {
            ComparableExpr::from(&decorator.expression).hash(&mut hasher);
        }

        if let Some(arguments) = &definition.arguments {
            for base in &arguments.args {
                ComparableExpr::from(base).hash(&mut hasher);
            }

            for keyword in &arguments.keywords {
                keyword
                    .arg
                    .as_ref()
                    .map(|arg| arg.as_str())
                    .hash(&mut hasher);
                ComparableExpr::from(&keyword.value).hash(&mut hasher);
            }
        }

        for statement in &content {
            ComparableStmt::from(*statement).hash(&mut hasher);
        }

        let member_lines: Vec<LineRange> =
            members.iter().map(|member| self.lines(*member)).collect();
        let (class_decoration, class_decorators) = decoration(&definition.decorator_list);
        let class = self.define(
            Definition {
                name: class_name.to_owned(),
                kind: SymbolKind::Class,
                lines: ranges_without(self.lines(statement), &member_lines),
                hash: hasher.finish(),
                decoration: class_decoration,
                class: Some(class_header(definition)),
                member: None,
                value: false,
                decorators: class_decorators,
                stores_argument: false,
            },
            collected,
            &[class_name],
        );

        for member in members {
            let keys = member_keys(member).expect("members have keys");
            let (decoration, decorators) = match member {
                Stmt::FunctionDef(function) => decoration(&function.decorator_list),
                Stmt::ClassDef(nested) => decoration(&nested.decorator_list),
                _ => (Decoration::Plain, Vec::new()),
            };
            let collected =
                self.collect_in(Some(class_name), |collector| collector.visit_stmt(member));

            self.define(
                Definition {
                    name: format!("{class_name}.{}", keys[0]),
                    kind: SymbolKind::Member,
                    lines: vec![self.lines(member)],
                    hash: statement_hash(member),
                    decoration,
                    class: None,
                    member: Some(Member { class, keys }),
                    value: matches!(member, Stmt::Assign(_)),
                    decorators,
                    stores_argument: stores_argument(member),
                },
                collected,
                &[],
            );
        }
    }

    fn init_statement(&mut self, statement: &Stmt) {
        ComparableStmt::from(statement).hash(&mut self.init_hasher);

        let Collected {
            uses,
            imports,
            attributes,
            globals,
            bound,
            ..
        } = self.collect(|collector| collector.visit_stmt(statement));
        let init = &mut self.symbols[ModuleSymbols::INIT];

        init.uses.extend(uses);
        init.attributes.read.extend(attributes.read);
        init.attributes.read_on_self.extend(attributes.read_on_self);
        init.attributes.written.extend(attributes.written);
        init.attributes.computed |= attributes.computed;
        init.attributes.computed_on_self |= attributes.computed_on_self;

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
    symbol.attributes.read.extend(other.attributes.read);
    symbol
        .attributes
        .read_on_self
        .extend(other.attributes.read_on_self);
    symbol.attributes.written.extend(other.attributes.written);
    symbol.attributes.computed |= other.attributes.computed;
    symbol.attributes.computed_on_self |= other.attributes.computed_on_self;

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
    forwarded: Vec<BTreeSet<String>>,
    self_like: BTreeSet<String>,
    class_name: Option<String>,
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

    fn read_chain(&mut self, chain: Use) {
        let attributes = &mut self.collected.attributes;

        if let Some((first, rest)) = chain.attributes.split_first() {
            if self.self_like.contains(&chain.name) {
                attributes.read_on_self.insert(first.clone());
            } else {
                attributes.read.insert(first.clone());
            }

            attributes.read.extend(rest.iter().cloned());
        }

        self.collected.uses.insert(chain);
    }

    fn is_self(&self, expression: &Expr) -> bool {
        matches!(expression, Expr::Name(name) if self.self_like.contains(name.id.as_str()))
    }

    fn constructs_self(&self, value: &Expr) -> bool {
        let Expr::Call(call) = value else {
            return self.is_self(value);
        };

        match call.func.as_ref() {
            Expr::Name(callee) => {
                self.is_self(&call.func) || self.class_name.as_deref() == Some(callee.id.as_str())
            }
            Expr::Call(inner) => {
                dotted_name(&inner.func) == "type"
                    && inner
                        .arguments
                        .args
                        .first()
                        .is_some_and(|argument| self.is_self(argument))
            }
            Expr::Attribute(attribute) => {
                attribute.attr.as_str() == "__class__" && self.is_self(&attribute.value)
            }
            _ => false,
        }
    }

    fn call(&mut self, call: &ast::ExprCall) {
        let callee = dotted_name(&call.func);
        let last = callee.rsplit('.').next().unwrap_or_default();
        let writes = matches!(last, "setattr" | "delattr");
        let name_argument = match last {
            "getattr" | "hasattr" | "setattr" | "delattr" => call.arguments.args.get(1),
            "attrgetter" | "methodcaller" => call.arguments.args.first(),
            "getmembers" => {
                self.collected.attributes.computed = true;

                return;
            }
            _ => return,
        };
        let Some(name_argument) = name_argument else {
            return;
        };
        let Expr::StringLiteral(literal) = name_argument else {
            let forwards = matches!(
                name_argument,
                Expr::Name(name) if self.forwarded.last().is_some_and(|forwarded| forwarded.contains(name.id.as_str()))
            );
            let on_self = call
                .arguments
                .args
                .first()
                .is_some_and(|receiver| self.is_self(receiver))
                && matches!(last, "getattr" | "hasattr");

            if !writes && !forwards {
                if on_self {
                    self.collected.attributes.computed_on_self = true;
                } else {
                    self.collected.attributes.computed = true;
                }
            }

            return;
        };
        let names = literal.value.to_str().split('.').map(str::to_owned);

        if writes {
            self.collected.attributes.written.extend(names);
        } else {
            self.collected.attributes.read.extend(names);
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

                let forwarded = if PROXY_METHODS.contains(&definition.name.as_str()) {
                    definition
                        .parameters
                        .iter_non_variadic_params()
                        .map(|parameter| parameter.parameter.name.as_str().to_owned())
                        .collect()
                } else {
                    BTreeSet::new()
                };

                self.forwarded.push(forwarded);
                visitor::walk_stmt(self, statement);
                self.forwarded.pop();
            }
            Stmt::ClassDef(definition) => {
                self.collected
                    .bound
                    .insert(definition.name.as_str().to_owned());
                visitor::walk_stmt(self, statement);
            }
            Stmt::Assign(assign) => {
                if let [Expr::Name(target)] = assign.targets.as_slice()
                    && self.constructs_self(&assign.value)
                {
                    self.self_like.insert(target.id.as_str().to_owned());
                }

                visitor::walk_stmt(self, statement);
            }
            _ => visitor::walk_stmt(self, statement),
        }
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        match expression {
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
            Expr::Attribute(attribute) => {
                match (attribute.ctx, attribute_chain(&attribute.value)) {
                    (ExprContext::Load, Some(_)) => {
                        if let Some(chain) = attribute_chain(expression) {
                            self.read_chain(chain);
                        }
                    }
                    (context, receiver) => {
                        let attribute_name = attribute.attr.as_str().to_owned();
                        let on_self = self.is_self(&attribute.value);
                        let attributes = &mut self.collected.attributes;

                        match context {
                            ExprContext::Load if on_self => {
                                attributes.read_on_self.insert(attribute_name);
                            }
                            ExprContext::Load => {
                                attributes.read.insert(attribute_name);
                            }
                            _ => {
                                attributes.written.insert(attribute_name);
                            }
                        }

                        match receiver {
                            Some(receiver) => self.read_chain(receiver),
                            None => visitor::walk_expr(self, expression),
                        }
                    }
                }
            }
            Expr::Call(call) => {
                self.collected.calls = true;
                self.call(call);
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
    let mut cause = None;

    for statement in &module.body {
        let lazy_attributes = matches!(
            statement,
            Stmt::FunctionDef(function) if LAZY_ATTRIBUTE_FUNCTIONS.contains(&function.name.as_str())
        );
        let mut finder = FallbackFinder {
            cause: None,
            lazy_attributes,
        };

        finder.visit_stmt(statement);
        cause = cause.or(finder.cause);
    }

    cause
}

struct FallbackFinder {
    cause: Option<FallbackCause>,
    lazy_attributes: bool,
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

            if EXECUTING_FUNCTIONS.contains(&callee) {
                self.cause.get_or_insert(FallbackCause::Exec);
            } else if callee == NAMESPACE_FUNCTION && !self.lazy_attributes {
                self.cause.get_or_insert(FallbackCause::Namespace);
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

fn member_keys(statement: &Stmt) -> Option<Vec<String>> {
    match statement {
        Stmt::FunctionDef(function) => Some(vec![function.name.as_str().to_owned()]),
        Stmt::ClassDef(nested) => Some(vec![nested.name.as_str().to_owned()]),
        Stmt::Assign(assign) => target_names(&assign.targets),
        _ => None,
    }
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
