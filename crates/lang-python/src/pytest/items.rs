//! What pytest finds in one module: its fixtures with the fixtures they request, the fixtures its tests request, the plugins it declares, and fingerprints of each fixture and of the rest of the module.

use std::collections::BTreeSet;
use std::hash::{DefaultHasher, Hash, Hasher};

use prober_core::symbols::LineRange;
use ruff_python_ast::comparable::ComparableStmt;
use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{self as ast, Expr, ModModule, PySourceType, Stmt};
use ruff_python_parser::parse_unchecked_source;
use ruff_text_size::{Ranged, TextSize};

const FIXTURE_DECORATOR_SUFFIX: &str = "fixture";

const USEFIXTURES: &str = "usefixtures";

const GETFIXTUREVALUE: &str = "getfixturevalue";

const PLUGINS_VARIABLE: &str = "pytest_plugins";

const IGNORED_PARAMETERS: [&str; 2] = ["self", "cls"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fixture {
    pub name: String,
    pub lines: LineRange,
    pub hash: u64,
    pub autouse: bool,
    pub requests: BTreeSet<String>,
    pub dynamic_requests: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PytestItems {
    pub fixtures: Vec<Fixture>,
    pub requests: BTreeSet<String>,
    pub dynamic_requests: bool,
    pub plugins: Vec<String>,
    pub module_hash: u64,
    pub module_lines: Vec<LineRange>,
    pub parsed: bool,
}

impl PytestItems {
    pub fn fixture(&self, name: &str) -> Option<&Fixture> {
        self.fixtures.iter().find(|fixture| fixture.name == name)
    }
}

pub(crate) fn parse_items(source: &str) -> PytestItems {
    let parsed = parse_unchecked_source(source, PySourceType::Python);

    if !parsed.errors().is_empty() {
        return PytestItems::default();
    }

    pytest_items(parsed.syntax(), source)
}

pub(crate) fn pytest_items(module: &ModModule, source: &str) -> PytestItems {
    let line_starts = line_starts(source);
    let line_of =
        |offset: TextSize| line_starts.partition_point(|&start| start <= offset.to_usize()) as u32;
    let line_count = line_of(TextSize::of(source)).max(1);
    let mut items = PytestItems {
        parsed: true,
        ..PytestItems::default()
    };
    let mut module_hasher = DefaultHasher::new();
    let mut fixture_lines = Vec::new();

    for statement in &module.body {
        match statement {
            Stmt::FunctionDef(definition)
                if let Some(decorator) = fixture_decorator(definition) =>
            {
                let lines = LineRange {
                    start: line_of(definition.range().start()),
                    end: line_of(definition.range().end()),
                };
                let mut collector = RequestCollector::default();

                collector.visit_stmt(statement);
                fixture_lines.push(lines);
                items.fixtures.push(Fixture {
                    name: decorator
                        .name
                        .unwrap_or_else(|| definition.name.as_str().to_owned()),
                    lines,
                    hash: statement_hash(statement),
                    autouse: decorator.autouse,
                    requests: parameter_names(&definition.parameters)
                        .chain(collector.requests)
                        .collect(),
                    dynamic_requests: collector.dynamic,
                });
            }
            statement => {
                ComparableStmt::from(statement).hash(&mut module_hasher);

                if let Some(plugins) = declared_plugins(statement) {
                    items.plugins.extend(plugins);
                }
            }
        }
    }

    let mut collector = RequestCollector {
        all_parameters: true,
        ..RequestCollector::default()
    };

    collector.visit_body(&module.body);
    items.requests = collector.requests;
    items.dynamic_requests = collector.dynamic;
    items.module_hash = module_hasher.finish();
    items.module_lines = remaining_lines(line_count, &fixture_lines);

    items
}

struct FixtureDecorator {
    name: Option<String>,
    autouse: bool,
}

fn fixture_decorator(definition: &ast::StmtFunctionDef) -> Option<FixtureDecorator> {
    definition.decorator_list.iter().find_map(|decorator| {
        let (callee, call) = match &decorator.expression {
            Expr::Call(call) => (call.func.as_ref(), Some(call)),
            expression => (expression, None),
        };

        if !dotted_name(callee).ends_with(FIXTURE_DECORATOR_SUFFIX) {
            return None;
        }

        let keyword = |name: &str| {
            call.and_then(|call| {
                call.arguments
                    .keywords
                    .iter()
                    .find(|keyword| keyword.arg.as_ref().is_some_and(|arg| arg.as_str() == name))
            })
        };

        Some(FixtureDecorator {
            name: keyword("name").and_then(|keyword| match &keyword.value {
                Expr::StringLiteral(literal) => Some(literal.value.to_str().to_owned()),
                _ => None,
            }),
            autouse: keyword("autouse").is_some_and(|keyword| {
                !matches!(&keyword.value, Expr::BooleanLiteral(literal) if !literal.value)
            }),
        })
    })
}

#[derive(Default)]
struct RequestCollector {
    all_parameters: bool,
    requests: BTreeSet<String>,
    dynamic: bool,
}

impl<'a> Visitor<'a> for RequestCollector {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        if let Stmt::FunctionDef(definition) = statement
            && self.all_parameters
        {
            self.requests
                .extend(parameter_names(&definition.parameters));
        }

        visitor::walk_stmt(self, statement);
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Call(call) = expr {
            let callee = dotted_name(&call.func);
            let method = callee.rsplit('.').next().unwrap_or_default();

            if method == USEFIXTURES || method == GETFIXTUREVALUE {
                for argument in &call.arguments.args {
                    match argument {
                        Expr::StringLiteral(literal) => {
                            self.requests.insert(literal.value.to_str().to_owned());
                        }
                        _ if method == GETFIXTUREVALUE => self.dynamic = true,
                        _ => {}
                    }
                }
            }
        }

        visitor::walk_expr(self, expr);
    }
}

fn parameter_names(parameters: &ast::Parameters) -> impl Iterator<Item = String> + '_ {
    parameters
        .posonlyargs
        .iter()
        .chain(&parameters.args)
        .chain(&parameters.kwonlyargs)
        .map(|parameter| parameter.parameter.name.as_str())
        .filter(|name| !IGNORED_PARAMETERS.contains(name))
        .map(str::to_owned)
}

