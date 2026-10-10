//! Links the symbols of every analysed module: imported names followed to the symbols exporting them through re-exports, aliases and barrels, members read by name through classes, their bases and subclasses, what calling a function or constructing a class runs, module loads, and whole-module uses where a name cannot be followed.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use prober_core::files::{FileId, RepoPath};
use prober_core::graph::{EdgeKind, GraphBuilder};
use prober_core::symbols::{
    ModuleFallback, Symbol, SymbolChange, SymbolChangeKind, SymbolEdge, SymbolId, SymbolKind,
    SymbolTarget, WholeModuleCause,
};

use crate::conventions::is_asset_file;
use crate::linker::{Dependency, LinkedFile, RAW_IMPORT_QUERY};
use crate::symbols::changes::{Changed, module_changes};
use crate::symbols::{
    ExportTarget, Extends, Held, ImportBinding, Imported, Keeping, LocalSymbol, ModuleSymbols,
    Place, Shape, Use,
};

const DEFAULT_EXPORT: &str = "default";

const DATA_MODULE_EXTENSION: &str = ".json";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Resolved {
    Symbol {
        module: usize,
        symbol: usize,
    },
    Runs {
        module: usize,
        symbol: usize,
    },
    Namespace(usize),
    Whole {
        file: RepoPath,
        cause: WholeModuleCause,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Reached {
    resolved: Resolved,
    through: Vec<usize>,
}

impl Reached {
    fn direct(resolved: Resolved) -> Self {
        Self {
            resolved,
            through: Vec::new(),
        }
    }

    fn symbol(module: usize, symbol: usize) -> Self {
        Self::direct(Resolved::Symbol { module, symbol })
    }

    fn passing(mut self, module: usize, modules: &[Module]) -> Self {
        let ends_in_module = match &self.resolved {
            Resolved::Symbol {
                module: reached, ..
            }
            | Resolved::Runs {
                module: reached, ..
            }
            | Resolved::Namespace(reached) => *reached == module,
            Resolved::Whole { file, .. } => file == modules[module].path,
        };

        if !ends_in_module {
            self.through.insert(0, module);
        }

        self
    }

    fn behind(mut self, through: &[usize]) -> Self {
        self.through.splice(0..0, through.iter().copied());

        self
    }
}

struct ResolvedUse {
    dependent: usize,
    reached: Reached,
    raw: bool,
}

struct Module<'l> {
    path: &'l RepoPath,
    symbols: &'l ModuleSymbols,
    targets: &'l HashMap<String, Vec<RepoPath>>,
    dependencies: &'l [Dependency],
}

type Members = HashMap<(usize, usize, Place), HashMap<String, Vec<usize>>>;

struct Resolver<'l> {
    modules: Vec<Module<'l>>,
    module_indices: HashMap<&'l RepoPath, usize>,
    exports: HashMap<(usize, String), Vec<Reached>>,
    in_progress: HashSet<(usize, String)>,
    kept_parameters: HashMap<(usize, usize), Vec<bool>>,
    rest_parameters: HashSet<(usize, usize)>,
    members: Members,
    assigners: HashMap<(usize, usize), HashMap<String, Vec<usize>>>,
    subclasses: HashMap<(usize, usize), Vec<(usize, usize)>>,
    member_uses: HashMap<(usize, usize, String), Vec<Reached>>,
    members_in_progress: HashSet<(usize, usize, String)>,
}

struct Targets<'t> {
    symbol_ids: &'t [Vec<SymbolId>],
    module_files: &'t [FileId],
    runs_ids: HashMap<(usize, usize), SymbolId>,
    queue: Vec<(usize, usize)>,
}

