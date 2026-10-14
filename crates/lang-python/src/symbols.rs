//! Python modules per function: the symbols each module defines, the names they use and the modules they load, linked across modules so a change selects the tests using what changed.

mod changes;
mod extractor;
mod resolution;

use std::collections::{BTreeMap, BTreeSet};

use prober_core::files::RepoPath;
use prober_core::symbols::{LineRange, SymbolKind};

pub(crate) use extractor::extract_symbols;
pub(crate) use resolution::{AnalysedModule, Analysis};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Target {
    Repo(RepoPath),
    Standard(String),
    External(String),
    Native,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Imported {
    Module(Target),
    Name { module: Target, name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Binding {
    Local(usize),
    Import(Imported),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Use {
    pub name: String,
    pub attributes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decoration {
    Plain,
    Fixture,
    Registering,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClassHeader {
    pub bases: Vec<Option<Use>>,
    pub keywords: bool,
    pub init_subclass: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Member {
    pub class: usize,
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Attributes {
    pub read: BTreeSet<String>,
    pub read_on_self: BTreeSet<String>,
    pub written: BTreeSet<String>,
    pub computed: bool,
    pub computed_on_self: bool,
    pub computed_on_argument: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Namespace {
    pub reads: bool,
    pub writes: bool,
    pub writes_through_class: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub lines: Vec<LineRange>,
    pub hash: u64,
    pub uses: BTreeSet<Use>,
    pub imports: BTreeMap<String, Vec<Imported>>,
    pub attributes: Attributes,
    pub namespace: Namespace,
    pub decoration: Decoration,
    pub decorators: Vec<Option<Use>>,
    pub stores_argument: bool,
    pub calls_at_load: bool,
    pub class: Option<ClassHeader>,
    pub member: Option<Member>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ModuleSymbols {
    pub symbols: Vec<LocalSymbol>,
    pub names: BTreeMap<String, Vec<Binding>>,
    pub star_imports: Vec<Target>,
}

impl ModuleSymbols {
    pub(crate) const INIT: usize = 0;

    pub(crate) fn symbol_at(&self, line: u32) -> usize {
        self.symbols
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, symbol)| symbol.lines.iter().any(|lines| lines.contains(line)))
            .map_or(Self::INIT, |(index, _)| index)
    }
}
