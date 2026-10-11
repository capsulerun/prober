//! Module trees from each crate target and the files a path reaches through them: child modules, use bindings, globs, extern crates and re-exports, with macros, proc-macro crates and build scripts reaching whole crates.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use prober_core::files::RepoPath;
use prober_core::graph::{EdgeKind, Inference};

use crate::conventions::{
    BUILTIN_CRATES, join_directory, normalized_join, parent_directory, split_path,
};
use crate::source::{ImplTarget, IncludeBase, IncludeKind, ScannedPath, SourceFacts, UseEntry};
use crate::workspace::{Crate, DependencyKind, TargetKind, Workspace};

const MAX_DEPTH: usize = 8;

const INCLUDED_FILE: &str = "included file";

const MANIFEST_DIRECTORY_PATH: &str = "manifest directory path";

const FILE_PATH: &str = "file path";

type ModuleId = usize;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Dependency {
    File(RepoPath),
    Path(String),
    Crate(usize),
    External(String),
}

pub(crate) struct Link {
    pub dependent: RepoPath,
    pub dependency: Dependency,
    pub kind: EdgeKind,
    pub inference: Option<Inference>,
}

struct Module {
    file: RepoPath,
    scope: Vec<String>,
    parent: Option<ModuleId>,
    children: BTreeMap<String, ModuleId>,
    tree: usize,
}

struct Tree {
    krate: usize,
    kind: TargetKind,
    root: ModuleId,
}

#[derive(Default)]
struct Resolution {
    dependencies: BTreeSet<Dependency>,
    module: Option<ModuleId>,
}

pub(crate) struct ModuleGraph<'g> {
    workspace: &'g Workspace,
    repo_files: HashSet<&'g str>,
    facts: &'g HashMap<RepoPath, SourceFacts>,
    modules: Vec<Module>,
    trees: Vec<Tree>,
    lib_roots: Vec<Option<ModuleId>>,
    file_modules: HashMap<RepoPath, Vec<ModuleId>>,
    macros: HashMap<String, Vec<(RepoPath, bool)>>,
    crate_files: Vec<BTreeSet<RepoPath>>,
    lib_files: Vec<BTreeSet<RepoPath>>,
    reexport_cache: RefCell<HashMap<(ModuleId, String), BTreeSet<Dependency>>>,
    reexport_lookups: RefCell<HashSet<(ModuleId, String)>>,
    glob_cache: RefCell<HashMap<ModuleId, Vec<GlobResolution>>>,
}

type GlobResolution = (Option<ModuleId>, BTreeSet<Dependency>);

enum Lookup {
    Found(BTreeSet<Dependency>),
    InProgress,
    NotFound,
}

impl<'g> ModuleGraph<'g> {
    pub fn new(
        workspace: &'g Workspace,
        facts: &'g HashMap<RepoPath, SourceFacts>,
        repo_files: &'g [RepoPath],
    ) -> Self {
        let mut graph = Self {
            workspace,
            repo_files: repo_files.iter().map(RepoPath::as_str).collect(),
            facts,
            modules: Vec::new(),
            trees: Vec::new(),
            lib_roots: vec![None; workspace.crates.len()],
            file_modules: HashMap::new(),
            macros: HashMap::new(),
            crate_files: vec![BTreeSet::new(); workspace.crates.len()],
            lib_files: vec![BTreeSet::new(); workspace.crates.len()],
            reexport_cache: RefCell::new(HashMap::new()),
            reexport_lookups: RefCell::new(HashSet::new()),
            glob_cache: RefCell::new(HashMap::new()),
        };

        for (file, file_facts) in facts {
            for (name, exported) in &file_facts.macro_rules {
                graph
                    .macros
                    .entry(name.clone())
                    .or_default()
                    .push((file.clone(), *exported));
            }
        }

        for (krate, crate_info) in workspace.crates.iter().enumerate() {
            for target in &crate_info.targets {
                let Ok(root) = RepoPath::new(target.root.clone()) else {
                    continue;
                };

                if !facts.contains_key(&root) {
                    continue;
                }

                let tree = graph.trees.len();
                let root_module = graph.add_module(root, Vec::new(), None, tree);

                graph.trees.push(Tree {
                    krate,
                    kind: target.kind,
                    root: root_module,
                });

                if target.kind == TargetKind::Lib {
                    graph.lib_roots[krate] = Some(root_module);
                }

                graph.add_children(root_module, true, &mut BTreeSet::new());
            }
        }

        for module in &graph.modules {
            let tree = &graph.trees[module.tree];

            graph.crate_files[tree.krate].insert(module.file.clone());

            if matches!(tree.kind, TargetKind::Lib | TargetKind::Build) {
                graph.lib_files[tree.krate].insert(module.file.clone());
            }
        }

        graph
    }

