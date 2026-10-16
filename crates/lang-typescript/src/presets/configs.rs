//! Runner configs read from their evaluated values: the tests each project runs, and whether those tests render assets.

use prober_core::files::RepoPath;
use serde_json::Value as JsonValue;

use crate::dynamic::evaluator::Evaluator;
use crate::dynamic::value::{Part, Pattern, Value, is_open_object, merge_config, property};
use crate::presets::patterns::{TestPattern, join_directory, relative_globs};
use crate::presets::{
    JEST_DEFAULT_TEST_MATCH, RSTEST_DEFAULT_EXCLUDE, RSTEST_DEFAULT_INCLUDE,
    VITEST_DEFAULT_EXCLUDE, VITEST_DEFAULT_INCLUDE, is_jest_config, is_rstest_config,
    is_vitest_config,
};
use crate::workspaces::parent_directory;

const JEST_DEFAULT_IGNORED_PATHS: &str = "/node_modules/";

const RSTACK_CONFIG_PREFIX: &str = "rstack.config.";

const MOCHA_DEFAULT_EXTENSIONS: [&str; 3] = ["js", "cjs", "mjs"];

#[derive(Debug, Clone)]
pub(crate) struct ProjectReading {
    pub tests: Option<TestPattern>,
    pub renders_assets: Option<bool>,
}

impl ProjectReading {
    fn unknown() -> Self {
        Self {
            tests: None,
            renders_assets: None,
        }
    }
}

enum Field {
    Absent,
    Known(Value),
    Unknown,
}

fn field(object: &Value, key: &str) -> Field {
    match object {
        Value::Object(_) => match property(object, key) {
            Some(Value::Unknown(_)) => Field::Unknown,
            Some(value) => Field::Known(value),
            None if is_open_object(object) => Field::Unknown,
            None => Field::Absent,
        },
        _ => Field::Unknown,
    }
}

fn nested_field(object: &Value, keys: &[&str]) -> Field {
    let mut current = object.clone();

    for key in keys {
        match field(&current, key) {
            Field::Known(value) => current = value,
            other => return other,
        }
    }

    Field::Known(current)
}

fn config_path(value: &Value, directory: &str) -> Option<String> {
    let Value::Strings(patterns) = value else {
        return None;
    };
    let [pattern] = patterns.as_slice() else {
        return None;
    };

    match pattern.parts() {
        [Part::RepoRoot] => Some(String::new()),
        [Part::RepoRoot, Part::Text(path)] => Some(path.trim_start_matches('/').to_owned()),
        _ => {
            let literal = pattern.literal_text()?;
            let literal = literal.strip_prefix("<rootDir>/").unwrap_or(&literal);

            join_directory(directory, literal)
        }
    }
}

fn literal_texts(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Array(items) => items.iter().map(Value::single_literal).collect(),
        value => Some(vec![value.single_literal()?]),
    }
}

fn possible_texts(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Array(items) | Value::Alternatives(items) => Some(
            items
                .iter()
                .map(possible_texts)
                .collect::<Option<Vec<_>>>()?
                .concat(),
        ),
        Value::Strings(patterns) => patterns.iter().map(Pattern::literal_text).collect(),
        _ => None,
    }
}

fn with_field(object: Value, key: &str, value: Value) -> Value {
    match object {
        Value::Object(mut properties) => {
            properties.retain(|(property_key, _)| property_key != key);
            properties.push((key.to_owned(), value));

            Value::Object(properties)
        }
        object => object,
    }
}

fn globs_in(directory: &str, patterns: &[String]) -> Option<Vec<String>> {
    let mut globs = Vec::new();

    for pattern in patterns {
        globs.extend(relative_globs(directory, pattern)?);
    }

    Some(globs)
}

fn texts(literals: &[&str]) -> Vec<String> {
    literals.iter().map(|&literal| literal.to_owned()).collect()
}

pub(crate) fn evaluated_config(evaluator: &Evaluator, config: &RepoPath) -> Value {
    evaluator.evaluate_config(config)
}

