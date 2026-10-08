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

pub type ModuleFilter = fn(&RepoPath) -> bool;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Inference {
    pub line: u32,
    pub kind: &'static str,
    pub pattern: String,
    pub detail: Option<String>,
}

impl Inference {
    pub fn description(&self) -> String {
        let Inference {
            kind,
            pattern,
            detail,
            ..
        } = self;

        match detail {
            Some(detail) => format!("inferred {kind} {pattern} ({detail})"),
            None => format!("inferred {kind} {pattern}"),
        }
    }
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
    pub fn widens(self) -> bool {
        self != UnresolvedCause::NoMatchingFile
    }

    pub fn key(self) -> &'static str {
        match self {
            UnresolvedCause::InferenceDisabled => "inference_disabled",
            UnresolvedCause::Parameter => "parameter",
            UnresolvedCause::RuntimeValue => "runtime_value",
            UnresolvedCause::TooWide => "too_wide",
            UnresolvedCause::NoMatchingFile => "no_matching_file",
        }
    }

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
    pub language: &'static str,
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
    module_filters: HashMap<&'static str, ModuleFilter>,
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

    pub fn add_language(&mut self, language: &'static str, loads: ModuleFilter) {
        self.module_filters.insert(language, loads);
    }

    pub fn build(mut self) -> Graph {
        let mut dependents_by_file = vec![Vec::new(); self.files.len()];
        let mut dependencies_by_file = vec![Vec::new(); self.files.len()];

        for edge in self.edges {
            dependents_by_file[edge.dependency.index()].push(edge);
            dependencies_by_file[edge.dependent.index()].push(edge);
        }

        for dependents in &mut dependents_by_file {
            dependents.sort_by(|left, right| {
                let left_path = self.files.path(left.dependent);
                let right_path = self.files.path(right.dependent);

                left_path.cmp(right_path).then(left.kind.cmp(&right.kind))
            });
            dependents.dedup();
        }

        for dependencies in &mut dependencies_by_file {
            dependencies.sort_by(|left, right| {
                let left_path = self.files.path(left.dependency);
                let right_path = self.files.path(right.dependency);

                left_path.cmp(right_path).then(left.kind.cmp(&right.kind))
            });
            dependencies.dedup();
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
            dependencies_by_file,
            inferences: self.inferences,
            unresolved_sites: self.unresolved_sites,
            module_filters: self.module_filters,
        }
    }
}

#[derive(Debug)]
pub struct Graph {
    files: FileTable,
    test_languages: HashMap<FileId, &'static str>,
    dependents_by_file: Vec<Vec<Edge>>,
    dependencies_by_file: Vec<Vec<Edge>>,
    inferences: HashMap<(FileId, FileId), Inference>,
    unresolved_sites: Vec<UnresolvedSite>,
    module_filters: HashMap<&'static str, ModuleFilter>,
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

    pub fn dependencies(&self, file: FileId) -> &[Edge] {
        &self.dependencies_by_file[file.index()]
    }

    pub fn inference(&self, dependent: FileId, dependency: FileId) -> Option<&Inference> {
        self.inferences.get(&(dependent, dependency))
    }

    pub fn inferred_edges(&self) -> impl Iterator<Item = (Edge, &Inference)> {
        self.inferences
            .iter()
            .map(|(&(dependent, dependency), inference)| {
                let inferred_edge = Edge {
                    dependent,
                    dependency,
                    kind: EdgeKind::Dynamic,
                };

                (inferred_edge, inference)
            })
    }

    pub fn can_load(&self, language: &str, path: &RepoPath) -> bool {
        self.module_filters
            .get(language)
            .is_none_or(|loads| loads(path))
    }

    pub fn is_module(&self, path: &RepoPath) -> bool {
        self.module_filters.values().any(|loads| loads(path))
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
            kind: "glob",
            pattern: "src/plugins/*.ts".to_owned(),
            detail: None,
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
        assert_eq!(
            graph.dependencies(loader),
            [Edge {
                dependent: loader,
                dependency: plugin,
                kind: EdgeKind::Dynamic,
            }]
        );
        assert_eq!(
            graph.inferred_edges().collect::<Vec<_>>(),
            [(graph.dependencies(loader)[0], &inference_at(4))]
        );
    }

    #[test]
    fn inference_is_described_by_its_kind_pattern_and_detail() {
        let plugin_folder = Inference {
            line: 3,
            kind: "plugin folder",
            pattern: "extensions/{id}/".to_owned(),
            detail: Some("id=telegram".to_owned()),
        };

        assert_eq!(
            plugin_folder.description(),
            "inferred plugin folder extensions/{id}/ (id=telegram)"
        );
    }

    #[test]
    fn each_language_says_which_files_its_unresolved_sites_can_load() {
        let mut builder = GraphBuilder::default();

        builder.add_language("typescript", |path| path.as_str().ends_with(".ts"));

        let graph = builder.build();

        assert!(graph.can_load("typescript", &repo_path("src/a.ts")));
        assert!(!graph.can_load("typescript", &repo_path("docs/a.md")));
        assert!(graph.can_load("python", &repo_path("docs/a.md")));
        assert!(graph.is_module(&repo_path("src/a.ts")));
        assert!(!graph.is_module(&repo_path("docs/a.md")));
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
            language: "typescript",
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
