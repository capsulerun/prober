mod extractor;
mod fingerprint;
mod parameters;
mod resolution;

use std::collections::HashMap;

use prober_core::files::RepoPath;
use prober_core::symbols::{FallbackCause, LineRange, SymbolKind};

pub(crate) use extractor::extract_symbols;
pub(crate) use resolution::add_symbols;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Use {
    Local(usize),
    Import {
        binding: usize,
        member: Option<String>,
    },
    Module(String),
    Load(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Parameter {
    pub kept: bool,
    pub passed: Vec<(Use, usize)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub lines: Vec<LineRange>,
    pub hash: u64,
    pub uses: Vec<Use>,
    pub parameters: Vec<Parameter>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoadArgument {
    pub callee: Use,
    pub index: usize,
    pub uses: Vec<Use>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    pub load_arguments: Vec<LoadArgument>,
}

impl ModuleSymbols {
    pub(crate) const INIT: usize = 0;

    pub(crate) fn specifiers(&self) -> impl Iterator<Item = &str> {
        let used_modules = self
            .symbols
            .iter()
            .flat_map(|symbol| &symbol.uses)
            .chain(
                self.load_arguments
                    .iter()
                    .flat_map(|argument| &argument.uses),
            )
            .filter_map(|symbol_use| match symbol_use {
                Use::Module(specifier) | Use::Load(specifier) => Some(specifier.as_str()),
                Use::Local(_) | Use::Import { .. } => None,
            });
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
