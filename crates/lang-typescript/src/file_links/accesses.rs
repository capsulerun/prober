//! Where an evaluated file access leads: exact repository paths and globs, located from the working directory or the glob's own `cwd`.

use crate::dynamic::evaluator::FileAccess;
use crate::dynamic::value::{AccessKind, Part, Pattern, Value, property};

const GLOB_CHARACTERS: [char; 4] = ['*', '?', '[', '{'];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Location {
    Path(String),
    Glob(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccessTarget {
    pub kind: AccessKind,
    pub recursive: bool,
    pub location: Location,
}

pub(crate) fn access_targets(access: &FileAccess, working_directory: &Value) -> Vec<AccessTarget> {
    let bases = match property(&access.options, "cwd") {
        Some(cwd) if access.kind == AccessKind::Glob => cwd.patterns(),
        _ => working_directory.patterns(),
    };
    let recursive = access.kind == AccessKind::Copy
        || property(&access.options, "recursive") == Some(Value::Boolean(true));
    let mut targets = Vec::new();

    for pattern in path_patterns(&access.path) {
        for located in locate(&pattern, &bases) {
            let Some(location) = location_of(&located, access.kind) else {
                continue;
            };
            let target = AccessTarget {
                kind: access.kind,
                recursive,
                location,
            };

            if !targets.contains(&target) {
                targets.push(target);
            }
        }
    }

    targets
}

fn path_patterns(path: &Value) -> Vec<Pattern> {
    match path {
        Value::Array(items) | Value::Alternatives(items) => {
            items.iter().flat_map(path_patterns).collect()
        }
        value => value.patterns(),
    }
}

fn locate(pattern: &Pattern, bases: &[Pattern]) -> Vec<Pattern> {
    let pattern = pattern.without_file_scheme();

    match pattern.parts().first() {
        Some(Part::RepoRoot) => vec![pattern.normalized()],
        Some(Part::Text(text)) if !text.starts_with(['/', '!']) && !pattern.is_external() => bases
            .iter()
            .filter(|base| matches!(base.parts().first(), Some(Part::RepoRoot)))
            .map(|base| {
                let mut located = base.parts().to_vec();

                located.push(Part::Text("/".to_owned()));
                located.extend(pattern.parts().iter().cloned());

                Pattern::from_parts(located).normalized()
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn location_of(pattern: &Pattern, kind: AccessKind) -> Option<Location> {
    let [Part::RepoRoot, rest @ ..] = pattern.parts() else {
        return None;
    };
    let mut path = String::new();
    let mut glob = String::new();
    let mut has_hole = false;

    for part in rest {
        match part {
            Part::Text(text) => {
                path.push_str(text);

                if kind == AccessKind::Glob {
                    glob.push_str(text);
                } else {
                    glob.push_str(&escape_glob(text));
                }
            }
            Part::Hole(_) => {
                has_hole = true;

                if !glob.ends_with('*') {
                    glob.push('*');
                }
            }
            Part::RepoRoot | Part::External => return None,
        }
    }

    let path = path.trim_start_matches('/');
    let glob = glob.trim_start_matches('/');
    let is_glob = has_hole || kind == AccessKind::Glob && path.contains(GLOB_CHARACTERS);

    if !is_glob {
        return (!path.is_empty()).then(|| Location::Path(path.to_owned()));
    }

    let has_literal_name = glob
        .chars()
        .any(|character| character != '*' && character != '/');

    has_literal_name.then(|| Location::Glob(glob.to_owned()))
}

fn escape_glob(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '*' | '?' | '[' | ']' | '{' | '}' | '\\' => format!("[{character}]"),
            character => character.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dynamic::value::Unknown;

    fn repository_root() -> Value {
        Value::Strings(vec![Pattern::repo_path("")])
    }

    fn access(kind: AccessKind, path: Value, options: Value) -> FileAccess {
        FileAccess {
            kind,
            path,
            options,
        }
    }

    fn locations(targets: &[AccessTarget]) -> Vec<Location> {
        targets
            .iter()
            .map(|target| target.location.clone())
            .collect()
    }

    fn joined(parts: Vec<Pattern>) -> Value {
        Value::Strings(vec![Pattern::from_parts(
            parts
                .iter()
                .flat_map(|part| part.parts().to_vec())
                .collect(),
        )])
    }

    #[test]
    fn relative_reads_start_from_the_working_directory() {
        let read = access(
            AccessKind::Read,
            Value::text("./scripts/pr-lib/components.txt"),
            Value::runtime_value(),
        );

        assert_eq!(
            locations(&access_targets(&read, &repository_root())),
            [Location::Path("scripts/pr-lib/components.txt".to_owned())]
        );
    }

    #[test]
    fn holes_become_globs_and_unknown_roots_or_the_root_itself_lead_nowhere() {
        let manifests = access(
            AccessKind::Read,
            joined(vec![
                Pattern::repo_path("extensions/"),
                Pattern::hole(Unknown::runtime_value()),
                Pattern::text("/package.json"),
            ]),
            Value::runtime_value(),
        );
        let temporary = access(
            AccessKind::Read,
            joined(vec![
                Pattern::hole(Unknown::runtime_value()),
                Pattern::text("/package.json"),
            ]),
            Value::runtime_value(),
        );
        let everything = access(
            AccessKind::List,
            joined(vec![
                Pattern::repo_path(""),
                Pattern::hole(Unknown::runtime_value()),
            ]),
            Value::runtime_value(),
        );
        let root = access(AccessKind::Copy, Value::text("."), Value::runtime_value());

        assert_eq!(
            locations(&access_targets(&manifests, &repository_root())),
            [Location::Glob("extensions/*/package.json".to_owned())]
        );
        assert_eq!(access_targets(&temporary, &repository_root()), []);
        assert_eq!(access_targets(&everything, &repository_root()), []);
        assert_eq!(access_targets(&root, &repository_root()), []);
    }

    #[test]
    fn globs_keep_their_syntax_and_follow_their_cwd_option() {
        let glob = access(
            AccessKind::Glob,
            Value::Array(vec![
                Value::text("*/openclaw.plugin.json"),
                Value::text("!**/node_modules/**"),
            ]),
            Value::Object(vec![(
                "cwd".to_owned(),
                Value::Strings(vec![Pattern::repo_path("extensions")]),
            )]),
        );

        assert_eq!(
            locations(&access_targets(&glob, &repository_root())),
            [Location::Glob(
                "extensions/*/openclaw.plugin.json".to_owned()
            )]
        );
    }

    #[test]
    fn listings_are_recursive_only_when_asked_and_copies_always_are() {
        let recursive_options = Value::Object(vec![("recursive".to_owned(), Value::Boolean(true))]);
        let listing = access(
            AccessKind::List,
            Value::text("docs"),
            Value::runtime_value(),
        );
        let recursive_listing = access(AccessKind::List, Value::text("docs"), recursive_options);
        let copy = access(
            AccessKind::Copy,
            Value::text("docs"),
            Value::runtime_value(),
        );
        let is_recursive = |file_access: &FileAccess| {
            access_targets(file_access, &repository_root())
                .iter()
                .all(|target| target.recursive)
        };

        assert!(!is_recursive(&listing));
        assert!(is_recursive(&recursive_listing));
        assert!(is_recursive(&copy));
    }
}
