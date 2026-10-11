//! Rust conventions: which files are modules, where crates keep their targets, and which changes run every test.

use prober_core::files::RepoPath;

pub const LANGUAGE: &str = "rust";

pub const ALWAYS_ALL_DEFAULTS: &[&str] = &[
    "rust-toolchain",
    "rust-toolchain.toml",
    "**/.cargo/config",
    "**/.cargo/config.toml",
];

pub const MANIFEST_DEFAULTS: &[&str] = &["**/Cargo.toml", "**/Cargo.lock"];

pub(crate) const MANIFEST: &str = "Cargo.toml";

pub(crate) const LOCKFILE: &str = "Cargo.lock";

pub(crate) const TARGET_UNIT: &str = "crate";

pub(crate) const REGISTRY_NODE_PREFIX: &str = "cargo-registry/";

pub(crate) const BUILTIN_CRATES: [&str; 5] = ["std", "core", "alloc", "proc_macro", "test"];

const SOURCE_EXTENSION: &str = ".rs";

pub fn is_source_file(path: &RepoPath) -> bool {
    path.as_str().ends_with(SOURCE_EXTENSION)
}

pub fn is_manifest(path: &RepoPath) -> bool {
    split_path(path.as_str()).1 == MANIFEST
}

pub(crate) fn split_path(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

pub(crate) fn parent_directory(path: &str) -> &str {
    split_path(path).0
}

pub(crate) fn join_directory(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_owned()
    } else if name.is_empty() {
        directory.to_owned()
    } else {
        format!("{directory}/{name}")
    }
}

pub(crate) fn normalized_join(directory: &str, relative: &str) -> Option<String> {
    let mut segments: Vec<&str> = directory
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();

    for segment in relative.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            segment => segments.push(segment),
        }
    }

    Some(segments.join("/"))
}

pub(crate) fn crate_identifier(name: &str) -> String {
    name.replace('-', "_")
}

pub(crate) fn registry_node(package: &str) -> RepoPath {
    RepoPath::new(format!(
        "{REGISTRY_NODE_PREFIX}{}",
        crate_identifier(package)
    ))
    .expect("package names are valid path segments")
}
