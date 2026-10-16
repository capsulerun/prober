use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use prober_core::changes::{ChangeKind, FileChange};
use prober_core::config::{InferenceSettings, Presets};
use prober_core::files::{FileId, RepoPath};
use prober_core::frontend::{ChangeSet, Frontend};
use prober_core::graph::{Edge, EdgeKind, GraphBuilder, Inference, UnresolvedSite};
use prober_core::report::ModeReason;
use rayon::prelude::*;

use crate::conventions::{
    ALWAYS_ALL_DEFAULTS, CONFTEST, LANGUAGE, MANIFEST_DEFAULTS, PACKAGE_INIT,
    PYTEST_CONFIG_DEFAULTS, is_module_file, is_source_file, is_test_file, is_test_support_file,
    join_directory, parent_directory, split_path,
};
use crate::django::{installed_app_links, settings_modules};
use crate::families::naming_families;
use crate::links::{LinkSettings, Linking};
use crate::manifests::{
    DependencyChanges, PACKAGE_NODE_PREFIX, PackageChange, ProjectChange, project_directories,
    project_of,
};
use crate::modules::{ImportTarget, ModuleIndex};
use crate::pytest::configs::{PytestConfigs, is_within};
use crate::pytest::fixtures::{FixtureScopes, ModuleEdge};
use crate::pytest::{ConfigChanges, global_plugins, scoped_requests};
use crate::sessions::session_changes;
use crate::summarizer::summarize;
use crate::summary::ModuleSummary;
use crate::symbols::Analysis;

const PYTEST_PRESET: &str = "pytest";

const DJANGO_PRESET: &str = "django";

const PYTHON_BINDING: &str = "Python binding";

pub struct PythonLinker {
    repo_root: PathBuf,
    index: ModuleIndex,
    configs: PytestConfigs,
    inference: InferenceSettings,
    pytest: bool,
    django: bool,
    summaries: OnceLock<HashMap<RepoPath, ModuleSummary>>,
    dependency_changes: DependencyChanges,
    session_changes: Vec<ProjectChange>,
}

impl PythonLinker {
    pub fn new(repo_root: &Path, repo_files: &[RepoPath]) -> Result<Self> {
        let repo_root = repo_root
            .canonicalize()
            .with_context(|| format!("cannot open repository root {}", repo_root.display()))?;
        let read_head = |path: &RepoPath| fs::read_to_string(repo_root.join(path.as_str())).ok();
        let index = ModuleIndex::new(repo_files, &read_head);
        let configs = PytestConfigs::read(repo_files, &read_head);

        Ok(Self {
            repo_root,
            index,
            configs,
            inference: InferenceSettings::default(),
            pytest: true,
            django: true,
            summaries: OnceLock::new(),
            dependency_changes: DependencyChanges::default(),
            session_changes: Vec::new(),
        })
    }

    pub fn with_inference(self, inference: InferenceSettings) -> Self {
        Self { inference, ..self }
    }

    pub fn with_presets(self, presets: &Presets) -> Self {
        let enabled = |preset: &str| match presets {
            Presets::Auto => true,
            Presets::Only(names) => names.iter().any(|name| name == preset),
        };

        Self {
            pytest: enabled(PYTEST_PRESET),
            django: enabled(DJANGO_PRESET),
            ..self
        }
    }

    fn analyses_symbols(&self) -> bool {
        self.pytest && self.inference.symbols
    }

    fn is_test(&self, path: &RepoPath) -> bool {
        if self.pytest {
            self.configs.is_test(path)
        } else {
            is_test_file(path)
        }
    }

    fn read_head(&self, path: &RepoPath) -> Option<String> {
        fs::read_to_string(self.repo_root.join(path.as_str())).ok()
    }

    fn summaries(&self, repo_files: &[RepoPath]) -> Result<&HashMap<RepoPath, ModuleSummary>> {
        if let Some(summaries) = self.summaries.get() {
            return Ok(summaries);
        }

        let summaries = repo_files
            .par_iter()
            .filter(|path| is_source_file(path))
            .map(|path| {
                let bytes = fs::read(self.repo_root.join(path.as_str()))
                    .with_context(|| format!("cannot read {path}"))?;
                let source = String::from_utf8_lossy(&bytes);

                Ok((
                    path.clone(),
                    summarize(path, &source, &self.index, self.analyses_symbols()),
                ))
            })
            .collect::<Result<HashMap<RepoPath, ModuleSummary>>>()?;

        Ok(self.summaries.get_or_init(|| summaries))
    }

