//! Links the symbols of every analysed module: names followed through imports, re-exports, submodules and star imports to the symbols defining them, the modules each symbol loads, the definitions load-time code holds, and whole-module uses where a name cannot be followed.

use std::collections::{BTreeSet, HashMap, HashSet};

use prober_core::files::{FileId, RepoPath};
use prober_core::graph::{EdgeKind, GraphBuilder};
use prober_core::symbols::{
    ModuleFallback, Symbol, SymbolChange, SymbolChangeKind, SymbolEdge, SymbolId, SymbolKind,
    SymbolTarget, WholeModuleCause,
};

use crate::conventions::{PACKAGE_INIT, split_path};
use crate::links::AttributePath;
use crate::modules::{ImportTarget, ModuleIndex};
use crate::pytest::fixtures::ModuleEdge;
use crate::summary::ModuleSummary;
use crate::symbols::changes::{Changed, module_changes};
use crate::symbols::{Binding, Decoration, Imported, LocalSymbol, ModuleSymbols, Target, Use};

const MAX_DEPTH: usize = 32;

const PYTEST_NAMES: [&str; 2] = ["pytest", "collect_ignore"];

const MODULE_GETATTR: &str = "__getattr__";

const TEST_CASE_SUFFIX: &str = "TestCase";

const STAR_NAMES: &str = "__all__";

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
    pub attribute_paths: &'a [AttributePath],
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
    Binding {
        module: usize,
        name: String,
    },
}

