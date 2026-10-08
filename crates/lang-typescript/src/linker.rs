use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use prober_core::config::InferenceSettings;
use prober_core::files::FileId;
use prober_core::files::RepoPath;
use prober_core::graph::{Edge, EdgeKind, Graph, GraphBuilder, UnresolvedSite};
use prober_core::report::ModeReason;
use rayon::prelude::*;

use crate::conventions::{
    LANGUAGE, is_module_file, is_runner_config, is_source_file, is_test_file,
    is_test_runner_package,
};
use crate::dynamic::{DynamicInference, DynamicLoading};
use crate::file_system::RepoFileSystem;
use crate::imports::{parse_imports, string_literals};
use crate::manifests::{ManifestChanges, PackageChange};
use crate::resolver::{ModuleResolver, package_node};
use crate::workspaces::{PACKAGE_MANIFEST, discover_workspace, is_workspace_manifest};

const PACKAGE_NODE_PREFIX: &str = "node_modules/";

pub struct TypeScriptLinker {
    repo_root: PathBuf,
    resolver: ModuleResolver,
    inference: InferenceSettings,
}

struct Dependency {
    path: RepoPath,
    kind: EdgeKind,
}

struct LinkedFile {
    path: RepoPath,
    dependencies: Vec<Dependency>,
    has_dynamic_sites: bool,
    resolution_change: Option<String>,
}

impl TypeScriptLinker {
    pub fn new(repo_root: &Path, repo_files: &[RepoPath]) -> Result<Self> {
        let repo_root = repo_root
            .canonicalize()
            .with_context(|| format!("cannot open repository root {}", repo_root.display()))?;
        let read_head = |path: &RepoPath| fs::read_to_string(repo_root.join(path.as_str())).ok();
        let manifest_paths = repo_files.iter().filter(|path| is_workspace_manifest(path));
        let packages = discover_workspace(manifest_paths, &read_head).packages;
        let file_system =
            RepoFileSystem::at(&repo_root).with_links(package_links(&repo_root, &packages));
        let resolver = ModuleResolver::new(&repo_root, file_system, packages.into_keys().collect());

        Ok(Self {
            repo_root,
            resolver,
            inference: InferenceSettings::default(),
        })
    }

    pub fn with_inference(self, inference: InferenceSettings) -> Self {
        Self { inference, ..self }
    }

    pub fn link(&self, repo_files: &[RepoPath], graph: &mut GraphBuilder) -> Result<()> {
        self.link_changes(repo_files, &ManifestChanges::default(), graph)
    }

    pub fn link_changes(
        &self,
        repo_files: &[RepoPath],
        manifest_changes: &ManifestChanges,
        graph: &mut GraphBuilder,
    ) -> Result<()> {
        let base_resolver = manifest_changes
            .resolution_change()
            .map(|_| self.base_resolver(repo_files, manifest_changes));
        let linked_files = repo_files
            .par_iter()
            .filter(|path| is_source_file(path))
            .map(|path| self.link_file(path, base_resolver.as_ref()))
            .collect::<Result<Vec<LinkedFile>>>()?;
        let dynamic_inference = self.infer_dynamic_links(repo_files, &linked_files)?;

        graph.add_language(LANGUAGE, |path| is_module_file(path) && !is_test_file(path));

        for linked_file in linked_files {
            let file = if is_test_file(&linked_file.path) {
                graph.add_test_file(linked_file.path.clone(), LANGUAGE)
            } else {
                graph.add_file(linked_file.path.clone())
            };

            for dependency in linked_file.dependencies {
                let dependency_file = graph.add_file(dependency.path);

                graph.add_edge(Edge {
                    dependent: file,
                    dependency: dependency_file,
                    kind: dependency.kind,
                });
            }

            if let (Some(specifier), Some(resolution_change)) = (
                linked_file.resolution_change,
                manifest_changes.resolution_change(),
            ) {
                let description =
                    format!("{} resolves {specifier:?} differently", linked_file.path);

                graph.add_changed_part(resolution_change.clone(), linked_file.path, description);
            }
        }

        add_dynamic_inference(dynamic_inference, graph);
        add_manifest_changes(manifest_changes, graph);

        Ok(())
    }

