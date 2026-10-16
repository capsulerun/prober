//! Root scope of each runner group: its configs and the files only they reach, and the setup files they name with the files only those reach.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use prober_core::files::RepoPath;

use crate::conventions::{is_runner_config, is_source_file, is_test_support_file};
use crate::workspaces::parent_directory;

const PACKAGE_NODE_PREFIX: &str = "node_modules/";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RunnerGroup {
    pub directory: String,
    pub configs: BTreeSet<RepoPath>,
    pub config_scope: BTreeSet<RepoPath>,
    pub setup_scope: BTreeSet<RepoPath>,
    pub packages: BTreeSet<String>,
}

impl RunnerGroup {
    pub fn covers(&self, path: &RepoPath) -> bool {
        self.directory.is_empty() || path.as_str().starts_with(&format!("{}/", self.directory))
    }
}

pub(crate) struct ScopeInputs<'i> {
    pub runner_configs: &'i [RepoPath],
    pub manifest_setups: &'i [(RepoPath, Vec<RepoPath>)],
    pub package_directories: &'i BTreeSet<String>,
    pub tests: &'i HashSet<&'i RepoPath>,
    pub dependencies: &'i HashMap<&'i RepoPath, Vec<&'i RepoPath>>,
    pub literal_references: &'i dyn Fn(&RepoPath) -> Vec<RepoPath>,
}

impl<'i> ScopeInputs<'i> {
    fn dependencies_of(&self, file: &RepoPath) -> impl Iterator<Item = &'i RepoPath> + use<'i> {
        self.dependencies.get(file).into_iter().flatten().copied()
    }

    fn repo_dependencies(&self, file: &RepoPath) -> impl Iterator<Item = &'i RepoPath> + use<'i> {
        self.dependencies_of(file)
            .filter(|dependency| !dependency.as_str().starts_with(PACKAGE_NODE_PREFIX))
    }
}

pub(crate) fn runner_groups(inputs: &ScopeInputs) -> Vec<RunnerGroup> {
    let reached_by_tests = reached_by_tests(inputs);
    let mut references: HashMap<RepoPath, Vec<RepoPath>> = HashMap::new();
    let mut configs: Vec<&RepoPath> = inputs.runner_configs.iter().collect();

    configs.sort_by_key(|config| (config.as_str().matches('/').count(), *config));

    let mut grouped_configs: HashSet<RepoPath> = HashSet::new();
    let mut closures_by_directory: BTreeMap<String, BTreeSet<RepoPath>> = BTreeMap::new();

    for top_config in configs {
        if grouped_configs.contains(top_config) {
            continue;
        }

        let config_closure = config_closure(inputs, top_config, &reached_by_tests, &mut references);
        let config_directory = parent_directory(top_config);
        let group_directory = if inputs.package_directories.contains(config_directory) {
            config_directory.to_owned()
        } else {
            String::new()
        };

        grouped_configs.extend(
            config_closure
                .iter()
                .filter(|file| is_runner_config(file))
                .cloned(),
        );
        closures_by_directory
            .entry(group_directory)
            .or_default()
            .extend(config_closure);
    }

    let mut groups: Vec<RunnerGroup> = closures_by_directory
        .into_iter()
        .map(|(directory, config_closure)| {
            let configs: BTreeSet<RepoPath> = config_closure
                .iter()
                .filter(|file| is_runner_config(file))
                .cloned()
                .collect();
            let setup_seeds: BTreeSet<RepoPath> = config_closure
                .iter()
                .flat_map(|file| references.get(file).into_iter().flatten())
                .filter(|reference| {
                    is_setup_seed(reference, inputs, &reached_by_tests, &config_closure)
                })
                .cloned()
                .collect();

            group_with_setup(
                inputs,
                &reached_by_tests,
                directory,
                configs,
                config_closure,
                setup_seeds,
            )
        })
        .collect();

    for (manifest, references) in inputs.manifest_setups {
        let setup_seeds = references
            .iter()
            .filter(|reference| {
                is_setup_seed(reference, inputs, &reached_by_tests, &BTreeSet::new())
            })
            .cloned()
            .collect();

        groups.push(group_with_setup(
            inputs,
            &reached_by_tests,
            parent_directory(manifest).to_owned(),
            BTreeSet::new(),
            BTreeSet::new(),
            setup_seeds,
        ));
    }

    groups
}