    fn link(&self, change_set: &ChangeSet, graph: &mut GraphBuilder) -> Result<Vec<ModeReason>> {
        let repo_files = change_set.repo_files;
        let summaries = self.summaries(repo_files)?;
        let mut modules: Vec<&RepoPath> = summaries.keys().collect();

        modules.sort();
        graph.add_language(LANGUAGE, is_module_file);

        if self.inference.test_support {
            graph.add_test_support(LANGUAGE, is_test_support_file);
        }

        self.add_removed_tests(repo_files, change_set.changes, graph);

        let tests: BTreeSet<RepoPath> = modules
            .iter()
            .filter(|path| self.is_test(path))
            .map(|path| (*path).clone())
            .collect();
        let outside_packages = modules_outside_packages(&modules, &self.index);
        let environments = Environments::new(repo_files, self.dependency_changes.packages());
        let mut module_edges: HashMap<RepoPath, Vec<ModuleEdge>> = HashMap::new();

        for &module in &modules {
            let file = if tests.contains(module) {
                graph.add_test_file(module.clone(), LANGUAGE)
            } else {
                graph.add_file(module.clone())
            };

            for import in &summaries[module].imports {
                let kind = if import.type_only {
                    EdgeKind::TypeOnly
                } else {
                    EdgeKind::Import
                };
                for dependency in
                    import_dependencies(module, &import.target, &outside_packages, &environments)
                        .into_iter()
                        .filter(|dependency| dependency != module)
                {
                    module_edges
                        .entry(module.clone())
                        .or_default()
                        .push(ModuleEdge {
                            dependency: dependency.clone(),
                            kind,
                        });

                    let dependency = graph.add_file(dependency);

                    graph.add_edge(Edge {
                        dependent: file,
                        dependency,
                        kind,
                    });
                }
            }

            if self.inference.bindings {
                self.add_native_imports(file, &summaries[module], graph);
            }
        }

        self.add_test_runner_files(repo_files, &tests, &mut module_edges, graph);

        let read_head = |path: &RepoPath| self.read_head(path);
        let settings = if self.django {
            settings_modules(&self.configs, &self.index, repo_files, &read_head)
        } else {
            Vec::new()
        };

        for setting in &settings {
            for test in tests
                .iter()
                .filter(|test| is_within(test.as_str(), &setting.directory))
            {
                let test_file = graph.add_file(test.clone());
                let settings_file = graph.add_file(setting.module.clone());

                graph.add_edge(Edge {
                    dependent: test_file,
                    dependency: settings_file,
                    kind: EdgeKind::Import,
                });
                module_edges
                    .entry(test.clone())
                    .or_default()
                    .push(ModuleEdge {
                        dependency: setting.module.clone(),
                        kind: EdgeKind::Import,
                    });
            }
        }

        let links = Linking {
            repo_root: &self.repo_root,
            repo_files,
            index: &self.index,
            summaries,
            tests: &tests,
            settings: LinkSettings {
                dynamic_imports: self.inference.dynamic_imports,
                plugin_folders: self.inference.plugin_folders,
                files: self.inference.files,
                attribute_paths: self.analyses_symbols(),
            },
        }
        .links()?;

        let app_links = installed_app_links(&settings, summaries, &self.index);

        for link in links.links.into_iter().chain(app_links) {
            let reader = graph.add_file(link.reader);
            let file = graph.add_file(link.file);
            let edge = Edge {
                dependent: reader,
                dependency: file,
                kind: link.kind,
            };

            graph.add_inferred_edge(edge, link.inference);
        }

        for attribute_path in &links.attribute_paths {
            let reader = graph.add_file(attribute_path.reader.clone());

            for loaded in attribute_path
                .packages
                .iter()
                .chain([&attribute_path.module])
                .filter(|loaded| **loaded != attribute_path.reader)
            {
                let loaded = graph.add_file(loaded.clone());
                let edge = Edge {
                    dependent: reader,
                    dependency: loaded,
                    kind: EdgeKind::Import,
                };

                graph.add_inferred_edge(edge, attribute_path.inference.clone());
            }
        }

        for (folder, files) in links.folders {
            let folder = graph.add_file(folder);

            for file in files {
                let file = graph.add_file(file);

                graph.add_edge(Edge {
                    dependent: folder,
                    dependency: file,
                    kind: EdgeKind::Contains,
                });
            }
        }

        for unresolved in links.unresolved {
            let file = graph.add_file(unresolved.module);

            graph.add_unresolved_site(UnresolvedSite {
                file,
                line: unresolved.line,
                expression: unresolved.expression,
                cause: unresolved.cause,
                language: LANGUAGE,
            });
        }

        if self.inference.naming_families {
            for (test, file) in naming_families(repo_files, &tests, self.index.top_level_packages())
            {
                let test = graph.add_file(test.clone());
                let file = graph.add_file(file.clone());

                graph.add_edge(Edge {
                    dependent: test,
                    dependency: file,
                    kind: EdgeKind::Family,
                });
            }
        }

        for package in self.dependency_changes.packages() {
            let node = match &package.project {
                Some(project) => environment_package_node(project, &package.import_name),
                None => package_node(&package.import_name),
            };

            graph.add_changed_part(
                package.change_path.clone(),
                node,
                package.description.clone(),
            );
        }

        for session in &self.session_changes {
            let in_session = |path: &&RepoPath| {
                session.directory.is_empty()
                    || path
                        .as_str()
                        .starts_with(&format!("{}/", session.directory))
            };

            for test in repo_files
                .iter()
                .filter(in_session)
                .filter(|path| is_source_file(path) && self.is_test(path))
            {
                graph.add_changed_part(
                    session.change_path.clone(),
                    test.clone(),
                    session.description.clone(),
                );
            }
        }

        for project in self.dependency_changes.projects() {
            let prefix = format!("{}/", project.directory);

            for file in repo_files
                .iter()
                .filter(|path| path.as_str().starts_with(&prefix) && is_source_file(path))
            {
                graph.add_changed_part(
                    project.change_path.clone(),
                    file.clone(),
                    project.description.clone(),
                );
            }
        }

        if !self.pytest {
            return Ok(Vec::new());
        }

        if self.analyses_symbols() {
            let global_plugins = global_plugins(&self.configs, &self.index, repo_files, &read_head);
            let providers = self.providers(&modules, summaries, &global_plugins);
            let base_summaries = self.base_summaries(summaries, change_set)?;
            let dependencies = |importer: &RepoPath, target: &ImportTarget| {
                import_dependencies(importer, target, &outside_packages, &environments)
            };
            let analysed = Analysis {
                summaries,
                base_summaries: &base_summaries,
                index: &self.index,
                tests: &tests,
                providers: &providers,
                module_edges: &module_edges,
                dependencies: &dependencies,
                attribute_paths: &links.attribute_paths,
            }
            .add_to(graph);

            FixtureScopes {
                summaries,
                index: &self.index,
                tests: &tests,
                global_plugins,
                scoped_requests: scoped_requests(&self.configs),
                module_edges: &module_edges,
                analysed: &analysed,
            }
            .add_to(graph, change_set.changes, change_set.base)?;
        }

        ConfigChanges {
            configs: &self.configs,
            tests: &tests,
            changes: change_set.changes,
            base: change_set.base,
            read_head: &read_head,
        }
        .add_to(graph)
    }

