use std::collections::HashMap;
use std::collections::hash_map::Entry;

use crate::files::{FileId, FileTable, RepoPath};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EdgeKind {
    Import,
    TypeOnly,
    Dynamic,
    Contains,
}

impl EdgeKind {
    pub fn selects_tests(self) -> bool {
        match self {
            EdgeKind::Import | EdgeKind::Dynamic | EdgeKind::Contains => true,
            EdgeKind::TypeOnly => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Inference {
    pub line: u32,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UnresolvedCause {
    InferenceDisabled,
    Parameter,
    RuntimeValue,
    TooWide,
    NoMatchingFile,
}

impl UnresolvedCause {
    pub fn description(self) -> &'static str {
        match self {
            UnresolvedCause::InferenceDisabled => "inference is disabled",
            UnresolvedCause::Parameter => "comes from a function parameter",
            UnresolvedCause::RuntimeValue => "is computed at runtime",
            UnresolvedCause::TooWide => "has no literal directory or file name",
            UnresolvedCause::NoMatchingFile => "matches no file of the repository",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedSite {
    pub file: FileId,
    pub line: u32,
    pub expression: String,
    pub cause: UnresolvedCause,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    pub dependent: FileId,
    pub dependency: FileId,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestFile {
    pub file: FileId,
    pub language: &'static str,
}

#[derive(Debug, Default)]
pub struct GraphBuilder {
    files: FileTable,
    test_languages: HashMap<FileId, &'static str>,
    edges: Vec<Edge>,
    inferences: HashMap<(FileId, FileId), Inference>,
    unresolved_sites: Vec<UnresolvedSite>,
}

impl GraphBuilder {
    pub fn add_file(&mut self, path: RepoPath) -> FileId {
        self.files.intern(path)
    }

    pub fn add_test_file(&mut self, path: RepoPath, language: &'static str) -> FileId {
        let test_file = self.files.intern(path);

        self.test_languages.insert(test_file, language);

        test_file
    }

    pub fn add_edge(&mut self, edge: Edge) {
        self.edges.push(edge);
    }

    pub fn add_inferred_edge(&mut self, edge: Edge, inference: Inference) {
        match self.inferences.entry((edge.dependent, edge.dependency)) {
            Entry::Occupied(mut entry) if inference < *entry.get() => {
                entry.insert(inference);
            }
            Entry::Occupied(_) => {}
            Entry::Vacant(entry) => {
                entry.insert(inference);
            }
        }

        self.edges.push(edge);
    }

    pub fn add_unresolved_site(&mut self, unresolved_site: UnresolvedSite) {
        self.unresolved_sites.push(unresolved_site);
    }

    pub fn build(mut self) -> Graph {
        let mut dependents_by_file = vec![Vec::new(); self.files.len()];

        for edge in self.edges {
            dependents_by_file[edge.dependency.index()].push(edge);
        }

        for dependents in &mut dependents_by_file {
            dependents.sort_by(|left, right| {
                let left_path = self.files.path(left.dependent);
                let right_path = self.files.path(right.dependent);

                left_path.cmp(right_path).then(left.kind.cmp(&right.kind))
            });
            dependents.dedup();
        }

        self.unresolved_sites.sort_by(|left, right| {
            let left_path = self.files.path(left.file);
            let right_path = self.files.path(right.file);

            left_path
                .cmp(right_path)
                .then(left.line.cmp(&right.line))
                .then(left.expression.cmp(&right.expression))
        });
        self.unresolved_sites.dedup();

        Graph {
            files: self.files,
            test_languages: self.test_languages,
            dependents_by_file,
            inferences: self.inferences,
            unresolved_sites: self.unresolved_sites,
        }
    }
}

#[derive(Debug)]
pub struct Graph {
    files: FileTable,
    test_languages: HashMap<FileId, &'static str>,
    dependents_by_file: Vec<Vec<Edge>>,
    inferences: HashMap<(FileId, FileId), Inference>,
    unresolved_sites: Vec<UnresolvedSite>,
}

impl Graph {
    pub fn file_id(&self, path: &RepoPath) -> Option<FileId> {
        self.files.id(path)
    }

    pub fn path(&self, file: FileId) -> &RepoPath {
        self.files.path(file)
    }

    pub fn is_test(&self, file: FileId) -> bool {
        self.test_languages.contains_key(&file)
    }

    pub fn test_language(&self, file: FileId) -> Option<&'static str> {
        self.test_languages.get(&file).copied()
    }

    pub fn test_files_by_path(&self) -> Vec<TestFile> {
        let mut test_files: Vec<TestFile> = self
            .test_languages
            .iter()
            .map(|(&file, &language)| TestFile { file, language })
            .collect();

        test_files.sort_by(|left, right| self.path(left.file).cmp(self.path(right.file)));

        test_files
    }

    pub fn dependents(&self, file: FileId) -> &[Edge] {
        &self.dependents_by_file[file.index()]
    }

    pub fn inference(&self, dependent: FileId, dependency: FileId) -> Option<&Inference> {
        self.inferences.get(&(dependent, dependency))
    }

    pub fn unresolved_sites(&self) -> &[UnresolvedSite] {
        &self.unresolved_sites
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn dependent_paths(graph: &Graph, dependency: &str) -> Vec<(String, EdgeKind)> {
        let dependency_file = graph.file_id(&repo_path(dependency)).unwrap();

        graph
            .dependents(dependency_file)
            .iter()
            .map(|edge| (graph.path(edge.dependent).as_str().to_owned(), edge.kind))
            .collect()
    }

    #[test]
    fn dependents_are_sorted_by_path_and_deduplicated() {
        let mut builder = GraphBuilder::default();
        let shared = builder.add_file(repo_path("src/shared.ts"));
        let zeta = builder.add_file(repo_path("src/zeta.ts"));
        let alpha = builder.add_file(repo_path("src/alpha.ts"));
        let import_edges = [
            Edge {
                dependent: zeta,
                dependency: shared,
                kind: EdgeKind::Import,
            },
            Edge {
                dependent: alpha,
                dependency: shared,
                kind: EdgeKind::TypeOnly,
            },
            Edge {
                dependent: alpha,
                dependency: shared,
                kind: EdgeKind::Import,
            },
            Edge {
                dependent: zeta,
                dependency: shared,
                kind: EdgeKind::Import,
            },
        ];

        for edge in import_edges {
            builder.add_edge(edge);
        }

        let graph = builder.build();

        assert_eq!(
            dependent_paths(&graph, "src/shared.ts"),
            [
                ("src/alpha.ts".to_owned(), EdgeKind::Import),
                ("src/alpha.ts".to_owned(), EdgeKind::TypeOnly),
                ("src/zeta.ts".to_owned(), EdgeKind::Import),
            ]
        );
    }

    #[test]
    fn file_added_before_being_marked_as_test_keeps_one_id() {
        let mut builder = GraphBuilder::default();
        let imported_first = builder.add_file(repo_path("src/a.test.ts"));
        let marked_as_test = builder.add_test_file(repo_path("src/a.test.ts"), "typescript");
        let graph = builder.build();

        assert_eq!(imported_first, marked_as_test);
        assert!(graph.is_test(imported_first));
    }

    #[test]
    fn test_files_are_listed_by_path_with_their_language() {
        let mut builder = GraphBuilder::default();

        builder.add_test_file(repo_path("tests/test_api.py"), "python");
        builder.add_file(repo_path("src/a.ts"));
        builder.add_test_file(repo_path("src/a.test.ts"), "typescript");

        let graph = builder.build();
        let test_files: Vec<(&str, &str)> = graph
            .test_files_by_path()
            .iter()
            .map(|test_file| (graph.path(test_file.file).as_str(), test_file.language))
            .collect();

        assert_eq!(
            test_files,
            [
                ("src/a.test.ts", "typescript"),
                ("tests/test_api.py", "python")
            ]
        );
    }

    #[test]
    fn type_only_edges_never_select_tests() {
        assert!(EdgeKind::Import.selects_tests());
        assert!(EdgeKind::Dynamic.selects_tests());
        assert!(EdgeKind::Contains.selects_tests());
        assert!(!EdgeKind::TypeOnly.selects_tests());
    }

    #[test]
    fn inferred_edge_keeps_the_inference_of_its_first_site() {
        let mut builder = GraphBuilder::default();
        let loader = builder.add_file(repo_path("src/loader.ts"));
        let plugin = builder.add_file(repo_path("src/plugins/a.ts"));
        let dynamic_edge = Edge {
            dependent: loader,
            dependency: plugin,
            kind: EdgeKind::Dynamic,
        };
        let inference_at = |line: u32| Inference {
            line,
            description: "inferred glob src/plugins/*.ts".to_owned(),
        };

        builder.add_inferred_edge(dynamic_edge, inference_at(12));
        builder.add_inferred_edge(dynamic_edge, inference_at(4));
        builder.add_inferred_edge(dynamic_edge, inference_at(30));

        let graph = builder.build();

        assert_eq!(
            dependent_paths(&graph, "src/plugins/a.ts"),
            [("src/loader.ts".to_owned(), EdgeKind::Dynamic)]
        );
        assert_eq!(graph.inference(loader, plugin), Some(&inference_at(4)));
        assert_eq!(graph.inference(plugin, loader), None);
    }

    #[test]
    fn unresolved_sites_are_listed_by_path_and_line_without_duplicates() {
        let mut builder = GraphBuilder::default();
        let worker = builder.add_file(repo_path("src/worker.ts"));
        let loader = builder.add_file(repo_path("src/loader.ts"));
        let unresolved_site = |file: FileId, line: u32| UnresolvedSite {
            file,
            line,
            expression: "request.moduleUrl".to_owned(),
            cause: UnresolvedCause::RuntimeValue,
        };

        builder.add_unresolved_site(unresolved_site(worker, 3));
        builder.add_unresolved_site(unresolved_site(loader, 20));
        builder.add_unresolved_site(unresolved_site(loader, 8));
        builder.add_unresolved_site(unresolved_site(loader, 20));

        let graph = builder.build();
        let listed_sites: Vec<(&str, u32)> = graph
            .unresolved_sites()
            .iter()
            .map(|site| (graph.path(site.file).as_str(), site.line))
            .collect();

        assert_eq!(
            listed_sites,
            [
                ("src/loader.ts", 8),
                ("src/loader.ts", 20),
                ("src/worker.ts", 3)
            ]
        );
    }
}
