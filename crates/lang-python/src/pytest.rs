//! The pytest preset: configurations and the tests they discover, plugins, and fixtures followed by name.

pub(crate) mod configs;
pub(crate) mod fixtures;
pub(crate) mod items;

use std::collections::BTreeSet;

use anyhow::Result;
use prober_core::changes::{BaseRevision, ChangeKind, FileChange};
use prober_core::files::RepoPath;
use prober_core::graph::GraphBuilder;
use prober_core::report::ModeReason;
use toml::Value;

use crate::conventions::{parent_directory, split_path};
use crate::modules::ModuleIndex;
use crate::pytest::configs::{
    Discovery, Options, PytestConfigs, config_options, is_config_file, is_within, option_changes,
};

const ENTRY_POINT_GROUP: &str = "pytest11";

pub(crate) fn global_plugins(
    configs: &PytestConfigs,
    index: &ModuleIndex,
    repo_files: &[RepoPath],
    read: &dyn Fn(&RepoPath) -> Option<String>,
) -> Vec<RepoPath> {
    let mut plugins = Vec::new();

    for config in configs.configs() {
        for plugin in configs.plugins(config) {
            plugins.extend(
                index
                    .resolve_module(&config.path, &plugin)
                    .into_iter()
                    .map(|resolved| resolved.module),
            );
        }
    }

    for manifest in repo_files
        .iter()
        .filter(|path| split_path(path.as_str()).1 == "pyproject.toml")
    {
        let Some(document) =
            read(manifest).and_then(|text| toml::from_str::<toml::Table>(&text).ok())
        else {
            continue;
        };
        let entry_points = document
            .get("project")
            .and_then(|project| project.get("entry-points"))
            .and_then(|groups| groups.get(ENTRY_POINT_GROUP))
            .and_then(Value::as_table);

        for target in entry_points.into_iter().flat_map(|table| table.values()) {
            let Some(module) = target
                .as_str()
                .map(|target| target.split(':').next().unwrap_or(target))
            else {
                continue;
            };

            plugins.extend(
                index
                    .resolve_module(manifest, module.trim())
                    .into_iter()
                    .map(|resolved| resolved.module),
            );
        }
    }

    plugins.sort();
    plugins.dedup();

    plugins
}

pub(crate) fn scoped_requests(configs: &PytestConfigs) -> Vec<(String, Vec<String>)> {
    configs
        .configs()
        .iter()
        .map(|config| {
            (
                config.directory.clone(),
                PytestConfigs::used_fixtures(config),
            )
        })
        .filter(|(_, names)| !names.is_empty())
        .collect()
}

pub(crate) struct ConfigChanges<'c> {
    pub configs: &'c PytestConfigs,
    pub tests: &'c BTreeSet<RepoPath>,
    pub changes: &'c [FileChange],
    pub base: &'c dyn BaseRevision,
    pub read_head: &'c dyn Fn(&RepoPath) -> Option<String>,
}

impl ConfigChanges<'_> {
    pub fn add_to(&self, graph: &mut GraphBuilder) -> Result<Vec<ModeReason>> {
        let mut reasons = Vec::new();

        for change in self.changes {
            if is_config_file(&change.path) {
                reasons.extend(self.effect(change, graph)?);
            }
        }

        Ok(reasons)
    }

    fn effect(&self, change: &FileChange, graph: &mut GraphBuilder) -> Result<Option<ModeReason>> {
        let base_path = match &change.kind {
            ChangeKind::Renamed { from } => from,
            _ => &change.path,
        };
        let head_options = match change.kind {
            ChangeKind::Deleted => None,
            _ => {
                (self.read_head)(&change.path).and_then(|text| config_options(&change.path, &text))
            }
        };
        let base_options = match change.kind {
            ChangeKind::Added => None,
            _ => self
                .base
                .read_file(base_path)?
                .and_then(|bytes| config_options(base_path, &String::from_utf8_lossy(&bytes))),
        };

        if head_options.is_none() && base_options.is_none() {
            return Ok(None);
        }

        let directory = parent_directory(change.path.as_str());
        let scope: Vec<&RepoPath> = self
            .tests
            .iter()
            .filter(|test| is_within(test.as_str(), directory))
            .filter(|test| {
                self.configs
                    .config_for(test.as_str())
                    .is_none_or(|config| config.directory.len() <= directory.len())
            })
            .collect();
        let both_present = head_options.is_some() && base_options.is_some();
        let base_options = base_options.unwrap_or_default();
        let head_options = head_options.unwrap_or_default();
        let option_changes = option_changes(&base_options, &head_options);
        let configures = !both_present || !option_changes.effects.is_empty();

        if configures {
            if scope.len() == self.tests.len() {
                return Ok(Some(ModeReason::RootScope {
                    change: change.clone(),
                    configures: true,
                }));
            }

            for test in scope {
                let description = format!("{test} runs with this config");

                graph.add_changed_part(change.path.clone(), test.clone(), description);
            }

            return Ok(None);
        }

        if option_changes.discovery {
            self.add_discovered(change, directory, &base_options, &scope, graph);
        }

        Ok(None)
    }

    fn add_discovered(
        &self,
        change: &FileChange,
        directory: &str,
        base_options: &Options,
        scope: &[&RepoPath],
        graph: &mut GraphBuilder,
    ) {
        let base_discovery = Discovery::new(directory, base_options);

        for test in scope
            .iter()
            .filter(|test| !base_discovery.discovers(test.as_str()))
        {
            let description = format!("{test} is discovered by this config");

            graph.add_changed_part(change.path.clone(), (*test).clone(), description);
        }
    }
}
