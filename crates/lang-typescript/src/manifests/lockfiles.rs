use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Result, bail};
use serde_json::Value;

use crate::manifests::documents::{parse_json, parse_yaml, parse_yaml_documents};

const TYPES_SCOPE: &str = "@types/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockfileKind {
    Pnpm,
    Npm,
    Yarn,
}

impl LockfileKind {
    pub fn of(file_name: &str) -> Option<Self> {
        match file_name {
            "pnpm-lock.yaml" => Some(LockfileKind::Pnpm),
            "package-lock.json" | "npm-shrinkwrap.json" => Some(LockfileKind::Npm),
            "yarn.lock" => Some(LockfileKind::Yarn),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct LockedEntry {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LockedVersion {
    resolution: BTreeSet<String>,
    patches: BTreeSet<String>,
    dependencies: BTreeSet<String>,
}

#[derive(Debug, Default)]
pub(crate) struct LockedPackages {
    versions: BTreeMap<String, BTreeMap<String, LockedVersion>>,
    dependencies: BTreeMap<LockedEntry, BTreeSet<LockedEntry>>,
    unresolved_dependencies: Vec<(LockedEntry, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangedPackage {
    pub name: String,
    pub detail: String,
    pub changed_versions: BTreeSet<String>,
}

impl LockedPackages {
    pub fn parse(kind: LockfileKind, text: &str) -> Result<Self> {
        let mut locked_packages = Self::default();

        match kind {
            LockfileKind::Pnpm => locked_packages.read_pnpm(&parse_yaml_documents(text)?)?,
            LockfileKind::Npm => locked_packages.read_npm(&parse_json(text)?)?,
            LockfileKind::Yarn
                if text.contains("\n__metadata:") || text.starts_with("__metadata:") =>
            {
                locked_packages.read_yarn_berry(&parse_yaml(text)?);
            }
            LockfileKind::Yarn if text.contains("# yarn lockfile v1") => {
                locked_packages.read_yarn_classic(text);
            }
            LockfileKind::Yarn => bail!("unknown yarn.lock format"),
        }

        locked_packages.resolve_by_name();

        Ok(locked_packages)
    }

    pub fn changed_packages(base: &Self, head: &Self) -> Vec<ChangedPackage> {
        let names: BTreeSet<&String> = base.versions.keys().chain(head.versions.keys()).collect();
        let no_versions = BTreeMap::new();

        names
            .into_iter()
            .filter_map(|name| {
                let base_versions = base.versions.get(name).unwrap_or(&no_versions);
                let head_versions = head.versions.get(name).unwrap_or(&no_versions);
                let changed_versions = head_versions
                    .iter()
                    .filter(|(version, head_version)| {
                        base_versions.get(*version) != Some(head_version)
                    })
                    .map(|(version, _)| version.clone())
                    .collect();

                (base_versions != head_versions).then(|| ChangedPackage {
                    name: name.clone(),
                    detail: describe_change(base_versions, head_versions),
                    changed_versions,
                })
            })
            .collect()
    }

    pub fn entries_named<'l>(&'l self, name: &'l str) -> impl Iterator<Item = LockedEntry> + 'l {
        self.versions
            .get(name)
            .into_iter()
            .flat_map(BTreeMap::keys)
            .map(move |version| LockedEntry {
                name: name.to_owned(),
                version: version.clone(),
            })
    }

    pub fn dependency_edges(&self) -> impl Iterator<Item = (&LockedEntry, &LockedEntry)> {
        self.dependencies
            .iter()
            .flat_map(|(dependent, dependencies)| {
                dependencies
                    .iter()
                    .map(move |dependency| (dependent, dependency))
            })
    }

    fn lock(&mut self, entry: &LockedEntry) -> Option<&mut LockedVersion> {
        if entry.name.starts_with(TYPES_SCOPE) {
            return None;
        }

        let locked_version = self
            .versions
            .entry(entry.name.clone())
            .or_default()
            .entry(entry.version.clone())
            .or_default();

        Some(locked_version)
    }

    fn depend(&mut self, dependent: &LockedEntry, dependency: LockedEntry) {
        if dependent.name.starts_with(TYPES_SCOPE) || dependency.name.starts_with(TYPES_SCOPE) {
            return;
        }

        self.dependencies
            .entry(dependent.clone())
            .or_default()
            .insert(dependency);
    }

    fn depend_by_name(&mut self, dependent: &LockedEntry, dependency_name: &str) {
        self.unresolved_dependencies
            .push((dependent.clone(), dependency_name.to_owned()));
    }

    fn resolve_by_name(&mut self) {
        for (dependent, dependency_name) in std::mem::take(&mut self.unresolved_dependencies) {
            let dependencies: Vec<LockedEntry> = self.entries_named(&dependency_name).collect();

            for dependency in dependencies {
                self.depend(&dependent, dependency);
            }
        }
    }

    fn read_pnpm(&mut self, documents: &[Value]) -> Result<()> {
        let mut has_lockfile_version = false;

        for document in documents {
            let lockfile_version = match document.get("lockfileVersion") {
                Some(Value::String(version)) => version.clone(),
                Some(Value::Number(version)) => version.to_string(),
                _ => continue,
            };
            let major_version = lockfile_version
                .split('.')
                .next()
                .and_then(|major| major.parse::<u32>().ok());

            if major_version.is_none_or(|major| major < 6) {
                bail!("lockfileVersion {lockfile_version} is not supported");
            }

            has_lockfile_version = true;

            for section in ["packages", "snapshots"] {
                let Some(Value::Object(entries)) = document.get(section) else {
                    continue;
                };

                for (key, entry) in entries {
                    self.read_pnpm_entry(key, entry);
                }
            }
        }

        if !has_lockfile_version {
            bail!("pnpm-lock.yaml has no lockfileVersion");
        }

        Ok(())
    }

    fn read_pnpm_entry(&mut self, key: &str, entry: &Value) {
        let (spec, suffix_groups) = split_peer_suffix(key.strip_prefix('/').unwrap_or(key));
        let Some(locked_entry) = parse_locked_entry(spec) else {
            return;
        };
        let (patches, peers): (Vec<&str>, Vec<&str>) = suffix_groups
            .into_iter()
            .partition(|group| group.starts_with("patch_hash="));
        let dependencies: Vec<LockedEntry> =
            dependency_ranges(entry, &["dependencies", "optionalDependencies"])
                .into_iter()
                .map(|(dependency, version)| pnpm_dependency(dependency, &version))
                .collect();

        if let Some(locked_version) = self.lock(&locked_entry) {
            locked_version
                .patches
                .extend(patches.iter().map(|&patch| patch.to_owned()));

            if let Some(resolution) = entry.get("resolution") {
                locked_version.resolution.insert(resolution.to_string());
            }

            locked_version.dependencies.extend(
                dependencies
                    .iter()
                    .filter(|dependency| !dependency.name.starts_with(TYPES_SCOPE))
                    .map(|dependency| format!("{}@{}", dependency.name, dependency.version)),
            );
        }

        let peer_dependencies = peers
            .iter()
            .filter_map(|peer| parse_locked_entry(split_peer_suffix(peer).0));

        for dependency in dependencies.into_iter().chain(peer_dependencies) {
            self.depend(&locked_entry, dependency);
        }
    }

    fn read_npm(&mut self, document: &Value) -> Result<()> {
        let Some(Value::Object(packages)) = document.get("packages") else {
            let lockfile_version = document.get("lockfileVersion").unwrap_or(&Value::Null);

            bail!("lockfileVersion {lockfile_version} is not supported");
        };
        let installed: HashMap<&str, LockedEntry> = packages
            .iter()
            .filter(|(_, entry)| entry.get("link").and_then(Value::as_bool) != Some(true))
            .filter_map(|(key, entry)| {
                let (_, name) = key.rsplit_once("node_modules/")?;
                let version = entry.get("version").map(scalar_text).unwrap_or_default();

                Some((
                    key.as_str(),
                    LockedEntry {
                        name: name.to_owned(),
                        version,
                    },
                ))
            })
            .collect();

        for (key, locked_entry) in &installed {
            let entry = &packages[*key];
            let dependencies = dependency_ranges(
                entry,
                &["dependencies", "optionalDependencies", "peerDependencies"],
            );

            if let Some(locked_version) = self.lock(locked_entry) {
                locked_version
                    .resolution
                    .insert(property_text(entry, &["resolved", "integrity"]));
                locked_version.dependencies.extend(
                    dependencies
                        .iter()
                        .map(|(dependency, range)| format!("{dependency}@{range}")),
                );
            }

            for (dependency, _) in &dependencies {
                match installed_dependency(&installed, key, dependency) {
                    Some(dependency_entry) => self.depend(locked_entry, dependency_entry.clone()),
                    None => self.depend_by_name(locked_entry, dependency),
                }
            }
        }

        Ok(())
    }

    fn read_yarn_berry(&mut self, document: &Value) {
        let Some(entries) = document.as_object() else {
            return;
        };
        let yarn_entries = entries
            .iter()
            .filter(|(key, _)| *key != "__metadata")
            .filter(|(_, entry)| {
                !entry
                    .get("resolution")
                    .and_then(Value::as_str)
                    .is_some_and(|resolution| resolution.contains("@workspace:"))
            })
            .map(|(header, entry)| YarnEntry {
                header: header.clone(),
                version: entry.get("version").map(scalar_text).unwrap_or_default(),
                resolution: property_text(entry, &["resolution", "checksum"]),
                dependencies: dependency_ranges(
                    entry,
                    &["dependencies", "optionalDependencies", "peerDependencies"],
                ),
            })
            .collect();

        self.read_yarn_entries(yarn_entries);
    }

    fn read_yarn_classic(&mut self, text: &str) {
        let yarn_entries = yarn_classic_blocks(text)
            .into_iter()
            .map(|block| {
                let resolution = ["resolved", "integrity"]
                    .iter()
                    .filter_map(|property| block.properties.get(*property))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ");

                YarnEntry {
                    version: block.properties.get("version").cloned().unwrap_or_default(),
                    header: block.header,
                    resolution,
                    dependencies: block.dependencies,
                }
            })
            .collect();

        self.read_yarn_entries(yarn_entries);
    }

    fn read_yarn_entries(&mut self, yarn_entries: Vec<YarnEntry>) {
        let mut entries_by_spec: HashMap<String, LockedEntry> = HashMap::new();
        let mut locked_entries = Vec::new();

        for yarn_entry in yarn_entries {
            let specs: Vec<&str> = yarn_entry
                .header
                .split(", ")
                .map(|spec| unquote(spec.trim()))
                .collect();
            let Some((name, _)) = specs.first().and_then(|spec| split_name_and_version(spec))
            else {
                continue;
            };
            let locked_entry = LockedEntry {
                name: name.to_owned(),
                version: yarn_entry.version.clone(),
            };

            for spec in specs {
                entries_by_spec.insert(spec.to_owned(), locked_entry.clone());
            }

            if let Some(locked_version) = self.lock(&locked_entry) {
                locked_version
                    .resolution
                    .insert(yarn_entry.resolution.clone());
                locked_version.dependencies.extend(
                    yarn_entry
                        .dependencies
                        .iter()
                        .filter(|(dependency, _)| !dependency.starts_with(TYPES_SCOPE))
                        .map(|(dependency, range)| format!("{dependency}@{range}")),
                );
            }

            locked_entries.push((locked_entry, yarn_entry.dependencies));
        }

        for (locked_entry, dependencies) in locked_entries {
            for (dependency, range) in dependencies {
                let dependency_entry = entries_by_spec
                    .get(&format!("{dependency}@{range}"))
                    .or_else(|| entries_by_spec.get(&format!("{dependency}@npm:{range}")));

                match dependency_entry {
                    Some(dependency_entry) => self.depend(&locked_entry, dependency_entry.clone()),
                    None => self.depend_by_name(&locked_entry, &dependency),
                }
            }
        }
    }
}

struct YarnEntry {
    header: String,
    version: String,
    resolution: String,
    dependencies: Vec<(String, String)>,
}

struct YarnBlock {
    header: String,
    properties: BTreeMap<String, String>,
    dependencies: Vec<(String, String)>,
}

fn yarn_classic_blocks(text: &str) -> Vec<YarnBlock> {
    let mut blocks: Vec<YarnBlock> = Vec::new();
    let mut in_dependencies = false;

    for line in text.lines() {
        let content = line.trim_start();

        if content.is_empty() || content.starts_with('#') {
            continue;
        }

        let indentation = line.len() - content.len();

        if indentation == 0 {
            blocks.push(YarnBlock {
                header: content.trim_end_matches(':').to_owned(),
                properties: BTreeMap::new(),
                dependencies: Vec::new(),
            });
            in_dependencies = false;

            continue;
        }

        let Some(block) = blocks.last_mut() else {
            continue;
        };
        let (key, value) = split_property(content);

        if indentation <= 2 {
            in_dependencies = value.is_empty()
                && matches!(
                    key,
                    "dependencies" | "optionalDependencies" | "peerDependencies"
                );

            if !value.is_empty() {
                block.properties.insert(key.to_owned(), value.to_owned());
            }
        } else if in_dependencies {
            block.dependencies.push((key.to_owned(), value.to_owned()));
        }
    }

    blocks
}

fn installed_dependency<'i>(
    installed: &'i HashMap<&str, LockedEntry>,
    dependent_key: &str,
    dependency: &str,
) -> Option<&'i LockedEntry> {
    let mut install_prefix = dependent_key;

    loop {
        let candidate = if install_prefix.is_empty() {
            format!("node_modules/{dependency}")
        } else {
            format!("{install_prefix}/node_modules/{dependency}")
        };

        if let Some(dependency_entry) = installed.get(candidate.as_str()) {
            return Some(dependency_entry);
        }

        if install_prefix.is_empty() {
            return None;
        }

        install_prefix = install_prefix
            .rsplit_once("node_modules/")
            .map_or("", |(parent, _)| parent.trim_end_matches('/'));
    }
}

fn parse_locked_entry(spec: &str) -> Option<LockedEntry> {
    let (name, version) = split_name_and_version(spec)?;

    Some(LockedEntry {
        name: name.to_owned(),
        version: version.to_owned(),
    })
}

fn pnpm_dependency(dependency: String, version: &str) -> LockedEntry {
    let version = split_peer_suffix(version).0;
    let version = version.strip_prefix('/').unwrap_or(version);

    match split_name_and_version(version) {
        Some((aliased_name, aliased_version)) if !version.contains(':') => LockedEntry {
            name: aliased_name.to_owned(),
            version: aliased_version.to_owned(),
        },
        _ => LockedEntry {
            name: dependency,
            version: version.to_owned(),
        },
    }
}

fn split_property(content: &str) -> (&str, &str) {
    let content = content.trim_end_matches(':');
    let (key, value) = match content.strip_prefix('"') {
        Some(quoted) => match quoted.split_once('"') {
            Some((key, rest)) => (key, rest),
            None => (quoted, ""),
        },
        None => content.split_once(' ').unwrap_or((content, "")),
    };
    let value = value.trim_start_matches(':').trim();

    (key, unquote(value))
}

fn unquote(text: &str) -> &str {
    text.strip_prefix('"')
        .and_then(|unquoted| unquoted.strip_suffix('"'))
        .unwrap_or(text)
}

fn scalar_text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

fn property_text(entry: &Value, properties: &[&str]) -> String {
    properties
        .iter()
        .filter_map(|property| entry.get(*property))
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

fn dependency_ranges(entry: &Value, fields: &[&str]) -> Vec<(String, String)> {
    fields
        .iter()
        .filter_map(|field| entry.get(*field)?.as_object())
        .flatten()
        .map(|(dependency, range)| (dependency.clone(), scalar_text(range)))
        .collect()
}

fn split_peer_suffix(key: &str) -> (&str, Vec<&str>) {
    let Some(suffix_start) = key.find('(') else {
        return (key, Vec::new());
    };
    let mut groups = Vec::new();
    let mut depth = 0;
    let mut group_start = suffix_start;

    for (offset, character) in key.char_indices().skip(suffix_start) {
        match character {
            '(' => {
                if depth == 0 {
                    group_start = offset + 1;
                }

                depth += 1;
            }
            ')' => {
                depth -= 1;

                if depth == 0 {
                    groups.push(&key[group_start..offset]);
                }
            }
            _ => {}
        }
    }

    (&key[..suffix_start], groups)
}

pub(crate) fn split_name_and_version(spec: &str) -> Option<(&str, &str)> {
    let separator = match spec.strip_prefix('@') {
        Some(unscoped) => unscoped.find('@').map(|offset| offset + 1)?,
        None => spec.find('@').filter(|&offset| offset > 0)?,
    };

    Some((&spec[..separator], &spec[separator + 1..]))
}

fn describe_change(
    base_versions: &BTreeMap<String, LockedVersion>,
    head_versions: &BTreeMap<String, LockedVersion>,
) -> String {
    let removed = versions_missing_from(base_versions, head_versions);
    let added = versions_missing_from(head_versions, base_versions);

    match (removed.is_empty(), added.is_empty()) {
        (false, false) => format!("{} -> {}", removed.join(", "), added.join(", ")),
        (true, false) => format!("added {}", added.join(", ")),
        (false, true) => format!("removed {}", removed.join(", ")),
        (true, true) => {
            let (version, base_version, head_version) = head_versions
                .iter()
                .find_map(|(version, head_version)| {
                    let base_version = &base_versions[version];

                    (base_version != head_version).then_some((version, base_version, head_version))
                })
                .expect("versions with the same numbers differ in one of them");
            let what_changed = if base_version.patches != head_version.patches {
                "patched differently"
            } else if base_version.resolution != head_version.resolution {
                "resolved differently"
            } else {
                "dependencies changed"
            };

            format!("{version}, {what_changed}")
        }
    }
}

fn versions_missing_from<'v>(
    versions: &'v BTreeMap<String, LockedVersion>,
    other_versions: &BTreeMap<String, LockedVersion>,
) -> Vec<&'v str> {
    versions
        .keys()
        .filter(|version| !other_versions.contains_key(*version))
        .map(String::as_str)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed(kind: LockfileKind, base: &str, head: &str) -> Vec<(String, String)> {
        let base = LockedPackages::parse(kind, base).unwrap();
        let head = LockedPackages::parse(kind, head).unwrap();

        LockedPackages::changed_packages(&base, &head)
            .into_iter()
            .map(|changed_package| (changed_package.name, changed_package.detail))
            .collect()
    }

    fn change(name: &str, detail: &str) -> (String, String) {
        (name.to_owned(), detail.to_owned())
    }

    fn edges(kind: LockfileKind, text: &str) -> Vec<(String, String)> {
        LockedPackages::parse(kind, text)
            .unwrap()
            .dependency_edges()
            .map(|(dependent, dependency)| {
                (
                    format!("{}@{}", dependent.name, dependent.version),
                    format!("{}@{}", dependency.name, dependency.version),
                )
            })
            .collect()
    }

    fn edge(dependent: &str, dependency: &str) -> (String, String) {
        (dependent.to_owned(), dependency.to_owned())
    }

    const PNPM_V9: &str = "
lockfileVersion: '9.0'

importers:
  .:
    devDependencies:
      vitest:
        specifier: ^3.2.0
        version: 3.2.0(@types/node@22.0.0)(jsdom@26.0.0)

packages:
  '@types/node@22.0.0':
    resolution: {integrity: sha512-types}
  debug@4.3.4:
    resolution: {integrity: sha512-debug-4.3.4}
  express@4.19.0:
    resolution: {integrity: sha512-express}
  jsdom@26.0.0:
    resolution: {integrity: sha512-jsdom}
  vitest@3.2.0:
    resolution: {integrity: sha512-vitest}

snapshots:
  '@types/node@22.0.0': {}
  debug@4.3.4: {}
  express@4.19.0:
    dependencies:
      debug: 4.3.4
  jsdom@26.0.0: {}
  vitest@3.2.0(@types/node@22.0.0)(jsdom@26.0.0):
    dependencies:
      '@types/node': 22.0.0
";

    #[test]
    fn pnpm_lockfile_changes_are_the_packages_whose_resolved_versions_differ() {
        let head = PNPM_V9
            .replace("debug@4.3.4", "debug@4.3.5")
            .replace("debug: 4.3.4", "debug: 4.3.5")
            .replace("sha512-debug-4.3.4", "sha512-debug-4.3.5");

        assert_eq!(
            changed(LockfileKind::Pnpm, PNPM_V9, &head),
            [
                change("debug", "4.3.4 -> 4.3.5"),
                change("express", "4.19.0, dependencies changed"),
            ]
        );
    }

    #[test]
    fn type_packages_and_their_peer_suffixes_never_count_as_changes() {
        let head = PNPM_V9
            .replace("@types/node@22.0.0", "@types/node@22.1.0")
            .replace("'@types/node': 22.0.0", "'@types/node': 22.1.0");

        assert_eq!(changed(LockfileKind::Pnpm, PNPM_V9, &head), []);
    }

    #[test]
    fn pnpm_dependencies_peers_and_patches_are_read_from_snapshot_keys() {
        let head = PNPM_V9.replace("jsdom@26.0.0: {}", "jsdom@26.0.0(patch_hash=abc123): {}");

        assert_eq!(
            changed(LockfileKind::Pnpm, PNPM_V9, &head),
            [change("jsdom", "26.0.0, patched differently")]
        );
        assert_eq!(
            edges(LockfileKind::Pnpm, PNPM_V9),
            [
                edge("express@4.19.0", "debug@4.3.4"),
                edge("vitest@3.2.0", "jsdom@26.0.0"),
            ]
        );
    }

    #[test]
    fn pnpm_v6_keys_with_a_leading_slash_and_inline_dependencies_are_read() {
        let base = "
lockfileVersion: '6.0'
packages:
  /debug@4.3.4:
    resolution: {integrity: sha512-a}
    dev: false
  /express@4.19.0:
    resolution: {integrity: sha512-b}
    dependencies:
      debug: 4.3.4
    dev: false
";
        let head = base.replace("dev: false", "dev: true");

        assert_eq!(changed(LockfileKind::Pnpm, base, &head), []);
        assert_eq!(
            edges(LockfileKind::Pnpm, base),
            [edge("express@4.19.0", "debug@4.3.4")]
        );
    }

    #[test]
    fn old_and_unknown_lockfiles_cannot_be_diffed() {
        let errors = [
            (
                LockfileKind::Pnpm,
                "lockfileVersion: 5.4\n",
                "lockfileVersion 5.4 is not supported",
            ),
            (
                LockfileKind::Pnpm,
                "importers: {}\n",
                "pnpm-lock.yaml has no lockfileVersion",
            ),
            (
                LockfileKind::Npm,
                r#"{ "lockfileVersion": 1, "dependencies": {} }"#,
                "lockfileVersion 1 is not supported",
            ),
            (
                LockfileKind::Yarn,
                "lodash@^4:\n  version \"4.17.21\"\n",
                "unknown yarn.lock format",
            ),
            (
                LockfileKind::Npm,
                "{",
                "invalid JSON: EOF while parsing an object at line 1 column 1",
            ),
        ];

        for (kind, text, expected_error) in errors {
            let error = LockedPackages::parse(kind, text).unwrap_err();

            assert_eq!(error.to_string(), expected_error);
        }
    }

    #[test]
    fn npm_lockfile_packages_are_named_by_their_install_path() {
        let base = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "app", "dependencies": { "express": "^4.19.0" } },
                "packages/lib": { "name": "@acme/lib", "version": "1.0.0" },
                "node_modules/@acme/lib": { "resolved": "packages/lib", "link": true },
                "node_modules/express": {
                    "version": "4.19.0",
                    "resolved": "https://registry.npmjs.org/express/-/express-4.19.0.tgz",
                    "integrity": "sha512-express",
                    "dependencies": { "debug": "2.6.9" }
                },
                "node_modules/express/node_modules/debug": {
                    "version": "2.6.9",
                    "integrity": "sha512-debug-2"
                },
                "node_modules/debug": { "version": "4.3.4", "integrity": "sha512-debug-4" }
            }
        }"#;
        let head = base.replace("sha512-debug-2", "sha512-debug-2-republished");

        assert_eq!(
            changed(LockfileKind::Npm, base, &head),
            [change("debug", "2.6.9, resolved differently")]
        );
        assert_eq!(
            edges(LockfileKind::Npm, base),
            [edge("express@4.19.0", "debug@2.6.9")]
        );
    }

