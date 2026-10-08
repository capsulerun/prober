//! TypeScript conventions: which files are parsed, which are tests, which run them, and which changes run every test.

use prober_core::files::RepoPath;

pub const LANGUAGE: &str = "typescript";

pub const ALWAYS_ALL_DEFAULTS: &[&str] = &[
    "**/.yarnrc.yml",
    "**/bun.lock",
    "**/bun.lockb",
    "**/.npmrc",
    "**/.nvmrc",
    "**/.node-version",
    "**/babel.config.*",
    "**/.babelrc*",
    "**/.swcrc",
    "**/playwright.config.*",
    "**/cypress.config.*",
    "**/.mocharc*",
];

pub const MANIFEST_DEFAULTS: &[&str] = &[
    "**/package.json",
    "**/package-lock.json",
    "**/npm-shrinkwrap.json",
    "**/pnpm-lock.yaml",
    "**/pnpm-workspace.yaml",
    "**/yarn.lock",
    "**/tsconfig*.json",
    "**/jsconfig*.json",
];

const RUNNER_CONFIG_PREFIXES: [&str; 3] = ["vitest.", "vite.config.", "jest.config."];

const TEST_RUNNER_PACKAGES: &[&str] = &[
    "@playwright/test",
    "@swc/jest",
    "@vitest/browser",
    "@vitest/ui",
    "@vitest/web-worker",
    "ava",
    "babel-jest",
    "cypress",
    "jasmine",
    "jasmine-core",
    "jest",
    "karma",
    "mocha",
    "playwright",
    "playwright-core",
    "tap",
    "ts-jest",
    "ts-node",
    "tsx",
    "uvu",
    "vite",
    "vite-node",
    "vitest",
];

const TEST_RUNNER_PACKAGE_PREFIXES: [&str; 4] = ["@jest/", "@vitest/coverage-", "jest-", "karma-"];

const SOURCE_EXTENSIONS: [&str; 8] = ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

const DECLARATION_FILE_SUFFIXES: [&str; 3] = [".d.ts", ".d.mts", ".d.cts"];

const ASSET_EXTENSIONS: &[&str] = &[
    "aac", "apng", "avif", "bmp", "css", "eot", "flac", "gif", "ico", "jpeg", "jpg", "less", "m4a",
    "mov", "mp3", "mp4", "ogg", "otf", "pcss", "png", "postcss", "sass", "scss", "sss", "styl",
    "stylus", "svg", "tiff", "ttf", "wav", "webm", "webp", "woff", "woff2",
];

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

pub fn source_stem(path: &RepoPath) -> Option<&str> {
    is_source_file(path)
        .then(|| path.as_str().rsplit_once('.').map(|(stem, _)| stem))
        .flatten()
}

pub fn is_asset_file(path: &RepoPath) -> bool {
    file_name(path)
        .rsplit_once('.')
        .is_some_and(|(_, extension)| {
            ASSET_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
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

pub fn is_runner_config(path: &RepoPath) -> bool {
    let file_name = file_name(path);

    RUNNER_CONFIG_PREFIXES
        .iter()
        .any(|prefix| file_name.starts_with(prefix))
}

pub fn is_test_runner_package(package_name: &str) -> bool {
    TEST_RUNNER_PACKAGES.contains(&package_name)
        || TEST_RUNNER_PACKAGE_PREFIXES
            .iter()
            .any(|prefix| package_name.starts_with(prefix))
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
    fn always_all_defaults_match_tool_configs_at_any_depth_but_not_diffed_manifests() {
        let always_all = |patterns: &[&str]| {
            AlwaysAll::new(patterns.iter().map(|&pattern| pattern.to_owned()).collect()).unwrap()
        };
        let changes: Vec<FileChange> = [
            "package.json",
            "extensions/telegram/package.json",
            "pnpm-lock.yaml",
            "tsconfig.base.json",
            "patches/lodash@4.17.21.patch",
            "vitest.config.ts",
            "test/vitest.e2e.config.ts",
            "e2e/playwright.config.ts",
            ".npmrc",
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
        let matched_paths = |always_all: &AlwaysAll| -> Vec<String> {
            always_all
                .matching_changes(&changes)
                .iter()
                .map(|always_all_match| always_all_match.matched_path.as_str().to_owned())
                .collect()
        };

        assert_eq!(
            matched_paths(&always_all(ALWAYS_ALL_DEFAULTS)),
            [".npmrc", "e2e/playwright.config.ts"]
        );
        assert_eq!(
            matched_paths(&always_all(MANIFEST_DEFAULTS)),
            [
                "extensions/telegram/package.json",
                "package.json",
                "pnpm-lock.yaml",
                "tsconfig.base.json",
            ]
        );
    }

    #[test]
    fn runner_configs_and_test_runner_packages_are_recognised_by_name() {
        for runner_config in [
            "vitest.config.ts",
            "test/vitest.unit-paths.mjs",
            "jest.config.js",
            "app/vite.config.mts",
        ] {
            assert!(
                is_runner_config(&repo_path(runner_config)),
                "{runner_config}"
            );
        }

        for other_file in ["src/vitest-helpers.ts", "vite.ts", "jest.setup.js"] {
            assert!(!is_runner_config(&repo_path(other_file)), "{other_file}");
        }

        for runner_package in [
            "vitest",
            "@vitest/coverage-v8",
            "jest-environment-jsdom",
            "@jest/globals",
            "vite",
        ] {
            assert!(is_test_runner_package(runner_package), "{runner_package}");
        }

        for other_package in [
            "lodash",
            "vue",
            "@vue/shared",
            "jestful",
            "@vitest/eslint-plugin",
        ] {
            assert!(!is_test_runner_package(other_package), "{other_package}");
        }
    }

    #[test]
    fn package_lists_are_sorted_so_they_stay_easy_to_review() {
        assert!(TEST_RUNNER_PACKAGES.is_sorted());
    }
}