    pub fn root_scope_reasons(
        &self,
        graph: &Graph,
        repo_files: &[RepoPath],
        manifest_changes: &ManifestChanges,
    ) -> Result<Vec<ModeReason>> {
        if manifest_changes.packages().is_empty() {
            return Ok(Vec::new());
        }

        let root_scope_packages = self.root_scope_packages(graph, repo_files)?;
        let mut root_scope_changes: BTreeMap<&RepoPath, Vec<&PackageChange>> = BTreeMap::new();

        for package_change in manifest_changes.packages() {
            if is_test_runner_package(&package_change.name)
                || root_scope_packages.contains(&package_change.name)
            {
                root_scope_changes
                    .entry(&package_change.change.path)
                    .or_default()
                    .push(package_change);
            }
        }

        let root_scope_reasons = root_scope_changes
            .into_values()
            .filter_map(|package_changes| {
                let first_change = package_changes.iter().min_by_key(|package_change| {
                    (package_change.through.is_some(), &package_change.name)
                })?;
                let detail = match &first_change.through {
                    None => first_change.detail.clone(),
                    Some(dependency) => format!("through {dependency} {}", first_change.detail),
                };

                Some(ModeReason::RootScopePackage {
                    change: first_change.change.clone(),
                    package: first_change.name.clone(),
                    detail,
                    other_packages: package_changes.len() - 1,
                })
            })
            .collect();

        Ok(root_scope_reasons)
    }

    fn root_scope_packages(
        &self,
        graph: &Graph,
        repo_files: &[RepoPath],
    ) -> Result<BTreeSet<String>> {
        let runner_configs: Vec<&RepoPath> = repo_files
            .iter()
            .filter(|path| is_runner_config(path) && is_source_file(path))
            .collect();
        let mut files_to_visit: VecDeque<FileId> = VecDeque::new();

        for runner_config in runner_configs {
            let source_bytes = fs::read(self.repo_root.join(runner_config.as_str()))
                .with_context(|| format!("cannot read {runner_config}"))?;
            let source_text = String::from_utf8_lossy(&source_bytes);
            let referenced_files = string_literals(runner_config, &source_text)
                .iter()
                .flat_map(|literal| self.referenced_files(runner_config, literal))
                .filter(|referenced_file| !is_test_file(referenced_file))
                .collect::<Vec<_>>();

            files_to_visit.extend(
                std::iter::once(runner_config)
                    .chain(&referenced_files)
                    .filter_map(|path| graph.file_id(path)),
            );
        }

        let mut visited_files: HashSet<FileId> = files_to_visit.iter().copied().collect();
        let mut packages = BTreeSet::new();

        while let Some(file) = files_to_visit.pop_front() {
            let runtime_dependencies = graph
                .dependencies(file)
                .iter()
                .filter(|edge| edge.kind.selects_tests());

            for edge in runtime_dependencies {
                let dependency_path = graph.path(edge.dependency).as_str();

                if let Some(package_name) = dependency_path.strip_prefix(PACKAGE_NODE_PREFIX) {
                    packages.insert(package_name.to_owned());
                } else if visited_files.insert(edge.dependency) {
                    files_to_visit.push_back(edge.dependency);
                }
            }
        }

        Ok(packages)
    }

    fn referenced_files(&self, config: &RepoPath, literal: &str) -> Vec<RepoPath> {
        let literal = literal.strip_prefix("<rootDir>/").unwrap_or(literal);
        let looks_like_a_path = !literal.is_empty()
            && literal.len() < 256
            && !matches!(literal, "." | "..")
            && !literal.contains(|character: char| {
                character.is_whitespace() || "*?{}[]:".contains(character)
            });

        if !looks_like_a_path {
            return Vec::new();
        }

        let relative_specifier = if literal.starts_with("./") || literal.starts_with("../") {
            literal.to_owned()
        } else {
            format!("./{}", literal.trim_start_matches('/'))
        };
        let root_file = RepoPath::new(PACKAGE_MANIFEST).expect("valid repo path");

        [config, &root_file]
            .into_iter()
            .flat_map(|importer| self.resolver.resolve(importer, &relative_specifier))
            .filter(|referenced_file| !referenced_file.as_str().starts_with(PACKAGE_NODE_PREFIX))
            .collect()
    }

    fn base_resolver(
        &self,
        repo_files: &[RepoPath],
        manifest_changes: &ManifestChanges,
    ) -> ModuleResolver {
        let base_configs: HashMap<&RepoPath, Option<&[u8]>> = manifest_changes
            .base_configs()
            .iter()
            .map(|(path, bytes)| (path, bytes.as_deref()))
            .collect();
        let read_base = |path: &RepoPath| match base_configs.get(path) {
            Some(bytes) => bytes.and_then(|bytes| String::from_utf8(bytes.to_vec()).ok()),
            None => fs::read_to_string(self.repo_root.join(path.as_str())).ok(),
        };
        let manifest_paths: BTreeSet<&RepoPath> = repo_files
            .iter()
            .chain(base_configs.keys().copied())
            .filter(|path| is_workspace_manifest(path))
            .collect();
        let packages = discover_workspace(manifest_paths, &read_base).packages;
        let overlay = manifest_changes
            .base_configs()
            .iter()
            .map(|(path, bytes)| (self.repo_root.join(path.as_str()), bytes.clone()));
        let file_system = RepoFileSystem::at(&self.repo_root)
            .with_links(package_links(&self.repo_root, &packages))
            .with_overlay(overlay);

        ModuleResolver::new(&self.repo_root, file_system, packages.into_keys().collect())
    }