impl Targets<'_> {
    fn target(
        &mut self,
        resolved: Resolved,
        modules: &[Module],
        graph: &mut GraphBuilder,
    ) -> (SymbolTarget, FileId, Option<RepoPath>) {
        match resolved {
            Resolved::Symbol { module, symbol } => (
                SymbolTarget::Symbol(self.symbol_ids[module][symbol]),
                self.module_files[module],
                None,
            ),
            Resolved::Runs { module, symbol } => (
                SymbolTarget::Symbol(self.runs_node(module, symbol, modules, graph)),
                self.module_files[module],
                None,
            ),
            Resolved::Namespace(module) => (
                SymbolTarget::Module {
                    file: self.module_files[module],
                    cause: WholeModuleCause::EscapingNamespace,
                },
                self.module_files[module],
                None,
            ),
            Resolved::Whole { file: path, cause } => {
                let file = graph.add_file(path.clone());

                (SymbolTarget::Module { file, cause }, file, Some(path))
            }
        }
    }

    fn runs_node(
        &mut self,
        module: usize,
        symbol: usize,
        modules: &[Module],
        graph: &mut GraphBuilder,
    ) -> SymbolId {
        if let Some(&id) = self.runs_ids.get(&(module, symbol)) {
            return id;
        }

        let local = &modules[module].symbols.symbols[symbol];
        let runs = local
            .runs
            .as_ref()
            .expect("only symbols with runs are reached as runs");
        let id = graph.add_symbol(Symbol {
            file: self.module_files[module],
            name: runs_name(local),
            kind: SymbolKind::Call,
            lines: runs.lines.clone(),
            hash: runs.hash,
        });

        self.runs_ids.insert((module, symbol), id);
        self.queue.push((module, symbol));

        id
    }
}

#[derive(Default)]
struct Edges {
    edges: BTreeMap<SymbolEdge, Vec<FileId>>,
    covered: HashSet<FileId>,
}

impl Edges {
    fn add(
        &mut self,
        dependent: SymbolId,
        (reached, raw): (Reached, bool),
        targets: &mut Targets,
        modules: &[Module],
        graph: &mut GraphBuilder,
    ) {
        let through: Vec<FileId> = reached
            .through
            .iter()
            .map(|&module| targets.module_files[module])
            .collect();
        let (dependency, file, path) = targets.target(reached.resolved, modules, graph);
        let is_asset = path.is_some_and(|path| is_asset_file(&path));
        let kind = if is_asset && !raw {
            EdgeKind::Asset
        } else {
            EdgeKind::Import
        };
        let edge = SymbolEdge {
            dependent,
            dependency,
            kind,
        };

        self.covered.insert(file);

        match self.edges.entry(edge) {
            Entry::Vacant(entry) => {
                entry.insert(through);
            }
            Entry::Occupied(mut entry) => entry.get_mut().retain(|file| through.contains(file)),
        }
    }

    fn flush(self, graph: &mut GraphBuilder) {
        for (edge, through) in self.edges {
            graph.add_symbol_edge_through(edge, through);
        }
    }
}

pub(crate) fn add_symbols(
    linked_files: &[LinkedFile],
    base_modules: &HashMap<RepoPath, ModuleSymbols>,
    graph: &mut GraphBuilder,
) {
    let mut modules = Vec::new();

    for linked_file in linked_files {
        let Some(analysis) = &linked_file.symbols else {
            continue;
        };

        match &analysis.module {
            Ok(symbols) => modules.push(Module {
                path: &linked_file.path,
                symbols,
                targets: &analysis.targets,
                dependencies: &linked_file.dependencies,
            }),
            Err(cause) => {
                let file = graph.add_file(linked_file.path.clone());

                graph.add_fallback(ModuleFallback {
                    file,
                    cause: *cause,
                });
            }
        }
    }

    let module_files: Vec<FileId> = modules
        .iter()
        .map(|module| graph.add_file(module.path.clone()))
        .collect();
    let symbol_ids: Vec<Vec<SymbolId>> = modules
        .iter()
        .zip(&module_files)
        .map(|(module, &file)| {
            module
                .symbols
                .symbols
                .iter()
                .map(|local_symbol| {
                    graph.add_symbol(Symbol {
                        file,
                        name: local_symbol.name.clone(),
                        kind: local_symbol.kind,
                        lines: local_symbol.lines.clone(),
                        hash: local_symbol.hash,
                    })
                })
                .collect()
        })
        .collect();
    let mut resolver = Resolver::new(modules);
    let mut targets = Targets {
        symbol_ids: &symbol_ids,
        module_files: &module_files,
        runs_ids: HashMap::new(),
        queue: Vec::new(),
    };

    resolver.solve_kept_parameters();
    resolver.link_classes();

    for (module, module_symbol_ids) in symbol_ids.iter().enumerate() {
        let mut edges = Edges::default();

        for resolved_use in resolver.resolved_uses(module) {
            edges.add(
                module_symbol_ids[resolved_use.dependent],
                (resolved_use.reached, resolved_use.raw),
                &mut targets,
                &resolver.modules,
                graph,
            );
        }

        for (member, local_symbol) in resolver.modules[module].symbols.symbols.iter().enumerate() {
            if let Some(membership) = &local_symbol.member {
                edges.edges.insert(
                    SymbolEdge {
                        dependent: module_symbol_ids[membership.owner],
                        dependency: SymbolTarget::Symbol(module_symbol_ids[member]),
                        kind: EdgeKind::Import,
                    },
                    Vec::new(),
                );
            }
        }

        let init = module_symbol_ids[ModuleSymbols::INIT];

        for dependency in resolver.modules[module].dependencies {
            if !matches!(dependency.kind, EdgeKind::Import | EdgeKind::Asset) {
                continue;
            }

            let file = graph.add_file(dependency.path.clone());

            if edges.covered.insert(file) {
                let edge = SymbolEdge {
                    dependent: init,
                    dependency: SymbolTarget::Module {
                        file,
                        cause: WholeModuleCause::Unattributed,
                    },
                    kind: dependency.kind,
                };

                edges.edges.insert(edge, Vec::new());
            }
        }

        edges.flush(graph);
    }

    while let Some((module, symbol)) = targets.queue.pop() {
        let dependent = targets.runs_ids[&(module, symbol)];
        let mut edges = Edges::default();

        for reached in resolver.runs_uses(module, symbol) {
            edges.add(dependent, reached, &mut targets, &resolver.modules, graph);
        }

        edges.flush(graph);
    }

    for (module, &file) in module_files.iter().enumerate() {
        let Some(base) = base_modules.get(resolver.modules[module].path) else {
            continue;
        };
        let symbol_changes = resolver
            .symbol_changes(module, base, &targets.runs_ids)
            .into_iter()
            .map(|(name, kind, reaches)| SymbolChange {
                file,
                name,
                kind,
                reaches: reaches
                    .into_iter()
                    .map(|reached| targets.target(reached, &resolver.modules, graph).0)
                    .collect(),
            })
            .collect();

        graph.add_module_changes(file, symbol_changes);
    }
}

