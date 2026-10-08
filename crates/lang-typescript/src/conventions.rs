//! TypeScript conventions: which files are parsed, which are tests, and which changes run every test.

use prober_core::files::RepoPath;

pub const LANGUAGE: &str = "typescript";

pub const ALWAYS_ALL_DEFAULTS: &[&str] = &[
    "**/package.json",
    "**/package-lock.json",
    "**/npm-shrinkwrap.json",
    "**/pnpm-lock.yaml",
    "**/pnpm-workspace.yaml",
    "**/yarn.lock",
    "**/.yarnrc.yml",
    "**/bun.lock",
    "**/bun.lockb",
    "**/.npmrc",
    "**/.nvmrc",
    "**/.node-version",
    "**/tsconfig*.json",
    "**/jsconfig*.json",
    "**/vitest.*",
    "**/vite.config.*",
    "**/jest.*",
    "**/babel.config.*",
    "**/.babelrc*",
    "**/.swcrc",
];

const SOURCE_EXTENSIONS: [&str; 8] = ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

const DECLARATION_FILE_SUFFIXES: [&str; 3] = [".d.ts", ".d.mts", ".d.cts"];

const TEST_FILE_MARKERS: [&str; 2] = ["test", "spec"];

const PLUGIN_MANIFEST_NAMES: [&str; 3] = ["package.json", "plugin.json", "manifest.json"];

const PLUGIN_MANIFEST_SUFFIX: &str = ".plugin.json";

pub fn is_source_file(path: &RepoPath) -> bool {
    let file_name = file_name(path);
    let is_declaration_file = DECLARATION_FILE_SUFFIXES
        .iter()
        .any(|suffix| file_name.ends_with(suffix));
    let has_source_extension = file_name
        .rsplit_once('.')
        .is_some_and(|(_, extension)| SOURCE_EXTENSIONS.contains(&extension));

    has_source_extension && !is_declaration_file
}

pub fn is_test_file(path: &RepoPath) -> bool {
    let file_name = file_name(path);
    let test_marker = file_name
        .rsplit_once('.')
        .and_then(|(stem, _)| stem.rsplit_once('.'))
        .map(|(_, marker)| marker);

    is_source_file(path) && test_marker.is_some_and(|marker| TEST_FILE_MARKERS.contains(&marker))
}

pub fn is_module_file(path: &RepoPath) -> bool {
    is_source_file(path) || file_name(path).ends_with(".json")
}

pub fn is_plugin_manifest(path: &RepoPath) -> bool {
    let file_name = file_name(path);

    PLUGIN_MANIFEST_NAMES.contains(&file_name) || file_name.ends_with(PLUGIN_MANIFEST_SUFFIX)
}

fn file_name(path: &RepoPath) -> &str {
    let path = path.as_str();

    path.rsplit_once('/')
        .map_or(path, |(_, file_name)| file_name)
}

#[cfg(test)]
mod tests {
    use prober_core::changes::{ChangeKind, FileChange};
    use prober_core::config::AlwaysAll;

    use super::*;

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    #[test]
    fn source_files_are_typescript_and_javascript_modules_but_not_declarations() {
        let source_files = [
            "src/a.ts",
            "src/a.tsx",
            "a.mts",
            "a.cjs",
            "scripts/build.mjs",
            "ui/App.jsx",
        ];
        let other_files = [
            "src/types.d.ts",
            "src/types.d.mts",
            "README.md",
            "src/data.json",
            "Makefile",
        ];

        for source_file in source_files {
            assert!(is_source_file(&repo_path(source_file)), "{source_file}");
        }

        for other_file in other_files {
            assert!(!is_source_file(&repo_path(other_file)), "{other_file}");
        }
    }

    #[test]
    fn test_files_follow_the_vitest_and_jest_naming() {
        let test_files = [
            "src/a.test.ts",
            "src/a.spec.tsx",
            "test/cli.e2e.test.ts",
            "a.test.mjs",
        ];
        let other_files = [
            "src/test.ts",
            "src/a.test-support.ts",
            "src/a.test.d.ts",
            "src/latest.ts",
            "a.test.json",
        ];

        for test_file in test_files {
            assert!(is_test_file(&repo_path(test_file)), "{test_file}");
        }

        for other_file in other_files {
            assert!(!is_test_file(&repo_path(other_file)), "{other_file}");
        }
    }

    #[test]
    fn plugin_manifests_are_package_plugin_and_manifest_files() {
        let manifests = [
            "extensions/telegram/package.json",
            "plugins/a/plugin.json",
            "plugins/b/manifest.json",
            "extensions/slack/openclaw.plugin.json",
        ];
        let other_files = ["extensions/telegram/index.ts", "extensions/plugin.json.ts"];

        for manifest in manifests {
            assert!(is_plugin_manifest(&repo_path(manifest)), "{manifest}");
        }

        for other_file in other_files {
            assert!(!is_plugin_manifest(&repo_path(other_file)), "{other_file}");
        }
    }

    #[test]
    fn always_all_defaults_match_manifests_lockfiles_and_tool_configs_at_any_depth() {
        let patterns = ALWAYS_ALL_DEFAULTS
            .iter()
            .map(|&pattern| pattern.to_owned())
            .collect();
        let always_all = AlwaysAll::new(patterns).unwrap();
        let changes: Vec<FileChange> = [
            "package.json",
            "extensions/telegram/package.json",
            "pnpm-lock.yaml",
            "tsconfig.base.json",
            "vitest.config.ts",
            "test/vitest.e2e.config.ts",
            "src/a.ts",
            "src/package-helpers.ts",
            "docs/jest-migration.md",
        ]
        .into_iter()
        .map(|path| FileChange {
            path: repo_path(path),
            kind: ChangeKind::Modified,
        })
        .collect();
        let matched_paths: Vec<&str> = always_all
            .matching_changes(&changes)
            .iter()
            .map(|always_all_match| always_all_match.matched_path.as_str())
            .collect();

        assert_eq!(
            matched_paths,
            [
                "extensions/telegram/package.json",
                "package.json",
                "pnpm-lock.yaml",
                "test/vitest.e2e.config.ts",
                "tsconfig.base.json",
                "vitest.config.ts",
            ]
        );
    }
}
