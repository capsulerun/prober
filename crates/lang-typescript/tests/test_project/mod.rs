//! A TypeScript project written to a temporary directory, and the graph Prober links from it.

#![allow(dead_code)]

use std::fs;
use std::path::Path;

use prober_core::changes::{ChangeKind, FileChange};
use prober_core::config::InferenceSettings;
use prober_core::files::RepoPath;
use prober_core::graph::{EdgeKind, Graph, GraphBuilder};
use prober_core::select::{Widening, select_tests};
use prober_lang_typescript::TypeScriptLinker;
use tempfile::TempDir;

pub struct TestProject {
    directory: TempDir,
}

impl TestProject {
    pub fn new(files: &[(&str, &str)]) -> Self {
        let test_project = Self {
            directory: TempDir::new().unwrap(),
        };

        for &(path, content) in files {
            test_project.write(path, content);
        }

        test_project
    }

    pub fn root(&self) -> &Path {
        self.directory.path()
    }

    pub fn write(&self, path: &str, content: &str) {
        let file_path = self.root().join(path);

        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(file_path, content).unwrap();
    }

    #[cfg(unix)]
    pub fn symlink(&self, link: &str, target: &str) {
        let link_path = self.root().join(link);

        fs::create_dir_all(link_path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(target, link_path).unwrap();
    }

    pub fn repo_files(&self) -> Vec<RepoPath> {
        let mut repo_files = Vec::new();

        collect_repo_files(self.root(), "", &mut repo_files);
        repo_files.sort();

        repo_files
    }

    pub fn link(&self) -> Graph {
        self.link_with(InferenceSettings::default())
    }

    pub fn link_with(&self, inference: InferenceSettings) -> Graph {
        let mut builder = GraphBuilder::default();

        TypeScriptLinker::new(self.root())
            .unwrap()
            .with_inference(inference)
            .link(&self.repo_files(), &mut builder)
            .unwrap();

        builder.build()
    }
}

fn collect_repo_files(directory: &Path, prefix: &str, repo_files: &mut Vec<RepoPath>) {
    for entry in fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        let repo_relative_path = format!("{prefix}{name}");

        if name == "node_modules" {
            continue;
        }

        if entry.file_type().unwrap().is_dir() {
            collect_repo_files(&entry.path(), &format!("{repo_relative_path}/"), repo_files);
        } else {
            repo_files.push(repo_path(&repo_relative_path));
        }
    }
}

pub fn repo_path(path: &str) -> RepoPath {
    RepoPath::new(path).unwrap()
}

pub fn dependents<'g>(graph: &'g Graph, dependency: &str) -> Vec<(&'g str, EdgeKind)> {
    let Some(dependency_file) = graph.file_id(&repo_path(dependency)) else {
        return Vec::new();
    };

    graph
        .dependents(dependency_file)
        .iter()
        .map(|edge| (graph.path(edge.dependent).as_str(), edge.kind))
        .collect()
}

pub fn selected_tests(graph: &Graph, changes: &[FileChange]) -> Vec<String> {
    select_tests(graph, changes, Widening::Scoped)
        .iter()
        .map(|selected_test| graph.path(selected_test.file).as_str().to_owned())
        .collect()
}

pub fn change(path: &str, kind: ChangeKind) -> FileChange {
    FileChange {
        path: repo_path(path),
        kind,
    }
}
