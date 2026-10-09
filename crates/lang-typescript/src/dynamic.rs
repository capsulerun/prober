//! Dynamic loading inference: which modules are analysed, what their loads and process launches reach, and which sites stay unresolved.

pub(crate) mod evaluator;
pub(crate) mod spawn;
pub(crate) mod summarizer;
pub(crate) mod summary;
mod targets;
pub(crate) mod value;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use anyhow::Result;
use prober_core::config::InferenceSettings;
use prober_core::files::RepoPath;
use prober_core::graph::{EdgeKind, Inference, UnresolvedCause};
use rayon::prelude::*;

use crate::conventions::{is_module_file, is_plugin_manifest, is_test_file};
use crate::dynamic::evaluator::{Evaluator, Load, LoadBase, LoadScope};
use crate::dynamic::spawn::{Programs, Spawned, command_line_programs};
use crate::dynamic::summarizer::{SummaryScope, summarize_module};
use crate::dynamic::targets::{GlobTarget, Target, match_globs, targets_of};
use crate::dynamic::value::{Pattern, Value};
use crate::resolver::{ModuleResolver, Resolution, missing_path_candidates};
use crate::workspaces::package_directory;

const MAX_WAVES: usize = 4;

const MAX_SCRIPT_DEPTH: usize = 2;

const SPAWN: &str = "spawn";

const EVALUATION_CHUNK_SIZE: usize = 64;

type PendingGlob = (Site, GlobTarget, EdgeKind);

pub(crate) struct DynamicLink {
    pub dependent: RepoPath,
    pub dependency: RepoPath,
    pub kind: EdgeKind,
    pub inference: Inference,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DynamicSite {
    pub module: RepoPath,
    pub line: u32,
    pub expression: String,
    pub cause: UnresolvedCause,
}

#[derive(Default)]
pub(crate) struct DynamicInference {
    pub links: Vec<DynamicLink>,
    pub folders: BTreeMap<RepoPath, Vec<RepoPath>>,
    pub unresolved_sites: Vec<DynamicSite>,
}

impl DynamicInference {
    fn add_link(
        &mut self,
        site: &Site,
        dependency: RepoPath,
        kind: &'static str,
        pattern: &str,
        detail: Option<String>,
    ) {
        self.push_link(site, dependency, EdgeKind::Dynamic, kind, pattern, detail);
    }

    fn add_spawn_link(
        &mut self,
        site: &Site,
        dependency: RepoPath,
        pattern: &str,
        detail: Option<String>,
    ) {
        self.push_link(site, dependency, EdgeKind::Spawn, SPAWN, pattern, detail);
    }

    fn push_link(
        &mut self,
        site: &Site,
        dependency: RepoPath,
        edge_kind: EdgeKind,
        kind: &'static str,
        pattern: &str,
        detail: Option<String>,
    ) {
        if dependency == site.module {
            return;
        }

        self.links.push(DynamicLink {
            dependent: site.module.clone(),
            dependency,
            kind: edge_kind,
            inference: Inference {
                line: site.line,
                kind,
                pattern: pattern.to_owned(),
                detail,
            },
        });
    }

    fn add_unresolved(&mut self, site: &Site, cause: UnresolvedCause) {
        self.unresolved_sites.push(DynamicSite {
            module: site.module.clone(),
            line: site.line,
            expression: site.expression.clone(),
            cause,
        });
    }
}

#[derive(Clone)]
struct Site {
    module: RepoPath,
    line: u32,
    expression: String,
}

pub(crate) struct DynamicLoading<'d> {
    pub repo_root: &'d Path,
    pub resolver: &'d ModuleResolver,
    pub repo_files: &'d [RepoPath],
    pub importers: &'d HashMap<RepoPath, Vec<RepoPath>>,
    pub package_directories: &'d BTreeSet<String>,
    pub programs: &'d Programs,
    pub settings: InferenceSettings,
}

pub(crate) struct Analysis<'d> {
    loading: &'d DynamicLoading<'d>,
    evaluator: Evaluator<'d>,
    load_scope: LoadScope,
    analysed: Vec<RepoPath>,
}

