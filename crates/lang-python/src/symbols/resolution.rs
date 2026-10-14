//! Links the symbols of every analysed module: names followed through imports, re-exports, submodules and star imports to the symbols defining them, the modules each symbol loads, the definitions load-time code holds, and whole-module uses where a name cannot be followed.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use prober_core::files::{FileId, RepoPath};
use prober_core::graph::{EdgeKind, GraphBuilder};
use prober_core::symbols::{
    ModuleFallback, Symbol, SymbolChange, SymbolChangeKind, SymbolEdge, SymbolId, SymbolKind,
    SymbolTarget, WholeModuleCause,
};

use crate::conventions::{PACKAGE_INIT, split_path};
use crate::evaluator::{Evaluator, Part, Pattern};
use crate::modules::{ImportTarget, ModuleIndex};
use crate::pytest::fixtures::ModuleEdge;
use crate::summary::{ModuleSummary, SiteKind};
use crate::symbols::changes::{Changed, module_changes};
use crate::symbols::{Binding, Decoration, Imported, ModuleSymbols, Target, Use};

const MAX_DEPTH: usize = 32;

const PYTEST_NAMES: [&str; 2] = ["pytest", "collect_ignore"];

const MODULE_GETATTR: &str = "__getattr__";

const STAR_NAMES: &str = "__all__";

const NAME_HUB_THRESHOLD: usize = 8;

const ATTRIBUTES_NODE: &str = "<attributes read by name>";

const ANY_ATTRIBUTE: &str = "any attribute";

const PLAIN_STANDARD_BASES: [&str; 7] = [
    "object",
    "ABC",
    "Protocol",
    "Generic",
    "NamedTuple",
    "TypedDict",
    "BaseException",
];

const PLAIN_STANDARD_SUFFIXES: [&str; 3] = ["Error", "Exception", "Warning"];

pub(crate) struct AnalysedModule {
    pub init: SymbolId,
    pub fixtures: HashMap<String, SymbolId>,
}

pub(crate) struct Analysis<'a> {
    pub summaries: &'a HashMap<RepoPath, ModuleSummary>,
    pub base_summaries: &'a HashMap<RepoPath, ModuleSummary>,
    pub index: &'a ModuleIndex,
    pub tests: &'a BTreeSet<RepoPath>,
    pub providers: &'a HashSet<RepoPath>,
    pub module_edges: &'a HashMap<RepoPath, Vec<ModuleEdge>>,
    pub dependencies: &'a dyn Fn(&ImportTarget) -> Vec<RepoPath>,
    pub evaluator: &'a Evaluator<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Reached {
    Symbol {
        module: usize,
        symbol: usize,
    },
    Whole {
        path: RepoPath,
        cause: WholeModuleCause,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Definer {
    Symbol { module: usize, symbol: usize },
    Name(String),
    Any,
}

#[derive(Default)]
struct Hubs {
    file: Option<FileId>,
    names: HashMap<String, SymbolId>,
    any: Option<SymbolId>,
}

impl Hubs {
    fn file(&mut self, graph: &mut GraphBuilder) -> FileId {
        *self.file.get_or_insert_with(|| {
            graph.add_file(RepoPath::new(ATTRIBUTES_NODE).expect("a valid node name"))
        })
    }

    fn hub(&mut self, name: String, graph: &mut GraphBuilder) -> SymbolId {
        let file = self.file(graph);

        graph.add_symbol(Symbol {
            file,
            name,
            kind: SymbolKind::Member,
            lines: Vec::new(),
            hash: 0,
        })
    }

    fn symbol(
        &mut self,
        definer: &Definer,
        members: &Members,
        ids: &[Vec<SymbolId>],
        graph: &mut GraphBuilder,
    ) -> SymbolId {
        let (key, definers): (Option<String>, Vec<(usize, usize)>) = match definer {
            Definer::Symbol { module, symbol } => return ids[*module][*symbol],
            Definer::Name(key) => {
                if let Some(&hub) = self.names.get(key) {
                    return hub;
                }

                let definers = members
                    .defining(key)
                    .iter()
                    .chain(members.writing(key))
                    .copied()
                    .collect();

                (Some(key.clone()), definers)
            }
            Definer::Any => {
                if let Some(hub) = self.any {
                    return hub;
                }

                (None, members.all.clone())
            }
        };
        let hub = match &key {
            Some(key) => self.hub(format!(".{key}"), graph),
            None => self.hub(ANY_ATTRIBUTE.to_owned(), graph),
        };

        for (module, symbol) in definers {
            graph.add_symbol_edge(SymbolEdge {
                dependent: hub,
                dependency: SymbolTarget::Symbol(ids[module][symbol]),
                kind: EdgeKind::Import,
            });
        }

        match key {
            Some(key) => {
                self.names.insert(key, hub);
            }
            None => self.any = Some(hub),
        }

        hub
    }
}

#[derive(Clone, Copy)]
struct Module<'a> {
    path: &'a RepoPath,
    symbols: &'a ModuleSymbols,
    summary: &'a ModuleSummary,
}

struct Resolver<'a> {
    modules: Vec<Module<'a>>,
    module_indices: HashMap<&'a RepoPath, usize>,
    index: &'a ModuleIndex,
    attributes: HashMap<(usize, Vec<String>), Vec<Reached>>,
    in_progress: HashSet<(usize, Vec<String>)>,
    registering: HashMap<(usize, usize), bool>,
    class_level: HashMap<(usize, usize), bool>,
    hierarchies: HashMap<(usize, usize), Rc<HashSet<(usize, usize)>>>,
    self_reads: HashMap<(usize, String), Rc<Vec<Definer>>>,
    reads: HashMap<String, Rc<Vec<Definer>>>,
    members: Members,
    computed_names: HashMap<(usize, usize), Vec<Pattern>>,
    writers: HashMap<usize, Vec<usize>>,
}

