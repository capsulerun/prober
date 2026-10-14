//! A Django project tested with pytest-django, written to disk: the settings module every test runs with, the app modules Django loads by name, and the modules only some tests import.

mod test_project;

use prober_core::config::Presets;
use prober_core::frontend::{ChangeSet, Frontend};
use prober_core::graph::{EdgeKind, GraphBuilder};
use prober_lang_python::PythonLinker;

use crate::test_project::{TestProject, dependents, modified, selected};

fn site() -> TestProject {
    TestProject::new(&[
        (
            "pytest.ini",
            "[pytest]\nDJANGO_SETTINGS_MODULE = site.settings\npython_files = tests.py test_*.py\n",
        ),
        ("site/__init__.py", ""),
        (
            "site/settings.py",
            "INSTALLED_APPS = [\"django.contrib.auth\", \"polls\"] + [\"shop.apps.ShopConfig\"]\nROOT_URLCONF = \"site.urls\"\n",
        ),
        (
            "site/urls.py",
            "from django.urls import include, path\n\nurlpatterns = [path(\"polls/\", include(\"polls.urls\"))]\n",
        ),
        ("polls/urls.py", "from . import views\n"),
        ("polls/views.py", "def index(request): ...\n"),
        ("polls/__init__.py", ""),
        ("polls/models.py", "class Question: ...\n"),
        ("polls/admin.py", "from .models import Question\n"),
        ("polls/migrations/__init__.py", ""),
        ("polls/migrations/0001_initial.py", "operations = []\n"),
        ("polls/utils.py", "def slug(): ...\n"),
        ("polls/test_utils.py", "from polls.utils import slug\n"),
        ("shop/__init__.py", ""),
        ("shop/apps.py", "class ShopConfig: ...\n"),
        ("shop/models.py", "class Order: ...\n"),
        ("shop/tests.py", "def test_order(): ...\n"),
    ])
}

#[test]
fn installed_apps_link_the_modules_django_loads_by_name() {
    let graph = site().link();

    for loaded in [
        "polls/__init__.py",
        "polls/models.py",
        "polls/admin.py",
        "polls/migrations/0001_initial.py",
        "shop/apps.py",
        "shop/models.py",
    ] {
        assert!(
            dependents(&graph, loaded).contains(&("site/settings.py", EdgeKind::Dynamic)),
            "{loaded}"
        );
    }

    assert!(
        !dependents(&graph, "polls/utils.py").contains(&("site/settings.py", EdgeKind::Dynamic))
    );
}

#[test]
fn every_test_runs_with_the_settings_and_the_apps_it_installs() {
    let project = site();

    assert_eq!(
        project.outcome_of(&[], &[modified("shop/models.py")]),
        selected(&[
            (
                "polls/test_utils.py",
                &[
                    "shop/models.py (modified)",
                    "-> loaded by INSTALLED_APPS in site/settings.py:1 (inferred installed app shop.apps.ShopConfig)",
                    "-> imported by polls/test_utils.py",
                ],
            ),
            (
                "shop/tests.py",
                &[
                    "shop/models.py (modified)",
                    "-> loaded by INSTALLED_APPS in site/settings.py:1 (inferred installed app shop.apps.ShopConfig)",
                    "-> imported by shop/tests.py",
                ],
            ),
        ])
    );
    assert_eq!(
        project.selected_by("polls/utils.py"),
        ["polls/test_utils.py"]
    );
}

#[test]
fn without_the_preset_the_settings_are_an_ordinary_module() {
    let project = site();
    let repo_files = project.repo_files();
    let mut builder = GraphBuilder::default();

    PythonLinker::new(project.root(), &repo_files)
        .unwrap()
        .with_presets(&Presets::Only(vec!["pytest".to_owned()]))
        .add_links(&ChangeSet::unchanged(&repo_files), &mut builder)
        .unwrap();

    let graph = builder.build();

    assert!(dependents(&graph, "shop/models.py").is_empty());
}
