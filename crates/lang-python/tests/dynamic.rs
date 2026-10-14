//! Computed imports, plugin folders, dotted strings and data paths on projects written to disk.

mod test_project;

use prober_core::config::InferenceSettings;
use prober_core::graph::EdgeKind;

use crate::test_project::{TestProject, dependents, modified, selected};

#[test]
fn literal_and_enumerated_computed_imports_link_their_modules() {
    let project = TestProject::new(&[
        ("app/__init__.py", ""),
        ("app/codecs/__init__.py", ""),
        ("app/codecs/json_codec.py", ""),
        ("app/codecs/xml_codec.py", ""),
        ("app/codecs/yaml_codec.py", ""),
        (
            "app/registry.py",
            "import importlib\n\nCODECS = (\"json\", \"xml\")\n\n\ndef load():\n    for name in CODECS:\n        importlib.import_module(f\"app.codecs.{name}_codec\")\n    importlib.import_module(\"app.codecs\")\n",
        ),
        (
            "tests/test_registry.py",
            "from app.registry import load\n\n\ndef test_load():\n    load()\n",
        ),
    ]);
    let graph = project.link();

    assert_eq!(
        dependents(&graph, "app/codecs/xml_codec.py"),
        [("app/registry.py", EdgeKind::Dynamic)]
    );
    assert!(dependents(&graph, "app/codecs/yaml_codec.py").is_empty());
    assert_eq!(
        project.outcome_of(&[], &[modified("app/codecs/json_codec.py")]),
        selected(&[(
            "tests/test_registry.py",
            &[
                "app/codecs/json_codec.py (modified)",
                "-> loaded by load in app/registry.py:8 (inferred computed import app.codecs.json_codec)",
                "-> load used by test_load in tests/test_registry.py",
                "-> test_load run by tests/test_registry.py",
            ],
        )])
    );
}

#[test]
fn a_relative_computed_import_resolves_against_its_package() {
    let project = TestProject::new(&[
        ("app/__init__.py", ""),
        (
            "app/backends/__init__.py",
            "from importlib import import_module\n\n\ndef backend(name):\n    return import_module(f\".{name}\", __name__)\n\n\nbackend(\"redis\")\n",
        ),
        ("app/backends/redis.py", ""),
        ("app/backends/memory.py", ""),
        ("tests/test_backends.py", "import app.backends\n"),
    ]);

    assert_eq!(
        project.selected_by("app/backends/redis.py"),
        ["tests/test_backends.py"]
    );
    assert!(project.selected_by("app/backends/memory.py").is_empty());
}

#[test]
fn computed_imports_of_plugin_folders_link_each_folder() {
    let project = TestProject::new(&[
        ("core/__init__.py", ""),
        ("core/components/__init__.py", ""),
        (
            "core/components/light/__init__.py",
            "from .device import Device\n",
        ),
        ("core/components/light/device.py", ""),
        (
            "core/components/light/manifest.json",
            "{\"domain\": \"light\"}",
        ),
        ("core/components/fan/__init__.py", ""),
        ("core/components/fan/manifest.json", "{\"domain\": \"fan\"}"),
        (
            "core/loader.py",
            "import importlib\n\n\ndef load(domain):\n    return importlib.import_module(f\"core.components.{domain}\")\n",
        ),
        (
            "tests/test_loader.py",
            "from core import loader\n\n\ndef test_load():\n    assert loader.load\n",
        ),
    ]);
    let graph = project.link();

    assert_eq!(
        dependents(&graph, "core/components/light"),
        [("core/loader.py", EdgeKind::Dynamic)]
    );
    assert_eq!(
        dependents(&graph, "core/components/light/device.py"),
        [
            ("core/components/light", EdgeKind::Contains),
            ("core/components/light/__init__.py", EdgeKind::Import)
        ]
    );
    assert_eq!(
        project.outcome_of(&[], &[modified("core/components/light/device.py")]),
        selected(&[(
            "tests/test_loader.py",
            &[
                "core/components/light/device.py (modified)",
                "-> in folder core/components/light/",
                "-> loaded by load in core/loader.py:5 (inferred plugin folder core/components/{id}/ (id=light))",
                "-> load used by test_load in tests/test_loader.py",
                "-> test_load run by tests/test_loader.py",
            ],
        )])
    );
}

#[test]
fn a_computed_import_without_a_literal_package_widens_to_its_dependents() {
    let project = TestProject::new(&[
        ("app/__init__.py", ""),
        (
            "app/plugins.py",
            "import importlib\n\n\ndef load(path):\n    return importlib.import_module(path)\n\n\nload(input())\n",
        ),
        ("app/extra.py", ""),
        ("tests/test_plugins.py", "import app.plugins\n"),
        ("tests/test_other.py", "import os\n"),
    ]);

    assert_eq!(
        project.outcome_of(&[], &[modified("app/extra.py")]),
        selected(&[(
            "tests/test_plugins.py",
            &[
                "app/extra.py (modified)",
                "-> widened: unresolved dynamic import at app/plugins.py:5 (importlib.import_module(path))",
                "-> load used at load by app/plugins.py",
                "-> loaded by tests/test_plugins.py",
            ],
        )])
    );
}

