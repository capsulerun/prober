//! Native modules built by maturin: the imports of the module linked to the Rust crate declared for it.

mod test_project;

use prober_core::config::InferenceSettings;
use prober_core::graph::EdgeKind;

use crate::test_project::{TestProject, dependents, modified, selected};

fn mixed_project() -> TestProject {
    TestProject::new(&[
        (
            "python/pyproject.toml",
            "[build-system]\nbuild-backend = \"maturin\"\n\n[tool.maturin]\npython-source = \"python\"\nmodule-name = \"shop._native\"\n",
        ),
        (
            "python/Cargo.toml",
            "[package]\nname = \"shop-python\"\n\n[lib]\ncrate-type = [\"cdylib\"]\n",
        ),
        ("python/src/lib.rs", "pub fn total() {}\n"),
        ("python/python/shop/__init__.py", ""),
        (
            "python/python/shop/_native.pyi",
            "def total() -> int: ...\n",
        ),
        (
            "python/python/shop/cart.py",
            "from shop._native import total\n",
        ),
        ("python/python/shop/pricing.py", "from . import _native\n"),
        (
            "python/python/shop/report.py",
            "from typing import TYPE_CHECKING\n\nif TYPE_CHECKING:\n    from ._native import Report\n",
        ),
        ("python/python/shop/tax.py", "RATE = 1\n"),
        ("python/tests/test_cart.py", "from shop.cart import total\n"),
        ("python/tests/test_tax.py", "from shop.tax import RATE\n"),
    ])
}

#[test]
fn every_import_form_of_the_native_module_links_to_its_crate() {
    let graph = mixed_project().link();

    assert_eq!(
        dependents(&graph, "python/Cargo.toml"),
        [
            ("python/python/shop/cart.py", EdgeKind::Binding),
            ("python/python/shop/pricing.py", EdgeKind::Binding),
        ]
    );
}

#[test]
fn a_crate_change_selects_the_tests_importing_its_module_with_the_declaration() {
    assert_eq!(
        mixed_project().outcome_of(&[], &[modified("python/Cargo.toml")]),
        selected(&[(
            "python/tests/test_cart.py",
            &[
                "python/Cargo.toml (modified)",
                "-> loaded by python/python/shop/cart.py:1 (inferred Python binding shop._native (python/pyproject.toml))",
                "-> imported by python/tests/test_cart.py",
            ],
        )])
    );
}

#[test]
fn module_installed_beside_its_project_is_found_from_any_package() {
    let project = TestProject::new(&[
        (
            "runtime/pyproject.toml",
            "[build-system]\nbuild-backend = \"maturin\"\n\n[tool.maturin]\nmodule-name = \"_runtime._core\"\n",
        ),
        (
            "runtime/Cargo.toml",
            "[package]\nname = \"runtime\"\n\n[lib]\nname = \"_core\"\ncrate-type = [\"cdylib\"]\n",
        ),
        ("runtime/_runtime/__init__.py", "FLAGS = []\n"),
        (
            "py/src/frame/_plr.py",
            "def load():\n    import _runtime._core as plr\n\n    return plr\n",
        ),
        ("py/src/frame/__init__.py", "from frame._plr import load\n"),
        ("py/tests/test_frame.py", "import frame\n"),
    ]);

    assert_eq!(
        dependents(&project.link(), "runtime/Cargo.toml"),
        [("py/src/frame/_plr.py", EdgeKind::Binding)]
    );
    assert_eq!(
        project.selected_by("runtime/Cargo.toml"),
        ["py/tests/test_frame.py"]
    );
}

#[test]
fn bindings_switched_off_link_nothing_to_the_crate() {
    let graph = mixed_project().link_with(InferenceSettings {
        bindings: false,
        ..InferenceSettings::default()
    });

    assert_eq!(dependents(&graph, "python/Cargo.toml"), []);
}
