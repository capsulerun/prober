//! Links runners make outside imports: modules a test mocks, manual mocks in `__mocks__`, snapshot files, and `import.meta.glob` patterns.

use std::collections::{BTreeSet, HashMap, HashSet};

use prober_core::files::RepoPath;
use prober_core::graph::{Edge, EdgeKind, GraphBuilder, Inference};

use crate::conventions::source_stem;
use crate::linker::LinkedFile;
use crate::presets::patterns::{TestPattern, relative_globs};
use crate::resolver::{ModuleResolver, package_node};
use crate::workspaces::parent_directory;

const MANUAL_MOCKS_DIRECTORY: &str = "__mocks__";

const SNAPSHOTS_DIRECTORY: &str = "__snapshots__";

const SNAPSHOT_EXTENSION: &str = ".snap";

const GLOB_IMPORT: &str = "glob import";

pub(crate) struct TestLinks<'l> {
    pub repo_files: &'l [RepoPath],
    pub linked_files: &'l [LinkedFile],
    pub resolver: &'l ModuleResolver,
    pub follows_glob_imports: bool,
}

impl TestLinks<'_> {
    pub fn add_to(&self, graph: &mut GraphBuilder) -> HashSet<RepoPath> {
        let repo_file_set: HashSet<&RepoPath> = self.repo_files.iter().collect();
        let manual_mocks: HashMap<&str, &RepoPath> = self
            .repo_files
            .iter()
            .filter_map(|path| {
                let stem = source_stem(path)?;

                stem.contains(&format!("{MANUAL_MOCKS_DIRECTORY}/"))
                    .then_some((stem, path))
            })
            .collect();
        let mut mocked_modules = HashSet::new();

        for test in self
            .linked_files
            .iter()
            .filter(|linked_file| linked_file.is_test)
        {
            for mock in &test.mocks {
                let mocked_files: Vec<RepoPath> = self
                    .resolver
                    .resolve(&test.path, &mock.specifier)
                    .into_iter()
                    .filter(|resolved| repo_file_set.contains(resolved))
                    .collect();

                for mocked_file in mocked_files {
                    if mock.replaces_module {
                        let test_file = graph.add_file(test.path.clone());
                        let mocked_module = graph.add_file(mocked_file.clone());

                        graph.add_mock(test_file, mocked_module);
                        mocked_modules.insert(mocked_file.clone());
                    }

                    if !mock.has_factory
                        && let Some(manual_mock) = manual_mock_of(&mocked_file, &manual_mocks)
                    {
                        add_edge(graph, &test.path, manual_mock, EdgeKind::Import);
                    }
                }
            }
        }

        self.add_package_manual_mocks(graph);
        self.add_snapshots(graph);

        if self.follows_glob_imports {
            self.add_glob_imports(graph);
        }

        mocked_modules
    }

    fn add_package_manual_mocks(&self, graph: &mut GraphBuilder) {
        let module_stems: HashSet<&str> = self.repo_files.iter().filter_map(source_stem).collect();
        let mocks_directory = format!("{MANUAL_MOCKS_DIRECTORY}/");
        let package_mocks: Vec<(&RepoPath, RepoPath)> = self
            .repo_files
            .iter()
            .filter_map(|path| {
                let stem = source_stem(path)?;
                let (directory_prefix, mocked_stem) = stem.split_once(&mocks_directory)?;
                let is_module_mock =
                    module_stems.contains(format!("{directory_prefix}{mocked_stem}").as_str());

                (!is_module_mock).then(|| (path, package_node(mocked_stem)))
            })
            .collect();

        if package_mocks.is_empty() {
            return;
        }

        let mocked_packages: HashSet<&RepoPath> =
            package_mocks.iter().map(|(_, package)| package).collect();
        let mut importers: HashMap<&RepoPath, BTreeSet<&RepoPath>> = HashMap::new();

        for linked_file in self.linked_files {
            for dependency in &linked_file.dependencies {
                if mocked_packages.contains(&dependency.path) {
                    importers
                        .entry(&dependency.path)
                        .or_default()
                        .insert(&linked_file.path);
                }
            }
        }

        for (manual_mock, package) in package_mocks {
            for importer in importers.get(&package).into_iter().flatten() {
                add_edge(graph, importer, manual_mock.clone(), EdgeKind::Import);
            }
        }
    }

    fn add_snapshots(&self, graph: &mut GraphBuilder) {
        let tests: HashSet<&RepoPath> = self
            .linked_files
            .iter()
            .filter(|linked_file| linked_file.is_test)
            .map(|linked_file| &linked_file.path)
            .collect();

        for snapshot in self.repo_files {
            let Some(snapshotted_test) = snapshotted_test(snapshot) else {
                continue;
            };

            if tests.contains(&snapshotted_test) {
                add_edge(graph, &snapshotted_test, snapshot.clone(), EdgeKind::Reads);
            }
        }
    }

    fn add_glob_imports(&self, graph: &mut GraphBuilder) {
        for linked_file in self.linked_files {
            for glob_import in &linked_file.glob_imports {
                let Some(matcher) = glob_matcher(&linked_file.path, &glob_import.patterns) else {
                    continue;
                };
                let pattern = glob_import.patterns.join(", ");

                for matched in self
                    .repo_files
                    .iter()
                    .filter(|path| **path != linked_file.path && matcher.matches(path))
                {
                    let dependent = graph.add_file(linked_file.path.clone());
                    let dependency = graph.add_file(matched.clone());

                    graph.add_inferred_edge(
                        Edge {
                            dependent,
                            dependency,
                            kind: EdgeKind::Dynamic,
                        },
                        Inference {
                            line: glob_import.line,
                            kind: GLOB_IMPORT,
                            pattern: pattern.clone(),
                            detail: None,
                        },
                    );
                }
            }
        }
    }
}