    #[test]
    fn yarn_classic_entries_are_read_from_their_headers_and_indented_properties() {
        let base = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1


"@babel/code-frame@^7.0.0", "@babel/code-frame@^7.10.4":
  version "7.12.13"
  resolved "https://registry.yarnpkg.com/@babel/code-frame/-/code-frame-7.12.13.tgz"
  integrity sha512-code-frame
  dependencies:
    "@babel/highlight" "^7.12.13"

"@babel/highlight@^7.12.13":
  version "7.12.13"
  integrity sha512-highlight
"#;
        let head = base.replace(
            "\"7.12.13\"\n  integrity sha512-highlight",
            "\"7.14.0\"\n  integrity sha512-highlight-7.14",
        );

        assert_eq!(
            changed(LockfileKind::Yarn, base, &head),
            [change("@babel/highlight", "7.12.13 -> 7.14.0")]
        );
        assert_eq!(
            edges(LockfileKind::Yarn, base),
            [edge(
                "@babel/code-frame@7.12.13",
                "@babel/highlight@7.12.13"
            )]
        );
    }

    #[test]
    fn yarn_berry_entries_are_read_as_yaml_without_workspaces() {
        let base = r#"__metadata:
  version: 8
  cacheKey: 10

"@acme/lib@workspace:packages/lib":
  version: 0.0.0-use.local
  resolution: "@acme/lib@workspace:packages/lib"
  dependencies:
    lodash: "npm:^4.17.0"
  languageName: unknown
  linkType: soft

"lodash@npm:^4.17.0, lodash@npm:^4.17.20":
  version: 4.17.20
  resolution: "lodash@npm:4.17.20"
  checksum: 10/abc
  languageName: node
  linkType: hard
"#;
        let head = base
            .replace("version: 4.17.20", "version: 4.17.21")
            .replace("lodash@npm:4.17.20", "lodash@npm:4.17.21");

        assert_eq!(
            changed(LockfileKind::Yarn, base, &head),
            [change("lodash", "4.17.20 -> 4.17.21")]
        );
    }

