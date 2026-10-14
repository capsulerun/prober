//! Fixtures followed by name on projects written to disk: a fixture change selects the tests using it, through overrides, transitive requests and plugins, while load-time code, autouse fixtures and dynamic lookups reach every test in scope.

mod test_project;

use crate::test_project::{TestProject, modified, selected};

const ROOT_CONFTEST: &str = r#"import pytest

pytest_plugins = ["tests.plugins.network"]


@pytest.fixture
def database(tmp_path):
    return tmp_path / "db"


@pytest.fixture
def client(database):
    return {"db": database}


@pytest.fixture
def settings():
    return {}
"#;

const API_CONFTEST: &str = r#"import pytest


@pytest.fixture
def settings(settings):
    return {**settings, "api": True}
"#;

const NETWORK_PLUGIN: &str = r#"import pytest


@pytest.fixture
def network():
    return "offline"
"#;

fn project() -> TestProject {
    TestProject::new(&[
        ("tests/__init__.py", ""),
        ("tests/conftest.py", ROOT_CONFTEST),
        ("tests/plugins/__init__.py", ""),
        ("tests/plugins/network.py", NETWORK_PLUGIN),
        ("tests/test_db.py", "def test_db(database): ...\n"),
        ("tests/test_client.py", "def test_client(client): ...\n"),
        ("tests/test_plain.py", "def test_plain(): ...\n"),
        ("tests/api/__init__.py", ""),
        ("tests/api/conftest.py", API_CONFTEST),
        (
            "tests/api/test_settings.py",
            "def test_settings(settings): ...\n",
        ),
        (
            "tests/api/test_network.py",
            "import pytest\n\n\n@pytest.mark.usefixtures(\"network\")\ndef test_offline(): ...\n",
        ),
    ])
}

fn changed_conftest(project: &TestProject, path: &str, base: &str, head: &str) -> Vec<String> {
    project.write(path, head);

    match project.outcome_of(&[(path, base)], &[modified(path)]) {
        test_project::Outcome::Selected(selected_tests) => {
            selected_tests.into_iter().map(|(test, _)| test).collect()
        }
        outcome => panic!("expected a selection, got {outcome:?}"),
    }
}

#[test]
fn a_fixture_change_selects_the_tests_using_it_directly_or_through_other_fixtures() {
    let project = project();
    let head = ROOT_CONFTEST.replace("tmp_path / \"db\"", "tmp_path / \"database\"");

    project.write("tests/conftest.py", &head);

    assert_eq!(
        project.outcome_of(
            &[("tests/conftest.py", ROOT_CONFTEST)],
            &[modified("tests/conftest.py")]
        ),
        selected(&[
            (
                "tests/test_client.py",
                &[
                    "tests/conftest.py (modified)",
                    "-> database changed",
                    "-> fixture database used by tests/test_client.py",
                ],
            ),
            (
                "tests/test_db.py",
                &[
                    "tests/conftest.py (modified)",
                    "-> database changed",
                    "-> fixture database used by tests/test_db.py",
                ],
            ),
        ])
    );
}

#[test]
fn an_overridden_fixture_reaches_the_tests_of_the_override_through_its_own_request() {
    let project = project();
    let head = ROOT_CONFTEST.replace("    return {}\n", "    return {\"debug\": True}\n");

    assert_eq!(
        changed_conftest(&project, "tests/conftest.py", ROOT_CONFTEST, &head),
        ["tests/api/test_settings.py"]
    );

    let project = self::project();
    let api_head = API_CONFTEST.replace("True", "False");

    assert_eq!(
        changed_conftest(&project, "tests/api/conftest.py", API_CONFTEST, &api_head),
        ["tests/api/test_settings.py"]
    );
}

