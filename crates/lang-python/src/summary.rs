//! What one module tells about imports, computed imports and file access, as expressions independent of the syntax tree.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use prober_core::files::RepoPath;
use prober_core::symbols::FallbackCause;

use crate::modules::ImportTarget;
use crate::pytest::items::PytestItems;
use crate::symbols::ModuleSymbols;

pub(crate) type FunctionIndex = usize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModuleRef {
    Repo(RepoPath),
    Named(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Text(String),
    ModuleFile,
    ModuleName,
    PackageName,
    Concat(Vec<Expr>),
    Choice(Vec<Expr>),
    Sequence(Vec<Expr>),
    ElementOf(Box<Expr>),
    ItemOf {
        iterable: Box<Expr>,
        index: usize,
    },
    Parameter {
        function: FunctionIndex,
        index: usize,
    },
    Function(FunctionIndex),
    Method {
        class: String,
        name: String,
    },
    Module(ModuleRef),
    Imported {
        module: ModuleRef,
        name: String,
    },
    Attribute {
        object: Box<Expr>,
        name: String,
    },
    Call {
        callee: Box<Expr>,
        arguments: Vec<Expr>,
        keywords: Vec<(String, Expr)>,
    },
    Join(Box<Expr>, Box<Expr>),
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FunctionSummary {
    pub name: String,
    pub class: Option<String>,
    pub nested: bool,
    pub parameters: Vec<String>,
    pub defaults: Vec<Option<Expr>>,
    pub returns: Vec<Expr>,
    pub escapes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SiteKind {
    Import,
    Path,
    Listing,
    Attribute,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Site {
    pub line: u32,
    pub unit: Option<FunctionIndex>,
    pub expression: String,
    pub kind: SiteKind,
    pub value: Expr,
    pub context: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CallSite {
    pub line: u32,
    pub unit: Option<FunctionIndex>,
    pub callee: Expr,
    pub arguments: Vec<Expr>,
    pub keywords: Vec<(String, Expr)>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StaticImport {
    pub line: u32,
    pub target: ImportTarget,
    pub type_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NativeImport {
    pub line: u32,
    pub module: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ModuleSummary {
    pub imports: Vec<StaticImport>,
    pub native_imports: Vec<NativeImport>,
    pub functions: Vec<FunctionSummary>,
    pub bindings: HashMap<String, Expr>,
    pub sites: Vec<Site>,
    pub calls: Vec<CallSite>,
    pub method_calls: Vec<(String, CallSite)>,
    pub referenced_attributes: BTreeSet<String>,
    pub escapes: Vec<Expr>,
    pub unresolved_attributes: BTreeSet<String>,
    pub strings: Vec<(u32, String)>,
    pub pytest: PytestItems,
    pub setting_lists: BTreeMap<String, Vec<String>>,
    pub symbols: Option<Result<ModuleSymbols, FallbackCause>>,
}

impl ModuleSummary {
    pub fn function_named(&self, name: &str, class: Option<&str>) -> Option<FunctionIndex> {
        self.functions.iter().position(|function| {
            !function.nested && function.name == name && function.class.as_deref() == class
        })
    }
}
