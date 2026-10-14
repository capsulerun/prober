//! Python per function on projects written to disk: a change selects the tests using the functions and classes it changed, through re-exports, attribute chains, star imports and lazy module attributes, while load-time code, registrations and dynamic namespaces still reach every loader.

mod test_project;

use prober_core::config::InferenceSettings;

use crate::test_project::{Outcome, TestProject, modified, selected};

const UTILS: &str = "def check_separator(separator):\n    if len(separator) != 1:\n        raise ValueError(separator)\n\n\ndef normalize(path):\n    return str(path)\n";

const CORE: &str = "class Frame:\n    def __init__(self, rows):\n        self.rows = rows\n\n    def height(self):\n        return len(self.rows)\n\n\ndef concat(frames):\n    return Frame([row for frame in frames for row in frame.rows])\n";

const CONFIG: &str = "TIMEOUT = 5\nREGISTRY = {}\n\n\ndef register(name):\n    def decorate(function):\n        REGISTRY[name] = function\n        return function\n\n    return decorate\n";

const PLUGINS: &str = "from frame.config import register\n\n\n@register(\"upper\")\ndef upper(text):\n    return text.upper()\n\n\ndef lower(text):\n    return text.lower()\n";

const INIT: &str = "from frame.io.csv import read_csv\nfrom frame.io import fast\nfrom frame.core import Frame\nfrom frame import config\n";

fn frame() -> TestProject {
    TestProject::new(&[
        ("frame/__init__.py", INIT),
        ("frame/io/__init__.py", ""),
        (
            "frame/io/csv.py",
            "from frame.core import Frame\nfrom frame.io._utils import check_separator\n\n\ndef read_csv(path, separator=\",\"):\n    check_separator(separator)\n    return Frame([path])\n",
        ),
        (
            "frame/io/fast.py",
            "from frame.core import Frame\n\n\ndef read_csv(path, separator=\",\"):\n    return Frame([path])\n",
        ),
        ("frame/io/_utils.py", UTILS),
        ("frame/core.py", CORE),
        ("frame/config.py", CONFIG),
        ("frame/plugins.py", PLUGINS),
        (
            "tests/test_csv.py",
            "import frame as fr\n\n\ndef test_read_csv():\n    assert fr.read_csv(\"a.csv\").height() == 1\n",
        ),
        (
            "tests/test_concat.py",
            "from frame.core import concat\n\n\ndef test_concat():\n    assert concat([]).rows == []\n",
        ),
        (
            "tests/test_registry.py",
            "import frame.plugins\nfrom frame.config import REGISTRY\n\n\ndef test_upper():\n    assert REGISTRY[\"upper\"](\"a\") == \"A\"\n",
        ),
        (
            "tests/test_lower.py",
            "from frame.plugins import lower\n\n\ndef test_lower():\n    assert lower(\"A\") == \"a\"\n",
        ),
    ])
}

fn changed(project: &TestProject, path: &str, base: &str, head: &str) -> Outcome {
    project.write(path, head);
    project.outcome_of(&[(path, base)], &[modified(path)])
}

fn changed_tests(project: &TestProject, path: &str, base: &str, head: &str) -> Vec<String> {
    match changed(project, path, base, head) {
        Outcome::Selected(selected_tests) => {
            let mut tests: Vec<String> = selected_tests.into_iter().map(|(test, _)| test).collect();

            tests.sort();

            tests
        }
        outcome => panic!("expected a selection, got {outcome:?}"),
    }
}

#[test]
fn a_change_inside_one_function_selects_the_tests_calling_it_through_the_package() {
    let head = UTILS.replace("!= 1", "> 1");

    assert_eq!(
        changed(&frame(), "frame/io/_utils.py", UTILS, &head),
        selected(&[(
            "tests/test_csv.py",
            &[
                "frame/io/_utils.py (modified)",
                "-> check_separator changed",
                "-> check_separator used by read_csv in frame/io/csv.py",
                "-> read_csv used by test_read_csv in tests/test_csv.py",
                "-> test_read_csv run by tests/test_csv.py",
            ],
        )])
    );
    assert!(
        changed_tests(
            &frame(),
            "frame/io/_utils.py",
            UTILS,
            &UTILS.replace("str(path)", "path")
        )
        .is_empty()
    );
}

#[test]
fn a_method_change_reaches_the_readers_of_its_name_and_a_constructor_every_user() {
    let head = CORE.replace("len(self.rows)", "len(self.rows) + 0");

    assert_eq!(
        changed_tests(&frame(), "frame/core.py", CORE, &head),
        ["tests/test_csv.py"]
    );

    let head = CORE.replace("self.rows = rows", "self.rows = list(rows)");

    assert_eq!(
        changed_tests(&frame(), "frame/core.py", CORE, &head),
        ["tests/test_concat.py", "tests/test_csv.py"]
    );
}

