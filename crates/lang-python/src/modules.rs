//! Modules of the repository by dotted name: the package roots they are imported from, and the files an import runs.

use std::collections::{BTreeSet, HashSet};

use prober_core::files::RepoPath;
use toml::Value;

use crate::bindings::{NativeModule, native_module};
use crate::conventions::{
    PACKAGE_INIT, is_source_file, is_standard_library, join_directory, parent_directory, split_path,
};

const PROJECT_MANIFEST: &str = "pyproject.toml";

const SOURCE_DIRECTORY: &str = "src";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImportTarget {
    Repo(Vec<RepoPath>),
    External(String),
    Standard,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedModule {
    pub module: RepoPath,
    pub packages: Vec<RepoPath>,
}

pub(crate) struct ModuleIndex {
    files: HashSet<String>,
    directories: HashSet<String>,
    roots: Vec<String>,
    top_level_packages: Vec<String>,
    native_modules: Vec<NativeModule>,
}

impl ModuleIndex {
    pub fn new(repo_files: &[RepoPath], read_head: &dyn Fn(&RepoPath) -> Option<String>) -> Self {
        let files: HashSet<String> = repo_files
            .iter()
            .filter(|path| is_source_file(path))
            .map(|path| path.as_str().to_owned())
            .collect();
        let mut directories = HashSet::new();

        for file in &files {
            let mut directory = parent_directory(file);

            while !directory.is_empty() && directories.insert(directory.to_owned()) {
                directory = parent_directory(directory);
            }
        }

        let mut roots = BTreeSet::from([String::new()]);
        let mut top_level_packages = BTreeSet::new();

        for file in &files {
            let (directory, name) = split_path(file);

            if name != PACKAGE_INIT {
                continue;
            }

            let parent = parent_directory(directory);

            if !directory.is_empty() && !files.contains(&join_directory(parent, PACKAGE_INIT)) {
                roots.insert(parent.to_owned());
                top_level_packages.insert(directory.to_owned());
            }
        }

        for directory in &directories {
            if split_path(directory).1 == SOURCE_DIRECTORY {
                roots.insert(directory.clone());
            }
        }

        let mut native_modules = Vec::new();

        for manifest in repo_files
            .iter()
            .filter(|path| split_path(path.as_str()).1 == PROJECT_MANIFEST)
        {
            let project_directory = parent_directory(manifest.as_str());

            roots.insert(project_directory.to_owned());

            let Some(document) =
                read_head(manifest).and_then(|text| toml::from_str::<toml::Table>(&text).ok())
            else {
                continue;
            };

            native_modules.extend(native_module(manifest, &document, read_head));

            for declared in declared_roots(&Value::Table(document)) {
                if let Some(root) = normalized_join(project_directory, &declared) {
                    roots.insert(root);
                }
            }
        }

        Self {
            files,
            directories,
            roots: roots.into_iter().collect(),
            top_level_packages: top_level_packages.into_iter().collect(),
            native_modules,
        }
    }

    pub fn native_modules(&self) -> &[NativeModule] {
        &self.native_modules
    }

    pub fn native_module_named(&self, importer: &RepoPath, dotted: &str) -> Option<usize> {
        if self.native_modules.is_empty() || dotted.is_empty() {
            return None;
        }

        let path = dotted.replace('.', "/");

        self.roots_for(importer)
            .into_iter()
            .find_map(|root| self.native_module_at(&join_directory(root, &path)))
    }

    pub fn native_module_at(&self, location: &str) -> Option<usize> {
        self.native_modules.iter().position(|native| {
            location
                .strip_prefix(native.location.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
    }

    pub fn roots(&self) -> impl Iterator<Item = &str> {
        self.roots.iter().map(String::as_str)
    }

    pub fn dotted_name(&self, path: &str) -> Option<String> {
        let basedir = self.basedir(path);
        let relative = match basedir {
            "" => path,
            basedir => path.strip_prefix(basedir)?.strip_prefix('/')?,
        };
        let module = relative.strip_suffix(".py")?;
        let module = module
            .strip_suffix("/__init__")
            .unwrap_or(module)
            .replace('/', ".");

        (!module.is_empty()).then_some(module)
    }

    pub fn package_name(&self, path: &str) -> Option<String> {
        let dotted = self.dotted_name(path)?;

        if split_path(path).1 == PACKAGE_INIT {
            return Some(dotted);
        }

        Some(
            dotted
                .rsplit_once('.')
                .map_or(String::new(), |(package, _)| package.to_owned()),
        )
    }

    pub fn top_level_packages(&self) -> &[String] {
        &self.top_level_packages
    }

    pub fn contains(&self, path: &str) -> bool {
        self.files.contains(path)
    }

    pub fn files_under(&self, directory: &str) -> impl Iterator<Item = &str> {
        let prefix = format!("{directory}/");
        let mut files: Vec<&str> = self
            .files
            .iter()
            .map(String::as_str)
            .filter(|file| file.starts_with(&prefix))
            .collect();

        files.sort_unstable();
        files.into_iter()
    }

    pub fn is_directory(&self, path: &str) -> bool {
        path.is_empty() || self.directories.contains(path)
    }

    pub fn roots_for<'s>(&'s self, importer: &'s RepoPath) -> Vec<&'s str> {
        let basedir = self.basedir(importer.as_str());
        let mut roots = vec![basedir];

        roots.extend(
            self.roots
                .iter()
                .map(String::as_str)
                .filter(|root| *root != basedir),
        );

        roots
    }

    pub fn basedir<'p>(&self, path: &'p str) -> &'p str {
        let mut directory = parent_directory(path);

        while !directory.is_empty()
            && self
                .files
                .contains(&join_directory(directory, PACKAGE_INIT))
        {
            directory = parent_directory(directory);
        }

        directory
    }

    pub fn resolve_absolute(&self, importer: &RepoPath, dotted: &str) -> ImportTarget {
        if dotted.is_empty() || dotted.split('.').any(str::is_empty) {
            return ImportTarget::Missing;
        }

        let resolved: Vec<RepoPath> = self
            .roots_for(importer)
            .into_iter()
            .filter_map(|root| self.resolve_in(root, dotted))
            .flat_map(ResolvedModule::files)
            .collect();

        if !resolved.is_empty() || self.names_namespace(importer, dotted) {
            return ImportTarget::Repo(deduplicated(resolved));
        }

        if let Some((package, _)) = dotted.rsplit_once('.')
            && let ImportTarget::Repo(files) = self.resolve_absolute(importer, package)
        {
            return ImportTarget::Repo(files);
        }

        let top_level = dotted.split('.').next().unwrap_or(dotted);

        if is_standard_library(top_level) {
            ImportTarget::Standard
        } else {
            ImportTarget::External(top_level.to_owned())
        }
    }

    pub fn resolve_module(&self, importer: &RepoPath, dotted: &str) -> Vec<ResolvedModule> {
        self.roots_for(importer)
            .into_iter()
            .filter_map(|root| self.resolve_in(root, dotted))
            .collect()
    }

    pub fn relative_directory(&self, importer: &RepoPath, level: u32) -> Option<String> {
        let mut directory = parent_directory(importer.as_str()).to_owned();

        for _ in 1..level {
            if directory.is_empty() {
                return None;
            }

            directory = parent_directory(&directory).to_owned();
        }

        Some(directory)
    }

    pub fn resolve_in_directory(&self, directory: &str, relative: &str) -> Option<ResolvedModule> {
        if relative.is_empty() {
            let init = join_directory(directory, PACKAGE_INIT);

            return self.contains(&init).then(|| ResolvedModule {
                module: repo_path(&init),
                packages: Vec::new(),
            });
        }

        let segments: Vec<&str> = relative.split('.').collect();
        let mut packages = Vec::new();
        let mut current = directory.to_owned();

        for segment in &segments[..segments.len() - 1] {
            current = join_directory(&current, segment);

            let init = join_directory(&current, PACKAGE_INIT);

            if self.contains(&init) {
                packages.push(repo_path(&init));
            } else if !self.is_directory(&current) {
                return None;
            }
        }

        let last = join_directory(&current, segments[segments.len() - 1]);
        let module_file = format!("{last}.py");
        let package_init = join_directory(&last, PACKAGE_INIT);
        let module = if self.contains(&module_file) {
            module_file
        } else if self.contains(&package_init) {
            package_init
        } else {
            return None;
        };

        Some(ResolvedModule {
            module: repo_path(&module),
            packages,
        })
    }

    pub fn module_globs(&self, importer: &RepoPath, dotted_glob: &str) -> Vec<String> {
        let path_glob = dotted_glob.replace('.', "/");

        self.roots_for(importer)
            .into_iter()
            .flat_map(|root| {
                [
                    join_directory(root, &format!("{path_glob}.py")),
                    join_directory(root, &format!("{path_glob}/{PACKAGE_INIT}")),
                ]
            })
            .collect()
    }

    fn resolve_in(&self, root: &str, dotted: &str) -> Option<ResolvedModule> {
        let top_level = dotted.split('.').next().unwrap_or(dotted);
        let top_level_path = join_directory(root, top_level);

        if !self.is_directory(&top_level_path) && !self.contains(&format!("{top_level_path}.py")) {
            return None;
        }

        self.resolve_in_directory(root, dotted)
    }

    fn names_namespace(&self, importer: &RepoPath, dotted: &str) -> bool {
        let path = dotted.replace('.', "/");

        self.roots_for(importer)
            .into_iter()
            .any(|root| self.is_directory(&join_directory(root, &path)))
    }
}

impl ResolvedModule {
    pub fn files(self) -> Vec<RepoPath> {
        let mut files = self.packages;

        files.push(self.module);

        files
    }
}

fn declared_roots(document: &Value) -> Vec<String> {
    let mut roots = Vec::new();
    let tool = document.get("tool");
    let setuptools = tool.and_then(|tool| tool.get("setuptools"));

    if let Some(package_directories) = setuptools
        .and_then(|setuptools| setuptools.get("package-dir"))
        .and_then(Value::as_table)
    {
        roots.extend(
            package_directories
                .iter()
                .filter(|(package, _)| package.is_empty())
                .filter_map(|(_, directory)| directory.as_str().map(str::to_owned)),
        );
    }

    if let Some(find) = setuptools
        .and_then(|setuptools| setuptools.get("packages"))
        .and_then(|packages| packages.get("find"))
    {
        roots.extend(strings(find.get("where")));
    }

    if let Some(packages) = tool
        .and_then(|tool| tool.get("poetry"))
        .and_then(|poetry| poetry.get("packages"))
        .and_then(Value::as_array)
    {
        roots.extend(packages.iter().filter_map(|package| {
            package
                .get("from")
                .and_then(Value::as_str)
                .map(str::to_owned)
        }));
    }

    if let Some(packages) = tool
        .and_then(|tool| tool.get("hatch"))
        .and_then(|hatch| hatch.get("build"))
        .and_then(|build| build.get("targets"))
        .and_then(|targets| targets.get("wheel"))
        .and_then(|wheel| wheel.get("packages"))
    {
        roots.extend(
            strings(Some(packages))
                .iter()
                .map(|package| parent_directory(package.trim_end_matches('/')).to_owned()),
        );
    }

    if let Some(python_source) = tool
        .and_then(|tool| tool.get("maturin"))
        .and_then(|maturin| maturin.get("python-source"))
        .and_then(Value::as_str)
    {
        roots.push(python_source.to_owned());
    }

    if let Some(python_path) = tool
        .and_then(|tool| tool.get("pytest"))
        .and_then(|pytest| pytest.get("ini_options"))
        .and_then(|options| options.get("pythonpath"))
    {
        roots.extend(strings(Some(python_path)));
    }

    roots
}

fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(text)) => text.split_whitespace().map(str::to_owned).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn normalized_join(directory: &str, relative: &str) -> Option<String> {
    let mut segments: Vec<&str> = directory.split('/').filter(|s| !s.is_empty()).collect();

    for segment in relative.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            segment => segments.push(segment),
        }
    }

    Some(segments.join("/"))
}

