use anyhow::{Result, anyhow};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use xxhash_rust::xxh3::xxh3_64;

use crate::changes::FileChange;
use crate::files::RepoPath;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    always_all: AlwaysAllSection,

    #[serde(default)]
    inference: InferenceSettings,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AlwaysAllSection {
    #[serde(default)]
    files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct InferenceSettings {
    pub dynamic_imports: bool,
    pub plugin_folders: bool,
}

impl Default for InferenceSettings {
    fn default() -> Self {
        Self {
            dynamic_imports: true,
            plugin_folders: true,
        }
    }
}

#[derive(Debug)]
pub struct Config {
    pub always_all: AlwaysAll,
    pub inference: InferenceSettings,
}

impl Config {
    pub fn parse(config_text: &str) -> Result<Self> {
        let config_file: ConfigFile = toml::from_str(config_text)
            .map_err(|toml_error| anyhow!(describe_toml_error(config_text, &toml_error)))?;
        let always_all = AlwaysAll::new(config_file.always_all.files)
            .map_err(|pattern_error| anyhow!("invalid prober.toml: {pattern_error}"))?;

        Ok(Self {
            always_all,
            inference: config_file.inference,
        })
    }
}

fn describe_toml_error(config_text: &str, toml_error: &toml::de::Error) -> String {
    let Some(error_span) = toml_error.span() else {
        return format!("invalid prober.toml: {}", toml_error.message());
    };
    let (line, column) = line_and_column(config_text, error_span.start);

    format!(
        "invalid prober.toml at line {line}, column {column}: {}",
        toml_error.message()
    )
}

fn line_and_column(text: &str, byte_offset: usize) -> (usize, usize) {
    let text_before_offset = text.get(..byte_offset).unwrap_or(text);
    let line = text_before_offset.matches('\n').count() + 1;
    let line_start = text_before_offset
        .rfind('\n')
        .map_or(0, |newline_offset| newline_offset + 1);
    let column = text_before_offset[line_start..].chars().count() + 1;

    (line, column)
}

pub fn config_hash(config_text: &str) -> String {
    format!("{:016x}", xxh3_64(config_text.as_bytes()))
}

#[derive(Debug)]
pub struct AlwaysAll {
    patterns: Vec<String>,
    matcher: GlobSet,
}

impl AlwaysAll {
    pub fn new(patterns: Vec<String>) -> Result<Self> {
        let mut matcher_builder = GlobSetBuilder::new();

        for pattern in &patterns {
            let glob = GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map_err(|glob_error| {
                    anyhow!(
                        "invalid always_all pattern {pattern:?}: {}",
                        glob_error.kind()
                    )
                })?;

            matcher_builder.add(glob);
        }

        let matcher = matcher_builder
            .build()
            .map_err(|glob_error| anyhow!("cannot compile always_all patterns: {glob_error}"))?;

        Ok(Self { patterns, matcher })
    }

    pub fn matching_changes<'a>(&'a self, changes: &'a [FileChange]) -> Vec<AlwaysAllMatch<'a>> {
        let mut always_all_matches: Vec<AlwaysAllMatch> = changes
            .iter()
            .filter_map(|change| self.first_match_of(change))
            .collect();

        always_all_matches.sort_by(|left, right| left.change.path.cmp(&right.change.path));

        always_all_matches
    }

    fn first_match_of<'a>(&'a self, change: &'a FileChange) -> Option<AlwaysAllMatch<'a>> {
        change.touched_paths().find_map(|touched_path| {
            let first_matching_pattern = self
                .matcher
                .matches(touched_path.as_str())
                .into_iter()
                .min()?;

            Some(AlwaysAllMatch {
                change,
                matched_path: touched_path,
                pattern: &self.patterns[first_matching_pattern],
            })
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlwaysAllMatch<'a> {
    pub change: &'a FileChange,
    pub matched_path: &'a RepoPath,
    pub pattern: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::ChangeKind;

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn modified(path: &str) -> FileChange {
        FileChange {
            path: repo_path(path),
            kind: ChangeKind::Modified,
        }
    }

    fn always_all_config(patterns: &str) -> Config {
        Config::parse(&format!("[always_all]\nfiles = {patterns}\n")).unwrap()
    }

    fn matched_paths<'a>(always_all_matches: &[AlwaysAllMatch<'a>]) -> Vec<&'a str> {
        always_all_matches
            .iter()
            .map(|always_all_match| always_all_match.matched_path.as_str())
            .collect()
    }

    #[test]
    fn missing_config_runs_nothing_globally_and_enables_every_inference() {
        let config = Config::parse("").unwrap();

        assert_eq!(
            config
                .always_all
                .matching_changes(&[modified("ci/deploy.sh")]),
            []
        );
        assert_eq!(config.inference, InferenceSettings::default());
        assert!(config.inference.dynamic_imports && config.inference.plugin_folders);
    }

    #[test]
    fn each_inference_switches_off_on_its_own() {
        let config = Config::parse("[inference]\nplugin_folders = false\n").unwrap();

        assert_eq!(
            config.inference,
            InferenceSettings {
                dynamic_imports: true,
                plugin_folders: false,
            }
        );
    }

    #[test]
    fn change_matching_an_always_all_pattern_is_reported_with_the_pattern() {
        let config = always_all_config(r#"["scripts/release/**", "ci/**"]"#);
        let changes = [modified("src/a.ts"), modified("ci/nightly/deploy.sh")];
        let always_all_matches = config.always_all.matching_changes(&changes);

        assert_eq!(
            always_all_matches,
            [AlwaysAllMatch {
                change: &changes[1],
                matched_path: &changes[1].path,
                pattern: "ci/**",
            }]
        );
    }

    #[test]
    fn single_star_does_not_cross_directories() {
        let config = always_all_config(r#"["scripts/*.sh"]"#);
        let changes = [
            modified("scripts/release/publish.sh"),
            modified("scripts/publish.sh"),
        ];

        assert_eq!(
            matched_paths(&config.always_all.matching_changes(&changes)),
            ["scripts/publish.sh"]
        );
    }

    #[test]
    fn file_renamed_out_of_an_always_all_folder_matches_on_its_old_path() {
        let config = always_all_config(r#"["ci/**"]"#);
        let moved_out_of_ci = [FileChange {
            path: repo_path("tools/deploy.sh"),
            kind: ChangeKind::Renamed {
                from: repo_path("ci/deploy.sh"),
            },
        }];

        assert_eq!(
            matched_paths(&config.always_all.matching_changes(&moved_out_of_ci)),
            ["ci/deploy.sh"]
        );
    }

    #[test]
    fn every_matching_change_is_listed_by_path_whatever_the_change_order() {
        let config = always_all_config(r#"["ci/**"]"#);
        let changes = [
            modified("ci/z.sh"),
            modified("src/a.ts"),
            modified("ci/a.sh"),
        ];

        assert_eq!(
            matched_paths(&config.always_all.matching_changes(&changes)),
            ["ci/a.sh", "ci/z.sh"]
        );
    }

    #[test]
    fn every_config_error_is_one_line_with_its_position() {
        let invalid_configs = [
            (
                "[always_all\nfiles = []\n",
                "invalid prober.toml at line 1, column 12: unclosed table, expected `]`",
            ),
            (
                "[always_all]\nfiles = \"ci/**\"\n",
                "invalid prober.toml at line 2, column 9: invalid type: string \"ci/**\", expected a sequence",
            ),
            (
                "presets = \"auto\"\n",
                "invalid prober.toml at line 1, column 1: unknown field `presets`, expected `always_all` or `inference`",
            ),
            (
                "[inference]\nanchors = false\n",
                "invalid prober.toml at line 2, column 1: unknown field `anchors`, expected `dynamic_imports` or `plugin_folders`",
            ),
            (
                "[always_all]\ntests = [\"a.test.ts\"]\n",
                "invalid prober.toml at line 2, column 1: unknown field `tests`, expected `files`",
            ),
            (
                "[always_all]\nfiles = [\"ci/[unclosed\"]\n",
                "invalid prober.toml: invalid always_all pattern \"ci/[unclosed\": unclosed character class; missing ']'",
            ),
        ];

        for (invalid_config, expected_message) in invalid_configs {
            let error = Config::parse(invalid_config).unwrap_err();

            assert_eq!(error.to_string(), expected_message);
            assert_eq!(format!("{error:#}"), expected_message);
        }
    }

    #[test]
    fn column_counts_characters_not_bytes() {
        assert_eq!(line_and_column("files = \"é\"", 11), (1, 11));
        assert_eq!(line_and_column("a = 1\nbé = 2", 10), (2, 4));
    }

    #[test]
    fn config_hash_depends_only_on_the_config_text() {
        let config_text = "[always_all]\nfiles = [\"ci/**\"]\n";

        assert_eq!(config_hash(config_text), config_hash(config_text));
        assert_ne!(config_hash(config_text), config_hash(""));
        assert_eq!(config_hash(config_text).len(), 16);
    }
}
