//! Dependency changes diffed package by package: lockfiles with their dependency closure, pinned requirements, and the fields of pyproject.toml.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

use anyhow::{Result, anyhow};
use prober_core::changes::{BaseRevision, ChangeKind, FileChange};
use prober_core::files::RepoPath;
use prober_core::report::ModeReason;
use toml::Value;

use crate::conventions::split_path;
use crate::pytest::configs::sections_without_pytest;

pub(crate) const PACKAGE_NODE_PREFIX: &str = "site-packages/";

const PROJECT_MANIFEST: &str = "pyproject.toml";

const PIPFILE: &str = "Pipfile";

const PIPFILE_LOCK: &str = "Pipfile.lock";

const TOML_LOCKFILES: [&str; 3] = ["uv.lock", "poetry.lock", "pdm.lock"];

const TOX_INI: &str = "tox.ini";

const SETUP_CFG: &str = "setup.cfg";

const SECTIONS_WITHOUT_EFFECT: &[&str] = &[
    "bdist_wheel",
    "bumpversion",
    "codespell",
    "coverage",
    "flake8",
    "isort",
    "metadata",
    "mypy",
    "pycodestyle",
    "pydocstyle",
    "pylint",
    "tool:isort",
];

const PROJECT_FIELDS_WITHOUT_EFFECT: [&str; 12] = [
    "name",
    "version",
    "description",
    "readme",
    "authors",
    "maintainers",
    "license",
    "license-files",
    "keywords",
    "classifiers",
    "urls",
    "requires-python",
];

const TOOLS_WITHOUT_EFFECT: &[&str] = &[
    "autopep8",
    "bandit",
    "black",
    "bumpversion",
    "codespell",
    "commitizen",
    "coverage",
    "docformatter",
    "flake8",
    "interrogate",
    "isort",
    "mypy",
    "pydocstyle",
    "pylint",
    "pyright",
    "pyupgrade",
    "ruff",
    "semantic_release",
    "setuptools_scm",
    "towncrier",
    "typos",
    "vulture",
    "yapf",
];

const DEPENDENCY_TABLES: [&str; 3] = ["dependencies", "optional-dependencies", "dev-dependencies"];

const DISTRIBUTIONS_WITHOUT_EFFECT: &[&str] = &[
    "autopep8",
    "bandit",
    "black",
    "build",
    "codespell",
    "flake8",
    "isort",
    "mypy",
    "mypy_extensions",
    "pip",
    "pre_commit",
    "prek",
    "pydocstyle",
    "pylint",
    "pyright",
    "pyupgrade",
    "ruff",
    "towncrier",
    "twine",
    "uv",
    "wheel",
    "yamllint",
];

