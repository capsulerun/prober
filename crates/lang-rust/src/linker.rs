use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use prober_core::changes::FileChange;
use prober_core::config::{InferenceSettings, Presets};
use prober_core::files::RepoPath;
use prober_core::frontend::{ChangeSet, Frontend};
use prober_core::graph::{Edge, EdgeKind, GraphBuilder};
use prober_core::report::ModeReason;
use rayon::prelude::*;

use crate::conventions::{
    ALWAYS_ALL_DEFAULTS, LANGUAGE, MANIFEST_DEFAULTS, TARGET_UNIT, is_manifest, is_source_file,
    registry_node,
};
use crate::manifests::CargoChanges;
use crate::modules::{Dependency, FILE_GLOB, ModuleGraph};
use crate::source::{SourceFacts, source_facts};
use crate::workspace::Workspace;

const CARGO_PRESET: &str = "cargo";

pub struct RustLinker {
    repo_root: PathBuf,
    workspace: Workspace,
    inference: InferenceSettings,
    cargo: bool,
    cargo_changes: CargoChanges,
}

impl RustLinker {
    pub fn new(repo_root: &Path, repo_files: &[RepoPath]) -> Result<Self> {
        let repo_root = repo_root
            .canonicalize()
            .with_context(|| format!("cannot open repository root {}", repo_root.display()))?;
        let read_head = |path: &RepoPath| fs::read_to_string(repo_root.join(path.as_str())).ok();
        let workspace = Workspace::read(repo_files, &read_head);

        Ok(Self {
            repo_root,
            workspace,
            inference: InferenceSettings::default(),
            cargo: true,
            cargo_changes: CargoChanges::default(),
        })
    }

    pub fn with_inference(self, inference: InferenceSettings) -> Self {
        Self { inference, ..self }
    }

    pub fn with_presets(self, presets: &Presets) -> Self {
        let cargo = match presets {
            Presets::Auto => true,
            Presets::Only(names) => names.iter().any(|name| name == CARGO_PRESET),
        };

        Self { cargo, ..self }
    }

    fn read_head(&self, path: &RepoPath) -> Option<String> {
        fs::read_to_string(self.repo_root.join(path.as_str())).ok()
    }

    fn diffs_manifests(&self) -> bool {
        self.cargo && self.inference.manifests
    }