impl<'d> DynamicLoading<'d> {
    pub fn analyse(&'d self, seeds: Vec<RepoPath>, load_scope: LoadScope) -> Analysis<'d> {
        let evaluator = Evaluator::new(self.repo_root, self.resolver, self.importers)
            .with_load_scope(load_scope);
        let analysed = self.analysed_modules(&evaluator, seeds, load_scope);

        Analysis {
            loading: self,
            evaluator,
            load_scope,
            analysed,
        }
    }
}

impl Analysis<'_> {
    pub fn infer(self) -> Result<DynamicInference> {
        let Analysis {
            loading,
            evaluator,
            load_scope,
            analysed,
        } = self;
        let (mut inference, pending_globs) = match load_scope {
            LoadScope::Modules => loading.infer_modules(&evaluator, &analysed, load_scope),
            LoadScope::Processes => loading.infer_processes(&evaluator, &analysed),
        };

        loading.link_globs(pending_globs, &mut inference)?;
        inference.unresolved_sites.sort();
        inference.unresolved_sites.dedup();

        Ok(inference)
    }
}

impl DynamicLoading<'_> {
    fn infer_processes(
        &self,
        evaluator: &Evaluator,
        analysed: &[RepoPath],
    ) -> (DynamicInference, Vec<PendingGlob>) {
        let shared_summaries = evaluator.analysed_summaries();

        analysed
            .par_chunks(EVALUATION_CHUNK_SIZE)
            .map(|chunk| {
                let chunk_evaluator = Evaluator::new(self.repo_root, self.resolver, self.importers)
                    .with_load_scope(LoadScope::Processes)
                    .with_shared_summaries(&shared_summaries);

                self.infer_modules(&chunk_evaluator, chunk, LoadScope::Processes)
            })
            .reduce(
                || (DynamicInference::default(), Vec::new()),
                |(mut inference, mut pending_globs), (chunk_inference, chunk_globs)| {
                    inference.links.extend(chunk_inference.links);
                    inference.folders.extend(chunk_inference.folders);
                    inference
                        .unresolved_sites
                        .extend(chunk_inference.unresolved_sites);
                    pending_globs.extend(chunk_globs);

                    (inference, pending_globs)
                },
            )
    }

    fn infer_modules(
        &self,
        evaluator: &Evaluator,
        modules: &[RepoPath],
        load_scope: LoadScope,
    ) -> (DynamicInference, Vec<PendingGlob>) {
        let mut inference = DynamicInference::default();
        let mut pending_globs = Vec::new();

        for module in modules {
            let Some(summary) = evaluator.summary(module) else {
                continue;
            };

            if load_scope == LoadScope::Processes {
                evaluator.set_process_directory(self.working_directory(module));
            }

            for effect in &summary.effects {
                let site = Site {
                    module: module.clone(),
                    line: effect.line,
                    expression: effect.expression.clone(),
                };

                if load_scope == LoadScope::Modules && !self.settings.dynamic_imports {
                    if evaluator.is_load(module, effect) {
                        inference.add_unresolved(&site, UnresolvedCause::InferenceDisabled);
                    }

                    continue;
                }

                for load in evaluator.effect_loads(module, effect) {
                    if evaluator.is_reached_through_calls(module, effect, &load) {
                        continue;
                    }

                    if load.is_spawn() {
                        self.link_spawned(&site, &load, 0, &mut inference, &mut pending_globs);

                        continue;
                    }

                    for target in targets_of(&load) {
                        match target {
                            Target::Outside | Target::Program { .. } => {}
                            Target::RepoPath(path) => {
                                self.link_repo_path(&site, &path, &mut inference)
                            }
                            Target::Bare {
                                importer,
                                specifier,
                            } => self.link_bare_specifier(
                                &site,
                                importer,
                                &specifier,
                                &mut inference,
                            ),
                            Target::Glob(glob_target) => {
                                pending_globs.push((site.clone(), glob_target, EdgeKind::Dynamic))
                            }
                            Target::Unresolved(cause) => inference.add_unresolved(&site, cause),
                        }
                    }
                }
            }
        }

        (inference, pending_globs)
    }
}

