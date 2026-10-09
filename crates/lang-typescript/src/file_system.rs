//! The file system the resolver sees: the working tree, base revision contents of changed configs, workspace packages linked under node_modules, and build outputs mapped to their sources.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use oxc_resolver::{FileMetadata, FileSystem, FileSystemOs, ResolveError};
use serde_json::Value;

use crate::build_mapping::BuildMapping;
use crate::manifests::documents::parse_jsonc;
use crate::resolver::package_name;

#[derive(Debug, Default)]
pub(crate) struct RepoFileSystem {
    overlay: HashMap<PathBuf, Option<Vec<u8>>>,
    links: HashMap<PathBuf, PathBuf>,
    link_directories: HashSet<PathBuf>,
    links_root: PathBuf,
    repo_root: PathBuf,
    build_mapping: Option<Arc<BuildMapping>>,
}

enum Entry<'f> {
    Overlay(Option<&'f [u8]>),
    Link(&'f Path),
    LinkDirectory,
    BuildOutput,
    Disk(Cow<'f, Path>),
}

impl RepoFileSystem {
    pub fn at(repo_root: &Path) -> Self {
        Self {
            links_root: repo_root.join("node_modules"),
            repo_root: repo_root.to_path_buf(),
            ..Self::default()
        }
    }

    pub fn with_build_mapping(self, build_mapping: Option<Arc<BuildMapping>>) -> Self {
        Self {
            build_mapping,
            ..self
        }
    }

    pub fn with_links(mut self, links: impl IntoIterator<Item = (String, PathBuf)>) -> Self {
        for (package_name, target) in links {
            let link = self.links_root.join(&package_name);

            if fs::symlink_metadata(&link).is_ok() || self.links.contains_key(&link) {
                continue;
            }

            let missing_directories = link
                .ancestors()
                .skip(1)
                .take_while(|directory| directory.starts_with(&self.links_root))
                .filter(|directory| fs::symlink_metadata(directory).is_err())
                .map(Path::to_path_buf)
                .collect::<Vec<_>>();

            self.link_directories.extend(missing_directories);
            self.links.insert(link, target);
        }

        self
    }

    pub fn with_overlay(
        mut self,
        overlay: impl IntoIterator<Item = (PathBuf, Option<Vec<u8>>)>,
    ) -> Self {
        self.overlay.extend(overlay);

        self
    }

    fn entry<'e>(&'e self, path: &'e Path) -> Entry<'e> {
        let path = self.follow_links(path);

        if let Some(content) = self.overlay_content(&path) {
            return Entry::Overlay(content);
        }

        if let Some(target) = self.links.get(path.as_ref()) {
            return Entry::Link(target);
        }

        if self.link_directories.contains(path.as_ref()) {
            return Entry::LinkDirectory;
        }

        if self.is_build_output(&path) {
            return Entry::BuildOutput;
        }

        Entry::Disk(path)
    }

    fn is_build_output(&self, path: &Path) -> bool {
        let Some(build_mapping) = &self.build_mapping else {
            return false;
        };

        path.strip_prefix(&self.repo_root)
            .ok()
            .and_then(Path::to_str)
            .is_some_and(|relative_path| build_mapping.is_output(relative_path))
    }

    fn overlay_content(&self, path: &Path) -> Option<Option<&[u8]>> {
        if self.overlay.is_empty() {
            return None;
        }

        if let Some(content) = self.overlay.get(path) {
            return Some(content.as_deref());
        }

        let is_installed_path = path
            .components()
            .any(|component| component.as_os_str() == "node_modules");
        let real_path = is_installed_path
            .then(|| {
                Some(
                    fs::canonicalize(path.parent()?)
                        .ok()?
                        .join(path.file_name()?),
                )
            })
            .flatten()?;

        self.overlay.get(&real_path).map(Option::as_deref)
    }

    fn without_missing_bases(&self, tsconfig_path: &Path, bytes: Vec<u8>) -> Vec<u8> {
        let Some(Value::Object(mut fields)) = std::str::from_utf8(&bytes)
            .ok()
            .filter(|text| text.contains("\"extends\""))
            .and_then(|text| parse_jsonc(text).ok())
        else {
            return bytes;
        };
        let directory = tsconfig_path.parent().unwrap_or(tsconfig_path);
        let available_bases = match fields.get("extends") {
            Some(Value::String(base)) if !self.is_available_base(directory, base) => None,
            Some(Value::Array(bases)) => {
                let available_bases: Vec<Value> = bases
                    .iter()
                    .filter(|base| {
                        base.as_str()
                            .is_none_or(|base| self.is_available_base(directory, base))
                    })
                    .cloned()
                    .collect();

                if available_bases.len() == bases.len() {
                    return bytes;
                }

                Some(Value::Array(available_bases))
            }
            _ => return bytes,
        };

        match available_bases {
            Some(available_bases) => fields.insert("extends".to_owned(), available_bases),
            None => fields.remove("extends"),
        };

        serde_json::to_vec(&fields).unwrap_or(bytes)
    }

    fn is_available_base(&self, directory: &Path, specifier: &str) -> bool {
        let Some(package_name) = package_name(specifier) else {
            return true;
        };

        directory.ancestors().any(|ancestor| {
            let installed_package = ancestor.join("node_modules").join(package_name);

            self.links.contains_key(&installed_package) || fs::metadata(&installed_package).is_ok()
        })
    }

    fn follow_links<'p>(&self, path: &'p Path) -> Cow<'p, Path> {
        if self.links.is_empty() || !path.starts_with(&self.links_root) {
            return Cow::Borrowed(path);
        }

        let linked_ancestor = path
            .ancestors()
            .skip(1)
            .take_while(|ancestor| ancestor.starts_with(&self.links_root))
            .find_map(|ancestor| Some((ancestor, self.links.get(ancestor)?)));

        match linked_ancestor {
            Some((link, target)) => {
                let path_in_target = path.strip_prefix(link).expect("ancestor of the path");

                Cow::Owned(target.join(path_in_target))
            }
            None => Cow::Borrowed(path),
        }
    }
}

