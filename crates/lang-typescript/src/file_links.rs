//! Links through files: literals naming repository files, file system calls and globs, paths listed in data and text files, and the changes that reach a reader without a file at head.

mod accesses;
pub(crate) mod listed;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::Result;
use globset::{GlobBuilder, GlobSetBuilder};
use prober_core::changes::{ChangeKind, FileChange};
use prober_core::files::RepoPath;
use prober_core::graph::{EdgeKind, Inference};
use rayon::prelude::*;

use crate::dynamic::evaluator::Evaluator;
use crate::dynamic::summarizer::SummaryScope;
use crate::dynamic::summary::ModuleSummary;
use crate::dynamic::value::{AccessKind, Pattern, Value};
use crate::file_links::accesses::{AccessTarget, Location, access_targets};
use crate::file_links::listed::{ListedPath, is_stylesheet, listed_paths, lists_paths};
use crate::linker::LinkedFile;
use crate::presets::patterns::join_directory;
use crate::repo_index::RepoIndex;
use crate::resolver::ModuleResolver;
use crate::workspaces::parent_directory;

const MAX_LISTING_FILE_SIZE: u64 = 1 << 20;

const EVALUATION_CHUNK_SIZE: usize = 64;

const FILE_PATH: &str = "file path";

const LISTED_PATH: &str = "listed path";

pub(crate) struct FileLink {
    pub reader: RepoPath,
    pub file: RepoPath,
    pub kind: EdgeKind,
    pub inference: Inference,
}

pub(crate) struct ReaderChange {
    pub change_path: RepoPath,
    pub reader: RepoPath,
    pub description: String,
}

#[derive(Default)]
pub(crate) struct FileLinks {
    pub links: Vec<FileLink>,
    pub reader_changes: Vec<ReaderChange>,
}

pub(crate) struct FileReading<'f> {
    pub repo_root: &'f Path,
    pub repo_files: &'f [RepoPath],
    pub linked_files: &'f [LinkedFile],
    pub resolver: &'f ModuleResolver,
    pub package_directories: &'f BTreeSet<String>,
    pub changes: &'f [FileChange],
}

struct Reading<'r> {
    reader: &'r RepoPath,
    line: u32,
    kind: EdgeKind,
    target: ReadTarget,
}

enum ReadTarget {
    Named {
        inference_kind: &'static str,
        text: String,
        candidates: Vec<String>,
    },
    Accessed(AccessTarget),
}

struct ChangedPaths<'c> {
    removed: HashMap<&'c str, &'c FileChange>,
    added: HashMap<&'c str, &'c FileChange>,
}

impl<'c> ChangedPaths<'c> {
    fn new(changes: &'c [FileChange]) -> Self {
        let mut removed = HashMap::new();
        let mut added = HashMap::new();

        for change in changes {
            match &change.kind {
                ChangeKind::Added => {
                    added.insert(change.path.as_str(), change);
                }
                ChangeKind::Deleted => {
                    removed.insert(change.path.as_str(), change);
                }
                ChangeKind::Renamed { from } => {
                    added.insert(change.path.as_str(), change);
                    removed.insert(from.as_str(), change);
                }
                ChangeKind::Modified => {}
            }
        }

        Self { removed, added }
    }
}

struct Resolution<'r> {
    repo_index: &'r RepoIndex<'r>,
    changed_paths: &'r ChangedPaths<'r>,
    tests: HashSet<&'r RepoPath>,
    file_links: FileLinks,
}

impl<'r> Resolution<'r> {
    fn resolve(&mut self, readings: &[Reading]) -> Result<()> {
        let mut globbed = Vec::new();

        for (reading_index, reading) in readings.iter().enumerate() {
            match &reading.target {
                ReadTarget::Named {
                    inference_kind,
                    text,
                    candidates,
                } => self.resolve_named(reading, inference_kind, text, candidates),
                ReadTarget::Accessed(target) => match &target.location {
                    Location::Path(path) => self.resolve_path(reading, target, path),
                    Location::Glob(glob) => globbed.push((reading_index, glob.clone())),
                },
            }
        }

        self.resolve_globs(readings, &globbed)
    }

    fn link(
        &mut self,
        reading: &Reading,
        file: &RepoPath,
        pattern: &str,
        inference_kind: &'static str,
    ) {
        if file == reading.reader {
            return;
        }

        self.file_links.links.push(FileLink {
            reader: reading.reader.clone(),
            file: file.clone(),
            kind: reading.kind,
            inference: inference(reading.line, inference_kind, pattern),
        });
    }

