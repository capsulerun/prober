//! The Django preset: the settings modules tests run with, and the modules of each installed app that Django loads by name.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use prober_core::files::RepoPath;
use prober_core::graph::{EdgeKind, Inference};
use ruff_python_ast::{Expr, ModModule, Operator, Stmt};

use crate::conventions::{CONFTEST, PACKAGE_INIT, join_directory, parent_directory, split_path};
use crate::links::Link;
use crate::modules::ModuleIndex;
use crate::pytest::configs::{PytestConfigs, SETTINGS_OPTION};
use crate::summary::ModuleSummary;

const MANAGE_PY: &str = "manage.py";

const INSTALLED_APPS: &str = "INSTALLED_APPS";

const INSTALLED_APP: &str = "installed app";

const APP_MODULES: [&str; 4] = ["apps", "models", "admin", "signals"];

const APP_PACKAGES: [&str; 3] = ["models", "migrations", "templatetags"];

const MAX_SETTINGS_DEPTH: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Settings {
    pub module: RepoPath,
    pub directory: String,
}

pub(crate) fn setting_lists(module: &ModModule) -> BTreeMap<String, Vec<String>> {
    let mut lists: BTreeMap<String, Vec<String>> = BTreeMap::new();

    collect_lists(&module.body, &mut lists);

    lists
}

fn collect_lists(body: &[Stmt], lists: &mut BTreeMap<String, Vec<String>>) {
    for statement in body {
        let (target, value) = match statement {
            Stmt::Assign(assign) => match assign.targets.as_slice() {
                [target] => (target, assign.value.as_ref()),
                _ => continue,
            },
            Stmt::AnnAssign(assign) => match &assign.value {
                Some(value) => (assign.target.as_ref(), value.as_ref()),
                None => continue,
            },
            Stmt::AugAssign(assign) if assign.op == Operator::Add => {
                (assign.target.as_ref(), assign.value.as_ref())
            }
            Stmt::If(if_statement) => {
                collect_lists(&if_statement.body, lists);

                for clause in &if_statement.elif_else_clauses {
                    collect_lists(&clause.body, lists);
                }

                continue;
            }
            Stmt::Try(try_statement) => {
                collect_lists(&try_statement.body, lists);
                collect_lists(&try_statement.orelse, lists);

                continue;
            }
            _ => continue,
        };
        let Expr::Name(name) = target else {
            continue;
        };

        if !name
            .id
            .as_str()
            .chars()
            .all(|character| character.is_ascii_uppercase() || character == '_')
        {
            continue;
        }

        let mut strings = Vec::new();

        if list_strings(value, &mut strings) {
            lists
                .entry(name.id.as_str().to_owned())
                .or_default()
                .extend(strings);
        }
    }
}

fn list_strings(value: &Expr, strings: &mut Vec<String>) -> bool {
    match value {
        Expr::List(list) => elements(&list.elts, strings),
        Expr::Tuple(tuple) => elements(&tuple.elts, strings),
        Expr::BinOp(binary) if binary.op == Operator::Add => {
            list_strings(&binary.left, strings) | list_strings(&binary.right, strings)
        }
        _ => false,
    }
}

fn elements(elements: &[Expr], strings: &mut Vec<String>) -> bool {
    for element in elements {
        if let Expr::StringLiteral(literal) = element {
            strings.push(literal.value.to_str().to_owned());
        }
    }

    true
}

pub(crate) fn settings_modules(
    configs: &PytestConfigs,
    index: &ModuleIndex,
    repo_files: &[RepoPath],
    read: &dyn Fn(&RepoPath) -> Option<String>,
) -> Vec<Settings> {
    let mut settings = Vec::new();
    let mut add = |declarer: &RepoPath, dotted: &str, directory: &str| {
        if let Some(resolved) = index.resolve_module(declarer, dotted).into_iter().next() {
            settings.push(Settings {
                module: resolved.module,
                directory: directory.to_owned(),
            });
        }
    };

    for config in configs.configs() {
        if let Some(dotted) = configs.settings_module(config) {
            add(&config.path, &dotted, &config.directory);
        }
    }

    for file in repo_files {
        let name = split_path(file.as_str()).1;

        if name != CONFTEST && name != MANAGE_PY {
            continue;
        }

        let Some(text) = read(file) else {
            continue;
        };

        for dotted in declared_settings(&text) {
            add(file, &dotted, parent_directory(file.as_str()));
        }
    }

    settings.sort_by(|left, right| {
        (&left.directory, &left.module).cmp(&(&right.directory, &right.module))
    });
    settings.dedup();

    settings
}

