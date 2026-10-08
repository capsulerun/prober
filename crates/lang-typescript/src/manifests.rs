//! Manifest, lockfile and tsconfig changes diffed field by field: changed packages and their dependents, resolution changes, and fields that affect every test.

pub(crate) mod documents;
mod fields;
mod lockfiles;

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use prober_core::changes::{BaseRevision, ChangeKind, FileChange};
use prober_core::files::RepoPath;
use prober_core::report::ModeReason;
use serde_json::{Map, Value};

use crate::conventions::is_module_file;
use crate::manifests::documents::{parse_json, parse_jsonc, parse_yaml};
use crate::manifests::fields::{FieldEffect, FieldTable, PackageField};
use crate::manifests::lockfiles::{
    LockedEntry, LockedPackages, LockfileKind, split_name_and_version,
};
use crate::workspaces::{
    PACKAGE_MANIFEST, PNPM_WORKSPACE, discover_workspace, is_workspace_manifest, parent_directory,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackageChange {
    pub name: String,
    pub change: FileChange,
    pub detail: String,
    pub through: Option<String>,
    changed_versions: Option<BTreeSet<String>>,
}

impl PackageChange {
    pub fn description(&self) -> String {
        match &self.through {
            None => format!("package {} changed ({})", self.name, self.detail),
            Some(dependency) => format!(
                "package {} depends on {dependency}, which changed ({})",
                self.name, self.detail
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AffectedFile {
    pub change_path: RepoPath,
    pub file: RepoPath,
    pub field: String,
}

#[derive(Debug, Default)]
pub struct ManifestChanges {
    global_reasons: Vec<ModeReason>,
    packages: Vec<PackageChange>,
    affected_files: Vec<AffectedFile>,
    resolution_change: Option<RepoPath>,
    base_configs: Vec<(RepoPath, Option<Vec<u8>>)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManifestKind {
    Document(FieldTable),
    Lockfile(LockfileKind),
    Patch,
    Vendored,
}

impl ManifestChanges {
    pub fn between(
        repo_root: &Path,
        repo_files: &[RepoPath],
        changes: &[FileChange],
        base: &dyn BaseRevision,
    ) -> Result<Self> {
        let read_head = |path: &RepoPath| fs::read_to_string(repo_root.join(path.as_str())).ok();
        let workspace = discover_workspace(
            repo_files.iter().filter(|path| is_workspace_manifest(path)),
            &read_head,
        );
        let lockfile_directories = directories_with_lockfiles(repo_files);
        let changed_lockfile_directories = directories_with_lockfiles(
            changes
                .iter()
                .flat_map(FileChange::touched_paths)
                .filter(|path| !is_vendored(path)),
        );
        let mut diff = ManifestDiff {
            repo_root,
            repo_files,
            base,
            workspace_package_names: workspace.packages.into_keys().collect(),
            member_directories: workspace.member_directories,
            lockfile_directories,
            changed_lockfile_directories,
            changes: ManifestChanges::default(),
            head_lockfiles: BTreeMap::new(),
        };

        for change in changes {
            diff.diff_change(change)?;
        }

        diff.finish()
    }

    pub fn global_reasons(&self) -> &[ModeReason] {
        &self.global_reasons
    }

    pub(crate) fn packages(&self) -> &[PackageChange] {
        &self.packages
    }

    pub(crate) fn affected_files(&self) -> &[AffectedFile] {
        &self.affected_files
    }

    pub(crate) fn resolution_change(&self) -> Option<&RepoPath> {
        self.resolution_change.as_ref()
    }

    pub(crate) fn base_configs(&self) -> &[(RepoPath, Option<Vec<u8>>)] {
        &self.base_configs
    }
}

struct ManifestDiff<'d> {
    repo_root: &'d Path,
    repo_files: &'d [RepoPath],
    base: &'d dyn BaseRevision,
    workspace_package_names: BTreeSet<String>,
    member_directories: BTreeSet<String>,
    lockfile_directories: BTreeSet<String>,
    changed_lockfile_directories: BTreeSet<String>,
    changes: ManifestChanges,
    head_lockfiles: BTreeMap<RepoPath, LockedPackages>,
}

impl ManifestDiff<'_> {
    fn diff_change(&mut self, change: &FileChange) -> Result<()> {
        let Some(kind) = change.touched_paths().find_map(manifest_kind) else {
            return Ok(());
        };

        match kind {
            ManifestKind::Patch => {
                self.add_direct_package(change, patched_package(&change.path), "patched")
            }
            ManifestKind::Vendored => self.add_direct_package(
                change,
                vendored_package(&change.path),
                "vendored files changed",
            ),
            ManifestKind::Document(table) => self.diff_document(change, table)?,
            ManifestKind::Lockfile(lockfile_kind) => self.diff_lockfile(change, lockfile_kind)?,
        }

        Ok(())
    }

    fn add_direct_package(&mut self, change: &FileChange, name: Option<String>, detail: &str) {
        self.add_changed_versions(change, name, detail, None);
    }

    fn add_changed_versions(
        &mut self,
        change: &FileChange,
        name: Option<String>,
        detail: &str,
        changed_versions: Option<BTreeSet<String>>,
    ) {
        if let Some(name) = name.filter(|name| !self.workspace_package_names.contains(name)) {
            self.changes.packages.push(PackageChange {
                name,
                change: change.clone(),
                detail: detail.to_owned(),
                through: None,
                changed_versions,
            });
        }
    }

    fn diff_document(&mut self, change: &FileChange, table: FieldTable) -> Result<()> {
        let base_bytes = self.base_bytes(change)?;
        let head_bytes = self.head_bytes(change)?;
        let base_path = base_path(change);

        self.changes
            .base_configs
            .push((base_path.clone(), base_bytes.clone()));

        if base_path != change.path {
            self.changes.base_configs.push((change.path.clone(), None));
        }

        let parse = match table {
            FieldTable::Tsconfig => parse_jsonc,
            FieldTable::PnpmSettings => parse_yaml,
            FieldTable::PackageJson | FieldTable::CompilerOptions => parse_json,
        };
        let documents = document_fields(base_bytes.as_deref(), parse)
            .and_then(|base| Ok((base, document_fields(head_bytes.as_deref(), parse)?)));

        match documents {
            Ok((base_fields, head_fields)) => {
                self.diff_fields(change, table, "", &base_fields, &head_fields);
            }
            Err(error) => self
                .changes
                .global_reasons
                .push(ModeReason::UnparsableManifest {
                    change: change.clone(),
                    message: error.to_string(),
                }),
        }

        Ok(())
    }

    fn diff_fields(
        &mut self,
        change: &FileChange,
        table: FieldTable,
        prefix: &str,
        base_fields: &Map<String, Value>,
        head_fields: &Map<String, Value>,
    ) {
        let keys: BTreeSet<&String> = base_fields.keys().chain(head_fields.keys()).collect();

        for key in keys {
            let base_value = base_fields.get(key);
            let head_value = head_fields.get(key);

            if base_value == head_value {
                continue;
            }

            let field = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };

            match table.effect(key) {
                FieldEffect::NoTest => {}
                FieldEffect::Packages(_) if !self.counts_dependency_fields(change) => {}
                FieldEffect::Packages(package_field) => {
                    match changed_package_names(package_field, &field, base_value, head_value) {
                        Some(changed_names) => {
                            for (name, detail) in changed_names {
                                self.add_direct_package(change, Some(name), &detail);
                            }
                        }
                        None => self.add_global_field(change, field),
                    }
                }
                FieldEffect::Resolution => {
                    self.changes
                        .resolution_change
                        .get_or_insert_with(|| change.path.clone());
                }
                FieldEffect::PackageScope => self.affect_package_files(change, &field),
                FieldEffect::Entries => self.affect_entries(change, &field, base_value, head_value),
                FieldEffect::Nested(nested_table) => {
                    let empty_fields = Map::new();
                    let nested_fields = (
                        optional_object(base_value, &empty_fields),
                        optional_object(head_value, &empty_fields),
                    );

                    match nested_fields {
                        (Some(base_nested), Some(head_nested)) => {
                            self.diff_fields(
                                change,
                                nested_table,
                                &field,
                                base_nested,
                                head_nested,
                            );
                        }
                        _ => self.add_global_field(change, field),
                    }
                }
                FieldEffect::Global => self.add_global_field(change, field),
            }
        }
    }

    fn counts_dependency_fields(&self, change: &FileChange) -> bool {
        let directory = parent_directory(&change.path);
        let is_installed = file_name(&change.path) != PACKAGE_MANIFEST
            || self.member_directories.contains(directory)
            || self.lockfile_directories.contains(directory);
        let covering_lockfile = ancestor_directories(directory)
            .find(|ancestor| self.lockfile_directories.contains(*ancestor));

        is_installed
            && covering_lockfile.is_none_or(|lockfile_directory| {
                !self
                    .changed_lockfile_directories
                    .contains(lockfile_directory)
            })
    }

    fn add_global_field(&mut self, change: &FileChange, field: String) {
        self.changes.global_reasons.push(ModeReason::GlobalField {
            change: change.clone(),
            field,
        });
    }

    fn affect_package_files(&mut self, change: &FileChange, field: &str) {
        let package_directory = parent_directory(&change.path);
        let package_directories: BTreeSet<&str> = self
            .repo_files
            .iter()
            .filter(|path| file_name(path) == PACKAGE_MANIFEST)
            .map(parent_directory)
            .collect();
        let package_files = self.repo_files.iter().filter(|path| {
            is_module_file(path)
                && owning_package(path, &package_directories) == Some(package_directory)
        });
        let affected_files: Vec<AffectedFile> = package_files
            .map(|file| AffectedFile {
                change_path: change.path.clone(),
                file: file.clone(),
                field: field.to_owned(),
            })
            .collect();

        self.changes.affected_files.extend(affected_files);
    }

    fn affect_entries(
        &mut self,
        change: &FileChange,
        field: &str,
        base_value: Option<&Value>,
        head_value: Option<&Value>,
    ) {
        let package_directory = parent_directory(&change.path);
        let base_entries = entry_paths(base_value);
        let head_entries = entry_paths(head_value);
        let entry_names: BTreeSet<&String> =
            base_entries.keys().chain(head_entries.keys()).collect();

        for entry_name in entry_names {
            let base_entry = base_entries.get(entry_name);
            let head_entry = head_entries.get(entry_name);

            if base_entry == head_entry {
                continue;
            }

            for entry_path in base_entry.into_iter().chain(head_entry) {
                if let Some(file) = join_package_path(package_directory, entry_path) {
                    self.changes.affected_files.push(AffectedFile {
                        change_path: change.path.clone(),
                        file,
                        field: field.to_owned(),
                    });
                }
            }
        }
    }

    fn diff_lockfile(&mut self, change: &FileChange, kind: LockfileKind) -> Result<()> {
        let base_bytes = self.base_bytes(change)?;
        let head_bytes = self.head_bytes(change)?;
        let lockfiles = locked_packages(kind, base_bytes.as_deref())
            .and_then(|base| Ok((base, locked_packages(kind, head_bytes.as_deref())?)));

        match lockfiles {
            Ok((base_lockfile, head_lockfile)) => {
                for changed_package in
                    LockedPackages::changed_packages(&base_lockfile, &head_lockfile)
                {
                    self.add_changed_versions(
                        change,
                        Some(changed_package.name),
                        &changed_package.detail,
                        Some(changed_package.changed_versions),
                    );
                }

                self.head_lockfiles
                    .insert(change.path.clone(), head_lockfile);
            }
            Err(error) => self
                .changes
                .global_reasons
                .push(ModeReason::UnparsableManifest {
                    change: change.clone(),
                    message: error.to_string(),
                }),
        }

        Ok(())
    }

    fn base_bytes(&self, change: &FileChange) -> Result<Option<Vec<u8>>> {
        if change.kind == ChangeKind::Added {
            return Ok(None);
        }

        let path = base_path(change);

        self.base
            .read_file(&path)
            .with_context(|| format!("cannot read {path} at the base revision"))
    }

    fn head_bytes(&self, change: &FileChange) -> Result<Option<Vec<u8>>> {
        if change.kind == ChangeKind::Deleted {
            return Ok(None);
        }

        let bytes = fs::read(self.repo_root.join(change.path.as_str()))
            .with_context(|| format!("cannot read {}", change.path))?;

        Ok(Some(bytes))
    }

    fn finish(mut self) -> Result<ManifestChanges> {
        if self.changes.packages.is_empty() {
            return Ok(self.changes);
        }

        let unread_lockfiles: Vec<(RepoPath, LockfileKind)> = self
            .repo_files
            .iter()
            .filter(|path| !self.head_lockfiles.contains_key(*path) && !is_vendored(path))
            .filter_map(|path| Some((path.clone(), LockfileKind::of(file_name(path))?)))
            .collect();

        for (path, kind) in unread_lockfiles {
            let text = fs::read_to_string(self.repo_root.join(path.as_str()))
                .with_context(|| format!("cannot read {path}"))?;

            if let Ok(head_lockfile) = LockedPackages::parse(kind, &text) {
                self.head_lockfiles.insert(path, head_lockfile);
            }
        }

        let mut dependents: HashMap<&LockedEntry, BTreeSet<&LockedEntry>> = HashMap::new();

        for (dependent, dependency) in self
            .head_lockfiles
            .values()
            .flat_map(LockedPackages::dependency_edges)
        {
            dependents.entry(dependency).or_default().insert(dependent);
        }

        let mut direct_packages = self.changes.packages.clone();

        direct_packages.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then(left.change.path.cmp(&right.change.path))
        });

        let mut reached_names: BTreeSet<&str> = direct_packages
            .iter()
            .map(|package| package.name.as_str())
            .collect();
        let mut entries_to_visit: VecDeque<(LockedEntry, &PackageChange)> = VecDeque::new();

        for package in &direct_packages {
            let changed_entries: Vec<LockedEntry> = match &package.changed_versions {
                Some(changed_versions) => changed_versions
                    .iter()
                    .map(|version| LockedEntry {
                        name: package.name.clone(),
                        version: version.clone(),
                    })
                    .collect(),
                None => self
                    .head_lockfiles
                    .values()
                    .flat_map(|lockfile| lockfile.entries_named(&package.name))
                    .collect(),
            };

            entries_to_visit.extend(changed_entries.into_iter().map(|entry| (entry, package)));
        }

        let mut visited_entries: BTreeSet<LockedEntry> = entries_to_visit
            .iter()
            .map(|(entry, _)| entry.clone())
            .collect();
        let mut dependent_packages = Vec::new();

        while let Some((entry, origin)) = entries_to_visit.pop_front() {
            for &dependent in dependents.get(&entry).into_iter().flatten() {
                if !visited_entries.insert(dependent.clone()) {
                    continue;
                }

                if reached_names.insert(&dependent.name) {
                    dependent_packages.push(PackageChange {
                        name: dependent.name.clone(),
                        change: origin.change.clone(),
                        detail: origin.detail.clone(),
                        through: Some(origin.name.clone()),
                        changed_versions: None,
                    });
                }

                entries_to_visit.push_back((dependent.clone(), origin));
            }
        }

        self.changes.packages = direct_packages;
        self.changes.packages.extend(dependent_packages);

        Ok(self.changes)
    }
}

fn directories_with_lockfiles<'p>(
    paths: impl IntoIterator<Item = &'p RepoPath>,
) -> BTreeSet<String> {
    paths
        .into_iter()
        .filter(|path| LockfileKind::of(file_name(path)).is_some() && !is_vendored(path))
        .map(|path| parent_directory(path).to_owned())
        .collect()
}