    #[test]
    fn pnpm_lockfile_documents_are_all_read() {
        let package_manager_document = "---\nlockfileVersion: '9.0'\n\nimporters:\n  .:\n    packageManagerDependencies: {}\n\npackages:\n  '@pnpm/exe@12.4.2':\n    resolution: {integrity: sha512-pnpm}\n\n---\n";
        let head = format!("{package_manager_document}{PNPM_V9}");

        assert_eq!(
            changed(LockfileKind::Pnpm, PNPM_V9, &head),
            [change("@pnpm/exe", "added 12.4.2")]
        );
    }

    #[test]
    fn dependents_point_at_the_exact_version_they_use() {
        let lockfile = "
lockfileVersion: '9.0'
packages:
  tinyexec@0.3.2: {resolution: {integrity: a}}
  tinyexec@1.0.1: {resolution: {integrity: b}}
snapshots:
  tinyexec@0.3.2: {}
  tinyexec@1.0.1: {}
  vitest@3.2.0:
    dependencies:
      tinyexec: 0.3.2
  eslint-plugin@1.0.0:
    dependencies:
      tinyexec: 1.0.1
  aliased@1.0.0:
    dependencies:
      string-width-cjs: string-width@4.2.3
";

        assert_eq!(
            edges(LockfileKind::Pnpm, lockfile),
            [
                edge("aliased@1.0.0", "string-width@4.2.3"),
                edge("eslint-plugin@1.0.0", "tinyexec@1.0.1"),
                edge("vitest@3.2.0", "tinyexec@0.3.2"),
            ]
        );
    }