const IMPORT_NAMES: &[(&str, &str)] = &[
    ("attrs", "attr"),
    ("beautifulsoup4", "bs4"),
    ("google_api_python_client", "googleapiclient"),
    ("msgpack_python", "msgpack"),
    ("opencv_python", "cv2"),
    ("opencv_python_headless", "cv2"),
    ("pillow", "PIL"),
    ("protobuf", "google"),
    ("pycryptodome", "Crypto"),
    ("pycryptodomex", "Cryptodome"),
    ("pyjwt", "jwt"),
    ("pyopenssl", "OpenSSL"),
    ("pyserial", "serial"),
    ("python_dateutil", "dateutil"),
    ("python_dotenv", "dotenv"),
    ("pyyaml", "yaml"),
    ("scikit_learn", "sklearn"),
    ("setuptools", "pkg_resources"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackageChange {
    pub change_path: RepoPath,
    pub import_name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectChange {
    pub change_path: RepoPath,
    pub directory: String,
    pub description: String,
}

#[derive(Debug, Default)]
pub(crate) struct DependencyChanges {
    global_reasons: Vec<ModeReason>,
    packages: Vec<PackageChange>,
    projects: Vec<ProjectChange>,
}

struct ChangedDistribution {
    change: FileChange,
    name: String,
    detail: String,
    dependents: BTreeSet<String>,
}

impl DependencyChanges {
    pub fn between(
        read_head: &dyn Fn(&RepoPath) -> Option<String>,
        changes: &[FileChange],
        base: &dyn BaseRevision,
        imported: &HashSet<String>,
        project_directories: &BTreeSet<String>,
        pytest: bool,
    ) -> Result<Self> {
        let mut dependency_changes = DependencyChanges::default();
        let mut distributions = Vec::new();

        for change in changes {
            let Some(kind) = manifest_kind(&change.path, pytest) else {
                continue;
            };
            let base_path = match &change.kind {
                ChangeKind::Renamed { from } => from,
                _ => &change.path,
            };
            let head_text = match change.kind {
                ChangeKind::Deleted => None,
                _ => read_head(&change.path),
            };
            let base_text = match change.kind {
                ChangeKind::Added => None,
                _ => base
                    .read_file(base_path)?
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
            };
            let diffed = diff_manifest(
                &change.path,
                kind,
                head_text.as_deref().unwrap_or_default(),
                base_text.as_deref().unwrap_or_default(),
            );

            match diffed {
                Ok(diff) => {
                    for field in diff.global_fields {
                        dependency_changes.add_field(change, field, project_directories);
                    }

                    distributions.extend(diff.distributions.into_iter().map(
                        |(name, (detail, dependents))| ChangedDistribution {
                            change: change.clone(),
                            name,
                            detail,
                            dependents,
                        },
                    ));
                }
                Err(error) => {
                    dependency_changes
                        .global_reasons
                        .push(ModeReason::UnparsableManifest {
                            change: change.clone(),
                            message: format!("{error:#}"),
                        })
                }
            }
        }

        for distribution in distributions {
            let mut accounted = DISTRIBUTIONS_WITHOUT_EFFECT.contains(&distribution.name.as_str())
                || distribution.name.starts_with("types_")
                || distribution.name.ends_with("_stubs");

            for (name, through) in std::iter::once((&distribution.name, None)).chain(
                distribution
                    .dependents
                    .iter()
                    .map(|dependent| (dependent, Some(&distribution.name))),
            ) {
                for import_name in import_names(name) {
                    if !imported.contains(&import_name) {
                        continue;
                    }

                    let description = match through {
                        None => format!("package {name} changed ({})", distribution.detail),
                        Some(changed) => format!(
                            "package {name} depends on {changed}, which changed ({})",
                            distribution.detail
                        ),
                    };

                    accounted = true;
                    dependency_changes.packages.push(PackageChange {
                        change_path: distribution.change.path.clone(),
                        import_name,
                        description,
                    });
                }
            }

            if !accounted {
                let field = format!(
                    "{} ({}), which no module imports by a known name",
                    distribution.name, distribution.detail
                );

                dependency_changes.add_field(&distribution.change, field, project_directories);
            }
        }

        dependency_changes.packages.sort_by(|left, right| {
            (&left.change_path, &left.import_name).cmp(&(&right.change_path, &right.import_name))
        });
        dependency_changes.packages.dedup_by(|later, earlier| {
            later.change_path == earlier.change_path && later.import_name == earlier.import_name
        });

        dependency_changes.projects.dedup();

        Ok(dependency_changes)
    }

    fn add_field(
        &mut self,
        change: &FileChange,
        field: String,
        project_directories: &BTreeSet<String>,
    ) {
        match project_of(&change.path, project_directories) {
            Some(directory) => self.projects.push(ProjectChange {
                change_path: change.path.clone(),
                directory: directory.to_owned(),
                description: format!("changes \"{field}\" in the project {directory}"),
            }),
            None => self.global_reasons.push(ModeReason::GlobalField {
                change: change.clone(),
                field,
            }),
        }
    }

    pub fn global_reasons(&self) -> &[ModeReason] {
        &self.global_reasons
    }

    pub fn packages(&self) -> &[PackageChange] {
        &self.packages
    }

    pub fn projects(&self) -> &[ProjectChange] {
        &self.projects
    }
}

pub(crate) fn project_of<'d>(
    path: &RepoPath,
    project_directories: &'d BTreeSet<String>,
) -> Option<&'d str> {
    let mut directory = split_path(path.as_str()).0;

    while !directory.is_empty() {
        if let Some(project) = project_directories.get(directory) {
            return Some(project);
        }

        directory = split_path(directory).0;
    }

    None
}

pub(crate) fn project_directories(repo_files: &[RepoPath]) -> BTreeSet<String> {
    repo_files
        .iter()
        .map(|path| split_path(path.as_str()))
        .filter(|(directory, name)| *name == PROJECT_MANIFEST && !directory.is_empty())
        .map(|(directory, _)| directory.to_owned())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManifestKind {
    Project { pytest: bool },
    Ini,
    Pipfile,
    PipfileLock,
    TomlLockfile,
    Requirements,
}

fn manifest_kind(path: &RepoPath, pytest: bool) -> Option<ManifestKind> {
    let (directory, name) = split_path(path.as_str());

    match name {
        PROJECT_MANIFEST => Some(ManifestKind::Project { pytest }),
        TOX_INI | SETUP_CFG if pytest => Some(ManifestKind::Ini),
        PIPFILE => Some(ManifestKind::Pipfile),
        PIPFILE_LOCK => Some(ManifestKind::PipfileLock),
        name if TOML_LOCKFILES.contains(&name) => Some(ManifestKind::TomlLockfile),
        name if is_requirements_file(directory, name) => Some(ManifestKind::Requirements),
        _ => None,
    }
}

pub(crate) fn is_requirements_file(directory: &str, name: &str) -> bool {
    let in_requirements_directory = split_path(directory).1 == "requirements";

    name.ends_with(".txt")
        && (name.contains("requirements")
            || name.contains("constraints")
            || in_requirements_directory)
}

#[derive(Default)]
struct ManifestDiff {
    global_fields: Vec<String>,
    distributions: BTreeMap<String, (String, BTreeSet<String>)>,
}

fn diff_manifest(
    path: &RepoPath,
    kind: ManifestKind,
    head: &str,
    base: &str,
) -> Result<ManifestDiff> {
    let mut diff = ManifestDiff::default();

    match kind {
        ManifestKind::Project { pytest } => {
            let head = parse_toml(head)?;
            let base = parse_toml(base)?;

            diff_project(&head, &base, pytest, &mut diff);
        }
        ManifestKind::Ini => {
            let head = sections_without_pytest(path, head);
            let base = sections_without_pytest(path, base);
            let sections: BTreeSet<&String> = head.keys().chain(base.keys()).collect();

            for section in sections {
                let without_effect = SECTIONS_WITHOUT_EFFECT.iter().any(|prefix| {
                    section == prefix
                        || section.starts_with(&format!("{prefix}:"))
                        || section.starts_with(&format!("{prefix}-"))
                });

                if head.get(section) != base.get(section) && !without_effect {
                    diff.global_fields.push(format!("[{section}]"));
                }
            }
        }
        ManifestKind::Pipfile => {
            let head = parse_toml(head)?;
            let base = parse_toml(base)?;

            for table in ["packages", "dev-packages"] {
                diff_requirement_table(head.get(table), base.get(table), &mut diff);
            }

            for key in changed_keys(&head, &base) {
                if key != "packages" && key != "dev-packages" {
                    diff.global_fields.push(key);
                }
            }
        }
        ManifestKind::PipfileLock => {
            let head: serde_json::Value = parse_json(head)?;
            let base: serde_json::Value = parse_json(base)?;
            let versions = |document: &serde_json::Value| {
                let mut versions = BTreeMap::new();

                for section in ["default", "develop"] {
                    if let Some(packages) =
                        document.get(section).and_then(|value| value.as_object())
                    {
                        for (name, entry) in packages {
                            versions.insert(normalized(name), entry.to_string());
                        }
                    }
                }

                versions
            };

            diff_versions(
                &versions(&head),
                &versions(&base),
                &BTreeMap::new(),
                &mut diff,
            );
        }
        ManifestKind::TomlLockfile => {
            let head = locked_packages(&parse_toml(head)?);
            let base = locked_packages(&parse_toml(base)?);

            diff_versions(&head.versions, &base.versions, &head.dependents, &mut diff);
        }
        ManifestKind::Requirements => {
            diff_versions(
                &requirements(head),
                &requirements(base),
                &BTreeMap::new(),
                &mut diff,
            );
        }
    }

    Ok(diff)
}

fn diff_project(head: &Value, base: &Value, pytest: bool, diff: &mut ManifestDiff) {
    for key in changed_keys(head, base) {
        match key.as_str() {
            "project" => {
                let head_project = head.get("project");
                let base_project = base.get("project");

                for field in changed_keys_of(head_project, base_project) {
                    if PROJECT_FIELDS_WITHOUT_EFFECT.contains(&field.as_str()) {
                        continue;
                    }

                    if DEPENDENCY_TABLES.contains(&field.as_str()) {
                        diff_requirement_table(
                            head_project.and_then(|project| project.get(&field)),
                            base_project.and_then(|project| project.get(&field)),
                            diff,
                        );
                    } else {
                        diff.global_fields.push(format!("project.{field}"));
                    }
                }
            }
            "dependency-groups" => {
                diff_requirement_table(head.get(&key), base.get(&key), diff);
            }
            "tool" => {
                let head_tool = head.get("tool");
                let base_tool = base.get("tool");

                for tool in changed_keys_of(head_tool, base_tool) {
                    if TOOLS_WITHOUT_EFFECT.contains(&tool.as_str()) || pytest && tool == "pytest" {
                        continue;
                    }

                    if tool == "poetry" {
                        diff_poetry(
                            head_tool.and_then(|tool| tool.get("poetry")),
                            base_tool.and_then(|tool| tool.get("poetry")),
                            diff,
                        );
                    } else {
                        diff.global_fields.push(format!("tool.{tool}"));
                    }
                }
            }
            _ => diff.global_fields.push(key),
        }
    }
}

fn diff_poetry(head: Option<&Value>, base: Option<&Value>, diff: &mut ManifestDiff) {
    for field in changed_keys_of(head, base) {
        match field.as_str() {
            "dependencies" | "dev-dependencies" => diff_requirement_table(
                head.and_then(|poetry| poetry.get(&field)),
                base.and_then(|poetry| poetry.get(&field)),
                diff,
            ),
            "group" => {
                let head_groups = head.and_then(|poetry| poetry.get("group"));
                let base_groups = base.and_then(|poetry| poetry.get("group"));

                for group in changed_keys_of(head_groups, base_groups) {
                    diff_requirement_table(
                        head_groups
                            .and_then(|groups| groups.get(&group))
                            .and_then(|group| group.get("dependencies")),
                        base_groups
                            .and_then(|groups| groups.get(&group))
                            .and_then(|group| group.get("dependencies")),
                        diff,
                    );
                }
            }
            field if PROJECT_FIELDS_WITHOUT_EFFECT.contains(&field) => {}
            field => diff.global_fields.push(format!("tool.poetry.{field}")),
        }
    }
}

fn diff_requirement_table(head: Option<&Value>, base: Option<&Value>, diff: &mut ManifestDiff) {
    let head = requirement_specs(head);
    let base = requirement_specs(base);

    diff_versions(&head, &base, &BTreeMap::new(), diff);
}

fn requirement_specs(value: Option<&Value>) -> BTreeMap<String, String> {
    let mut specs = BTreeMap::new();

    match value {
        Some(Value::Array(requirements)) => {
            for requirement in requirements.iter().filter_map(Value::as_str) {
                if let Some(name) = requirement_name(requirement) {
                    specs.insert(name, requirement.to_owned());
                }
            }
        }
        Some(Value::Table(table)) => {
            for (key, entry) in table {
                match entry {
                    Value::Array(_) => {
                        for (name, spec) in requirement_specs(Some(entry)) {
                            specs.insert(name, format!("{key}: {spec}"));
                        }
                    }
                    entry => {
                        specs.insert(normalized(key), entry.to_string());
                    }
                }
            }
        }
        _ => {}
    }

    specs
}

fn diff_versions(
    head: &BTreeMap<String, String>,
    base: &BTreeMap<String, String>,
    dependents: &BTreeMap<String, BTreeSet<String>>,
    diff: &mut ManifestDiff,
) {
    let names: BTreeSet<&String> = head.keys().chain(base.keys()).collect();

    for name in names {
        let detail = match (base.get(name), head.get(name)) {
            (Some(before), Some(after)) if before != after => {
                format!("{} -> {}", short(before), short(after))
            }
            (None, Some(after)) => format!("added {}", short(after)),
            (Some(before), None) => format!("removed {}", short(before)),
            _ => continue,
        };

        diff.distributions
            .insert(name.clone(), (detail, closure(name, dependents)));
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

struct LockedPackages {
    versions: BTreeMap<String, String>,
    dependents: BTreeMap<String, BTreeSet<String>>,
}

fn locked_packages(document: &Value) -> LockedPackages {
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
        let name = normalized(name);
        let mut fingerprint = package.clone();

        if let Some(table) = fingerprint.as_table_mut() {
            table.remove("dependencies");
            table.remove("optional-dependencies");
            table.remove("dev-dependencies");
            table.remove("metadata");
        }

        let version = package.get("version").and_then(Value::as_str).map_or_else(
            || fingerprint.to_string(),
            |version| format!("{version} {}", fingerprint_hash(&fingerprint)),
        );

        versions.insert(name.clone(), version);

        for dependency in lock_dependencies(package) {
            dependents
                .entry(dependency)
                .or_default()
                .insert(name.clone());
        }
    }

    LockedPackages {
        versions,
        dependents,
    }
}

fn lock_dependencies(package: &Value) -> Vec<String> {
    let mut dependencies = Vec::new();

    for table in ["dependencies", "optional-dependencies", "dev-dependencies"] {
        match package.get(table) {
            Some(Value::Array(entries)) => dependencies.extend(
                entries
                    .iter()
                    .filter_map(|entry| entry.get("name").and_then(Value::as_str).map(normalized)),
            ),
            Some(Value::Table(entries)) => {
                for (key, entry) in entries {
                    match entry {
                        Value::Array(nested) => {
                            dependencies.extend(nested.iter().filter_map(|entry| {
                                entry.get("name").and_then(Value::as_str).map(normalized)
                            }))
                        }
                        _ => dependencies.push(normalized(key)),
                    }
                }
            }
            _ => {}
        }
    }

    dependencies
}

fn fingerprint_hash(value: &Value) -> String {
    let text = value.to_string();
    let hash = text.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });

    format!("{hash:016x}")
}

fn requirements(text: &str) -> BTreeMap<String, String> {
    let mut pins = BTreeMap::new();
    let mut logical_line = String::new();

    for line in text.lines() {
        let line = line.split(" #").next().unwrap_or_default().trim_end();

        if let Some(continued) = line.strip_suffix('\\') {
            logical_line.push_str(continued);

            continue;
        }

        logical_line.push_str(line);

        let requirement = logical_line.trim();

        if !requirement.is_empty() && !requirement.starts_with(['#', '-']) {
            let spec = requirement
                .split("--hash")
                .next()
                .unwrap_or(requirement)
                .trim()
                .to_owned();

            if let Some(name) = requirement_name(&spec) {
                pins.insert(name, spec);
            }
        } else if requirement.starts_with('-') && !requirement.is_empty() {
            pins.insert(format!("option {requirement}"), requirement.to_owned());
        }

        logical_line.clear();
    }

    pins
}

fn requirement_name(requirement: &str) -> Option<String> {
    let end = requirement
        .find(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
        })
        .unwrap_or(requirement.len());
    let name = &requirement[..end];

    (!name.is_empty()).then(|| normalized(name))
}

