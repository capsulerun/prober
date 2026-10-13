//! Native modules that maturin builds from a Rust crate, read from the pyproject.toml declaring them.

use prober_core::files::RepoPath;
use toml::{Table, Value};

use crate::conventions::{join_directory, parent_directory};
use crate::modules::normalized_join;

const MATURIN: &str = "maturin";

const CRATE_MANIFEST: &str = "Cargo.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeModule {
    pub name: String,
    pub location: String,
    pub manifest: RepoPath,
    pub declaration: RepoPath,
}

pub(crate) fn native_module(
    declaration: &RepoPath,
    document: &Table,
    read_head: &dyn Fn(&RepoPath) -> Option<String>,
) -> Option<NativeModule> {
    let maturin = document.get("tool").and_then(|tool| tool.get(MATURIN));
    let backend = document
        .get("build-system")
        .and_then(|build_system| build_system.get("build-backend"))
        .and_then(Value::as_str);

    if maturin.is_none() && backend != Some(MATURIN) {
        return None;
    }

    let setting = |key: &str| {
        maturin
            .and_then(|maturin| maturin.get(key))
            .and_then(Value::as_str)
    };
    let project_directory = parent_directory(declaration.as_str());
    let manifest = normalized_join(
        project_directory,
        setting("manifest-path").unwrap_or(CRATE_MANIFEST),
    )?;
    let manifest = RepoPath::new(manifest).ok()?;
    let crate_document: Table = toml::from_str(&read_head(&manifest)?).ok()?;
    let name = match setting("module-name") {
        Some(name) => name.to_owned(),
        None => library_name(&crate_document)?,
    };
    let python_source = match setting("python-source") {
        Some(python_source) => normalized_join(project_directory, python_source)?,
        None => project_directory.to_owned(),
    };

    Some(NativeModule {
        location: join_directory(&python_source, &name.replace('.', "/")),
        name,
        manifest,
        declaration: declaration.clone(),
    })
}

fn library_name(crate_document: &Table) -> Option<String> {
    let declared = crate_document
        .get("lib")
        .and_then(|library| library.get("name"))
        .and_then(Value::as_str);
    let package = crate_document
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(Value::as_str);

    declared.or(package).map(|name| name.replace('-', "_"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn read(declaration: &str, pyproject: &str, files: &[(&str, &str)]) -> Option<NativeModule> {
        let files: HashMap<String, String> = files
            .iter()
            .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
            .collect();
        let read_head = |path: &RepoPath| files.get(path.as_str()).cloned();
        let document: Table = toml::from_str(pyproject).unwrap();

        native_module(&RepoPath::new(declaration).unwrap(), &document, &read_head)
    }

    #[test]
    fn module_name_and_python_source_place_the_native_module() {
        let native = read(
            "python/pyproject.toml",
            "[tool.maturin]\npython-source = \"python\"\nmodule-name = \"lancedb._lancedb\"\n",
            &[(
                "python/Cargo.toml",
                "[package]\nname = \"lancedb-python\"\n",
            )],
        )
        .unwrap();

        assert_eq!(native.name, "lancedb._lancedb");
        assert_eq!(native.location, "python/python/lancedb/_lancedb");
        assert_eq!(native.manifest.as_str(), "python/Cargo.toml");
        assert_eq!(native.declaration.as_str(), "python/pyproject.toml");
    }

    #[test]
    fn without_a_module_name_the_library_or_package_name_is_the_module() {
        let backend = "[build-system]\nbuild-backend = \"maturin\"\n";
        let library = read(
            "ext/pyproject.toml",
            backend,
            &[(
                "ext/Cargo.toml",
                "[package]\nname = \"ext\"\n[lib]\nname = \"_fast\"\n",
            )],
        )
        .unwrap();
        let package = read(
            "ext/pyproject.toml",
            backend,
            &[("ext/Cargo.toml", "[package]\nname = \"fast-ext\"\n")],
        )
        .unwrap();

        assert_eq!(library.location, "ext/_fast");
        assert_eq!(package.name, "fast_ext");
    }

    #[test]
    fn manifest_path_names_a_crate_elsewhere() {
        let native = read(
            "py/pyproject.toml",
            "[tool.maturin]\nmanifest-path = \"../crates/py/Cargo.toml\"\nmodule-name = \"core\"\n",
            &[("crates/py/Cargo.toml", "[package]\nname = \"py\"\n")],
        )
        .unwrap();

        assert_eq!(native.manifest.as_str(), "crates/py/Cargo.toml");
        assert_eq!(native.location, "py/core");
    }

    #[test]
    fn projects_built_otherwise_or_without_their_crate_declare_no_module() {
        let crate_files = [("Cargo.toml", "[package]\nname = \"ext\"\n")];

        assert_eq!(
            read(
                "pyproject.toml",
                "[build-system]\nbuild-backend = \"hatchling.build\"\n",
                &crate_files
            ),
            None
        );
        assert_eq!(
            read(
                "pyproject.toml",
                "[tool.maturin]\nmodule-name = \"ext\"\n",
                &[]
            ),
            None
        );
    }
}