    pub fn crate_files(&self, krate: usize) -> &BTreeSet<RepoPath> {
        &self.crate_files[krate]
    }

    pub fn lib_files(&self, krate: usize) -> &BTreeSet<RepoPath> {
        &self.lib_files[krate]
    }

    fn add_module(
        &mut self,
        file: RepoPath,
        scope: Vec<String>,
        parent: Option<ModuleId>,
        tree: usize,
    ) -> ModuleId {
        let id = self.modules.len();

        self.file_modules.entry(file.clone()).or_default().push(id);
        self.modules.push(Module {
            file,
            scope,
            parent,
            children: BTreeMap::new(),
            tree,
        });

        id
    }

    fn add_children(&mut self, module: ModuleId, is_root: bool, visiting: &mut BTreeSet<RepoPath>) {
        let file = self.modules[module].file.clone();
        let scope = self.modules[module].scope.clone();
        let tree = self.modules[module].tree;
        let Some(facts) = self.facts.get(&file) else {
            return;
        };
        let declarations: Vec<_> = facts
            .mods
            .iter()
            .filter(|declaration| declaration.scope == scope)
            .cloned()
            .collect();

        visiting.insert(file.clone());

        for declaration in declarations {
            let child = if declaration.inline {
                let mut child_scope = scope.clone();

                child_scope.push(declaration.name.clone());

                Some(self.add_module(file.clone(), child_scope, Some(module), tree))
            } else {
                let child_file = self.child_file(&file, &scope, is_root, &declaration);

                child_file
                    .filter(|child_file| !visiting.contains(child_file))
                    .map(|child_file| self.add_module(child_file, Vec::new(), Some(module), tree))
            };
            let Some(child) = child else {
                continue;
            };

            self.modules[module]
                .children
                .insert(declaration.name.clone(), child);

            let child_is_root = false;

            self.add_children(child, child_is_root, visiting);
        }

        visiting.remove(&file);
    }

    fn child_file(
        &self,
        file: &RepoPath,
        scope: &[String],
        is_root: bool,
        declaration: &crate::source::ModDecl,
    ) -> Option<RepoPath> {
        let (directory, name) = split_path(file.as_str());
        let stem = name.strip_suffix(".rs").unwrap_or(name);
        let owns_directory = is_root || name == "mod.rs";
        let module_directory = if owns_directory {
            directory.to_owned()
        } else {
            join_directory(directory, stem)
        };
        let scoped_directory = join_directory(&module_directory, &scope.join("/"));
        let candidates = match &declaration.path_attribute {
            Some(path) if scope.is_empty() => vec![normalized_join(directory, path)?],
            Some(path) => vec![normalized_join(&scoped_directory, path)?],
            None => vec![
                join_directory(&scoped_directory, &format!("{}.rs", declaration.name)),
                join_directory(&scoped_directory, &format!("{}/mod.rs", declaration.name)),
            ],
        };

        candidates
            .into_iter()
            .filter_map(|candidate| RepoPath::new(candidate).ok())
            .find(|candidate| self.facts.contains_key(candidate))
    }

