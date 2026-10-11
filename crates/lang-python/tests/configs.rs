//! pytest configurations on projects written to disk: options that configure the tests in their scope, discovery changes selecting the tests they add, and options without effect.

mod test_project;

use crate::test_project::{TestProject, all, modified, selected};

const PYPROJECT: &str =
    "[project]\nname = \"app\"\n\n[tool.pytest.ini_options]\naddopts = \"-x\"\nlog_cli = true\n";

fn project() -> TestProject {
    TestProject::new(&[
        ("pyproject.toml", PYPROJECT),
        ("tests/test_core.py", "def test_core(): ...\n"),
        ("tests/check_extra.py", "def test_extra(): ...\n"),
        ("plugins/billing/pytest.ini", "[pytest]\naddopts = -q\n"),
        (
            "plugins/billing/tests/test_invoice.py",
            "def test_invoice(): ...\n",
        ),
        (
            "tox.ini",
            "[tox]\nenvlist = py312\n\n[flake8]\nmax-line-length = 88\n",
        ),
    ])
}

fn change(project: &TestProject, path: &str, base: &str, head: &str) -> test_project::Outcome {
    project.write(path, head);

    project.outcome_of(&[(path, base)], &[modified(path)])
}

#[test]
fn an_option_change_configures_every_test_of_its_scope() {
    let project = project();

    assert_eq!(
        change(
            &project,
            "plugins/billing/pytest.ini",
            "[pytest]\naddopts = -q\n",
            "[pytest]\naddopts = -q --strict-markers\n"
        ),
        selected(&[(
            "plugins/billing/tests/test_invoice.py",
            &[
                "plugins/billing/pytest.ini (modified)",
                "-> plugins/billing/tests/test_invoice.py runs with this config",
            ],
        )])
    );

    let project = self::project();

    assert_eq!(
        change(
            &project,
            "pyproject.toml",
            PYPROJECT,
            &PYPROJECT.replace("-x", "-x -p no:cacheprovider")
        ),
        selected(&[(
            "tests/test_core.py",
            &[
                "pyproject.toml (modified)",
                "-> tests/test_core.py runs with this config",
            ],
        )])
    );
}

#[test]
fn a_root_config_covering_every_test_runs_them_all() {
    let project = TestProject::new(&[
        ("pytest.ini", "[pytest]\naddopts = -x\n"),
        ("tests/test_a.py", "def test_a(): ...\n"),
        ("tests/test_b.py", "def test_b(): ...\n"),
    ]);

    assert_eq!(
        change(
            &project,
            "pytest.ini",
            "[pytest]\naddopts = -x\n",
            "[pytest]\naddopts = -x\nasyncio_mode = auto\n"
        ),
        all(&["pytest.ini (modified) configures the test runner of every test"])
    );
}

#[test]
fn discovery_changes_select_the_tests_they_add_and_output_settings_nothing() {
    let project = project();
    let discovering = PYPROJECT.replace(
        "log_cli = true\n",
        "log_cli = true\npython_files = [\"test_*.py\", \"check_*.py\"]\n",
    );

    assert_eq!(
        change(&project, "pyproject.toml", PYPROJECT, &discovering),
        selected(&[(
            "tests/check_extra.py",
            &[
                "pyproject.toml (modified)",
                "-> tests/check_extra.py is discovered by this config",
            ],
        )])
    );

    let project = self::project();

    assert_eq!(
        change(
            &project,
            "pyproject.toml",
            PYPROJECT,
            &PYPROJECT.replace("log_cli = true", "log_cli = false")
        ),
        selected(&[])
    );
}

#[test]
fn other_sections_of_shared_config_files_are_diffed_one_by_one() {
    let project = project();
    let base = "[tox]\nenvlist = py312\n\n[flake8]\nmax-line-length = 88\n";

    assert_eq!(
        change(&project, "tox.ini", base, &base.replace("88", "100")),
        selected(&[])
    );
    assert_eq!(
        change(&project, "tox.ini", base, &base.replace("py312", "py313")),
        all(&["tox.ini (modified) changes \"[tox]\", which may affect every test"])
    );
}