fn ancestor_directories(directory: &str) -> impl Iterator<Item = &str> {
    std::iter::successors(Some(directory), |directory| {
        (!directory.is_empty()).then(|| directory.rsplit_once('/').map_or("", |(parent, _)| parent))
    })
}

fn manifest_kind(path: &RepoPath) -> Option<ManifestKind> {
    let file_name = file_name(path);

    if is_vendored(path) {
        return vendored_package(path).map(|_| ManifestKind::Vendored);
    }

    if file_name == PACKAGE_MANIFEST {
        return Some(ManifestKind::Document(FieldTable::PackageJson));
    }

    if file_name == PNPM_WORKSPACE {
        return Some(ManifestKind::Document(FieldTable::PnpmSettings));
    }

    if (file_name.starts_with("tsconfig") || file_name.starts_with("jsconfig"))
        && file_name.ends_with(".json")
    {
        return Some(ManifestKind::Document(FieldTable::Tsconfig));
    }

    if let Some(lockfile_kind) = LockfileKind::of(file_name) {
        return Some(ManifestKind::Lockfile(lockfile_kind));
    }

    let is_patch = file_name.ends_with(".patch")
        && parent_directory(path).rsplit('/').next() == Some("patches");

    is_patch.then_some(ManifestKind::Patch)
}