    pub fn links(&self) -> Vec<Link> {
        let mut links = Vec::new();
        let mut referenced: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();

        for (file, modules) in &self.file_modules {
            let Some(facts) = self.facts.get(file) else {
                continue;
            };
            let mut dependencies: BTreeSet<(Dependency, EdgeKind)> = BTreeSet::new();

            for &module in modules {
                let krate = self.trees[self.modules[module].tree].krate;
                let file_scope_module = self.file_scope_module(module);

                for entry in facts.uses.iter() {
                    let Some(scope_module) = self.scoped_module(file_scope_module, &entry.scope)
                    else {
                        continue;
                    };
                    let resolution = self.resolve(&entry.path, scope_module, 0);

                    if entry.public && !self.uses_reexport(facts, entry, &resolution) {
                        continue;
                    }

                    dependencies.extend(
                        resolution
                            .dependencies
                            .into_iter()
                            .map(|dependency| (dependency, EdgeKind::Import)),
                    );
                }

                for path in &facts.paths {
                    for dependency in self.resolve_scanned(path, module, krate) {
                        dependencies.insert((dependency, EdgeKind::Import));
                    }
                }

                for crate_name in &facts.macro_use_crates {
                    if let Some(dependency) = self.extern_crate(crate_name, module) {
                        dependencies.insert((dependency, EdgeKind::Import));
                    }
                }

                for implementation in &facts.impls {
                    let Some(scope_module) =
                        self.scoped_module(file_scope_module, &implementation.scope)
                    else {
                        continue;
                    };

                    links.extend(self.impl_links(file, facts, implementation, scope_module));
                }

                links.extend(self.include_links(file, facts, krate));
            }

            for (dependency, kind) in dependencies {
                if let Dependency::File(target) = &dependency
                    && target == file
                {
                    continue;
                }

                for &module in modules {
                    let krate = self.trees[self.modules[module].tree].krate;
                    let name = match &dependency {
                        Dependency::Crate(other) => {
                            Some(self.workspace.crates[*other].name.clone())
                        }
                        Dependency::External(package) => Some(package.clone()),
                        Dependency::Path(_) => None,
                        Dependency::File(target) => self
                            .file_modules
                            .get(target)
                            .and_then(|targets| targets.first())
                            .map(|&target| self.trees[self.modules[target].tree].krate)
                            .filter(|&other| other != krate)
                            .map(|other| self.workspace.crates[other].name.clone()),
                    };

                    referenced.entry(krate).or_default().extend(name);
                }

                links.push(Link {
                    dependent: file.clone(),
                    dependency,
                    kind,
                    inference: None,
                });
            }
        }

        for (krate, crate_info) in self.workspace.crates.iter().enumerate() {
            let used = referenced.get(&krate);

            for dependency in &crate_info.dependencies {
                let is_used = used.is_some_and(|used| used.contains(&dependency.package));

                match self.workspace.workspace_dependency(dependency) {
                    Some(other) if !is_used && other != krate => {
                        let dependents: Vec<RepoPath> = if dependency.kind == DependencyKind::Dev {
                            vec![crate_info.manifest.clone()]
                        } else {
                            self.lib_files(krate).iter().cloned().collect()
                        };

                        for dependent in dependents {
                            links.push(Link {
                                dependent,
                                dependency: Dependency::Crate(other),
                                kind: EdgeKind::Import,
                                inference: None,
                            });
                        }
                    }
                    Some(_) => {}
                    None => links.push(Link {
                        dependent: crate_info.manifest.clone(),
                        dependency: Dependency::External(dependency.package.clone()),
                        kind: EdgeKind::Import,
                        inference: None,
                    }),
                }
            }
        }

        links
    }

    fn impl_links(
        &self,
        file: &RepoPath,
        facts: &SourceFacts,
        implementation: &ImplTarget,
        scope_module: ModuleId,
    ) -> Vec<Link> {
        let tree = self.modules[scope_module].tree;
        let local_files = |path: &Option<Vec<String>>| -> Option<BTreeSet<RepoPath>> {
            let path = path.as_ref()?;

            if let [name] = path.as_slice()
                && facts
                    .items
                    .iter()
                    .any(|(scope, defined)| defined == name && *scope == implementation.scope)
            {
                return Some(BTreeSet::from([file.clone()]));
            }

            let files: BTreeSet<RepoPath> = self
                .resolve(path, scope_module, 0)
                .dependencies
                .into_iter()
                .filter_map(|dependency| match dependency {
                    Dependency::File(target) if self.in_tree(&target, tree) => Some(target),
                    _ => None,
                })
                .collect();

            (!files.is_empty()).then_some(files)
        };
        let owners = match local_files(&implementation.self_type) {
            Some(type_files) => type_files,
            None => local_files(&implementation.trait_path).unwrap_or_default(),
        };

        owners
            .into_iter()
            .filter(|owner| owner != file)
            .map(|owner| Link {
                dependent: owner,
                dependency: Dependency::File(file.clone()),
                kind: EdgeKind::Import,
                inference: None,
            })
            .collect()
    }

    fn file_scope_module(&self, module: ModuleId) -> ModuleId {
        let mut current = module;

        while !self.modules[current].scope.is_empty() {
            match self.modules[current].parent {
                Some(parent) => current = parent,
                None => break,
            }
        }

        current
    }

    fn scoped_module(&self, file_module: ModuleId, scope: &[String]) -> Option<ModuleId> {
        let mut current = file_module;

        for name in scope {
            current = *self.modules[current].children.get(name)?;
        }

        Some(current)
    }