pub(crate) fn read_vitest(
    evaluator: &Evaluator,
    config: &RepoPath,
    runner_configs: &[RepoPath],
) -> Vec<ProjectReading> {
    let value = evaluated_config(evaluator, config);
    let config_directory = parent_directory(config);
    let projects = match nested_field(&value, &["test", "projects"]) {
        Field::Absent => nested_field(&value, &["test", "workspace"]),
        projects => projects,
    };

    match projects {
        Field::Absent => vec![read_vitest_project(&value, config_directory)],
        Field::Known(Value::Array(entries)) => entries
            .iter()
            .flat_map(|entry| {
                read_vitest_project_entry(
                    evaluator,
                    &value,
                    entry,
                    config_directory,
                    runner_configs,
                )
            })
            .collect(),
        Field::Known(_) | Field::Unknown => vec![ProjectReading::unknown()],
    }
}

fn read_vitest_project_entry(
    evaluator: &Evaluator,
    root_value: &Value,
    entry: &Value,
    config_directory: &str,
    runner_configs: &[RepoPath],
) -> Vec<ProjectReading> {
    match entry {
        Value::Object(_) => {
            let project_value = match field(entry, "extends") {
                Field::Known(Value::Boolean(true)) => {
                    merge_config(root_value.clone(), entry.clone())
                }
                Field::Absent | Field::Known(Value::Boolean(false)) => entry.clone(),
                Field::Known(_) | Field::Unknown => return vec![ProjectReading::unknown()],
            };

            vec![read_vitest_project(&project_value, config_directory)]
        }
        Value::Strings(_) => {
            let Some(project_glob) = entry
                .single_literal()
                .and_then(|literal| relative_globs(config_directory, &literal))
            else {
                return vec![ProjectReading::unknown()];
            };
            let Some(project_matcher) = TestPattern::builder().include(project_glob).build() else {
                return vec![ProjectReading::unknown()];
            };
            let project_configs: Vec<&RepoPath> = runner_configs
                .iter()
                .filter(|path| is_vitest_config(path))
                .filter(|path| {
                    project_matcher.matches(path)
                        || RepoPath::new(parent_directory(path))
                            .is_ok_and(|directory| project_matcher.matches(&directory))
                })
                .collect();

            if project_configs.is_empty() {
                return vec![ProjectReading::unknown()];
            }

            project_configs
                .into_iter()
                .map(|project_config| {
                    let project_value = evaluated_config(evaluator, project_config);

                    read_vitest_project(&project_value, parent_directory(project_config))
                })
                .collect()
        }
        _ => vec![ProjectReading::unknown()],
    }
}

fn read_vitest_project(value: &Value, config_directory: &str) -> ProjectReading {
    let root = match field(value, "root") {
        Field::Absent => Some(config_directory.to_owned()),
        Field::Known(root) => config_path(&root, config_directory),
        Field::Unknown => None,
    };
    let test_directory = root.and_then(|root| match nested_field(value, &["test", "dir"]) {
        Field::Absent => Some(root),
        Field::Known(directory) => config_path(&directory, &root),
        Field::Unknown => None,
    });
    let include = known_texts_or(value, &["test", "include"], VITEST_DEFAULT_INCLUDE);
    let include_source = known_texts_or(value, &["test", "includeSource"], &[]);
    let exclude = known_texts_or(value, &["test", "exclude"], VITEST_DEFAULT_EXCLUDE);
    let tests = (|| {
        let directory = test_directory?;
        let include: Vec<String> = include?.into_iter().chain(include_source?).collect();

        TestPattern::builder()
            .include(globs_in(&directory, &include)?)
            .exclude(globs_in(&directory, &exclude?)?)
            .build()
    })();
    let browser = match nested_field(value, &["test", "browser", "enabled"]) {
        Field::Absent | Field::Known(Value::Boolean(false)) => Some(false),
        Field::Known(Value::Boolean(true)) => Some(true),
        Field::Known(_) | Field::Unknown => None,
    };
    let css = match nested_field(value, &["test", "css"]) {
        Field::Absent | Field::Known(Value::Boolean(false)) => Some(false),
        Field::Known(_) | Field::Unknown => Some(true),
    };

    ProjectReading {
        tests,
        renders_assets: browser.zip(css).map(|(browser, css)| browser || css),
    }
}

fn known_texts_or(value: &Value, keys: &[&str], defaults: &[&str]) -> Option<Vec<String>> {
    match nested_field(value, keys) {
        Field::Absent => Some(texts(defaults)),
        Field::Known(known) => literal_texts(&known),
        Field::Unknown => None,
    }
}