fn runs_name(local: &LocalSymbol) -> String {
    if local.kind == SymbolKind::Class {
        format!("new {}()", local.name)
    } else {
        format!("{}()", local.name)
    }
}

impl<'l> Resolver<'l> {
    fn new(modules: Vec<Module<'l>>) -> Self {
        let mut members: Members = HashMap::new();
        let mut assigners: HashMap<(usize, usize), HashMap<String, Vec<usize>>> = HashMap::new();

        for (module_index, module) in modules.iter().enumerate() {
            for (symbol, local) in module.symbols.symbols.iter().enumerate() {
                let Some(membership) = &local.member else {
                    continue;
                };

                members
                    .entry((module_index, membership.owner, membership.place))
                    .or_default()
                    .entry(membership.key.clone())
                    .or_default()
                    .push(symbol);

                for assigned in &membership.assigns {
                    assigners
                        .entry((module_index, membership.owner))
                        .or_default()
                        .entry(assigned.clone())
                        .or_default()
                        .push(symbol);
                }
            }
        }

        Self {
            module_indices: modules
                .iter()
                .enumerate()
                .map(|(index, module)| (module.path, index))
                .collect(),
            modules,
            exports: HashMap::new(),
            in_progress: HashSet::new(),
            kept_parameters: HashMap::new(),
            rest_parameters: HashSet::new(),
            members,
            assigners,
            subclasses: HashMap::new(),
            member_uses: HashMap::new(),
            members_in_progress: HashSet::new(),
        }
    }

    fn targets(&self, module: usize, specifier: &str) -> Vec<RepoPath> {
        self.modules[module]
            .targets
            .get(specifier)
            .cloned()
            .unwrap_or_default()
    }

    fn local(&self, module: usize, symbol: usize) -> &'l LocalSymbol {
        let symbols: &'l ModuleSymbols = self.modules[module].symbols;