fn is_setup_seed(
    reference: &RepoPath,
    inputs: &ScopeInputs,
    reached_by_tests: &HashSet<&RepoPath>,
    config_scope: &BTreeSet<RepoPath>,
) -> bool {
    is_source_file(reference)
        && !is_runner_config(reference)
        && !inputs.tests.contains(reference)
        && !reached_by_tests.contains(reference)
        && !config_scope.contains(reference)
}

fn group_with_setup(
    inputs: &ScopeInputs,
    reached_by_tests: &HashSet<&RepoPath>,
    directory: String,
    configs: BTreeSet<RepoPath>,
    config_scope: BTreeSet<RepoPath>,
    setup_seeds: BTreeSet<RepoPath>,
) -> RunnerGroup {
    let mut setup_scope = setup_seeds.clone();
    let mut files_to_visit: VecDeque<RepoPath> = setup_seeds.into_iter().collect();

    while let Some(file) = files_to_visit.pop_front() {
        for dependency in inputs.repo_dependencies(&file) {
            let runs_for_every_test =
                !reached_by_tests.contains(dependency) || is_test_support_file(dependency);

            if runs_for_every_test && setup_scope.insert(dependency.clone()) {
                files_to_visit.push_back(dependency.clone());
            }
        }
    }

    let packages = config_scope
        .iter()
        .chain(&setup_scope)
        .flat_map(|file| inputs.dependencies_of(file))
        .filter_map(|dependency| dependency.as_str().strip_prefix(PACKAGE_NODE_PREFIX))
        .map(str::to_owned)
        .collect();

    RunnerGroup {
        directory,
        configs,
        config_scope,
        setup_scope,
        packages,
    }
}

fn config_closure(
    inputs: &ScopeInputs,
    top_config: &RepoPath,
    reached_by_tests: &HashSet<&RepoPath>,
    references: &mut HashMap<RepoPath, Vec<RepoPath>>,
) -> BTreeSet<RepoPath> {
    let mut closure = BTreeSet::from([top_config.clone()]);
    let mut files_to_visit = VecDeque::from([top_config.clone()]);

    while let Some(file) = files_to_visit.pop_front() {
        let file_references = references
            .entry(file.clone())
            .or_insert_with(|| (inputs.literal_references)(&file));
        let next_files: Vec<RepoPath> = inputs
            .repo_dependencies(&file)
            .cloned()
            .chain(
                file_references
                    .iter()
                    .filter(|reference| is_runner_config(reference))
                    .cloned(),
            )
            .collect();

        for next_file in next_files {
            let is_runner_only =
                is_runner_config(&next_file) || !reached_by_tests.contains(&next_file);

            if is_runner_only && closure.insert(next_file.clone()) {
                files_to_visit.push_back(next_file);
            }
        }
    }

    closure
}