    fn link(&self, change_set: &ChangeSet, graph: &mut GraphBuilder) -> Result<()> {
        let repo_files = change_set.repo_files;
        let facts: HashMap<RepoPath, SourceFacts> = repo_files
            .par_iter()
            .filter(|path| is_source_file(path))
            .filter_map(|path| Some((path.clone(), source_facts(&self.read_head(path)?))))
            .collect();
        let modules = ModuleGraph::new(&self.workspace, &facts, repo_files);
        let mut sorted_files: Vec<&RepoPath> = repo_files.iter().collect();

        sorted_files.sort();
        graph.add_language(LANGUAGE, is_source_file);
        self.add_removed_crates(repo_files, change_set.changes, graph);

        for (krate, crate_info) in self.workspace.crates.iter().enumerate() {
            let manifest = graph.add_test_file(crate_info.manifest.clone(), LANGUAGE);

            graph.name_target(manifest, TARGET_UNIT, crate_info.name.clone());

            for file in modules.crate_files(krate) {
                let file = graph.add_file(file.clone());

                graph.add_edge(Edge {
                    dependent: manifest,
                    dependency: file,
                    kind: EdgeKind::Build,
                });
            }

            let Some(build_script) = crate_info.build_script() else {
                continue;
            };
            let build_script = graph.add_file(build_script);

            for file in modules.lib_files(krate) {
                let file = graph.add_file(file.clone());

                if file != build_script {
                    graph.add_edge(Edge {
                        dependent: file,
                        dependency: build_script,
                        kind: EdgeKind::Build,
                    });
                }
            }
        }

        let file_set: HashSet<&str> = repo_files.iter().map(RepoPath::as_str).collect();

        for link in modules.links() {
            if !self.inference.files && matches!(link.dependency, Dependency::Path(_)) {
                continue;
            }

            let dependencies = match &link.dependency {
                Dependency::File(file) => vec![file.clone()],
                Dependency::Crate(krate) => {
                    let lib_files: Vec<RepoPath> =
                        modules.lib_files(*krate).iter().cloned().collect();

                    if lib_files.is_empty() {
                        vec![self.workspace.crates[*krate].manifest.clone()]
                    } else {
                        lib_files
                    }
                }
                Dependency::External(package) => vec![registry_node(package)],
                Dependency::Path(path) if file_set.contains(path.as_str()) => {
                    RepoPath::new(path.clone()).into_iter().collect()
                }
                Dependency::Path(path) => files_under(&sorted_files, path)
                    .into_iter()
                    .filter(|file| !is_source_file(file))
                    .collect(),
            };
            let dependent = graph.add_file(link.dependent.clone());
            let reads_cases = link
                .inference
                .as_ref()
                .is_some_and(|inference| inference.kind == FILE_GLOB);

            for dependency in dependencies {
                let dependency = graph.add_file(dependency);

                if reads_cases {
                    graph.mark_case_file(dependency);
                }

                let edge = Edge {
                    dependent,
                    dependency,
                    kind: link.kind,
                };

                match &link.inference {
                    Some(inference) => graph.add_inferred_edge(edge, inference.clone()),
                    None => graph.add_edge(edge),
                }
            }
        }

        for package in self.cargo_changes.packages() {
            graph.add_changed_part(
                package.change_path.clone(),
                registry_node(&package.package),
                package.description.clone(),
            );
        }

        for crate_change in self.cargo_changes.crates() {
            for file in modules.lib_files(crate_change.krate) {
                graph.add_changed_part(
                    crate_change.change_path.clone(),
                    file.clone(),
                    crate_change.description.clone(),
                );
            }
        }

        Ok(())
    }

    fn add_removed_crates(
        &self,
        repo_files: &[RepoPath],
        changes: &[FileChange],
        graph: &mut GraphBuilder,
    ) {
        let present: HashSet<&RepoPath> = repo_files.iter().collect();

        for removed in changes
            .iter()
            .flat_map(FileChange::touched_paths)
            .filter(|path| !present.contains(path) && is_manifest(path))
        {
            graph.add_removed_test(removed.clone());
        }
    }
}

fn files_under(sorted_files: &[&RepoPath], directory: &str) -> Vec<RepoPath> {
    let prefix = format!("{directory}/");
    let start = sorted_files.partition_point(|path| path.as_str() < prefix.as_str());

    sorted_files[start..]
        .iter()
        .take_while(|path| path.as_str().starts_with(&prefix))
        .map(|path| (*path).clone())
        .collect()
}

impl Frontend for RustLinker {
    fn language(&self) -> &'static str {
        LANGUAGE
    }

    fn always_all_patterns(&self) -> Vec<&'static str> {
        let manifest_patterns = if self.diffs_manifests() {
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

    fn test_files(&self, _: &[RepoPath]) -> Vec<RepoPath> {
        self.workspace
            .crates
            .iter()
            .map(|krate| krate.manifest.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn target_name(&self, test: &RepoPath) -> Option<(&'static str, String)> {
        self.workspace
            .crates
            .iter()
            .find(|krate| krate.manifest == *test)
            .map(|krate| (TARGET_UNIT, krate.name.clone()))
    }

    fn diff_manifests(&mut self, change_set: &ChangeSet) -> Result<Vec<ModeReason>> {
        if !self.diffs_manifests() {
            return Ok(Vec::new());
        }

        let repo_root = &self.repo_root;
        let read_head = |path: &RepoPath| fs::read_to_string(repo_root.join(path.as_str())).ok();

        self.cargo_changes = CargoChanges::between(
            &self.workspace,
            &read_head,
            change_set.changes,
            change_set.base,
        )?;

        Ok(self.cargo_changes.global_reasons().to_vec())
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
