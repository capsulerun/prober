use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use crate::files::{FileId, FileTable, RepoPath};
use crate::patterns::Captures;
use crate::symbols::{
    FallbackCause, Membership, ModuleFallback, Symbol, SymbolChange, SymbolEdge, SymbolId,
    SymbolKind, SymbolTable, SymbolTableBuilder, SymbolTarget,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EdgeKind {
    Import,
    TypeOnly,
    Dynamic,
    Contains,
    Asset,
    Reads,
    Lists,
    Family,
    Spawn,
    Build,
    Binding,
    Rule,
}

impl EdgeKind {
    pub fn selects_tests(self) -> bool {
        match self {
            EdgeKind::Import
            | EdgeKind::Dynamic
            | EdgeKind::Contains
            | EdgeKind::Asset
            | EdgeKind::Reads
            | EdgeKind::Lists
            | EdgeKind::Family
            | EdgeKind::Spawn
            | EdgeKind::Build
            | EdgeKind::Binding
            | EdgeKind::Rule => true,
            EdgeKind::TypeOnly => false,
        }
    }

    pub fn follows(self, dependency_content: ContentChange) -> bool {
        match self {
            EdgeKind::Reads | EdgeKind::Family => dependency_content != ContentChange::None,
            EdgeKind::Lists => dependency_content == ContentChange::Direct,
            _ => true,
        }
    }

    pub fn dependent_content(self) -> ContentChange {
        if self == EdgeKind::Lists {
            ContentChange::Listed
        } else {
            ContentChange::None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContentChange {
    None,
    Listed,
    Direct,
}

impl ContentChange {
    pub const ALL: [ContentChange; 3] = [
        ContentChange::None,
        ContentChange::Listed,
        ContentChange::Direct,
    ];
}

pub type ModuleFilter = fn(&RepoPath) -> bool;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Inference {
    pub line: u32,
    pub kind: &'static str,
    pub pattern: String,
    pub detail: Option<String>,
}

impl Inference {
    pub fn description(&self) -> String {
        let Inference {
            kind,
            pattern,
            detail,
            ..
        } = self;

        match detail {
            Some(detail) => format!("inferred {kind} {pattern} ({detail})"),
            None => format!("inferred {kind} {pattern}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UnresolvedCause {
    InferenceDisabled,
    Parameter,
    RuntimeValue,
    TooWide,
    NoMatchingFile,
}

impl UnresolvedCause {
    pub fn widens(self) -> bool {
        self != UnresolvedCause::NoMatchingFile
    }

    pub fn key(self) -> &'static str {
        match self {
            UnresolvedCause::InferenceDisabled => "inference_disabled",
            UnresolvedCause::Parameter => "parameter",
            UnresolvedCause::RuntimeValue => "runtime_value",
            UnresolvedCause::TooWide => "too_wide",
            UnresolvedCause::NoMatchingFile => "no_matching_file",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            UnresolvedCause::InferenceDisabled => "inference is disabled",
            UnresolvedCause::Parameter => "comes from a function parameter",
            UnresolvedCause::RuntimeValue => "is computed at runtime",
            UnresolvedCause::TooWide => "has no literal directory or file name",
            UnresolvedCause::NoMatchingFile => "matches no file of the repository",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedSite {
    pub file: FileId,
    pub line: u32,
    pub expression: String,
    pub cause: UnresolvedCause,
    pub language: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetAsideSite {
    pub site: UnresolvedSite,
    pub line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IgnoredEdge {
    pub edge: Edge,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleLink {
    pub line: usize,
    pub pattern: String,
    pub captures: Captures,
    pub from: Vec<FileId>,
    pub to: Vec<FileId>,
}

impl RuleLink {
    fn link_count(
        &self,
        from: impl Fn(FileId) -> bool,
        to: impl Fn(FileId) -> bool,
        is_test: impl Fn(FileId) -> bool,
    ) -> usize {
        let from_files: HashSet<FileId> = self
            .from
            .iter()
            .copied()
            .filter(|&file| from(file))
            .collect();
        let to_files: Vec<FileId> = self.to.iter().copied().filter(|&file| to(file)).collect();
        let self_links = to_files
            .iter()
            .filter(|file| from_files.contains(file))
            .count();
        let tests_linked = from_files.iter().filter(|&&file| is_test(file)).count();
        let product_targets = to_files.iter().filter(|&&file| !is_test(file)).count();

        from_files.len() * to_files.len() - tests_linked * product_targets - self_links
    }
}

#[derive(Debug)]
struct RuleIgnore {
    line: usize,
    from: HashSet<FileId>,
    to: HashSet<FileId>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct IgnoredLinks {
    pub links: usize,
    pub imports: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootScope {
    pub directory: String,
    pub configs: Vec<RepoPath>,
    pub files: Vec<RepoPath>,
    pub packages: usize,
    pub tests: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedPart {
    pub change_path: RepoPath,
    pub node: FileId,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    pub dependent: FileId,
    pub dependency: FileId,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestFile {
    pub file: FileId,
    pub language: &'static str,
}

#[derive(Debug, Default)]
pub struct GraphBuilder {
    files: FileTable,
    test_languages: HashMap<FileId, &'static str>,
    target_names: HashMap<FileId, TargetName>,
    removed_tests: HashSet<FileId>,
    edges: Vec<Edge>,
    inferences: HashMap<(FileId, FileId, EdgeKind), Inference>,
    unresolved_sites: Vec<UnresolvedSite>,
    changed_parts: Vec<ChangedPart>,
    mocked_modules: HashMap<FileId, Vec<FileId>>,
    changed_contracts: HashSet<FileId>,
    asset_blind_tests: HashSet<FileId>,
    module_filters: HashMap<&'static str, ModuleFilter>,
    test_support_names: HashMap<&'static str, ModuleFilter>,
    ignored_edges: Vec<IgnoredEdge>,
    set_aside_sites: Vec<SetAsideSite>,
    root_scopes: Vec<RootScope>,
    rule_links: Vec<RuleLink>,
    rule_ignores: Vec<RuleIgnore>,
    symbols: SymbolTableBuilder,
    commands: HashMap<FileId, Vec<String>>,
}

impl GraphBuilder {
    pub fn add_file(&mut self, path: RepoPath) -> FileId {
        self.files.intern(path)
    }

    pub fn add_test_file(&mut self, path: RepoPath, language: &'static str) -> FileId {
        let test_file = self.files.intern(path);

        self.test_languages.insert(test_file, language);

        test_file
    }

    pub fn name_target(&mut self, file: FileId, unit: &'static str, name: String) {
        self.target_names.insert(file, TargetName { unit, name });
    }

    pub fn add_removed_test(&mut self, path: RepoPath) {
        let removed_test = self.files.intern(path);

        self.removed_tests.insert(removed_test);
    }

    pub fn add_edge(&mut self, edge: Edge) {
        self.edges.push(edge);
    }

    pub fn add_inferred_edge(&mut self, edge: Edge, inference: Inference) {
        match self
            .inferences
            .entry((edge.dependent, edge.dependency, edge.kind))
        {
            Entry::Occupied(mut entry) if inference < *entry.get() => {
                entry.insert(inference);
            }
            Entry::Occupied(_) => {}
            Entry::Vacant(entry) => {
                entry.insert(inference);
            }
        }

        self.edges.push(edge);
    }

    pub fn add_unresolved_site(&mut self, unresolved_site: UnresolvedSite) {
        self.unresolved_sites.push(unresolved_site);
    }

    pub fn add_changed_part(&mut self, change_path: RepoPath, node: RepoPath, description: String) {
        let node = self.files.intern(node);

        self.changed_parts.push(ChangedPart {
            change_path,
            node,
            description,
        });
    }

    pub fn add_mock(&mut self, test: FileId, mocked_module: FileId) {
        self.mocked_modules
            .entry(test)
            .or_default()
            .push(mocked_module);
    }

    pub fn mark_changed_contract(&mut self, module: FileId) {
        self.changed_contracts.insert(module);
    }

    pub fn mark_asset_blind(&mut self, test: FileId) {
        self.asset_blind_tests.insert(test);
    }

    pub fn add_language(&mut self, language: &'static str, loads: ModuleFilter) {
        self.module_filters.insert(language, loads);
    }

    pub fn add_test_support(&mut self, language: &'static str, names_test_support: ModuleFilter) {
        self.test_support_names.insert(language, names_test_support);
    }

    pub fn add_root_scope(&mut self, root_scope: RootScope) {
        self.root_scopes.push(root_scope);
    }

    pub fn add_rule_link(&mut self, mut rule_link: RuleLink) -> usize {
        rule_link.from.sort();
        rule_link.from.dedup();
        rule_link.to.sort();
        rule_link.to.dedup();

        let links = rule_link.link_count(|_| true, |_| true, |file| self.is_or_was_test(file));

        self.rule_links.push(rule_link);

        links
    }

    pub fn add_symbol(&mut self, symbol: Symbol) -> SymbolId {
        self.symbols.add_symbol(symbol)
    }

    pub fn add_symbol_edge(&mut self, edge: SymbolEdge) {
        self.symbols.add_edge(edge);
    }

    pub fn add_symbol_edge_through(&mut self, edge: SymbolEdge, through: Vec<FileId>) {
        self.symbols.add_edge_through(edge, through);
    }

    pub fn add_fallback(&mut self, fallback: ModuleFallback) {
        self.symbols.add_fallback(fallback);
    }

    pub fn add_module_changes(&mut self, file: FileId, changes: Vec<SymbolChange>) {
        self.symbols.add_module_changes(file, changes);
    }

    pub fn add_membership(&mut self, member: SymbolId, membership: Membership) {
        self.symbols.add_membership(member, membership);
    }

    pub fn mark_dispatch(&mut self, edge: SymbolEdge) {
        self.symbols.mark_dispatch(edge);
    }

    pub fn add_commands(&mut self, file: FileId, commands: Vec<String>) {
        if !commands.is_empty() {
            self.commands.insert(file, commands);
        }
    }

    pub(crate) fn files(&self) -> impl Iterator<Item = (FileId, &RepoPath)> {
        self.files.iter()
    }

    pub(crate) fn is_test(&self, file: FileId) -> bool {
        self.test_languages.contains_key(&file)
    }

    fn is_or_was_test(&self, file: FileId) -> bool {
        self.is_test(file) || self.removed_tests.contains(&file)
    }

    pub(crate) fn ignore_links(
        &mut self,
        line: usize,
        from: HashSet<FileId>,
        to: HashSet<FileId>,
    ) -> IgnoredLinks {
        let inferences = &self.inferences;
        let is_import = |edge: &Edge| {
            matches!(
                edge.kind,
                EdgeKind::Import | EdgeKind::Dynamic | EdgeKind::Asset
            ) && !inferences.contains_key(&(edge.dependent, edge.dependency, edge.kind))
        };
        let (ignored, kept): (Vec<Edge>, Vec<Edge>) = std::mem::take(&mut self.edges)
            .into_iter()
            .partition(|edge| to.contains(&edge.dependent) && from.contains(&edge.dependency));
        let imports = ignored.iter().filter(|edge| is_import(edge)).count();
        let test_languages = &self.test_languages;
        let removed_tests = &self.removed_tests;
        let rule_links: usize = self
            .rule_links
            .iter()
            .map(|rule_link| {
                rule_link.link_count(
                    |file| from.contains(&file),
                    |file| to.contains(&file),
                    |file| test_languages.contains_key(&file) || removed_tests.contains(&file),
                )
            })
            .sum();
        let ignored_links = IgnoredLinks {
            links: ignored.len() + rule_links,
            imports,
        };

        self.edges = kept;
        self.ignored_edges
            .extend(ignored.into_iter().map(|edge| IgnoredEdge { edge, line }));
        self.symbols.retain_edges(|dependent, dependency| {
            !(to.contains(&dependent) && from.contains(&dependency))
        });

        if rule_links > 0 {
            self.rule_ignores.push(RuleIgnore { line, from, to });
        }

        ignored_links
    }

    pub(crate) fn set_aside_sites(
        &mut self,
        line: usize,
        matches: impl Fn(&UnresolvedSite, &RepoPath) -> bool,
    ) -> usize {
        let files = &self.files;
        let (matching, kept): (Vec<UnresolvedSite>, Vec<UnresolvedSite>) =
            std::mem::take(&mut self.unresolved_sites)
                .into_iter()
                .partition(|site| site.cause.widens() && matches(site, files.path(site.file)));
        let set_aside_count = matching.len();

        self.unresolved_sites = kept;
        self.set_aside_sites
            .extend(matching.into_iter().map(|site| SetAsideSite { site, line }));

        set_aside_count
    }

    pub fn build(mut self) -> Graph {
        let mut dependents_by_file = vec![Vec::new(); self.files.len()];
        let mut dependencies_by_file = vec![Vec::new(); self.files.len()];
        let mut ignored_dependents: HashMap<FileId, Vec<IgnoredEdge>> = HashMap::new();

        for edge in self.edges {
            dependents_by_file[edge.dependency.index()].push(edge);
            dependencies_by_file[edge.dependent.index()].push(edge);
        }

        for ignored_edge in self.ignored_edges {
            ignored_dependents
                .entry(ignored_edge.edge.dependency)
                .or_default()
                .push(ignored_edge);
        }

        for dependents in &mut dependents_by_file {
            dependents.sort_by(|left, right| {
                let left_path = self.files.path(left.dependent);
                let right_path = self.files.path(right.dependent);

                left_path.cmp(right_path).then(left.kind.cmp(&right.kind))
            });
            dependents.dedup();
        }

        for dependencies in &mut dependencies_by_file {
            dependencies.sort_by(|left, right| {
                let left_path = self.files.path(left.dependency);
                let right_path = self.files.path(right.dependency);

                left_path.cmp(right_path).then(left.kind.cmp(&right.kind))
            });
            dependencies.dedup();
        }

        self.unresolved_sites.sort_by(|left, right| {
            let left_path = self.files.path(left.file);
            let right_path = self.files.path(right.file);

            left_path
                .cmp(right_path)
                .then(left.line.cmp(&right.line))
                .then(left.expression.cmp(&right.expression))
        });
        self.unresolved_sites.dedup();
        self.changed_parts.sort_by(|left, right| {
            let left_node = self.files.path(left.node);
            let right_node = self.files.path(right.node);

            left.change_path
                .cmp(&right.change_path)
                .then(left_node.cmp(right_node))
                .then(left.description.cmp(&right.description))
        });
        self.changed_parts.dedup();

        for mocked_modules in self.mocked_modules.values_mut() {
            mocked_modules.sort();
            mocked_modules.dedup();
        }

        let mut rule_links_by_dependency: HashMap<FileId, Vec<usize>> = HashMap::new();
        let mut rule_links_by_dependent: HashMap<FileId, Vec<usize>> = HashMap::new();

        for (rule_index, rule_link) in self.rule_links.iter().enumerate() {
            for &file in &rule_link.from {
                rule_links_by_dependency
                    .entry(file)
                    .or_default()
                    .push(rule_index);
            }

            for &file in &rule_link.to {
                rule_links_by_dependent
                    .entry(file)
                    .or_default()
                    .push(rule_index);
            }
        }

        let mut graph = Graph {
            files: self.files,
            test_languages: self.test_languages,
            target_names: self.target_names,
            removed_tests: self.removed_tests,
            dependents_by_file,
            dependencies_by_file,
            inferences: self.inferences,
            unresolved_sites: self.unresolved_sites,
            changed_parts: self.changed_parts,
            mocked_modules: self.mocked_modules,
            changed_contracts: self.changed_contracts,
            asset_blind_tests: self.asset_blind_tests,
            module_filters: self.module_filters,
            ignored_dependents,
            set_aside_sites: self.set_aside_sites,
            root_scopes: self.root_scopes,
            rule_links: self.rule_links,
            rule_links_by_dependency,
            rule_links_by_dependent,
            rule_ignores: self.rule_ignores,
            symbols: self.symbols.build(),
            commands: self.commands,
            test_support: HashSet::new(),
        };

        graph.test_support = test_support(&graph, &self.test_support_names);

        graph
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetName {
    pub unit: &'static str,
    pub name: String,
}

#[derive(Debug)]
pub struct Graph {
    files: FileTable,
    test_languages: HashMap<FileId, &'static str>,
    target_names: HashMap<FileId, TargetName>,
    removed_tests: HashSet<FileId>,
    dependents_by_file: Vec<Vec<Edge>>,
    dependencies_by_file: Vec<Vec<Edge>>,
    inferences: HashMap<(FileId, FileId, EdgeKind), Inference>,
    unresolved_sites: Vec<UnresolvedSite>,
    changed_parts: Vec<ChangedPart>,
    mocked_modules: HashMap<FileId, Vec<FileId>>,
    changed_contracts: HashSet<FileId>,
    asset_blind_tests: HashSet<FileId>,
    module_filters: HashMap<&'static str, ModuleFilter>,
    ignored_dependents: HashMap<FileId, Vec<IgnoredEdge>>,
    set_aside_sites: Vec<SetAsideSite>,
    root_scopes: Vec<RootScope>,
    rule_links: Vec<RuleLink>,
    rule_links_by_dependency: HashMap<FileId, Vec<usize>>,
    rule_links_by_dependent: HashMap<FileId, Vec<usize>>,
    rule_ignores: Vec<RuleIgnore>,
    symbols: SymbolTable,
    commands: HashMap<FileId, Vec<String>>,
    test_support: HashSet<FileId>,
}

impl Graph {
    pub fn file_id(&self, path: &RepoPath) -> Option<FileId> {
        self.files.id(path)
    }

    pub fn path(&self, file: FileId) -> &RepoPath {
        self.files.path(file)
    }

    pub fn is_test(&self, file: FileId) -> bool {
        self.test_languages.contains_key(&file)
    }

    pub fn test_language(&self, file: FileId) -> Option<&'static str> {
        self.test_languages.get(&file).copied()
    }

    pub fn target_name(&self, file: FileId) -> Option<&TargetName> {
        self.target_names.get(&file)
    }

    pub fn target_named(&self, name: &str) -> Option<FileId> {
        self.target_names
            .iter()
            .find(|(_, target_name)| target_name.name == name)
            .map(|(&file, _)| file)
    }

    pub fn test_files_by_path(&self) -> Vec<TestFile> {
        let mut test_files: Vec<TestFile> = self
            .test_languages
            .iter()
            .map(|(&file, &language)| TestFile { file, language })
            .collect();

        test_files.sort_by(|left, right| self.path(left.file).cmp(self.path(right.file)));

        test_files
    }

    pub fn dependents(&self, file: FileId) -> impl Iterator<Item = Edge> + '_ {
        let rule_edges = self
            .rule_dependents(file)
            .filter(|edge| self.rule_ignore_line(edge).is_none());

        self.dependents_by_file[file.index()]
            .iter()
            .copied()
            .chain(rule_edges)
    }

    pub fn dependencies(&self, file: FileId) -> impl Iterator<Item = Edge> + '_ {
        let rule_edges = self
            .rule_dependencies(file)
            .filter(|edge| self.rule_ignore_line(edge).is_none());

        self.dependencies_by_file[file.index()]
            .iter()
            .copied()
            .chain(rule_edges)
    }

    pub fn rule_link(&self, edge: &Edge) -> Option<&RuleLink> {
        if edge.kind != EdgeKind::Rule {
            return None;
        }

        rule_links_at(
            &self.rule_links,
            &self.rule_links_by_dependency,
            edge.dependency,
        )
        .find(|rule_link| rule_link.to.binary_search(&edge.dependent).is_ok())
    }

    fn rule_dependents(&self, file: FileId) -> impl Iterator<Item = Edge> + '_ {
        rule_links_at(&self.rule_links, &self.rule_links_by_dependency, file)
            .flat_map(move |rule_link| {
                rule_link.to.iter().map(move |&dependent| Edge {
                    dependent,
                    dependency: file,
                    kind: EdgeKind::Rule,
                })
            })
            .filter(move |edge| edge.dependent != file && !self.loads_a_test(edge))
    }

    fn rule_dependencies(&self, file: FileId) -> impl Iterator<Item = Edge> + '_ {
        rule_links_at(&self.rule_links, &self.rule_links_by_dependent, file)
            .flat_map(move |rule_link| {
                rule_link.from.iter().map(move |&dependency| Edge {
                    dependent: file,
                    dependency,
                    kind: EdgeKind::Rule,
                })
            })
            .filter(move |edge| edge.dependency != file && !self.loads_a_test(edge))
    }

    fn loads_a_test(&self, edge: &Edge) -> bool {
        self.is_or_was_test(edge.dependency) && !self.is_or_was_test(edge.dependent)
    }

    fn is_or_was_test(&self, file: FileId) -> bool {
        self.is_test(file) || self.removed_tests.contains(&file)
    }

    fn rule_ignore_line(&self, edge: &Edge) -> Option<usize> {
        self.rule_ignores
            .iter()
            .find(|rule_ignore| {
                rule_ignore.from.contains(&edge.dependency)
                    && rule_ignore.to.contains(&edge.dependent)
            })
            .map(|rule_ignore| rule_ignore.line)
    }

    pub fn inference(&self, edge: &Edge) -> Option<&Inference> {
        self.inferences
            .get(&(edge.dependent, edge.dependency, edge.kind))
    }

    pub fn inferred_edges(&self) -> impl Iterator<Item = (Edge, &Inference)> {
        self.inferences
            .iter()
            .map(|(&(dependent, dependency, kind), inference)| {
                let inferred_edge = Edge {
                    dependent,
                    dependency,
                    kind,
                };

                (inferred_edge, inference)
            })
    }

    pub fn can_load(&self, language: &str, path: &RepoPath) -> bool {
        self.module_filters
            .get(language)
            .is_none_or(|loads| loads(path))
    }

    pub fn is_module(&self, path: &RepoPath) -> bool {
        self.module_filters.values().any(|loads| loads(path))
    }

    pub fn is_test_support(&self, file: FileId) -> bool {
        self.test_support.contains(&file)
    }

    pub fn test_support_files(&self) -> impl Iterator<Item = FileId> + '_ {
        self.test_support.iter().copied()
    }

    pub fn is_test_code(&self, file: FileId) -> bool {
        self.is_test(file) || self.is_test_support(file)
    }

    pub fn site_can_load(&self, site: &UnresolvedSite, path: &RepoPath) -> bool {
        let is_test_support = self
            .file_id(path)
            .is_some_and(|file| self.is_test_support(file));

        self.can_load(site.language, path) && (self.is_test_code(site.file) || !is_test_support)
    }

    pub fn unresolved_sites(&self) -> &[UnresolvedSite] {
        &self.unresolved_sites
    }

    pub fn ignored_dependents(&self, file: FileId) -> impl Iterator<Item = IgnoredEdge> + '_ {
        let ignored_rule_edges = self.rule_dependents(file).filter_map(|edge| {
            self.rule_ignore_line(&edge)
                .map(|line| IgnoredEdge { edge, line })
        });

        self.ignored_dependents
            .get(&file)
            .into_iter()
            .flatten()
            .copied()
            .chain(ignored_rule_edges)
    }

    pub fn ignored_edge(&self, edge: &Edge) -> Option<IgnoredEdge> {
        self.ignored_dependents(edge.dependency)
            .find(|ignored_edge| ignored_edge.edge == *edge)
    }

    pub fn set_aside_sites(&self) -> &[SetAsideSite] {
        &self.set_aside_sites
    }

    pub fn root_scopes(&self) -> &[RootScope] {
        &self.root_scopes
    }

    pub fn changed_parts(&self) -> &[ChangedPart] {
        &self.changed_parts
    }

    pub fn mocked_modules(&self, test: FileId) -> &[FileId] {
        self.mocked_modules.get(&test).map_or(&[], Vec::as_slice)
    }

    pub fn has_changed_contract(&self, module: FileId) -> bool {
        self.changed_contracts.contains(&module)
    }

    pub fn is_asset_blind(&self, test: FileId) -> bool {
        self.asset_blind_tests.contains(&test)
    }

    pub fn is_asset(&self, file: FileId) -> bool {
        let mut dependents = self.dependents(file).peekable();

        dependents.peek().is_some() && dependents.all(|edge| edge.kind == EdgeKind::Asset)
    }

    pub fn symbol(&self, symbol: SymbolId) -> &Symbol {
        self.symbols.symbol(symbol)
    }

    pub fn symbol_count(&self) -> usize {
        self.symbols.len()
    }

    pub fn symbol_kind_count(&self, kind: SymbolKind) -> usize {
        self.symbols
            .all()
            .iter()
            .filter(|symbol| symbol.kind == kind)
            .count()
    }

    pub fn analysed_module_count(&self) -> usize {
        self.symbols.analysed_module_count()
    }

    pub fn symbols_of(&self, file: FileId) -> &[SymbolId] {
        self.symbols.symbols_of(file)
    }

    pub fn module_init(&self, file: FileId) -> Option<SymbolId> {
        self.symbols_of(file)
            .iter()
            .copied()
            .find(|&symbol| self.symbol(symbol).kind == SymbolKind::ModuleInit)
    }

    pub fn symbols_at(&self, file: FileId, line: u32) -> Vec<SymbolId> {
        let enclosing: Vec<SymbolId> = self
            .symbols_of(file)
            .iter()
            .copied()
            .filter(|&symbol| {
                self.symbol(symbol)
                    .lines
                    .iter()
                    .any(|lines| lines.contains(line))
            })
            .collect();

        if enclosing.is_empty() {
            return self.module_init(file).into_iter().collect();
        }

        enclosing
    }

    pub fn symbol_edges(&self) -> &[SymbolEdge] {
        self.symbols.edges()
    }

    pub fn symbol_dependencies(&self, symbol: SymbolId) -> &[SymbolEdge] {
        self.symbols.dependencies(symbol)
    }

    pub fn symbol_dependents(&self, symbol: SymbolId) -> impl Iterator<Item = &SymbolEdge> {
        self.symbols.dependents(symbol)
    }

    pub fn module_dependents(&self, file: FileId) -> impl Iterator<Item = &SymbolEdge> {
        self.symbols.module_dependents(file)
    }

    pub fn fallbacks(&self) -> &[ModuleFallback] {
        self.symbols.fallbacks()
    }

    pub fn symbol_edge_through(&self, edge: &SymbolEdge) -> &[FileId] {
        self.symbols.through(edge)
    }

    pub fn fallback_of(&self, file: FileId) -> Option<FallbackCause> {
        self.fallbacks()
            .iter()
            .find(|fallback| fallback.file == file)
            .map(|fallback| fallback.cause)
    }

    pub fn symbol_changes(&self) -> &[SymbolChange] {
        self.symbols.changes()
    }

    pub fn module_changes(&self, file: FileId) -> Option<&[usize]> {
        self.symbols.changes_of(file)
    }

    pub fn symbol_target_file(&self, target: SymbolTarget) -> FileId {
        match target {
            SymbolTarget::Symbol(symbol) => self.symbol(symbol).file,
            SymbolTarget::Module { file, .. } => file,
        }
    }

    pub fn memberships(&self) -> impl Iterator<Item = (SymbolId, &Membership)> {
        self.symbols.memberships()
    }

    pub fn is_dispatch(&self, edge: &SymbolEdge) -> bool {
        self.symbols.is_dispatch(edge)
    }

    pub fn dispatch_edges(&self) -> impl Iterator<Item = &SymbolEdge> {
        self.symbols.dispatch_edges()
    }

    pub fn commands(&self) -> impl Iterator<Item = (FileId, &[String])> {
        self.commands
            .iter()
            .map(|(&file, commands)| (file, commands.as_slice()))
    }
}

fn test_support(
    graph: &Graph,
    test_support_names: &HashMap<&'static str, ModuleFilter>,
) -> HashSet<FileId> {
    let runs = |edge: &Edge| !matches!(edge.kind, EdgeKind::TypeOnly | EdgeKind::Contains);
    let mut test_support: HashSet<FileId> = graph
        .files
        .iter()
        .filter(|&(file, path)| {
            !graph.is_test(file)
                && test_support_names
                    .values()
                    .any(|names_test_support| names_test_support(path))
        })
        .map(|(file, _)| file)
        .collect();
    let mut dropped: Vec<FileId> = test_support
        .iter()
        .copied()
        .filter(|&file| {
            graph.dependents(file).any(|edge| {
                runs(&edge)
                    && !graph.is_test(edge.dependent)
                    && !test_support.contains(&edge.dependent)
            })
        })
        .collect();

    while let Some(file) = dropped.pop() {
        if !test_support.remove(&file) {
            continue;
        }

        dropped.extend(
            graph
                .dependencies(file)
                .filter(|edge| runs(edge) && test_support.contains(&edge.dependency))
                .map(|edge| edge.dependency),
        );
    }

    test_support
}

fn rule_links_at<'g>(
    rule_links: &'g [RuleLink],
    rule_links_by_file: &'g HashMap<FileId, Vec<usize>>,
    file: FileId,
) -> impl Iterator<Item = &'g RuleLink> {
    rule_links_by_file
        .get(&file)
        .into_iter()
        .flatten()
        .map(|&rule_index| &rule_links[rule_index])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn dependent_paths(graph: &Graph, dependency: &str) -> Vec<(String, EdgeKind)> {
        let dependency_file = graph.file_id(&repo_path(dependency)).unwrap();

        graph
            .dependents(dependency_file)
            .map(|edge| (graph.path(edge.dependent).as_str().to_owned(), edge.kind))
            .collect()
    }

    #[test]
    fn dependents_are_sorted_by_path_and_deduplicated() {
        let mut builder = GraphBuilder::default();
        let shared = builder.add_file(repo_path("src/shared.ts"));
        let zeta = builder.add_file(repo_path("src/zeta.ts"));
        let alpha = builder.add_file(repo_path("src/alpha.ts"));
        let import_edges = [
            Edge {
                dependent: zeta,
                dependency: shared,
                kind: EdgeKind::Import,
            },
            Edge {
                dependent: alpha,
                dependency: shared,
                kind: EdgeKind::TypeOnly,
            },
            Edge {
                dependent: alpha,
                dependency: shared,
                kind: EdgeKind::Import,
            },
            Edge {
                dependent: zeta,
                dependency: shared,
                kind: EdgeKind::Import,
            },
        ];

        for edge in import_edges {
            builder.add_edge(edge);
        }

        let graph = builder.build();

        assert_eq!(
            dependent_paths(&graph, "src/shared.ts"),
            [
                ("src/alpha.ts".to_owned(), EdgeKind::Import),
                ("src/alpha.ts".to_owned(), EdgeKind::TypeOnly),
                ("src/zeta.ts".to_owned(), EdgeKind::Import),
            ]
        );
    }

    #[test]
    fn file_added_before_being_marked_as_test_keeps_one_id() {
        let mut builder = GraphBuilder::default();
        let imported_first = builder.add_file(repo_path("src/a.test.ts"));
        let marked_as_test = builder.add_test_file(repo_path("src/a.test.ts"), "typescript");
        let graph = builder.build();

        assert_eq!(imported_first, marked_as_test);
        assert!(graph.is_test(imported_first));
    }

    #[test]
    fn test_files_are_listed_by_path_with_their_language() {
        let mut builder = GraphBuilder::default();

        builder.add_test_file(repo_path("tests/test_api.py"), "python");
        builder.add_file(repo_path("src/a.ts"));
        builder.add_test_file(repo_path("src/a.test.ts"), "typescript");

        let graph = builder.build();
        let test_files: Vec<(&str, &str)> = graph
            .test_files_by_path()
            .iter()
            .map(|test_file| (graph.path(test_file.file).as_str(), test_file.language))
            .collect();

        assert_eq!(
            test_files,
            [
                ("src/a.test.ts", "typescript"),
                ("tests/test_api.py", "python")
            ]
        );
    }

    #[test]
    fn type_only_edges_never_select_tests() {
        assert!(EdgeKind::Import.selects_tests());
        assert!(EdgeKind::Dynamic.selects_tests());
        assert!(EdgeKind::Contains.selects_tests());
        assert!(EdgeKind::Asset.selects_tests());
        assert!(EdgeKind::Reads.selects_tests());
        assert!(EdgeKind::Lists.selects_tests());
        assert!(EdgeKind::Family.selects_tests());
        assert!(EdgeKind::Spawn.selects_tests());
        assert!(EdgeKind::Build.selects_tests());
        assert!(EdgeKind::Binding.selects_tests());
        assert!(!EdgeKind::TypeOnly.selects_tests());
    }

    #[test]
    fn reads_and_naming_families_follow_content_and_listings_pass_it_one_hop() {
        for content in ContentChange::ALL {
            assert!(EdgeKind::Import.follows(content));
            assert!(EdgeKind::Dynamic.follows(content));
            assert!(EdgeKind::Asset.follows(content));
            assert!(EdgeKind::Spawn.follows(content));
            assert!(EdgeKind::Build.follows(content));
            assert!(EdgeKind::Binding.follows(content));
        }

        assert!(!EdgeKind::Reads.follows(ContentChange::None));
        assert!(EdgeKind::Reads.follows(ContentChange::Listed));
        assert!(!EdgeKind::Family.follows(ContentChange::None));
        assert!(EdgeKind::Family.follows(ContentChange::Direct));
        assert!(!EdgeKind::Lists.follows(ContentChange::Listed));
        assert!(EdgeKind::Lists.follows(ContentChange::Direct));
        assert_eq!(EdgeKind::Lists.dependent_content(), ContentChange::Listed);
        assert_eq!(EdgeKind::Reads.dependent_content(), ContentChange::None);
        assert_eq!(EdgeKind::Spawn.dependent_content(), ContentChange::None);
        assert_eq!(EdgeKind::Build.dependent_content(), ContentChange::None);
    }

    #[test]
    fn asset_is_a_file_only_imported_as_an_asset() {
        let mut builder = GraphBuilder::default();
        let button = builder.add_file(repo_path("src/button.ts"));
        let styles = builder.add_file(repo_path("src/button.css"));
        let shared = builder.add_file(repo_path("src/shared.css"));
        let theme = builder.add_file(repo_path("src/theme.ts"));
        let unused = builder.add_file(repo_path("src/unused.css"));
        let asset_edges = [
            (button, styles, EdgeKind::Asset),
            (button, shared, EdgeKind::Asset),
        ];
        let other_edges = [(theme, shared, EdgeKind::Import)];

        for (dependent, dependency, kind) in asset_edges.into_iter().chain(other_edges) {
            builder.add_edge(Edge {
                dependent,
                dependency,
                kind,
            });
        }

        let graph = builder.build();

        assert!(graph.is_asset(styles));
        assert!(!graph.is_asset(shared));
        assert!(!graph.is_asset(unused));
        assert!(!graph.is_asset(button));
    }

    #[test]
    fn inferred_edge_keeps_the_inference_of_its_first_site() {
        let mut builder = GraphBuilder::default();
        let loader = builder.add_file(repo_path("src/loader.ts"));
        let plugin = builder.add_file(repo_path("src/plugins/a.ts"));
        let dynamic_edge = Edge {
            dependent: loader,
            dependency: plugin,
            kind: EdgeKind::Dynamic,
        };
        let inference_at = |line: u32| Inference {
            line,
            kind: "glob",
            pattern: "src/plugins/*.ts".to_owned(),
            detail: None,
        };

        builder.add_inferred_edge(dynamic_edge, inference_at(12));
        builder.add_inferred_edge(dynamic_edge, inference_at(4));
        builder.add_inferred_edge(dynamic_edge, inference_at(30));

        let graph = builder.build();
        let read_edge = Edge {
            kind: EdgeKind::Reads,
            ..dynamic_edge
        };

        assert_eq!(
            dependent_paths(&graph, "src/plugins/a.ts"),
            [("src/loader.ts".to_owned(), EdgeKind::Dynamic)]
        );
        assert_eq!(graph.inference(&dynamic_edge), Some(&inference_at(4)));
        assert_eq!(graph.inference(&read_edge), None);
        assert_eq!(
            graph.dependencies(loader).collect::<Vec<_>>(),
            [dynamic_edge]
        );
        assert_eq!(
            graph.inferred_edges().collect::<Vec<_>>(),
            [(dynamic_edge, &inference_at(4))]
        );
    }

    #[test]
    fn rule_links_expand_into_edges_from_every_file_to_every_target_when_visited() {
        let mut builder = GraphBuilder::default();
        let sources: Vec<FileId> = ["crates/lang-a/src/lib.rs", "crates/lang-a/src/parse.rs"]
            .into_iter()
            .map(|path| builder.add_file(repo_path(path)))
            .collect();
        let targets: Vec<FileId> = ["project-a-test/a.test.ts", "project-a-test/b.test.ts"]
            .into_iter()
            .map(|path| builder.add_test_file(repo_path(path), "typescript"))
            .collect();
        let links = builder.add_rule_link(RuleLink {
            line: 3,
            pattern: "crates/lang-{lang}/**".to_owned(),
            captures: vec![("lang".to_owned(), "a".to_owned())],
            from: sources.clone(),
            to: targets.clone(),
        });
        let ignored =
            builder.ignore_links(9, HashSet::from([sources[1]]), HashSet::from([targets[1]]));
        let graph = builder.build();
        let ignored_edge = Edge {
            dependent: targets[1],
            dependency: sources[1],
            kind: EdgeKind::Rule,
        };

        assert_eq!(links, 4);
        assert_eq!(ignored.links, 1);
        assert_eq!(
            dependent_paths(&graph, "crates/lang-a/src/lib.rs"),
            [
                ("project-a-test/a.test.ts".to_owned(), EdgeKind::Rule),
                ("project-a-test/b.test.ts".to_owned(), EdgeKind::Rule),
            ]
        );
        assert_eq!(
            dependent_paths(&graph, "crates/lang-a/src/parse.rs"),
            [("project-a-test/a.test.ts".to_owned(), EdgeKind::Rule)]
        );
        assert_eq!(
            graph
                .dependencies(targets[1])
                .map(|edge| graph.path(edge.dependency).as_str())
                .collect::<Vec<_>>(),
            ["crates/lang-a/src/lib.rs"]
        );
        assert_eq!(
            graph.ignored_edge(&ignored_edge),
            Some(IgnoredEdge {
                edge: ignored_edge,
                line: 9
            })
        );
        assert_eq!(
            graph
                .rule_link(&ignored_edge)
                .map(|rule_link| rule_link.line),
            Some(3)
        );
    }

    #[test]
    fn rule_links_never_make_product_code_depend_on_a_test_file() {
        let linked = |ignores_loader: bool| {
            let mut builder = GraphBuilder::default();
            let loader = builder.add_file(repo_path("src/loader.ts"));
            let plugin = builder.add_file(repo_path("extensions/a/index.ts"));
            let plugin_test =
                builder.add_test_file(repo_path("extensions/a/index.test.ts"), "typescript");
            let meta_test = builder.add_test_file(repo_path("test/meta.test.ts"), "typescript");

            builder.add_removed_test(repo_path("extensions/a/old.test.ts"));

            let removed_test = builder.add_file(repo_path("extensions/a/old.test.ts"));
            let links = builder.add_rule_link(RuleLink {
                line: 1,
                pattern: "extensions/**".to_owned(),
                captures: Vec::new(),
                from: vec![plugin, plugin_test, removed_test],
                to: vec![loader, meta_test],
            });
            let ignored = ignores_loader.then(|| {
                builder.ignore_links(
                    5,
                    HashSet::from([plugin, plugin_test, removed_test]),
                    HashSet::from([loader]),
                )
            });

            (builder.build(), links, ignored)
        };
        let (graph, links, _) = linked(false);
        let (_, _, ignored) = linked(true);
        let loader = graph.file_id(&repo_path("src/loader.ts")).unwrap();

        assert_eq!(links, 4);
        assert_eq!(ignored.map(|ignored| ignored.links), Some(1));
        assert_eq!(
            dependent_paths(&graph, "extensions/a/index.ts"),
            [
                ("src/loader.ts".to_owned(), EdgeKind::Rule),
                ("test/meta.test.ts".to_owned(), EdgeKind::Rule),
            ]
        );
        assert_eq!(
            dependent_paths(&graph, "extensions/a/index.test.ts"),
            [("test/meta.test.ts".to_owned(), EdgeKind::Rule)]
        );
        assert_eq!(
            dependent_paths(&graph, "extensions/a/old.test.ts"),
            [("test/meta.test.ts".to_owned(), EdgeKind::Rule)]
        );
        assert_eq!(
            graph
                .dependencies(loader)
                .map(|edge| graph.path(edge.dependency).as_str())
                .collect::<Vec<_>>(),
            ["extensions/a/index.ts"]
        );
    }

    #[test]
    fn inference_is_described_by_its_kind_pattern_and_detail() {
        let plugin_folder = Inference {
            line: 3,
            kind: "plugin folder",
            pattern: "extensions/{id}/".to_owned(),
            detail: Some("id=telegram".to_owned()),
        };

        assert_eq!(
            plugin_folder.description(),
            "inferred plugin folder extensions/{id}/ (id=telegram)"
        );
    }

    #[test]
    fn each_language_says_which_files_its_unresolved_sites_can_load() {
        let mut builder = GraphBuilder::default();

        builder.add_language("typescript", |path| path.as_str().ends_with(".ts"));

        let graph = builder.build();

        assert!(graph.can_load("typescript", &repo_path("src/a.ts")));
        assert!(!graph.can_load("typescript", &repo_path("docs/a.md")));
        assert!(graph.can_load("python", &repo_path("docs/a.md")));
        assert!(graph.is_module(&repo_path("src/a.ts")));
        assert!(!graph.is_module(&repo_path("docs/a.md")));
    }

    #[test]
    fn test_support_is_named_as_test_code_and_run_only_by_tests_and_test_support() {
        let mut builder = GraphBuilder::default();
        let edges = [
            ("src/a.test.ts", EdgeKind::Import, "src/a.test-support.ts"),
            ("src/c.ts", EdgeKind::TypeOnly, "src/a.test-support.ts"),
            ("src/a.test-support.ts", EdgeKind::Import, "test/env.ts"),
            ("test/env.ts", EdgeKind::Import, "test/clock.ts"),
            ("test/clock.ts", EdgeKind::Import, "test/env.ts"),
            ("src/a.test-support.ts", EdgeKind::Import, "src/a.ts"),
            ("src/b.test.ts", EdgeKind::Import, "src/b.test-support.ts"),
            ("src/b.ts", EdgeKind::Import, "src/b.test-support.ts"),
            ("src/b.test-support.ts", EdgeKind::Import, "test/server.ts"),
            ("src/d.test.ts", EdgeKind::Import, "test/plugin.ts"),
            ("config/plugins.json", EdgeKind::Lists, "test/plugin.ts"),
            ("src/d.test.ts", EdgeKind::Import, "extensions/x/index.ts"),
            (
                "src/d.test.ts",
                EdgeKind::Import,
                "extensions/x/x.test-support.ts",
            ),
            ("extensions/x", EdgeKind::Contains, "extensions/x/index.ts"),
            (
                "extensions/x",
                EdgeKind::Contains,
                "extensions/x/x.test-support.ts",
            ),
        ];

        for test in ["src/a.test.ts", "src/b.test.ts", "src/d.test.ts"] {
            builder.add_test_file(repo_path(test), "typescript");
        }

        for (dependent, kind, dependency) in edges {
            let dependent = builder.add_file(repo_path(dependent));
            let dependency = builder.add_file(repo_path(dependency));

            builder.add_edge(Edge {
                dependent,
                dependency,
                kind,
            });
        }

        builder.add_file(repo_path("test/setup.ts"));
        builder.add_test_support("typescript", |path| {
            path.as_str().starts_with("test/") || path.as_str().contains(".test-support.")
        });

        let graph = builder.build();
        let mut test_support: Vec<&str> = graph
            .test_support_files()
            .map(|file| graph.path(file).as_str())
            .collect();

        test_support.sort_unstable();

        assert_eq!(
            test_support,
            [
                "extensions/x/x.test-support.ts",
                "src/a.test-support.ts",
                "test/clock.ts",
                "test/env.ts",
                "test/setup.ts"
            ]
        );
    }

    #[test]
    fn changed_parts_are_listed_by_change_and_node_without_duplicates() {
        let mut builder = GraphBuilder::default();
        let lodash_change = "package lodash changed (4.17.20 -> 4.17.21)".to_owned();
        let resolution_change = "src/a.ts resolves \"@acme/lib\" differently".to_owned();

        builder.add_changed_part(
            repo_path("pnpm-lock.yaml"),
            repo_path("node_modules/lodash"),
            lodash_change.clone(),
        );
        builder.add_changed_part(
            repo_path("packages/lib/package.json"),
            repo_path("src/a.ts"),
            resolution_change.clone(),
        );
        builder.add_changed_part(
            repo_path("pnpm-lock.yaml"),
            repo_path("node_modules/lodash"),
            lodash_change.clone(),
        );

        let graph = builder.build();
        let listed_parts: Vec<(&str, &str, &str)> = graph
            .changed_parts()
            .iter()
            .map(|part| {
                (
                    part.change_path.as_str(),
                    graph.path(part.node).as_str(),
                    part.description.as_str(),
                )
            })
            .collect();

        assert_eq!(
            listed_parts,
            [
                (
                    "packages/lib/package.json",
                    "src/a.ts",
                    resolution_change.as_str()
                ),
                (
                    "pnpm-lock.yaml",
                    "node_modules/lodash",
                    lodash_change.as_str()
                ),
            ]
        );
    }

    #[test]
    fn unresolved_sites_are_listed_by_path_and_line_without_duplicates() {
        let mut builder = GraphBuilder::default();
        let worker = builder.add_file(repo_path("src/worker.ts"));
        let loader = builder.add_file(repo_path("src/loader.ts"));
        let unresolved_site = |file: FileId, line: u32| UnresolvedSite {
            file,
            line,
            expression: "request.moduleUrl".to_owned(),
            cause: UnresolvedCause::RuntimeValue,
            language: "typescript",
        };

        builder.add_unresolved_site(unresolved_site(worker, 3));
        builder.add_unresolved_site(unresolved_site(loader, 20));
        builder.add_unresolved_site(unresolved_site(loader, 8));
        builder.add_unresolved_site(unresolved_site(loader, 20));

        let graph = builder.build();
        let listed_sites: Vec<(&str, u32)> = graph
            .unresolved_sites()
            .iter()
            .map(|site| (graph.path(site.file).as_str(), site.line))
            .collect();

        assert_eq!(
            listed_sites,
            [
                ("src/loader.ts", 8),
                ("src/loader.ts", 20),
                ("src/worker.ts", 3)
            ]
        );
    }
}