    #[test]
    fn added_and_removed_packages_are_changes_too() {
        let head = PNPM_V9
            .replace(
                "  express@4.19.0:\n    resolution: {integrity: sha512-express}\n",
                "",
            )
            .replace(
                "  express@4.19.0:\n    dependencies:\n      debug: 4.3.4\n",
                "",
            )
            .replace(
                "  jsdom@26.0.0: {}\n",
                "  jsdom@26.0.0: {}\n  zod@3.0.0: {}\n",
            );

        assert_eq!(
            changed(LockfileKind::Pnpm, PNPM_V9, &head),
            [
                change("express", "removed 4.19.0"),
                change("zod", "added 3.0.0")
            ]
        );
    }

    #[test]
    fn names_and_versions_are_split_at_the_version_separator() {
        assert_eq!(
            split_name_and_version("lodash@4.17.21"),
            Some(("lodash", "4.17.21"))
        );
        assert_eq!(
            split_name_and_version("@vue/shared@3.5.0"),
            Some(("@vue/shared", "3.5.0"))
        );
        assert_eq!(
            split_name_and_version("lodash@npm:^4.17.0"),
            Some(("lodash", "npm:^4.17.0"))
        );
        assert_eq!(split_name_and_version("lodash"), None);
        assert_eq!(split_name_and_version("@vue/shared"), None);
    }

    #[test]
    fn peer_suffixes_are_split_at_top_level_parentheses() {
        assert_eq!(
            split_peer_suffix("@vitest/mocker@3.0.0(vite@6.0.0(@types/node@22.0.0))(msw@2.0.0)"),
            (
                "@vitest/mocker@3.0.0",
                vec!["vite@6.0.0(@types/node@22.0.0)", "msw@2.0.0"]
            )
        );
        assert_eq!(
            split_peer_suffix("lodash@4.17.21"),
            ("lodash@4.17.21", vec![])
        );
    }
}
