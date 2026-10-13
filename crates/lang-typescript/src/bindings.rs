//! Node addons that napi builds from a Rust crate: the package declaring them, its crate, and the loaders that require the addon.

use std::collections::{BTreeSet, HashSet};

use prober_core::files::RepoPath;
use prober_core::graph::Inference;
use regex::Regex;
use serde_json::Value;

use crate::conventions::is_source_file;
use crate::dynamic::DynamicSite;
use crate::linker::LinkedFile;
use crate::manifests::documents::parse_jsonc;
use crate::presets::patterns::join_directory;
use crate::workspaces::{PACKAGE_MANIFEST, parent_directory};

const CRATE_MANIFEST: &str = "Cargo.toml";

const ADDON_EXTENSION: &str = ".node";

const DEFAULT_LOADER: &str = "index.js";

const LIBRARY_PATH_VARIABLE: &str = "NAPI_RS_NATIVE_LIBRARY_PATH";

const NODE_BINDING: &str = "Node binding";

const COMMAND_SEPARATORS: [&str; 4] = ["&&", "||", ";", "|"];

#[derive(Debug, Clone, PartialEq, Eq)]
struct NapiPackage {
    directory: String,
    declaration: RepoPath,
    binary_name: String,
    crate_manifest: RepoPath,
    generated_loaders: Vec<RepoPath>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BindingLink {
    pub loader: RepoPath,
    pub crate_manifest: RepoPath,
    pub inference: Inference,
}

#[derive(Debug, Default)]
pub(crate) struct NapiPackages(Vec<NapiPackage>);

impl NapiPackages {
    pub fn read(repo_files: &[RepoPath], read_head: &dyn Fn(&RepoPath) -> Option<String>) -> Self {
        let file_set: HashSet<&str> = repo_files.iter().map(RepoPath::as_str).collect();
        let packages = repo_files
            .iter()
            .filter(|path| path.as_str().rsplit('/').next() == Some(PACKAGE_MANIFEST))
            .filter(|path| {
                !path
                    .as_str()
                    .split('/')
                    .any(|segment| segment == "node_modules")
            })
            .filter_map(|declaration| napi_package(declaration, &file_set, read_head))
            .collect();

        Self(packages)
    }

    pub fn links(&self, linked_files: &[LinkedFile]) -> Vec<BindingLink> {
        let mut loaders: BTreeSet<(RepoPath, usize)> = BTreeSet::new();

        for (index, package) in self.0.iter().enumerate() {
            for loader in &package.generated_loaders {
                loaders.insert((loader.clone(), index));
            }

            for linked_file in linked_files
                .iter()
                .filter(|linked_file| package.contains(&linked_file.path))
            {
                let requires_addon = linked_file
                    .dependencies
                    .iter()
                    .any(|dependency| package.is_addon(&dependency.path));

                if requires_addon {
                    loaders.insert((linked_file.path.clone(), index));
                }
            }
        }

        loaders
            .into_iter()
            .map(|(loader, index)| {
                let package = &self.0[index];

                BindingLink {
                    loader,
                    crate_manifest: package.crate_manifest.clone(),
                    inference: Inference {
                        line: 0,
                        kind: NODE_BINDING,
                        pattern: package.binary_name.clone(),
                        detail: Some(package.declaration.to_string()),
                    },
                }
            })
            .collect()
    }
}

pub(crate) fn loads_the_addon(site: &DynamicSite, binding_links: &[BindingLink]) -> bool {
    site.expression.contains(LIBRARY_PATH_VARIABLE)
        && binding_links
            .iter()
            .any(|binding_link| binding_link.loader == site.module)
}

impl NapiPackage {
    fn contains(&self, path: &RepoPath) -> bool {
        self.directory.is_empty()
            || path
                .as_str()
                .strip_prefix(self.directory.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    }

    fn is_addon(&self, path: &RepoPath) -> bool {
        let file_name = path.as_str().rsplit('/').next().unwrap_or_default();

        file_name.ends_with(ADDON_EXTENSION)
            && file_name
                .strip_prefix(self.binary_name.as_str())
                .is_some_and(|rest| rest.starts_with('.'))
    }
}

fn napi_package(
    declaration: &RepoPath,
    file_set: &HashSet<&str>,
    read_head: &dyn Fn(&RepoPath) -> Option<String>,
) -> Option<NapiPackage> {
    let document = parse_jsonc(&read_head(declaration)?).ok()?;
    let napi = document.get("napi")?;
    let binary_name = ["binaryName", "name"]
        .iter()
        .find_map(|key| napi.get(*key).and_then(Value::as_str))?
        .to_owned();
    let directory = parent_directory(declaration).to_owned();
    let scripts: Vec<&str> = document
        .get("scripts")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(_, script)| script.as_str())
        .collect();
    let builds = napi_builds(&scripts);
    let existing = |path: Option<String>| {
        path.filter(|path| file_set.contains(path.as_str()))
            .and_then(|path| RepoPath::new(path).ok())
    };
    let named_manifests = builds.iter().filter_map(|arguments| {
        let manifest_path = option(arguments, &["--manifest-path", "-m"])?;

        join_directory(&command_directory(&directory, arguments)?, manifest_path)
    });
    let crate_manifest = named_manifests
        .chain(join_directory(&directory, CRATE_MANIFEST))
        .chain(script_manifests(&directory, &scripts, file_set, read_head))
        .find_map(|path| existing(Some(path)))?;
    let generated_loaders = builds
        .iter()
        .filter_map(|arguments| generated_loader(&directory, &crate_manifest, arguments))
        .collect();

