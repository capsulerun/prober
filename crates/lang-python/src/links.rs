//! Links from evaluated sites and literals: computed imports to modules, globs and plugin folders, paths to the files they read, dotted strings to their modules, and the sites that stay unresolved.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::Result;
use globset::{GlobBuilder, GlobSetBuilder};
use prober_core::files::RepoPath;
use prober_core::graph::{EdgeKind, Inference, UnresolvedCause};

use crate::conventions::{is_module_file, join_directory, parent_directory, split_path};
use crate::evaluator::{Evaluator, Hole, Part, Pattern, Value};
use crate::modules::{ImportTarget, ModuleIndex, normalized_join};
use crate::summary::{ModuleSummary, Site, SiteKind};

const COMPUTED_IMPORT: &str = "computed import";

const PLUGIN_FOLDER: &str = "plugin folder";

const MODULE_PATH: &str = "module path";

const FILE_PATH: &str = "file path";

const LISTED_MODULE: &str = "listed module";

const DATA_EXTENSIONS: [&str; 6] = ["json", "yaml", "yml", "toml", "ini", "cfg"];

const MAX_DATA_FILE_SIZE: u64 = 1 << 20;

const PLUGIN_MANIFEST_NAMES: [&str; 4] = [
    "manifest.json",
    "plugin.json",
    "package.json",
    "pyproject.toml",
];

const PLUGIN_MANIFEST_SUFFIX: &str = ".plugin.json";

const PROJECT_FILES: [&str; 4] = ["pyproject.toml", "setup.py", "pytest.ini", "tox.ini"];

const FILE_EXTENSIONS: &[&str] = &[
    "cfg", "csv", "html", "ini", "j2", "jinja", "json", "md", "py", "pyi", "rst", "sql", "toml",
    "txt", "xml", "yaml", "yml",
];

const GLOB_CHARACTERS: [char; 4] = ['*', '?', '[', '{'];

pub(crate) struct Link {
    pub reader: RepoPath,
    pub file: RepoPath,
    pub kind: EdgeKind,
    pub inference: Inference,
}

pub(crate) struct UnresolvedImport {
    pub module: RepoPath,
    pub line: u32,
    pub expression: String,
    pub cause: UnresolvedCause,
}

#[derive(Default)]
pub(crate) struct Links {
    pub links: Vec<Link>,
    pub folders: BTreeMap<RepoPath, Vec<RepoPath>>,
    pub unresolved: Vec<UnresolvedImport>,
}

pub(crate) struct LinkSettings {
    pub dynamic_imports: bool,
    pub plugin_folders: bool,
    pub files: bool,
}

struct PendingGlob {
    reader: RepoPath,
    site_module: RepoPath,
    line: u32,
    expression: String,
    globs: Vec<String>,
    kind: EdgeKind,
    names_folders: bool,
    pattern: String,
}

pub(crate) struct Linking<'l> {
    pub repo_root: &'l Path,
    pub repo_files: &'l [RepoPath],
    pub index: &'l ModuleIndex,
    pub summaries: &'l HashMap<RepoPath, ModuleSummary>,
    pub tests: &'l BTreeSet<RepoPath>,
    pub settings: LinkSettings,
}