type Anchors = HashMap<(usize, String), SymbolId>;

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
    writers: HashMap<usize, Vec<usize>>,
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
            writers: HashMap::new(),
        };

        let mut anchors = Anchors::new();

        resolver.index_namespace_writers();

        for module in 0..resolver.modules.len() {
            self.add_edges(&mut resolver, module, &ids, &files, &mut anchors, graph);
        }

        for module in 0..resolver.modules.len() {
            self.add_changes(&mut resolver, module, &ids, &files, &anchors, graph);
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
        module: usize,
        ids: &[Vec<SymbolId>],
        files: &[FileId],
        anchors: &mut Anchors,
        graph: &mut GraphBuilder,
    ) {
        let Module {
            path,
            symbols,
            summary,
        } = resolver.modules[module];
        let mut edges = Edges {
            edges: Vec::new(),
            covered: HashSet::new(),
            anchors,
        };
        let init = ids[module][ModuleSymbols::INIT];

        for (symbol, local) in symbols.symbols.iter().enumerate() {
            let dependent = ids[module][symbol];

            for symbol_use in &local.uses {
                for reached in resolver.resolve_use(module, symbol, symbol_use) {
                    edges.add(dependent, reached, ids, files, graph);
                }
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

            if symbol != ModuleSymbols::INIT && self.held(resolver, module, local) {
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

        for attribute_path in self
            .attribute_paths
            .iter()
            .filter(|attribute_path| attribute_path.reader == *path)
        {
            let reader = ids[module][symbols.symbol_at(attribute_path.line)];

            for package in attribute_path
                .packages
                .iter()
                .filter(|package| *package != path)
            {
                let reached = match resolver.module_indices.get(package) {
                    Some(&loaded) => Reached::Symbol {
                        module: loaded,
                        symbol: ModuleSymbols::INIT,
                    },
                    None => Reached::Whole {
                        path: package.clone(),
                        cause: WholeModuleCause::NotAnalysed,
                    },
                };

                edges.add(reader, reached, ids, files, graph);
            }

            let module_target = Target::Repo(attribute_path.module.clone());

            if !attribute_path.attributes.is_empty()
                && let Some(&loaded) = resolver.module_indices.get(&attribute_path.module)
            {
                let init = Reached::Symbol {
                    module: loaded,
                    symbol: ModuleSymbols::INIT,
                };

                edges.add(reader, init, ids, files, graph);
            }

            for reached in resolver.resolve_target(&module_target, &attribute_path.attributes, 0) {
                edges.add(reader, reached, ids, files, graph);
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

    fn held(&self, resolver: &mut Resolver, module: usize, local: &LocalSymbol) -> bool {
        let path = resolver.modules[module].path;

        if self.tests.contains(path) {
            return true;
        }

        let is_provider = self.providers.contains(path);
        let held = match local.decoration {
            Decoration::Registering => resolver.decorators_register(module, &local.decorators),
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
        anchors: &Anchors,
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
            let (name, kind, mut reaches) = match changed {
                Changed::Symbol { index, kind } => (
                    head.symbols[index].name.clone(),
                    kind,
                    vec![SymbolTarget::Symbol(ids[module][index])],
                ),
                Changed::Removed(name) => (name, SymbolChangeKind::Removed, Vec::new()),
                Changed::Binding(name) => {
                    let reaches = anchors
                        .get(&(module, name.clone()))
                        .map(|&anchor| SymbolTarget::Symbol(anchor))
                        .into_iter()
                        .collect();

                    (name, SymbolChangeKind::Export, reaches)
                }
            };
            let held_at_base = base
                .symbols
                .iter()
                .skip(1)
                .filter(|local| local.name == name)
                .any(|local| self.held(resolver, module, local));

            if held_at_base {
                reaches.push(SymbolTarget::Symbol(ids[module][ModuleSymbols::INIT]));
            }

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

struct Edges<'a> {
    edges: Vec<SymbolEdge>,
    covered: HashSet<FileId>,
    anchors: &'a mut Anchors,
}

impl Edges<'_> {
    fn add(
        &mut self,
        dependent: SymbolId,
        reached: Reached,
        ids: &[Vec<SymbolId>],
        files: &[FileId],
        graph: &mut GraphBuilder,
    ) {
        let (dependency, file) = target(reached, ids, files, self.anchors, graph);

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
    anchors: &mut Anchors,
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
        Reached::Binding { module, name } => {
            let file = files[module];
            let anchor = *anchors.entry((module, name.clone())).or_insert_with(|| {
                graph.add_symbol(Symbol {
                    file,
                    name,
                    kind: SymbolKind::Variable,
                    lines: Vec::new(),
                    hash: 0,
                })
            });

            (SymbolTarget::Symbol(anchor), file)
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
                symbol: *symbol,
            }],
            Binding::Import(imported) => self.resolve_imported(imported, attributes, depth),
        }
    }

    fn index_namespace_writers(&mut self) {
        for module in 0..self.modules.len() {
            let symbols = self.modules[module].symbols;
            let mut writers = Vec::new();

            for (symbol, local) in symbols.symbols.iter().enumerate() {
                if local.namespace.writes {
                    writers.push(symbol);
                }
            }

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

    fn decorators_register(&mut self, module: usize, decorators: &[Option<Use>]) -> bool {
        decorators
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
                let (module, symbol) = match reached {
                    Reached::Symbol { module, symbol } => (module, symbol),
                    Reached::Binding { .. } => continue,
                    Reached::Whole { .. } => return true,
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
            let mut reached: Vec<Reached> = bindings
                .iter()
                .flat_map(|binding| self.resolve_binding(module, binding, rest, depth))
                .collect();

            if bindings
                .iter()
                .any(|binding| matches!(binding, Binding::Import(_)))
            {
                reached.push(Reached::Binding {
                    module,
                    name: name.clone(),
                });
            }

            return reached;
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
                            _ if names_a_test_case(imported, base) => continue,
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
            Reached::Binding { .. } => false,
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

fn names_a_test_case(imported: &Imported, base: &Use) -> bool {
    let name = match (imported, base.attributes.last()) {
        (_, Some(attribute)) => attribute,
        (Imported::Name { name, .. }, None) => name,
        (Imported::Module(_), None) => return false,
    };

    name.ends_with(TEST_CASE_SUFFIX)
}

fn imported_target(imported: &Imported) -> &Target {
    match imported {
        Imported::Module(target) | Imported::Name { module: target, .. } => target,
    }
}