fn is_vendored(path: &RepoPath) -> bool {
    path.as_str()
        .split('/')
        .any(|segment| segment == "node_modules")
}

fn vendored_package(path: &RepoPath) -> Option<String> {
    let (_, installed_path) = path.as_str().rsplit_once("node_modules/")?;
    let mut segments = installed_path.split('/');
    let first_segment = segments.next()?;
    let name = if first_segment.starts_with('@') {
        format!("{first_segment}/{}", segments.next()?)
    } else {
        first_segment.to_owned()
    };

    segments.next().map(|_| name)
}

fn patched_package(path: &RepoPath) -> Option<String> {
    let stem = file_name(path).strip_suffix(".patch")?;

    if let Some((name, _)) = split_name_and_version(stem) {
        return Some(name.replace("__", "/"));
    }

    let patched = stem.rsplit("++").next()?;
    let mut parts = patched.split('+');
    let first_part = parts.next()?;
    let name = if first_part.starts_with('@') {
        format!("{first_part}/{}", parts.next()?)
    } else {
        first_part.to_owned()
    };

    (!name.is_empty()).then_some(name)
}

fn base_path(change: &FileChange) -> RepoPath {
    match &change.kind {
        ChangeKind::Renamed { from } => from.clone(),
        ChangeKind::Added | ChangeKind::Modified | ChangeKind::Deleted => change.path.clone(),
    }
}