impl Linking<'_> {
    pub fn links(&self) -> Result<Links> {
        let mut sorted_files: Vec<&RepoPath> = self.repo_files.iter().collect();

        sorted_files.sort();

        let file_set: HashSet<&str> = self.repo_files.iter().map(RepoPath::as_str).collect();
        let directories = directories(self.repo_files);
        let resolution = Resolution {
            tests: self.tests,
            file_set: &file_set,
            directories: &directories,
            sorted_files: &sorted_files,
            index: self.index,
        };
        let evaluator = Evaluator::new(self.summaries, self.index);
        let mut links = Links::default();
        let mut pending = Vec::new();
        let mut modules: Vec<&RepoPath> = self.summaries.keys().collect();

        modules.sort();

        for &module in &modules {
            let summary = &self.summaries[module];

            for site in &summary.sites {
                let enabled = match site.kind {
                    SiteKind::Import => self.settings.dynamic_imports,
                    SiteKind::Path | SiteKind::Listing => self.settings.files,
                };

                if !enabled {
                    if site.kind == SiteKind::Import {
                        links.unresolved.push(UnresolvedImport {
                            module: module.clone(),
                            line: site.line,
                            expression: site.expression.clone(),
                            cause: UnresolvedCause::InferenceDisabled,
                        });
                    }

                    continue;
                }

                for site_value in evaluator.site_values(module, site) {
                    let context = site
                        .context
                        .as_ref()
                        .map(|context| evaluator.evaluate_in(module, context));
                    let reading = Reading {
                        reader: site_value.reader,
                        site_module: module,
                        site,
                    };

                    match site.kind {
                        SiteKind::Import => resolution.import(
                            &reading,
                            &site_value.value,
                            context.as_ref(),
                            &mut links,
                            &mut pending,
                        ),
                        SiteKind::Path | SiteKind::Listing => {
                            resolution.path(&reading, &site_value.value, &mut links, &mut pending);
                        }
                    }
                }
            }

            for (line, text) in &summary.strings {
                if self.settings.dynamic_imports {
                    let kind = if summary.pytest.plugins.contains(text) {
                        EdgeKind::Import
                    } else {
                        EdgeKind::Dynamic
                    };

                    resolution.dotted_string(module, *line, text, kind, &mut links);
                }

                if self.settings.files {
                    resolution.file_string(module, *line, text, &mut links);
                }
            }
        }

        resolution.resolve_globs(pending, self.settings.plugin_folders, &mut links)?;

        if self.settings.dynamic_imports {
            let listed = self.listed_modules(&links.links);

            links.links.extend(listed);
        }

        Ok(links)
    }

    fn listed_modules(&self, links: &[Link]) -> Vec<Link> {
        let data_files: BTreeSet<&RepoPath> = links
            .iter()
            .filter(|link| link.kind == EdgeKind::Reads && is_data_file(&link.file))
            .map(|link| &link.file)
            .collect();
        let mut listed = Vec::new();

        for data_file in data_files {
            let path = self.repo_root.join(data_file.as_str());
            let small =
                fs::metadata(&path).is_ok_and(|metadata| metadata.len() <= MAX_DATA_FILE_SIZE);
            let Some(text) = small.then(|| fs::read_to_string(&path).ok()).flatten() else {
                continue;
            };
            let mut seen = HashSet::new();

            for (offset, token) in dotted_tokens(&text) {
                if !seen.insert(token) {
                    continue;
                }

                let ImportTarget::Repo(files) = self.index.resolve_absolute(data_file, token)
                else {
                    continue;
                };
                let line = text[..offset].matches('\n').count() as u32 + 1;

                for file in files.into_iter().filter(|file| !self.tests.contains(file)) {
                    listed.push(Link {
                        reader: data_file.clone(),
                        file,
                        kind: EdgeKind::Lists,
                        inference: Inference {
                            line,
                            kind: LISTED_MODULE,
                            pattern: token.to_owned(),
                            detail: None,
                        },
                    });
                }
            }
        }

        listed
    }
}

fn is_data_file(path: &RepoPath) -> bool {
    split_path(path.as_str())
        .1
        .rsplit_once('.')
        .is_some_and(|(_, extension)| DATA_EXTENSIONS.contains(&extension))
}

fn dotted_tokens(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let is_token_character =
        |character: char| character == '_' || character == '.' || character.is_alphanumeric();
    let mut start = None;
    let mut tokens = Vec::new();

    for (offset, character) in text.char_indices().chain([(text.len(), ' ')]) {
        match (start, is_token_character(character)) {
            (None, true) => start = Some(offset),
            (Some(token_start), false) => {
                tokens.push((token_start, &text[token_start..offset]));
                start = None;
            }
            _ => {}
        }
    }

    tokens.into_iter().filter(|(_, token)| {
        let segments: Vec<&str> = token.split('.').collect();

        segments.len() >= 2
            && segments.iter().all(|segment| is_identifier(segment))
            && !FILE_EXTENSIONS.contains(segments.last().unwrap_or(&""))
    })
}

struct Reading<'r> {
    reader: &'r RepoPath,
    site_module: &'r RepoPath,
    site: &'r Site,
}

struct Resolution<'r> {
    tests: &'r BTreeSet<RepoPath>,
    file_set: &'r HashSet<&'r str>,
    directories: &'r HashSet<String>,
    sorted_files: &'r [&'r RepoPath],
    index: &'r ModuleIndex,
}