pub(crate) fn normalized(name: &str) -> String {
    name.to_ascii_lowercase().replace(['-', '.'], "_")
}

fn import_names(distribution: &str) -> Vec<String> {
    let mut names = vec![distribution.to_owned()];

    for (known, import_name) in IMPORT_NAMES {
        if *known == distribution {
            names.push((*import_name).to_owned());
        }
    }

    for prefix in ["python_", "py_", "py"] {
        if let Some(stripped) = distribution
            .strip_prefix(prefix)
            .filter(|rest| !rest.is_empty())
        {
            names.push(stripped.to_owned());
        }
    }

    for suffix in ["_python", "_py"] {
        if let Some(stripped) = distribution
            .strip_suffix(suffix)
            .filter(|rest| !rest.is_empty())
        {
            names.push(stripped.to_owned());
        }
    }

    if let Some((first, _)) = distribution.split_once('_') {
        names.push(first.to_owned());
    }

    names.push(distribution.replace('_', ""));
    names.sort();
    names.dedup();

    names
}

fn changed_keys(head: &Value, base: &Value) -> Vec<String> {
    changed_keys_of(Some(head), Some(base))
}

fn changed_keys_of(head: Option<&Value>, base: Option<&Value>) -> Vec<String> {
    let empty = toml::map::Map::new();
    let head = head.and_then(Value::as_table).unwrap_or(&empty);
    let base = base.and_then(Value::as_table).unwrap_or(&empty);
    let keys: BTreeSet<&String> = head.keys().chain(base.keys()).collect();

    keys.into_iter()
        .filter(|key| head.get(*key) != base.get(*key))
        .cloned()
        .collect()
}

