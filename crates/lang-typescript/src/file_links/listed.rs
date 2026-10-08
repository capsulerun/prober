//! Paths a data or text file lists: string values of JSON and YAML documents, and path-like words of any other text.

use std::collections::{HashMap, HashSet};

use prober_core::files::RepoPath;

use crate::conventions::{is_asset_file, is_source_file};
use crate::imports::is_path_like;
use crate::manifests::documents::{parse_jsonc, parse_yaml};
use crate::manifests::is_manifest;

const JSON_EXTENSIONS: [&str; 3] = ["json", "jsonc", "json5"];

const YAML_EXTENSIONS: [&str; 2] = ["yaml", "yml"];

const STYLESHEET_EXTENSIONS: [&str; 9] = [
    "css", "less", "pcss", "postcss", "sass", "scss", "sss", "styl", "stylus",
];

const SKIPPED_EXTENSIONS: &[&str] = &[
    "7z", "a", "adoc", "asciidoc", "bin", "bz2", "class", "db", "dll", "dylib", "exe", "gz", "jar",
    "lock", "log", "map", "markdown", "md", "mdx", "node", "o", "pdf", "pyc", "rst", "snap", "so",
    "sqlite", "tar", "tgz", "wasm", "xz", "zip",
];

const DECLARATION_FILE_SUFFIXES: [&str; 3] = [".d.ts", ".d.mts", ".d.cts"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListedPath {
    pub line: u32,
    pub text: String,
}

pub(crate) fn lists_paths(path: &RepoPath) -> bool {
    let file_name = path.as_str().rsplit('/').next().unwrap_or_default();
    let extension = file_name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    let is_binary_asset =
        is_asset_file(path) && !STYLESHEET_EXTENSIONS.contains(&extension.as_str());
    let is_declaration_file = DECLARATION_FILE_SUFFIXES
        .iter()
        .any(|suffix| file_name.ends_with(suffix));

    !is_source_file(path)
        && !is_declaration_file
        && !is_binary_asset
        && !SKIPPED_EXTENSIONS.contains(&extension.as_str())
        && !is_manifest(path)
}

pub(crate) fn is_stylesheet(path: &RepoPath) -> bool {
    path.as_str()
        .rsplit_once('.')
        .is_some_and(|(_, extension)| {
            STYLESHEET_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
}

pub(crate) fn listed_paths(
    path: &RepoPath,
    text: &str,
    file_extensions: &HashSet<&str>,
) -> Vec<ListedPath> {
    let is_listed_path = |word: &str| {
        is_path_like(word)
            && (word.contains('/')
                || word
                    .rsplit_once('.')
                    .is_some_and(|(_, extension)| file_extensions.contains(extension)))
    };
    let extension = path
        .as_str()
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    let document = if JSON_EXTENSIONS.contains(&extension.as_str()) {
        parse_jsonc(text).ok()
    } else if YAML_EXTENSIONS.contains(&extension.as_str()) {
        parse_yaml(text).ok()
    } else {
        None
    };
    let document_values: Option<HashSet<String>> = document.map(|document| {
        document_strings(&document)
            .into_iter()
            .filter(|value| is_listed_path(value))
            .collect()
    });

    if document_values.as_ref().is_some_and(HashSet::is_empty) {
        return Vec::new();
    }

    let line_starts = line_starts(text);
    let line_of = |offset: usize| {
        let line_index = line_starts.partition_point(|&line_start| line_start <= offset);

        u32::try_from(line_index).unwrap_or(u32::MAX)
    };
    let mut first_offsets: HashMap<&str, usize> = HashMap::new();

    for word in text.split(is_word_delimiter) {
        let word = word.trim_end_matches('.');

        if is_listed_path(word) {
            let offset = word.as_ptr() as usize - text.as_ptr() as usize;

            first_offsets.entry(word).or_insert(offset);
        }
    }

    let mut listed: Vec<ListedPath> = match document_values {
        Some(values) => values
            .into_iter()
            .map(|value| ListedPath {
                line: first_offsets
                    .get(value.as_str())
                    .map_or(1, |&offset| line_of(offset)),
                text: value,
            })
            .collect(),
        None => first_offsets
            .into_iter()
            .map(|(word, offset)| ListedPath {
                line: line_of(offset),
                text: word.to_owned(),
            })
            .collect(),
    };

    listed.sort_by(|left, right| left.line.cmp(&right.line).then(left.text.cmp(&right.text)));

    listed
}

fn is_word_delimiter(character: char) -> bool {
    character.is_whitespace()
        || matches!(
            character,
            '"' | '\''
                | '`'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | ','
                | ';'
                | '='
                | '|'
                | '&'
                | '!'
                | ':'
                | '#'
                | '$'
                | '\\'
        )
}

pub(crate) fn document_strings(document: &serde_json::Value) -> Vec<String> {
    match document {
        serde_json::Value::String(text) => vec![text.clone()],
        serde_json::Value::Array(items) => items.iter().flat_map(document_strings).collect(),
        serde_json::Value::Object(properties) => {
            properties.values().flat_map(document_strings).collect()
        }
        _ => Vec::new(),
    }
}

fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(
            text.match_indices('\n')
                .map(|(newline_offset, _)| newline_offset + 1),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn listed(path: &str, text: &str) -> Vec<(u32, String)> {
        let file_extensions = HashSet::from(["css", "json", "mjs", "png", "sh", "ts", "yml"]);

        listed_paths(&repo_path(path), text, &file_extensions)
            .into_iter()
            .map(|listed_path| (listed_path.line, listed_path.text))
            .collect()
    }

    fn at(line: u32, text: &str) -> (u32, String) {
        (line, text.to_owned())
    }

    #[test]
    fn json_and_yaml_list_their_string_values_but_not_their_keys() {
        let manifest = r#"{
  "name": "@acme/telegram",
  "openclaw": { "extensions": ["./index.ts"], "setupEntry": "./setup.ts" },
  "files": ["dist", "openclaw.mjs"],
  "scripts": { "build": "node scripts/build.mjs" },
  "timings": { "src/a.test.ts": 12 }
}"#;
        let workflow = "jobs:\n  test:\n    steps:\n      - run: pnpm test\n        working-directory: packages/app\n      - uses: ./.github/actions/setup.yml\n";

        assert_eq!(
            listed("extensions/telegram/package.json", manifest),
            [
                at(2, "@acme/telegram"),
                at(3, "./index.ts"),
                at(3, "./setup.ts"),
                at(4, "openclaw.mjs")
            ]
        );
        assert_eq!(
            listed(".github/workflows/ci.yml", workflow),
            [at(5, "packages/app"), at(6, "./.github/actions/setup.yml")]
        );
    }

    #[test]
    fn other_text_lists_its_path_like_words() {
        let components = "cli-root-options.mjs\npackages/acp-core/package.json\n\nREADME\n";
        let script = "#!/usr/bin/env bash\nsource \"$(dirname \"$0\")/lib/common.sh\"\nnode scripts/run.mjs --config=test/vitest.config.ts.\n";
        let stylesheet =
            "@import \"./theme.css\";\nbody { background: url(../assets/logo.png); }\n";

        assert_eq!(
            listed("scripts/pr-lib/components.txt", components),
            [
                at(1, "cli-root-options.mjs"),
                at(2, "packages/acp-core/package.json")
            ]
        );
        assert_eq!(
            listed("scripts/pr", script),
            [
                at(1, "/usr/bin/env"),
                at(2, "/lib/common.sh"),
                at(3, "scripts/run.mjs"),
                at(3, "test/vitest.config.ts")
            ]
        );
        assert_eq!(
            listed("src/app.css", stylesheet),
            [at(1, "./theme.css"), at(2, "../assets/logo.png")]
        );
    }

    #[test]
    fn sources_prose_binaries_and_manifests_list_nothing() {
        for path in [
            "src/a.ts",
            "src/types.d.ts",
            "docs/guide.md",
            "assets/logo.png",
            "pnpm-lock.yaml",
            "yarn.lock",
            "packages/core/package.json",
            "tsconfig.build.json",
            "src/__snapshots__/a.test.ts.snap",
        ] {
            assert!(!lists_paths(&repo_path(path)), "{path}");
        }

        for path in [
            "scripts/pr-lib/components.txt",
            "extensions/telegram/openclaw.plugin.json",
            "src/app.css",
            "scripts/pr",
            "android/app/src/test/ContractTest.kt",
        ] {
            assert!(lists_paths(&repo_path(path)), "{path}");
        }
    }
}