impl Resolution<'_> {
    fn import(
        &self,
        reading: &Reading,
        value: &Value,
        context: Option<&Value>,
        links: &mut Links,
        pending: &mut Vec<PendingGlob>,
    ) {
        let package = context
            .and_then(|context| match context {
                Value::Strings(patterns) => patterns.first().and_then(Pattern::text),
                _ => None,
            })
            .or_else(|| self.index.package_name(reading.site_module.as_str()));

        for pattern in value.patterns() {
            let pattern = absolute_module(&pattern, package.as_deref());

            if let Some(dotted) = pattern.text() {
                match self.index.resolve_absolute(reading.site_module, &dotted) {
                    ImportTarget::Repo(files) => {
                        for file in files {
                            links.links.push(Link {
                                reader: reading.reader.clone(),
                                file,
                                kind: EdgeKind::Dynamic,
                                inference: inference(reading, COMPUTED_IMPORT, &dotted, None),
                            });
                        }
                    }
                    ImportTarget::Missing => {
                        unresolved(reading, UnresolvedCause::NoMatchingFile, links)
                    }
                    ImportTarget::External(_) | ImportTarget::Standard => {}
                }

                continue;
            }

            let Some((dotted_glob, names_folders)) = dotted_glob(&pattern) else {
                unresolved(reading, hole_cause(&pattern), links);

                continue;
            };

            pending.push(PendingGlob {
                reader: reading.reader.clone(),
                site_module: reading.site_module.clone(),
                line: reading.site.line,
                expression: reading.site.expression.clone(),
                globs: self.index.module_globs(reading.site_module, &dotted_glob),
                kind: EdgeKind::Dynamic,
                names_folders,
                pattern: dotted_glob,
            });
        }
    }

    fn path(
        &self,
        reading: &Reading,
        value: &Value,
        links: &mut Links,
        pending: &mut Vec<PendingGlob>,
    ) {
        let listing = reading.site.kind == SiteKind::Listing;

        for pattern in value.patterns() {
            for located in self.locate(reading.reader, &pattern) {
                match located {
                    Located::Path(path) => {
                        if self.file_set.contains(path.as_str()) {
                            self.link_file(reading, &path, &path, links);
                        } else if listing && self.directories.contains(&path) {
                            for file in self.files_under(&path) {
                                self.link_file(reading, file.as_str(), &path, links);
                            }
                        }
                    }
                    Located::Glob(glob) => {
                        let names_a_file = split_path(&glob)
                            .1
                            .chars()
                            .any(|character| !GLOB_CHARACTERS.contains(&character));

                        if !listing && !names_a_file {
                            continue;
                        }

                        pending.push(PendingGlob {
                            reader: reading.reader.clone(),
                            site_module: reading.site_module.clone(),
                            line: reading.site.line,
                            expression: reading.site.expression.clone(),
                            globs: vec![glob.clone()],
                            kind: EdgeKind::Reads,
                            names_folders: false,
                            pattern: glob,
                        });
                    }
                }
            }
        }
    }

    fn link_file(&self, reading: &Reading, file: &str, pattern: &str, links: &mut Links) {
        let Ok(file) = RepoPath::new(file) else {
            return;
        };

        if file == *reading.reader {
            return;
        }

        links.links.push(Link {
            reader: reading.reader.clone(),
            file,
            kind: EdgeKind::Reads,
            inference: inference(reading, FILE_PATH, pattern, None),
        });
    }

    fn locate(&self, reader: &RepoPath, pattern: &Pattern) -> Vec<Located> {
        let (anchored, parts) = match pattern.0.split_first() {
            Some((Part::Root, rest)) => (true, rest),
            _ => (false, &pattern.0[..]),
        };
        let mut text = String::new();
        let mut glob = String::new();
        let mut has_hole = false;

        for part in parts {
            match part {
                Part::Text(part) => {
                    text.push_str(part);
                    glob.push_str(&escape_glob(part));
                }
                Part::Hole(_) => {
                    has_hole = true;

                    if !glob.ends_with('*') {
                        glob.push('*');
                    }
                }
                Part::Root => return Vec::new(),
            }
        }

        if text.starts_with('/') && !anchored {
            return Vec::new();
        }

        let bases = if anchored {
            vec![String::new()]
        } else {
            self.working_directories(reader)
        };
        let mut located = Vec::new();

        for base in bases {
            if has_hole {
                let Some(joined) = normalized_join(&base, glob.trim_start_matches('/')) else {
                    continue;
                };
                let literal_directory = joined
                    .split('/')
                    .next()
                    .is_some_and(|segment| !segment.is_empty() && !segment.contains('*'));

                if literal_directory {
                    located.push(Located::Glob(joined));
                }
            } else if let Some(joined) = normalized_join(&base, text.trim_start_matches('/'))
                && !joined.is_empty()
            {
                located.push(Located::Path(joined));
            }
        }

        located
    }

    fn working_directories(&self, reader: &RepoPath) -> Vec<String> {
        let mut directories = vec![String::new()];
        let mut directory = parent_directory(reader.as_str());

        while !directory.is_empty() {
            let is_project = PROJECT_FILES.iter().any(|file| {
                self.file_set
                    .contains(join_directory(directory, file).as_str())
            });

            if is_project {
                directories.push(directory.to_owned());

                break;
            }

            directory = parent_directory(directory);
        }

        directories
    }

    fn files_under(&self, directory: &str) -> impl Iterator<Item = &RepoPath> {
        let prefix = format!("{directory}/");
        let start = self
            .sorted_files
            .partition_point(|path| path.as_str() < prefix.as_str());

        self.sorted_files[start..]
            .iter()
            .take_while(move |path| path.as_str().starts_with(&prefix))
            .copied()
    }

    fn dotted_string(
        &self,
        module: &RepoPath,
        line: u32,
        text: &str,
        kind: EdgeKind,
        links: &mut Links,
    ) {
        let segments: Vec<&str> = text.split('.').collect();
        let is_dotted = segments.len() >= 2
            && segments.iter().all(|segment| is_identifier(segment))
            && !FILE_EXTENSIONS.contains(segments.last().unwrap_or(&""));

        if !is_dotted {
            return;
        }

        if let ImportTarget::Repo(files) = self.index.resolve_absolute(module, text) {
            for file in files
                .into_iter()
                .filter(|file| file != module && !self.tests.contains(file))
            {
                links.links.push(Link {
                    reader: module.clone(),
                    file,
                    kind,
                    inference: Inference {
                        line,
                        kind: MODULE_PATH,
                        pattern: text.to_owned(),
                        detail: None,
                    },
                });
            }
        }
    }

    fn file_string(&self, module: &RepoPath, line: u32, text: &str, links: &mut Links) {
        let has_extension = split_path(text)
            .1
            .rsplit_once('.')
            .is_some_and(|(stem, extension)| !stem.is_empty() && !extension.is_empty());

        if !has_extension || text.starts_with('/') || text.contains(GLOB_CHARACTERS) {
            return;
        }

        let mut bases = self.working_directories(module);

        bases.push(parent_directory(module.as_str()).to_owned());

        let mut seen = BTreeSet::new();

        for base in bases {
            let Some(path) = normalized_join(&base, text) else {
                continue;
            };

            if path != module.as_str()
                && self.file_set.contains(path.as_str())
                && seen.insert(path.clone())
            {
                let Ok(file) = RepoPath::new(path) else {
                    continue;
                };

                links.links.push(Link {
                    reader: module.clone(),
                    file,
                    kind: EdgeKind::Reads,
                    inference: Inference {
                        line,
                        kind: FILE_PATH,
                        pattern: text.to_owned(),
                        detail: None,
                    },
                });
            }
        }
    }

    fn resolve_globs(
        &self,
        pending: Vec<PendingGlob>,
        plugin_folders: bool,
        links: &mut Links,
    ) -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }

        let mut builder = GlobSetBuilder::new();
        let mut owners = Vec::new();

        for (pending_index, pending_glob) in pending.iter().enumerate() {
            for glob in &pending_glob.globs {
                let Ok(glob) = GlobBuilder::new(glob).literal_separator(true).build() else {
                    continue;
                };

                builder.add(glob);
                owners.push(pending_index);
            }
        }

        let glob_set = builder.build()?;
        let mut matched: Vec<BTreeSet<&RepoPath>> = vec![BTreeSet::new(); pending.len()];

        for &file in self.sorted_files {
            for glob_index in glob_set.matches(file.as_str()) {
                let pending_glob = &pending[owners[glob_index]];

                if pending_glob.kind == EdgeKind::Dynamic && !is_module_file(file) {
                    continue;
                }

                matched[owners[glob_index]].insert(file);
            }
        }

        for (pending_glob, matched) in pending.iter().zip(matched) {
            let site = UnresolvedImport {
                module: pending_glob.site_module.clone(),
                line: pending_glob.line,
                expression: pending_glob.expression.clone(),
                cause: UnresolvedCause::NoMatchingFile,
            };

            if matched.is_empty() {
                if pending_glob.kind == EdgeKind::Dynamic {
                    links.unresolved.push(site);
                }

                continue;
            }

            let folders = (plugin_folders && pending_glob.names_folders)
                .then(|| self.plugin_folders(&matched, &pending_glob.globs))
                .flatten();
            let inference_of =
                |kind: &'static str, pattern: &str, detail: Option<String>| Inference {
                    line: pending_glob.line,
                    kind,
                    pattern: pattern.to_owned(),
                    detail,
                };

            match folders {
                Some(folders) => {
                    for folder in folders {
                        let Ok(folder_path) = RepoPath::new(folder.as_str()) else {
                            continue;
                        };
                        let id = split_path(&folder).1.to_owned();

                        links.folders.entry(folder_path.clone()).or_insert_with(|| {
                            self.files_under(&folder)
                                .filter(|file| is_module_file(file))
                                .cloned()
                                .collect()
                        });
                        links.links.push(Link {
                            reader: pending_glob.reader.clone(),
                            file: folder_path,
                            kind: EdgeKind::Dynamic,
                            inference: inference_of(
                                PLUGIN_FOLDER,
                                &join_directory(parent_directory(&folder), "{id}/"),
                                Some(format!("id={id}")),
                            ),
                        });
                    }
                }
                None => {
                    let kind = if pending_glob.kind == EdgeKind::Dynamic {
                        COMPUTED_IMPORT
                    } else {
                        FILE_PATH
                    };

                    for file in matched {
                        if *file == pending_glob.reader {
                            continue;
                        }

                        links.links.push(Link {
                            reader: pending_glob.reader.clone(),
                            file: file.clone(),
                            kind: pending_glob.kind,
                            inference: inference_of(kind, &pending_glob.pattern, None),
                        });
                    }
                }
            }
        }

        Ok(())
    }

    fn plugin_folders(
        &self,
        matched: &BTreeSet<&RepoPath>,
        globs: &[String],
    ) -> Option<Vec<String>> {
        let prefixes: Vec<&str> = globs
            .iter()
            .filter_map(|glob| glob.split_once('*').map(|(prefix, _)| prefix))
            .filter(|prefix| prefix.is_empty() || prefix.ends_with('/'))
            .collect();
        let folders: BTreeSet<String> = matched
            .iter()
            .filter_map(|path| {
                let path = path.as_str();
                let prefix = prefixes.iter().find(|prefix| path.starts_with(**prefix))?;
                let (folder, rest) = path[prefix.len()..].split_once('/')?;

                (!rest.is_empty()).then(|| format!("{prefix}{folder}"))
            })
            .collect();
        let every_folder_has_a_manifest = !folders.is_empty()
            && folders.iter().all(|folder| {
                self.files_under(folder).any(|file| {
                    let (directory, name) = split_path(file.as_str());

                    directory == folder
                        && (PLUGIN_MANIFEST_NAMES.contains(&name)
                            || name.ends_with(PLUGIN_MANIFEST_SUFFIX))
                })
            });

        every_folder_has_a_manifest.then(|| folders.into_iter().collect())
    }
}