#[test]
fn a_computed_top_level_import_links_the_modules_its_prefix_names() {
    let project = TestProject::new(&[
        ("app/__init__.py", ""),
        (
            "app/drivers.py",
            "import importlib\n\n\ndef driver(uri):\n    name = uri.split(\":\")[0]\n    return importlib.import_module(f\"adbc_driver_{name}\")\n\n\ndef codec(name):\n    return importlib.import_module(f\"codec_{name}\")\n\n\ndriver(input())\ncodec(input())\n",
        ),
        ("codec_json.py", ""),
        ("app/extra.py", ""),
        ("tests/test_drivers.py", "import app.drivers\n"),
    ]);

    assert!(project.selected_by("app/extra.py").is_empty());
    assert_eq!(
        project.selected_by("codec_json.py"),
        ["tests/test_drivers.py"]
    );
}

#[test]
fn a_computed_import_whose_callers_explode_widens_instead_of_evaluating_them_all() {
    let project = TestProject::new(&[
        ("app/__init__.py", ""),
        (
            "app/expressions.py",
            "import importlib\n\n\ndef resolve(node):\n    if node.kind == \"plus\":\n        return resolve(node.lhs) + resolve(node.rhs)\n    if node.kind == \"minus\":\n        return resolve(node.lhs) + \"-\" + resolve(node.rhs)\n    if node.kind == \"times\":\n        return resolve(node.lhs) + \"*\" + resolve(node.rhs)\n    if node.kind == \"handler\":\n        return importlib.import_module(\"app.handlers.\" + resolve(node.lhs) + resolve(node.rhs))\n    return node.name\n",
        ),
        ("app/handlers/__init__.py", ""),
        ("app/handlers/sum.py", ""),
        (
            "tests/test_expressions.py",
            "import app.expressions\n\n\ndef test_resolve():\n    app.expressions.resolve(None)\n",
        ),
    ]);

    assert_eq!(
        project.outcome_of(&[], &[modified("app/handlers/sum.py")]),
        selected(&[(
            "tests/test_expressions.py",
            &[
                "app/handlers/sum.py (modified)",
                "-> widened: unresolved dynamic import at app/expressions.py:12 (importlib.import_module(\"app.handlers.\" + resolve(node.lhs) + resolve(node.rhs)))",
                "-> resolve used by test_resolve in tests/test_expressions.py",
                "-> test_resolve run by tests/test_expressions.py",
            ],
        )])
    );
}

#[test]
fn division_by_a_number_is_not_a_path() {
    let project = TestProject::new(&[
        (
            "tests/test_ratio.py",
            "from pathlib import Path\n\nDATA = Path(__file__).parent / \"data\"\n\n\ndef test_ratio():\n    half = DATA / 2\n    (DATA / \"table.csv\").read_text()\n",
        ),
        ("tests/data/table.csv", ""),
        ("tests/data/2", ""),
    ]);
    let graph = project.link();

    assert_eq!(
        dependents(&graph, "tests/data/table.csv"),
        [("tests/test_ratio.py", EdgeKind::Reads)]
    );
    assert!(dependents(&graph, "tests/data/2").is_empty());
}

#[test]
fn dotted_module_strings_link_the_module_they_name() {
    let project = TestProject::new(&[
        ("app/__init__.py", ""),
        ("app/mail.py", "def send(): ...\n"),
        (
            "tests/test_signup.py",
            "from unittest.mock import patch\n\n\n@patch(\"app.mail.send\")\ndef test_signup(send): ...\n",
        ),
    ]);
    let graph = project.link();

    assert_eq!(
        dependents(&graph, "app/mail.py"),
        [("tests/test_signup.py", EdgeKind::Dynamic)]
    );
    assert!(
        dependents(
            &project.link_with(InferenceSettings {
                dynamic_imports: false,
                ..InferenceSettings::default()
            }),
            "app/mail.py"
        )
        .is_empty()
    );
}

#[test]
fn data_files_built_from_the_module_path_are_read() {
    let project = TestProject::new(&[
        (
            "tests/test_parser.py",
            "import json\nfrom pathlib import Path\n\nFIXTURES = Path(__file__).parent / \"fixtures\"\n\n\ndef test_parse():\n    json.loads((FIXTURES / \"order.json\").read_text())\n    open(\"tests/data/config.yaml\")\n",
        ),
        ("tests/fixtures/order.json", "{}"),
        ("tests/fixtures/other.json", "{}"),
        ("tests/data/config.yaml", ""),
    ]);
    let graph = project.link();

    assert_eq!(
        dependents(&graph, "tests/fixtures/order.json"),
        [("tests/test_parser.py", EdgeKind::Reads)]
    );
    assert_eq!(
        dependents(&graph, "tests/data/config.yaml"),
        [("tests/test_parser.py", EdgeKind::Reads)]
    );
    assert!(dependents(&graph, "tests/fixtures/other.json").is_empty());
}