#[test]
fn a_registered_function_reaches_every_test_loading_its_module() {
    let project = frame();

    assert_eq!(
        changed_tests(
            &project,
            "frame/plugins.py",
            PLUGINS,
            &PLUGINS.replace("text.upper()", "text.upper().strip()")
        ),
        ["tests/test_lower.py", "tests/test_registry.py"]
    );
    assert_eq!(
        changed_tests(
            &frame(),
            "frame/plugins.py",
            PLUGINS,
            &PLUGINS.replace("text.lower()", "text.lower().strip()")
        ),
        ["tests/test_lower.py"]
    );
}

#[test]
fn load_time_code_reaches_every_loader_and_a_comment_reaches_none() {
    let load_time = format!("{CONFIG}\nREGISTRY.clear()\n");

    assert_eq!(
        changed_tests(&frame(), "frame/config.py", CONFIG, &load_time),
        [
            "tests/test_concat.py",
            "tests/test_csv.py",
            "tests/test_lower.py",
            "tests/test_registry.py"
        ]
    );
    assert!(
        changed_tests(
            &frame(),
            "frame/config.py",
            CONFIG,
            &format!("# Settings.\n{CONFIG}")
        )
        .is_empty()
    );
}

#[test]
fn a_rebound_name_reaches_the_tests_using_it_through_the_package() {
    let head = INIT
        .replace(
            "from frame.io.csv import read_csv",
            "from frame.io.fast import read_csv",
        )
        .replace("from frame.io import fast", "from frame.io import csv");

    assert_eq!(
        changed_tests(&frame(), "frame/__init__.py", INIT, &head),
        ["tests/test_csv.py"]
    );
}

#[test]
fn symbols_switched_off_leave_python_modules_at_file_level() {
    let graph = frame().link_with(InferenceSettings {
        symbols: false,
        ..InferenceSettings::default()
    });

    assert_eq!(graph.symbol_count(), 0);
    assert!(frame().link().symbol_count() > 0);
}

#[test]
fn star_imports_and_lazy_attributes_resolve_to_the_symbols_behind_them() {
    let project = TestProject::new(&[
        (
            "kit/__init__.py",
            "from kit.impl import *\n\n\ndef __getattr__(name):\n    import importlib\n\n    return getattr(importlib.import_module(\"kit.lazy\"), name)\n",
        ),
        (
            "kit/impl.py",
            "def shout(text):\n    return text.upper()\n\n\ndef whisper(text):\n    return text.lower()\n",
        ),
        ("kit/lazy.py", "def later():\n    return 1\n"),
        (
            "tests/test_shout.py",
            "import kit\n\n\ndef test_shout():\n    assert kit.shout(\"a\") == \"A\"\n",
        ),
        (
            "tests/test_later.py",
            "import kit\n\n\ndef test_later():\n    assert kit.later() == 1\n",
        ),
    ]);
    let impl_base = "def shout(text):\n    return text.upper()\n\n\ndef whisper(text):\n    return text.lower()\n";

    assert_eq!(
        changed_tests(
            &project,
            "kit/impl.py",
            impl_base,
            &impl_base.replace("text.upper()", "text.upper() + \"!\"")
        ),
        ["tests/test_shout.py"]
    );
    assert!(
        changed_tests(
            &project,
            "kit/impl.py",
            impl_base,
            &impl_base.replace("text.lower()", "text.lower() + \".\"")
        )
        .is_empty()
    );
    assert_eq!(
        changed_tests(
            &project,
            "kit/lazy.py",
            "def later():\n    return 1\n",
            "def later():\n    return 2\n"
        ),
        ["tests/test_later.py"]
    );
}

#[test]
fn subclasses_of_a_registering_base_reach_every_loader_and_plain_ones_their_users() {
    let shapes = "class Registry:\n    kinds = {}\n\n    def __init_subclass__(cls, **kwargs):\n        Registry.kinds[cls.__name__] = cls\n\n\nclass Square(Registry):\n    def area(self):\n        return 4\n\n\nclass Plain:\n    pass\n\n\nclass Circle(Plain):\n    def perimeter(self):\n        return 3\n";
    let project = TestProject::new(&[
        ("shapes.py", shapes),
        (
            "tests/test_registry.py",
            "from shapes import Registry\n\n\ndef test_kinds():\n    assert Registry.kinds[\"Square\"]().area() == 4\n",
        ),
        (
            "tests/test_plain.py",
            "from shapes import Plain\n\n\ndef test_plain():\n    assert Plain()\n",
        ),
    ]);

    assert_eq!(
        changed_tests(
            &project,
            "shapes.py",
            shapes,
            &shapes.replace("return 4", "return 5")
        ),
        ["tests/test_plain.py", "tests/test_registry.py"]
    );
    assert!(
        changed_tests(
            &project,
            "shapes.py",
            shapes,
            &shapes.replace("return 3", "return 2")
        )
        .is_empty()
    );
}

