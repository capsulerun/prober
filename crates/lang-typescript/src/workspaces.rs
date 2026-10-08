use std::collections::{BTreeMap, BTreeSet};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use prober_core::files::RepoPath;
use serde_json::Value;

use crate::manifests::documents::{parse_json, parse_yaml};

pub(crate) const PACKAGE_MANIFEST: &str = "package.json";

pub(crate) const PNPM_WORKSPACE: &str = "pnpm-workspace.yaml";

struct WorkspaceRoot {
    directory: String,
    members: GlobSet,
    excluded: GlobSet,
}

impl WorkspaceRoot {
    fn contains(&self, directory: &str) -> bool {
        if directory == self.directory {
            return true;
        }

        let Some(relative_directory) = relative_to(directory, &self.directory) else {
            return false;
        };

        self.members.is_match(relative_directory) && !self.excluded.is_match(relative_directory)
    }
}

#[derive(Debug, Default)]
pub(crate) struct Workspace {
    pub member_directories: BTreeSet<String>,
    pub packages: BTreeMap<String, String>,
}

pub(crate) fn discover_workspace<'p>(
    manifest_paths: impl IntoIterator<Item = &'p RepoPath>,
    read: &dyn Fn(&RepoPath) -> Option<String>,
) -> Workspace {
    let mut manifest_paths: Vec<&RepoPath> = manifest_paths
        .into_iter()
        .filter(|path| {
            !path
                .as_str()
                .split('/')
                .any(|segment| segment == "node_modules")
        })
        .collect();

    manifest_paths.sort();

    let package_manifests: Vec<(&RepoPath, Value)> = manifest_paths
        .iter()
        .filter(|path| file_name(path) == PACKAGE_MANIFEST)
        .filter_map(|&path| Some((path, parse_json(&read(path)?).ok()?)))
        .collect();
    let mut roots = vec![WorkspaceRoot {
        directory: String::new(),
        members: GlobSet::empty(),
        excluded: GlobSet::empty(),
    }];

    for &path in manifest_paths
        .iter()
        .filter(|path| file_name(path) == PNPM_WORKSPACE)
    {
        if let Some(workspace) = read(path).and_then(|text| parse_yaml(&text).ok()) {
            roots.extend(workspace_root(path, workspace.get("packages")));
        }
    }

    for (path, manifest) in &package_manifests {
        let patterns = match manifest.get("workspaces") {
            Some(Value::Object(workspaces)) => workspaces.get("packages"),
            patterns => patterns,
        };

        roots.extend(workspace_root(path, patterns));
    }

    let mut workspace = Workspace::default();

    for (path, manifest) in &package_manifests {
        let directory = parent_directory(path);

        if !roots.iter().any(|root| root.contains(directory)) {
            continue;
        }

        workspace.member_directories.insert(directory.to_owned());

        if let Some(Value::String(name)) = manifest.get("name") {
            workspace
                .packages
                .entry(name.clone())
                .or_insert_with(|| directory.to_owned());
        }
    }

    workspace
}

fn workspace_root(manifest_path: &RepoPath, patterns: Option<&Value>) -> Option<WorkspaceRoot> {
    let Some(Value::Array(patterns)) = patterns else {
        return None;
    };

    let mut members = GlobSetBuilder::new();
    let mut excluded = GlobSetBuilder::new();

    for pattern in patterns.iter().filter_map(Value::as_str) {
        let (builder, pattern) = match pattern.strip_prefix('!') {
            Some(excluded_pattern) => (&mut excluded, excluded_pattern),
            None => (&mut members, pattern),
        };
        let pattern = pattern.trim_start_matches("./").trim_end_matches('/');

        if let Ok(glob) = GlobBuilder::new(pattern).literal_separator(true).build() {
            builder.add(glob);
        }
    }

    Some(WorkspaceRoot {
        directory: parent_directory(manifest_path).to_owned(),
        members: members.build().ok()?,
        excluded: excluded.build().ok()?,
    })
}

