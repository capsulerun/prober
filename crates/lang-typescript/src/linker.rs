use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use prober_core::changes::{BaseRevision, FileChange};
use prober_core::config::{InferenceSettings, Presets};
use prober_core::files::RepoPath;
use prober_core::graph::{Edge, EdgeKind, GraphBuilder, UnresolvedSite};
use prober_core::report::ModeReason;
use rayon::prelude::*;

use crate::conventions::{LANGUAGE, is_asset_file, is_module_file, is_source_file, is_test_file};
use crate::dynamic::evaluator::Evaluator;
use crate::dynamic::{DynamicInference, DynamicLoading};
use crate::file_system::RepoFileSystem;
use crate::imports::{GlobImport, MockRequest, parse_imports, string_literals};
use crate::manifests::ManifestChanges;
use crate::manifests::documents::{parse_jsonc, parse_yaml};
use crate::presets::patterns::join_directory;
use crate::presets::{RunnerGroup, RunnerPresets, ScopeInputs, imports_node_test, runner_groups};
use crate::resolver::{ModuleResolver, package_node};
use crate::root_scope::RootScopeEffects;
use crate::test_links::TestLinks;
use crate::workspaces::{
    PACKAGE_MANIFEST, discover_workspace, is_workspace_manifest, parent_directory,
};

pub(crate) const PACKAGE_NODE_PREFIX: &str = "node_modules/";

const RAW_IMPORT_QUERY: &str = "?raw";

pub struct TypeScriptLinker {
    repo_root: PathBuf,
    resolver: ModuleResolver,
    inference: InferenceSettings,
    presets: RunnerPresets,
}

pub struct LinkChanges<'c> {
    pub changes: &'c [FileChange],
    pub manifests: &'c ManifestChanges,
    pub base: &'c dyn BaseRevision,
}

struct NoBase;

impl BaseRevision for NoBase {
    fn read_file(&self, _: &RepoPath) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

pub(crate) struct Dependency {
    pub path: RepoPath,
    pub kind: EdgeKind,
}

pub(crate) struct LinkedFile {
    pub path: RepoPath,
    pub is_test: bool,
    pub dependencies: Vec<Dependency>,
    pub has_dynamic_sites: bool,
    pub resolution_change: Option<String>,
    pub mocks: Vec<MockRequest>,
    pub glob_imports: Vec<GlobImport>,
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
        let linker = Self {
            repo_root,
            resolver,
            inference: InferenceSettings::default(),
            presets: RunnerPresets::default(),
        };

        Ok(linker.with_presets(&Presets::Auto, repo_files))
    }

    pub fn with_inference(self, inference: InferenceSettings) -> Self {
        Self { inference, ..self }
    }

    pub fn with_presets(self, presets: &Presets, repo_files: &[RepoPath]) -> Self {
        let importers = HashMap::new();
        let evaluator = Evaluator::new(&self.repo_root, &self.resolver, &importers);
        let presets = RunnerPresets::read(presets, &self.repo_root, repo_files, &evaluator);

        Self { presets, ..self }
    }