    fn resolve_scanned(
        &self,
        path: &ScannedPath,
        module: ModuleId,
        krate: usize,
    ) -> BTreeSet<Dependency> {
        let resolution = self.resolve(&path.segments, module, 0);

        if !path.is_macro {
            return resolution.dependencies;
        }

        let name = path.segments.last().map(String::as_str).unwrap_or_default();
        let mut dependencies = BTreeSet::new();

        for dependency in resolution.dependencies {
            match dependency {
                Dependency::File(file) => match self.crate_of(&file) {
                    Some(other) if other != krate => {
                        let definitions = self.crate_macro_definitions(name, other);

                        if definitions.is_empty() {
                            dependencies.insert(Dependency::Crate(other));
                        } else {
                            dependencies.extend(definitions);
                        }
                    }
                    _ => {
                        dependencies.insert(Dependency::File(file));
                    }
                },
                dependency => {
                    dependencies.insert(dependency);
                }
            }
        }

        if path.segments.len() == 1 {
            dependencies.extend(self.macro_definitions(name, module, krate));
        }

        dependencies
    }

    fn crate_macro_definitions(&self, name: &str, krate: usize) -> Vec<Dependency> {
        self.macros
            .get(name)
            .into_iter()
            .flatten()
            .filter(|(file, exported)| *exported && self.crate_of(file) == Some(krate))
            .map(|(file, _)| Dependency::File(file.clone()))
            .collect()
    }

    fn macro_definitions(&self, name: &str, module: ModuleId, krate: usize) -> Vec<Dependency> {
        let mut definitions = Vec::new();

        for (file, exported) in self.macros.get(name).into_iter().flatten() {
            for &defining in self.file_modules.get(file).into_iter().flatten() {
                let other = self.trees[self.modules[defining].tree].krate;

                if other == krate || *exported && self.depends_on(module, other) {
                    definitions.push(Dependency::File(file.clone()));
                }
            }
        }

        definitions
    }

    fn depends_on(&self, module: ModuleId, other: usize) -> bool {
        let tree = &self.trees[self.modules[module].tree];

        self.workspace.crates[tree.krate]
            .dependencies
            .iter()
            .any(|dependency| self.workspace.workspace_dependency(dependency) == Some(other))
    }

    fn in_tree(&self, file: &RepoPath, tree: usize) -> bool {
        self.file_modules.get(file).is_some_and(|modules| {
            modules
                .iter()
                .any(|&module| self.modules[module].tree == tree)
        })
    }

    fn crate_of(&self, file: &RepoPath) -> Option<usize> {
        let module = *self.file_modules.get(file)?.first()?;

        Some(self.trees[self.modules[module].tree].krate)
    }

    fn resolve(&self, segments: &[String], from: ModuleId, depth: usize) -> Resolution {
        let mut resolution = Resolution::default();

        if depth > MAX_DEPTH || segments.is_empty() {
            return resolution;
        }

        let tree = &self.trees[self.modules[from].tree];
        let first = segments[0].as_str();
        let rest = &segments[1..];

        match first {
            "crate" => self.walk(tree.root, rest, &mut resolution),
            "self" => self.walk(from, rest, &mut resolution),
            "super" => {
                let mut current = from;
                let mut remaining = segments;

                while let Some(("super", tail)) = remaining
                    .split_first()
                    .map(|(head, tail)| (head.as_str(), tail))
                {
                    match self.modules[current].parent {
                        Some(parent) => current = parent,
                        None => return resolution,
                    }

                    remaining = tail;
                }

                self.walk(current, remaining, &mut resolution);
            }
            "Self" => {}
            name => {
                if let Some(&child) = self.modules[from].children.get(name) {
                    self.walk(child, rest, &mut resolution);

                    return resolution;
                }

                if let Some(bound) = self.bound_use(from, name) {
                    for entry in bound {
                        let used = self.resolve(&entry.path, from, depth + 1);

                        match used.module {
                            Some(module) if !rest.is_empty() => {
                                self.walk(module, rest, &mut resolution);
                            }
                            _ => resolution.dependencies.extend(used.dependencies),
                        }
                    }

                    return resolution;
                }

                if let Some(dependency) = self.extern_crate(name, from) {
                    match dependency {
                        Dependency::Crate(other) => {
                            let whole = self.workspace.crates[other].proc_macro;

                            match self.lib_roots[other] {
                                Some(root) if !whole => self.walk(root, rest, &mut resolution),
                                _ => {
                                    resolution.dependencies.insert(Dependency::Crate(other));
                                }
                            }
                        }
                        dependency => {
                            resolution.dependencies.insert(dependency);
                        }
                    }

                    return resolution;
                }

                for glob in self.glob_uses(from) {
                    let used = self.resolve(&glob.path, from, depth + 1);

                    if let Some(module) = used.module
                        && let Some(&child) = self.modules[module].children.get(name)
                    {
                        self.walk(child, rest, &mut resolution);
                    }
                }
            }
        }

        resolution
    }