pub(crate) fn read_rstest(
    evaluator: &Evaluator,
    config: &RepoPath,
    runner_configs: &[RepoPath],
) -> Vec<ProjectReading> {
    let is_registered = config
        .as_str()
        .rsplit('/')
        .next()
        .is_some_and(|file_name| file_name.starts_with(RSTACK_CONFIG_PREFIX));
    let value = if is_registered {
        evaluator
            .evaluate_registered_config(config)
            .unwrap_or_else(Value::runtime_value)
    } else {
        evaluated_config(evaluator, config)
    };
    let Some(root) = rstest_root(&value, parent_directory(config)) else {
        return vec![ProjectReading::unknown()];
    };

    match field(&value, "projects") {
        Field::Absent => vec![read_rstest_project(&value, &root)],
        Field::Known(Value::Array(entries)) => entries
            .iter()
            .flat_map(|entry| {
                read_rstest_project_entry(evaluator, config, entry, &root, runner_configs)
            })
            .collect(),
        Field::Known(_) | Field::Unknown => vec![ProjectReading::unknown()],
    }
}

fn rstest_root(value: &Value, directory: &str) -> Option<String> {
    match field(value, "root") {
        Field::Absent => Some(directory.to_owned()),
        Field::Known(root) => config_path(&root, directory),
        Field::Unknown => None,
    }
}

fn read_rstest_project_entry(
    evaluator: &Evaluator,
    config: &RepoPath,
    entry: &Value,
    root: &str,
    runner_configs: &[RepoPath],
) -> Vec<ProjectReading> {
    match entry {
        Value::Object(_) => {
            let project = match (field(entry, "extends"), field(entry, "exclude")) {
                (Field::Absent, _) => entry.clone(),
                (Field::Known(base @ Value::Object(_)), Field::Known(exclude)) => {
                    with_field(merge_config(base, entry.clone()), "exclude", exclude)
                }
                (Field::Known(base @ Value::Object(_)), _) => merge_config(base, entry.clone()),
                (Field::Known(_) | Field::Unknown, _) => return vec![ProjectReading::unknown()],
            };

            vec![
                rstest_root(&project, root).map_or_else(ProjectReading::unknown, |project_root| {
                    read_rstest_project(&project, &project_root)
                }),
            ]
        }
        Value::Strings(_) => {
            let Some(project_matcher) = entry
                .single_literal()
                .and_then(|literal| relative_globs(root, &literal))
                .and_then(|project_glob| TestPattern::builder().include(project_glob).build())
            else {
                return vec![ProjectReading::unknown()];
            };
            let project_configs: Vec<&RepoPath> = runner_configs
                .iter()
                .filter(|path| is_rstest_config(path) && *path != config)
                .filter(|path| {
                    project_matcher.matches(path)
                        || RepoPath::new(parent_directory(path))
                            .is_ok_and(|directory| project_matcher.matches(&directory))
                })
                .collect();

            if project_configs.is_empty() {
                return vec![ProjectReading::unknown()];
            }

            project_configs
                .into_iter()
                .flat_map(|project_config| read_rstest(evaluator, project_config, &[]))
                .collect()
        }
        _ => vec![ProjectReading::unknown()],
    }
}

fn read_rstest_project(value: &Value, root: &str) -> ProjectReading {
    let possible_texts_or = |key: &str, defaults: &[&str]| match field(value, key) {
        Field::Absent => Some(texts(defaults)),
        Field::Known(known) => possible_texts(&known),
        Field::Unknown => None,
    };
    let include = possible_texts_or("include", RSTEST_DEFAULT_INCLUDE);
    let include_source = possible_texts_or("includeSource", &[]);
    let exclude = known_texts_or(value, &["exclude"], RSTEST_DEFAULT_EXCLUDE);
    let tests = (|| {
        let include: Vec<String> = include?.into_iter().chain(include_source?).collect();

        TestPattern::builder()
            .include(globs_in(root, &include)?)
            .exclude(globs_in(root, &exclude?)?)
            .build()
    })();

    ProjectReading {
        tests,
        renders_assets: Some(true),
    }
}