fn document_fields(
    bytes: Option<&[u8]>,
    parse: fn(&str) -> Result<Value>,
) -> Result<Map<String, Value>> {
    let Some(bytes) = bytes else {
        return Ok(Map::new());
    };
    let text = std::str::from_utf8(bytes).map_err(|_| anyhow!("not valid UTF-8"))?;

    match parse(text)? {
        Value::Object(fields) => Ok(fields),
        Value::Null => Ok(Map::new()),
        _ => bail!("expected an object at the top level"),
    }
}

fn locked_packages(kind: LockfileKind, bytes: Option<&[u8]>) -> Result<LockedPackages> {
    let Some(bytes) = bytes else {
        return Ok(LockedPackages::default());
    };
    let text = std::str::from_utf8(bytes).map_err(|_| anyhow!("not valid UTF-8"))?;

    LockedPackages::parse(kind, text)
}

fn optional_object<'v>(
    value: Option<&'v Value>,
    empty: &'v Map<String, Value>,
) -> Option<&'v Map<String, Value>> {
    match value {
        None => Some(empty),
        Some(value) => value.as_object(),
    }
}

fn changed_package_names(
    package_field: PackageField,
    field: &str,
    base_value: Option<&Value>,
    head_value: Option<&Value>,
) -> Option<Vec<(String, String)>> {
    let empty_map = Map::new();

    match package_field {
        PackageField::NameMap => {
            let base_map = optional_object(base_value, &empty_map)?;
            let head_map = optional_object(head_value, &empty_map)?;

            Some(
                changed_keys(base_map, head_map)
                    .into_iter()
                    .map(|name| {
                        let detail =
                            describe_value_change(base_map.get(name), head_map.get(name), field);

                        (name.to_owned(), detail)
                    })
                    .collect(),
            )
        }
        PackageField::NameMaps => {
            let base_maps = optional_object(base_value, &empty_map)?;
            let head_maps = optional_object(head_value, &empty_map)?;
            let mut changed_names = Vec::new();

            for map_name in changed_keys(base_maps, head_maps) {
                changed_names.extend(changed_package_names(
                    PackageField::NameMap,
                    field,
                    base_maps.get(map_name),
                    head_maps.get(map_name),
                )?);
            }

            Some(changed_names)
        }
        PackageField::SelectorMap => {
            let base_map = optional_object(base_value, &empty_map)?;
            let head_map = optional_object(head_value, &empty_map)?;
            let mut changed_names = BTreeSet::new();

            for selector in changed_keys(base_map, head_map) {
                changed_names.extend(selector_package_name(selector));

                for nested_value in [base_map.get(selector), head_map.get(selector)]
                    .into_iter()
                    .flatten()
                {
                    collect_nested_selector_names(nested_value, &mut changed_names);
                }
            }

            Some(
                changed_names
                    .into_iter()
                    .map(|name| (name, format!("{field} changed")))
                    .collect(),
            )
        }
        PackageField::NameList => {
            let base_names = optional_names(base_value)?;
            let head_names = optional_names(head_value)?;

            Some(
                base_names
                    .symmetric_difference(&head_names)
                    .map(|name| ((*name).to_owned(), format!("{field} changed")))
                    .collect(),
            )
        }
    }
}