#[derive(Default)]
struct Members {
    definers: HashMap<String, Vec<(usize, usize)>>,
    writers: HashMap<String, Vec<(usize, usize)>>,
    all: Vec<(usize, usize)>,
    of_class: HashMap<(usize, usize), Vec<usize>>,
    parents: HashMap<(usize, usize), Vec<(usize, usize)>>,
    children: HashMap<(usize, usize), Vec<(usize, usize)>>,
    keys: Vec<String>,
}

impl Members {
    fn index(modules: &[Module], tests: &BTreeSet<RepoPath>) -> Self {
        let mut members = Members::default();

        for (module, analysed) in modules.iter().enumerate() {
            let in_tests = tests.contains(analysed.path);

            for (symbol, local) in analysed.symbols.symbols.iter().enumerate() {
                let writes_at_load =
                    matches!(local.kind, SymbolKind::ModuleInit | SymbolKind::Class) && !in_tests;

                for written in local.attributes.written.iter().filter(|_| writes_at_load) {
                    members
                        .writers
                        .entry(written.clone())
                        .or_default()
                        .push((module, symbol));
                }

                let Some(member) = &local.member else {
                    continue;
                };

                members.all.push((module, symbol));
                members
                    .of_class
                    .entry((module, member.class))
                    .or_default()
                    .push(symbol);

                for key in &member.keys {
                    members
                        .definers
                        .entry(key.clone())
                        .or_default()
                        .push((module, symbol));
                }
            }
        }

        members.keys = members
            .definers
            .keys()
            .chain(members.writers.keys())
            .cloned()
            .collect();
        members.keys.sort_unstable();
        members.keys.dedup();

        for definers in members
            .definers
            .values_mut()
            .chain(members.writers.values_mut())
        {
            definers.sort_unstable();
            definers.dedup();
        }

        members
    }

    fn defining(&self, key: &str) -> &[(usize, usize)] {
        self.definers.get(key).map_or(&[], Vec::as_slice)
    }

    fn writing(&self, key: &str) -> &[(usize, usize)] {
        self.writers.get(key).map_or(&[], Vec::as_slice)
    }
}

