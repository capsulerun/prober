//! Adds TypeScript and JavaScript files to the graph: test files, and an edge for every import that resolves.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use prober_core::files::RepoPath;
use prober_core::graph::{Edge, EdgeKind, GraphBuilder};
use rayon::prelude::*;

use crate::conventions::{LANGUAGE, is_source_file, is_test_file};
use crate::imports::parse_imports;
use crate::resolver::ModuleResolver;

pub struct TypeScriptLinker {
    repo_root: PathBuf,
    resolver: ModuleResolver,
}

struct Dependency {
    path: RepoPath,
    kind: EdgeKind,
}

struct LinkedFile {
    path: RepoPath,
    dependencies: Vec<Dependency>,
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
        })
    }

    pub fn link(&self, repo_files: &[RepoPath], graph: &mut GraphBuilder) -> Result<()> {
        let linked_files = repo_files
            .par_iter()
            .filter(|path| is_source_file(path))
            .map(|path| self.link_file(path))
            .collect::<Result<Vec<LinkedFile>>>()?;

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

        Ok(())
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
        let dependencies = parse_imports(path, &source_text)
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
        })
    }
}