fn changed_keys<'m>(
    base_map: &'m Map<String, Value>,
    head_map: &'m Map<String, Value>,
) -> BTreeSet<&'m str> {
    base_map
        .keys()
        .chain(head_map.keys())
        .filter(|key| base_map.get(*key) != head_map.get(*key))
        .map(String::as_str)
        .collect()
}

fn optional_names(value: Option<&Value>) -> Option<BTreeSet<&str>> {
    match value {
        None => Some(BTreeSet::new()),
        Some(Value::Array(items)) => items.iter().map(Value::as_str).collect(),
        Some(_) => None,
    }
}

fn collect_nested_selector_names(value: &Value, names: &mut BTreeSet<String>) {
    let Value::Object(nested_selectors) = value else {
        return;
    };

    for (selector, nested_value) in nested_selectors {
        if selector != "." {
            names.extend(selector_package_name(selector));
        }

        collect_nested_selector_names(nested_value, names);
    }
}

fn selector_package_name(selector: &str) -> Option<String> {
    let target = selector.rsplit('>').next()?;
    let segments: Vec<&str> = target.split('/').collect();
    let last_package = match segments.as_slice() {
        [.., scope, name] if scope.starts_with('@') => format!("{scope}/{name}"),
        [.., name] => (*name).to_owned(),
        [] => return None,
    };
    let name =
        split_name_and_version(&last_package).map_or(last_package.as_str(), |(name, _)| name);

    (!name.is_empty() && name != "**" && name != "*").then(|| name.to_owned())
}