    fn add_test_runner_files(
        &self,
        repo_files: &[RepoPath],
        tests: &BTreeSet<RepoPath>,
        module_edges: &mut HashMap<RepoPath, Vec<ModuleEdge>>,
        graph: &mut GraphBuilder,
    ) {
        let conftests: HashSet<&str> = repo_files
            .iter()
            .map(RepoPath::as_str)
            .filter(|path| split_path(path).1 == CONFTEST)
            .collect();

        for test in tests {
            let file = graph.add_file(test.clone());
            let mut runner_files = Vec::new();
            let mut directory = parent_directory(test.as_str());
            let mut in_package = true;

            loop {
                let conftest = join_directory(directory, CONFTEST);
                let init = join_directory(directory, PACKAGE_INIT);

                if conftests.contains(conftest.as_str()) {
                    runner_files.push(conftest);
                }

                in_package = in_package && self.index.contains(&init);

                if in_package && let Ok(init) = RepoPath::new(init.clone()) {
                    module_edges
                        .entry(test.clone())
                        .or_default()
                        .push(ModuleEdge {
                            dependency: init,
                            kind: EdgeKind::Import,
                        });
                }

                if in_package {
                    runner_files.push(init);
                }

                if directory.is_empty() {
                    break;
                }

                directory = parent_directory(directory);
            }

            for runner_file in runner_files {
                let Ok(runner_file) = RepoPath::new(runner_file) else {
                    continue;
                };
                let runner_file = graph.add_file(runner_file);

                graph.add_edge(Edge {
                    dependent: file,
                    dependency: runner_file,
                    kind: EdgeKind::Import,
                });
            }
        }
    }
}

