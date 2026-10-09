use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{Expression, ObjectProperty, PropertyKey};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::SourceType;
use prober_core::files::RepoPath;
use prober_core::graph::Inference;
use serde_json::Value;

use crate::conventions::is_source_file;
use crate::manifests::documents::parse_jsonc;
use crate::presets::patterns::join_directory;
use crate::workspaces::{PACKAGE_MANIFEST, parent_directory};

const OUTPUT_EXTENSIONS: [&str; 7] = [".d.mts", ".d.cts", ".d.ts", ".mjs", ".cjs", ".jsx", ".js"];

const SOURCE_EXTENSIONS: [&str; 8] = [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"];

const DEFAULT_OUTPUT_DIRECTORY: &str = "dist";

const SOURCE_DIRECTORY: &str = "src";

const BUNDLER_CONFIG_PREFIXES: [&str; 7] = [
    "tsdown.config.",
    "tsup.config.",
    "rollup.config.",
    "rolldown.config.",
    "vite.config.",
    "build.config.",
    "esbuild.config.",
];

const OUTPUT_DIRECTORY_KEYS: [&str; 3] = ["outDir", "outdir", "dir"];

const MANIFEST_ENTRY_FIELDS: [&str; 7] = [
    "main", "module", "types", "typings", "browser", "bin", "exports",
];

const MAX_EXTENDS_DEPTH: usize = 4;

const BUILD_MAPPING: &str = "build mapping";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuildSource {
    pub path: RepoPath,
    pub inference: Inference,
}

#[derive(Debug, Clone)]
struct SourceRule {
    source_directory: String,
    origin: Option<String>,
}

#[derive(Debug, Clone)]
struct BuildEntry {
    source: RepoPath,
    origin: String,
}

#[derive(Debug, Default)]
pub(crate) struct BuildMapping {
    files: HashSet<String>,
    rules: HashMap<String, Vec<SourceRule>>,
    entries: HashMap<String, Vec<BuildEntry>>,
}

#[derive(Default)]
struct TsconfigOptions {
    out_dir: Option<String>,
    root_dir: Option<String>,
    include: Option<Vec<String>>,
    no_emit: bool,
}

impl BuildMapping {
    pub fn new(repo_root: &Path, repo_files: &[RepoPath]) -> Self {
        let read = |path: &RepoPath| fs::read_to_string(repo_root.join(path.as_str())).ok();
        let mut mapping = Self {
            files: repo_files
                .iter()
                .map(|path| path.as_str().to_owned())
                .collect(),
            ..Self::default()
        };
        let directories: HashSet<&str> = repo_files
            .iter()
            .flat_map(|path| {
                path.as_str()
                    .match_indices('/')
                    .map(|(offset, _)| &path.as_str()[..offset])
            })
            .collect();
        let is_output_directory = |directory: &str| !directories.contains(directory);

        for tsconfig in repo_files.iter().filter(|path| is_tsconfig(path)) {
            mapping.add_tsconfig(tsconfig, &read, &is_output_directory);
        }

        let manifests = repo_files
            .iter()
            .filter(|path| file_name(path) == PACKAGE_MANIFEST && !is_installed(path));

        for manifest in manifests {
            let package_directory = parent_directory(manifest);

            if let Some(document) = read(manifest).and_then(|text| parse_jsonc(&text).ok()) {
                mapping.add_manifest(manifest, &document, &is_output_directory);
            }

            mapping.add_convention(package_directory, &directories, &is_output_directory);
        }

        for config in repo_files.iter().filter(|path| is_bundler_config(path)) {
            if let Some(text) = read(config) {
                mapping.add_bundler_config(config, &text);
            }
        }

        mapping
    }

    pub fn is_output(&self, path: &str) -> bool {
        let Some(stem) = output_stem(path) else {
            return false;
        };

        if self.files.contains(path) {
            return false;
        }

        if self.entries.contains_key(stem) {
            return true;
        }

        self.matching_rules(stem).any(|(directory, rules)| {
            let relative_stem = &stem[directory.len() + 1..];

            rules
                .iter()
                .any(|rule| !self.rule_sources(rule, relative_stem).is_empty())
        })
    }

    pub fn sources_of(&self, path: &str) -> Vec<BuildSource> {
        if self.files.contains(path) {
            return Vec::new();
        }

        let Some(stem) = output_stem(path) else {
            return Vec::new();
        };

        if let Some(entries) = self.entries.get(stem) {
            return entries
                .iter()
                .map(|entry| BuildSource {
                    path: entry.source.clone(),
                    inference: build_inference(
                        format!("{stem} <- {}", entry.source),
                        Some(entry.origin.clone()),
                    ),
                })
                .collect();
        }

        for (directory, rules) in self.matching_rules(stem) {
            let relative_stem = &stem[directory.len() + 1..];

            for rule in rules {
                let sources = self.rule_sources(rule, relative_stem);

                if sources.is_empty() {
                    continue;
                }

                let pattern = format!(
                    "{directory}/ <- {}/",
                    display_directory(&rule.source_directory)
                );

                return sources
                    .into_iter()
                    .map(|path| BuildSource {
                        path,
                        inference: build_inference(pattern.clone(), rule.origin.clone()),
                    })
                    .collect();
            }
        }

        Vec::new()
    }

    fn matching_rules<'m>(
        &'m self,
        stem: &'m str,
    ) -> impl Iterator<Item = (&'m str, &'m Vec<SourceRule>)> + 'm {
        stem.rmatch_indices('/').filter_map(move |(offset, _)| {
            let directory = &stem[..offset];

            self.rules.get(directory).map(|rules| (directory, rules))
        })
    }

    fn rule_sources(&self, rule: &SourceRule, relative_stem: &str) -> Vec<RepoPath> {
        let Some(source_stem) = join_directory(&rule.source_directory, relative_stem) else {
            return Vec::new();
        };

        SOURCE_EXTENSIONS
            .iter()
            .map(|extension| format!("{source_stem}{extension}"))
            .filter(|candidate| self.files.contains(candidate))
            .filter_map(|candidate| RepoPath::new(candidate).ok())
            .filter(is_source_file)
            .collect()
    }

    fn add_rule(&mut self, output_directory: String, rule: SourceRule) {
        let rules = self.rules.entry(output_directory).or_default();
        let is_known = rules.iter().any(|known| {
            known.source_directory == rule.source_directory && known.origin == rule.origin
        });

        if !is_known {
            rules.push(rule);
        }
    }

    fn add_tsconfig(
        &mut self,
        tsconfig: &RepoPath,
        read: &dyn Fn(&RepoPath) -> Option<String>,
        is_output_directory: &dyn Fn(&str) -> bool,
    ) {
        let options = tsconfig_options(tsconfig, read);
        let Some(out_dir) = options.out_dir.filter(|_| !options.no_emit) else {
            return;
        };

        if out_dir.is_empty() || !is_output_directory(&out_dir) {
            return;
        }

        let source_directory = options
            .root_dir
            .or_else(|| options.include.as_deref().map(common_directory))
            .unwrap_or_else(|| parent_directory(tsconfig).to_owned());

        self.add_rule(
            out_dir,
            SourceRule {
                source_directory,
                origin: Some(tsconfig.to_string()),
            },
        );
    }

    fn add_manifest(
        &mut self,
        manifest: &RepoPath,
        document: &Value,
        is_output_directory: &dyn Fn(&str) -> bool,
    ) {
        let package_directory = parent_directory(manifest);
        let mut targets = Vec::new();

        for field in MANIFEST_ENTRY_FIELDS {
            if let Some(value) = document.get(field) {
                collect_strings(value, &mut targets);
            }
        }

        let mut output_directories: Vec<String> = Vec::new();

        for target in targets {
            let Some((first_segment, _)) = target
                .trim_start_matches("./")
                .split_once('/')
                .filter(|(first_segment, _)| !first_segment.is_empty() && *first_segment != "..")
            else {
                continue;
            };
            let Some(output_directory) = join_directory(package_directory, first_segment) else {
                continue;
            };

            if is_output_directory(&output_directory)
                && !output_directories.contains(&output_directory)
            {
                output_directories.push(output_directory);
            }
        }

        for output_directory in output_directories {
            for source_directory in package_source_directories(package_directory) {
                self.add_rule(
                    output_directory.clone(),
                    SourceRule {
                        source_directory,
                        origin: Some(manifest.to_string()),
                    },
                );
            }
        }
    }

    fn add_convention(
        &mut self,
        package_directory: &str,
        directories: &HashSet<&str>,
        is_output_directory: &dyn Fn(&str) -> bool,
    ) {
        let Some(output_directory) = join_directory(package_directory, DEFAULT_OUTPUT_DIRECTORY)
        else {
            return;
        };

        if !is_output_directory(&output_directory) {
            return;
        }

        for source_directory in package_source_directories(package_directory) {
            if source_directory != package_directory
                && !directories.contains(source_directory.as_str())
            {
                continue;
            }

            self.add_rule(
                output_directory.clone(),
                SourceRule {
                    source_directory,
                    origin: None,
                },
            );
        }
    }

    fn add_bundler_config(&mut self, config: &RepoPath, text: &str) {
        let config_directory = parent_directory(config);
        let allocator = Allocator::default();
        let source_type = SourceType::from_path(config.as_str()).unwrap_or_default();
        let parsed = Parser::new(&allocator, text, source_type).parse();
        let mut collector = PropertyCollector::default();

        collector.visit_program(&parsed.program);

        let mut output_directories: Vec<String> = collector
            .properties
            .iter()
            .filter(|(key, _)| OUTPUT_DIRECTORY_KEYS.contains(&key.as_str()))
            .filter_map(|(_, value)| join_directory(config_directory, value))
            .filter(|directory| !directory.is_empty())
            .collect();

        if output_directories.is_empty() {
            output_directories.extend(join_directory(config_directory, DEFAULT_OUTPUT_DIRECTORY));
        }

        output_directories.sort();
        output_directories.dedup();

        for (key, value) in &collector.properties {
            let Some(source) = [config_directory, ""]
                .iter()
                .filter_map(|directory| join_directory(directory, value))
                .filter(|candidate| self.files.contains(candidate))
                .find_map(|candidate| RepoPath::new(candidate).ok().filter(is_source_file))
            else {
                continue;
            };
            let Some(output_name) = output_stem(key.trim_start_matches("./"))
                .or_else(|| Some(key.trim_start_matches("./")))
                .filter(|name| !name.is_empty() && !name.starts_with('.'))
            else {
                continue;
            };

            for output_directory in &output_directories {
                let Some(output_stem) = join_directory(output_directory, output_name) else {
                    continue;
                };

                self.entries
                    .entry(output_stem)
                    .or_default()
                    .push(BuildEntry {
                        source: source.clone(),
                        origin: config.to_string(),
                    });
            }
        }
    }
}