fn add_edge(graph: &mut GraphBuilder, dependent: &RepoPath, dependency: RepoPath, kind: EdgeKind) {
    let dependent = graph.add_file(dependent.clone());
    let dependency = graph.add_file(dependency);

    graph.add_edge(Edge {
        dependent,
        dependency,
        kind,
    });
}

fn manual_mock_of(module: &RepoPath, manual_mocks: &HashMap<&str, &RepoPath>) -> Option<RepoPath> {
    let stem = source_stem(module)?;
    let manual_mock_stem = match stem.rsplit_once('/') {
        Some((directory, name)) => format!("{directory}/{MANUAL_MOCKS_DIRECTORY}/{name}"),
        None => format!("{MANUAL_MOCKS_DIRECTORY}/{stem}"),
    };

    manual_mocks
        .get(manual_mock_stem.as_str())
        .map(|manual_mock| (*manual_mock).clone())
}

fn snapshotted_test(snapshot: &RepoPath) -> Option<RepoPath> {
    let snapshot_name = snapshot.as_str().strip_suffix(SNAPSHOT_EXTENSION)?;
    let (snapshots_directory, test_name) = snapshot_name.rsplit_once('/')?;
    let test_directory = match snapshots_directory.strip_suffix(SNAPSHOTS_DIRECTORY)? {
        "" => String::new(),
        directory => directory.to_owned(),
    };

    RepoPath::new(format!("{test_directory}{test_name}")).ok()
}

fn glob_matcher(importer: &RepoPath, patterns: &[String]) -> Option<TestPattern> {
    let importer_directory = parent_directory(importer);
    let mut include = Vec::new();
    let mut exclude = Vec::new();

    for pattern in patterns {
        let (globs, pattern) = match pattern.strip_prefix('!') {
            Some(excluded) => (&mut exclude, excluded),
            None => (&mut include, pattern.as_str()),
        };
        let directory = if pattern.starts_with('/') {
            ""
        } else if pattern.starts_with("./") || pattern.starts_with("../") {
            importer_directory
        } else {
            return None;
        };

        globs.extend(relative_globs(directory, pattern.trim_start_matches('/'))?);
    }

    TestPattern::builder()
        .include(include)
        .exclude(exclude)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    #[test]
    fn snapshot_files_name_their_test() {
        assert_eq!(
            snapshotted_test(&repo_path("src/__snapshots__/render.test.ts.snap")),
            Some(repo_path("src/render.test.ts"))
        );
        assert_eq!(
            snapshotted_test(&repo_path("__snapshots__/a.spec.js.snap")),
            Some(repo_path("a.spec.js"))
        );
        assert_eq!(
            snapshotted_test(&repo_path("src/render.test.ts.snap")),
            None
        );
    }

    #[test]
    fn manual_mocks_sit_beside_the_module_they_replace() {
        let repo_files = [
            repo_path("src/db.ts"),
            repo_path("src/__mocks__/db.ts"),
            repo_path("src/clock.ts"),
        ];
        let manual_mocks: HashMap<&str, &RepoPath> = repo_files
            .iter()
            .filter_map(|path| Some((source_stem(path)?, path)))
            .collect();

        assert_eq!(
            manual_mock_of(&repo_path("src/db.ts"), &manual_mocks),
            Some(repo_path("src/__mocks__/db.ts"))
        );
        assert_eq!(
            manual_mock_of(&repo_path("src/clock.ts"), &manual_mocks),
            None
        );
    }

    #[test]
    fn glob_import_patterns_are_relative_to_the_importer_or_the_root() {
        let matcher = glob_matcher(
            &repo_path("src/i18n/index.ts"),
            &[
                "./locales/*.json".to_owned(),
                "!./locales/draft.json".to_owned(),
                "/plugins/*/index.ts".to_owned(),
            ],
        )
        .unwrap();

        assert!(matcher.matches(&repo_path("src/i18n/locales/fr.json")));
        assert!(!matcher.matches(&repo_path("src/i18n/locales/draft.json")));
        assert!(matcher.matches(&repo_path("plugins/a/index.ts")));
        assert!(glob_matcher(&repo_path("src/a.ts"), &["@/pages/*.ts".to_owned()]).is_none());
    }
}