    fn reach_reader(
        &mut self,
        reading: &Reading,
        change: &FileChange,
        pattern: &str,
        inference_kind: &'static str,
    ) {
        let inference = inference(reading.line, inference_kind, pattern);
        let description = format!(
            "read by {}:{} ({})",
            reading.reader,
            reading.line,
            inference.description()
        );

        self.file_links.reader_changes.push(ReaderChange {
            change_path: change.path.clone(),
            reader: reading.reader.clone(),
            description,
        });
    }

    fn resolve_named(
        &mut self,
        reading: &Reading,
        inference_kind: &'static str,
        text: &str,
        candidates: &[String],
    ) {
        let repo_index = self.repo_index;
        let changed_paths = self.changed_paths;

        for candidate in candidates {
            if let Some(file) = repo_index.file(candidate)
                && !self.tests.contains(file)
            {
                self.link(reading, file, text, inference_kind);
            }

            if let Some(change) = changed_paths.removed.get(candidate.as_str()) {
                self.reach_reader(reading, change, text, inference_kind);
            }
        }
    }

    fn resolve_path(&mut self, reading: &Reading, target: &AccessTarget, path: &str) {
        let inference_kind = access_inference_kind(target.kind);
        let changed_paths = self.changed_paths;

        if let Some(file) = self.repo_index.file(path) {
            self.link(reading, file, path, inference_kind);
        } else if lists_directories(target.kind) && self.repo_index.is_directory(path) {
            let pattern = format!("{path}/");

            for file in self.directory_files(path, target.recursive) {
                self.link(reading, file, &pattern, inference_kind);
            }

            self.resolve_directory_changes(reading, target, path, &pattern);
        }

        if let Some(change) = changed_paths.removed.get(path) {
            self.reach_reader(reading, change, path, inference_kind);
        }
    }

    fn resolve_directory_changes(
        &mut self,
        reading: &Reading,
        target: &AccessTarget,
        directory: &str,
        pattern: &str,
    ) {
        let inference_kind = access_inference_kind(target.kind);
        let changed_paths = self.changed_paths;
        let prefix = if directory.is_empty() {
            String::new()
        } else {
            format!("{directory}/")
        };
        let entry_changes: Vec<&FileChange> = changed_paths
            .removed
            .iter()
            .filter(|(path, _)| path.starts_with(&prefix))
            .filter(|(path, _)| target.recursive || self.is_removed_entry(&prefix, path))
            .chain(
                changed_paths
                    .added
                    .iter()
                    .filter(|(path, _)| path.starts_with(&prefix))
                    .filter(|(path, _)| !target.recursive && self.is_added_entry(&prefix, path)),
            )
            .map(|(_, change)| *change)
            .collect();

        for change in entry_changes {
            self.reach_reader(reading, change, pattern, inference_kind);
        }
    }

    fn is_removed_entry(&self, prefix: &str, path: &str) -> bool {
        match path[prefix.len()..].split_once('/') {
            None => true,
            Some((entry, _)) => {
                let entry_directory = format!("{prefix}{entry}");

                self.repo_index.files_under(&entry_directory).is_empty()
            }
        }
    }

    fn is_added_entry(&self, prefix: &str, path: &str) -> bool {
        let Some((entry, _)) = path[prefix.len()..].split_once('/') else {
            return false;
        };
        let entry_directory = format!("{prefix}{entry}");

        self.repo_index
            .files_under(&entry_directory)
            .iter()
            .all(|file| self.changed_paths.added.contains_key(file.as_str()))
    }

    fn directory_files(&self, directory: &str, recursive: bool) -> Vec<&'r RepoPath> {
        let files = self.repo_index.files_under(directory);

        if recursive {
            return files.to_vec();
        }

        files
            .iter()
            .filter(|file| parent_directory(file) == directory)
            .copied()
            .collect()
    }

    fn resolve_globs(&mut self, readings: &[Reading], globbed: &[(usize, String)]) -> Result<()> {
        if globbed.is_empty() {
            return Ok(());
        }

        let repo_index = self.repo_index;
        let mut glob_set_builder = GlobSetBuilder::new();

        for (_, glob) in globbed {
            glob_set_builder.add(GlobBuilder::new(glob).literal_separator(true).build()?);
        }

        let glob_set = glob_set_builder.build()?;
        let globs_directories = globbed.iter().any(|(reading_index, _)| {
            matches!(
                &readings[*reading_index].target,
                ReadTarget::Accessed(target) if lists_directories(target.kind)
            )
        });

        for &file in repo_index.files() {
            for slot in glob_set.matches(file.as_str()) {
                let (reading_index, glob) = &globbed[slot];
                let reading = &readings[*reading_index];

                if let ReadTarget::Accessed(target) = &reading.target {
                    self.link(reading, file, glob, access_inference_kind(target.kind));
                }
            }
        }

        if globs_directories {
            for directory in repo_index.directories() {
                for slot in glob_set.matches(directory) {
                    let (reading_index, glob) = &globbed[slot];
                    let reading = &readings[*reading_index];

                    if let ReadTarget::Accessed(target) = &reading.target
                        && lists_directories(target.kind)
                    {
                        for file in self.directory_files(directory, target.recursive) {
                            self.link(reading, file, glob, access_inference_kind(target.kind));
                        }
                    }
                }
            }
        }

        let changed_paths = self.changed_paths;

        for (path, change) in &changed_paths.removed {
            for slot in glob_set.matches(path) {
                let (reading_index, glob) = &globbed[slot];
                let reading = &readings[*reading_index];

                if let ReadTarget::Accessed(target) = &reading.target {
                    self.reach_reader(reading, change, glob, access_inference_kind(target.kind));
                }
            }
        }

        Ok(())
    }
}

