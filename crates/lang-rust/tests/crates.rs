//! Crates of a workspace written to disk: dependents narrowed to the modules they use, re-exports, globs, macros, proc-macro and build-script crates, trait impls and the files code reads.

mod test_project;

use crate::test_project::{TestProject, modified, selected};

const WORKSPACE: &str = "[workspace]\nmembers = [\"crates/*\"]\n";

fn workspace(extra: &[(&str, &str)]) -> TestProject {
    let mut files = vec![
        ("Cargo.toml", WORKSPACE),
        (
            "crates/store/Cargo.toml",
            "[package]\nname = \"store\"\nversion = \"0.1.0\"\n",
        ),
        (
            "crates/store/src/lib.rs",
            "pub mod cache;\npub mod disk;\nmod records;\n\npub use records::Record;\n",
        ),
        ("crates/store/src/cache.rs", "pub fn get() {}\n"),
        ("crates/store/src/disk.rs", "pub fn write() {}\n"),
        ("crates/store/src/records.rs", "pub struct Record;\n"),
        (
            "crates/api/Cargo.toml",
            "[package]\nname = \"api\"\nversion = \"0.1.0\"\n\n[dependencies]\nstore = { path = \"../store\" }\n",
        ),
        ("crates/api/src/lib.rs", "mod handlers;\n"),
        (
            "crates/api/src/handlers.rs",
            "use store::cache;\n\npub fn handle() -> store::Record {\n    cache::get();\n    store::Record\n}\n",
        ),
        (
            "crates/web/Cargo.toml",
            "[package]\nname = \"web\"\nversion = \"0.1.0\"\n\n[dependencies]\nstore = { path = \"../store\" }\n",
        ),
        (
            "crates/web/src/lib.rs",
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn writes() {\n        assert_eq!(store::disk::write(), ());\n    }\n}\n",
        ),
    ];

    files.extend_from_slice(extra);

    TestProject::new(&files)
}

#[test]
fn dependents_are_selected_only_when_a_module_they_use_changes() {
    let project = workspace(&[]);

    assert_eq!(
        project.selected_by("crates/store/src/cache.rs"),
        ["store", "api"]
    );
    assert_eq!(
        project.selected_by("crates/store/src/disk.rs"),
        ["store", "web"]
    );
    assert_eq!(
        project.outcome_of(&[], &[modified("crates/store/src/records.rs")]),
        selected(&[
            (
                "store",
                &[
                    "crates/store/src/records.rs (modified)",
                    "-> built into crates/store/Cargo.toml",
                ],
            ),
            (
                "api",
                &[
                    "crates/store/src/records.rs (modified)",
                    "-> imported by crates/api/src/handlers.rs",
                    "-> built into crates/api/Cargo.toml",
                ],
            ),
        ])
    );
}

#[test]
fn glob_imports_and_exported_macros_reach_the_users_of_their_module_or_definition() {
    let project = workspace(&[
        (
            "crates/store/src/macros.rs",
            "#[macro_export]\nmacro_rules! key { ($name:expr) => { $name } }\n",
        ),
        (
            "crates/web/src/routes.rs",
            "use store::disk::*;\n\npub fn route() { write(); }\n",
        ),
        (
            "crates/api/src/keys.rs",
            "pub fn key() -> &'static str { store::key!(\"a\") }\n",
        ),
    ]);

    project.write("crates/web/src/lib.rs", "mod routes;\n");
    project.write("crates/api/src/lib.rs", "mod handlers;\nmod keys;\n");
    project.write(
        "crates/store/src/lib.rs",
        "pub mod cache;\npub mod disk;\nmod records;\nmod macros;\n\npub use records::Record;\n",
    );

    assert_eq!(
        project.selected_by("crates/store/src/disk.rs"),
        ["store", "web"]
    );
    assert_eq!(
        project.selected_by("crates/store/src/macros.rs"),
        ["store", "api"]
    );
}