    Some(NapiPackage {
        directory,
        declaration: declaration.clone(),
        binary_name,
        crate_manifest,
        generated_loaders,
    })
}

fn napi_builds<'s>(scripts: &[&'s str]) -> Vec<Vec<&'s str>> {
    let mut builds = Vec::new();

    for script in scripts {
        let words: Vec<&str> = script.split_whitespace().collect();

        for (position, pair) in words.windows(2).enumerate() {
            if pair != ["napi", "build"] {
                continue;
            }

            let arguments = words[position + 2..]
                .iter()
                .take_while(|word| !COMMAND_SEPARATORS.contains(word))
                .copied()
                .collect();

            builds.push(arguments);
        }
    }

    builds
}

fn option<'a>(arguments: &[&'a str], names: &[&str]) -> Option<&'a str> {
    arguments
        .iter()
        .enumerate()
        .find_map(|(position, argument)| {
            if names.contains(argument) {
                return arguments.get(position + 1).copied();
            }

            names.iter().find_map(|name| {
                argument
                    .strip_prefix(name)
                    .and_then(|rest| rest.strip_prefix('='))
            })
        })
        .map(|value| value.trim_matches(['"', '\'']))
}

fn command_directory(directory: &str, arguments: &[&str]) -> Option<String> {
    join_directory(directory, option(arguments, &["--cwd"]).unwrap_or("."))
}

fn generated_loader(
    directory: &str,
    crate_manifest: &RepoPath,
    arguments: &[&str],
) -> Option<RepoPath> {
    if !arguments.contains(&"--platform") || arguments.contains(&"--no-js") {
        return None;
    }

    let output_directory = match option(arguments, &["--output-dir", "-o"]) {
        Some(output) => join_directory(&command_directory(directory, arguments)?, output)?,
        None => parent_directory(crate_manifest).to_owned(),
    };
    let loader = option(arguments, &["--js"]).unwrap_or(DEFAULT_LOADER);

    if loader == "false" {
        return None;
    }

    RepoPath::new(join_directory(&output_directory, loader)?).ok()
}

