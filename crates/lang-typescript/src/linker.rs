use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use prober_core::config::InferenceSettings;
use prober_core::files::RepoPath;
use prober_core::graph::{Edge, EdgeKind, GraphBuilder, UnresolvedSite};
use rayon::prelude::*;

use crate::conventions::{LANGUAGE, is_module_file, is_source_file, is_test_file};
use crate::dynamic::{DynamicInference, DynamicLoading};
use crate::imports::parse_imports;
use crate::resolver::ModuleResolver;

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
}

impl TypeScriptLinker {
    pub fn new(repo_root: &Path) -> Result<Self> {
        let repo_root = repo_root
            .canonicalize()
            .with_context(|| format!("cannot open repository root {}", repo_root.display()))?;
        let resolver = ModuleResolver::new(&repo_root);

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
        let linked_files = repo_files
            .par_iter()
            .filter(|path| is_source_file(path))
            .map(|path| self.link_file(path))
            .collect::<Result<Vec<LinkedFile>>>()?;
        let dynamic_inference = self.infer_dynamic_links(repo_files, &linked_files)?;

        graph.add_language(LANGUAGE, |path| is_module_file(path) && !is_test_file(path));

        for linked_file in linked_files {
            let file = if is_test_file(&linked_file.path) {
                graph.add_test_file(linked_file.path, LANGUAGE)
            } else {
                graph.add_file(linked_file.path)
            };

            for dependency in linked_file.dependencies {
                let dependency_file = graph.add_file(dependency.path);

                graph.add_edge(Edge {
                    dependent: file,
                    dependency: dependency_file,
                    kind: dependency.kind,
                });
            }
        }

        add_dynamic_inference(dynamic_inference, graph);

        Ok(())
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

    pub fn missing_dependencies_warning(&self) -> Option<String> {
        let has_package_manifest = self.repo_root.join("package.json").is_file();
        let has_installed_dependencies = self.repo_root.join("node_modules").is_dir();

        (has_package_manifest && !has_installed_dependencies).then(|| {
            "node_modules not found: imports between workspace packages cannot be resolved, run prober after installing dependencies"
                .to_owned()
        })
    }

    fn link_file(&self, path: &RepoPath) -> Result<LinkedFile> {
        let source_bytes = fs::read(self.repo_root.join(path.as_str()))
            .with_context(|| format!("cannot read {path}"))?;
        let source_text = String::from_utf8_lossy(&source_bytes);
        let parsed_imports = parse_imports(path, &source_text);
        let dependencies = parsed_imports
            .import_requests
            .into_iter()
            .flat_map(|import_request| {
                self.resolver
                    .resolve(path, &import_request.specifier)
                    .into_iter()
                    .map(move |dependency_path| Dependency {
                        path: dependency_path,
                        kind: import_request.kind,
                    })
            })
            .collect();

        Ok(LinkedFile {
            path: path.clone(),
            dependencies,
            has_dynamic_sites: parsed_imports.has_dynamic_sites,
        })
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