fn describe_value_change(
    base_value: Option<&Value>,
    head_value: Option<&Value>,
    field: &str,
) -> String {
    match (
        base_value.and_then(Value::as_str),
        head_value.and_then(Value::as_str),
    ) {
        (Some(base_spec), Some(head_spec)) => format!("{base_spec} -> {head_spec}"),
        (None, Some(head_spec)) if base_value.is_none() => format!("added {head_spec}"),
        (Some(base_spec), None) if head_value.is_none() => format!("removed {base_spec}"),
        _ => format!("{field} changed"),
    }
}

fn entry_paths(value: Option<&Value>) -> BTreeMap<String, String> {
    match value {
        Some(Value::String(path)) => BTreeMap::from([(String::new(), path.clone())]),
        Some(Value::Object(entries)) => entries
            .iter()
            .filter_map(|(name, path)| Some((name.clone(), path.as_str()?.to_owned())))
            .collect(),
        _ => BTreeMap::new(),
    }
}

fn join_package_path(package_directory: &str, relative_path: &str) -> Option<RepoPath> {
    let mut segments: Vec<&str> = package_directory
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();

    for segment in relative_path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            segment => segments.push(segment),
        }
    }

    RepoPath::new(segments.join("/")).ok()
}

fn owning_package<'d>(path: &RepoPath, package_directories: &BTreeSet<&'d str>) -> Option<&'d str> {
    ancestor_directories(parent_directory(path))
        .find_map(|directory| package_directories.get(directory).copied())
}