fn deduplicated(mut paths: Vec<RepoPath>) -> Vec<RepoPath> {
    let mut seen = HashSet::new();

    paths.retain(|path| seen.insert(path.clone()));

    paths
}

fn repo_path(path: &str) -> RepoPath {
    RepoPath::new(path).expect("module paths come from repository paths")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(files: &[&str], manifests: &[(&str, &str)]) -> ModuleIndex {
        let repo_files: Vec<RepoPath> = files
            .iter()
            .chain(manifests.iter().map(|(path, _)| path))
            .map(|path| RepoPath::new(*path).unwrap())
            .collect();
        let read_head = |path: &RepoPath| {
            manifests
                .iter()
                .find(|(manifest, _)| *manifest == path.as_str())
                .map(|(_, text)| (*text).to_owned())
        };

        ModuleIndex::new(&repo_files, &read_head)
    }

    fn resolved(target: ImportTarget) -> Vec<String> {
        match target {
            ImportTarget::Repo(files) => files.iter().map(ToString::to_string).collect(),
            other => panic!("expected repository files, found {other:?}"),
        }
    }

    fn importer(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    #[test]
    fn absolute_imports_run_every_package_on_the_way() {
        let index = index(
            &[
                "app/__init__.py",
                "app/core/__init__.py",
                "app/core/models.py",
                "tests/test_models.py",
            ],
            &[],
        );

        assert_eq!(
            resolved(index.resolve_absolute(&importer("tests/test_models.py"), "app.core.models")),
            [
                "app/__init__.py",
                "app/core/__init__.py",
                "app/core/models.py"
            ]
        );
        assert_eq!(
            resolved(index.resolve_absolute(&importer("tests/test_models.py"), "app")),
            ["app/__init__.py"]
        );
    }

    #[test]
    fn src_layouts_and_declared_roots_are_package_roots() {
        let index = index(
            &[
                "src/lib/__init__.py",
                "src/lib/api.py",
                "python/tool/cli.py",
                "tests/test_api.py",
            ],
            &[(
                "pyproject.toml",
                "[tool.pytest.ini_options]\npythonpath = [\"python\"]\n",
            )],
        );
        let test = importer("tests/test_api.py");

        assert_eq!(
            resolved(index.resolve_absolute(&test, "lib.api")),
            ["src/lib/__init__.py", "src/lib/api.py"]
        );
        assert_eq!(
            resolved(index.resolve_absolute(&test, "tool.cli")),
            ["python/tool/cli.py"]
        );
    }

    #[test]
    fn a_test_imports_its_siblings_through_its_basedir() {
        let index = index(&["tests/helpers.py", "tests/test_api.py"], &[]);

        assert_eq!(
            resolved(index.resolve_absolute(&importer("tests/test_api.py"), "helpers")),
            ["tests/helpers.py"]
        );
        assert_eq!(index.basedir("tests/test_api.py"), "tests");
    }

    #[test]
    fn namespace_packages_resolve_without_an_init() {
        let index = index(&["ns/part/module.py", "main.py"], &[]);
        let main = importer("main.py");

        assert_eq!(
            resolved(index.resolve_absolute(&main, "ns.part.module")),
            ["ns/part/module.py"]
        );
        assert_eq!(
            resolved(index.resolve_absolute(&main, "ns.part")),
            Vec::<String>::new()
        );
    }

    #[test]
    fn other_imports_are_standard_external_or_missing() {
        let index = index(&["app/__init__.py"], &[]);
        let module = importer("app/__init__.py");

        assert_eq!(
            index.resolve_absolute(&module, "os.path"),
            ImportTarget::Standard
        );
        assert_eq!(
            index.resolve_absolute(&module, "yaml.loader"),
            ImportTarget::External("yaml".to_owned())
        );
        assert_eq!(
            index.resolve_absolute(&module, "a..b"),
            ImportTarget::Missing
        );
    }
}
