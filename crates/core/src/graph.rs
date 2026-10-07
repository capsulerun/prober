//! File-level dependency graph, stored as a reverse index from each file to the files that depend on it.

use std::collections::HashSet;

use crate::files::{FileId, FileTable, RepoPath};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EdgeKind {
    Import,
    TypeOnly,
}

impl EdgeKind {
    pub fn selects_tests(self) -> bool {
        match self {
            EdgeKind::Import => true,
            EdgeKind::TypeOnly => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    pub dependent: FileId,
    pub dependency: FileId,
    pub kind: EdgeKind,
}

#[derive(Debug, Default)]
pub struct GraphBuilder {
    files: FileTable,
    test_files: HashSet<FileId>,
    edges: Vec<Edge>,
}

impl GraphBuilder {
    pub fn add_file(&mut self, path: RepoPath) -> FileId {
        self.files.intern(path)
    }

    pub fn add_test_file(&mut self, path: RepoPath) -> FileId {
        let test_file = self.files.intern(path);

        self.test_files.insert(test_file);

        test_file
    }

    pub fn add_edge(&mut self, edge: Edge) {
        self.edges.push(edge);
    }

    pub fn build(self) -> Graph {
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

        Graph {
            files: self.files,
            test_files: self.test_files,
            dependents_by_file,
        }
    }
}

#[derive(Debug)]
pub struct Graph {
    files: FileTable,
    test_files: HashSet<FileId>,
    dependents_by_file: Vec<Vec<Edge>>,
}

impl Graph {
    pub fn file_id(&self, path: &RepoPath) -> Option<FileId> {
        self.files.id(path)
    }

    pub fn path(&self, file: FileId) -> &RepoPath {
        self.files.path(file)
    }

    pub fn is_test(&self, file: FileId) -> bool {
        self.test_files.contains(&file)
    }

    pub fn dependents(&self, file: FileId) -> &[Edge] {
        &self.dependents_by_file[file.index()]
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
        let marked_as_test = builder.add_test_file(repo_path("src/a.test.ts"));
        let graph = builder.build();

        assert_eq!(imported_first, marked_as_test);
        assert!(graph.is_test(imported_first));
    }

    #[test]
    fn type_only_edges_never_select_tests() {
        assert!(EdgeKind::Import.selects_tests());
        assert!(!EdgeKind::TypeOnly.selects_tests());
    }
}
