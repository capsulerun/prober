//! Cargo changes diffed field by field: lockfile entries with their dependents in the lock, and the tables of each Cargo.toml.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use anyhow::{Result, anyhow};
use prober_core::changes::{BaseRevision, ChangeKind, FileChange};
use prober_core::files::RepoPath;
use prober_core::report::ModeReason;
use toml::{Table, Value};

use crate::conventions::{LOCKFILE, MANIFEST, split_path};
use crate::workspace::Workspace;

const PACKAGE_FIELDS_WITHOUT_EFFECT: &[&str] = &[
    "authors",
    "badges",
    "categories",
    "default-run",
    "description",
    "documentation",
    "exclude",
    "homepage",
    "include",
    "keywords",
    "license",
    "license-file",
    "metadata",
    "publish",
    "readme",
    "repository",
    "version",
];

const TABLES_WITHOUT_EFFECT: [&str; 7] = [
    "badges",
    "bench",
    "bin",
    "dev-dependencies",
    "example",
    "lints",
    "test",
];

const CRATE_WIDE_TABLES: [&str; 5] = [
    "build-dependencies",
    "dependencies",
    "features",
    "lib",
    "target",
];

const WORKSPACE_FIELDS_WITHOUT_EFFECT: [&str; 5] =
    ["default-members", "exclude", "lints", "members", "metadata"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackageChange {
    pub change_path: RepoPath,
    pub package: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CrateChange {
    pub change_path: RepoPath,
    pub krate: usize,
    pub description: String,
}

#[derive(Debug, Default)]
pub(crate) struct CargoChanges {
    global_reasons: Vec<ModeReason>,
    packages: Vec<PackageChange>,
    crates: Vec<CrateChange>,
}

impl CargoChanges {
    pub fn between(
        workspace: &Workspace,
        read_head: &dyn Fn(&RepoPath) -> Option<String>,
        changes: &[FileChange],
        base: &dyn BaseRevision,
    ) -> Result<Self> {
        let mut cargo_changes = CargoChanges::default();

        for change in changes {
            let name = split_path(change.path.as_str()).1;

            if name != MANIFEST && name != LOCKFILE {
                continue;
            }

            let base_path = match &change.kind {
                ChangeKind::Renamed { from } => from,
                _ => &change.path,
            };
            let head = match change.kind {
                ChangeKind::Deleted => String::new(),
                _ => read_head(&change.path).unwrap_or_default(),
            };
            let base_text = match change.kind {
                ChangeKind::Added => String::new(),
                _ => base
                    .read_file(base_path)?
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_default(),
            };
            let diffed = if name == LOCKFILE {
                cargo_changes.diff_lockfile(change, &head, &base_text)
            } else {
                cargo_changes.diff_manifest(workspace, change, &head, &base_text)
            };

            if let Err(error) = diffed {
                cargo_changes
                    .global_reasons
                    .push(ModeReason::UnparsableManifest {
                        change: change.clone(),
                        message: format!("{error:#}"),
                    });
            }
        }

        Ok(cargo_changes)
    }

    pub fn global_reasons(&self) -> &[ModeReason] {
        &self.global_reasons
    }

    pub fn packages(&self) -> &[PackageChange] {
        &self.packages
    }

    pub fn crates(&self) -> &[CrateChange] {
        &self.crates
    }

    fn diff_lockfile(&mut self, change: &FileChange, head: &str, base: &str) -> Result<()> {
        let head = locked_packages(&parse(head)?);
        let base = locked_packages(&parse(base)?);
        let names: BTreeSet<&String> = head.versions.keys().chain(base.versions.keys()).collect();

        for name in names {
            let (before, after) = (base.versions.get(name), head.versions.get(name));
            let detail = match (before, after) {
                (Some(before), Some(after)) if before != after => {
                    format!("{} -> {}", before.0, after.0)
                }
                (None, Some(after)) => format!("added {}", after.0),
                (Some(before), None) => format!("removed {}", before.0),
                _ => continue,
            };

            if after.or(before).is_some_and(|(_, external)| *external) {
                self.packages.push(PackageChange {
                    change_path: change.path.clone(),
                    package: name.clone(),
                    description: format!("crate {name} changed ({detail})"),
                });
            }

            for dependent in closure(name, &head.dependents) {
                if head
                    .versions
                    .get(&dependent)
                    .is_some_and(|(_, external)| *external)
                {
                    self.packages.push(PackageChange {
                        change_path: change.path.clone(),
                        package: dependent.clone(),
                        description: format!(
                            "crate {dependent} depends on {name}, which changed ({detail})"
                        ),
                    });
                }
            }
        }

        Ok(())
    }

    fn diff_manifest(
        &mut self,
        workspace: &Workspace,
        change: &FileChange,
        head: &str,
        base: &str,
    ) -> Result<()> {
        if change.kind != ChangeKind::Modified {
            return Ok(());
        }

        let head = parse(head)?;
        let base = parse(base)?;
        let krate = workspace
            .crates
            .iter()
            .position(|krate| krate.manifest == change.path);

        for key in changed_keys(Some(&head), Some(&base)) {
            match key.as_str() {
                "package" => {
                    for field in changed_keys(table(&head, "package"), table(&base, "package")) {
                        if !PACKAGE_FIELDS_WITHOUT_EFFECT.contains(&field.as_str()) {
                            self.crate_wide(change, krate, &format!("package.{field}"));
                        }
                    }
                }
                "workspace" => self.diff_workspace(workspace, change, &head, &base),
                key if TABLES_WITHOUT_EFFECT.contains(&key) => {}
                key if CRATE_WIDE_TABLES.contains(&key) => self.crate_wide(change, krate, key),
                key => self.global_reasons.push(ModeReason::GlobalField {
                    change: change.clone(),
                    field: key.to_owned(),
                }),
            }
        }

        Ok(())
    }

    fn diff_workspace(
        &mut self,
        workspace: &Workspace,
        change: &FileChange,
        head: &Table,
        base: &Table,
    ) {
        let head = table(head, "workspace");
        let base = table(base, "workspace");

        for key in changed_keys(head, base) {
            match key.as_str() {
                key if WORKSPACE_FIELDS_WITHOUT_EFFECT.contains(&key) => {}
                "package" => {
                    for field in changed_keys(
                        head.and_then(|head| table(head, "package")),
                        base.and_then(|base| table(base, "package")),
                    ) {
                        if !PACKAGE_FIELDS_WITHOUT_EFFECT.contains(&field.as_str()) {
                            self.global_reasons.push(ModeReason::GlobalField {
                                change: change.clone(),
                                field: format!("workspace.package.{field}"),
                            });
                        }
                    }
                }
                "dependencies" => {
                    let head_dependencies = head.and_then(|head| table(head, "dependencies"));
                    let base_dependencies = base.and_then(|base| table(base, "dependencies"));

                    for dependency in changed_keys(head_dependencies, base_dependencies) {
                        let package = head_dependencies
                            .and_then(|table| table.get(&dependency))
                            .or_else(|| base_dependencies.and_then(|table| table.get(&dependency)))
                            .and_then(|specification| specification.get("package"))
                            .and_then(Value::as_str)
                            .unwrap_or(&dependency)
                            .to_owned();

                        for (krate, crate_info) in workspace.crates.iter().enumerate() {
                            if crate_info
                                .dependencies
                                .iter()
                                .any(|used| used.package == package)
                            {
                                self.crates.push(CrateChange {
                                    change_path: change.path.clone(),
                                    krate,
                                    description: format!(
                                        "{} inherits workspace dependency {dependency}, which changed",
                                        crate_info.name
                                    ),
                                });
                            }
                        }
                    }
                }
                key => self.global_reasons.push(ModeReason::GlobalField {
                    change: change.clone(),
                    field: format!("workspace.{key}"),
                }),
            }
        }
    }

    fn crate_wide(&mut self, change: &FileChange, krate: Option<usize>, field: &str) {
        match krate {
            Some(krate) => self.crates.push(CrateChange {
                change_path: change.path.clone(),
                krate,
                description: format!("{field} changed"),
            }),
            None => self.global_reasons.push(ModeReason::GlobalField {
                change: change.clone(),
                field: field.to_owned(),
            }),
        }
    }
}

struct LockedPackages {
    versions: BTreeMap<String, (String, bool)>,
    dependents: BTreeMap<String, BTreeSet<String>>,
}

fn locked_packages(document: &Table) -> LockedPackages {
    let mut versions = BTreeMap::new();
    let mut dependents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for package in document
        .get("package")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = package.get("name").and_then(Value::as_str) else {
            continue;
        };
        let version = package
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let source = package.get("source").and_then(Value::as_str);
        let checksum = package
            .get("checksum")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let fingerprint = match source {
            Some(source) if source.starts_with("git+") => format!("{version} ({source})"),
            _ if checksum.is_empty() => version.to_owned(),
            _ => format!("{version} ({})", &checksum[..checksum.len().min(12)]),
        };

        versions.insert(name.to_owned(), (fingerprint, source.is_some()));

        for dependency in package
            .get("dependencies")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let dependency = dependency.split(' ').next().unwrap_or(dependency);

            dependents
                .entry(dependency.to_owned())
                .or_default()
                .insert(name.to_owned());
        }
    }

    LockedPackages {
        versions,
        dependents,
    }
}

