//! What changes to the root scope of runner groups select: every test, the tests of a group, or the tests a changed config literal names; and which mocked modules changed their exports.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use prober_core::changes::{ChangeKind, FileChange};
use prober_core::files::RepoPath;
use prober_core::graph::GraphBuilder;
use prober_core::report::ModeReason;

use crate::conventions::{is_module_file, is_test_runner_package};
use crate::linker::LinkChanges;
use crate::manifests::PackageChange;
use crate::mocks::export_signature;
use crate::presets::RunnerGroup;
use crate::presets::config_diff::{ConfigDiff, config_diff};
use crate::presets::patterns::{TestPattern, join_directory, relative_globs};
use crate::workspaces::parent_directory;

pub(crate) struct RootScopeEffects<'e> {
    pub repo_root: &'e Path,
    pub repo_files: &'e [RepoPath],
    pub groups: &'e [RunnerGroup],
    pub tests: &'e BTreeSet<RepoPath>,
    pub changes: &'e LinkChanges<'e>,
    pub mocked_modules: &'e HashSet<RepoPath>,
}

impl RootScopeEffects<'_> {
    pub fn add_to(&self, graph: &mut GraphBuilder) -> Result<Vec<ModeReason>> {
        let mut reasons = Vec::new();

        for change in self.changes.changes {
            reasons.extend(self.file_effect(change, graph)?);
        }

        reasons.extend(self.package_effects(graph));
        self.mark_changed_contracts(graph)?;

        Ok(reasons)
    }

    fn file_effect(
        &self,
        change: &FileChange,
        graph: &mut GraphBuilder,
    ) -> Result<Option<ModeReason>> {
        let touches = |scope: fn(&RunnerGroup) -> &BTreeSet<RepoPath>| -> Vec<&RunnerGroup> {
            self.groups
                .iter()
                .filter(|group| {
                    change
                        .touched_paths()
                        .any(|path| scope(group).contains(path))
                })
                .collect()
        };
        let config_groups = touches(|group| &group.config_scope);
        let setup_groups = touches(|group| &group.setup_scope);

        if config_groups.is_empty() && setup_groups.is_empty() {
            return Ok(None);
        }

        let named_tests = if config_groups.is_empty() {
            None
        } else {
            self.tests_named_by_config_change(change)?
        };
        let configures = !config_groups.is_empty() && named_tests.is_none();

        if let Some(named_tests) = named_tests {
            for (test, literal) in named_tests {
                let description = format!("{test} matches {literal:?}, which changed");

                graph.add_changed_part(change.path.clone(), test, description);
            }
        }

        let affected_groups: Vec<&RunnerGroup> = if configures {
            config_groups.into_iter().chain(setup_groups).collect()
        } else {
            setup_groups
        };

        if affected_groups
            .iter()
            .any(|group| self.covers_every_test(group))
        {
            return Ok(Some(ModeReason::RootScope {
                change: change.clone(),
                configures,
            }));
        }

        for group in affected_groups {
            for test in self.tests.iter().filter(|test| group.covers(test)) {
                let description = if configures {
                    format!("{test} runs with this config")
                } else {
                    format!("loaded before {test}")
                };

                graph.add_changed_part(change.path.clone(), test.clone(), description);
            }
        }

        Ok(None)
    }

    fn package_effects(&self, graph: &mut GraphBuilder) -> Vec<ModeReason> {
        let mut changes_everywhere: BTreeMap<&RepoPath, Vec<&PackageChange>> = BTreeMap::new();

        for package_change in self.changes.manifests.packages() {
            let groups_loading_it: Vec<&RunnerGroup> = self
                .groups
                .iter()
                .filter(|group| group.packages.contains(&package_change.name))
                .collect();
            let is_loaded_everywhere = is_test_runner_package(&package_change.name)
                || groups_loading_it
                    .iter()
                    .any(|group| self.covers_every_test(group));

            if is_loaded_everywhere {
                changes_everywhere
                    .entry(&package_change.change.path)
                    .or_default()
                    .push(package_change);

                continue;
            }

            for group in groups_loading_it {
                for test in self.tests.iter().filter(|test| group.covers(test)) {
                    let description = format!(
                        "{test} loads package {} before it runs",
                        package_change.name
                    );

                    graph.add_changed_part(
                        package_change.change.path.clone(),
                        test.clone(),
                        description,
                    );
                }
            }
        }

        changes_everywhere
            .into_values()
            .filter_map(|package_changes| {
                let first_change = package_changes.iter().min_by_key(|package_change| {
                    (package_change.through.is_some(), &package_change.name)
                })?;
                let detail = match &first_change.through {
                    None => first_change.detail.clone(),
                    Some(dependency) => format!("through {dependency} {}", first_change.detail),
                };

                Some(ModeReason::RootScopePackage {
                    change: first_change.change.clone(),
                    package: first_change.name.clone(),
                    detail,
                    other_packages: package_changes.len() - 1,
                })
            })
            .collect()
    }

    fn covers_every_test(&self, group: &RunnerGroup) -> bool {
        self.tests.iter().all(|test| group.covers(test))
    }

    fn tests_named_by_config_change(
        &self,
        change: &FileChange,
    ) -> Result<Option<Vec<(RepoPath, String)>>> {
        if change.kind != ChangeKind::Modified {
            return Ok(None);
        }

        let (Some(base_text), Some(head_text)) =
            (self.base_text(&change.path)?, self.head_text(&change.path)?)
        else {
            return Ok(None);
        };
        let ConfigDiff::Literals(changed_literals) =
            config_diff(&change.path, &base_text, &head_text)
        else {
            return Ok(None);
        };
        let config_directory = parent_directory(&change.path);
        let repo_index = RepoIndex::new(self.repo_files);
        let mut named_tests = Vec::new();

        for literal in changed_literals {
            let Some(tests) = self.tests_named_by(&repo_index, config_directory, &literal) else {
                return Ok(None);
            };

            named_tests.extend(tests.into_iter().map(|test| (test, literal.clone())));
        }

        Ok(Some(named_tests))
    }

    fn tests_named_by(
        &self,
        repo_index: &RepoIndex,
        config_directory: &str,
        literal: &str,
    ) -> Option<BTreeSet<RepoPath>> {
        let pattern = literal.trim_start_matches('!');
        let path = pattern.strip_prefix("<rootDir>/").unwrap_or(pattern);
        let is_glob = pattern.contains(['*', '?', '{', '[']);
        let mut named = BTreeSet::new();
        let mut is_path_like = is_glob || path.contains('/') || has_module_extension(path);

        for directory in [config_directory, ""] {
            let Some(joined) = join_directory(directory, path) else {
                continue;
            };

            if let Ok(named_path) = RepoPath::new(&joined)
                && repo_index.files.contains(&named_path)
            {
                if !self.tests.contains(&named_path) {
                    return None;
                }

                named.insert(named_path);
                is_path_like = true;
            }

            if repo_index.directories.contains(joined.as_str()) {
                let directory_prefix = format!("{joined}/");

                named.extend(
                    self.tests
                        .iter()
                        .skip_while(|test| test.as_str() < directory_prefix.as_str())
                        .take_while(|test| test.as_str().starts_with(&directory_prefix))
                        .cloned(),
                );
                is_path_like = true;
            }

            if is_glob
                && let Some(matcher) = relative_globs(directory, pattern)
                    .and_then(|globs| TestPattern::builder().include(globs).build())
            {
                named.extend(
                    self.tests
                        .iter()
                        .filter(|test| matcher.matches(test))
                        .cloned(),
                );
            }
        }

        is_path_like.then_some(named)
    }

    fn mark_changed_contracts(&self, graph: &mut GraphBuilder) -> Result<()> {
        for change in self.changes.changes {
            let base_path = match &change.kind {
                ChangeKind::Added => continue,
                ChangeKind::Renamed { from } => from,
                ChangeKind::Modified | ChangeKind::Deleted => &change.path,
            };

            if !self.mocked_modules.contains(&change.path)
                && !self.mocked_modules.contains(base_path)
            {
                continue;
            }

            let base_signature = self
                .base_text(base_path)?
                .and_then(|text| export_signature(base_path, &text));
            let head_signature = match change.kind {
                ChangeKind::Deleted => None,
                _ => self
                    .head_text(&change.path)?
                    .and_then(|text| export_signature(&change.path, &text)),
            };

            if base_signature.is_none() || base_signature != head_signature {
                let module = graph.add_file(change.path.clone());

                graph.mark_changed_contract(module);
            }
        }

        Ok(())
    }

    fn base_text(&self, path: &RepoPath) -> Result<Option<String>> {
        self.changes
            .base
            .read_file(path)
            .with_context(|| format!("cannot read {path} at the base revision"))?
            .map(|bytes| String::from_utf8(bytes).map_err(|_| anyhow!("{path} is not valid UTF-8")))
            .transpose()
    }

    fn head_text(&self, path: &RepoPath) -> Result<Option<String>> {
        match fs::read_to_string(self.repo_root.join(path.as_str())) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => Ok(None),
            Err(error) => Err(error).with_context(|| format!("cannot read {path}")),
        }
    }
}

struct RepoIndex<'r> {
    files: HashSet<&'r RepoPath>,
    directories: HashSet<&'r str>,
}

impl<'r> RepoIndex<'r> {
    fn new(repo_files: &'r [RepoPath]) -> Self {
        let directories = repo_files
            .iter()
            .flat_map(|path| {
                path.as_str()
                    .match_indices('/')
                    .map(|(offset, _)| &path.as_str()[..offset])
            })
            .collect();

        Self {
            files: repo_files.iter().collect(),
            directories,
        }
    }
}

fn has_module_extension(path: &str) -> bool {
    RepoPath::new(path).is_ok_and(|path| is_module_file(&path))
}