impl Frontend for PythonLinker {
    fn language(&self) -> &'static str {
        LANGUAGE
    }

    fn always_all_patterns(&self) -> Vec<&'static str> {
        let manifest_patterns = if self.inference.manifests {
            &[][..]
        } else {
            MANIFEST_DEFAULTS
        };
        let config_patterns = if self.pytest {
            &[][..]
        } else {
            PYTEST_CONFIG_DEFAULTS
        };

        ALWAYS_ALL_DEFAULTS
            .iter()
            .chain(manifest_patterns)
            .chain(config_patterns)
            .copied()
            .collect()
    }

    fn test_files(&self, repo_files: &[RepoPath]) -> Vec<RepoPath> {
        repo_files
            .iter()
            .filter(|path| is_source_file(path) && self.is_test(path))
            .cloned()
            .collect()
    }

    fn diff_manifests(&mut self, change_set: &ChangeSet) -> Result<Vec<ModeReason>> {
        let repo_root = &self.repo_root;
        let read_head = |path: &RepoPath| fs::read_to_string(repo_root.join(path.as_str())).ok();
        let project_directories = project_directories(change_set.repo_files);
        let is_test = |path: &RepoPath| is_source_file(path) && self.is_test(path);

        self.session_changes = session_changes(
            &read_head,
            change_set.changes,
            change_set.base,
            &project_directories,
            &is_test,
        )?;

        if !self.inference.manifests {
            return Ok(Vec::new());
        }

        let imported: HashSet<String> = self
            .summaries(change_set.repo_files)?
            .values()
            .flat_map(|summary| &summary.imports)
            .filter_map(|import| match &import.target {
                ImportTarget::External(name) => Some(name.to_ascii_lowercase()),
                _ => None,
            })
            .collect();
        self.dependency_changes = DependencyChanges::between(
            &read_head,
            change_set.changes,
            change_set.base,
            &imported,
            &project_directories,
            self.pytest,
        )?;

        Ok(self.dependency_changes.global_reasons().to_vec())
    }

    fn add_links(
        &self,
        change_set: &ChangeSet,
        graph: &mut GraphBuilder,
    ) -> Result<Vec<ModeReason>> {
        self.link(change_set, graph)
    }
}

fn package_node(import_name: &str) -> RepoPath {
    RepoPath::new(format!(
        "{PACKAGE_NODE_PREFIX}{}",
        import_name.to_ascii_lowercase()
    ))
    .expect("import names are valid path segments")
}

fn environment_package_node(project: &str, import_name: &str) -> RepoPath {
    RepoPath::new(format!(
        "{PACKAGE_NODE_PREFIX}{project}/{}",
        import_name.to_ascii_lowercase()
    ))
    .expect("project directories and import names are valid path segments")
}

struct Environments {
    project_directories: BTreeSet<String>,
    scoped_packages: HashSet<(String, String)>,
}

impl Environments {
    fn new(repo_files: &[RepoPath], packages: &[PackageChange]) -> Self {
        let scoped_packages: HashSet<(String, String)> = packages
            .iter()
            .filter_map(|package| {
                let project = package.project.clone()?;

                Some((project, package.import_name.to_ascii_lowercase()))
            })
            .collect();
        let project_directories = if scoped_packages.is_empty() {
            BTreeSet::new()
        } else {
            project_directories(repo_files)
        };

        Self {
            project_directories,
            scoped_packages,
        }
    }

