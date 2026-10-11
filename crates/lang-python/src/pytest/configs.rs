//! pytest configurations: where they are, the tests they discover, the plugins and fixtures they add to every test, and which of their changes matter.

use std::collections::BTreeMap;

use globset::{Glob, GlobMatcher};
use prober_core::files::RepoPath;
use toml::Value;

use crate::conventions::{join_directory, split_path};

const PYTEST_INI: &str = "pytest.ini";

const PROJECT_MANIFEST: &str = "pyproject.toml";

const TOX_INI: &str = "tox.ini";

const SETUP_CFG: &str = "setup.cfg";

const CONFIG_FILES: [&str; 4] = [PYTEST_INI, PROJECT_MANIFEST, TOX_INI, SETUP_CFG];

const DEFAULT_PYTHON_FILES: &str = "test_*.py *_test.py";

const DEFAULT_NORECURSEDIRS: &str = "*.egg .* _darcs build CVS dist node_modules venv {arch}";

pub(crate) const DISCOVERY_OPTIONS: [&str; 3] = ["python_files", "testpaths", "norecursedirs"];

const OPTIONS_WITHOUT_EFFECT: &[&str] = &[
    "cache_dir",
    "console_output_style",
    "junit_duration_report",
    "junit_family",
    "junit_log_passing_tests",
    "junit_logging",
    "junit_suite_name",
    "log_cli",
    "log_cli_date_format",
    "log_cli_format",
    "log_cli_level",
    "log_file",
    "log_file_date_format",
    "log_file_format",
    "log_file_level",
    "truncation_limit_chars",
    "truncation_limit_lines",
    "verbosity_assertions",
    "verbosity_test_cases",
];

pub(crate) const SETTINGS_OPTION: &str = "DJANGO_SETTINGS_MODULE";

pub(crate) type Options = BTreeMap<String, String>;

#[derive(Debug, Clone)]
pub(crate) struct PytestConfig {
    pub path: RepoPath,
    pub directory: String,
    pub options: Options,
}

#[derive(Debug)]
pub(crate) struct Discovery {
    directory: String,
    python_files: Vec<GlobMatcher>,
    testpaths: Vec<GlobMatcher>,
    norecursedirs: Vec<GlobMatcher>,
}

#[derive(Debug, Default)]
pub(crate) struct PytestConfigs {
    configs: Vec<PytestConfig>,
    discoveries: Vec<Discovery>,
    default_discovery: Option<Discovery>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct OptionChanges {
    pub discovery: bool,
    pub effects: Vec<String>,
}

impl PytestConfigs {
    pub fn read(repo_files: &[RepoPath], read: &dyn Fn(&RepoPath) -> Option<String>) -> Self {
        let mut candidates: BTreeMap<&str, Vec<&RepoPath>> = BTreeMap::new();

        for path in repo_files {
            let (directory, name) = split_path(path.as_str());

            if CONFIG_FILES.contains(&name) {
                candidates.entry(directory).or_default().push(path);
            }
        }

        let mut configs = Vec::new();

        for (directory, mut paths) in candidates {
            paths.sort_by_key(|path| {
                CONFIG_FILES
                    .iter()
                    .position(|name| *name == split_path(path.as_str()).1)
            });

            let found = paths.into_iter().find_map(|path| {
                let options = config_options(path, &read(path)?)?;

                Some(PytestConfig {
                    path: path.clone(),
                    directory: directory.to_owned(),
                    options,
                })
            });

            configs.extend(found);
        }

        configs.sort_by_key(|config| std::cmp::Reverse(config.directory.len()));

        let discoveries = configs
            .iter()
            .map(|config| Discovery::new(&config.directory, &config.options))
            .collect();

        Self {
            configs,
            discoveries,
            default_discovery: Some(Discovery::new("", &Options::new())),
        }
    }

    pub fn configs(&self) -> &[PytestConfig] {
        &self.configs
    }

    pub fn config_for(&self, path: &str) -> Option<&PytestConfig> {
        self.configs
            .iter()
            .find(|config| is_within(path, &config.directory))
    }

    pub fn is_test(&self, path: &RepoPath) -> bool {
        let discovery = self
            .discoveries
            .iter()
            .find(|discovery| is_within(path.as_str(), &discovery.directory))
            .or(self.default_discovery.as_ref());

        discovery.is_some_and(|discovery| discovery.discovers(path.as_str()))
    }

    pub fn plugins(&self, config: &PytestConfig) -> Vec<String> {
        command_line_values(config, &["-p"], &[])
            .into_iter()
            .filter(|plugin| !plugin.starts_with("no:"))
            .collect()
    }

    pub fn settings_module(&self, config: &PytestConfig) -> Option<String> {
        config
            .options
            .get(SETTINGS_OPTION)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                command_line_values(config, &[], &["--ds"])
                    .into_iter()
                    .next()
            })
    }

    pub fn used_fixtures(config: &PytestConfig) -> Vec<String> {
        config
            .options
            .get("usefixtures")
            .map(|value| value.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default()
    }
}