#[test]
fn load_time_code_and_autouse_fixtures_reach_every_test_in_scope() {
    let project = project();
    let load_time = ROOT_CONFTEST.replace(
        "import pytest\n",
        "import os\n\nimport pytest\n\nos.environ[\"MODE\"] = \"test\"\n",
    );

    assert_eq!(
        changed_conftest(&project, "tests/conftest.py", ROOT_CONFTEST, &load_time).len(),
        5
    );

    let project = self::project();
    let collected = ROOT_CONFTEST.replace(
        "import pytest\n",
        "import pytest\n\ncollect_ignore = [\"legacy\"]\n",
    );

    assert_eq!(
        changed_conftest(&project, "tests/conftest.py", ROOT_CONFTEST, &collected).len(),
        5
    );

    let project = self::project();
    let unread = ROOT_CONFTEST.replace("import pytest\n", "import pytest\n\nTIMEOUT = 5\n");

    assert!(changed_conftest(&project, "tests/conftest.py", ROOT_CONFTEST, &unread).is_empty());

    let project = self::project();
    let autouse = API_CONFTEST.replace(
        "@pytest.fixture\ndef settings",
        "@pytest.fixture(autouse=True)\ndef settings",
    );

    project.write("tests/api/conftest.py", &autouse);

    let head = autouse.replace("True}", "False}");

    assert_eq!(
        changed_conftest(&project, "tests/api/conftest.py", &autouse, &head),
        ["tests/api/test_network.py", "tests/api/test_settings.py"]
    );
}

#[test]
fn comments_select_nothing_and_plugin_fixtures_reach_their_users() {
    let project = project();
    let commented =
        ROOT_CONFTEST.replace("def client(database):", "def client(database):  # shared");

    assert!(changed_conftest(&project, "tests/conftest.py", ROOT_CONFTEST, &commented).is_empty());

    let project = self::project();
    let plugin = NETWORK_PLUGIN.replace("offline", "online");

    assert_eq!(
        changed_conftest(
            &project,
            "tests/plugins/network.py",
            NETWORK_PLUGIN,
            &plugin
        ),
        ["tests/api/test_network.py"]
    );
}

#[test]
fn a_dynamic_fixture_lookup_uses_every_fixture_in_scope() {
    let project = project();

    project.write(
        "tests/test_dynamic.py",
        "def test_dynamic(request, name):\n    request.getfixturevalue(name)\n",
    );

    let head = ROOT_CONFTEST.replace("    return {}\n", "    return {\"debug\": True}\n");

    assert_eq!(
        changed_conftest(&project, "tests/conftest.py", ROOT_CONFTEST, &head),
        ["tests/api/test_settings.py", "tests/test_dynamic.py"]
    );
}

#[test]
fn a_conftest_that_cannot_be_parsed_still_reaches_every_test_below_it() {
    let project = project();
    let broken = "import pytest\n\n\n@pytest.fixture\ndef settings(:\n";

    assert_eq!(
        changed_conftest(&project, "tests/api/conftest.py", API_CONFTEST, broken),
        ["tests/api/test_network.py", "tests/api/test_settings.py"]
    );
}

#[test]
fn syntax_newer_than_the_parser_default_keeps_fixtures_apart() {
    let project = project();
    let typed = format!("type Settings = dict[str, bool]\n\n{API_CONFTEST}");

    project.write("tests/api/conftest.py", &typed);

    let head = typed.replace("True", "False");

    assert_eq!(
        changed_conftest(&project, "tests/api/conftest.py", &typed, &head),
        ["tests/api/test_settings.py"]
    );
}

#[test]
fn entry_point_plugins_and_configured_fixtures_reach_the_tests_they_apply_to() {
    let plugin = "import pytest\n\n\n@pytest.fixture\ndef vault():\n    return {}\n\n\n@pytest.fixture\ndef clock():\n    return 0\n";
    let project = TestProject::new(&[
        (
            "pyproject.toml",
            "[project]\nname = \"app\"\n\n[project.entry-points.pytest11]\napp = \"app.testing:plugin\"\n\n[tool.pytest.ini_options]\nusefixtures = \"clock\"\n",
        ),
        ("app/__init__.py", ""),
        ("app/testing.py", plugin),
        ("tests/test_vault.py", "def test_vault(vault): ...\n"),
        ("tests/test_plain.py", "def test_plain(): ...\n"),
    ]);

    assert_eq!(
        changed_conftest(
            &project,
            "app/testing.py",
            plugin,
            &plugin.replace("return {}", "return {\"sealed\": True}")
        ),
        ["tests/test_vault.py"]
    );

    project.write("app/testing.py", plugin);

    assert_eq!(
        changed_conftest(
            &project,
            "app/testing.py",
            plugin,
            &plugin.replace("return 0", "return 1")
        ),
        ["tests/test_plain.py", "tests/test_vault.py"]
    );
}