    fn package_node(&self, importer: &RepoPath, name: &str) -> Option<RepoPath> {
        let project = project_of(importer, &self.project_directories)?;
        let key = (project.to_owned(), name.to_ascii_lowercase());

        self.scoped_packages
            .contains(&key)
            .then(|| environment_package_node(project, name))
    }
}

fn import_dependencies(
    importer: &RepoPath,
    target: &ImportTarget,
    outside_packages: &HashMap<&str, Vec<RepoPath>>,
    environments: &Environments,
) -> Vec<RepoPath> {
    match target {
        ImportTarget::Repo(files) => files.clone(),
        ImportTarget::External(name) => {
            let mut dependencies = vec![package_node(name)];

            dependencies.extend(environments.package_node(importer, name));

            dependencies.extend(
                outside_packages
                    .get(name.as_str())
                    .into_iter()
                    .flatten()
                    .cloned(),
            );

            dependencies
        }
        ImportTarget::Standard | ImportTarget::Missing => Vec::new(),
    }
}

fn modules_outside_packages<'m>(
    modules: &[&'m RepoPath],
    index: &ModuleIndex,
) -> HashMap<&'m str, Vec<RepoPath>> {
    let mut outside: HashMap<&str, Vec<RepoPath>> = HashMap::new();

    for &module in modules {
        let (directory, name) = split_path(module.as_str());
        let (package_directory, importable_name) = if name == PACKAGE_INIT {
            let (parent, package) = split_path(directory);

            (parent, package)
        } else {
            (directory, name.strip_suffix(".py").unwrap_or(name))
        };

        if !index.contains(&join_directory(package_directory, PACKAGE_INIT)) {
            outside
                .entry(importable_name)
                .or_default()
                .push(module.clone());
        }
    }

    outside
}

impl PythonLinker {
    fn providers(
        &self,
        modules: &[&RepoPath],
        summaries: &HashMap<RepoPath, ModuleSummary>,
        global_plugins: &[RepoPath],
    ) -> HashSet<RepoPath> {
        let mut providers: HashSet<RepoPath> = global_plugins.iter().cloned().collect();

        for &module in modules {
            if split_path(module.as_str()).1 == CONFTEST {
                providers.insert(module.clone());
            }

            for plugin in &summaries[module].pytest.plugins {
                if let Some(resolved) = self.index.resolve_module(module, plugin).into_iter().next()
                {
                    providers.insert(resolved.module);
                }
            }
        }

        providers
    }

    fn base_summaries(
        &self,
        summaries: &HashMap<RepoPath, ModuleSummary>,
        change_set: &ChangeSet,
    ) -> Result<HashMap<RepoPath, ModuleSummary>> {
        let mut base_summaries = HashMap::new();

        for change in change_set.changes.iter().filter(|change| {
            change.kind == ChangeKind::Modified && summaries.contains_key(&change.path)
        }) {
            let Some(bytes) = change_set.base.read_file(&change.path)? else {
                continue;
            };
            let source = String::from_utf8_lossy(&bytes);

            base_summaries.insert(
                change.path.clone(),
                summarize(&change.path, &source, &self.index, true),
            );
        }

        Ok(base_summaries)
    }

    fn add_native_imports(&self, file: FileId, summary: &ModuleSummary, graph: &mut GraphBuilder) {
        for native_import in &summary.native_imports {
            let native = &self.index.native_modules()[native_import.module];
            let manifest = graph.add_file(native.manifest.clone());

            graph.add_inferred_edge(
                Edge {
                    dependent: file,
                    dependency: manifest,
                    kind: EdgeKind::Binding,
                },
                Inference {
                    line: native_import.line,
                    kind: PYTHON_BINDING,
                    pattern: native.name.clone(),
                    detail: Some(native.declaration.to_string()),
                },
            );
        }
    }

    fn add_removed_tests(
        &self,
        repo_files: &[RepoPath],
        changes: &[FileChange],
        graph: &mut GraphBuilder,
    ) {
        let present: HashSet<&RepoPath> = repo_files.iter().collect();

        for removed_test in changes
            .iter()
            .flat_map(FileChange::touched_paths)
            .filter(|path| !present.contains(path) && is_source_file(path) && self.is_test(path))
        {
            graph.add_removed_test(removed_test.clone());
        }
    }
}