pub(crate) fn read_jest(
    evaluator: &Evaluator,
    config: &RepoPath,
    value: Option<Value>,
    runner_configs: &[RepoPath],
    jest_manifests: &[(RepoPath, Value)],
) -> Vec<ProjectReading> {
    let value = value.unwrap_or_else(|| evaluated_config(evaluator, config));
    let config_directory = parent_directory(config);
    let Some(root_directory) = jest_root_directory(&value, config_directory) else {
        return vec![ProjectReading::unknown()];
    };

    match field(&value, "projects") {
        Field::Absent => vec![read_jest_project(&value, &root_directory)],
        Field::Known(Value::Array(entries)) => entries
            .iter()
            .flat_map(|entry| match entry {
                Value::Object(_) => {
                    let project_root = jest_root_directory(entry, &root_directory);

                    vec![
                        project_root.map_or_else(ProjectReading::unknown, |project_root| {
                            read_jest_project(entry, &project_root)
                        }),
                    ]
                }
                Value::Strings(_) => read_jest_project_directories(
                    evaluator,
                    entry,
                    &root_directory,
                    runner_configs,
                    jest_manifests,
                ),
                _ => vec![ProjectReading::unknown()],
            })
            .collect(),
        Field::Known(_) | Field::Unknown => vec![ProjectReading::unknown()],
    }
}

fn jest_root_directory(value: &Value, config_directory: &str) -> Option<String> {
    match field(value, "rootDir") {
        Field::Absent => Some(config_directory.to_owned()),
        Field::Known(root) => config_path(&root, config_directory),
        Field::Unknown => None,
    }
}

fn read_jest_project_directories(
    evaluator: &Evaluator,
    entry: &Value,
    root_directory: &str,
    runner_configs: &[RepoPath],
    jest_manifests: &[(RepoPath, Value)],
) -> Vec<ProjectReading> {
    let Some(project_matcher) = entry
        .single_literal()
        .and_then(|literal| relative_globs(root_directory, &literal))
        .and_then(|project_glob| TestPattern::builder().include(project_glob).build())
    else {
        return vec![ProjectReading::unknown()];
    };
    let in_project = |path: &RepoPath| {
        project_matcher.matches(path)
            || RepoPath::new(parent_directory(path))
                .is_ok_and(|directory| project_matcher.matches(&directory))
    };
    let mut readings: Vec<ProjectReading> = runner_configs
        .iter()
        .filter(|path| is_jest_config(path) && in_project(path))
        .flat_map(|config| read_jest(evaluator, config, None, runner_configs, jest_manifests))
        .collect();

    readings.extend(
        jest_manifests
            .iter()
            .filter(|(manifest, _)| in_project(manifest))
            .flat_map(|(manifest, value)| {
                read_jest(
                    evaluator,
                    manifest,
                    Some(value.clone()),
                    runner_configs,
                    jest_manifests,
                )
            }),
    );

    if readings.is_empty() {
        return vec![ProjectReading::unknown()];
    }

    readings
}

fn read_jest_project(value: &Value, root_directory: &str) -> ProjectReading {
    let rooted_directory = format!("/{root_directory}")
        .trim_end_matches('/')
        .to_owned();
    let with_root_directory = |pattern: &str| pattern.replace("<rootDir>", &rooted_directory);
    let roots = match field(value, "roots") {
        Field::Absent => Some(vec![root_directory.to_owned()]),
        Field::Known(roots) => literal_texts(&roots).and_then(|roots| {
            roots
                .iter()
                .map(|root| {
                    let root = root.strip_prefix("<rootDir>").unwrap_or(root);

                    join_directory(root_directory, root)
                })
                .collect()
        }),
        Field::Unknown => None,
    };
    let test_regex = match field(value, "testRegex") {
        Field::Absent => Some(Vec::new()),
        Field::Known(test_regex) => literal_texts(&test_regex),
        Field::Unknown => None,
    };
    let test_match = match (field(value, "testMatch"), &test_regex) {
        (Field::Absent, Some(test_regex)) if test_regex.is_empty() => {
            Some(texts(JEST_DEFAULT_TEST_MATCH))
        }
        (Field::Absent, _) => Some(Vec::new()),
        (Field::Known(test_match), _) => literal_texts(&test_match),
        (Field::Unknown, _) => None,
    };
    let ignored_paths = match field(value, "testPathIgnorePatterns") {
        Field::Absent => Some(vec![JEST_DEFAULT_IGNORED_PATHS.to_owned()]),
        Field::Known(ignored_paths) => literal_texts(&ignored_paths),
        Field::Unknown => None,
    };
    let tests = (|| {
        let test_match = globs_in(root_directory, &test_match?)?;
        let test_regex = test_regex?;
        let ignored_paths = ignored_paths?
            .iter()
            .map(|pattern| with_root_directory(pattern))
            .collect::<Vec<_>>();

        TestPattern::builder()
            .include(test_match)
            .include_regex(test_regex)
            .exclude_regex(ignored_paths)
            .roots(roots?)
            .build()
    })();

    ProjectReading {
        tests,
        renders_assets: Some(false),
    }
}