fn declared_settings(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| line.contains(SETTINGS_OPTION))
        .flat_map(|line| {
            line.split(['"', '\''])
                .skip(1)
                .step_by(2)
                .filter(|literal| *literal != SETTINGS_OPTION && literal.contains('.'))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

pub(crate) fn installed_app_links(
    settings: &[Settings],
    summaries: &HashMap<RepoPath, ModuleSummary>,
    index: &ModuleIndex,
) -> Vec<Link> {
    let mut links = Vec::new();

    for setting in settings {
        let mut apps = BTreeSet::new();

        collect_apps(&setting.module, summaries, 0, &mut apps);

        for app in apps {
            for file in app_files(&setting.module, &app, index) {
                if file == setting.module {
                    continue;
                }

                links.push(Link {
                    reader: setting.module.clone(),
                    file,
                    kind: EdgeKind::Dynamic,
                    inference: Inference {
                        line: 1,
                        kind: INSTALLED_APP,
                        pattern: app.clone(),
                        detail: None,
                    },
                });
            }
        }
    }

    links
}

fn collect_apps(
    module: &RepoPath,
    summaries: &HashMap<RepoPath, ModuleSummary>,
    depth: usize,
    apps: &mut BTreeSet<String>,
) {
    let Some(summary) = summaries.get(module) else {
        return;
    };

    apps.extend(
        summary
            .setting_lists
            .get(INSTALLED_APPS)
            .into_iter()
            .flatten()
            .cloned(),
    );

    if depth >= MAX_SETTINGS_DEPTH {
        return;
    }

    for import in &summary.imports {
        if let crate::modules::ImportTarget::Repo(files) = &import.target {
            for file in files
                .iter()
                .filter(|file| split_path(file.as_str()).1 != PACKAGE_INIT)
            {
                collect_apps(file, summaries, depth + 1, apps);
            }
        }
    }
}

fn app_files(settings: &RepoPath, app: &str, index: &ModuleIndex) -> Vec<RepoPath> {
    let package = match app.split_once(".apps.") {
        Some((package, _)) => package,
        None => app,
    };
    let Some(resolved) = index.resolve_module(settings, package).into_iter().next() else {
        return Vec::new();
    };
    let (directory, name) = split_path(resolved.module.as_str());

    if name != PACKAGE_INIT {
        return vec![resolved.module];
    }

    let mut files = vec![resolved.module.clone()];

    for module in APP_MODULES {
        let file = join_directory(directory, &format!("{module}.py"));

        if index.contains(&file)
            && let Ok(file) = RepoPath::new(file)
        {
            files.push(file);
        }
    }

    for package in APP_PACKAGES {
        let prefix = join_directory(directory, package);

        files.extend(
            index
                .files_under(&prefix)
                .filter_map(|file| RepoPath::new(file).ok()),
        );
    }

    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    #[test]
    fn uppercase_lists_of_strings_are_collected_with_their_additions() {
        let source = "INSTALLED_APPS = [\"django.contrib.auth\", \"polls\"] + [\"shop.apps.ShopConfig\"]\nif DEBUG:\n    INSTALLED_APPS += (\"debug_toolbar\",)\nmiddleware = [\"lower.case\"]\n";
        let parsed = parse_unchecked_source(source, PySourceType::Python);
        let lists = setting_lists(parsed.syntax());

        assert_eq!(
            lists[INSTALLED_APPS],
            [
                "django.contrib.auth",
                "polls",
                "shop.apps.ShopConfig",
                "debug_toolbar"
            ]
        );
        assert!(!lists.contains_key("middleware"));
    }

    #[test]
    fn settings_declared_next_to_the_variable_name_are_found() {
        assert_eq!(
            declared_settings(
                "os.environ.setdefault(\"DJANGO_SETTINGS_MODULE\", \"site.settings.test\")\n"
            ),
            ["site.settings.test"]
        );
    }
}