enum Located {
    Path(String),
    Glob(String),
}

fn absolute_module(pattern: &Pattern, package: Option<&str>) -> Pattern {
    let Some(Part::Text(first)) = pattern.0.first() else {
        return pattern.clone();
    };
    let level = first
        .chars()
        .take_while(|character| *character == '.')
        .count();

    if level == 0 {
        return pattern.clone();
    }

    let Some(package) = package else {
        return Pattern(vec![Part::Hole(Hole::Runtime)]);
    };
    let mut segments: Vec<&str> = package.split('.').filter(|s| !s.is_empty()).collect();

    for _ in 1..level {
        if segments.pop().is_none() {
            return Pattern(vec![Part::Hole(Hole::Runtime)]);
        }
    }

    let rest = &first[level..];
    let mut prefix = segments.join(".");

    if !rest.is_empty() || pattern.0.len() > 1 {
        if !prefix.is_empty() {
            prefix.push('.');
        }

        prefix.push_str(rest);
    }

    let mut parts = vec![Part::Text(prefix)];

    parts.extend(pattern.0[1..].iter().cloned());

    Pattern(parts)
}

fn dotted_glob(pattern: &Pattern) -> Option<(String, bool)> {
    let Some(Part::Text(first)) = pattern.0.first() else {
        return None;
    };

    if !first.contains('.') {
        return None;
    }

    let mut glob = String::new();
    let mut names_folders = None;

    for part in &pattern.0 {
        match part {
            Part::Text(text) => {
                if text.contains(GLOB_CHARACTERS) {
                    return None;
                }

                glob.push_str(text);
            }
            Part::Hole(_) => {
                if names_folders.is_none() {
                    names_folders = Some(glob.ends_with('.'));
                }

                glob.push('*');
            }
            Part::Root => return None,
        }
    }

    Some((glob, names_folders.unwrap_or_default()))
}