impl Analysis<'_> {
    pub fn add_to(&self, graph: &mut GraphBuilder) -> HashMap<RepoPath, AnalysedModule> {
        let mut paths: Vec<&RepoPath> = self.summaries.keys().collect();

        paths.sort();

        let mut modules = Vec::new();

        for path in paths {
            let summary = &self.summaries[path];

            match &summary.symbols {
                Some(Ok(symbols)) => modules.push(Module {
                    path,
                    symbols,
                    summary,
                }),
                Some(Err(cause)) => {
                    let file = graph.add_file(path.clone());

                    graph.add_fallback(ModuleFallback {
                        file,
                        cause: *cause,
                    });
                }
                None => {}
            }
        }

        let files: Vec<FileId> = modules
            .iter()
            .map(|module| graph.add_file(module.path.clone()))
            .collect();
        let ids: Vec<Vec<SymbolId>> = modules
            .iter()
            .zip(&files)
            .map(|(module, &file)| {
                module
                    .symbols
                    .symbols
                    .iter()
                    .map(|local| {
                        graph.add_symbol(Symbol {
                            file,
                            name: local.name.clone(),
                            kind: local.kind,
                            lines: local.lines.clone(),
                            hash: local.hash,
                        })
                    })
                    .collect()
            })
            .collect();
        let mut resolver = Resolver {
            module_indices: modules
                .iter()
                .enumerate()
                .map(|(position, module)| (module.path, position))
                .collect(),
            modules,
            index: self.index,
            attributes: HashMap::new(),
            in_progress: HashSet::new(),
            registering: HashMap::new(),
            class_level: HashMap::new(),
            hierarchies: HashMap::new(),
            self_reads: HashMap::new(),
            reads: HashMap::new(),
            members: Members::default(),
            computed_names: HashMap::new(),
            writers: HashMap::new(),
        };

        let timer = std::time::Instant::now();
        resolver.members = Members::index(&resolver.modules, self.tests);
        eprintln!("TIMING members {:?}", timer.elapsed());
        resolver.index_classes();
        eprintln!("TIMING classes {:?}", timer.elapsed());
        resolver.index_namespace_writers();
        resolver.computed_names = self.computed_names(&resolver.modules);
        eprintln!("TIMING computed {:?}", timer.elapsed());

        let mut hubs = Hubs::default();
        let mut uses_time = std::time::Duration::ZERO;
        let mut definers_time = std::time::Duration::ZERO;

        for module in 0..resolver.modules.len() {
            self.add_edges(
                &mut resolver,
                &mut hubs,
                module,
                &ids,
                &files,
                graph,
                &mut uses_time,
                &mut definers_time,
            );
        }
        eprintln!(
            "TIMING edges {:?} uses {:?} definers {:?}",
            timer.elapsed(),
            uses_time,
            definers_time
        );

        for module in 0..resolver.modules.len() {
            self.add_changes(&mut resolver, module, &ids, &files, graph);
        }

        resolver
            .modules
            .iter()
            .enumerate()
            .map(|(module, analysed)| {
                let fixtures = analysed
                    .symbols
                    .symbols
                    .iter()
                    .enumerate()
                    .filter(|(_, local)| local.kind == SymbolKind::Fixture)
                    .map(|(symbol, local)| (local.name.clone(), ids[module][symbol]))
                    .collect();

                (
                    analysed.path.clone(),
                    AnalysedModule {
                        init: ids[module][ModuleSymbols::INIT],
                        fixtures,
                    },
                )
            })
            .collect()
    }

    fn add_edges(
        &self,
        resolver: &mut Resolver,
        hubs: &mut Hubs,
        module: usize,
        ids: &[Vec<SymbolId>],
        files: &[FileId],
        graph: &mut GraphBuilder,
        uses_time: &mut std::time::Duration,
        definers_time: &mut std::time::Duration,
    ) {
        let Module {
            path,
            symbols,
            summary,
        } = resolver.modules[module];
        let mut edges = Edges::default();
        let init = ids[module][ModuleSymbols::INIT];

        for (symbol, local) in symbols.symbols.iter().enumerate() {
            let dependent = ids[module][symbol];

            let mut passes_to_reflection = false;
            let uses_timer = std::time::Instant::now();

            for symbol_use in &local.uses {
                for reached in resolver.resolve_use(module, symbol, symbol_use) {
                    if let Reached::Symbol {
                        module: used_module,
                        symbol: used,
                    } = reached
                        && resolver.reflects_on_argument(used_module, used)
                        && !resolver
                            .decorators_of(module, symbol)
                            .contains(&(used_module, used))
                    {
                        passes_to_reflection = true;
                    }

                    edges.add(dependent, reached, ids, files, graph);
                }
            }

            if passes_to_reflection {
                let any = hubs.symbol(&Definer::Any, &resolver.members, ids, graph);

                edges.add_symbol(dependent, any);
            }

            if local.namespace.reads {
                let whole = Reached::Whole {
                    path: path.clone(),
                    cause: WholeModuleCause::EscapingNamespace,
                };

                edges.add(dependent, whole, ids, files, graph);
            }

            for writer in resolver.namespace_writers(module) {
                if writer != (Reached::Symbol { module, symbol }) {
                    edges.add(dependent, writer, ids, files, graph);
                }
            }

            *uses_time += uses_timer.elapsed();
            let definers_timer = std::time::Instant::now();
            let found = resolver.attribute_definers(module, symbol);
            *definers_time += definers_timer.elapsed();
            for definer in found.iter().flat_map(|definers| definers.iter()) {
                let dependency = hubs.symbol(definer, &resolver.members, ids, graph);

                if dependency != dependent {
                    edges.add_symbol(dependent, dependency);
                }
            }

            if let Some(member) = &local.member {
                let class = Reached::Symbol {
                    module,
                    symbol: member.class,
                };

                edges.add(dependent, class, ids, files, graph);
            }

            if local.class.is_some() {
                for reached in resolver.members_of_class(module, symbol) {
                    edges.add(dependent, reached, ids, files, graph);
                }
            }

            if symbol != ModuleSymbols::INIT && self.held(resolver, module, symbol) {
                edges.add(init, Reached::Symbol { module, symbol }, ids, files, graph);
            }
        }

        for import in summary.imports.iter().filter(|import| !import.type_only) {
            let loader = ids[module][symbols.symbol_at(import.line)];

            for dependency in (self.dependencies)(&import.target)
                .into_iter()
                .filter(|dependency| dependency != path)
            {
                let reached = match resolver.module_indices.get(&dependency) {
                    Some(&loaded) => Reached::Symbol {
                        module: loaded,
                        symbol: ModuleSymbols::INIT,
                    },
                    None => Reached::Whole {
                        path: dependency,
                        cause: WholeModuleCause::NotAnalysed,
                    },
                };

                edges.add(loader, reached, ids, files, graph);
            }
        }

        for star in &symbols.star_imports {
            let Target::Repo(source) = star else {
                continue;
            };
            let Some(&source) = resolver.module_indices.get(source) else {
                continue;
            };
            let star_names = resolver.modules[source].symbols.names.get(STAR_NAMES);

            for binding in star_names.into_iter().flatten() {
                if let Binding::Local(symbol) = binding {
                    let reached = Reached::Symbol {
                        module: source,
                        symbol: *symbol,
                    };

                    edges.add(init, reached, ids, files, graph);
                }
            }
        }

        for module_edge in self.module_edges.get(path).into_iter().flatten() {
            let file = graph.add_file(module_edge.dependency.clone());

            if !edges.covered.contains(&file) {
                edges.edges.push(SymbolEdge {
                    dependent: init,
                    dependency: SymbolTarget::Module {
                        file,
                        cause: WholeModuleCause::Imported,
                    },
                    kind: module_edge.kind,
                });
            }
        }

        edges.edges.sort_unstable();
        edges.edges.dedup();

        for edge in edges.edges {
            graph.add_symbol_edge(edge);
        }
    }

    fn computed_names(&self, modules: &[Module]) -> HashMap<(usize, usize), Vec<Pattern>> {
        let evaluator = self.evaluator;
        let mut computed_names: HashMap<(usize, usize), Vec<Pattern>> = HashMap::new();

        for (module, analysed) in modules.iter().enumerate() {
            for site in analysed
                .summary
                .sites
                .iter()
                .filter(|site| site.kind == SiteKind::Attribute)
            {
                let symbol = analysed.symbols.symbol_at(site.line);

                let attributes = &analysed.symbols.symbols[symbol].attributes;

                if !attributes.computed
                    && !attributes.computed_on_self
                    && !attributes.computed_on_argument
                {
                    continue;
                }

                let patterns = evaluator
                    .site_values(analysed.path, site)
                    .into_iter()
                    .flat_map(|site_value| site_value.value.patterns());

                computed_names
                    .entry((module, symbol))
                    .or_default()
                    .extend(patterns);
            }
        }

        computed_names
    }

    fn held(&self, resolver: &mut Resolver, module: usize, symbol: usize) -> bool {
        let Module { path, symbols, .. } = resolver.modules[module];
        let local = &symbols.symbols[symbol];

        if self.tests.contains(path) {
            return true;
        }

        let is_provider = self.providers.contains(path);
        let held = match local.decoration {
            Decoration::Registering => resolver.registers(module, symbol),
            Decoration::Fixture => !is_provider,
            Decoration::Plain => {
                local.calls_at_load
                    || is_provider
                        && PYTEST_NAMES
                            .iter()
                            .any(|prefix| local.name.starts_with(prefix))
            }
        };

        held || local.class.as_ref().is_some_and(|header| {
            header.keywords
                || header
                    .bases
                    .iter()
                    .any(|base| resolver.base_registers(module, base.as_ref(), 0))
        })
    }

    fn add_changes(
        &self,
        resolver: &mut Resolver,
        module: usize,
        ids: &[Vec<SymbolId>],
        files: &[FileId],
        graph: &mut GraphBuilder,
    ) {
        let file = files[module];
        let Module {
            path,
            symbols: head,
            summary,
        } = resolver.modules[module];
        let Some(Some(Ok(base))) = self
            .base_summaries
            .get(path)
            .map(|base_summary| base_summary.symbols.as_ref())
        else {
            return;
        };
        let base_summary = &self.base_summaries[path];
        let base_loads = self.init_loads(base, base_summary);
        let head_loads = self.init_loads(head, summary);
        let mut symbol_changes = Vec::new();

        for changed in module_changes(base, &base_loads, head, &head_loads) {
            let (name, kind, reaches) = match changed {
                Changed::Symbol { index, kind } => (
                    head.symbols[index].name.clone(),
                    kind,
                    vec![SymbolTarget::Symbol(ids[module][index])],
                ),
                Changed::Removed { name, keys, class } => {
                    let mut reaches: Vec<SymbolTarget> = keys
                        .iter()
                        .flat_map(|key| resolver.members.defining(key).to_vec())
                        .map(|(module, symbol)| SymbolTarget::Symbol(ids[module][symbol]))
                        .collect();

                    if let Some(class) = class
                        && let Some(&Binding::Local(symbol)) =
                            head.names.get(&class).and_then(|bindings| bindings.first())
                    {
                        reaches.push(SymbolTarget::Symbol(ids[module][symbol]));
                    }

                    (name, SymbolChangeKind::Removed, reaches)
                }
                Changed::Binding(name) => {
                    let reaches = resolver
                        .resolve_attributes(module, std::slice::from_ref(&name), 0)
                        .into_iter()
                        .map(|reached| target(reached, ids, files, graph).0)
                        .collect();

                    (name, SymbolChangeKind::Export, reaches)
                }
            };

            symbol_changes.push(SymbolChange {
                file,
                name,
                kind,
                reaches,
            });
        }

        graph.add_module_changes(file, symbol_changes);
    }

    fn init_loads(&self, symbols: &ModuleSymbols, summary: &ModuleSummary) -> BTreeSet<String> {
        summary
            .imports
            .iter()
            .filter(|import| {
                !import.type_only && symbols.symbol_at(import.line) == ModuleSymbols::INIT
            })
            .flat_map(|import| (self.dependencies)(&import.target))
            .map(|dependency| dependency.to_string())
            .collect()
    }
}