    fn infer_dynamic_links(
        &self,
        repo_files: &[RepoPath],
        linked_files: &[LinkedFile],
    ) -> Result<DynamicInference> {
        let mut importers: HashMap<RepoPath, Vec<RepoPath>> = HashMap::new();
        let seeds = linked_files
            .iter()
            .filter(|linked_file| linked_file.has_dynamic_sites)
            .map(|linked_file| linked_file.path.clone())
            .collect();

        for linked_file in linked_files {
            for dependency in &linked_file.dependencies {
                if dependency.kind == EdgeKind::Import {
                    importers
                        .entry(dependency.path.clone())
                        .or_default()
                        .push(linked_file.path.clone());
                }
            }
        }

        for module_importers in importers.values_mut() {
            module_importers.sort();
            module_importers.dedup();
        }

        DynamicLoading {
            repo_root: &self.repo_root,
            resolver: &self.resolver,
            repo_files,
            importers: &importers,
            settings: self.inference,
        }
        .infer(seeds)
    }

    fn link_file(
        &self,
        path: &RepoPath,
        base_resolver: Option<&ModuleResolver>,
    ) -> Result<LinkedFile> {
        let source_bytes = fs::read(self.repo_root.join(path.as_str()))
            .with_context(|| format!("cannot read {path}"))?;
        let source_text = String::from_utf8_lossy(&source_bytes);
        let parsed_imports = parse_imports(path, &source_text);
        let mut dependencies = Vec::new();
        let mut resolution_change = None;

        for import_request in parsed_imports.import_requests {
            let resolved_paths = self.resolver.resolve(path, &import_request.specifier);
            let resolves_differently_at_base = resolution_change.is_none()
                && base_resolver.is_some_and(|base_resolver| {
                    base_resolver.resolve(path, &import_request.specifier) != resolved_paths
                });

            if resolves_differently_at_base {
                resolution_change = Some(import_request.specifier.clone());
            }

            dependencies.extend(
                resolved_paths
                    .into_iter()
                    .map(|dependency_path| Dependency {
                        path: dependency_path,
                        kind: import_request.kind,
                    }),
            );
        }

        Ok(LinkedFile {
            path: path.clone(),
            dependencies,
            has_dynamic_sites: parsed_imports.has_dynamic_sites,
            resolution_change,
        })
    }
}

fn package_links(repo_root: &Path, packages: &BTreeMap<String, String>) -> Vec<(String, PathBuf)> {
    packages
        .iter()
        .map(|(name, directory)| (name.clone(), repo_root.join(directory)))
        .collect()
}

fn add_manifest_changes(manifest_changes: &ManifestChanges, graph: &mut GraphBuilder) {
    for package_change in manifest_changes.packages() {
        graph.add_changed_part(
            package_change.change.path.clone(),
            package_node(&package_change.name),
            package_change.description(),
        );
    }

    for affected_file in manifest_changes.affected_files() {
        let description = format!(
            "{} is affected by field {:?}",
            affected_file.file, affected_file.field
        );

        graph.add_changed_part(
            affected_file.change_path.clone(),
            affected_file.file.clone(),
            description,
        );
    }
}

fn add_dynamic_inference(dynamic_inference: DynamicInference, graph: &mut GraphBuilder) {
    for dynamic_link in dynamic_inference.links {
        let dependent = graph.add_file(dynamic_link.dependent);
        let dependency = graph.add_file(dynamic_link.dependency);
        let dynamic_edge = Edge {
            dependent,
            dependency,
            kind: EdgeKind::Dynamic,
        };

        graph.add_inferred_edge(dynamic_edge, dynamic_link.inference);
    }

    for (folder, files) in dynamic_inference.folders {
        let folder_node = graph.add_file(folder);

        for file in files {
            let file_node = graph.add_file(file);

            graph.add_edge(Edge {
                dependent: folder_node,
                dependency: file_node,
                kind: EdgeKind::Contains,
            });
        }
    }

    for dynamic_site in dynamic_inference.unresolved_sites {
        let file = graph.add_file(dynamic_site.module);

        graph.add_unresolved_site(UnresolvedSite {
            file,
            line: dynamic_site.line,
            expression: dynamic_site.expression,
            cause: dynamic_site.cause,
            language: LANGUAGE,
        });
    }
}