#[test]
fn a_helper_taking_a_file_name_is_followed_to_each_caller() {
    let project = TestProject::new(&[
        ("tests/__init__.py", ""),
        (
            "tests/common.py",
            "from pathlib import Path\n\n\ndef fixture_path(name, integration):\n    return Path(__file__).parent / \"components\" / integration / \"fixtures\" / name\n\n\ndef load_fixture(name, integration):\n    return fixture_path(name, integration).read_text()\n",
        ),
        ("tests/components/__init__.py", ""),
        ("tests/components/hue/__init__.py", ""),
        ("tests/components/hue/const.py", "DOMAIN = \"hue\"\n"),
        (
            "tests/components/hue/test_bridge.py",
            "from tests.common import load_fixture\nfrom .const import DOMAIN\n\n\ndef test_bridge():\n    load_fixture(\"bridge.json\", DOMAIN)\n",
        ),
        ("tests/components/hue/fixtures/bridge.json", "{}"),
        ("tests/components/hue/fixtures/lights.json", "{}"),
        ("tests/components/nest/__init__.py", ""),
        (
            "tests/components/nest/test_api.py",
            "from tests.common import load_fixture\n",
        ),
    ]);
    let graph = project.link();

    assert_eq!(
        dependents(&graph, "tests/components/hue/fixtures/bridge.json"),
        [("tests/components/hue/test_bridge.py", EdgeKind::Reads)]
    );
    assert!(dependents(&graph, "tests/components/hue/fixtures/lights.json").is_empty());
    assert_eq!(
        project.selected_by("tests/components/hue/fixtures/bridge.json"),
        ["tests/components/hue/test_bridge.py"]
    );
}

#[test]
fn a_helper_passed_as_a_value_keeps_its_own_glob() {
    let project = TestProject::new(&[
        (
            "tests/helpers.py",
            "from pathlib import Path\n\n\ndef read(name):\n    return (Path(__file__).parent / \"data\" / f\"{name}.json\").read_text()\n\n\nREADERS = [read]\n",
        ),
        ("tests/data/a.json", "{}"),
        ("tests/test_a.py", "from helpers import read\n"),
    ]);

    assert_eq!(
        dependents(&project.link(), "tests/data/a.json"),
        [("tests/helpers.py", EdgeKind::Reads)]
    );
    assert_eq!(
        project.selected_by("tests/data/a.json"),
        ["tests/test_a.py"]
    );
}

#[test]
fn modules_named_in_a_data_file_reach_its_readers() {
    let project = TestProject::new(&[
        ("app/__init__.py", ""),
        ("app/handlers.py", "def on_event(): ...\n"),
        (
            "tests/test_logging.py",
            "from pathlib import Path\n\nCONFIG = Path(__file__).parent / \"logging.yaml\"\n",
        ),
        (
            "tests/logging.yaml",
            "handlers:\n  event:\n    class: app.handlers.on_event\n",
        ),
    ]);

    assert_eq!(
        project.outcome_of(&[], &[modified("app/handlers.py")]),
        selected(&[(
            "tests/test_logging.py",
            &[
                "app/handlers.py (modified)",
                "-> listed by tests/logging.yaml:3 (inferred listed module app.handlers.on_event)",
                "-> read by tests/test_logging.py:3 (inferred file path logging.yaml)",
            ],
        )])
    );
}

#[test]
fn a_helper_called_through_an_unresolved_attribute_keeps_its_own_glob() {
    let project = TestProject::new(&[
        ("tests/__init__.py", ""),
        (
            "tests/common.py",
            "from pathlib import Path\n\n\ndef load(name):\n    return (Path(__file__).parent / \"data\" / f\"{name}.json\").read_text()\n",
        ),
        ("tests/data/a.json", "{}"),
        (
            "tests/test_a.py",
            "import tests.common\n\n\ndef test_a():\n    tests.common.load(\"a\")\n",
        ),
    ]);

    assert_eq!(
        dependents(&project.link(), "tests/data/a.json"),
        [("tests/common.py", EdgeKind::Reads)]
    );
}

#[test]
fn a_test_module_named_in_a_string_is_not_loaded_by_the_code_naming_it() {
    let project = TestProject::new(&[
        ("tests/__init__.py", ""),
        ("tests/conftest.py", "from tests.ignored import IGNORED\n"),
        ("tests/ignored.py", "IGNORED = [\"tests.test_flaky\"]\n"),
        ("tests/test_flaky.py", "def test_flaky(): ...\n"),
        ("tests/test_other.py", "def test_other(): ...\n"),
    ]);

    assert_eq!(
        project.selected_by("tests/test_flaky.py"),
        ["tests/test_flaky.py"]
    );
}