#[derive(Default)]
struct Edges {
    edges: Vec<SymbolEdge>,
    covered: HashSet<FileId>,
}

impl Edges {
    fn add_symbol(&mut self, dependent: SymbolId, dependency: SymbolId) {
        self.edges.push(SymbolEdge {
            dependent,
            dependency: SymbolTarget::Symbol(dependency),
            kind: EdgeKind::Import,
        });
    }

    fn add(
        &mut self,
        dependent: SymbolId,
        reached: Reached,
        ids: &[Vec<SymbolId>],
        files: &[FileId],
        graph: &mut GraphBuilder,
    ) {
        let (dependency, file) = target(reached, ids, files, graph);

        self.covered.insert(file);
        self.edges.push(SymbolEdge {
            dependent,
            dependency,
            kind: EdgeKind::Import,
        });
    }
}

fn target(
    reached: Reached,
    ids: &[Vec<SymbolId>],
    files: &[FileId],
    graph: &mut GraphBuilder,
) -> (SymbolTarget, FileId) {
    match reached {
        Reached::Symbol { module, symbol } => {
            (SymbolTarget::Symbol(ids[module][symbol]), files[module])
        }
        Reached::Whole { path, cause } => {
            let file = graph.add_file(path);

            (SymbolTarget::Module { file, cause }, file)
        }
    }
}

