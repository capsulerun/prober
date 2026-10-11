//! Static imports on projects written to disk: import forms, package roots and layouts, type-only imports, conftests and package inits, and installed packages.

mod test_project;

use prober_core::graph::EdgeKind;

use crate::test_project::{TestProject, dependents, modified, selected};

fn shop() -> TestProject {
    TestProject::new(&[
        ("shop/__init__.py", ""),
        (
            "shop/cart.py",
            "from .pricing import total\nfrom . import tax\n",
        ),
        ("shop/pricing.py", "import shop.money as money\n"),
        ("shop/money.py", "ROUND = 2\n"),
        ("shop/tax.py", "from shop.money import *\n"),
        (
            "shop/report.py",
            "from typing import TYPE_CHECKING\n\nif TYPE_CHECKING:\n    from shop.cart import Cart\n\n\ndef render():\n    import json\n    from shop import tax\n",
        ),
        ("tests/__init__.py", ""),
        ("tests/conftest.py", "import pytest\n"),
        ("tests/unit/__init__.py", ""),
        ("tests/unit/test_cart.py", "from shop.cart import total\n"),
        ("tests/unit/test_report.py", "import shop.report\n"),
    ])
}

#[test]
fn each_import_form_links_the_modules_it_runs() {
    let graph = shop().link();

    assert_eq!(
        dependents(&graph, "shop/pricing.py"),
        [("shop/cart.py", EdgeKind::Import)]
    );
    assert_eq!(
        dependents(&graph, "shop/tax.py"),
        [
            ("shop/cart.py", EdgeKind::Import),
            ("shop/report.py", EdgeKind::Import)
        ]
    );
    assert_eq!(
        dependents(&graph, "shop/money.py"),
        [
            ("shop/pricing.py", EdgeKind::Import),
            ("shop/tax.py", EdgeKind::Import)
        ]
    );
    assert!(
        dependents(&graph, "shop/__init__.py")
            .contains(&("tests/unit/test_cart.py", EdgeKind::Import))
    );
}

#[test]
fn type_checking_imports_never_select_tests() {
    let project = shop();

    assert_eq!(
        dependents(&project.link(), "shop/cart.py"),
        [
            ("shop/report.py", EdgeKind::TypeOnly),
            ("tests/unit/test_cart.py", EdgeKind::Import)
        ]
    );
    assert_eq!(
        project.selected_by("shop/cart.py"),
        ["tests/unit/test_cart.py"]
    );
}

#[test]
fn a_change_selects_the_tests_reaching_it_with_the_import_chain() {
    assert_eq!(
        shop().outcome_of(&[], &[modified("shop/money.py")]),
        selected(&[
            (
                "tests/unit/test_cart.py",
                &[
                    "shop/money.py (modified)",
                    "-> imported by shop/pricing.py",
                    "-> imported by shop/cart.py",
                    "-> imported by tests/unit/test_cart.py",
                ],
            ),
            (
                "tests/unit/test_report.py",
                &[
                    "shop/money.py (modified)",
                    "-> imported by shop/tax.py",
                    "-> imported by shop/report.py",
                    "-> imported by tests/unit/test_report.py",
                ],
            ),
        ])
    );
}

#[test]
fn tests_load_the_conftests_and_package_inits_above_them() {
    let project = shop();

    assert_eq!(
        project.selected_by("tests/conftest.py"),
        ["tests/unit/test_cart.py", "tests/unit/test_report.py"]
    );
    assert_eq!(
        project.selected_by("tests/unit/__init__.py"),
        ["tests/unit/test_cart.py", "tests/unit/test_report.py"]
    );
}

#[test]
fn src_layouts_and_sibling_imports_of_rootless_tests_resolve() {
    let project = TestProject::new(&[
        ("pyproject.toml", "[project]\nname = \"lib\"\n"),
        ("src/lib/__init__.py", ""),
        ("src/lib/parse.py", "def parse(): ...\n"),
        ("tests/helpers.py", "from lib.parse import parse\n"),
        ("tests/test_parse.py", "import helpers\n"),
    ]);

    assert_eq!(
        project.selected_by("src/lib/parse.py"),
        ["tests/test_parse.py"]
    );
}

#[test]
fn namespace_packages_resolve_without_an_init() {
    let project = TestProject::new(&[
        ("acme/plugins/audio.py", "RATE = 1\n"),
        ("tests/test_audio.py", "from acme.plugins import audio\n"),
    ]);

    assert_eq!(
        project.selected_by("acme/plugins/audio.py"),
        ["tests/test_audio.py"]
    );
}

#[test]
fn installed_packages_are_nodes_and_the_standard_library_adds_nothing() {
    let graph = TestProject::new(&[(
        "tests/test_fetch.py",
        "import os\nimport requests.adapters\nfrom yaml import safe_load\n",
    )])
    .link();

    assert_eq!(
        dependents(&graph, "site-packages/requests"),
        [("tests/test_fetch.py", EdgeKind::Import)]
    );
    assert_eq!(
        dependents(&graph, "site-packages/yaml"),
        [("tests/test_fetch.py", EdgeKind::Import)]
    );
    assert!(
        graph
            .file_id(&test_project::repo_path("site-packages/os"))
            .is_none()
    );
}