fn parse_toml(text: &str) -> Result<Value> {
    if text.trim().is_empty() {
        return Ok(Value::Table(toml::map::Map::new()));
    }

    toml::from_str::<toml::Table>(text)
        .map(Value::Table)
        .map_err(|error| anyhow!("{}", error.message()))
}

fn parse_json(text: &str) -> Result<serde_json::Value> {
    if text.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }

    Ok(serde_json::from_str(text)?)
}

fn short(spec: &str) -> String {
    let spec: String = spec.chars().take(60).collect();

    spec
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Base(Vec<(&'static str, &'static str)>);

    impl BaseRevision for Base {
        fn read_file(&self, path: &RepoPath) -> Result<Option<Vec<u8>>> {
            Ok(self
                .0
                .iter()
                .find(|(base_path, _)| *base_path == path.as_str())
                .map(|(_, text)| text.as_bytes().to_vec()))
        }
    }

    fn modified(path: &str) -> FileChange {
        FileChange {
            path: RepoPath::new(path).unwrap(),
            kind: ChangeKind::Modified,
        }
    }

    fn diff(
        path: &'static str,
        base: &'static str,
        head: &'static str,
        imported: &[&str],
    ) -> DependencyChanges {
        let read_head = |_: &RepoPath| Some(head.to_owned());
        let imported: HashSet<String> = imported.iter().map(|name| (*name).to_owned()).collect();

        DependencyChanges::between(
            &read_head,
            &[modified(path)],
            &Base(vec![(path, base)]),
            &imported,
            &BTreeSet::from(["libs/partners/openai".to_owned()]),
            false,
        )
        .unwrap()
    }

    fn messages(changes: &DependencyChanges) -> Vec<String> {
        changes
            .global_reasons()
            .iter()
            .map(ModeReason::message)
            .collect()
    }

    fn imports(changes: &DependencyChanges) -> Vec<(&str, &str)> {
        changes
            .packages()
            .iter()
            .map(|package| (package.import_name.as_str(), package.description.as_str()))
            .collect()
    }

    #[test]
    fn a_changed_pin_reaches_the_modules_importing_it() {
        let changes = diff(
            "requirements.txt",
            "PyYAML==6.0.1\nrequests==2.31.0\n",
            "PyYAML==6.0.2\nrequests==2.31.0\n",
            &["yaml", "requests"],
        );

        assert_eq!(
            imports(&changes),
            [(
                "yaml",
                "package pyyaml changed (PyYAML==6.0.1 -> PyYAML==6.0.2)"
            )]
        );
        assert!(messages(&changes).is_empty());
    }

    #[test]
    fn a_lockfile_change_reaches_the_dependents_of_the_changed_package() {
        let base = "[[package]]\nname = \"httpx\"\nversion = \"0.27.0\"\ndependencies = [{ name = \"idna\" }]\n\n[[package]]\nname = \"idna\"\nversion = \"3.6\"\n";
        let head = "[[package]]\nname = \"httpx\"\nversion = \"0.27.0\"\ndependencies = [{ name = \"idna\" }]\n\n[[package]]\nname = \"idna\"\nversion = \"3.7\"\n";
        let changes = diff("uv.lock", base, head, &["httpx"]);

        assert_eq!(imports(&changes).len(), 1);
        assert!(
            imports(&changes)[0]
                .1
                .starts_with("package httpx depends on idna, which changed")
        );
        assert!(messages(&changes).is_empty());
    }

    #[test]
    fn a_package_nothing_imports_runs_every_test_unless_it_is_a_tool() {
        let changes = diff(
            "requirements_test.txt",
            "pytest-asyncio==0.23\nruff==0.5\n",
            "pytest-asyncio==0.24\nruff==0.6\n",
            &[],
        );

        assert_eq!(
            messages(&changes),
            [
                "requirements_test.txt (modified) changes \"pytest_asyncio (pytest-asyncio==0.23 -> pytest-asyncio==0.24), which no module imports by a known name\", which may affect every test"
            ]
        );
    }

    #[test]
    fn pyproject_fields_are_diffed_one_by_one() {
        let base = "[project]\nname = \"app\"\nversion = \"1.0\"\ndependencies = [\"httpx>=0.27\"]\n\n[tool.ruff]\nline-length = 88\n";
        let head = "[project]\nname = \"app\"\nversion = \"1.1\"\ndependencies = [\"httpx>=0.28\"]\n\n[tool.ruff]\nline-length = 100\n\n[tool.pytest.ini_options]\naddopts = \"-x\"\n";
        let changes = diff("pyproject.toml", base, head, &["httpx"]);

        assert_eq!(imports(&changes).len(), 1);
        assert_eq!(
            messages(&changes),
            ["pyproject.toml (modified) changes \"tool.pytest\", which may affect every test"]
        );
    }
}