fn script_manifests(
    directory: &str,
    scripts: &[&str],
    file_set: &HashSet<&str>,
    read_head: &dyn Fn(&RepoPath) -> Option<String>,
) -> Vec<String> {
    let literal = Regex::new(r#"["'`]([^"'`\s]*Cargo\.toml)["'`]"#).expect("valid pattern");

    scripts
        .iter()
        .flat_map(|script| script.split_whitespace())
        .filter_map(|word| join_directory(directory, word))
        .filter(|path| file_set.contains(path.as_str()))
        .filter_map(|path| RepoPath::new(path).ok())
        .filter(is_source_file)
        .filter_map(|script_file| read_head(&script_file))
        .flat_map(|text| {
            literal
                .captures_iter(&text)
                .filter_map(|captures| join_directory(directory, &captures[1]))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn packages(files: &[(&str, &str)]) -> Vec<NapiPackage> {
        let repo_files: Vec<RepoPath> = files
            .iter()
            .map(|(path, _)| RepoPath::new(*path).unwrap())
            .collect();
        let contents: HashMap<&str, &str> = files.iter().copied().collect();
        let read_head = |path: &RepoPath| contents.get(path.as_str()).map(|text| text.to_string());

        NapiPackages::read(&repo_files, &read_head).0
    }

    #[test]
    fn package_with_its_crate_beside_it_declares_the_addon() {
        let packages = packages(&[
            (
                "crates/node/package.json",
                r#"{ "name": "@app/binding", "main": "binding.js", "napi": { "binaryName": "app" } }"#,
            ),
            ("crates/node/Cargo.toml", "[package]\nname = \"app-node\"\n"),
            ("crates/node/binding.js", ""),
        ]);

        assert_eq!(
            packages,
            [NapiPackage {
                directory: "crates/node".to_owned(),
                declaration: RepoPath::new("crates/node/package.json").unwrap(),
                binary_name: "app".to_owned(),
                crate_manifest: RepoPath::new("crates/node/Cargo.toml").unwrap(),
                generated_loaders: Vec::new(),
            }]
        );
    }

    #[test]
    fn build_script_names_the_crate_and_the_loader_it_generates() {
        let packages = packages(&[
            (
                "nodejs/package.json",
                r#"{ "napi": { "binaryName": "db" }, "scripts": { "build": "napi build --platform --js ../lib/native.js --output-dir lib && tsc", "build:wasm": "napi build --platform --no-js --manifest-path ../rust/node/Cargo.toml" } }"#,
            ),
            ("rust/node/Cargo.toml", "[package]\nname = \"db-node\"\n"),
        ]);

        assert_eq!(
            packages[0].crate_manifest,
            RepoPath::new("rust/node/Cargo.toml").unwrap()
        );
        assert_eq!(
            packages[0].generated_loaders,
            [RepoPath::new("nodejs/lib/native.js").unwrap()]
        );
    }

    #[test]
    fn script_file_run_by_the_package_names_the_crate() {
        let packages = packages(&[
            (
                "packages/bundler/package.json",
                r#"{ "napi": { "binaryName": "bundler-binding" }, "scripts": { "build-binding": "node ./build-binding.mjs" } }"#,
            ),
            (
                "packages/bundler/build-binding.mjs",
                "await napi.build({ manifestPath: '../../crates/binding/Cargo.toml' });\n",
            ),
            (
                "crates/binding/Cargo.toml",
                "[package]\nname = \"binding\"\n",
            ),
        ]);

        assert_eq!(
            packages[0].crate_manifest,
            RepoPath::new("crates/binding/Cargo.toml").unwrap()
        );
    }

    #[test]
    fn packages_without_napi_or_their_crate_declare_no_addon() {
        assert_eq!(
            packages(&[
                ("app/package.json", r#"{ "name": "app" }"#),
                ("app/Cargo.toml", "[package]\nname = \"app\"\n"),
                ("lib/package.json", r#"{ "napi": { "binaryName": "lib" } }"#),
            ]),
            []
        );
    }

    #[test]
    fn addons_are_the_platform_files_named_after_the_binary() {
        let package = &packages(&[
            (
                "node/package.json",
                r#"{ "napi": { "binaryName": "app" } }"#,
            ),
            ("node/Cargo.toml", ""),
        ])[0];

        assert!(package.is_addon(&RepoPath::new("node/app.linux-x64-gnu.node").unwrap()));
        assert!(!package.is_addon(&RepoPath::new("node/app-wasm.node").unwrap()));
        assert!(!package.is_addon(&RepoPath::new("node/app.linux-x64-gnu.js").unwrap()));
    }
}