#[derive(Default)]
struct PropertyCollector {
    properties: Vec<(String, String)>,
}

impl<'a> Visit<'a> for PropertyCollector {
    fn visit_object_property(&mut self, property: &ObjectProperty<'a>) {
        let key = match &property.key {
            PropertyKey::StaticIdentifier(identifier) => Some(identifier.name.to_string()),
            PropertyKey::StringLiteral(literal) => Some(literal.value.to_string()),
            _ => None,
        };
        let value = match property.value.get_inner_expression() {
            Expression::StringLiteral(literal) => Some(literal.value.to_string()),
            Expression::TemplateLiteral(template) if template.expressions.is_empty() => template
                .quasis
                .first()
                .and_then(|quasi| quasi.value.cooked.as_deref())
                .map(str::to_owned),
            _ => None,
        };

        if let (Some(key), Some(value)) = (key, value) {
            self.properties.push((key, value));
        }

        walk::walk_object_property(self, property);
    }
}

fn tsconfig_options(
    tsconfig: &RepoPath,
    read: &dyn Fn(&RepoPath) -> Option<String>,
) -> TsconfigOptions {
    let mut options = TsconfigOptions::default();
    let mut current = Some(tsconfig.clone());

    for _ in 0..MAX_EXTENDS_DEPTH {
        let Some(config) = current.take() else {
            break;
        };
        let Some(document) = read(&config).and_then(|text| parse_jsonc(&text).ok()) else {
            break;
        };
        let directory = parent_directory(&config);
        let compiler_option = |name: &str| {
            document
                .get("compilerOptions")
                .and_then(|compiler_options| compiler_options.get(name))
        };
        let relative_path = |value: Option<&Value>| {
            value
                .and_then(Value::as_str)
                .and_then(|path| join_directory(directory, path))
        };

        if options.out_dir.is_none() {
            options.out_dir = relative_path(compiler_option("outDir"));
        }

        if options.root_dir.is_none() {
            options.root_dir = relative_path(compiler_option("rootDir"));
        }

        if options.include.is_none() {
            options.include = document
                .get("include")
                .and_then(Value::as_array)
                .map(|patterns| {
                    patterns
                        .iter()
                        .filter_map(Value::as_str)
                        .filter_map(|pattern| join_directory(directory, pattern))
                        .collect()
                });
        }

        if compiler_option("noEmit") == Some(&Value::Bool(true)) {
            options.no_emit = true;
        }

        current = extended_config(document.get("extends"), directory);
    }

    options
}