fn reached_by_tests<'i>(inputs: &ScopeInputs<'i>) -> HashSet<&'i RepoPath> {
    let mut reached: HashSet<&RepoPath> = inputs.tests.iter().copied().collect();
    let mut files_to_visit: VecDeque<&RepoPath> = reached.iter().copied().collect();

    while let Some(file) = files_to_visit.pop_front() {
        for dependency in inputs.repo_dependencies(file) {
            if reached.insert(dependency) {
                files_to_visit.push_back(dependency);
            }
        }
    }

    reached
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn paths(paths: &[&str]) -> BTreeSet<RepoPath> {
        paths.iter().map(|path| repo_path(path)).collect()
    }

    struct Project {
        runner_configs: Vec<RepoPath>,
        tests: HashSet<RepoPath>,
        dependencies: HashMap<RepoPath, Vec<RepoPath>>,
        references: HashMap<RepoPath, Vec<RepoPath>>,
        package_directories: BTreeSet<String>,
    }

    impl Project {
        fn new(
            runner_configs: &[&str],
            tests: &[&str],
            dependencies: &[(&str, &str)],
            references: &[(&str, &str)],
        ) -> Self {
            let mut dependency_map: HashMap<RepoPath, Vec<RepoPath>> = HashMap::new();
            let mut reference_map: HashMap<RepoPath, Vec<RepoPath>> = HashMap::new();

            for &(dependent, dependency) in dependencies {
                dependency_map
                    .entry(repo_path(dependent))
                    .or_default()
                    .push(repo_path(dependency));
            }

            for &(file, reference) in references {
                reference_map
                    .entry(repo_path(file))
                    .or_default()
                    .push(repo_path(reference));
            }

            Self {
                runner_configs: runner_configs.iter().map(|path| repo_path(path)).collect(),
                tests: tests.iter().map(|path| repo_path(path)).collect(),
                dependencies: dependency_map,
                references: reference_map,
                package_directories: BTreeSet::from([String::new(), "packages/ui".to_owned()]),
            }
        }

        fn groups(&self) -> Vec<RunnerGroup> {
            let literal_references =
                |file: &RepoPath| self.references.get(file).cloned().unwrap_or_default();
            let tests: HashSet<&RepoPath> = self.tests.iter().collect();
            let dependencies: HashMap<&RepoPath, Vec<&RepoPath>> = self
                .dependencies
                .iter()
                .map(|(dependent, dependencies)| (dependent, dependencies.iter().collect()))
                .collect();

            runner_groups(&ScopeInputs {
                runner_configs: &self.runner_configs,
                manifest_setups: &[],
                package_directories: &self.package_directories,
                tests: &tests,
                dependencies: &dependencies,
                literal_references: &literal_references,
            })
        }
    }

    #[test]
    fn root_scope_holds_configs_setup_files_and_what_only_they_reach() {
        let project = Project::new(
            &["vitest.config.ts", "test/vitest/vitest.unit.config.ts"],
            &["src/a.test.ts", "test/vitest/vitest.config.test.ts"],
            &[
                ("vitest.config.ts", "test/vitest/shared-config.ts"),
                ("vitest.config.ts", "node_modules/@vitejs/plugin-vue"),
                ("test/vitest/shared-config.ts", "src/constants.ts"),
                ("test/setup.ts", "test/setup.shared.ts"),
                ("test/setup.shared.ts", "src/infra/warnings.ts"),
                ("test/setup.shared.ts", "node_modules/fake-indexeddb"),
                ("src/a.test.ts", "src/a.ts"),
                ("src/a.ts", "src/infra/warnings.ts"),
                ("src/a.ts", "src/constants.ts"),
                (
                    "test/vitest/vitest.config.test.ts",
                    "test/vitest/vitest.unit.config.ts",
                ),
            ],
            &[
                ("vitest.config.ts", "test/vitest/vitest.unit.config.ts"),
                ("test/vitest/shared-config.ts", "test/setup.ts"),
                ("test/vitest/shared-config.ts", "src/a.test.ts"),
                ("test/vitest/shared-config.ts", "src/a.ts"),
            ],
        );

        assert_eq!(
            project.groups(),
            [RunnerGroup {
                directory: String::new(),
                configs: paths(&["test/vitest/vitest.unit.config.ts", "vitest.config.ts"]),
                config_scope: paths(&[
                    "test/vitest/shared-config.ts",
                    "test/vitest/vitest.unit.config.ts",
                    "vitest.config.ts",
                ]),
                setup_scope: paths(&["test/setup.shared.ts", "test/setup.ts"]),
                packages: BTreeSet::from([
                    "@vitejs/plugin-vue".to_owned(),
                    "fake-indexeddb".to_owned()
                ]),
            }]
        );
    }

    #[test]
    fn configs_no_other_config_reaches_start_their_own_group_in_their_directory() {
        let project = Project::new(
            &["vitest.config.ts", "packages/ui/vite.config.ts"],
            &["src/a.test.ts", "packages/ui/src/b.test.ts"],
            &[("packages/ui/vite.config.ts", "packages/ui/test/setup.ts")],
            &[],
        );
        let groups = project.groups();
        let directories: Vec<&str> = groups
            .iter()
            .map(|group| group.directory.as_str())
            .collect();

        assert_eq!(directories, ["", "packages/ui"]);
        assert!(groups[1].covers(&repo_path("packages/ui/src/b.test.ts")));
        assert!(!groups[1].covers(&repo_path("src/a.test.ts")));
        assert!(groups[0].covers(&repo_path("packages/ui/src/b.test.ts")));
        assert_eq!(
            groups[1].config_scope,
            paths(&["packages/ui/test/setup.ts", "packages/ui/vite.config.ts"])
        );
    }
}
