//! Test runner presets: the runners a repository uses, the tests they run, and which of those tests render assets.

pub(crate) mod config_diff;
mod configs;
pub(crate) mod patterns;
mod scope;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use prober_core::config::Presets;
use prober_core::files::RepoPath;
use serde_json::Value as JsonValue;

use crate::conventions::{is_runner_config, is_source_file, is_test_file};
use crate::dynamic::evaluator::Evaluator;
use crate::dynamic::value::Value;
use crate::manifests::documents::{parse_json, parse_jsonc, parse_yaml};
use crate::presets::configs::{
    ProjectReading, json_to_value, read_cypress, read_jest, read_mocha, read_playwright,
    read_rstest, read_vitest,
};
use crate::presets::patterns::{TestPattern, relative_globs};
use crate::workspaces::{PACKAGE_MANIFEST, parent_directory};

pub(crate) use scope::{RunnerGroup, ScopeInputs, runner_groups};

pub(crate) const VITEST_DEFAULT_INCLUDE: &[&str] = &["**/*.{test,spec}.?(c|m)[jt]s?(x)"];

pub(crate) const VITEST_DEFAULT_EXCLUDE: &[&str] = &["**/node_modules/**", "**/.git/**"];

pub(crate) const JEST_DEFAULT_TEST_MATCH: &[&str] = &[
    "**/__tests__/**/*.?([mc])[jt]s?(x)",
    "**/?(*.)+(spec|test).?([mc])[jt]s?(x)",
];

pub(crate) const RSTEST_DEFAULT_INCLUDE: &[&str] = &["**/*.{test,spec}.?(c|m)[jt]s?(x)"];

pub(crate) const RSTEST_DEFAULT_EXCLUDE: &[&str] = &[
    "**/node_modules/**",
    "**/dist/**",
    "**/.{idea,git,cache,output,temp}/**",
];

const CYPRESS_DEFAULT_SPEC: &str = "**/*.cy.{js,jsx,ts,tsx,mjs,cjs}";

const NODE_TEST_MODULE: &str = "node:test";

const RENDERING_TEST_MARKERS: [&str; 3] = [".e2e.", ".browser.", ".visual."];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Preset {
    Cypress,
    Jest,
    Mocha,
    NodeTest,
    Playwright,
    Rstest,
    Vite,
    Vitest,
}

impl Preset {
    const ALL: [Preset; 8] = [
        Preset::Cypress,
        Preset::Jest,
        Preset::Mocha,
        Preset::NodeTest,
        Preset::Playwright,
        Preset::Rstest,
        Preset::Vite,
        Preset::Vitest,
    ];