    fn walk(&self, start: ModuleId, segments: &[String], resolution: &mut Resolution) {
        let mut current = start;

        for segment in segments {
            if segment == "super" {
                match self.modules[current].parent {
                    Some(parent) => {
                        current = parent;

                        continue;
                    }
                    None => return,
                }
            }

            if segment == "self" {
                continue;
            }

            if let Some(&child) = self.modules[current].children.get(segment) {
                current = child;

                continue;
            }

            if self.defines(current, segment) {
                resolution
                    .dependencies
                    .insert(Dependency::File(self.modules[current].file.clone()));
            } else {
                match self.reexported(current, segment) {
                    Lookup::Found(reexported) => resolution.dependencies.extend(reexported),
                    Lookup::InProgress => {}
                    Lookup::NotFound => {
                        resolution
                            .dependencies
                            .insert(Dependency::File(self.modules[current].file.clone()));

                        for (_, glob_dependencies) in self.glob_resolutions(current) {
                            resolution.dependencies.extend(glob_dependencies);
                        }
                    }
                }
            }

            return;
        }

        resolution
            .dependencies
            .insert(Dependency::File(self.modules[current].file.clone()));
        resolution.module = Some(current);
    }

    fn uses_reexport(
        &self,
        facts: &SourceFacts,
        entry: &UseEntry,
        resolution: &Resolution,
    ) -> bool {
        if !entry.glob {
            return entry
                .binding
                .as_ref()
                .is_some_and(|binding| facts.identifiers.contains(binding));
        }

        let Some(module) = resolution.module else {
            return true;
        };
        let module = &self.modules[module];

        self.facts.get(&module.file).is_none_or(|target| {
            target
                .items
                .iter()
                .filter(|(scope, _)| *scope == module.scope)
                .any(|(_, name)| facts.identifiers.contains(name))
        })
    }

    fn defines(&self, module: ModuleId, item: &str) -> bool {
        let module = &self.modules[module];

        self.facts.get(&module.file).is_some_and(|facts| {
            facts
                .items
                .iter()
                .any(|(scope, name)| name == item && *scope == module.scope)
                || facts.macro_rules.iter().any(|(name, _)| name == item)
        })
    }

    fn reexported(&self, module: ModuleId, item: &str) -> Lookup {
        let key = (module, item.to_owned());

        if let Some(cached) = self.reexport_cache.borrow().get(&key) {
            return Lookup::Found(cached.clone());
        }

        if !self.reexport_lookups.borrow_mut().insert(key.clone()) {
            return Lookup::InProgress;
        }

        let mut dependencies = BTreeSet::new();
        let mut found = false;

        for entry in self.scope_uses(module) {
            if entry.binding.as_deref() == Some(item) {
                dependencies.extend(self.resolve(&entry.path, module, 1).dependencies);
                found = true;
            }
        }

        if !found {
            for (glob_module, _) in self.glob_resolutions(module) {
                let Some(glob_module) = glob_module else {
                    continue;
                };

                if self.defines(glob_module, item) {
                    dependencies.insert(Dependency::File(self.modules[glob_module].file.clone()));
                    found = true;
                } else if let Lookup::Found(reexported) = self.reexported(glob_module, item) {
                    dependencies.extend(reexported);
                    found = true;
                }
            }
        }

        self.reexport_lookups.borrow_mut().remove(&key);

        if !found {
            return Lookup::NotFound;
        }

        self.reexport_cache
            .borrow_mut()
            .insert(key, dependencies.clone());

        Lookup::Found(dependencies)
    }

    fn glob_resolutions(&self, module: ModuleId) -> Vec<GlobResolution> {
        if let Some(cached) = self.glob_cache.borrow().get(&module) {
            return cached.clone();
        }

        self.glob_cache.borrow_mut().insert(module, Vec::new());

        let resolutions: Vec<GlobResolution> = self
            .scope_uses(module)
            .filter(|entry| entry.glob)
            .map(|entry| {
                let resolution = self.resolve(&entry.path, module, 1);

                (resolution.module, resolution.dependencies)
            })
            .collect();

        self.glob_cache
            .borrow_mut()
            .insert(module, resolutions.clone());

        resolutions
    }