        &symbols.symbols[symbol]
    }

    fn is_class(&self, module: usize, symbol: usize) -> bool {
        self.local(module, symbol).kind == SymbolKind::Class
    }

    fn resolved_uses(&mut self, module: usize) -> Vec<ResolvedUse> {
        let symbols: &'l ModuleSymbols = self.modules[module].symbols;
        let mut resolved_uses = Vec::new();

        for (dependent, local_symbol) in symbols.symbols.iter().enumerate() {
            for symbol_use in &local_symbol.uses {
                let raw = is_raw(symbols, symbol_use);

                resolved_uses.extend(self.resolve_use(module, symbol_use).into_iter().map(
                    |reached| ResolvedUse {
                        dependent,
                        reached,
                        raw,
                    },
                ));
            }
        }

        for held in &symbols.held {
            if self.is_kept(module, held) {
                continue;
            }

            for symbol_use in &held.uses {
                let raw = is_raw(symbols, symbol_use);

                resolved_uses.extend(self.resolve_use(module, symbol_use).into_iter().map(
                    |reached| ResolvedUse {
                        dependent: ModuleSymbols::INIT,
                        reached,
                        raw,
                    },
                ));
            }
        }

        for specifier in &symbols.loads {
            let raw = specifier.contains(RAW_IMPORT_QUERY);

            resolved_uses.extend(self.load(module, specifier).into_iter().map(|resolved| {
                ResolvedUse {
                    dependent: ModuleSymbols::INIT,
                    reached: Reached::direct(resolved),
                    raw,
                }
            }));
        }

        resolved_uses
    }

    fn runs_uses(&mut self, module: usize, symbol: usize) -> Vec<(Reached, bool)> {
        let symbols: &'l ModuleSymbols = self.modules[module].symbols;
        let Some(runs) = &symbols.symbols[symbol].runs else {
            return Vec::new();
        };
        let held_uses = runs
            .held
            .iter()
            .filter(|held| !self.is_kept(module, held))
            .flat_map(|held| &held.uses)
            .collect::<Vec<_>>();
        let mut reached = Vec::new();

        for symbol_use in runs.uses.iter().chain(held_uses) {
            let raw = is_raw(symbols, symbol_use);

            reached.extend(
                self.resolve_use(module, symbol_use)
                    .into_iter()
                    .map(|resolved| (resolved, raw)),
            );
        }

        reached
    }

    fn is_kept(&mut self, module: usize, held: &Held) -> bool {
        held.kept_by
            .iter()
            .all(|keeping| self.keeps(module, keeping))
    }

    fn symbol_changes(
        &mut self,
        module: usize,
        base: &ModuleSymbols,
        runs_ids: &HashMap<(usize, usize), SymbolId>,
    ) -> Vec<(String, SymbolChangeKind, Vec<Resolved>)> {
        let head: &'l ModuleSymbols = self.modules[module].symbols;
        let mut symbol_changes = Vec::new();

        for changed in module_changes(base, head) {
            match changed {
                Changed::Symbol { index, kind } => symbol_changes.push((
                    head.symbols[index].name.clone(),
                    kind,
                    vec![Resolved::Symbol {
                        module,
                        symbol: index,
                    }],
                )),
                Changed::Held { index, groups } => {
                    let runs = groups
                        .iter()
                        .any(|&group| !self.is_kept(module, &head.held[group]));

                    if runs {
                        symbol_changes.push((
                            head.symbols[index].name.clone(),
                            SymbolChangeKind::Modified,
                            vec![Resolved::Symbol {
                                module,
                                symbol: index,
                            }],
                        ));
                    }
                }
                Changed::Runs { index, groups } => {
                    let local = &head.symbols[index];

                    if !runs_ids.contains_key(&(module, index)) {
                        continue;
                    }

                    let runs = match (&groups, &local.runs) {
                        (Some(groups), Some(runs)) => groups
                            .iter()
                            .any(|&group| !self.is_kept(module, &runs.held[group])),
                        _ => true,
                    };

                    if runs {
                        symbol_changes.push((
                            runs_name(local),
                            SymbolChangeKind::Modified,
                            vec![Resolved::Runs {
                                module,
                                symbol: index,
                            }],
                        ));
                    }
                }
                Changed::Removed(name) => {
                    symbol_changes.push((name, SymbolChangeKind::Removed, Vec::new()));
                }
                Changed::Export(name) => {
                    let reaches = self
                        .resolve_name(module, &name)
                        .into_iter()
                        .map(|reached| reached.resolved)
                        .collect();

                    symbol_changes.push((name, SymbolChangeKind::Export, reaches));
                }
                Changed::StarExport(specifier) => {
                    for target in self.targets(module, &specifier) {
                        let Some(&target_module) = self.module_indices.get(&target) else {
                            let reaches = vec![Resolved::Whole {
                                file: target,
                                cause: WholeModuleCause::NotAnalysed,
                            }];

                            symbol_changes.push((
                                specifier.clone(),
                                SymbolChangeKind::Export,
                                reaches,
                            ));

                            continue;
                        };
                        let mut names = BTreeSet::new();

                        self.exported_names(target_module, &mut HashSet::new(), &mut names);

                        for name in names {
                            let reaches = self
                                .resolve_name(module, &name)
                                .into_iter()
                                .map(|reached| reached.resolved)
                                .collect();

                            symbol_changes.push((name, SymbolChangeKind::Export, reaches));
                        }
                    }
                }
            }
        }

        symbol_changes
    }

    fn exported_names(
        &self,
        module: usize,
        visited: &mut HashSet<usize>,
        names: &mut BTreeSet<String>,
    ) {
        if !visited.insert(module) {
            return;
        }

        let symbols = self.modules[module].symbols;

        names.extend(
            symbols
                .exports
                .iter()
                .map(|(name, _)| name)
                .filter(|name| *name != DEFAULT_EXPORT)
                .cloned(),
        );

        for specifier in &symbols.star_exports {
            for target in self.targets(module, specifier) {
                if let Some(&target_module) = self.module_indices.get(&target) {
                    self.exported_names(target_module, visited, names);
                }
            }
        }
    }

    fn star_sources(&self, module: usize) -> Vec<usize> {
        let mut searched = vec![module];
        let mut index = 0;

        while let Some(&current) = searched.get(index) {
            index += 1;

            for specifier in &self.modules[current].symbols.star_exports {
                for target in self.targets(current, specifier) {
                    if let Some(&target_module) = self.module_indices.get(&target)
                        && !searched.contains(&target_module)
                    {
                        searched.push(target_module);
                    }
                }
            }
        }

        searched
    }

    fn solve_kept_parameters(&mut self) {
        for (module_index, module) in self.modules.iter().enumerate() {
            for (symbol_index, symbol) in module.symbols.symbols.iter().enumerate() {
                if symbol.parameters.iter().any(|parameter| parameter.kept) {
                    let kept = symbol
                        .parameters
                        .iter()
                        .map(|parameter| parameter.kept)
                        .collect();

                    self.kept_parameters
                        .insert((module_index, symbol_index), kept);
                }

                if symbol
                    .parameters
                    .last()
                    .is_some_and(|parameter| parameter.rest)
                {
                    self.rest_parameters.insert((module_index, symbol_index));
                }
            }
        }

        let mut changed = true;

        while changed {
            changed = false;

            let mut kept: Vec<(usize, usize, usize)> = self
                .kept_parameters
                .iter()
                .flat_map(|(&(module, symbol), flags)| {
                    flags
                        .iter()
                        .enumerate()
                        .filter(|(_, kept)| **kept)
                        .map(move |(parameter, _)| (module, symbol, parameter))
                })
                .collect();

            kept.sort_unstable();

            for (module, symbol, parameter) in kept {
                let passed = &self.local(module, symbol).parameters[parameter].passed;
                let still_kept = passed.iter().all(|keeping| self.keeps(module, keeping));

                if !still_kept {
                    self.kept_parameters
                        .get_mut(&(module, symbol))
                        .expect("kept parameters are listed")[parameter] = false;
                    changed = true;
                }
            }
        }
    }

    fn keeps(&mut self, module: usize, keeping: &Keeping) -> bool {
        match self.resolve_use(module, &keeping.callee).as_slice() {
            [
                Reached {
                    resolved: Resolved::Symbol { module, symbol },
                    ..
                },
            ] => {
                if self.local(*module, *symbol).shape == Some(Shape::Assign) {
                    return keeping.index == 0 || keeping.fresh_target;
                }

                let rest = self.rest_parameters.contains(&(*module, *symbol));

                self.kept_parameters
                    .get(&(*module, *symbol))
                    .and_then(|kept| {
                        kept.get(keeping.index)
                            .or_else(|| kept.last().filter(|_| rest))
                    })
                    .copied()
                    .unwrap_or(false)
            }
            _ => false,
        }
    }

    fn link_classes(&mut self) {
        for module in 0..self.modules.len() {
            let symbols: &'l ModuleSymbols = self.modules[module].symbols;

            for (class, local) in symbols.symbols.iter().enumerate() {
                let Some(Shape::Class {
                    extends: Extends::Binding(base),
                }) = &local.shape
                else {
                    continue;
                };

                for reached in self.resolve_use(module, base) {
                    if let Resolved::Symbol {
                        module: base_module,
                        symbol: base_class,
                    } = reached.resolved
                        && self.is_class(base_module, base_class)
                    {
                        self.subclasses
                            .entry((base_module, base_class))
                            .or_default()
                            .push((module, class));
                    }
                }
            }
        }
    }

    fn defined(&self, module: usize, owner: usize, key: &str, place: Place) -> Vec<Reached> {
        let members = self
            .members
            .get(&(module, owner, place))
            .and_then(|by_key| by_key.get(key))
            .into_iter()
            .flatten();
        let assigners = (place == Place::Instance)
            .then(|| {
                self.assigners
                    .get(&(module, owner))
                    .and_then(|by_key| by_key.get(key))
            })
            .flatten()
            .into_iter()
            .flatten();
        let mut defined: Vec<usize> = members.chain(assigners).copied().collect();

        defined.sort_unstable();
        defined.dedup();

        defined
            .into_iter()
            .map(|symbol| Reached::symbol(module, symbol))
            .collect()
    }

    fn lookup(
        &mut self,
        module: usize,
        owner: usize,
        key: &str,
        place: Place,
        visited: &mut HashSet<(usize, usize)>,
    ) -> Vec<Reached> {
        if !visited.insert((module, owner)) {
            return Vec::new();
        }

        if self.local(module, owner).shape == Some(Shape::Object { open: true }) {
            return vec![Reached::symbol(module, owner)];
        }

        let defined = self.defined(module, owner, key, place);

        if !defined.is_empty() {
            return defined;
        }

        let inherited = self.in_bases(module, owner, |resolver, base_module, base_class| {
            resolver.lookup(base_module, base_class, key, place, visited)
        });

        if inherited.is_empty() {
            vec![Reached::symbol(module, owner)]
        } else {
            inherited
        }
    }

    fn in_bases(
        &mut self,
        module: usize,
        class: usize,
        mut lookup: impl FnMut(&mut Self, usize, usize) -> Vec<Reached>,
    ) -> Vec<Reached> {
        let Some(Shape::Class {
            extends: Extends::Binding(base),
        }) = &self.local(module, class).shape
        else {
            return Vec::new();
        };
        let mut reached = Vec::new();

        for base_reached in self.resolve_use(module, base) {
            match base_reached.resolved {
                Resolved::Symbol {
                    module: base_module,
                    symbol: base_class,
                } if self.is_class(base_module, base_class) => {
                    let found = lookup(self, base_module, base_class);

                    reached.extend(
                        found
                            .into_iter()
                            .map(|found| found.behind(&base_reached.through)),
                    );
                }
                _ => reached.push(base_reached),
            }
        }

        reached
    }

    fn member_of(&mut self, module: usize, symbol: usize, member: &str) -> Vec<Reached> {
        let key = (module, symbol, member.to_owned());

        if let Some(reached) = self.member_uses.get(&key) {
            return reached.clone();
        }

        if !self.members_in_progress.insert(key.clone()) {
            return vec![Reached::symbol(module, symbol)];
        }

        let reached = self.find_member(module, symbol, member);

        self.members_in_progress.remove(&key);
        self.member_uses.insert(key, reached.clone());

        reached
    }

    fn find_member(&mut self, module: usize, symbol: usize, member: &str) -> Vec<Reached> {
        let local = self.local(module, symbol);

        match &local.shape {
            Some(Shape::Class { .. } | Shape::Object { .. }) => {
                self.lookup(module, symbol, member, Place::Own, &mut HashSet::new())
            }
            Some(Shape::Instance(class)) => {
                let mut reached = Vec::new();

                for class_reached in self.resolve_use(module, class) {
                    match class_reached.resolved {
                        Resolved::Symbol {
                            module: class_module,
                            symbol: class_symbol,
                        } if self.is_class(class_module, class_symbol) => {
                            let found = self.lookup(
                                class_module,
                                class_symbol,
                                member,
                                Place::Instance,
                                &mut HashSet::new(),
                            );

                            reached.extend(
                                found
                                    .into_iter()
                                    .map(|found| found.behind(&class_reached.through)),
                            );
                        }
                        _ => reached.push(class_reached),
                    }
                }

                for other in local.uses.iter().filter(|other| *other != class) {
                    reached.extend(self.resolve_use(module, other));
                }

                reached
            }
            Some(Shape::Assign) | None => vec![Reached::symbol(module, symbol)],
        }
    }

    fn dispatch(
        &mut self,
        module: usize,
        class: usize,
        member: &str,
        place: Place,
    ) -> Vec<Reached> {
        let mut reached = self.lookup(module, class, member, place, &mut HashSet::new());
        let mut stack: Vec<(usize, usize)> = self
            .subclasses
            .get(&(module, class))
            .cloned()
            .unwrap_or_default();
        let mut seen = HashSet::new();

        while let Some((subclass_module, subclass)) = stack.pop() {
            if !seen.insert((subclass_module, subclass)) {
                continue;
            }

            reached.extend(self.defined(subclass_module, subclass, member, place));
            stack.extend(
                self.subclasses
                    .get(&(subclass_module, subclass))
                    .into_iter()
                    .flatten()
                    .copied(),
            );
        }

        reached
    }

    fn inherited(
        &mut self,
        module: usize,
        class: usize,
        member: &str,
        place: Place,
    ) -> Vec<Reached> {
        let inherited = self.in_bases(module, class, |resolver, base_module, base_class| {
            resolver.lookup(base_module, base_class, member, place, &mut HashSet::new())
        });

        if inherited.is_empty() {
            vec![Reached::symbol(module, class)]
        } else {
            inherited
        }
    }

    fn called(&self, reached: Reached, kind: SymbolKind) -> Reached {
        match reached.resolved {
            Resolved::Symbol { module, symbol } => {
                let local = self.local(module, symbol);

                if local.kind == kind && local.runs.is_some() {
                    Reached {
                        resolved: Resolved::Runs { module, symbol },
                        through: reached.through,
                    }
                } else {
                    reached
                }
            }
            _ => reached,
        }
    }

    fn load(&self, module: usize, specifier: &str) -> Vec<Resolved> {
        let raw = specifier.contains(RAW_IMPORT_QUERY);

        self.targets(module, specifier)
            .into_iter()
            .filter_map(|target| match self.module_indices.get(&target) {
                Some(&target_module) => Some(Resolved::Symbol {
                    module: target_module,
                    symbol: ModuleSymbols::INIT,
                }),
                None if raw || target.as_str().ends_with(DATA_MODULE_EXTENSION) => None,
                None => Some(Resolved::Whole {
                    file: target,
                    cause: WholeModuleCause::NotAnalysed,
                }),
            })
            .collect()
    }

    fn resolve_use(&mut self, module: usize, symbol_use: &Use) -> Vec<Reached> {
        match symbol_use {
            Use::Local(symbol) => vec![Reached::symbol(module, *symbol)],
            Use::Member { symbol, member } => self.member_of(module, *symbol, member),
            Use::This {
                class,
                member,
                place,
            } => self.dispatch(module, *class, member, *place),
            Use::Super {
                class,
                member,
                place,
            } => self.inherited(module, *class, member, *place),
            Use::Call(called) => self
                .resolve_use(module, called)
                .into_iter()
                .map(|reached| self.called(reached, SymbolKind::Function))
                .collect(),
            Use::New(constructed) => self
                .resolve_use(module, constructed)
                .into_iter()
                .map(|reached| self.called(reached, SymbolKind::Class))
                .collect(),
            Use::Import { binding, member } => {
                let binding = &self.local_imports(module)[*binding];
                let mut reached = Vec::new();

                for imported in self.resolve_import(module, &binding.specifier, &binding.imported) {
                    match (&imported.resolved, member) {
                        (Resolved::Namespace(namespace), Some(member)) => {
                            let namespace = *namespace;
                            let members = self.resolve_name(namespace, member);

                            reached.extend(members.into_iter().map(|member| {
                                member
                                    .passing(namespace, &self.modules)
                                    .behind(&imported.through)
                            }));
                        }
                        (Resolved::Namespace(namespace), None) => {
                            let cause = if binding.imported == Imported::Dynamic {
                                WholeModuleCause::DynamicLoad
                            } else {
                                WholeModuleCause::EscapingNamespace
                            };

                            reached.push(Reached {
                                resolved: Resolved::Whole {
                                    file: self.modules[*namespace].path.clone(),
                                    cause,
                                },
                                through: imported.through,
                            });
                        }
                        (
                            Resolved::Symbol {
                                module: symbol_module,
                                symbol,
                            },
                            Some(member),
                        ) => {
                            let found = self.member_of(*symbol_module, *symbol, member);

                            reached.extend(
                                found
                                    .into_iter()
                                    .map(|found| found.behind(&imported.through)),
                            );
                        }
                        _ => reached.push(imported),
                    }
                }

                reached
            }
            Use::Module(specifier) => self
                .targets(module, specifier)
                .into_iter()
                .map(|file| {
                    Reached::direct(Resolved::Whole {
                        file,
                        cause: WholeModuleCause::DynamicLoad,
                    })
                })
                .collect(),
            Use::Load(specifier) => self
                .load(module, specifier)
                .into_iter()
                .map(Reached::direct)
                .collect(),
        }
    }

    fn local_imports(&self, module: usize) -> &'l [ImportBinding] {
        let symbols: &'l ModuleSymbols = self.modules[module].symbols;

        &symbols.imports
    }

    fn resolve_import(
        &mut self,
        module: usize,
        specifier: &str,
        imported: &Imported,
    ) -> Vec<Reached> {
        let mut reached = Vec::new();

        for target in self.targets(module, specifier) {
            let Some(&target_module) = self.module_indices.get(&target) else {
                reached.push(Reached::direct(Resolved::Whole {
                    file: target,
                    cause: WholeModuleCause::NotAnalysed,
                }));

                continue;
            };

            match imported {
                Imported::Namespace | Imported::Dynamic => {
                    reached.push(Reached::direct(Resolved::Namespace(target_module)));
                }
                Imported::Module => reached.push(Reached::direct(Resolved::Whole {
                    file: target,
                    cause: WholeModuleCause::DynamicLoad,
                })),
                Imported::Name(name) => {
                    let named = self.resolve_name(target_module, name);

                    reached.extend(
                        named
                            .into_iter()
                            .map(|named| named.passing(target_module, &self.modules)),
                    );
                }
            }
        }

        reached
    }

    fn resolve_name(&mut self, module: usize, name: &str) -> Vec<Reached> {
        let exported = self.resolve_export(module, name);

        if !exported.is_empty() {
            return exported;
        }

        let searched = if name == DEFAULT_EXPORT {
            vec![module]
        } else {
            self.star_sources(module)
        };

        searched
            .into_iter()
            .map(|searched_module| {
                Reached::direct(Resolved::Whole {
                    file: self.modules[searched_module].path.clone(),
                    cause: WholeModuleCause::MissingExport,
                })
            })
            .collect()
    }

    fn resolve_export(&mut self, module: usize, name: &str) -> Vec<Reached> {
        let key = (module, name.to_owned());

        if let Some(resolved) = self.exports.get(&key) {
            return resolved.clone();
        }

        if !self.in_progress.insert(key.clone()) {
            return Vec::new();
        }

        let resolved = self.find_export(module, name);

        self.in_progress.remove(&key);
        self.exports.insert(key, resolved.clone());

        resolved
    }

    fn find_export(&mut self, module: usize, name: &str) -> Vec<Reached> {
        let symbols: &'l ModuleSymbols = self.modules[module].symbols;
        let mut entries = symbols
            .exports
            .iter()
            .filter(|(exported_name, _)| exported_name == name)
            .map(|(_, target)| target)
            .peekable();

        if entries.peek().is_some() {
            let mut resolved = Vec::new();

            for entry in entries {
                match entry {
                    ExportTarget::Local(symbol) => resolved.push(Reached::symbol(module, *symbol)),
                    ExportTarget::Import(binding) => {
                        let binding = &symbols.imports[*binding];

                        resolved.extend(self.resolve_import(
                            module,
                            &binding.specifier,
                            &binding.imported,
                        ));
                    }
                    ExportTarget::Reexport { specifier, name } => {
                        resolved.extend(self.resolve_import(
                            module,
                            specifier,
                            &Imported::Name(name.clone()),
                        ));
                    }
                    ExportTarget::Namespace(specifier) => {
                        resolved.extend(self.resolve_import(
                            module,
                            specifier,
                            &Imported::Namespace,
                        ));
                    }
                }
            }

            return resolved;
        }

        if name == DEFAULT_EXPORT {
            return Vec::new();
        }

        let mut found = Vec::new();
        let mut not_analysed = Vec::new();

        for specifier in &symbols.star_exports {
            for target in self.targets(module, specifier) {
                match self.module_indices.get(&target) {
                    Some(&target_module) => {
                        let exported = self.resolve_export(target_module, name);

                        found.extend(
                            exported
                                .into_iter()
                                .map(|exported| exported.passing(target_module, &self.modules)),
                        );
                    }
                    None => not_analysed.push(Reached::direct(Resolved::Whole {
                        file: target,
                        cause: WholeModuleCause::NotAnalysed,
                    })),
                }
            }
        }

        if found.is_empty() {
            not_analysed
        } else {
            found
        }
    }
}

fn is_raw(symbols: &ModuleSymbols, symbol_use: &Use) -> bool {
    match symbol_use {
        Use::Import { binding, .. } => symbols.imports[*binding]
            .specifier
            .contains(RAW_IMPORT_QUERY),
        Use::Module(specifier) | Use::Load(specifier) => specifier.contains(RAW_IMPORT_QUERY),
        Use::Call(called) | Use::New(called) => is_raw(symbols, called),
        Use::Local(_) | Use::Member { .. } | Use::This { .. } | Use::Super { .. } => false,
    }
}