impl FileReading<'_> {
    pub fn infer(&self, access_summaries: Vec<(usize, ModuleSummary)>) -> Result<FileLinks> {
        let repo_index = RepoIndex::new(self.repo_files);
        let changed_paths = ChangedPaths::new(self.changes);
        let is_known = |candidate: &str| {
            repo_index.file(candidate).is_some() || changed_paths.removed.contains_key(candidate)
        };
        let mut readings = self.named_readings(&is_known);

        readings.extend(self.accessed_readings(access_summaries));

        let mut resolution = Resolution {
            repo_index: &repo_index,
            changed_paths: &changed_paths,
            tests: self
                .linked_files
                .iter()
                .filter(|linked_file| linked_file.is_test)
                .map(|linked_file| &linked_file.path)
                .collect(),
            file_links: FileLinks::default(),
        };

        resolution.resolve(&readings)?;

        let file_extensions = file_extensions(self.repo_files);
        let mut listing_files = HashSet::new();
        let mut read_files: Vec<&RepoPath> = self
            .linked_files
            .iter()
            .flat_map(|linked_file| &linked_file.dependencies)
            .filter_map(|dependency| repo_index.file(dependency.path.as_str()))
            .collect();
        let mut first_new_link = 0;

        loop {
            read_files.extend(
                resolution.file_links.links[first_new_link..]
                    .iter()
                    .filter_map(|file_link| repo_index.file(file_link.file.as_str())),
            );
            first_new_link = resolution.file_links.links.len();

            let new_listing_files: Vec<&RepoPath> = read_files
                .drain(..)
                .filter(|path| lists_paths(path) && listing_files.insert(*path))
                .collect();

            if new_listing_files.is_empty() {
                break;
            }

            let listed_readings =
                self.listed_readings(&new_listing_files, &is_known, &file_extensions);

            resolution.resolve(&listed_readings)?;
        }

        Ok(resolution.file_links)
    }

    fn named_readings(&self, is_known: &(dyn Fn(&str) -> bool + Sync)) -> Vec<Reading<'_>> {
        self.linked_files
            .par_iter()
            .filter(|linked_file| !linked_file.path_literals.is_empty())
            .flat_map_iter(|linked_file| {
                let imported: HashSet<&str> = linked_file
                    .dependencies
                    .iter()
                    .map(|dependency| dependency.path.as_str())
                    .collect();
                let file_directory = parent_directory(&linked_file.path);
                let package_directory = self.package_directory(&linked_file.path);

                linked_file
                    .path_literals
                    .iter()
                    .filter_map(move |path_literal| {
                        let directories = if path_literal.text.contains('/') {
                            vec![file_directory, package_directory, ""]
                        } else {
                            vec![file_directory]
                        };
                        let candidates: Vec<String> =
                            candidate_paths(&directories, &path_literal.text)
                                .into_iter()
                                .filter(|candidate| {
                                    is_known(candidate) && !imported.contains(candidate.as_str())
                                })
                                .collect();

                        (!candidates.is_empty()).then(|| Reading {
                            reader: &linked_file.path,
                            line: path_literal.line,
                            kind: EdgeKind::Reads,
                            target: ReadTarget::Named {
                                inference_kind: FILE_PATH,
                                text: path_literal.text.clone(),
                                candidates,
                            },
                        })
                    })
            })
            .collect()
    }

    fn listed_readings<'r>(
        &self,
        listing_files: &[&'r RepoPath],
        is_known: &(dyn Fn(&str) -> bool + Sync),
        file_extensions: &HashSet<&str>,
    ) -> Vec<Reading<'r>> {
        listing_files
            .par_iter()
            .flat_map_iter(|&reader| {
                let listed = self
                    .read_listing(reader, file_extensions)
                    .unwrap_or_default();
                let kind = if is_stylesheet(reader) {
                    EdgeKind::Asset
                } else {
                    EdgeKind::Lists
                };
                let directories = [parent_directory(reader), ""];

                listed.into_iter().filter_map(move |listed_path| {
                    let candidates: Vec<String> = candidate_paths(&directories, &listed_path.text)
                        .into_iter()
                        .filter(|candidate| is_known(candidate))
                        .collect();

                    (!candidates.is_empty()).then_some(Reading {
                        reader,
                        line: listed_path.line,
                        kind,
                        target: ReadTarget::Named {
                            inference_kind: LISTED_PATH,
                            text: listed_path.text,
                            candidates,
                        },
                    })
                })
            })
            .collect()
    }

    fn read_listing(
        &self,
        path: &RepoPath,
        file_extensions: &HashSet<&str>,
    ) -> Option<Vec<ListedPath>> {
        let absolute_path = self.repo_root.join(path.as_str());

        if fs::metadata(&absolute_path).ok()?.len() > MAX_LISTING_FILE_SIZE {
            return None;
        }

        let text = String::from_utf8(fs::read(&absolute_path).ok()?).ok()?;

        (!text.contains('\0')).then(|| listed_paths(path, &text, file_extensions))
    }

    fn accessed_readings(&self, access_summaries: Vec<(usize, ModuleSummary)>) -> Vec<Reading<'_>> {
        access_summaries
            .into_par_iter()
            .map(|(file_index, summary)| (&self.linked_files[file_index].path, summary))
            .chunks(EVALUATION_CHUNK_SIZE)
            .flat_map_iter(|chunk| self.evaluate_accesses(chunk))
            .collect()
    }

    fn evaluate_accesses<'r>(&self, chunk: Vec<(&'r RepoPath, ModuleSummary)>) -> Vec<Reading<'r>> {
        let importers = HashMap::new();
        let evaluator = Evaluator::new(self.repo_root, self.resolver, &importers)
            .with_summary_scope(SummaryScope::Accesses);
        let mut readings = Vec::new();

        for (module, summary) in chunk {
            let accesses = summary.accesses.clone();
            let working_directory = self.working_directory(module);

            evaluator.add_summary(module.clone(), summary);

            for access in &accesses {
                let Some(file_access) =
                    evaluator.file_access(module, access, working_directory.clone())
                else {
                    continue;
                };

                for target in access_targets(&file_access, &working_directory) {
                    readings.push(Reading {
                        reader: module,
                        line: access.line,
                        kind: EdgeKind::Reads,
                        target: ReadTarget::Accessed(target),
                    });
                }
            }
        }

        readings
    }

    fn working_directory(&self, module: &RepoPath) -> Value {
        let package_directory = self.package_directory(module);
        let mut directories = vec![Pattern::repo_path("")];

        if !package_directory.is_empty() {
            directories.push(Pattern::repo_path(package_directory));
        }

        Value::Strings(directories)
    }

    fn package_directory<'p>(&'p self, path: &'p RepoPath) -> &'p str {
        let mut directory = parent_directory(path);

        loop {
            if let Some(package_directory) = self.package_directories.get(directory) {
                return package_directory;
            }

            match directory.rsplit_once('/') {
                Some((parent, _)) => directory = parent,
                None if directory.is_empty() => return "",
                None => directory = "",
            }
        }
    }
}

fn file_extensions(repo_files: &[RepoPath]) -> HashSet<&str> {
    repo_files
        .iter()
        .filter_map(|path| {
            let file_name = path.as_str().rsplit('/').next()?;

            Some(file_name.rsplit_once('.')?.1)
        })
        .collect()
}

fn candidate_paths(directories: &[&str], text: &str) -> Vec<String> {
    let mut candidates = Vec::new();

    for directory in directories {
        let Some(candidate) = join_directory(directory, text) else {
            continue;
        };

        if !candidate.is_empty() && !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }

    candidates
}

fn lists_directories(kind: AccessKind) -> bool {
    matches!(kind, AccessKind::List | AccessKind::Copy)
}

fn access_inference_kind(kind: AccessKind) -> &'static str {
    match kind {
        AccessKind::Read => "file read",
        AccessKind::List => "directory listing",
        AccessKind::Copy => "file copy",
        AccessKind::Glob => "file glob",
    }
}

fn inference(line: u32, kind: &'static str, pattern: &str) -> Inference {
    Inference {
        line,
        kind,
        pattern: pattern.to_owned(),
        detail: None,
    }
}