impl Resolver<'_> {
    fn resolve_use(&mut self, module: usize, symbol: usize, symbol_use: &Use) -> Vec<Reached> {
        let symbols = self.modules[module].symbols;

        if let Some(imported) = symbols.symbols[symbol].imports.get(&symbol_use.name) {
            return imported
                .iter()
                .flat_map(|imported| self.resolve_imported(imported, &symbol_use.attributes, 0))
                .collect();
        }

        if let Some(bindings) = symbols.names.get(&symbol_use.name) {
            return bindings
                .iter()
                .flat_map(|binding| {
                    self.resolve_binding(module, binding, &symbol_use.attributes, 0)
                })
                .collect();
        }

        let mut chain = vec![symbol_use.name.clone()];

        chain.extend(symbol_use.attributes.iter().cloned());

        self.resolve_starred(module, &chain, 0).unwrap_or_default()
    }

    fn resolve_binding(
        &mut self,
        module: usize,
        binding: &Binding,
        attributes: &[String],
        depth: usize,
    ) -> Vec<Reached> {
        match binding {
            Binding::Local(symbol) => vec![Reached::Symbol {
                module,
                symbol: self.member_named(module, *symbol, attributes.first()),
            }],
            Binding::Import(imported) => self.resolve_imported(imported, attributes, depth),
        }
    }

    fn member_named(&self, module: usize, class: usize, attribute: Option<&String>) -> usize {
        let Some(attribute) = attribute else {
            return class;
        };

        self.members
            .of_class
            .get(&(module, class))
            .into_iter()
            .flatten()
            .copied()
            .find(|&member| {
                self.modules[module].symbols.symbols[member]
                    .member
                    .as_ref()
                    .is_some_and(|member| member.keys.contains(attribute))
            })
            .unwrap_or(class)
    }

    fn attribute_definers(&mut self, module: usize, symbol: usize) -> Vec<Rc<Vec<Definer>>> {
        let attributes = &self.modules[module].symbols.symbols[symbol].attributes;
        let mut definers: Vec<Rc<Vec<Definer>>> = Vec::new();

        if attributes.computed || attributes.computed_on_self || attributes.computed_on_argument {
            match self.computed_names.get(&(module, symbol)) {
                Some(patterns) if patterns.iter().all(has_text) => {
                    for key in self.matching_keys(patterns) {
                        definers.push(self.reading(&key));
                    }
                }
                _ if attributes.computed => definers.push(Rc::new(vec![Definer::Any])),
                _ if attributes.computed_on_self => {
                    definers.push(Rc::new(
                        self.hierarchy_members(module, symbol)
                            .into_iter()
                            .map(|(module, symbol)| Definer::Symbol { module, symbol })
                            .collect(),
                    ));
                }
                _ => {}
            }
        }

        let symbols = self.modules[module].symbols;
        let attributes = &symbols.symbols[symbol].attributes;

        for read in &attributes.read {
            definers.push(self.reading(read));
        }

        if !attributes.read_on_self.is_empty() {
            let hierarchy = self.hierarchy(module, symbol);

            for read in &attributes.read_on_self {
                let key = (Rc::as_ptr(&hierarchy) as usize, read.clone());
                let found = match self.self_reads.get(&key) {
                    Some(found) => Rc::clone(found),
                    None => {
                        let found = Rc::new(self.reading_in(&hierarchy, read));

                        self.self_reads.insert(key, Rc::clone(&found));

                        found
                    }
                };

                definers.push(found);
            }
        }

        definers
    }

    fn reading_in(&self, hierarchy: &HashSet<(usize, usize)>, key: &str) -> Vec<Definer> {
        let defining: Vec<(usize, usize)> = self
            .members
            .defining(key)
            .iter()
            .filter(|&&(module, member)| {
                self.modules[module].symbols.symbols[member]
                    .member
                    .as_ref()
                    .is_some_and(|member| hierarchy.contains(&(module, member.class)))
            })
            .copied()
            .collect();
        let writing = self.members.writing(key);

        if defining.len() + writing.len() > NAME_HUB_THRESHOLD {
            return vec![Definer::Name(key.to_owned())];
        }

        defining
            .iter()
            .chain(writing)
            .map(|&(module, symbol)| Definer::Symbol { module, symbol })
            .collect()
    }

    fn reading(&mut self, key: &str) -> Rc<Vec<Definer>> {
        if let Some(found) = self.reads.get(key) {
            return Rc::clone(found);
        }

        let defining = self.members.defining(key);
        let writing = self.members.writing(key);
        let found = if defining.len() + writing.len() > NAME_HUB_THRESHOLD {
            vec![Definer::Name(key.to_owned())]
        } else {
            defining
                .iter()
                .chain(writing)
                .map(|&(module, symbol)| Definer::Symbol { module, symbol })
                .collect()
        };
        let found = Rc::new(found);

        self.reads.insert(key.to_owned(), Rc::clone(&found));

        found
    }

    fn matching_keys(&self, patterns: &[Pattern]) -> Vec<String> {
        let mut keys = Vec::new();
        let mut scanned = Vec::new();

        for pattern in patterns {
            match pattern.0.as_slice() {
                [Part::Text(text)] => keys.push(text.clone()),
                _ => scanned.push(NamePattern::of(pattern)),
            }
        }

        for pattern in &scanned {
            let prefix = pattern.texts.first().filter(|_| !pattern.open_start);
            let start = prefix.map_or(0, |prefix| {
                self.members
                    .keys
                    .partition_point(|key| key.as_str() < *prefix)
            });

            for key in &self.members.keys[start..] {
                if prefix.is_some_and(|prefix| !key.starts_with(prefix)) {
                    break;
                }

                if pattern.matches(key) {
                    keys.push(key.clone());
                }
            }
        }

        keys.sort_unstable();
        keys.dedup();
        keys
    }

    fn index_classes(&mut self) {
        let mut parents: HashMap<(usize, usize), Vec<(usize, usize)>> = HashMap::new();

        for module in 0..self.modules.len() {
            let symbols = self.modules[module].symbols;

            for (class, local) in symbols.symbols.iter().enumerate() {
                let Some(header) = &local.class else {
                    continue;
                };

                for base in header.bases.iter().flatten() {
                    let Some(bindings) = symbols.names.get(&base.name) else {
                        continue;
                    };

                    for binding in bindings {
                        for reached in self.resolve_binding(module, binding, &base.attributes, 0) {
                            if let Reached::Symbol {
                                module: base_module,
                                symbol,
                            } = reached
                                && self.modules[base_module].symbols.symbols[symbol]
                                    .class
                                    .is_some()
                            {
                                parents
                                    .entry((module, class))
                                    .or_default()
                                    .push((base_module, symbol));
                            }
                        }
                    }
                }
            }
        }

        for (&class, class_parents) in &parents {
            for &parent in class_parents {
                self.members.children.entry(parent).or_default().push(class);
            }
        }

        self.members.parents = parents;
    }

    fn index_namespace_writers(&mut self) {
        for module in 0..self.modules.len() {
            let symbols = self.modules[module].symbols;
            let mut writers = Vec::new();

            for (symbol, local) in symbols.symbols.iter().enumerate() {
                if local.namespace.writes {
                    writers.push(symbol);
                }

                if local.namespace.writes_through_class
                    && let Some(member) = &local.member
                {
                    writers.push(member.class);
                    writers.extend(
                        self.members
                            .of_class
                            .get(&(module, member.class))
                            .into_iter()
                            .flatten()
                            .copied(),
                    );
                }
            }

            writers.sort_unstable();
            writers.dedup();

            if !writers.is_empty() {
                self.writers.insert(module, writers);
            }
        }
    }

    fn namespace_writers(&self, module: usize) -> Vec<Reached> {
        self.writers
            .get(&module)
            .into_iter()
            .flatten()
            .map(|&symbol| Reached::Symbol { module, symbol })
            .collect()
    }

    fn reflects_on_argument(&self, module: usize, symbol: usize) -> bool {
        let resolved = self
            .computed_names
            .get(&(module, symbol))
            .is_some_and(|patterns| patterns.iter().all(has_text));

        self.modules[module].symbols.symbols[symbol]
            .attributes
            .computed_on_argument
            && !resolved
    }

    fn decorators_of(&mut self, module: usize, symbol: usize) -> Vec<(usize, usize)> {
        let symbols = self.modules[module].symbols;
        let mut decorators = Vec::new();

        for decorator in symbols.symbols[symbol].decorators.iter().flatten() {
            for binding in symbols.names.get(&decorator.name).into_iter().flatten() {
                for reached in self.resolve_binding(module, binding, &decorator.attributes, 0) {
                    if let Reached::Symbol { module, symbol } = reached {
                        decorators.push((module, symbol));
                    }
                }
            }
        }

        decorators
    }

    fn hierarchy(&mut self, module: usize, symbol: usize) -> Rc<HashSet<(usize, usize)>> {
        let Some(member) = &self.modules[module].symbols.symbols[symbol].member else {
            return Rc::default();
        };
        let root = (module, member.class);

        if let Some(classes) = self.hierarchies.get(&root) {
            return Rc::clone(classes);
        }

        let mut pending = vec![root];
        let mut classes: HashSet<(usize, usize)> = HashSet::from([root]);

        while let Some(class) = pending.pop() {
            let related = self
                .members
                .parents
                .get(&class)
                .into_iter()
                .chain(self.members.children.get(&class))
                .flatten();

            for &next in related {
                if classes.insert(next) {
                    pending.push(next);
                }
            }
        }

        let classes = Rc::new(classes);

        for &class in classes.iter() {
            self.hierarchies.insert(class, Rc::clone(&classes));
        }

        classes
    }

    fn hierarchy_members(&mut self, module: usize, symbol: usize) -> Vec<(usize, usize)> {
        let classes = self.hierarchy(module, symbol);

        classes
            .iter()
            .flat_map(|&(module, class)| {
                self.members
                    .of_class
                    .get(&(module, class))
                    .into_iter()
                    .flatten()
                    .map(move |&member| (module, member))
            })
            .collect()
    }

    fn members_of_class(&mut self, module: usize, class: usize) -> Vec<Reached> {
        let members = self
            .members
            .of_class
            .get(&(module, class))
            .cloned()
            .unwrap_or_default();
        let whole = self.is_class_level(module, class, 0);
        let symbols = self.modules[module].symbols;

        members
            .into_iter()
            .filter(|&member| {
                whole
                    || symbols.symbols[member]
                        .member
                        .as_ref()
                        .is_some_and(|member| member.keys.iter().any(|key| is_special(key)))
            })
            .map(|symbol| Reached::Symbol { module, symbol })
            .collect()
    }

    fn registers(&mut self, module: usize, symbol: usize) -> bool {
        let local = &self.modules[module].symbols.symbols[symbol];

        local.decoration == Decoration::Registering
            && local
                .decorators
                .iter()
                .any(|decorator| self.decorator_registers(module, decorator.as_ref()))
    }

    fn decorator_registers(&mut self, module: usize, decorator: Option<&Use>) -> bool {
        let Some(decorator) = decorator else {
            return true;
        };
        let symbols = self.modules[module].symbols;
        let Some(bindings) = symbols.names.get(&decorator.name) else {
            return true;
        };

        for binding in bindings {
            if let Binding::Import(imported) = binding
                && !matches!(imported_target(imported), Target::Repo(_))
            {
                return true;
            }

            for reached in self.resolve_binding(module, binding, &decorator.attributes, 0) {
                let Reached::Symbol { module, symbol } = reached else {
                    return true;
                };
                let target = &self.modules[module].symbols.symbols[symbol];
                let wraps = matches!(target.kind, SymbolKind::Function | SymbolKind::Member)
                    && !target.stores_argument;

                if !wraps {
                    return true;
                }
            }
        }

        false
    }

    fn is_class_level(&mut self, module: usize, class: usize, depth: usize) -> bool {
        let key = (module, class);

        if let Some(&class_level) = self.class_level.get(&key) {
            return class_level;
        }

        self.class_level.insert(key, true);

        let local = &self.modules[module].symbols.symbols[class];
        let class_level = match &local.class {
            None => true,
            Some(header) => {
                header.keywords
                    || self.registers(module, class)
                    || self
                        .decorators_of(module, class)
                        .into_iter()
                        .any(|(module, symbol)| self.reflects_on_argument(module, symbol))
                    || header
                        .bases
                        .iter()
                        .any(|base| self.base_is_class_level(module, base.as_ref(), depth))
            }
        };

        self.class_level.insert(key, class_level);

        class_level
    }

    fn base_is_class_level(&mut self, module: usize, base: Option<&Use>, depth: usize) -> bool {
        let Some(base) = base else {
            return true;
        };

        if depth > MAX_DEPTH {
            return true;
        }

        let symbols = self.modules[module].symbols;
        let Some(bindings) = symbols.names.get(&base.name) else {
            let mut chain = vec![base.name.clone()];

            chain.extend(base.attributes.iter().cloned());

            return match self.resolve_starred(module, &chain, 0) {
                Some(reached) => self.reached_is_class_level(reached, depth),
                None => false,
            };
        };
        let mut reached = Vec::new();

        for binding in bindings {
            if let Binding::Import(imported) = binding {
                match imported_target(imported) {
                    Target::Standard(_) => {
                        let name = match (base.attributes.last(), imported) {
                            (Some(last), _) => last.as_str(),
                            (None, Imported::Name { name, .. }) => name.as_str(),
                            (None, Imported::Module(_)) => base.name.as_str(),
                        };

                        if !is_plain_standard_base(name) {
                            return true;
                        }

                        continue;
                    }
                    Target::Repo(_) => {}
                    _ => return true,
                }
            }

            reached.extend(self.resolve_binding(module, binding, &base.attributes, 0));
        }

        self.reached_is_class_level(reached, depth)
    }

    fn reached_is_class_level(&mut self, reached: Vec<Reached>, depth: usize) -> bool {
        reached.into_iter().any(|reached| match reached {
            Reached::Symbol { module, symbol } => {
                let local = &self.modules[module].symbols.symbols[symbol];

                match &local.class {
                    Some(header) => {
                        header.init_subclass || self.is_class_level(module, symbol, depth + 1)
                    }
                    None => true,
                }
            }
            Reached::Whole { .. } => true,
        })
    }

    fn resolve_imported(
        &mut self,
        imported: &Imported,
        attributes: &[String],
        depth: usize,
    ) -> Vec<Reached> {
        match imported {
            Imported::Module(target) => self.resolve_target(target, attributes, depth),
            Imported::Name { module, name } => {
                let mut chain = vec![name.clone()];

                chain.extend(attributes.iter().cloned());

                self.resolve_target(module, &chain, depth)
            }
        }
    }

    fn resolve_target(
        &mut self,
        target: &Target,
        attributes: &[String],
        depth: usize,
    ) -> Vec<Reached> {
        let Target::Repo(path) = target else {
            return Vec::new();
        };

        match self.module_indices.get(path) {
            Some(&module) => self.resolve_attributes(module, attributes, depth + 1),
            None => vec![Reached::Whole {
                path: path.clone(),
                cause: WholeModuleCause::NotAnalysed,
            }],
        }
    }

    fn resolve_attributes(
        &mut self,
        module: usize,
        attributes: &[String],
        depth: usize,
    ) -> Vec<Reached> {
        let path = self.modules[module].path;

        if attributes.is_empty() {
            return vec![Reached::Whole {
                path: path.clone(),
                cause: WholeModuleCause::EscapingNamespace,
            }];
        }

        let key = (module, attributes.to_vec());

        if let Some(reached) = self.attributes.get(&key) {
            return reached.clone();
        }

        if depth > MAX_DEPTH || !self.in_progress.insert(key.clone()) {
            return Vec::new();
        }

        let reached = self.find_attribute(module, attributes, depth);

        self.in_progress.remove(&key);
        self.attributes.insert(key, reached.clone());

        reached
    }

    fn find_attribute(
        &mut self,
        module: usize,
        attributes: &[String],
        depth: usize,
    ) -> Vec<Reached> {
        let mut reached = self.find_static_attribute(module, attributes, depth);

        reached.extend(self.namespace_writers(module));

        reached
    }

    fn find_static_attribute(
        &mut self,
        module: usize,
        attributes: &[String],
        depth: usize,
    ) -> Vec<Reached> {
        let symbols = self.modules[module].symbols;
        let path = self.modules[module].path;
        let (name, rest) = (&attributes[0], &attributes[1..]);

        if let Some(bindings) = symbols.names.get(name) {
            return bindings
                .iter()
                .flat_map(|binding| self.resolve_binding(module, binding, rest, depth))
                .collect();
        }

        if let Some(submodule) = self.submodule(path, name) {
            return self.resolve_target(&Target::Repo(submodule), rest, depth);
        }

        if let Some(reached) = self.resolve_starred(module, attributes, depth) {
            return reached;
        }

        if let Some(Binding::Local(getattr)) = symbols
            .names
            .get(MODULE_GETATTR)
            .and_then(|bindings| bindings.first())
        {
            return vec![Reached::Symbol {
                module,
                symbol: *getattr,
            }];
        }

        vec![Reached::Whole {
            path: path.clone(),
            cause: WholeModuleCause::MissingExport,
        }]
    }

    fn resolve_starred(
        &mut self,
        module: usize,
        attributes: &[String],
        depth: usize,
    ) -> Option<Vec<Reached>> {
        let symbols = self.modules[module].symbols;
        let mut found = None;

        for star in &symbols.star_imports {
            let Target::Repo(source) = star else {
                continue;
            };
            let reached = match self.module_indices.get(source) {
                Some(&source) if self.defines(source, &attributes[0], &mut HashSet::new()) => {
                    self.resolve_attributes(source, attributes, depth + 1)
                }
                Some(_) => continue,
                None => vec![Reached::Whole {
                    path: source.clone(),
                    cause: WholeModuleCause::NotAnalysed,
                }],
            };

            found.get_or_insert_with(Vec::new).extend(reached);
        }

        found
    }

    fn defines(&self, module: usize, name: &str, visited: &mut HashSet<usize>) -> bool {
        if !visited.insert(module) {
            return false;
        }

        let Module { path, symbols, .. } = self.modules[module];

        symbols.names.contains_key(name)
            || self.submodule(path, name).is_some()
            || symbols.star_imports.iter().any(|star| match star {
                Target::Repo(source) => match self.module_indices.get(source) {
                    Some(&source) => self.defines(source, name, visited),
                    None => true,
                },
                _ => false,
            })
    }

    fn submodule(&self, path: &RepoPath, name: &str) -> Option<RepoPath> {
        let (directory, file_name) = split_path(path.as_str());

        if file_name != PACKAGE_INIT {
            return None;
        }

        self.index
            .resolve_in_directory(directory, name)
            .map(|resolved| resolved.module)
    }

    fn base_registers(&mut self, module: usize, base: Option<&Use>, depth: usize) -> bool {
        let Some(base) = base else {
            return true;
        };

        if depth > MAX_DEPTH {
            return true;
        }

        let symbols = self.modules[module].symbols;
        let reached = match symbols.names.get(&base.name) {
            Some(bindings) => {
                let mut reached = Vec::new();

                for binding in bindings {
                    if let Binding::Import(imported) = binding {
                        match imported_target(imported) {
                            Target::Standard(_) => continue,
                            Target::Repo(_) => {}
                            _ => return true,
                        }
                    }

                    reached.extend(self.resolve_binding(module, binding, &base.attributes, 0));
                }

                reached
            }
            None => {
                let mut chain = vec![base.name.clone()];

                chain.extend(base.attributes.iter().cloned());

                match self.resolve_starred(module, &chain, 0) {
                    Some(reached) => reached,
                    None => return false,
                }
            }
        };

        reached.into_iter().any(|reached| match reached {
            Reached::Symbol { module, symbol } => self.class_registers(module, symbol, depth + 1),
            Reached::Whole { .. } => true,
        })
    }

    fn class_registers(&mut self, module: usize, symbol: usize, depth: usize) -> bool {
        let key = (module, symbol);

        if let Some(&registers) = self.registering.get(&key) {
            return registers;
        }

        self.registering.insert(key, false);

        let local = &self.modules[module].symbols.symbols[symbol];
        let registers = match &local.class {
            None => true,
            Some(header) => {
                header.keywords
                    || header.init_subclass
                    || header
                        .bases
                        .iter()
                        .any(|base| self.base_registers(module, base.as_ref(), depth))
            }
        };

        self.registering.insert(key, registers);

        registers
    }
}