    fn name(self) -> &'static str {
        match self {
            Preset::Cypress => "cypress",
            Preset::Jest => "jest",
            Preset::Mocha => "mocha",
            Preset::NodeTest => "node-test",
            Preset::Playwright => "playwright",
            Preset::Rstest => "rstest",
            Preset::Vite => "vite",
            Preset::Vitest => "vitest",
        }
    }

    fn packages(self) -> &'static [&'static str] {
        match self {
            Preset::Cypress => &["cypress"],
            Preset::Jest => &["jest", "ts-jest", "babel-jest"],
            Preset::Mocha => &["mocha"],
            Preset::NodeTest => &[],
            Preset::Playwright => &["@playwright/test", "playwright"],
            Preset::Rstest => &["@rstest/core", "rstack"],
            Preset::Vite => &["vite"],
            Preset::Vitest => &["vitest"],
        }
    }

    fn config_prefixes(self) -> &'static [&'static str] {
        match self {
            Preset::Cypress => &["cypress.config."],
            Preset::Jest => &["jest.config."],
            Preset::Mocha => &[".mocharc."],
            Preset::NodeTest => &[],
            Preset::Playwright => &["playwright.config."],
            Preset::Rstest => &["rstest.config.", "rstack.config."],
            Preset::Vite => &["vite.config."],
            Preset::Vitest => &["vitest.config.", "vitest.workspace."],
        }
    }

    fn global_config_patterns(self) -> &'static [&'static str] {
        match self {
            Preset::Jest => &["**/jest.*"],
            Preset::Rstest => &["**/rstest.config.*"],
            Preset::Vite => &["**/vite.config.*"],
            Preset::Vitest => &["**/vitest.*"],
            Preset::Cypress | Preset::Mocha | Preset::NodeTest | Preset::Playwright => &[],
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct RunnerPresets {
    enabled: BTreeSet<Preset>,
    readings: Vec<ProjectReading>,
    runner_configs: Vec<RepoPath>,
    jest_manifests: Vec<RepoPath>,
}

impl RunnerPresets {
    pub fn read(
        setting: &Presets,
        repo_root: &Path,
        repo_files: &[RepoPath],
        evaluator: &Evaluator,
    ) -> Self {
        let manifests = package_manifests(repo_root, repo_files);
        let enabled = match setting {
            Presets::Auto => detected_presets(repo_files, &manifests),
            Presets::Only(names) => Preset::ALL
                .into_iter()
                .filter(|preset| names.iter().any(|name| name == preset.name()))
                .collect(),
        };
        let mut presets = Self {
            enabled,
            ..Self::default()
        };

        presets.runner_configs = repo_files
            .iter()
            .filter(|path| is_runner_config(path) && presets.reads_config(path, evaluator))
            .cloned()
            .collect();
        presets.read_configs(repo_root, repo_files, &manifests, evaluator);

        presets
    }

    pub fn uses(&self, preset: Preset) -> bool {
        self.enabled.contains(&preset)
    }

    pub fn follows_glob_imports(&self) -> bool {
        self.uses(Preset::Vite) || self.uses(Preset::Vitest)
    }

    pub fn detects_node_test(&self) -> bool {
        self.uses(Preset::NodeTest)
    }

    pub fn runner_configs(&self) -> &[RepoPath] {
        &self.runner_configs
    }

    pub fn jest_manifests(&self) -> &[RepoPath] {
        &self.jest_manifests
    }

    pub fn global_config_patterns(&self) -> Vec<&'static str> {
        Preset::ALL
            .into_iter()
            .filter(|preset| !self.uses(*preset))
            .flat_map(Preset::global_config_patterns)
            .copied()
            .collect()
    }

    pub fn is_test(&self, path: &RepoPath, imports_node_test: bool) -> bool {
        is_source_file(path)
            && (is_test_file(path)
                || (self.uses(Preset::NodeTest) && imports_node_test)
                || self
                    .readings
                    .iter()
                    .filter_map(|reading| reading.tests.as_ref())
                    .any(|tests| tests.matches(path)))
    }

    pub fn is_asset_blind(&self, test: &RepoPath) -> bool {
        let every_project_is_known = self
            .readings
            .iter()
            .all(|reading| reading.tests.is_some() && reading.renders_assets.is_some());
        let mut matching_readings = self
            .readings
            .iter()
            .filter(|reading| {
                reading
                    .tests
                    .as_ref()
                    .is_some_and(|tests| tests.matches(test))
            })
            .peekable();
        let is_named_as_rendering = RENDERING_TEST_MARKERS
            .iter()
            .any(|marker| test.as_str().contains(marker));

        every_project_is_known
            && !is_named_as_rendering
            && matching_readings.peek().is_some()
            && matching_readings.all(|reading| reading.renders_assets == Some(false))
    }

    fn reads_config(&self, path: &RepoPath, evaluator: &Evaluator) -> bool {
        let file_name = file_name(path);

        if file_name.starts_with("rstest.config.") {
            return self.uses(Preset::Rstest);
        }

        if file_name.starts_with("rstack.config.") {
            return self.uses(Preset::Rstest)
                && evaluator.evaluate_registered_config(path).is_some();
        }

        if file_name.starts_with("vitest.") {
            return self.uses(Preset::Vitest);
        }

        if file_name.starts_with("vite.config.") {
            return self.uses(Preset::Vitest) || self.uses(Preset::Vite);
        }

        self.uses(Preset::Jest)
    }

    fn read_configs(
        &mut self,
        repo_root: &Path,
        repo_files: &[RepoPath],
        manifests: &[(RepoPath, JsonValue)],
        evaluator: &Evaluator,
    ) {
        let jest_manifests: Vec<(RepoPath, Value)> = manifests
            .iter()
            .filter_map(|(path, manifest)| {
                Some((path.clone(), json_to_value(manifest.get("jest")?)))
            })
            .collect();

        if self.uses(Preset::Vitest) {
            for config in repo_files
                .iter()
                .filter(|path| is_vitest_entry(path, repo_files))
            {
                self.readings
                    .extend(read_vitest(evaluator, config, &self.runner_configs));
            }
        }

        if self.uses(Preset::Jest) {
            let jest_configs = repo_files.iter().filter(|path| is_jest_config(path));

            for config in jest_configs {
                self.readings.extend(read_jest(
                    evaluator,
                    config,
                    None,
                    &self.runner_configs,
                    &jest_manifests,
                ));
            }

            for (manifest, value) in &jest_manifests {
                self.readings.extend(read_jest(
                    evaluator,
                    manifest,
                    Some(value.clone()),
                    &self.runner_configs,
                    &jest_manifests,
                ));
            }

            self.jest_manifests = jest_manifests.into_iter().map(|(path, _)| path).collect();
        }

        if self.uses(Preset::Rstest) {
            for config in self
                .runner_configs
                .iter()
                .filter(|path| has_prefix(path, Preset::Rstest))
            {
                self.readings
                    .extend(read_rstest(evaluator, config, &self.runner_configs));
            }
        }

        if self.uses(Preset::Playwright) {
            for config in repo_files
                .iter()
                .filter(|path| has_prefix(path, Preset::Playwright))
            {
                self.readings.extend(read_playwright(evaluator, config));
            }
        }

        if self.uses(Preset::Cypress) {
            self.readings.push(ProjectReading {
                tests: relative_globs("", CYPRESS_DEFAULT_SPEC)
                    .and_then(|globs| TestPattern::builder().include(globs).build()),
                renders_assets: Some(true),
            });

            for config in repo_files
                .iter()
                .filter(|path| has_prefix(path, Preset::Cypress))
            {
                self.readings.extend(read_cypress(evaluator, config));
            }
        }

        if self.uses(Preset::Mocha) {
            self.read_mocha_configs(repo_root, repo_files, manifests, evaluator);
        }
    }

    fn read_mocha_configs(
        &mut self,
        repo_root: &Path,
        repo_files: &[RepoPath],
        manifests: &[(RepoPath, JsonValue)],
        evaluator: &Evaluator,
    ) {
        let mut mocha_configs: Vec<(String, Value)> = repo_files
            .iter()
            .filter(|path| has_prefix(path, Preset::Mocha))
            .filter_map(|path| {
                let value = if is_source_file(path) {
                    evaluator.evaluate_config(path)
                } else {
                    let text = fs::read_to_string(repo_root.join(path.as_str())).ok()?;
                    let document = if file_name(path).ends_with(".json")
                        || file_name(path).ends_with(".jsonc")
                    {
                        parse_jsonc(&text)
                    } else {
                        parse_yaml(&text)
                    };

                    json_to_value(&document.ok()?)
                };

                Some((parent_directory(path).to_owned(), value))
            })
            .collect();

        mocha_configs.extend(manifests.iter().filter_map(|(path, manifest)| {
            Some((
                parent_directory(path).to_owned(),
                json_to_value(manifest.get("mocha")?),
            ))
        }));

        if mocha_configs.is_empty() {
            mocha_configs.push((String::new(), Value::Object(Vec::new())));
        }

        for (directory, value) in mocha_configs {
            self.readings.push(read_mocha(&value, &directory));
        }
    }
}

pub(crate) fn imports_node_test(specifier: &str) -> bool {
    specifier == NODE_TEST_MODULE
}

pub(crate) fn is_vitest_config(path: &RepoPath) -> bool {
    let file_name = file_name(path);

    file_name.starts_with("vitest.config.")
        || file_name.starts_with("vitest.workspace.")
        || file_name.starts_with("vite.config.")
}

pub(crate) fn is_rstest_config(path: &RepoPath) -> bool {
    has_prefix(path, Preset::Rstest)
}

pub(crate) fn is_jest_config(path: &RepoPath) -> bool {
    file_name(path).starts_with("jest.config.")
}

fn is_vitest_entry(path: &RepoPath, repo_files: &[RepoPath]) -> bool {
    let config_name = file_name(path);
    let directory = parent_directory(path);
    let has_vitest_config_beside = || {
        repo_files.iter().any(|other| {
            parent_directory(other) == directory && file_name(other).starts_with("vitest.config.")
        })
    };

    config_name.starts_with("vitest.config.")
        || (config_name.starts_with("vite.config.") && !has_vitest_config_beside())
}

fn has_prefix(path: &RepoPath, preset: Preset) -> bool {
    preset
        .config_prefixes()
        .iter()
        .any(|prefix| file_name(path).starts_with(prefix))
}

fn package_manifests(repo_root: &Path, repo_files: &[RepoPath]) -> Vec<(RepoPath, JsonValue)> {
    repo_files
        .iter()
        .filter(|path| {
            file_name(path) == PACKAGE_MANIFEST
                && !path
                    .as_str()
                    .split('/')
                    .any(|segment| segment == "node_modules")
        })
        .filter_map(|path| {
            let text = fs::read_to_string(repo_root.join(path.as_str())).ok()?;

            Some((path.clone(), parse_json(&text).ok()?))
        })
        .collect()
}

fn detected_presets(
    repo_files: &[RepoPath],
    manifests: &[(RepoPath, JsonValue)],
) -> BTreeSet<Preset> {
    let declared_packages: BTreeSet<&str> = manifests
        .iter()
        .flat_map(|(_, manifest)| {
            [
                "dependencies",
                "devDependencies",
                "optionalDependencies",
                "peerDependencies",
            ]
            .into_iter()
            .filter_map(|field| manifest.get(field)?.as_object())
            .flat_map(|dependencies| dependencies.keys().map(String::as_str))
        })
        .collect();
    let has_jest_field = manifests
        .iter()
        .any(|(_, manifest)| manifest.get("jest").is_some());
    let runs_node_test = manifests.iter().any(|(_, manifest)| {
        manifest
            .get("scripts")
            .and_then(JsonValue::as_object)
            .is_some_and(|scripts| {
                scripts
                    .values()
                    .filter_map(JsonValue::as_str)
                    .any(runs_node_test_runner)
            })
    });

    Preset::ALL
        .into_iter()
        .filter(|preset| {
            (*preset == Preset::NodeTest && runs_node_test)
                || (*preset == Preset::Jest && has_jest_field)
                || preset
                    .packages()
                    .iter()
                    .any(|package| declared_packages.contains(package))
                || repo_files.iter().any(|path| has_prefix(path, *preset))
        })
        .collect()
}

fn runs_node_test_runner(script: &str) -> bool {
    script
        .split_whitespace()
        .any(|argument| argument == "--test" || argument.starts_with("--test="))
}

fn file_name(path: &RepoPath) -> &str {
    path.as_str()
        .rsplit_once('/')
        .map_or(path.as_str(), |(_, file_name)| file_name)
}
