//! Crates read from the Cargo.toml files of the repository: workspace members, their targets and their dependencies, with workspace inheritance.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use globset::Glob;
use prober_core::files::RepoPath;
use toml::{Table, Value};

use crate::conventions::{
    MANIFEST, crate_identifier, join_directory, normalized_join, parent_directory, split_path,
};

const DEPENDENCY_TABLES: [(&str, DependencyKind); 3] = [
    ("dependencies", DependencyKind::Normal),
    ("dev-dependencies", DependencyKind::Dev),
    ("build-dependencies", DependencyKind::Build),
];

const TARGET_DIRECTORIES: [(&str, &str); 4] = [
    ("bin", "src/bin"),
    ("test", "tests"),
    ("example", "examples"),
    ("bench", "benches"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DependencyKind {
    Normal,
    Dev,
    Build,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Dependency {
    pub name_in_code: String,
    pub package: String,
    pub kind: DependencyKind,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetKind {
    Lib,
    Bin,
    Test,
    Example,
    Bench,
    Build,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub kind: TargetKind,
    pub root: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Crate {
    pub name: String,
    pub lib_name: String,
    pub manifest: RepoPath,
    pub directory: String,
    pub targets: Vec<Target>,
    pub proc_macro: bool,
    pub dependencies: Vec<Dependency>,
}

impl Crate {
    pub fn build_script(&self) -> Option<RepoPath> {
        self.targets
            .iter()
            .find(|target| target.kind == TargetKind::Build)
            .and_then(|target| RepoPath::new(target.root.clone()).ok())
    }
}

#[derive(Debug, Default)]
pub(crate) struct Workspace {
    pub crates: Vec<Crate>,
}

impl Workspace {
    pub fn read(repo_files: &[RepoPath], read: &dyn Fn(&RepoPath) -> Option<String>) -> Self {
        let files: HashSet<&str> = repo_files.iter().map(RepoPath::as_str).collect();
        let manifests: BTreeMap<&RepoPath, Table> = repo_files
            .iter()
            .filter(|path| split_path(path.as_str()).1 == MANIFEST)
            .filter_map(|path| Some((path, toml::from_str::<Table>(&read(path)?).ok()?)))
            .collect();
        let roots: Vec<(&str, &Table)> = manifests
            .iter()
            .filter_map(|(path, manifest)| {
                Some((
                    parent_directory(path.as_str()),
                    manifest.get("workspace")?.as_table()?,
                ))
            })
            .collect();
        let mut crates = Vec::new();

        for (path, manifest) in &manifests {
            let Some(package) = manifest.get("package").and_then(Value::as_table) else {
                continue;
            };
            let directory = parent_directory(path.as_str());
            let workspace = roots
                .iter()
                .filter(|(root, _)| is_within(directory, root))
                .max_by_key(|(root, _)| root.len());

            if let Some((root, workspace)) = workspace
                && !is_member(directory, root, workspace)
            {
                continue;
            }

            let inherited = workspace.map(|(root, workspace)| (*root, *workspace));
            let Some(name) = package.get("name").and_then(Value::as_str) else {
                continue;
            };
            let lib = manifest.get("lib").and_then(Value::as_table);
            let lib_name = lib
                .and_then(|lib| lib.get("name"))
                .and_then(Value::as_str)
                .map_or_else(|| crate_identifier(name), str::to_owned);

            crates.push(Crate {
                name: name.to_owned(),
                lib_name,
                manifest: (*path).clone(),
                directory: directory.to_owned(),
                targets: targets(directory, manifest, package, &files),
                proc_macro: lib
                    .and_then(|lib| lib.get("proc-macro").or_else(|| lib.get("proc_macro")))
                    .and_then(Value::as_bool)
                    .unwrap_or_default(),
                dependencies: dependencies(directory, manifest, inherited),
            });
        }

        Self { crates }
    }

    pub fn crate_at(&self, directory: &str) -> Option<usize> {
        self.crates
            .iter()
            .position(|krate| krate.directory == directory)
    }

    pub fn workspace_dependency(&self, dependency: &Dependency) -> Option<usize> {
        match &dependency.path {
            Some(path) => self.crate_at(path),
            None => None,
        }
    }
}

fn is_within(directory: &str, root: &str) -> bool {
    root.is_empty()
        || directory == root
        || directory.starts_with(root) && directory[root.len()..].starts_with('/')
}

fn is_member(directory: &str, root: &str, workspace: &Table) -> bool {
    if directory == root {
        return true;
    }

    let relative = directory
        .strip_prefix(root)
        .unwrap_or(directory)
        .trim_start_matches('/');
    let matches = |key: &str| {
        workspace
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|pattern| {
                let pattern = pattern.trim_end_matches('/');

                pattern == relative
                    || Glob::new(pattern)
                        .is_ok_and(|glob| glob.compile_matcher().is_match(relative))
            })
    };

    matches("members") && !matches("exclude")
}

fn targets(
    directory: &str,
    manifest: &Table,
    package: &Table,
    files: &HashSet<&str>,
) -> Vec<Target> {
    let exists = |relative: &str| files.contains(join_directory(directory, relative).as_str());
    let mut targets = Vec::new();
    let mut add = |kind: TargetKind, relative: &str| {
        if let Some(root) = normalized_join(directory, relative)
            && files.contains(root.as_str())
            && !targets.iter().any(|target: &Target| target.root == root)
        {
            targets.push(Target { kind, root });
        }
    };
    let lib_path = manifest
        .get("lib")
        .and_then(|lib| lib.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("src/lib.rs");

    add(TargetKind::Lib, lib_path);

    for (table, kind) in [
        ("bin", TargetKind::Bin),
        ("test", TargetKind::Test),
        ("example", TargetKind::Example),
        ("bench", TargetKind::Bench),
    ] {
        for declared in manifest
            .get(table)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(path) = declared.get("path").and_then(Value::as_str) {
                add(kind, path);
            }
        }
    }

    if exists("src/main.rs") {
        add(TargetKind::Bin, "src/main.rs");
    }

    let prefix = |relative: &str| format!("{}/", join_directory(directory, relative));

    for (table, relative) in TARGET_DIRECTORIES {
        let kind = match table {
            "bin" => TargetKind::Bin,
            "test" => TargetKind::Test,
            "example" => TargetKind::Example,
            _ => TargetKind::Bench,
        };
        let prefix = prefix(relative);
        let mut found: BTreeSet<&str> = BTreeSet::new();

        for file in files.iter().filter(|file| file.starts_with(&prefix)) {
            let rest = &file[prefix.len()..];
            let is_root = match rest.split_once('/') {
                None => rest.ends_with(".rs"),
                Some((_, "main.rs")) => true,
                _ => false,
            };

            if is_root {
                found.insert(file);
            }
        }

        for file in found {
            let relative = &file[directory.len()..].trim_start_matches('/');

            add(kind, relative);
        }
    }

    match package.get("build") {
        Some(Value::String(path)) => add(TargetKind::Build, path),
        Some(Value::Boolean(false)) => {}
        _ => add(TargetKind::Build, "build.rs"),
    }

    targets
}

fn dependencies(
    directory: &str,
    manifest: &Table,
    workspace: Option<(&str, &Table)>,
) -> Vec<Dependency> {
    let mut tables: Vec<(&Table, DependencyKind)> = Vec::new();

    for (name, kind) in DEPENDENCY_TABLES {
        if let Some(table) = manifest.get(name).and_then(Value::as_table) {
            tables.push((table, kind));
        }
    }

    for platform in manifest
        .get("target")
        .and_then(Value::as_table)
        .into_iter()
        .flat_map(|targets| targets.values())
    {
        for (name, kind) in DEPENDENCY_TABLES {
            if let Some(table) = platform.get(name).and_then(Value::as_table) {
                tables.push((table, kind));
            }
        }
    }

    let workspace_dependencies = workspace
        .and_then(|(root, workspace)| Some((root, workspace.get("dependencies")?.as_table()?)));
    let mut dependencies = Vec::new();

    for (table, kind) in tables {
        for (key, specification) in table {
            let inherits = specification
                .get("workspace")
                .and_then(Value::as_bool)
                .unwrap_or_default();
            let (base, specification) = match workspace_dependencies {
                Some((root, inherited)) if inherits => match inherited.get(key) {
                    Some(inherited) => (root, inherited),
                    None => (directory, specification),
                },
                _ => (directory, specification),
            };
            let package = specification
                .get("package")
                .and_then(Value::as_str)
                .unwrap_or(key);
            let path = specification
                .get("path")
                .and_then(Value::as_str)
                .and_then(|path| normalized_join(base, path));

            dependencies.push(Dependency {
                name_in_code: crate_identifier(key),
                package: package.to_owned(),
                kind,
                path,
            });
        }
    }

    dependencies
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(files: &[(&str, &str)]) -> Workspace {
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

    #[test]
    fn members_targets_and_inherited_dependencies() {
        let workspace = workspace(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/skipped\"]\n\n[workspace.dependencies]\ncore = { package = \"app-core\", path = \"crates/core\" }\nserde = \"1\"\n",
            ),
            (
                "crates/core/Cargo.toml",
                "[package]\nname = \"app-core\"\n\n[dependencies]\nserde.workspace = true\n",
            ),
            ("crates/core/src/lib.rs", ""),
            ("crates/core/build.rs", ""),
            (
                "crates/cli/Cargo.toml",
                "[package]\nname = \"app-cli\"\n\n[dependencies]\ncore.workspace = true\n\n[dev-dependencies]\ntempfile = \"3\"\n",
            ),
            ("crates/cli/src/main.rs", ""),
            ("crates/cli/src/bin/tool.rs", ""),
            ("crates/cli/tests/end_to_end.rs", ""),
            ("crates/cli/tests/common/mod.rs", ""),
            (
                "crates/skipped/Cargo.toml",
                "[package]\nname = \"skipped\"\n",
            ),
            ("crates/skipped/src/lib.rs", ""),
        ]);
        let names: Vec<&str> = workspace
            .crates
            .iter()
            .map(|krate| krate.name.as_str())
            .collect();
        let named = |name: &str| {
            workspace
                .crates
                .iter()
                .find(|krate| krate.name == name)
                .unwrap()
        };
        let cli = named("app-cli");
        let core = named("app-core");

        assert_eq!(names, ["app-cli", "app-core"]);
        assert_eq!(
            cli.targets
                .iter()
                .map(|target| target.root.as_str())
                .collect::<Vec<_>>(),
            [
                "crates/cli/src/main.rs",
                "crates/cli/src/bin/tool.rs",
                "crates/cli/tests/end_to_end.rs"
            ]
        );
        assert_eq!(cli.dependencies[0].name_in_code, "core");
        assert_eq!(cli.dependencies[0].package, "app-core");
        assert_eq!(cli.dependencies[0].path.as_deref(), Some("crates/core"));
        assert_eq!(cli.dependencies[1].kind, DependencyKind::Dev);
        assert_eq!(core.lib_name, "app_core");
        assert!(core.build_script().is_some());
    }
}