#[test]
fn a_function_level_import_links_the_function_to_what_it_imports() {
    let project = TestProject::new(&[
        ("app/__init__.py", ""),
        (
            "app/report.py",
            "def render():\n    from app.format import title\n\n    return title(\"x\")\n",
        ),
        (
            "app/format.py",
            "def title(text):\n    return text.title()\n",
        ),
        (
            "tests/test_report.py",
            "from app.report import render\n\n\ndef test_render():\n    assert render() == \"X\"\n",
        ),
    ]);

    assert_eq!(
        changed_tests(
            &project,
            "app/format.py",
            "def title(text):\n    return text.title()\n",
            "def title(text):\n    return text.upper()\n"
        ),
        ["tests/test_report.py"]
    );
}

#[test]
fn names_written_into_the_module_namespace_reach_their_readers() {
    let module = "def make(name):\n    globals()[name] = lambda: name\n\n\nmake(\"alpha\")\n\n\ndef beta():\n    return 2\n";
    let project = TestProject::new(&[
        ("names.py", module),
        (
            "tests/test_alpha.py",
            "from names import alpha\n\n\ndef test_alpha():\n    assert alpha() == \"alpha\"\n",
        ),
    ]);

    assert_eq!(
        changed(
            &project,
            "names.py",
            module,
            &module.replace("lambda: name", "lambda: name.upper()")
        ),
        selected(&[(
            "tests/test_alpha.py",
            &[
                "names.py (modified)",
                "-> make changed",
                "-> make used at load by names.py",
                "-> loaded by tests/test_alpha.py",
            ],
        )])
    );
}

#[test]
fn a_namespace_handed_to_other_code_is_analysed_at_file_level() {
    let module = "from registry import fill\n\nfill(globals())\n\n\ndef beta():\n    return 2\n";
    let project = TestProject::new(&[
        (
            "registry.py",
            "def fill(namespace):\n    namespace[\"alpha\"] = 1\n",
        ),
        ("names.py", module),
        (
            "tests/test_beta.py",
            "from names import beta\n\n\ndef test_beta():\n    assert beta() == 2\n",
        ),
    ]);

    assert_eq!(
        changed(
            &project,
            "names.py",
            module,
            &module.replace("return 2", "return 3")
        ),
        selected(&[(
            "tests/test_beta.py",
            &[
                "names.py (modified)",
                "-> used whole at load by tests/test_beta.py (analysed at file level: globals() or sys.modules)",
            ],
        )])
    );
}

#[test]
fn a_class_decorator_walking_its_class_makes_only_that_class_whole() {
    let shapes = "def dispatch(cls):\n    for name in dir(cls):\n        getattr(cls, name)\n\n    return cls\n\n\n@dispatch\nclass Square:\n    def area(self):\n        return 4\n\n\nclass Circle:\n    def area(self):\n        return 3\n";
    let project = TestProject::new(&[
        ("shapes.py", shapes),
        (
            "tests/test_square.py",
            "from shapes import Square\n\n\ndef test_square():\n    assert Square()\n",
        ),
        (
            "tests/test_circle.py",
            "from shapes import Circle\n\n\ndef test_circle():\n    assert Circle()\n",
        ),
    ]);

    assert_eq!(
        changed_tests(
            &project,
            "shapes.py",
            shapes,
            &shapes.replace("return 4", "return 5")
        ),
        ["tests/test_square.py"]
    );
    assert!(
        changed_tests(
            &project,
            "shapes.py",
            shapes,
            &shapes.replace("return 3", "return 2")
        )
        .is_empty()
    );
}

#[test]
fn a_computed_name_passed_as_a_literal_by_the_callers_of_a_method_reaches_only_that_name() {
    let library = "class Frame:\n    def write(self):\n        return 1\n\n\nclass Series:\n    def __init__(self, s):\n        self._s = s\n\n    def _comp(self, other, op):\n        return getattr(self._s, op)(other)\n\n    def __eq__(self, other):\n        return self._comp(other, \"eq\")\n";
    let project = TestProject::new(&[
        ("library.py", library),
        (
            "tests/test_series.py",
            "from library import Series\n\n\ndef test_eq():\n    assert Series(1) == Series(1)\n",
        ),
    ]);

    assert!(
        changed_tests(
            &project,
            "library.py",
            library,
            &library.replace("return 1", "return 2")
        )
        .is_empty()
    );
}