fn hole_cause(pattern: &Pattern) -> UnresolvedCause {
    let mut holes = pattern.holes().peekable();

    if holes.peek().is_some() && holes.all(|hole| matches!(hole, Hole::Parameter(..))) {
        UnresolvedCause::Parameter
    } else if pattern.text().is_some() {
        UnresolvedCause::NoMatchingFile
    } else if matches!(pattern.0.first(), Some(Part::Hole(_))) {
        UnresolvedCause::TooWide
    } else {
        UnresolvedCause::RuntimeValue
    }
}

fn unresolved(reading: &Reading, cause: UnresolvedCause, links: &mut Links) {
    links.unresolved.push(UnresolvedImport {
        module: reading.site_module.clone(),
        line: reading.site.line,
        expression: reading.site.expression.clone(),
        cause,
    });
}

fn inference(
    reading: &Reading,
    kind: &'static str,
    pattern: &str,
    detail: Option<String>,
) -> Inference {
    Inference {
        line: reading.site.line,
        kind,
        pattern: pattern.to_owned(),
        detail,
    }
}

fn is_identifier(segment: &str) -> bool {
    let mut characters = segment.chars();

    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_alphabetic())
        && characters.all(|character| character == '_' || character.is_alphanumeric())
}

fn escape_glob(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());

    for character in text.chars() {
        if GLOB_CHARACTERS.contains(&character) || matches!(character, ']' | '}' | '\\') {
            escaped.push('\\');
        }

        escaped.push(character);
    }

    escaped
}