fn relative_to<'d>(directory: &'d str, root: &str) -> Option<&'d str> {
    if root.is_empty() {
        return Some(directory);
    }

    directory.strip_prefix(root)?.strip_prefix('/')
}

pub(crate) fn is_workspace_manifest(path: &RepoPath) -> bool {
    matches!(file_name(path), PACKAGE_MANIFEST | PNPM_WORKSPACE)
}

pub(crate) fn parent_directory(path: &RepoPath) -> &str {
    path.as_str()
        .rsplit_once('/')
        .map_or("", |(directory, _)| directory)
}

fn file_name(path: &RepoPath) -> &str {
    path.as_str()
        .rsplit_once('/')
        .map_or(path.as_str(), |(_, file_name)| file_name)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn workspace_of(files: &[(&str, &str)]) -> Workspace {
        let contents: HashMap<RepoPath, String> = files
            .iter()
            .map(|&(path, content)| (RepoPath::new(path).unwrap(), content.to_owned()))
            .collect();
        let read = |path: &RepoPath| contents.get(path).cloned();

        discover_workspace(contents.keys(), &read)
    }

    fn packages_of(files: &[(&str, &str)]) -> Vec<(String, String)> {
        workspace_of(files).packages.into_iter().collect()
    }

    fn package(name: &str, directory: &str) -> (String, String) {
        (name.to_owned(), directory.to_owned())
    }

    #[test]
    fn pnpm_workspace_globs_select_the_member_packages() {
        let packages = packages_of(&[
            ("package.json", r#"{ "name": "root" }"#),
            (
                "pnpm-workspace.yaml",
                "packages:\n  - packages/*\n  - './apps/**'\n  - '!apps/legacy'\n",
            ),
            ("packages/lib/package.json", r#"{ "name": "@acme/lib" }"#),
            ("apps/web/client/package.json", r#"{ "name": "@acme/web" }"#),
            ("apps/legacy/package.json", r#"{ "name": "@acme/legacy" }"#),
            ("test/fixtures/package.json", r#"{ "name": "lodash" }"#),
            (
                "packages/lib/node_modules/dep/package.json",
                r#"{ "name": "dep" }"#,
            ),
        ]);

        assert_eq!(
            packages,
            [
                package("@acme/lib", "packages/lib"),
                package("@acme/web", "apps/web/client"),
                package("root", ""),
            ]
        );
    }

    #[test]
    fn npm_and_yarn_workspaces_are_read_from_the_root_manifest() {
        let array_workspaces = packages_of(&[
            ("package.json", r#"{ "workspaces": ["packages/*"] }"#),
            ("packages/a/package.json", r#"{ "name": "a" }"#),
        ]);
        let object_workspaces = packages_of(&[
            (
                "package.json",
                r#"{ "workspaces": { "packages": ["packages/*"] } }"#,
            ),
            ("packages/b/package.json", r#"{ "name": "b" }"#),
        ]);

        assert_eq!(array_workspaces, [package("a", "packages/a")]);
        assert_eq!(object_workspaces, [package("b", "packages/b")]);
    }

    #[test]
    fn members_include_unnamed_packages_but_not_templates_outside_the_workspace() {
        let workspace = workspace_of(&[
            ("package.json", r#"{ "private": true }"#),
            ("pnpm-workspace.yaml", "packages:\n  - packages/*\n"),
            ("packages/vue/package.json", r#"{ "name": "vue" }"#),
            (
                "packages/playground/src/template/package.json",
                r#"{ "name": "template" }"#,
            ),
        ]);

        assert_eq!(
            workspace.member_directories,
            BTreeSet::from(["".to_owned(), "packages/vue".to_owned()])
        );
    }

    #[test]
    fn repository_without_workspaces_only_names_its_root_package() {
        let packages = packages_of(&[
            ("package.json", r#"{ "name": "app" }"#),
            ("examples/demo/package.json", r#"{ "name": "demo" }"#),
            ("broken/package.json", "{"),
        ]);

        assert_eq!(packages, [package("app", "")]);
    }
}
