use std::path::{Component, Path, PathBuf};

use oxc_resolver::{ResolveError, ResolveOptions, Resolver, TsconfigDiscovery};
use prober_core::files::RepoPath;

const RESOLVED_EXTENSIONS: [&str; 9] = [
    ".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".json",
];

const TYPESCRIPT_EQUIVALENTS: [(&str, &[&str]); 4] = [
    (".js", &[".ts", ".tsx", ".js", ".jsx"]),
    (".jsx", &[".tsx", ".jsx"]),
    (".mjs", &[".mts", ".mjs"]),
    (".cjs", &[".cts", ".cjs"]),
];

const CONDITION_NAMES: [&str; 4] = ["node", "import", "require", "default"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    RepoFile(RepoPath),
    Outside,
    Missing,
}

pub struct ModuleResolver {
    resolver: Resolver,
    repo_root: PathBuf,
}

impl ModuleResolver {
    pub fn new(repo_root: &Path) -> Self {
        let extension_alias = TYPESCRIPT_EQUIVALENTS
            .iter()
            .map(|&(javascript_extension, candidate_extensions)| {
                let candidate_extensions = candidate_extensions
                    .iter()
                    .map(|&extension| extension.to_owned())
                    .collect();

                (javascript_extension.to_owned(), candidate_extensions)
            })
            .collect();
        let options = ResolveOptions {
            tsconfig: Some(TsconfigDiscovery::Auto),
            condition_names: CONDITION_NAMES.map(String::from).to_vec(),
            extensions: RESOLVED_EXTENSIONS.map(String::from).to_vec(),
            extension_alias,
            builtin_modules: true,
            node_path: false,
            ..ResolveOptions::default()
        };

        Self {
            resolver: Resolver::new(options),
            repo_root: repo_root.to_path_buf(),
        }
    }

    pub fn resolve(&self, importer: &RepoPath, specifier: &str) -> Vec<RepoPath> {
        let importer_path = self.repo_root.join(importer.as_str());

        match self.resolver.resolve_file(&importer_path, specifier) {
            Ok(resolution) => self.repo_path(resolution.path()).into_iter().collect(),
            Err(_) => missing_relative_candidates(importer, specifier),
        }
    }

    pub fn resolve_specifier(&self, importer: &RepoPath, specifier: &str) -> Resolution {
        let importer_path = self.repo_root.join(importer.as_str());

        match self.resolver.resolve_file(&importer_path, specifier) {
            Ok(resolution) => self
                .repo_path(resolution.path())
                .map_or(Resolution::Outside, Resolution::RepoFile),
            Err(ResolveError::Builtin { .. }) => Resolution::Outside,
            Err(_) => Resolution::Missing,
        }
    }

    pub fn absolute_specifier(&self, path: &str) -> String {
        self.repo_root.join(path).to_string_lossy().into_owned()
    }

    fn repo_path(&self, absolute_path: &Path) -> Option<RepoPath> {
        let relative_path = absolute_path.strip_prefix(&self.repo_root).ok()?;
        let is_installed_package = relative_path
            .components()
            .any(|component| component == Component::Normal("node_modules".as_ref()));

        if is_installed_package {
            return None;
        }

        let components: Vec<&str> = relative_path
            .components()
            .map(|component| component.as_os_str().to_str())
            .collect::<Option<_>>()?;

        RepoPath::new(components.join("/")).ok()
    }
}

fn missing_relative_candidates(importer: &RepoPath, specifier: &str) -> Vec<RepoPath> {
    let is_relative = specifier.starts_with("./") || specifier.starts_with("../");
    let Some(missing_path) = is_relative
        .then(|| join_relative(importer, specifier))
        .flatten()
    else {
        return Vec::new();
    };

    missing_path_candidates(&missing_path)
}

pub fn missing_path_candidates(missing_path: &str) -> Vec<RepoPath> {
    let with_extensions = RESOLVED_EXTENSIONS
        .iter()
        .map(|extension| format!("{missing_path}{extension}"));
    let index_files = RESOLVED_EXTENSIONS
        .iter()
        .map(|extension| format!("{missing_path}/index{extension}"));
    let typescript_equivalents = TYPESCRIPT_EQUIVALENTS
        .iter()
        .filter_map(|(javascript_extension, candidate_extensions)| {
            let stem = missing_path.strip_suffix(javascript_extension)?;

            Some(
                candidate_extensions
                    .iter()
                    .map(move |extension| format!("{stem}{extension}")),
            )
        })
        .flatten();

    std::iter::once(missing_path.to_owned())
        .chain(with_extensions)
        .chain(index_files)
        .chain(typescript_equivalents)
        .filter_map(|candidate| RepoPath::new(candidate).ok())
        .collect()
}

fn join_relative(importer: &RepoPath, specifier: &str) -> Option<String> {
    let mut components: Vec<&str> = importer.as_str().split('/').collect();

    components.pop();

    for component in specifier.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            name => components.push(name),
        }
    }

    Some(components.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate_paths(importer: &str, specifier: &str) -> Vec<String> {
        missing_relative_candidates(&RepoPath::new(importer).unwrap(), specifier)
            .iter()
            .map(|candidate| candidate.as_str().to_owned())
            .collect()
    }

    #[test]
    fn missing_relative_import_lists_every_file_it_could_have_meant() {
        let candidates = candidate_paths("src/app/a.ts", "../util");

        assert!(candidates.contains(&"src/util".to_owned()));
        assert!(candidates.contains(&"src/util.ts".to_owned()));
        assert!(candidates.contains(&"src/util.json".to_owned()));
        assert!(candidates.contains(&"src/util/index.tsx".to_owned()));
    }

    #[test]
    fn missing_javascript_specifier_also_lists_its_typescript_source() {
        let candidates = candidate_paths("src/a.ts", "./util.js");

        assert!(candidates.contains(&"src/util.js".to_owned()));
        assert!(candidates.contains(&"src/util.ts".to_owned()));
        assert!(candidates.contains(&"src/util.tsx".to_owned()));
    }

    #[test]
    fn bare_and_escaping_specifiers_have_no_candidates() {
        assert_eq!(candidate_paths("src/a.ts", "lodash"), Vec::<String>::new());
        assert_eq!(candidate_paths("src/a.ts", "@/util"), Vec::<String>::new());
        assert_eq!(
            candidate_paths("src/a.ts", "../../outside"),
            Vec::<String>::new()
        );
    }
}