fn extended_config(extends: Option<&Value>, directory: &str) -> Option<RepoPath> {
    let base = match extends? {
        Value::String(base) => base.as_str(),
        Value::Array(bases) => bases.first()?.as_str()?,
        _ => return None,
    };

    if !base.starts_with('.') {
        return None;
    }

    let joined = join_directory(directory, base)?;
    let joined = if joined.ends_with(".json") {
        joined
    } else {
        format!("{joined}.json")
    };

    RepoPath::new(joined).ok()
}

fn common_directory(patterns: &[String]) -> String {
    let directories: Vec<Vec<&str>> = patterns
        .iter()
        .map(|pattern| {
            let segments: Vec<&str> = pattern
                .split('/')
                .take_while(|segment| !segment.contains(['*', '?', '{', '[']))
                .collect();
            let is_file = segments.len() == pattern.split('/').count()
                && segments.last().is_some_and(|name| name.contains('.'));

            if is_file {
                segments[..segments.len() - 1].to_vec()
            } else {
                segments
            }
        })
        .collect();
    let Some(first) = directories.first() else {
        return String::new();
    };
    let common_length = (0..first.len())
        .take_while(|&position| {
            directories
                .iter()
                .all(|directory| directory.get(position) == first.get(position))
        })
        .count();

    first[..common_length].join("/")
}

