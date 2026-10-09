//! Links the symbols of every analysed module: imported names followed to the symbols exporting them through re-exports, aliases and barrels, module loads, and whole-module uses where a name cannot be followed.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use prober_core::files::{FileId, RepoPath};
use prober_core::graph::{EdgeKind, GraphBuilder};
use prober_core::symbols::{
    ModuleFallback, Symbol, SymbolChange, SymbolChangeKind, SymbolEdge, SymbolId, SymbolTarget,
    WholeModuleCause,
};

use crate::conventions::is_asset_file;
use crate::linker::{Dependency, LinkedFile, RAW_IMPORT_QUERY};
use crate::symbols::changes::{Changed, module_changes};
use crate::symbols::{ExportTarget, Imported, ModuleSymbols, Use};

const DEFAULT_EXPORT: &str = "default";

const DATA_MODULE_EXTENSION: &str = ".json";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Resolved {
    Symbol {
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

    fn passing(mut self, module: usize, modules: &[Module]) -> Self {
        let ends_in_module = match &self.resolved {
            Resolved::Symbol {
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

struct Resolver<'l> {
    modules: Vec<Module<'l>>,
    module_indices: HashMap<&'l RepoPath, usize>,
    exports: HashMap<(usize, String), Vec<Reached>>,
    in_progress: HashSet<(usize, String)>,
    kept_parameters: HashMap<(usize, usize), Vec<bool>>,
}

struct Targets<'t> {
    symbol_ids: &'t [Vec<SymbolId>],
    module_files: &'t [FileId],
}

impl Targets<'_> {
    fn target(
        &self,
        resolved: Resolved,
        graph: &mut GraphBuilder,
    ) -> (SymbolTarget, FileId, Option<RepoPath>) {
        match resolved {
            Resolved::Symbol { module, symbol } => (
                SymbolTarget::Symbol(self.symbol_ids[module][symbol]),
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
    let mut resolver = Resolver {
        module_indices: modules
            .iter()
            .enumerate()
            .map(|(index, module)| (module.path, index))
            .collect(),
        modules,
        exports: HashMap::new(),
        in_progress: HashSet::new(),
        kept_parameters: HashMap::new(),
    };

    let targets = Targets {
        symbol_ids: &symbol_ids,
        module_files: &module_files,
    };

    resolver.solve_kept_parameters();

    for (module, module_symbol_ids) in symbol_ids.iter().enumerate() {
        let mut edges: BTreeMap<SymbolEdge, Vec<FileId>> = BTreeMap::new();
        let mut covered_files = HashSet::new();

        for resolved_use in resolver.resolved_uses(module) {
            let through: Vec<FileId> = resolved_use
                .reached
                .through
                .iter()
                .map(|&module| module_files[module])
                .collect();
            let (dependency, file, path) = targets.target(resolved_use.reached.resolved, graph);
            let is_asset = path.is_some_and(|path| is_asset_file(&path));
            let kind = if is_asset && !resolved_use.raw {
                EdgeKind::Asset
            } else {
                EdgeKind::Import
            };
            let edge = SymbolEdge {
                dependent: module_symbol_ids[resolved_use.dependent],
                dependency,
                kind,
            };

            covered_files.insert(file);

            match edges.entry(edge) {
                Entry::Vacant(entry) => {
                    entry.insert(through);
                }
                Entry::Occupied(mut entry) => entry.get_mut().retain(|file| through.contains(file)),
            }
        }

        let init = module_symbol_ids[ModuleSymbols::INIT];

        for dependency in resolver.modules[module].dependencies {
            if !matches!(dependency.kind, EdgeKind::Import | EdgeKind::Asset) {
                continue;
            }

            let file = graph.add_file(dependency.path.clone());

            if covered_files.insert(file) {
                let edge = SymbolEdge {
                    dependent: init,
                    dependency: SymbolTarget::Module {
                        file,
                        cause: WholeModuleCause::Unattributed,
                    },
                    kind: dependency.kind,
                };

                edges.insert(edge, Vec::new());
            }
        }

        for (edge, through) in edges {
            graph.add_symbol_edge_through(edge, through);
        }
    }

    for (module, &file) in module_files.iter().enumerate() {
        let Some(base) = base_modules.get(resolver.modules[module].path) else {
            continue;
        };
        let symbol_changes = resolver
            .symbol_changes(module, base)
            .into_iter()
            .map(|(name, kind, reaches)| SymbolChange {
                file,
                name,
                kind,
                reaches: reaches
                    .into_iter()
                    .map(|reached| targets.target(reached.resolved, graph).0)
                    .collect(),
            })
            .collect();

        graph.add_module_changes(file, symbol_changes);
    }
}

impl Resolver<'_> {
    fn targets(&self, module: usize, specifier: &str) -> Vec<RepoPath> {
        self.modules[module]
            .targets
            .get(specifier)
            .cloned()
            .unwrap_or_default()
    }

    fn resolved_uses(&mut self, module: usize) -> Vec<ResolvedUse> {
        let symbols = self.modules[module].symbols;
        let mut resolved_uses = Vec::new();

        for (dependent, local_symbol) in symbols.symbols.iter().enumerate() {
            for symbol_use in &local_symbol.uses {
                let raw = match symbol_use {
                    Use::Local(_) => false,
                    Use::Import { binding, .. } => symbols.imports[*binding]
                        .specifier
                        .contains(RAW_IMPORT_QUERY),
                    Use::Module(specifier) | Use::Load(specifier) => {
                        specifier.contains(RAW_IMPORT_QUERY)
                    }
                };

                resolved_uses.extend(self.resolve_use(module, symbol_use).into_iter().map(
                    |reached| ResolvedUse {
                        dependent,
                        reached,
                        raw,
                    },
                ));
            }
        }

        for argument in &symbols.load_arguments {
            if self.keeps(module, &argument.callee, argument.index) {
                continue;
            }

            for symbol_use in &argument.uses {
                resolved_uses.extend(self.resolve_use(module, symbol_use).into_iter().map(
                    |reached| ResolvedUse {
                        dependent: ModuleSymbols::INIT,
                        reached,
                        raw: false,
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

    fn symbol_changes(
        &mut self,
        module: usize,
        base: &ModuleSymbols,
    ) -> Vec<(String, SymbolChangeKind, Vec<Reached>)> {
        let head = self.modules[module].symbols;
        let mut symbol_changes = Vec::new();

        for changed in module_changes(base, head) {
            match changed {
                Changed::Symbol { index, kind } => symbol_changes.push((
                    head.symbols[index].name.clone(),
                    kind,
                    vec![Reached::direct(Resolved::Symbol {
                        module,
                        symbol: index,
                    })],
                )),
                Changed::Removed(name) => {
                    symbol_changes.push((name, SymbolChangeKind::Removed, Vec::new()));
                }
                Changed::Export(name) => {
                    let reaches = self.resolve_name(module, &name);

                    symbol_changes.push((name, SymbolChangeKind::Export, reaches));
                }
                Changed::StarExport(specifier) => {
                    for target in self.targets(module, &specifier) {
                        let Some(&target_module) = self.module_indices.get(&target) else {
                            let reaches = vec![Reached::direct(Resolved::Whole {
                                file: target,
                                cause: WholeModuleCause::NotAnalysed,
                            })];

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
                            let reaches = self.resolve_name(module, &name);

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
                let symbols = self.modules[module].symbols;
                let passed = &symbols.symbols[symbol].parameters[parameter].passed;
                let still_kept = passed
                    .iter()
                    .all(|(callee, argument)| self.keeps(module, callee, *argument));

                if !still_kept {
                    self.kept_parameters
                        .get_mut(&(module, symbol))
                        .expect("kept parameters are listed")[parameter] = false;
                    changed = true;
                }
            }
        }
    }

    fn keeps(&mut self, module: usize, callee: &Use, argument: usize) -> bool {
        match self.resolve_use(module, callee).as_slice() {
            [
                Reached {
                    resolved: Resolved::Symbol { module, symbol },
                    ..
                },
            ] => self
                .kept_parameters
                .get(&(*module, *symbol))
                .and_then(|kept| kept.get(argument))
                .copied()
                .unwrap_or(false),
            _ => false,
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
            Use::Local(symbol) => vec![Reached::direct(Resolved::Symbol {
                module,
                symbol: *symbol,
            })],
            Use::Import { binding, member } => {
                let binding = &self.modules[module].symbols.imports[*binding];
                let mut reached = Vec::new();

                for imported in self.resolve_import(module, &binding.specifier, &binding.imported) {
                    let Resolved::Namespace(namespace) = imported.resolved else {
                        reached.push(imported);

                        continue;
                    };

                    match member {
                        Some(member) => {
                            let members = self.resolve_name(namespace, member);

                            reached.extend(members.into_iter().map(|member| {
                                let mut member = member.passing(namespace, &self.modules);

                                member
                                    .through
                                    .splice(0..0, imported.through.iter().copied());

                                member
                            }));
                        }
                        None => {
                            let cause = if binding.imported == Imported::Dynamic {
                                WholeModuleCause::DynamicLoad
                            } else {
                                WholeModuleCause::EscapingNamespace
                            };

                            reached.push(Reached {
                                resolved: Resolved::Whole {
                                    file: self.modules[namespace].path.clone(),
                                    cause,
                                },
                                through: imported.through,
                            });
                        }
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
        let symbols = self.modules[module].symbols;
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
                    ExportTarget::Local(symbol) => {
                        resolved.push(Reached::direct(Resolved::Symbol {
                            module,
                            symbol: *symbol,
                        }))
                    }
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