impl DynamicLoading<'_> {
    fn analysed_modules(
        &self,
        evaluator: &Evaluator,
        mut seeds: Vec<RepoPath>,
        load_scope: LoadScope,
    ) -> Vec<RepoPath> {
        let mut analysed = BTreeSet::new();

        seeds.sort();
        seeds.dedup();

        let mut wave = seeds;

        for _ in 0..MAX_WAVES {
            let summaries: Vec<_> = wave
                .par_iter()
                .filter_map(|module| {
                    let source_bytes = fs::read(self.repo_root.join(module.as_str())).ok()?;
                    let summary = summarize_module(
                        module,
                        &String::from_utf8_lossy(&source_bytes),
                        SummaryScope::Loads,
                    );

                    Some((module.clone(), summary))
                })
                .collect();

            for (module, summary) in summaries {
                analysed.insert(module.clone());
                evaluator.add_analysed(module, summary);
            }

            if load_scope == LoadScope::Modules && !self.settings.dynamic_imports {
                break;
            }

            let next_wave: BTreeSet<RepoPath> = wave
                .iter()
                .filter(|module| evaluator.exports_parametric_loader(module))
                .flat_map(|module| self.importers.get(module).into_iter().flatten())
                .filter(|importer| !evaluator.is_analysed(importer))
                .cloned()
                .collect();

            if next_wave.is_empty() {
                break;
            }

            wave = next_wave.into_iter().collect();
        }

        analysed.into_iter().collect()
    }

    fn working_directory(&self, module: &RepoPath) -> Value {
        let package_directory = package_directory(self.package_directories, module);
        let mut directories = vec![Pattern::repo_path("")];

        if !package_directory.is_empty() {
            directories.push(Pattern::repo_path(package_directory));
        }

        Value::Strings(directories)
    }

    fn link_spawned(
        &self,
        site: &Site,
        load: &Load,
        depth: usize,
        inference: &mut DynamicInference,
        pending_globs: &mut Vec<PendingGlob>,
    ) {
        for target in targets_of(load) {
            match target {
                Target::RepoPath(path) => {
                    self.link_spawned_path(site, &path, &path, None, inference);
                }
                Target::Glob(glob_target) if names_scripts(&glob_target.glob) => {
                    pending_globs.push((site.clone(), glob_target, EdgeKind::Spawn));
                }
                Target::Program { name, directories } => {
                    self.link_program(site, &name, &directories, depth, inference, pending_globs);
                }
                _ => {}
            }
        }
    }

    fn link_program(
        &self,
        site: &Site,
        name: &str,
        directories: &[String],
        depth: usize,
        inference: &mut DynamicInference,
        pending_globs: &mut Vec<PendingGlob>,
    ) {
        let mut runs_a_script = false;

        for directory in directories {
            let Some(command_line) = self.programs.script(directory, name) else {
                continue;
            };

            runs_a_script = true;

            if depth >= MAX_SCRIPT_DEPTH {
                continue;
            }

            let script_directories = vec![Pattern::repo_path(directory)];

            for spawned in command_line_programs(&Value::text(command_line), 0) {
                let load = match spawned {
                    Spawned::Script(script) => Load {
                        specifier: script,
                        base: LoadBase::Process(script_directories.clone()),
                    },
                    Spawned::Program(program) => Load {
                        specifier: Value::text(&program),
                        base: LoadBase::Program(script_directories.clone()),
                    },
                };

                self.link_spawned(site, &load, depth + 1, inference, pending_globs);
            }
        }

        if runs_a_script {
            return;
        }

        for (manifest, target) in self.programs.bins(name) {
            let detail = Some(format!("bin of {manifest}"));

            self.link_spawned_path(site, target, name, detail, inference);
        }
    }

    fn link_spawned_path(
        &self,
        site: &Site,
        path: &str,
        pattern: &str,
        detail: Option<String>,
        inference: &mut DynamicInference,
    ) {
        let specifier = self.resolver.absolute_specifier(path);

        if let Resolution::RepoFile(dependency) =
            self.resolver.resolve_specifier(&site.module, &specifier)
        {
            inference.add_spawn_link(site, dependency, pattern, detail);
        }
    }

    fn link_repo_path(&self, site: &Site, path: &str, inference: &mut DynamicInference) {
        let specifier = self.resolver.absolute_specifier(path);

        match self.resolver.resolve_specifier(&site.module, &specifier) {
            Resolution::RepoFile(dependency) => {
                inference.add_link(site, dependency, "path", path, None);
            }
            Resolution::Outside => {}
            Resolution::Missing => {
                for candidate in missing_path_candidates(path) {
                    inference.add_link(site, candidate, "path", path, None);
                }

                inference.add_unresolved(site, UnresolvedCause::NoMatchingFile);
            }
        }
    }

    fn link_bare_specifier(
        &self,
        site: &Site,
        importer: Option<RepoPath>,
        specifier: &str,
        inference: &mut DynamicInference,
    ) {
        let importer = importer.unwrap_or_else(|| site.module.clone());

        if let Resolution::RepoFile(dependency) =
            self.resolver.resolve_specifier(&importer, specifier)
        {
            inference.add_link(site, dependency, "specifier", specifier, None);
        }
    }

    fn link_globs(
        &self,
        pending_globs: Vec<PendingGlob>,
        inference: &mut DynamicInference,
    ) -> Result<()> {
        if pending_globs.is_empty() {
            return Ok(());
        }

        let mut repo_files = self.repo_files.to_vec();

        repo_files.sort();

        let candidates: Vec<RepoPath> = repo_files
            .iter()
            .filter(|path| is_module_file(path) && !is_test_file(path))
            .cloned()
            .collect();
        let globs: Vec<String> = pending_globs
            .iter()
            .map(|(_, glob_target, _)| glob_target.glob.clone())
            .collect();
        let matches = match_globs(&globs, &candidates)?;

        for ((site, glob_target, kind), matched) in pending_globs.iter().zip(matches) {
            if *kind == EdgeKind::Spawn {
                for file in matched {
                    inference.add_spawn_link(site, file, &glob_target.glob, None);
                }

                continue;
            }

            if matched.is_empty() {
                inference.add_unresolved(site, UnresolvedCause::NoMatchingFile);

                continue;
            }

            let plugin_folders = glob_target
                .folder_segment
                .filter(|_| self.settings.plugin_folders)
                .and_then(|segment| plugin_folders(&repo_files, &matched, segment));

            match plugin_folders {
                Some(folders) => {
                    let folder_pattern = folder_pattern(
                        &glob_target.glob,
                        glob_target.folder_segment.unwrap_or_default(),
                    );

                    for folder in folders {
                        let Ok(folder_path) = RepoPath::new(folder.as_str()) else {
                            continue;
                        };
                        let id = folder.rsplit('/').next().unwrap_or_default().to_owned();

                        inference
                            .folders
                            .entry(folder_path.clone())
                            .or_insert_with(|| {
                                files_under(&repo_files, &folder)
                                    .filter(|file| !is_test_file(file))
                                    .cloned()
                                    .collect()
                            });
                        inference.add_link(
                            site,
                            folder_path,
                            "plugin folder",
                            &folder_pattern,
                            Some(format!("id={id}")),
                        );
                    }
                }
                None => {
                    for file in matched {
                        inference.add_link(site, file, "glob", &glob_target.glob, None);
                    }
                }
            }
        }

        Ok(())
    }
}