impl Discovery {
    pub(crate) fn new(directory: &str, options: &Options) -> Self {
        let matchers = |key: &str, default: &str| -> Vec<GlobMatcher> {
            options
                .get(key)
                .map_or(default, String::as_str)
                .split_whitespace()
                .filter_map(|pattern| Glob::new(pattern).ok())
                .map(|glob| glob.compile_matcher())
                .collect()
        };
        let testpaths = options
            .get("testpaths")
            .map(|value| {
                value
                    .split_whitespace()
                    .map(|testpath| join_directory(directory, testpath.trim_end_matches('/')))
                    .flat_map(|testpath| [testpath.clone(), format!("{testpath}/**")])
                    .filter_map(|pattern| Glob::new(&pattern).ok())
                    .map(|glob| glob.compile_matcher())
                    .collect()
            })
            .unwrap_or_default();

        Self {
            directory: directory.to_owned(),
            python_files: matchers("python_files", DEFAULT_PYTHON_FILES),
            testpaths,
            norecursedirs: matchers("norecursedirs", DEFAULT_NORECURSEDIRS),
        }
    }

    pub(crate) fn discovers(&self, path: &str) -> bool {
        let (directory, name) = split_path(path);
        let relative_directory = directory
            .strip_prefix(&self.directory)
            .unwrap_or(directory)
            .trim_start_matches('/');
        let in_skipped_directory = relative_directory
            .split('/')
            .filter(|segment| !segment.is_empty())
            .any(|segment| {
                self.norecursedirs
                    .iter()
                    .any(|matcher| matcher.is_match(segment))
            });
        let in_testpaths = self.testpaths.is_empty()
            || self.testpaths.iter().any(|matcher| matcher.is_match(path));

        name.ends_with(".py")
            && self
                .python_files
                .iter()
                .any(|matcher| matcher.is_match(name))
            && in_testpaths
            && !in_skipped_directory
    }
}

pub(crate) fn is_config_file(path: &RepoPath) -> bool {
    CONFIG_FILES.contains(&split_path(path.as_str()).1)
}

pub(crate) fn config_options(path: &RepoPath, text: &str) -> Option<Options> {
    match split_path(path.as_str()).1 {
        PYTEST_INI => Some(ini_section(text, &["pytest", "tool:pytest"]).unwrap_or_default()),
        PROJECT_MANIFEST => toml_options(text),
        TOX_INI => ini_section(text, &["pytest"]),
        SETUP_CFG => ini_section(text, &["tool:pytest"]),
        _ => None,
    }
}

pub(crate) fn option_changes(base: &Options, head: &Options) -> OptionChanges {
    let mut changes = OptionChanges::default();
    let keys: std::collections::BTreeSet<&String> = base.keys().chain(head.keys()).collect();

    for key in keys {
        if base.get(key) == head.get(key) || OPTIONS_WITHOUT_EFFECT.contains(&key.as_str()) {
            continue;
        }

        if DISCOVERY_OPTIONS.contains(&key.as_str()) {
            changes.discovery = true;
        } else {
            changes.effects.push(key.clone());
        }
    }

    changes
}

pub(crate) fn sections_without_pytest(path: &RepoPath, text: &str) -> BTreeMap<String, String> {
    match split_path(path.as_str()).1 {
        TOX_INI | SETUP_CFG => ini_sections(text)
            .into_iter()
            .filter(|(section, _)| section != "pytest" && section != "tool:pytest")
            .map(|(section, options)| (section, format!("{options:?}")))
            .collect(),
        _ => BTreeMap::new(),
    }
}

fn toml_options(text: &str) -> Option<Options> {
    let document: toml::Table = toml::from_str(text).ok()?;
    let pytest = document.get("tool")?.get("pytest")?.as_table()?;
    let mut options = Options::new();

    for (key, value) in pytest {
        if key == "ini_options" {
            for (key, value) in value.as_table()? {
                options.insert(key.clone(), toml_value(value));
            }
        } else {
            options.insert(key.clone(), toml_value(value));
        }
    }

    Some(options)
}

fn toml_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(toml_value).collect::<Vec<_>>().join("\n"),
        value => value.to_string(),
    }
}

fn ini_section(text: &str, names: &[&str]) -> Option<Options> {
    ini_sections(text)
        .into_iter()
        .find(|(section, _)| names.contains(&section.as_str()))
        .map(|(_, options)| options)
}