fn collect_strings(value: &Value, strings: &mut Vec<String>) {
    match value {
        Value::String(string) => strings.push(string.clone()),
        Value::Array(items) => {
            for item in items {
                collect_strings(item, strings);
            }
        }
        Value::Object(fields) => {
            for field in fields.values() {
                collect_strings(field, strings);
            }
        }
        _ => {}
    }
}

fn package_source_directories(package_directory: &str) -> Vec<String> {
    join_directory(package_directory, SOURCE_DIRECTORY)
        .into_iter()
        .chain(std::iter::once(package_directory.to_owned()))
        .collect()
}

fn output_stem(path: &str) -> Option<&str> {
    OUTPUT_EXTENSIONS
        .iter()
        .find_map(|extension| path.strip_suffix(extension))
        .filter(|stem| !stem.is_empty() && !stem.ends_with('/'))
}

fn display_directory(directory: &str) -> &str {
    if directory.is_empty() { "." } else { directory }
}

fn build_inference(pattern: String, origin: Option<String>) -> Inference {
    Inference {
        line: 0,
        kind: BUILD_MAPPING,
        pattern,
        detail: origin,
    }
}

fn file_name(path: &RepoPath) -> &str {
    path.as_str()
        .rsplit_once('/')
        .map_or(path.as_str(), |(_, name)| name)
}

fn is_installed(path: &RepoPath) -> bool {
    path.as_str()
        .split('/')
        .any(|segment| segment == "node_modules")
}

fn is_tsconfig(path: &RepoPath) -> bool {
    let name = file_name(path);

    (name.starts_with("tsconfig") || name.starts_with("jsconfig"))
        && name.ends_with(".json")
        && !is_installed(path)
}