fn names_scripts(glob: &str) -> bool {
    let (directory, file_name) = glob.rsplit_once('/').unwrap_or(("", glob));
    let stem = file_name
        .rsplit_once('.')
        .map_or(file_name, |(stem, _)| stem);

    !directory.contains(['*', '?', '[', '{']) && stem.chars().any(|character| character != '*')
}

fn plugin_folders(
    repo_files: &[RepoPath],
    matched: &[RepoPath],
    segment: usize,
) -> Option<Vec<String>> {
    let folders: BTreeSet<String> = matched
        .iter()
        .filter_map(|path| {
            let segments: Vec<&str> = path.as_str().split('/').collect();

            (segments.len() > segment + 1).then(|| segments[..=segment].join("/"))
        })
        .collect();
    let every_folder_has_a_manifest = !folders.is_empty()
        && folders.iter().all(|folder| {
            files_under(repo_files, folder).any(|file| {
                file.as_str()
                    .rsplit_once('/')
                    .is_some_and(|(parent, _)| parent == folder)
                    && is_plugin_manifest(file)
            })
        });

    every_folder_has_a_manifest.then(|| folders.into_iter().collect())
}

fn files_under<'f>(repo_files: &'f [RepoPath], folder: &str) -> impl Iterator<Item = &'f RepoPath> {
    let prefix = format!("{folder}/");
    let start = repo_files.partition_point(|path| path.as_str() < prefix.as_str());

    repo_files[start..]
        .iter()
        .take_while(move |path| path.as_str().starts_with(&prefix))
}

fn folder_pattern(glob: &str, segment: usize) -> String {
    let segments: Vec<&str> = glob
        .split('/')
        .take(segment + 1)
        .enumerate()
        .map(|(position, glob_segment)| {
            if position == segment {
                "{id}"
            } else {
                glob_segment
            }
        })
        .collect();

    format!("{}/", segments.join("/"))
}