    fn scope_uses(&self, module: ModuleId) -> impl Iterator<Item = &UseEntry> {
        let module_info = &self.modules[module];

        self.facts
            .get(&module_info.file)
            .into_iter()
            .flat_map(|facts| &facts.uses)
            .filter(move |entry| entry.scope == module_info.scope)
    }

    fn bound_use(&self, module: ModuleId, name: &str) -> Option<Vec<&UseEntry>> {
        let imports_itself = |entry: &UseEntry| entry.path.len() == 1 && entry.path[0] == name;
        let bound: Vec<&UseEntry> = self
            .scope_uses(module)
            .filter(|entry| entry.binding.as_deref() == Some(name) && !imports_itself(entry))
            .collect();

        (!bound.is_empty()).then_some(bound)
    }

    fn glob_uses(&self, module: ModuleId) -> impl Iterator<Item = &UseEntry> {
        self.scope_uses(module).filter(|entry| entry.glob)
    }

    fn extern_crate(&self, name: &str, from: ModuleId) -> Option<Dependency> {
        if BUILTIN_CRATES.contains(&name) {
            return None;
        }

        let tree = &self.trees[self.modules[from].tree];
        let crate_info: &Crate = &self.workspace.crates[tree.krate];

        if tree.kind != TargetKind::Lib && crate_info.lib_name == name {
            return self.lib_roots[tree.krate].map(|_| Dependency::Crate(tree.krate));
        }

        let allowed = |kind: DependencyKind| match tree.kind {
            TargetKind::Lib | TargetKind::Bin => kind == DependencyKind::Normal,
            TargetKind::Build => kind == DependencyKind::Build,
            TargetKind::Test | TargetKind::Example | TargetKind::Bench => {
                kind != DependencyKind::Build
            }
        };
        let dependency = crate_info
            .dependencies
            .iter()
            .find(|dependency| dependency.name_in_code == name && allowed(dependency.kind))?;

        Some(match self.workspace.workspace_dependency(dependency) {
            Some(other) => Dependency::Crate(other),
            None => Dependency::External(dependency.package.clone()),
        })
    }

    fn include_links(&self, file: &RepoPath, facts: &SourceFacts, krate: usize) -> Vec<Link> {
        let crate_info = &self.workspace.crates[krate];
        let mut links = Vec::new();

        for (line, text) in &facts.strings {
            for base in [
                crate_info.directory.as_str(),
                parent_directory(file.as_str()),
            ] {
                let Some(path) = normalized_join(base, text) else {
                    continue;
                };

                if path != file.as_str() && self.repo_files.contains(path.as_str()) {
                    links.push(Link {
                        dependent: file.clone(),
                        dependency: Dependency::Path(path),
                        kind: EdgeKind::Reads,
                        inference: Some(Inference {
                            line: *line,
                            kind: FILE_PATH,
                            pattern: text.clone(),
                            detail: None,
                        }),
                    });
                }
            }
        }

        for include in &facts.includes {
            let base = match include.base {
                IncludeBase::File => parent_directory(file.as_str()).to_owned(),
                IncludeBase::ManifestDirectory => crate_info.directory.clone(),
                IncludeBase::OutDirectory => {
                    if let Some(build) = crate_info
                        .targets
                        .iter()
                        .find(|target| target.kind == TargetKind::Build)
                        .and_then(|target| RepoPath::new(target.root.clone()).ok())
                    {
                        links.push(Link {
                            dependent: file.clone(),
                            dependency: Dependency::File(build),
                            kind: EdgeKind::Import,
                            inference: None,
                        });
                    }

                    continue;
                }
            };
            let Some(path) = normalized_join(&base, &include.path) else {
                continue;
            };
            let (kind, inference_kind) = match (include.kind, &include.base) {
                (IncludeKind::Code, _) => (EdgeKind::Import, INCLUDED_FILE),
                (IncludeKind::Data, IncludeBase::ManifestDirectory) => {
                    (EdgeKind::Reads, MANIFEST_DIRECTORY_PATH)
                }
                (IncludeKind::Data, _) => (EdgeKind::Reads, INCLUDED_FILE),
            };

            links.push(Link {
                dependent: file.clone(),
                dependency: Dependency::Path(path),
                kind,
                inference: Some(Inference {
                    line: include.line,
                    kind: inference_kind,
                    pattern: include.path.clone(),
                    detail: None,
                }),
            });
        }

        links
    }
}