fn file_name(path: &RepoPath) -> &str {
    path.as_str()
        .rsplit_once('/')
        .map_or(path.as_str(), |(_, file_name)| file_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_lockfiles_patches_and_vendored_packages_are_recognised_by_path() {
        let kinds = [
            (
                "package.json",
                Some(ManifestKind::Document(FieldTable::PackageJson)),
            ),
            (
                "packages/lib/package.json",
                Some(ManifestKind::Document(FieldTable::PackageJson)),
            ),
            (
                "pnpm-workspace.yaml",
                Some(ManifestKind::Document(FieldTable::PnpmSettings)),
            ),
            (
                "tsconfig.build.json",
                Some(ManifestKind::Document(FieldTable::Tsconfig)),
            ),
            (
                "web/jsconfig.json",
                Some(ManifestKind::Document(FieldTable::Tsconfig)),
            ),
            (
                "pnpm-lock.yaml",
                Some(ManifestKind::Lockfile(LockfileKind::Pnpm)),
            ),
            (
                "npm-shrinkwrap.json",
                Some(ManifestKind::Lockfile(LockfileKind::Npm)),
            ),
            ("patches/lodash@4.17.21.patch", Some(ManifestKind::Patch)),
            ("node_modules/lodash/index.js", Some(ManifestKind::Vendored)),
            (
                "node_modules/lodash/package.json",
                Some(ManifestKind::Vendored),
            ),
            ("docs/patches.md", None),
            ("fixes/lodash.patch", None),
            ("tsconfig.ts", None),
        ];

        for (path, expected_kind) in kinds {
            assert_eq!(
                manifest_kind(&RepoPath::new(path).unwrap()),
                expected_kind,
                "{path}"
            );
        }
    }

    #[test]
    fn patch_files_name_their_package_in_pnpm_and_patch_package_styles() {
        let patches = [
            ("patches/lodash@4.17.21.patch", Some("lodash")),
            ("patches/@vue__shared@3.5.0.patch", Some("@vue/shared")),
            ("patches/lodash+4.17.21.patch", Some("lodash")),
            ("patches/@vue+shared+3.5.0.patch", Some("@vue/shared")),
            ("patches/react-native++metro+0.80.0.patch", Some("metro")),
            ("patches/lodash.patch", Some("lodash")),
        ];

        for (path, expected_name) in patches {
            assert_eq!(
                patched_package(&RepoPath::new(path).unwrap()).as_deref(),
                expected_name,
                "{path}"
            );
        }
    }

    #[test]
    fn selectors_name_the_package_they_override() {
        let selectors = [
            ("axios", Some("axios")),
            ("lodash@4", Some("lodash")),
            ("mailauth@5.0.3>undici", Some("undici")),
            (
                "@codemirror/commands@6.11.1>@codemirror/view",
                Some("@codemirror/view"),
            ),
            ("**/@vue/shared", Some("@vue/shared")),
            ("webpack/**/lodash", Some("lodash")),
            ("**", None),
        ];

        for (selector, expected_name) in selectors {
            assert_eq!(
                selector_package_name(selector).as_deref(),
                expected_name,
                "{selector}"
            );
        }
    }

    #[test]
    fn package_entries_are_joined_to_the_package_directory() {
        assert_eq!(
            join_package_path("packages/cli", "./bin/cli.js"),
            Some(RepoPath::new("packages/cli/bin/cli.js").unwrap())
        );
        assert_eq!(
            join_package_path("", "cli.mjs"),
            Some(RepoPath::new("cli.mjs").unwrap())
        );
        assert_eq!(join_package_path("", "../outside.js"), None);
    }
}