    pub fn global_config_patterns(&self) -> Vec<&'static str> {
        self.presets.global_config_patterns()
    }

    pub fn test_files(&self, repo_files: &[RepoPath]) -> Vec<RepoPath> {
        repo_files
            .iter()
            .filter(|path| {
                is_test_file(path) || self.presets.is_test(path, self.reads_node_test(path))
            })
            .cloned()
            .collect()
    }

    fn reads_node_test(&self, path: &RepoPath) -> bool {
        self.presets.detects_node_test()
            && is_source_file(path)
            && fs::read_to_string(self.repo_root.join(path.as_str()))
                .is_ok_and(|source_text| source_text.contains("node:test"))
    }

    pub fn link(&self, repo_files: &[RepoPath], graph: &mut GraphBuilder) -> Result<()> {
        let no_changes = LinkChanges {
            changes: &[],
            manifests: &ManifestChanges::default(),
            base: &NoBase,
        };

        self.link_changes(repo_files, &no_changes, graph)?;

        Ok(())
    }

    pub fn link_changes(
        &self,
        repo_files: &[RepoPath],
        link_changes: &LinkChanges,
        graph: &mut GraphBuilder,
    ) -> Result<Vec<ModeReason>> {
        let manifest_changes = link_changes.manifests;
        let base_resolver = manifest_changes
            .resolution_change()
            .map(|_| self.base_resolver(repo_files, manifest_changes));
        let linked_files = repo_files
            .par_iter()
            .filter(|path| is_source_file(path))
            .map(|path| self.link_file(path, base_resolver.as_ref()))
            .collect::<Result<Vec<LinkedFile>>>()?;
        let dynamic_inference = self.infer_dynamic_links(repo_files, &linked_files)?;
        let groups = self.runner_groups(repo_files, &linked_files, &dynamic_inference);
        let tests: BTreeSet<RepoPath> = linked_files
            .iter()
            .filter(|linked_file| linked_file.is_test)
            .map(|linked_file| linked_file.path.clone())
            .collect();

        graph.add_language(LANGUAGE, |path| is_module_file(path) && !is_test_file(path));

        for linked_file in &linked_files {
            self.add_linked_file(linked_file, manifest_changes, graph);
        }

        let mocked_modules = TestLinks {
            repo_files,
            linked_files: &linked_files,
            resolver: &self.resolver,
            follows_glob_imports: self.presets.follows_glob_imports(),
        }
        .add_to(graph);

        add_dynamic_inference(dynamic_inference, graph);
        add_manifest_changes(manifest_changes, graph);

        RootScopeEffects {
            repo_root: &self.repo_root,
            repo_files,
            groups: &groups,
            tests: &tests,
            changes: link_changes,
            mocked_modules: &mocked_modules,
        }
        .add_to(graph)
    }

    fn add_linked_file(
        &self,
        linked_file: &LinkedFile,
        manifest_changes: &ManifestChanges,
        graph: &mut GraphBuilder,
    ) {
        let file = if linked_file.is_test {
            let test = graph.add_test_file(linked_file.path.clone(), LANGUAGE);

            if self.presets.is_asset_blind(&linked_file.path) {
                graph.mark_asset_blind(test);
            }

            test
        } else {
            graph.add_file(linked_file.path.clone())
        };

        for dependency in &linked_file.dependencies {
            let dependency_file = graph.add_file(dependency.path.clone());

            graph.add_edge(Edge {
                dependent: file,
                dependency: dependency_file,
                kind: dependency.kind,
            });
        }

        if let (Some(specifier), Some(resolution_change)) = (
            &linked_file.resolution_change,
            manifest_changes.resolution_change(),
        ) {
            let description = format!("{} resolves {specifier:?} differently", linked_file.path);

            graph.add_changed_part(
                resolution_change.clone(),
                linked_file.path.clone(),
                description,
            );
        }
    }

    fn runner_groups(
        &self,
        repo_files: &[RepoPath],
        linked_files: &[LinkedFile],
        dynamic_inference: &DynamicInference,
    ) -> Vec<RunnerGroup> {
        if self.presets.runner_configs().is_empty() && self.presets.jest_manifests().is_empty() {
            return Vec::new();
        }

        let repo_file_set: HashSet<&RepoPath> = repo_files.iter().collect();
        let is_linked = |path: &RepoPath| {
            repo_file_set.contains(path) || path.as_str().starts_with(PACKAGE_NODE_PREFIX)
        };
        let mut dependencies: HashMap<&RepoPath, Vec<&RepoPath>> = HashMap::new();

        for linked_file in linked_files {
            dependencies.entry(&linked_file.path).or_default().extend(
                linked_file
                    .dependencies
                    .iter()
                    .filter(|dependency| {
                        dependency.kind.selects_tests() && is_linked(&dependency.path)
                    })
                    .map(|dependency| &dependency.path),
            );
        }

        for dynamic_link in dynamic_inference
            .links
            .iter()
            .filter(|dynamic_link| is_linked(&dynamic_link.dependency))
        {
            dependencies
                .entry(&dynamic_link.dependent)
                .or_default()
                .push(&dynamic_link.dependency);
        }

        let tests: HashSet<&RepoPath> = linked_files
            .iter()
            .filter(|linked_file| linked_file.is_test)
            .map(|linked_file| &linked_file.path)
            .collect();
        let package_directories: BTreeSet<String> = repo_files
            .iter()
            .filter(|path| path.as_str().rsplit('/').next() == Some(PACKAGE_MANIFEST))
            .map(|path| parent_directory(path).to_owned())
            .collect();
        let literal_references = |file: &RepoPath| self.literal_references(file, &repo_file_set);
        let manifest_setups: Vec<(RepoPath, Vec<RepoPath>)> = self
            .presets
            .jest_manifests()
            .iter()
            .map(|manifest| {
                let references = self.jest_field_references(manifest, &repo_file_set);

                (manifest.clone(), references)
            })
            .collect();

        runner_groups(&ScopeInputs {
            runner_configs: self.presets.runner_configs(),
            manifest_setups: &manifest_setups,
            package_directories: &package_directories,
            tests: &tests,
            dependencies: &dependencies,
            literal_references: &literal_references,
        })
    }

    fn literal_references(
        &self,
        file: &RepoPath,
        repo_file_set: &HashSet<&RepoPath>,
    ) -> Vec<RepoPath> {
        let Ok(text) = fs::read_to_string(self.repo_root.join(file.as_str())) else {
            return Vec::new();
        };
        let literals = if is_source_file(file) {
            string_literals(file, &text)
        } else {
            parse_jsonc(&text)
                .or_else(|_| parse_yaml(&text))
                .map(|document| document_strings(&document))
                .unwrap_or_default()
        };

        self.referenced_files(file, &literals, repo_file_set)
    }

    fn jest_field_references(
        &self,
        manifest: &RepoPath,
        repo_file_set: &HashSet<&RepoPath>,
    ) -> Vec<RepoPath> {
        let literals = fs::read_to_string(self.repo_root.join(manifest.as_str()))
            .ok()
            .and_then(|text| parse_jsonc(&text).ok())
            .and_then(|document| Some(document_strings(document.get("jest")?)))
            .unwrap_or_default();

        self.referenced_files(manifest, &literals, repo_file_set)
    }

    fn referenced_files(
        &self,
        file: &RepoPath,
        literals: &[String],
        repo_file_set: &HashSet<&RepoPath>,
    ) -> Vec<RepoPath> {
        let file_directory = parent_directory(file);
        let mut referenced = BTreeSet::new();

        for literal in literals {
            let literal = literal.strip_prefix("<rootDir>/").unwrap_or(literal);

            if literal.is_empty() || literal.len() > 255 {
                continue;
            }

            for directory in [file_directory, ""] {
                let named_file = join_directory(directory, literal.trim_start_matches('/'))
                    .and_then(|joined| RepoPath::new(joined).ok())
                    .filter(|named_file| repo_file_set.contains(named_file));

                referenced.extend(named_file);
            }
        }

        referenced.into_iter().collect()
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
        let imports_node_test = parsed_imports
            .import_requests
            .iter()
            .any(|import_request| imports_node_test(&import_request.specifier));
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

            let is_raw_import = import_request.specifier.contains(RAW_IMPORT_QUERY);

            dependencies.extend(resolved_paths.into_iter().map(|dependency_path| {
                let kind = if import_request.kind == EdgeKind::Import
                    && is_asset_file(&dependency_path)
                    && !is_raw_import
                {
                    EdgeKind::Asset
                } else {
                    import_request.kind
                };

                Dependency {
                    path: dependency_path,
                    kind,
                }
            }));
        }

        Ok(LinkedFile {
            path: path.clone(),
            is_test: self.presets.is_test(path, imports_node_test),
            dependencies,
            has_dynamic_sites: parsed_imports.has_dynamic_sites,
            resolution_change,
            mocks: parsed_imports.mocks,
            glob_imports: parsed_imports.glob_imports,
        })
    }
}

fn document_strings(document: &serde_json::Value) -> Vec<String> {
    match document {
        serde_json::Value::String(text) => vec![text.clone()],
        serde_json::Value::Array(items) => items.iter().flat_map(document_strings).collect(),
        serde_json::Value::Object(properties) => {
            properties.values().flat_map(document_strings).collect()
        }
        _ => Vec::new(),
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