fn is_bundler_config(path: &RepoPath) -> bool {
    let name = file_name(path);

    BUNDLER_CONFIG_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
        && is_source_file(path)
        && !is_installed(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping(files: &[(&str, &str)]) -> BuildMapping {
        let directory = tempfile::tempdir().unwrap();
        let mut repo_files = Vec::new();

        for (path, content) in files {
            let absolute_path = directory.path().join(path);

            fs::create_dir_all(absolute_path.parent().unwrap()).unwrap();
            fs::write(absolute_path, content).unwrap();
            repo_files.push(RepoPath::new(*path).unwrap());
        }

        BuildMapping::new(directory.path(), &repo_files)
    }

    fn sources(mapping: &BuildMapping, output: &str) -> Vec<(String, String)> {
        mapping
            .sources_of(output)
            .into_iter()
            .map(|source| (source.path.to_string(), source.inference.description()))
            .collect()
    }

    #[test]
    fn package_outputs_map_to_the_same_path_under_src_or_the_package() {
        let mapping = mapping(&[
            (
                "package.json",
                r#"{ "name": "cli", "main": "dist/index.js" }"#,
            ),
            ("src/index.ts", ""),
            ("src/commands/run.ts", ""),
            ("packages/ui/package.json", r#"{ "name": "ui" }"#),
            ("packages/ui/button.tsx", ""),
        ]);

        assert_eq!(
            sources(&mapping, "dist/commands/run.js"),
            [(
                "src/commands/run.ts".to_owned(),
                "inferred build mapping dist/ <- src/ (package.json)".to_owned()
            )]
        );
        assert_eq!(
            sources(&mapping, "packages/ui/dist/button.mjs"),
            [(
                "packages/ui/button.tsx".to_owned(),
                "inferred build mapping packages/ui/dist/ <- packages/ui/".to_owned()
            )]
        );
        assert!(mapping.is_output("dist/index.d.ts"));
        assert!(!mapping.is_output("dist/missing.js"));
        assert!(!mapping.is_output("src/index.ts"));
    }

    #[test]
    fn tsconfig_out_dir_maps_to_its_root_dir_or_its_include_directory() {
        let mapping = mapping(&[
            (
                "tools/tsconfig.json",
                r#"{ "extends": "./tsconfig.base", "include": ["lib/**/*.ts"] }"#,
            ),
            (
                "tools/tsconfig.base.json",
                r#"{ "compilerOptions": { "outDir": "../out/tools" } }"#,
            ),
            ("tools/lib/task.ts", ""),
            (
                "server/tsconfig.json",
                r#"{ "compilerOptions": { "outDir": "build", "rootDir": "code" } }"#,
            ),
            ("server/code/main.ts", ""),
            (
                "tsconfig.json",
                r#"{ "compilerOptions": { "outDir": "types", "noEmit": true } }"#,
            ),
            ("main.ts", ""),
        ]);

        assert_eq!(
            sources(&mapping, "out/tools/task.js"),
            [(
                "tools/lib/task.ts".to_owned(),
                "inferred build mapping out/tools/ <- tools/lib/ (tools/tsconfig.json)".to_owned()
            )]
        );
        assert_eq!(
            sources(&mapping, "server/build/main.js")[0].0,
            "server/code/main.ts"
        );
        assert!(sources(&mapping, "types/main.js").is_empty());
    }

    #[test]
    fn bundler_entries_map_renamed_outputs_to_their_sources() {
        let mapping = mapping(&[
            (
                "package.json",
                r#"{ "name": "cli", "bin": { "cli": "./bin/cli.mjs" } }"#,
            ),
            ("bin/cli.mjs", ""),
            (
                "tsdown.config.ts",
                r#"export default { entry: { index: "src/main.ts", "cli/run": "src/commands/run.ts" } }"#,
            ),
            ("src/main.ts", ""),
            ("src/commands/run.ts", ""),
        ]);

        assert_eq!(
            sources(&mapping, "dist/cli/run.js"),
            [(
                "src/commands/run.ts".to_owned(),
                "inferred build mapping dist/cli/run <- src/commands/run.ts (tsdown.config.ts)"
                    .to_owned()
            )]
        );
        assert_eq!(sources(&mapping, "dist/index.mjs")[0].0, "src/main.ts");
        assert!(sources(&mapping, "bin/cli.mjs").is_empty());
    }

    #[test]
    fn tracked_directories_are_never_build_outputs() {
        let mapping = mapping(&[
            (
                "package.json",
                r#"{ "name": "lib", "main": "lib/index.js" }"#,
            ),
            ("lib/index.js", ""),
            ("dist/index.js", ""),
            ("src/index.ts", ""),
        ]);

        assert!(!mapping.is_output("lib/other.js"));
        assert!(!mapping.is_output("dist/index.js"));
        assert!(!mapping.is_output("dist/other.js"));
    }
}
