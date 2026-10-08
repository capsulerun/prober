//! Dynamic loading inference: which modules are analysed, what their loads reach, and which sites stay unresolved.

mod evaluator;
mod summarizer;
mod summary;
mod targets;
mod value;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use anyhow::Result;
use prober_core::config::InferenceSettings;
use prober_core::files::RepoPath;
use prober_core::graph::{Inference, UnresolvedCause};
use rayon::prelude::*;

use crate::conventions::{is_module_file, is_plugin_manifest, is_test_file};
use crate::dynamic::evaluator::Evaluator;
use crate::dynamic::summarizer::summarize_module;
use crate::dynamic::targets::{GlobTarget, Target, match_globs, targets_of};
use crate::resolver::{ModuleResolver, Resolution};

const MAX_WAVES: usize = 4;

pub(crate) struct DynamicLink {
    pub dependent: RepoPath,
    pub dependency: RepoPath,
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
    fn add_link(&mut self, site: &Site, dependency: RepoPath, description: String) {
        if dependency == site.module {
            return;
        }

        self.links.push(DynamicLink {
            dependent: site.module.clone(),
            dependency,
            inference: Inference {
                line: site.line,
                description,
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
    pub settings: InferenceSettings,
}

impl DynamicLoading<'_> {
    pub fn infer(&self, seeds: Vec<RepoPath>) -> Result<DynamicInference> {
        let evaluator = Evaluator::new(self.repo_root, self.resolver, self.importers);
        let analysed = self.analyse(&evaluator, seeds);
        let mut inference = DynamicInference::default();
        let mut pending_globs = Vec::new();

        for module in &analysed {
            let Some(summary) = evaluator.summary(module) else {
                continue;
            };

            for effect in &summary.effects {
                let site = Site {
                    module: module.clone(),
                    line: effect.line,
                    expression: effect.expression.clone(),
                };

                if !self.settings.dynamic_imports {
                    if evaluator.is_load(module, effect) {
                        inference.add_unresolved(&site, UnresolvedCause::InferenceDisabled);
                    }

                    continue;
                }

                for load in evaluator.effect_loads(module, effect) {
                    if evaluator.is_reached_through_calls(module, effect, &load) {
                        continue;
                    }

                    for target in targets_of(&load) {
                        match target {
                            Target::Outside => {}
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
                                pending_globs.push((site.clone(), glob_target))
                            }
                            Target::Unresolved(cause) => inference.add_unresolved(&site, cause),
                        }
                    }
                }
            }
        }

        self.link_globs(pending_globs, &mut inference)?;
        inference.unresolved_sites.sort();
        inference.unresolved_sites.dedup();

        Ok(inference)
    }

    fn analyse(&self, evaluator: &Evaluator, mut seeds: Vec<RepoPath>) -> Vec<RepoPath> {
        let mut analysed = BTreeSet::new();

        seeds.sort();
        seeds.dedup();

        let mut wave = seeds;

        for _ in 0..MAX_WAVES {
            let summaries: Vec<_> = wave
                .par_iter()
                .filter_map(|module| {
                    let source_bytes = fs::read(self.repo_root.join(module.as_str())).ok()?;
                    let summary = summarize_module(module, &String::from_utf8_lossy(&source_bytes));

                    Some((module.clone(), summary))
                })
                .collect();

            for (module, summary) in summaries {
                analysed.insert(module.clone());
                evaluator.add_analysed(module, summary);
            }

            if !self.settings.dynamic_imports {
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

    fn link_repo_path(&self, site: &Site, path: &str, inference: &mut DynamicInference) {
        let specifier = self.resolver.absolute_specifier(path);

        match self.resolver.resolve_specifier(&site.module, &specifier) {
            Resolution::RepoFile(dependency) => {
                inference.add_link(site, dependency, format!("inferred path {path}"));
            }
            Resolution::Outside => {}
            Resolution::Missing => inference.add_unresolved(site, UnresolvedCause::NoMatchingFile),
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
            inference.add_link(site, dependency, format!("inferred specifier {specifier}"));
        }
    }

    fn link_globs(
        &self,
        pending_globs: Vec<(Site, GlobTarget)>,
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
            .map(|(_, glob_target)| glob_target.glob.clone())
            .collect();
        let matches = match_globs(&globs, &candidates)?;

        for ((site, glob_target), matched) in pending_globs.iter().zip(matches) {
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
                            format!("inferred plugin folder {folder_pattern} (id={id})"),
                        );
                    }
                }
                None => {
                    for file in matched {
                        inference.add_link(
                            site,
                            file,
                            format!("inferred glob {}", glob_target.glob),
                        );
                    }
                }
            }
        }

        Ok(())
    }
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