fn is_repo_tsconfig(path: &Path) -> bool {
    let is_tsconfig = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            (name.starts_with("tsconfig") || name.starts_with("jsconfig"))
                && name.ends_with(".json")
        });

    is_tsconfig
        && !path
            .components()
            .any(|component| component.as_os_str() == "node_modules")
}

fn not_found() -> io::Error {
    io::Error::from(io::ErrorKind::NotFound)
}

fn directory_read() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "is a directory")
}

impl FileSystem for RepoFileSystem {
    fn new() -> Self {
        Self::default()
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let bytes = match self.entry(path) {
            Entry::Overlay(Some(content)) => content.to_vec(),
            Entry::Overlay(None) => return Err(not_found()),
            Entry::BuildOutput => Vec::new(),
            Entry::Link(_) | Entry::LinkDirectory => return Err(directory_read()),
            Entry::Disk(path) => fs::read(path)?,
        };

        if is_repo_tsconfig(path) {
            return Ok(self.without_missing_bases(path, bytes));
        }

        Ok(bytes)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        FileSystemOs::validate_string(self.read(path)?)
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        match self.entry(path) {
            Entry::Overlay(Some(_)) | Entry::BuildOutput => {
                Ok(FileMetadata::new(true, false, false))
            }
            Entry::Overlay(None) => Err(not_found()),
            Entry::Link(target) => FileSystemOs::metadata(target),
            Entry::LinkDirectory => Ok(FileMetadata::new(false, true, false)),
            Entry::Disk(path) => FileSystemOs::metadata(&path),
        }
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        match self.entry(path) {
            Entry::Overlay(Some(_)) | Entry::BuildOutput => {
                Ok(FileMetadata::new(true, false, false))
            }
            Entry::Overlay(None) => Err(not_found()),
            Entry::Link(_) => Ok(FileMetadata::new(false, false, true)),
            Entry::LinkDirectory => Ok(FileMetadata::new(false, true, false)),
            Entry::Disk(path) => FileSystemOs::symlink_metadata(&path),
        }
    }

    fn read_link(&self, path: &Path) -> Result<PathBuf, ResolveError> {
        match self.entry(path) {
            Entry::Link(target) => Ok(target.to_path_buf()),
            Entry::Disk(path) => FileSystemOs::read_link(&path),
            Entry::Overlay(_) | Entry::LinkDirectory | Entry::BuildOutput => {
                Err(io::Error::from(io::ErrorKind::InvalidInput).into())
            }
        }
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        match self.entry(path) {
            Entry::Link(target) => fs::canonicalize(target),
            Entry::Disk(path) => fs::canonicalize(path),
            Entry::Overlay(Some(_)) | Entry::LinkDirectory | Entry::BuildOutput => {
                fs::canonicalize(path).or_else(|_| Ok(path.to_path_buf()))
            }
            Entry::Overlay(None) => Err(not_found()),
        }
    }
}