fn has_text(pattern: &Pattern) -> bool {
    pattern
        .0
        .iter()
        .any(|part| matches!(part, Part::Text(text) if !text.is_empty()))
}

struct NamePattern<'p> {
    texts: Vec<&'p str>,
    open_start: bool,
    open_end: bool,
}

impl<'p> NamePattern<'p> {
    fn of(pattern: &'p Pattern) -> Self {
        Self {
            texts: pattern
                .0
                .iter()
                .filter_map(|part| match part {
                    Part::Text(text) => Some(text.as_str()),
                    Part::Root | Part::Hole(_) => None,
                })
                .collect(),
            open_start: !matches!(pattern.0.first(), Some(Part::Text(_))),
            open_end: !matches!(pattern.0.last(), Some(Part::Text(_))),
        }
    }

    fn matches(&self, name: &str) -> bool {
        let mut rest = name;

        for (position, text) in self.texts.iter().enumerate() {
            if position == 0 && !self.open_start {
                let Some(after) = rest.strip_prefix(text) else {
                    return false;
                };

                rest = after;
            } else if position == self.texts.len() - 1 && !self.open_end {
                return rest.ends_with(text);
            } else {
                let Some(found) = rest.find(text) else {
                    return false;
                };

                rest = &rest[found + text.len()..];
            }
        }

        self.open_end || rest.is_empty()
    }
}

fn is_special(key: &str) -> bool {
    key.len() > 2 && key.starts_with('_') && key.ends_with('_')
}

fn is_plain_standard_base(name: &str) -> bool {
    PLAIN_STANDARD_BASES.contains(&name)
        || PLAIN_STANDARD_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

fn imported_target(imported: &Imported) -> &Target {
    match imported {
        Imported::Module(target) | Imported::Name { module: target, .. } => target,
    }
}
