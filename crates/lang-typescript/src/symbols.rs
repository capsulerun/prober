mod changes;
mod extractor;
mod fingerprint;
mod parameters;
mod placement;
mod resolution;

use std::collections::HashMap;

use prober_core::files::RepoPath;
use prober_core::symbols::{FallbackCause, LineRange, SymbolKind};

pub(crate) use extractor::extract_symbols;
pub(crate) use resolution::add_symbols;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Place {
    Own,
    Instance,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Use {
    Local(usize),
    Import {
        binding: usize,
        member: Option<String>,
    },
    Module(String),
    Load(String),
    Member {
        symbol: usize,
        member: String,
    },
    This {
        class: usize,
        member: String,
        place: Place,
    },
    Super {
        class: usize,
        member: String,
        place: Place,
    },
    Call(Box<Use>),
    New(Box<Use>),
    Keyed(Box<Use>),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Keeping {
    pub callee: Use,
    pub index: usize,
    pub fresh_target: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Parameter {
    pub kept: bool,
    pub passed: Vec<Keeping>,
    pub rest: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Held {
    pub kept_by: Vec<Keeping>,
    pub hash: u64,
    pub uses: Vec<Use>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Runs {
    pub lines: Vec<LineRange>,
    pub hash: u64,
    pub uses: Vec<Use>,
    pub held: Vec<Held>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Membership {
    pub owner: usize,
    pub key: String,
    pub place: Place,
    pub assigns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Extends {
    Nothing,
    Binding(Use),
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Shape {
    Object { open: bool },
    Class { extends: Extends },
    Instance(Use),
    Assign,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub lines: Vec<LineRange>,
    pub hash: u64,
    pub uses: Vec<Use>,
    pub parameters: Vec<Parameter>,
    pub member: Option<Membership>,
    pub shape: Option<Shape>,
    pub runs: Option<Runs>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Imported {
    Name(String),
    Namespace,
    Dynamic,
    Module,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportBinding {
    pub specifier: String,
    pub imported: Imported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExportTarget {
    Local(usize),
    Import(usize),
    Reexport { specifier: String, name: String },
    Namespace(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModuleSymbols {
    pub symbols: Vec<LocalSymbol>,
    pub imports: Vec<ImportBinding>,
    pub exports: Vec<(String, ExportTarget)>,
    pub star_exports: Vec<String>,
    pub loads: Vec<String>,
    pub held: Vec<Held>,
    pub commands: Vec<String>,
}

impl Use {
    fn specifier(&self) -> Option<&str> {
        match self {
            Use::Module(specifier) | Use::Load(specifier) => Some(specifier),
            Use::Call(called) | Use::New(called) | Use::Keyed(called) => called.specifier(),
            Use::Local(_)
            | Use::Import { .. }
            | Use::Member { .. }
            | Use::This { .. }
            | Use::Super { .. } => None,
        }
    }
}

impl ModuleSymbols {
    pub(crate) const INIT: usize = 0;

    pub(crate) fn specifiers(&self) -> impl Iterator<Item = &str> {
        let used_modules = self
            .symbols
            .iter()
            .flat_map(|symbol| {
                let runs = symbol.runs.iter().flat_map(|runs| {
                    runs.uses
                        .iter()
                        .chain(runs.held.iter().flat_map(|held| &held.uses))
                });

                symbol.uses.iter().chain(runs)
            })
            .chain(self.held.iter().flat_map(|held| &held.uses))
            .filter_map(Use::specifier);
        let reexported = self.exports.iter().filter_map(|(_, target)| match target {
            ExportTarget::Reexport { specifier, .. } | ExportTarget::Namespace(specifier) => {
                Some(specifier.as_str())
            }
            ExportTarget::Local(_) | ExportTarget::Import(_) => None,
        });

        self.imports
            .iter()
            .map(|import| import.specifier.as_str())
            .chain(self.star_exports.iter().map(String::as_str))
            .chain(self.loads.iter().map(String::as_str))
            .chain(reexported)
            .chain(used_modules)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SymbolAnalysis {
    pub module: Result<ModuleSymbols, FallbackCause>,
    pub targets: HashMap<String, Vec<RepoPath>>,
}
