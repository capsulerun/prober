//! Dependency changes and naming families on projects written to disk: changed pins and lockfile entries reaching the modules importing them, pyproject fields, and tests paired with what they are named after.

mod test_project;

use crate::test_project::{TestProject, all, modified, selected};

fn service(requirements: &str, lockfile: &str) -> TestProject {
    TestProject::new(&[
        (
            "pyproject.toml",
            "[project]\nname = \"service\"\nversion = \"1.1\"\ndependencies = [\"httpx>=0.27\"]\n",
        ),
        ("requirements.txt", requirements),
        ("uv.lock", lockfile),
        ("service/__init__.py", ""),
        ("service/client.py", "import httpx\n"),
        ("service/config.py", "import yaml\n"),
        ("tests/test_client.py", "from service import client\n"),
        ("tests/test_config.py", "from service import config\n"),
    ])
}

const LOCKFILE: &str = "[[package]]\nname = \"httpx\"\nversion = \"0.27.0\"\ndependencies = [{ name = \"idna\" }]\n\n[[package]]\nname = \"idna\"\nversion = \"3.7\"\n";

#[test]
fn a_changed_pin_selects_the_tests_reaching_its_importers() {
    let project = service("PyYAML==6.0.2\n", LOCKFILE);

    assert_eq!(
        project.outcome_of(
            &[("requirements.txt", "PyYAML==6.0.1\n")],
            &[modified("requirements.txt")]
        ),
        selected(&[(
            "tests/test_config.py",
            &[
                "requirements.txt (modified)",
                "-> package pyyaml changed (PyYAML==6.0.1 -> PyYAML==6.0.2)",
                "-> imported by service/config.py",
                "-> imported by tests/test_config.py",
            ],
        )])
    );
}

#[test]
fn a_lockfile_change_reaches_the_importers_of_its_dependents() {
    let project = service("PyYAML==6.0.2\n", LOCKFILE);
    let base_lockfile = LOCKFILE.replace("3.7", "3.6");

    assert_eq!(
        project
            .outcome_of(&[("uv.lock", &base_lockfile)], &[modified("uv.lock")])
            .selected_paths(),
        ["tests/test_client.py"]
    );
}

#[test]
fn pyproject_metadata_selects_nothing_and_unknown_tables_run_every_test() {
    let project = service("PyYAML==6.0.2\n", LOCKFILE);
    let old_version =
        "[project]\nname = \"service\"\nversion = \"1.0\"\ndependencies = [\"httpx>=0.27\"]\n";
    let build_change = "[project]\nname = \"service\"\nversion = \"1.1\"\ndependencies = [\"httpx>=0.27\"]\n\n[build-system]\nrequires = [\"hatchling\"]\n";

    assert_eq!(
        project.outcome_of(
            &[("pyproject.toml", old_version)],
            &[modified("pyproject.toml")]
        ),
        selected(&[])
    );

    project.write("pyproject.toml", build_change);

    assert_eq!(
        project.outcome_of(
            &[("pyproject.toml", old_version)],
            &[modified("pyproject.toml")]
        ),
        all(&["pyproject.toml (modified) changes \"build-system\", which may affect every test"])
    );
}

#[test]
fn tests_are_paired_with_the_modules_and_snapshots_named_after_them() {
    let project = TestProject::new(&[
        ("app/__init__.py", ""),
        ("app/components/__init__.py", ""),
        ("app/components/light/__init__.py", ""),
        ("app/components/light/sensor.py", "VALUE = 1\n"),
        ("tests/__init__.py", ""),
        ("tests/components/__init__.py", ""),
        ("tests/components/light/__init__.py", ""),
        ("tests/components/light/test_init.py", "import os\n"),
        ("tests/components/light/snapshots/test_init.ambr", ""),
    ]);

    assert_eq!(
        project.outcome_of(&[], &[modified("app/components/light/sensor.py")]),
        selected(&[(
            "tests/components/light/test_init.py",
            &[
                "app/components/light/sensor.py (modified)",
                "-> paired by name with tests/components/light/test_init.py",
            ],
        )])
    );
    assert_eq!(
        project.selected_by("tests/components/light/snapshots/test_init.ambr"),
        ["tests/components/light/test_init.py"]
    );
}
