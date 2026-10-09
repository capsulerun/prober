//! Links the symbols of every analysed module: imported names followed to the symbols exporting them through re-exports, aliases and barrels, module loads, and whole-module uses where a name cannot be followed.

use std::collections::{BTreeSet, HashMap, HashSet};

use prober_core::files::{FileId, RepoPath};
use prober_core::graph::{EdgeKind, GraphBuilder};
use prober_core::symbols::{
    ModuleFallback, Symbol, SymbolEdge, SymbolId, SymbolTarget, WholeModuleCause,
};

use crate::conventions::is_asset_file;
use crate::linker::{Dependency, LinkedFile, RAW_IMPORT_QUERY};
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

struct ResolvedUse {
    dependent: usize,
    resolved: Resolved,
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
    exports: HashMap<(usize, String), Vec<Resolved>>,
    in_progress: HashSet<(usize, String)>,
    kept_parameters: HashMap<(usize, usize), Vec<bool>>,
}

pub(crate) fn add_symbols(linked_files: &[LinkedFile], graph: &mut GraphBuilder) {
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

    resolver.solve_kept_parameters();

    for module in 0..resolver.modules.len() {
        let mut edges = BTreeSet::new();
        let mut covered_files = HashSet::new();

        for resolved_use in resolver.resolved_uses(module) {
            let (dependency, file, path) = match resolved_use.resolved {
                Resolved::Symbol { module, symbol } => (
                    SymbolTarget::Symbol(symbol_ids[module][symbol]),
                    module_files[module],
                    None,
                ),
                Resolved::Namespace(module) => (
                    SymbolTarget::Module {
                        file: module_files[module],
                        cause: WholeModuleCause::EscapingNamespace,
                    },
                    module_files[module],
                    None,
                ),
                Resolved::Whole { file: path, cause } => {
                    let file = graph.add_file(path.clone());

                    (SymbolTarget::Module { file, cause }, file, Some(path))
                }
            };
            let is_asset = path.is_some_and(|path| is_asset_file(&path));
            let kind = if is_asset && !resolved_use.raw {
                EdgeKind::Asset
            } else {
                EdgeKind::Import
            };

            covered_files.insert(file);
            edges.insert(SymbolEdge {
                dependent: symbol_ids[module][resolved_use.dependent],
                dependency,
                kind,
            });
        }

        let init = symbol_ids[module][ModuleSymbols::INIT];

        for dependency in resolver.modules[module].dependencies {
            if !matches!(dependency.kind, EdgeKind::Import | EdgeKind::Asset) {
                continue;
            }

            let file = graph.add_file(dependency.path.clone());

            if covered_files.insert(file) {
                edges.insert(SymbolEdge {
                    dependent: init,
                    dependency: SymbolTarget::Module {
                        file,
                        cause: WholeModuleCause::Unattributed,
                    },
                    kind: dependency.kind,
                });
            }
        }

        for edge in edges {
            graph.add_symbol_edge(edge);
        }
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
                    |resolved| ResolvedUse {
                        dependent,
                        resolved,
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
                    |resolved| ResolvedUse {
                        dependent: ModuleSymbols::INIT,
                        resolved,
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
                    resolved,
                    raw,
                }
            }));
        }

        resolved_uses
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
                let passed =
                    &self.modules[module].symbols.symbols[symbol].parameters[parameter].passed;
                let still_kept = passed
                    .clone()
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
            [Resolved::Symbol { module, symbol }] => self
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

    fn resolve_use(&mut self, module: usize, symbol_use: &Use) -> Vec<Resolved> {
        match symbol_use {
            Use::Local(symbol) => vec![Resolved::Symbol {
                module,
                symbol: *symbol,
            }],
            Use::Import { binding, member } => {
                let binding = &self.modules[module].symbols.imports[*binding];
                let mut resolved = Vec::new();

                for imported in self.resolve_import(module, &binding.specifier, &binding.imported) {
                    match (imported, member) {
                        (Resolved::Namespace(namespace), Some(member)) => {
                            resolved.extend(self.resolve_name(namespace, member));
                        }
                        (Resolved::Namespace(namespace), None) => {
                            let cause = if binding.imported == Imported::Dynamic {
                                WholeModuleCause::DynamicLoad
                            } else {
                                WholeModuleCause::EscapingNamespace
                            };

                            resolved.push(Resolved::Whole {
                                file: self.modules[namespace].path.clone(),
                                cause,
                            });
                        }
                        (imported, _) => resolved.push(imported),
                    }
                }

                resolved
            }
            Use::Module(specifier) => self
                .targets(module, specifier)
                .into_iter()
                .map(|file| Resolved::Whole {
                    file,
                    cause: WholeModuleCause::DynamicLoad,
                })
                .collect(),
            Use::Load(specifier) => self.load(module, specifier),
        }
    }

    fn resolve_import(
        &mut self,
        module: usize,
        specifier: &str,
        imported: &Imported,
    ) -> Vec<Resolved> {
        let mut resolved = Vec::new();

        for target in self.targets(module, specifier) {
            let Some(&target_module) = self.module_indices.get(&target) else {
                resolved.push(Resolved::Whole {
                    file: target,
                    cause: WholeModuleCause::NotAnalysed,
                });

                continue;
            };

            match imported {
                Imported::Namespace | Imported::Dynamic => {
                    resolved.push(Resolved::Namespace(target_module));
                }
                Imported::Module => resolved.push(Resolved::Whole {
                    file: target,
                    cause: WholeModuleCause::DynamicLoad,
                }),
                Imported::Name(name) => resolved.extend(self.resolve_name(target_module, name)),
            }
        }

        resolved
    }

    fn resolve_name(&mut self, module: usize, name: &str) -> Vec<Resolved> {
        let exported = self.resolve_export(module, name);

        if exported.is_empty() {
            return vec![Resolved::Whole {
                file: self.modules[module].path.clone(),
                cause: WholeModuleCause::MissingExport,
            }];
        }

        exported
    }

    fn resolve_export(&mut self, module: usize, name: &str) -> Vec<Resolved> {
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

    fn find_export(&mut self, module: usize, name: &str) -> Vec<Resolved> {
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
                    ExportTarget::Local(symbol) => resolved.push(Resolved::Symbol {
                        module,
                        symbol: *symbol,
                    }),
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
                    Some(&target_module) => found.extend(self.resolve_export(target_module, name)),
                    None => not_analysed.push(Resolved::Whole {
                        file: target,
                        cause: WholeModuleCause::NotAnalysed,
                    }),
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
