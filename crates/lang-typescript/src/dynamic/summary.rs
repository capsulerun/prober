//! What one module tells about dynamic loading: its functions, load and call sites, exports and import uses, as expressions independent of the syntax tree.

pub(crate) type FunctionIndex = usize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImportedName {
    Named(String),
    Default,
    Namespace,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Text(String),
    ModuleFile,
    ModuleDirectory,
    ModuleResolve,
    Concat(Vec<Expr>),
    Choice(Vec<Expr>),
    Conditional {
        comparison: Option<Comparison>,
        consequent: Box<Expr>,
        alternate: Box<Expr>,
    },
    Array(Vec<Expr>),
    Spread(Box<Expr>),
    Object(Vec<(String, Expr)>),
    ElementOf(Box<Expr>),
    Parameter {
        function: FunctionIndex,
        index: usize,
    },
    Function(FunctionIndex),
    Imported {
        specifier: String,
        name: ImportedName,
    },
    Global(String),
    Member {
        object: Box<Expr>,
        property: String,
    },
    Call {
        callee: Box<Expr>,
        arguments: Vec<Expr>,
    },
    New {
        callee: Box<Expr>,
        arguments: Vec<Expr>,
    },
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Comparison {
    pub left: Box<Expr>,
    pub right: Box<Expr>,
    pub equal: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Reach {
    Named {
        exported_as: Vec<String>,
        escapes: bool,
    },
    ParameterDefault {
        function: FunctionIndex,
        index: usize,
        escapes: bool,
    },
    Nested,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FunctionSummary {
    pub parent: Option<FunctionIndex>,
    pub parameter_defaults: Vec<Option<Expr>>,
    pub returns: Vec<Expr>,
    pub reach: Reach,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Action {
    Import(Expr),
    Worker(Expr),
    Call { callee: Expr, arguments: Vec<Expr> },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Effect {
    pub line: u32,
    pub unit: Option<FunctionIndex>,
    pub expression: String,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UseKind {
    Call,
    Exported(String),
    ReexportAll,
    Escape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportUse {
    pub specifier: String,
    pub name: ImportedName,
    pub kind: UseKind,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ModuleSummary {
    pub functions: Vec<FunctionSummary>,
    pub effects: Vec<Effect>,
    pub exports: Vec<(String, Expr)>,
    pub star_exports: Vec<String>,
    pub import_uses: Vec<ImportUse>,
}

impl ModuleSummary {
    pub fn export(&self, name: &str) -> Option<&Expr> {
        self.exports
            .iter()
            .find(|(exported_name, _)| exported_name == name)
            .map(|(_, value)| value)
    }

    pub fn unit_of(&self, function: FunctionIndex) -> Option<FunctionIndex> {
        let mut current = Some(function);

        while let Some(index) = current {
            if self.functions[index].reach != Reach::Nested {
                return Some(index);
            }

            current = self.functions[index].parent;
        }

        None
    }
}