fn declared_plugins(statement: &Stmt) -> Option<Vec<String>> {
    let (targets, value): (Vec<&Expr>, &Expr) = match statement {
        Stmt::Assign(assign) => (assign.targets.iter().collect(), &assign.value),
        Stmt::AnnAssign(assign) => (vec![assign.target.as_ref()], assign.value.as_deref()?),
        _ => return None,
    };
    let declares = targets
        .iter()
        .any(|target| matches!(target, Expr::Name(name) if name.id.as_str() == PLUGINS_VARIABLE));

    if !declares {
        return None;
    }

    let elements: Vec<&Expr> = match value {
        Expr::List(list) => list.elts.iter().collect(),
        Expr::Tuple(tuple) => tuple.elts.iter().collect(),
        value => vec![value],
    };

    Some(
        elements
            .into_iter()
            .filter_map(|element| match element {
                Expr::StringLiteral(literal) => Some(literal.value.to_str().to_owned()),
                _ => None,
            })
            .collect(),
    )
}

fn dotted_name(expr: &Expr) -> String {
    match expr {
        Expr::Name(name) => name.id.as_str().to_owned(),
        Expr::Attribute(attribute) => {
            format!(
                "{}.{}",
                dotted_name(&attribute.value),
                attribute.attr.as_str()
            )
        }
        _ => String::new(),
    }
}

fn statement_hash(statement: &Stmt) -> u64 {
    let mut hasher = DefaultHasher::new();

    ComparableStmt::from(statement).hash(&mut hasher);

    hasher.finish()
}

fn remaining_lines(line_count: u32, taken: &[LineRange]) -> Vec<LineRange> {
    let mut ranges = Vec::new();
    let mut start = 1;
    let mut taken = taken.to_vec();

    taken.sort_by_key(|range| range.start);

    for range in taken {
        if range.start > start {
            ranges.push(LineRange {
                start,
                end: range.start - 1,
            });
        }

        start = start.max(range.end + 1);
    }

    if start <= line_count {
        ranges.push(LineRange {
            start,
            end: line_count,
        });
    }

    ranges
}

fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];

    starts.extend(
        source
            .bytes()
            .enumerate()
            .filter(|(_, byte)| *byte == b'\n')
            .map(|(offset, _)| offset + 1),
    );

    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFTEST: &str = r#"import pytest

pytest_plugins = ["tests.plugins.network"]


@pytest.fixture
def database(tmp_path):
    return tmp_path


@pytest.fixture(name="client", autouse=True)
def client_fixture(database, request):
    return request.getfixturevalue("settings")


def pytest_configure(config): ...
"#;

    #[test]
    fn fixtures_their_requests_and_the_plugins_a_module_declares() {
        let items = parse_items(CONFTEST);
        let names: Vec<&str> = items
            .fixtures
            .iter()
            .map(|fixture| fixture.name.as_str())
            .collect();
        let client = items.fixture("client").unwrap();

        assert_eq!(names, ["database", "client"]);
        assert!(client.autouse);
        assert_eq!(
            client.requests,
            BTreeSet::from([
                "database".to_owned(),
                "request".to_owned(),
                "settings".to_owned()
            ])
        );
        assert_eq!(items.plugins, ["tests.plugins.network"]);
        assert_eq!(
            items.module_lines,
            [
                LineRange { start: 1, end: 5 },
                LineRange { start: 9, end: 10 },
                LineRange { start: 14, end: 17 }
            ]
        );
    }

    #[test]
    fn fingerprints_ignore_comments_and_follow_each_fixture() {
        let base = parse_items(CONFTEST);
        let commented = parse_items(
            &CONFTEST.replace("    return tmp_path", "    # path\n    return tmp_path"),
        );
        let changed = parse_items(&CONFTEST.replace("return tmp_path", "return tmp_path / \"db\""));

        assert_eq!(
            base.fixture("database").unwrap().hash,
            commented.fixture("database").unwrap().hash
        );
        assert_ne!(
            base.fixture("database").unwrap().hash,
            changed.fixture("database").unwrap().hash
        );
        assert_eq!(
            base.fixture("client").unwrap().hash,
            changed.fixture("client").unwrap().hash
        );
        assert_eq!(base.module_hash, changed.module_hash);
    }

    #[test]
    fn a_test_module_requests_its_parameters_marks_and_dynamic_lookups() {
        let items = parse_items(
            "import pytest\n\npytestmark = pytest.mark.usefixtures(\"network\")\n\n\nclass TestApi:\n    def test_get(self, client): ...\n\n\ndef test_any(request, name):\n    request.getfixturevalue(name)\n",
        );

        assert_eq!(
            items.requests,
            BTreeSet::from([
                "client".to_owned(),
                "name".to_owned(),
                "network".to_owned(),
                "request".to_owned()
            ])
        );
        assert!(items.dynamic_requests);
    }
}
