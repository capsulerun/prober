//! A TypeScript project written to a temporary directory, and the graph Prober links from it.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::Result;
use prober_core::changes::{BaseRevision, ChangeKind, FileChange};
use prober_core::config::InferenceSettings;
use prober_core::files::RepoPath;
use prober_core::graph::{EdgeKind, Graph, GraphBuilder};
use prober_core::report::{ModeReason, Report};
use prober_core::select::{Widening, select_tests};
use prober_lang_typescript::{LinkChanges, ManifestChanges, TypeScriptLinker};
use tempfile::TempDir;

struct BaseFiles(HashMap<RepoPath, Vec<u8>>);

impl BaseRevision for BaseFiles {
    fn read_file(&self, path: &RepoPath) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get(path).cloned())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    All(Vec<String>),
    Selected(Vec<(String, Vec<String>)>),
}

impl Outcome {
    pub fn selected_paths(&self) -> Vec<&str> {
        match self {
            Outcome::All(_) => panic!("expected a selection, got {self:?}"),
            Outcome::Selected(selected_tests) => selected_tests
                .iter()
                .map(|(test, _)| test.as_str())
                .collect(),
        }
    }
}

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

    pub fn remove(&self, path: &str) {
        fs::remove_file(self.root().join(path)).unwrap();
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

    pub fn link_symbols(&self) -> Graph {
        let repo_files = self.repo_files();
        let mut builder = GraphBuilder::default();

        TypeScriptLinker::new(self.root(), &repo_files)
            .unwrap()
            .with_symbol_analysis()
            .link(&repo_files, &mut builder)
            .unwrap();

        builder.build()
    }

    pub fn link_with(&self, inference: InferenceSettings) -> Graph {
        let repo_files = self.repo_files();
        let mut builder = GraphBuilder::default();

        TypeScriptLinker::new(self.root(), &repo_files)
            .unwrap()
            .with_inference(inference)
            .link(&repo_files, &mut builder)
            .unwrap();

        builder.build()
    }
}

impl TestProject {
    pub fn outcome_of(&self, base_files: &[(&str, &str)], changes: &[FileChange]) -> Outcome {
        let repo_files = self.repo_files();
        let base_files = BaseFiles(
            base_files
                .iter()
                .map(|&(path, content)| (repo_path(path), content.as_bytes().to_vec()))
                .collect(),
        );
        let manifest_changes =
            ManifestChanges::between(self.root(), &repo_files, changes, &base_files).unwrap();
        let global_reasons = manifest_changes.global_reasons();

        if !global_reasons.is_empty() {
            return Outcome::All(global_reasons.iter().map(ModeReason::message).collect());
        }

        let linker = TypeScriptLinker::new(self.root(), &repo_files).unwrap();
        let link_changes = LinkChanges {
            changes,
            manifests: &manifest_changes,
            base: &base_files,
        };
        let mut builder = GraphBuilder::default();
        let root_scope_reasons = linker
            .link_changes(&repo_files, &link_changes, &mut builder)
            .unwrap();

        if !root_scope_reasons.is_empty() {
            return Outcome::All(root_scope_reasons.iter().map(ModeReason::message).collect());
        }

        let graph = builder.build();
        let selected_tests = select_tests(&graph, changes, Widening::Scoped);
        let report = Report::from_selection(&graph, changes, &selected_tests, String::new());

        Outcome::Selected(
            report
                .targets
                .into_iter()
                .map(|target| (target.path.to_string(), target.impact.unwrap().reason))
                .collect(),
        )
    }
}

pub fn modified(path: &str) -> FileChange {
    change(path, ChangeKind::Modified)
}

pub fn selected(tests: &[(&str, &[&str])]) -> Outcome {
    Outcome::Selected(
        tests
            .iter()
            .map(|(test, reason)| {
                let reason = reason.iter().map(|&hop| hop.to_owned()).collect();

                ((*test).to_owned(), reason)
            })
            .collect(),
    )
}

pub fn all(messages: &[&str]) -> Outcome {
    Outcome::All(messages.iter().map(|&message| message.to_owned()).collect())
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