#[test]
fn proc_macro_and_build_script_crates_reach_every_user() {
    let project = workspace(&[
        (
            "crates/derive/Cargo.toml",
            "[package]\nname = \"derive\"\nversion = \"0.1.0\"\n\n[lib]\nproc-macro = true\n",
        ),
        ("crates/derive/src/lib.rs", "mod expand;\n"),
        ("crates/derive/src/expand.rs", "pub fn expand() {}\n"),
        (
            "crates/schema/Cargo.toml",
            "[package]\nname = \"schema\"\nversion = \"0.1.0\"\n\n[dependencies]\nderive = { path = \"../derive\" }\n",
        ),
        (
            "crates/schema/build.rs",
            "fn main() {\n    println!(\"cargo:rerun-if-changed=proto/api.proto\");\n}\n",
        ),
        ("crates/schema/proto/api.proto", "message Api {}\n"),
        (
            "crates/schema/src/lib.rs",
            "#[derive(derive::Model)]\npub struct Api;\ninclude!(concat!(env!(\"OUT_DIR\"), \"/api.rs\"));\n",
        ),
        (
            "crates/client/Cargo.toml",
            "[package]\nname = \"client\"\nversion = \"0.1.0\"\n\n[dependencies]\nschema = { path = \"../schema\" }\n",
        ),
        (
            "crates/client/src/lib.rs",
            "pub fn call() -> schema::Api { schema::Api }\n",
        ),
    ]);

    assert_eq!(
        project.selected_by("crates/derive/src/expand.rs"),
        ["derive", "schema", "client"]
    );
    assert_eq!(
        project.selected_by("crates/schema/proto/api.proto"),
        ["schema", "client"]
    );
}

#[test]
fn an_impl_reaches_the_users_of_its_type() {
    let project = workspace(&[(
        "crates/store/src/display.rs",
        "use crate::records::Record;\n\nimpl std::fmt::Display for Record {\n    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { Ok(()) }\n}\n",
    )]);

    project.write(
        "crates/store/src/lib.rs",
        "pub mod cache;\npub mod disk;\nmod records;\nmod display;\n\npub use records::Record;\n",
    );

    assert_eq!(
        project.selected_by("crates/store/src/display.rs"),
        ["store", "api"]
    );
}

#[test]
fn included_files_and_integration_tests_belong_to_their_crate() {
    let project = workspace(&[
        (
            "crates/web/tests/pages.rs",
            "const PAGE: &str = include_str!(\"../templates/page.html\");\n\n#[test]\nfn renders() { web::render(); }\n",
        ),
        ("crates/web/templates/page.html", "<html></html>\n"),
        ("crates/web/tests/data/routes.json", "[]\n"),
        (
            "crates/web/tests/routes.rs",
            "#[test]\nfn routes() { std::fs::read_to_string(\"tests/data/routes.json\").unwrap(); }\n",
        ),
    ]);

    assert_eq!(
        project.selected_by("crates/web/templates/page.html"),
        ["web"]
    );
    assert_eq!(
        project.selected_by("crates/web/tests/data/routes.json"),
        ["web"]
    );
}

#[test]
fn manifest_changes_reach_the_users_of_a_crate_only_when_they_change_its_build() {
    let project = workspace(&[]);
    let base = "[package]\nname = \"store\"\nversion = \"0.1.0\"\n";

    project.write(
        "crates/store/Cargo.toml",
        "[package]\nname = \"store\"\nversion = \"0.2.0\"\n",
    );

    assert_eq!(
        project
            .outcome_of(
                &[("crates/store/Cargo.toml", base)],
                &[modified("crates/store/Cargo.toml")]
            )
            .selected_paths(),
        ["store"]
    );

    project.write(
        "crates/store/Cargo.toml",
        "[package]\nname = \"store\"\nversion = \"0.1.0\"\n\n[features]\ndefault = [\"compress\"]\ncompress = []\n",
    );

    assert_eq!(
        project
            .outcome_of(
                &[("crates/store/Cargo.toml", base)],
                &[modified("crates/store/Cargo.toml")]
            )
            .selected_paths(),
        ["store", "api", "web"]
    );
}

#[test]
fn a_locked_crate_change_reaches_the_crates_using_it() {
    let lock = |version: &str| {
        format!(
            "[[package]]\nname = \"api\"\nversion = \"0.1.0\"\ndependencies = [\"serde\"]\n\n[[package]]\nname = \"serde\"\nversion = \"{version}\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n"
        )
    };
    let project = workspace(&[("Cargo.lock", &lock("1.0.2"))]);

    project.write(
        "crates/api/Cargo.toml",
        "[package]\nname = \"api\"\nversion = \"0.1.0\"\n\n[dependencies]\nstore = { path = \"../store\" }\nserde = \"1\"\n",
    );

    let base = lock("1.0.1");

    assert_eq!(
        project.outcome_of(&[("Cargo.lock", &base)], &[modified("Cargo.lock")]),
        selected(&[(
            "api",
            &[
                "Cargo.lock (modified)",
                "-> crate serde changed (1.0.1 -> 1.0.2)",
                "-> imported by crates/api/Cargo.toml",
            ],
        )])
    );
}