pub(crate) fn read_playwright(evaluator: &Evaluator, config: &RepoPath) -> Vec<ProjectReading> {
    let value = evaluated_config(evaluator, config);
    let config_directory = parent_directory(config);
    let test_directory = match field(&value, "testDir") {
        Field::Absent => Some(config_directory.to_owned()),
        Field::Known(directory) => config_path(&directory, config_directory),
        Field::Unknown => None,
    };
    let tests = match field(&value, "testMatch") {
        Field::Absent => return Vec::new(),
        Field::Known(test_match) => (|| {
            let directory = test_directory?;

            TestPattern::builder()
                .include(globs_in(&directory, &literal_texts(&test_match)?)?)
                .build()
        })(),
        Field::Unknown => None,
    };

    vec![ProjectReading {
        tests,
        renders_assets: Some(true),
    }]
}

pub(crate) fn read_cypress(evaluator: &Evaluator, config: &RepoPath) -> Vec<ProjectReading> {
    let value = evaluated_config(evaluator, config);
    let config_directory = parent_directory(config);

    ["e2e", "component"]
        .iter()
        .filter_map(
            |testing_type| match nested_field(&value, &[testing_type, "specPattern"]) {
                Field::Absent => None,
                Field::Known(spec_pattern) => Some(ProjectReading {
                    tests: literal_texts(&spec_pattern)
                        .and_then(|patterns| globs_in(config_directory, &patterns))
                        .and_then(|globs| TestPattern::builder().include(globs).build()),
                    renders_assets: Some(true),
                }),
                Field::Unknown => Some(ProjectReading {
                    tests: None,
                    renders_assets: Some(true),
                }),
            },
        )
        .collect()
}

pub(crate) fn read_mocha(value: &Value, config_directory: &str) -> ProjectReading {
    let extensions = match field(value, "extension") {
        Field::Absent => Some(texts(&MOCHA_DEFAULT_EXTENSIONS)),
        Field::Known(extensions) => literal_texts(&extensions),
        Field::Unknown => None,
    };
    let recursive = match field(value, "recursive") {
        Field::Absent | Field::Known(Value::Boolean(false)) => Some(false),
        Field::Known(Value::Boolean(true)) => Some(true),
        Field::Known(_) | Field::Unknown => None,
    };
    let spec = match field(value, "spec") {
        Field::Absent => (|| {
            let extensions = extensions.as_ref()?.join(",");
            let depth = if recursive? { "**/" } else { "" };

            Some(vec![format!("test/{depth}*.{{{extensions}}}")])
        })(),
        Field::Known(spec) => literal_texts(&spec),
        Field::Unknown => None,
    };
    let tests = spec
        .and_then(|spec| globs_in(config_directory, &spec))
        .and_then(|globs| TestPattern::builder().include(globs).build());

    ProjectReading {
        tests,
        renders_assets: Some(false),
    }
}

pub(crate) fn json_to_value(json: &JsonValue) -> Value {
    match json {
        JsonValue::String(text) => Value::text(text),
        JsonValue::Bool(boolean) => Value::Boolean(*boolean),
        JsonValue::Array(items) => Value::Array(items.iter().map(json_to_value).collect()),
        JsonValue::Object(properties) => Value::Object(
            properties
                .iter()
                .map(|(key, value)| (key.clone(), json_to_value(value)))
                .collect(),
        ),
        JsonValue::Number(_) | JsonValue::Null => Value::runtime_value(),
    }
}