fn directories(repo_files: &[RepoPath]) -> HashSet<String> {
    let mut directories = HashSet::new();

    for file in repo_files {
        let mut directory = parent_directory(file.as_str());

        while !directory.is_empty() && directories.insert(directory.to_owned()) {
            directory = parent_directory(directory);
        }
    }

    directories
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(text: &str) -> Part {
        Part::Text(text.to_owned())
    }

    #[test]
    fn relative_module_names_resolve_against_the_package() {
        let pattern = Pattern(vec![text(".platforms."), Part::Hole(Hole::Runtime)]);

        assert_eq!(
            absolute_module(&pattern, Some("app.plugins")),
            Pattern(vec![
                text("app.plugins.platforms."),
                Part::Hole(Hole::Runtime)
            ])
        );
        assert_eq!(
            absolute_module(&Pattern(vec![text("..core")]), Some("app.plugins")),
            Pattern(vec![text("app.core")])
        );
    }

    #[test]
    fn dotted_globs_need_a_literal_package() {
        let pattern = Pattern(vec![text("app.plugins."), Part::Hole(Hole::Runtime)]);

        assert_eq!(
            dotted_glob(&pattern),
            Some(("app.plugins.*".to_owned(), true))
        );
        assert_eq!(
            dotted_glob(&Pattern(vec![Part::Hole(Hole::Runtime), text(".tasks")])),
            None
        );
    }
}