fn ini_sections(text: &str) -> Vec<(String, Options)> {
    let mut sections: Vec<(String, Options)> = Vec::new();
    let mut last_key: Option<String> = None;

    for line in text.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with(['#', ';']) {
            continue;
        }

        if let Some(name) = trimmed
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            sections.push((name.trim().to_owned(), Options::new()));
            last_key = None;

            continue;
        }

        let Some((_, options)) = sections.last_mut() else {
            continue;
        };

        if line.starts_with([' ', '\t'])
            && let Some(key) = &last_key
        {
            let value = options.entry(key.clone()).or_default();

            if !value.is_empty() {
                value.push('\n');
            }

            value.push_str(trimmed);

            continue;
        }

        if let Some((key, value)) = trimmed.split_once(['=', ':']) {
            let key = key.trim().to_owned();

            options.insert(key.clone(), value.trim().to_owned());
            last_key = Some(key);
        }
    }

    sections
}

fn command_line_values(config: &PytestConfig, short: &[&str], long: &[&str]) -> Vec<String> {
    let Some(addopts) = config.options.get("addopts") else {
        return Vec::new();
    };
    let arguments: Vec<&str> = addopts
        .split_whitespace()
        .map(|argument| argument.trim_matches(['"', '\'']))
        .collect();
    let mut values = Vec::new();

    for (position, argument) in arguments.iter().enumerate() {
        for flag in short.iter().chain(long) {
            if argument == flag {
                values.extend(arguments.get(position + 1).map(|value| (*value).to_owned()));
            } else if let Some(value) = argument.strip_prefix(&format!("{flag}=")) {
                values.push(value.to_owned());
            } else if short.contains(flag)
                && let Some(value) = argument.strip_prefix(flag)
                && !value.is_empty()
                && !value.starts_with('-')
            {
                values.push(value.to_owned());
            }
        }
    }

    values
}

pub(crate) fn is_within(path: &str, directory: &str) -> bool {
    directory.is_empty() || path.starts_with(directory) && path[directory.len()..].starts_with('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn configs(files: &[(&str, &str)], others: &[&str]) -> PytestConfigs {
        let repo_files: Vec<RepoPath> = files
            .iter()
            .map(|(path, _)| *path)
            .chain(others.iter().copied())
            .map(repo_path)
            .collect();
        let read = |path: &RepoPath| {
            files
                .iter()
                .find(|(file, _)| *file == path.as_str())
                .map(|(_, text)| (*text).to_owned())
        };

        PytestConfigs::read(&repo_files, &read)
    }

    #[test]
    fn the_first_config_file_holding_pytest_options_wins_in_each_directory() {
        let configs = configs(
            &[
                ("setup.cfg", "[tool:pytest]\npython_files = check_*.py\n"),
                ("tox.ini", "[tox]\nenvlist = py312\n"),
                ("pyproject.toml", "[project]\nname = \"app\"\n"),
                (
                    "sub/pytest.ini",
                    "[pytest]\naddopts = -p app.plugin --ds=app.settings\n",
                ),
            ],
            &[],
        );
        let root = configs.config_for("tests/test_a.py").unwrap();
        let sub = configs.config_for("sub/tests/test_a.py").unwrap();

        assert_eq!(root.path.as_str(), "setup.cfg");
        assert_eq!(sub.path.as_str(), "sub/pytest.ini");
        assert_eq!(configs.plugins(sub), ["app.plugin"]);
        assert_eq!(
            configs.settings_module(sub).as_deref(),
            Some("app.settings")
        );
        assert!(configs.is_test(&repo_path("tests/check_api.py")));
        assert!(!configs.is_test(&repo_path("tests/test_api.py")));
        assert!(configs.is_test(&repo_path("sub/tests/test_api.py")));
    }

    #[test]
    fn testpaths_and_norecursedirs_limit_discovery() {
        let configs = configs(
            &[(
                "pyproject.toml",
                "[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\nnorecursedirs = [\"testing_config\"]\n",
            )],
            &[],
        );

        assert!(configs.is_test(&repo_path("tests/unit/test_a.py")));
        assert!(!configs.is_test(&repo_path("scripts/test_a.py")));
        assert!(!configs.is_test(&repo_path("tests/testing_config/test_a.py")));
    }

    #[test]
    fn option_changes_tell_discovery_from_effects_and_ignore_output_settings() {
        let base = Options::from([
            ("addopts".to_owned(), "-x".to_owned()),
            ("log_cli".to_owned(), "true".to_owned()),
        ]);
        let head = Options::from([
            ("addopts".to_owned(), "-x -q".to_owned()),
            ("log_cli".to_owned(), "false".to_owned()),
            ("python_files".to_owned(), "check_*.py".to_owned()),
        ]);

        assert_eq!(
            option_changes(&base, &head),
            OptionChanges {
                discovery: true,
                effects: vec!["addopts".to_owned()],
            }
        );
    }

    #[test]
    fn ini_values_continue_on_indented_lines() {
        let options = ini_section(
            "[pytest]\naddopts =\n    -p app.plugin\n    --strict-markers\nmarkers: slow\n",
            &["pytest"],
        )
        .unwrap();

        assert_eq!(options["addopts"], "-p app.plugin\n--strict-markers");
        assert_eq!(options["markers"], "slow");
    }
}