fn closure(name: &str, dependents: &BTreeMap<String, BTreeSet<String>>) -> BTreeSet<String> {
    let mut reached = BTreeSet::new();
    let mut queue = VecDeque::from([name.to_owned()]);

    while let Some(current) = queue.pop_front() {
        for dependent in dependents.get(&current).into_iter().flatten() {
            if dependent != name && reached.insert(dependent.clone()) {
                queue.push_back(dependent.clone());
            }
        }
    }

    reached
}

fn table<'t>(document: &'t Table, key: &str) -> Option<&'t Table> {
    document.get(key).and_then(Value::as_table)
}

fn changed_keys(head: Option<&Table>, base: Option<&Table>) -> Vec<String> {
    let empty = Table::new();
    let head = head.unwrap_or(&empty);
    let base = base.unwrap_or(&empty);
    let keys: BTreeSet<&String> = head.keys().chain(base.keys()).collect();

    keys.into_iter()
        .filter(|key| head.get(*key) != base.get(*key))
        .cloned()
        .collect()
}

fn parse(text: &str) -> Result<Table> {
    if text.trim().is_empty() {
        return Ok(Table::new());
    }

    toml::from_str::<Table>(text).map_err(|error| anyhow!("{}", error.message()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Base(&'static str);

    impl BaseRevision for Base {
        fn read_file(&self, _: &RepoPath) -> Result<Option<Vec<u8>>> {
            Ok(Some(self.0.as_bytes().to_vec()))
        }
    }

    fn modified(path: &str) -> FileChange {
        FileChange {
            path: RepoPath::new(path).unwrap(),
            kind: ChangeKind::Modified,
        }
    }

    fn workspace() -> Workspace {
        let files = [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"app\"]\n\n[workspace.dependencies]\nserde = \"1\"\n",
            ),
            (
                "app/Cargo.toml",
                "[package]\nname = \"app\"\n\n[dependencies]\nserde.workspace = true\n",
            ),
            ("app/src/lib.rs", ""),
        ];
        let repo_files: Vec<RepoPath> = files
            .iter()
            .map(|(path, _)| RepoPath::new(*path).unwrap())
            .collect();
        let read = |path: &RepoPath| {
            files
                .iter()
                .find(|(file, _)| *file == path.as_str())
                .map(|(_, text)| (*text).to_owned())
        };

        Workspace::read(&repo_files, &read)
    }

    fn diff(path: &str, base: &'static str, head: &'static str) -> CargoChanges {
        let read_head = |_: &RepoPath| Some(head.to_owned());

        CargoChanges::between(&workspace(), &read_head, &[modified(path)], &Base(base)).unwrap()
    }

    #[test]
    fn lockfile_changes_reach_the_packages_depending_on_them() {
        let base = "[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"serde\"]\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.1\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\ndependencies = [\"serde_derive\"]\n\n[[package]]\nname = \"serde_derive\"\nversion = \"1.0.1\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
        let head = "[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"serde\"]\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.1\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\ndependencies = [\"serde_derive\"]\n\n[[package]]\nname = \"serde_derive\"\nversion = \"1.0.2\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
        let changes = diff("Cargo.lock", base, head);
        let packages: Vec<(&str, &str)> = changes
            .packages()
            .iter()
            .map(|package| (package.package.as_str(), package.description.as_str()))
            .collect();

        assert_eq!(
            packages,
            [
                (
                    "serde_derive",
                    "crate serde_derive changed (1.0.1 -> 1.0.2)"
                ),
                (
                    "serde",
                    "crate serde depends on serde_derive, which changed (1.0.1 -> 1.0.2)"
                ),
            ]
        );
    }

    #[test]
    fn manifest_tables_are_diffed_one_by_one() {
        let version = diff(
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
            "[package]\nname = \"app\"\nversion = \"0.2.0\"\n",
        );
        let features = diff(
            "app/Cargo.toml",
            "[package]\nname = \"app\"\n\n[features]\ndefault = []\n",
            "[package]\nname = \"app\"\n\n[features]\ndefault = [\"fast\"]\nfast = []\n",
        );
        let profile = diff(
            "Cargo.toml",
            "[workspace]\nmembers = [\"app\"]\n",
            "[workspace]\nmembers = [\"app\"]\n\n[profile.test]\nopt-level = 1\n",
        );
        let inherited = diff(
            "Cargo.toml",
            "[workspace]\nmembers = [\"app\"]\n\n[workspace.dependencies]\nserde = \"1.0.1\"\n",
            "[workspace]\nmembers = [\"app\"]\n\n[workspace.dependencies]\nserde = \"1.0.2\"\n",
        );

        assert!(version.crates().is_empty() && version.global_reasons().is_empty());
        assert_eq!(features.crates()[0].description, "features changed");
        assert_eq!(
            profile
                .global_reasons()
                .iter()
                .map(ModeReason::message)
                .collect::<Vec<_>>(),
            ["Cargo.toml (modified) changes \"profile\", which may affect every test"]
        );
        assert_eq!(
            inherited.crates()[0].description,
            "app inherits workspace dependency serde, which changed"
        );
    }
}
