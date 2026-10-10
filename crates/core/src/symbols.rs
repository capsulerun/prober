use std::collections::{HashMap, HashSet};

use crate::files::FileId;
use crate::graph::EdgeKind;

pub const MODULE_INIT_NAME: &str = "<module>";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SymbolId(u32);

impl SymbolId {
    fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolKind {
    Function,
    Class,
    Variable,
    ModuleInit,
    Member,
    Call,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

impl LineRange {
    pub fn contains(self, line: u32) -> bool {
        self.start <= line && line <= self.end
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub file: FileId,
    pub name: String,
    pub kind: SymbolKind,
    pub lines: Vec<LineRange>,
    pub hash: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WholeModuleCause {
    NotAnalysed,
    EscapingNamespace,
    DynamicLoad,
    MissingExport,
    Unattributed,
}

impl WholeModuleCause {
    pub fn key(self) -> &'static str {
        match self {
            WholeModuleCause::NotAnalysed => "not_analysed",
            WholeModuleCause::EscapingNamespace => "escaping_namespace",
            WholeModuleCause::DynamicLoad => "dynamic_load",
            WholeModuleCause::MissingExport => "missing_export",
            WholeModuleCause::Unattributed => "unattributed",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            WholeModuleCause::NotAnalysed => "module analysed at file level",
            WholeModuleCause::EscapingNamespace => "namespace object used as a value",
            WholeModuleCause::DynamicLoad => "loaded with import() or require()",
            WholeModuleCause::MissingExport => "imported name not found",
            WholeModuleCause::Unattributed => "import no symbol uses",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolTarget {
    Symbol(SymbolId),
    Module {
        file: FileId,
        cause: WholeModuleCause,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SymbolEdge {
    pub dependent: SymbolId,
    pub dependency: SymbolTarget,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    pub owner: SymbolId,
    pub key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FallbackCause {
    CommonJs,
    ExportAssignment,
    Eval,
    ParseError,
}

impl FallbackCause {
    pub fn key(self) -> &'static str {
        match self {
            FallbackCause::CommonJs => "commonjs",
            FallbackCause::ExportAssignment => "export_assignment",
            FallbackCause::Eval => "eval",
            FallbackCause::ParseError => "parse_error",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            FallbackCause::CommonJs => "CommonJS exports",
            FallbackCause::ExportAssignment => "export =",
            FallbackCause::Eval => "direct eval",
            FallbackCause::ParseError => "parse error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleFallback {
    pub file: FileId,
    pub cause: FallbackCause,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolChangeKind {
    Modified,
    Added,
    Renamed { from: String },
    Removed,
    Export,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolChange {
    pub file: FileId,
    pub name: String,
    pub kind: SymbolChangeKind,
    pub reaches: Vec<SymbolTarget>,
}

#[derive(Debug, Default)]
pub(crate) struct SymbolTableBuilder {
    symbols: Vec<Symbol>,
    edges: Vec<SymbolEdge>,
    fallbacks: Vec<ModuleFallback>,
    changes: Vec<SymbolChange>,
    compared_modules: Vec<FileId>,
    through: HashMap<SymbolEdge, Vec<FileId>>,
    memberships: HashMap<SymbolId, Membership>,
    dispatch_edges: HashSet<SymbolEdge>,
}

impl SymbolTableBuilder {
    pub(crate) fn add_symbol(&mut self, symbol: Symbol) -> SymbolId {
        let symbol_id =
            SymbolId(u32::try_from(self.symbols.len()).expect("symbol count exceeds u32::MAX"));

        self.symbols.push(symbol);

        symbol_id
    }

    pub(crate) fn add_edge(&mut self, edge: SymbolEdge) {
        if edge.dependency != SymbolTarget::Symbol(edge.dependent) {
            self.edges.push(edge);
        }
    }

    pub(crate) fn add_edge_through(&mut self, edge: SymbolEdge, through: Vec<FileId>) {
        if !through.is_empty() {
            self.through.insert(edge, through);
        }

        self.add_edge(edge);
    }

    pub(crate) fn add_fallback(&mut self, fallback: ModuleFallback) {
        self.fallbacks.push(fallback);
    }

    pub(crate) fn add_membership(&mut self, member: SymbolId, membership: Membership) {
        self.memberships.insert(member, membership);
    }

    pub(crate) fn mark_dispatch(&mut self, edge: SymbolEdge) {
        self.dispatch_edges.insert(edge);
    }

    pub(crate) fn add_module_changes(&mut self, file: FileId, changes: Vec<SymbolChange>) {
        self.compared_modules.push(file);
        self.changes.extend(changes);
    }

    pub(crate) fn retain_edges(&mut self, mut keep: impl FnMut(FileId, FileId) -> bool) {
        let symbols = &self.symbols;

        self.edges.retain(|edge| {
            let dependency_file = match edge.dependency {
                SymbolTarget::Symbol(dependency) => symbols[dependency.index()].file,
                SymbolTarget::Module { file, .. } => file,
            };

            keep(symbols[edge.dependent.index()].file, dependency_file)
        });
    }

    pub(crate) fn build(mut self) -> SymbolTable {
        let mut symbols_by_file: HashMap<FileId, Vec<SymbolId>> = HashMap::new();

        for (index, symbol) in self.symbols.iter().enumerate() {
            symbols_by_file
                .entry(symbol.file)
                .or_default()
                .push(SymbolId(index as u32));
        }

        self.edges.sort();
        self.edges.dedup();
        self.fallbacks.sort();
        self.fallbacks.dedup();

        let dependency_offsets = offsets(
            self.symbols.len(),
            self.edges.iter().map(|edge| edge.dependent.index()),
        );
        let mut dependents_by_symbol: Vec<Vec<u32>> = vec![Vec::new(); self.symbols.len()];
        let mut dependents_by_module: HashMap<FileId, Vec<u32>> = HashMap::new();

        for (edge_index, edge) in self.edges.iter().enumerate() {
            let edge_index = edge_index as u32;

            match edge.dependency {
                SymbolTarget::Symbol(dependency) => {
                    dependents_by_symbol[dependency.index()].push(edge_index);
                }
                SymbolTarget::Module { file, .. } => {
                    dependents_by_module
                        .entry(file)
                        .or_default()
                        .push(edge_index);
                }
            }
        }

        let mut changes_by_module: HashMap<FileId, Vec<usize>> = self
            .compared_modules
            .iter()
            .map(|&file| (file, Vec::new()))
            .collect();

        for (index, change) in self.changes.iter().enumerate() {
            changes_by_module
                .entry(change.file)
                .or_default()
                .push(index);
        }

        SymbolTable {
            symbols: self.symbols,
            symbols_by_file,
            edges: self.edges,
            dependency_offsets,
            dependents_by_symbol,
            dependents_by_module,
            fallbacks: self.fallbacks,
            changes: self.changes,
            changes_by_module,
            through: self.through,
            memberships: self.memberships,
            dispatch_edges: self.dispatch_edges,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct SymbolTable {
    symbols: Vec<Symbol>,
    symbols_by_file: HashMap<FileId, Vec<SymbolId>>,
    edges: Vec<SymbolEdge>,
    dependency_offsets: Vec<usize>,
    dependents_by_symbol: Vec<Vec<u32>>,
    dependents_by_module: HashMap<FileId, Vec<u32>>,
    fallbacks: Vec<ModuleFallback>,
    changes: Vec<SymbolChange>,
    changes_by_module: HashMap<FileId, Vec<usize>>,
    through: HashMap<SymbolEdge, Vec<FileId>>,
    memberships: HashMap<SymbolId, Membership>,
    dispatch_edges: HashSet<SymbolEdge>,
}

impl SymbolTable {
    pub(crate) fn symbol(&self, symbol: SymbolId) -> &Symbol {
        &self.symbols[symbol.index()]
    }

    pub(crate) fn len(&self) -> usize {
        self.symbols.len()
    }

    pub(crate) fn all(&self) -> &[Symbol] {
        &self.symbols
    }

    pub(crate) fn symbols_of(&self, file: FileId) -> &[SymbolId] {
        self.symbols_by_file.get(&file).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn analysed_module_count(&self) -> usize {
        self.symbols_by_file.len()
    }

    pub(crate) fn edges(&self) -> &[SymbolEdge] {
        &self.edges
    }

    pub(crate) fn dependencies(&self, symbol: SymbolId) -> &[SymbolEdge] {
        let index = symbol.index();

        &self.edges[self.dependency_offsets[index]..self.dependency_offsets[index + 1]]
    }

    pub(crate) fn dependents(&self, symbol: SymbolId) -> impl Iterator<Item = &SymbolEdge> {
        self.dependents_by_symbol[symbol.index()]
            .iter()
            .map(|&edge_index| &self.edges[edge_index as usize])
    }

    pub(crate) fn module_dependents(&self, file: FileId) -> impl Iterator<Item = &SymbolEdge> {
        self.dependents_by_module
            .get(&file)
            .into_iter()
            .flatten()
            .map(|&edge_index| &self.edges[edge_index as usize])
    }

    pub(crate) fn fallbacks(&self) -> &[ModuleFallback] {
        &self.fallbacks
    }

    pub(crate) fn through(&self, edge: &SymbolEdge) -> &[FileId] {
        self.through.get(edge).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn changes(&self) -> &[SymbolChange] {
        &self.changes
    }

    pub(crate) fn changes_of(&self, file: FileId) -> Option<&[usize]> {
        self.changes_by_module.get(&file).map(Vec::as_slice)
    }

    pub(crate) fn memberships(&self) -> impl Iterator<Item = (SymbolId, &Membership)> {
        self.memberships
            .iter()
            .map(|(&member, membership)| (member, membership))
    }

    pub(crate) fn is_dispatch(&self, edge: &SymbolEdge) -> bool {
        self.dispatch_edges.contains(edge)
    }

    pub(crate) fn dispatch_edges(&self) -> impl Iterator<Item = &SymbolEdge> {
        self.dispatch_edges.iter()
    }
}

fn offsets(count: usize, sorted_keys: impl Iterator<Item = usize>) -> Vec<usize> {
    let mut offsets = vec![0; count + 1];

    for key in sorted_keys {
        offsets[key + 1] += 1;
    }

    for index in 0..count {
        offsets[index + 1] += offsets[index];
    }

    offsets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::{FileTable, RepoPath};

    fn symbol(file: FileId, name: &str, kind: SymbolKind) -> Symbol {
        Symbol {
            file,
            name: name.to_owned(),
            kind,
            lines: vec![LineRange { start: 1, end: 1 }],
            hash: 0,
        }
    }

    #[test]
    fn edges_are_indexed_by_dependent_dependency_and_whole_module() {
        let mut files = FileTable::default();
        let server = files.intern(RepoPath::new("src/server.ts").unwrap());
        let channel = files.intern(RepoPath::new("src/channel.ts").unwrap());
        let legacy = files.intern(RepoPath::new("src/legacy.cjs").unwrap());
        let mut builder = SymbolTableBuilder::default();
        let server_init =
            builder.add_symbol(symbol(server, MODULE_INIT_NAME, SymbolKind::ModuleInit));
        let start = builder.add_symbol(symbol(server, "start", SymbolKind::Function));
        let create = builder.add_symbol(symbol(channel, "createChannel", SymbolKind::Function));
        let uses = [
            (start, SymbolTarget::Symbol(create)),
            (start, SymbolTarget::Symbol(start)),
            (server_init, SymbolTarget::Symbol(start)),
            (
                start,
                SymbolTarget::Module {
                    file: legacy,
                    cause: WholeModuleCause::NotAnalysed,
                },
            ),
            (start, SymbolTarget::Symbol(create)),
        ];

        for (dependent, dependency) in uses {
            builder.add_edge(SymbolEdge {
                dependent,
                dependency,
                kind: EdgeKind::Import,
            });
        }

        let table = builder.build();
        let dependencies: Vec<SymbolTarget> = table
            .dependencies(start)
            .iter()
            .map(|edge| edge.dependency)
            .collect();

        assert_eq!(
            dependencies,
            [
                SymbolTarget::Symbol(create),
                SymbolTarget::Module {
                    file: legacy,
                    cause: WholeModuleCause::NotAnalysed,
                },
            ]
        );
        assert_eq!(table.dependencies(create), []);
        assert_eq!(
            table
                .dependents(start)
                .map(|edge| edge.dependent)
                .collect::<Vec<_>>(),
            [server_init]
        );
        assert_eq!(
            table
                .module_dependents(legacy)
                .map(|edge| edge.dependent)
                .collect::<Vec<_>>(),
            [start]
        );
        assert_eq!(table.symbols_of(server), [server_init, start]);
        assert_eq!(table.symbols_of(legacy), []);
        assert_eq!(table.analysed_module_count(), 2);
    }

    #[test]
    fn line_ranges_include_both_ends() {
        let range = LineRange { start: 3, end: 5 };

        assert!(range.contains(3));
        assert!(range.contains(5));
        assert!(!range.contains(2));
        assert!(!range.contains(6));
    }
}
