use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use prober_core::changes::FileChange;
use prober_core::config::InferenceSettings;
use prober_core::files::RepoPath;
use prober_core::frontend::{ChangeSet, Frontend};
use prober_core::graph::{Edge, EdgeKind, GraphBuilder, UnresolvedSite};
use prober_core::report::ModeReason;
use rayon::prelude::*;

use crate::conventions::{
    ALWAYS_ALL_DEFAULTS, CONFTEST, LANGUAGE, MANIFEST_DEFAULTS, PACKAGE_INIT, is_module_file,
    is_source_file, is_test_file, is_test_support_file, join_directory, parent_directory,
    split_path,
};
use crate::families::naming_families;
use crate::links::{LinkSettings, Linking};
use crate::manifests::{DependencyChanges, PACKAGE_NODE_PREFIX};
use crate::modules::{ImportTarget, ModuleIndex};
use crate::summarizer::summarize;
use crate::summary::ModuleSummary;

pub struct PythonLinker {
    repo_root: PathBuf,
    index: ModuleIndex,
    inference: InferenceSettings,
    summaries: OnceLock<HashMap<RepoPath, ModuleSummary>>,
    dependency_changes: DependencyChanges,
}

impl PythonLinker {
    pub fn new(repo_root: &Path, repo_files: &[RepoPath]) -> Result<Self> {
        let repo_root = repo_root
            .canonicalize()
            .with_context(|| format!("cannot open repository root {}", repo_root.display()))?;
        let read_head = |path: &RepoPath| fs::read_to_string(repo_root.join(path.as_str())).ok();
        let index = ModuleIndex::new(repo_files, &read_head);

        Ok(Self {
            repo_root,
            index,
            inference: InferenceSettings::default(),
            summaries: OnceLock::new(),
            dependency_changes: DependencyChanges::default(),
        })
    }

    pub fn with_inference(self, inference: InferenceSettings) -> Self {
        Self { inference, ..self }
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

                Ok((path.clone(), summarize(path, &source, &self.index)))
            })
            .collect::<Result<HashMap<RepoPath, ModuleSummary>>>()?;

        Ok(self.summaries.get_or_init(|| summaries))
    }

    fn link(&self, change_set: &ChangeSet, graph: &mut GraphBuilder) -> Result<()> {
        let repo_files = change_set.repo_files;
        let summaries = self.summaries(repo_files)?;
        let mut modules: Vec<&RepoPath> = summaries.keys().collect();

        modules.sort();
        graph.add_language(LANGUAGE, is_module_file);

        if self.inference.test_support {
            graph.add_test_support(LANGUAGE, is_test_support_file);
        }

        add_removed_tests(repo_files, change_set.changes, graph);

        let tests: BTreeSet<RepoPath> = modules
            .iter()
            .filter(|path| is_test_file(path))
            .map(|path| (*path).clone())
            .collect();
        let outside_packages = modules_outside_packages(&modules, &self.index);

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
                let dependencies: Vec<RepoPath> = match &import.target {
                    ImportTarget::Repo(files) => files.clone(),
                    ImportTarget::External(name) => {
                        let mut dependencies = vec![package_node(name)];

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
                };

                for dependency in dependencies
                    .into_iter()
                    .filter(|dependency| dependency != module)
                {
                    let dependency = graph.add_file(dependency);

                    graph.add_edge(Edge {
                        dependent: file,
                        dependency,
                        kind,
                    });
                }
            }
        }

        self.add_test_runner_files(repo_files, &tests, graph);

        let links = Linking {
            repo_root: &self.repo_root,
            repo_files,
            index: &self.index,
            summaries,
            settings: LinkSettings {
                dynamic_imports: self.inference.dynamic_imports,
                plugin_folders: self.inference.plugin_folders,
                files: self.inference.files,
            },
        }
        .links()?;

        for link in links.links {
            let reader = graph.add_file(link.reader);
            let file = graph.add_file(link.file);
            let edge = Edge {
                dependent: reader,
                dependency: file,
                kind: link.kind,
            };

            graph.add_inferred_edge(edge, link.inference);
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
            graph.add_changed_part(
                package.change_path.clone(),
                package_node(&package.import_name),
                package.description.clone(),
            );
        }

        Ok(())
    }

    fn add_test_runner_files(
        &self,
        repo_files: &[RepoPath],
        tests: &BTreeSet<RepoPath>,
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

        ALWAYS_ALL_DEFAULTS
            .iter()
            .chain(manifest_patterns)
            .copied()
            .collect()
    }

    fn test_files(&self, repo_files: &[RepoPath]) -> Vec<RepoPath> {
        repo_files
            .iter()
            .filter(|path| is_test_file(path))
            .cloned()
            .collect()
    }

    fn diff_manifests(&mut self, change_set: &ChangeSet) -> Result<Vec<ModeReason>> {
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
        let repo_root = &self.repo_root;
        let read_head = |path: &RepoPath| fs::read_to_string(repo_root.join(path.as_str())).ok();

        self.dependency_changes =
            DependencyChanges::between(&read_head, change_set.changes, change_set.base, &imported)?;

        Ok(self.dependency_changes.global_reasons().to_vec())
    }

    fn add_links(
        &self,
        change_set: &ChangeSet,
        graph: &mut GraphBuilder,
    ) -> Result<Vec<ModeReason>> {
        self.link(change_set, graph)?;

        Ok(Vec::new())
    }
}

fn package_node(import_name: &str) -> RepoPath {
    RepoPath::new(format!(
        "{PACKAGE_NODE_PREFIX}{}",
        import_name.to_ascii_lowercase()
    ))
    .expect("import names are valid path segments")
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

fn add_removed_tests(repo_files: &[RepoPath], changes: &[FileChange], graph: &mut GraphBuilder) {
    let present: HashSet<&RepoPath> = repo_files.iter().collect();

    for removed_test in changes
        .iter()
        .flat_map(FileChange::touched_paths)
        .filter(|path| !present.contains(path) && is_test_file(path))
    {
        graph.add_removed_test(removed_test.clone());
    }
}
