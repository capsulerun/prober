//! Where an evaluated load can lead: outside the repository, one repository path, a bare specifier, or a glob over the repository files.

use anyhow::Result;
use globset::{GlobBuilder, GlobSetBuilder};
use prober_core::files::RepoPath;
use prober_core::graph::UnresolvedCause;

use crate::dynamic::evaluator::{Load, LoadBase, repo_module};
use crate::dynamic::value::{Part, Pattern, Unknown, Value};

const MODULE_EXTENSIONS: [&str; 9] = [
    ".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".json",
];

const TYPESCRIPT_EQUIVALENTS: [(&str, &[&str]); 4] = [
    (".js", &[".ts", ".tsx"]),
    (".jsx", &[".tsx"]),
    (".mjs", &[".mts"]),
    (".cjs", &[".cts"]),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    Outside,
    RepoPath(String),
    Bare {
        importer: Option<RepoPath>,
        specifier: String,
    },
    Glob(GlobTarget),
    Program {
        name: String,
        directories: Vec<String>,
    },
    Unresolved(UnresolvedCause),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GlobTarget {
    pub glob: String,
    pub folder_segment: Option<usize>,
}

pub(crate) fn targets_of(load: &Load) -> Vec<Target> {
    if let LoadBase::Program(directories) = &load.base {
        return load
            .specifier
            .single_literal()
            .map(|name| Target::Program {
                name,
                directories: directories
                    .iter()
                    .filter_map(|directory| literal_path(directory.parts().get(1..)?))
                    .collect(),
            })
            .into_iter()
            .collect();
    }

    let patterns = match &load.specifier {
        Value::Strings(patterns) => patterns.clone(),
        Value::Unknown(unknown) => return vec![Target::Unresolved(unknown.cause)],
        _ => return vec![Target::Unresolved(UnresolvedCause::RuntimeValue)],
    };
    let mut targets = Vec::new();

    for pattern in patterns {
        for located in located_patterns(&pattern.without_file_scheme(), &load.base) {
            let target = target_of(&located, &load.base);

            if !targets.contains(&target) {
                targets.push(target);
            }
        }
    }

    targets
}

fn located_patterns(pattern: &Pattern, base: &LoadBase) -> Vec<Pattern> {
    if pattern.is_external() || pattern.is_absolute() {
        return vec![pattern.normalized()];
    }

    let relative_to_base = pattern.is_relative();
    let directories = match base {
        LoadBase::Process(directories) | LoadBase::Program(directories) => directories.clone(),
        LoadBase::Module(module) if relative_to_base => {
            vec![Pattern::repo_path(module.as_str()).directory()]
        }
        LoadBase::Require(bases) if relative_to_base => bases
            .iter()
            .map(|base| base.normalized().directory())
            .collect(),
        LoadBase::WorkingDirectory => vec![Pattern::hole(Unknown::runtime_value())],
        LoadBase::Module(_) | LoadBase::Require(_) => return vec![pattern.clone()],
    };

    directories
        .into_iter()
        .map(|directory| {
            let mut located = directory.parts().to_vec();

            located.push(Part::Text("/".to_owned()));
            located.extend(pattern.parts().iter().cloned());

            Pattern::from_parts(located).normalized()
        })
        .collect()
}

fn target_of(pattern: &Pattern, base: &LoadBase) -> Target {
    match pattern.parts() {
        [] => Target::Unresolved(UnresolvedCause::RuntimeValue),
        [Part::External, ..] => Target::Outside,
        _ if pattern.is_external() => Target::Outside,
        [Part::RepoRoot, rest @ ..] => match literal_path(rest) {
            Some(path) if path.is_empty() => Target::Unresolved(UnresolvedCause::NoMatchingFile),
            Some(path) => Target::RepoPath(path),
            None => Target::Glob(GlobTarget {
                glob: glob_of(&segments(rest)),
                folder_segment: folder_segment(&segments(rest)),
            }),
        },
        [Part::Hole(unknown)] => Target::Unresolved(unknown.cause),
        [Part::Hole(_), ..] => unanchored_glob(pattern.parts()),
        [Part::Text(text), ..] if text.starts_with('/') => {
            let mut unanchored = vec![Part::Hole(Unknown::runtime_value())];

            unanchored.extend(pattern.parts().iter().cloned());

            unanchored_glob(&unanchored)
        }
        parts => match literal_path(parts) {
            Some(specifier) => Target::Bare {
                importer: match base {
                    LoadBase::Module(module) => Some(module.clone()),
                    LoadBase::Require(bases) => bases.first().and_then(repo_module),
                    LoadBase::WorkingDirectory | LoadBase::Process(_) | LoadBase::Program(_) => {
                        None
                    }
                },
                specifier,
            },
            None => Target::Unresolved(first_cause(parts)),
        },
    }
}

fn unanchored_glob(parts: &[Part]) -> Target {
    let segments = segments(parts);
    let has_literal_segment = segments.iter().skip(1).any(|segment| {
        !segment.is_empty() && segment.iter().all(|piece| matches!(piece, Piece::Text(_)))
    });

    if !has_literal_segment {
        return Target::Unresolved(first_cause(parts));
    }

    let glob = match segments.first().map(Vec::as_slice) {
        Some([Piece::Hole]) => format!("**/{}", glob_of(&segments[1..])),
        _ => format!("**/{}", glob_of(&segments)),
    };

    Target::Glob(GlobTarget {
        glob,
        folder_segment: None,
    })
}

fn first_cause(parts: &[Part]) -> UnresolvedCause {
    parts
        .iter()
        .find_map(|part| match part {
            Part::Hole(unknown) if unknown.cause == UnresolvedCause::Parameter => {
                Some(UnresolvedCause::Parameter)
            }
            _ => None,
        })
        .unwrap_or(UnresolvedCause::TooWide)
}

fn literal_path(parts: &[Part]) -> Option<String> {
    let mut path = String::new();

    for part in parts {
        let Part::Text(text) = part else {
            return None;
        };

        path.push_str(text);
    }

    Some(path.trim_start_matches('/').to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Text(String),
    Hole,
}

fn segments(parts: &[Part]) -> Vec<Vec<Piece>> {
    let mut segments: Vec<Vec<Piece>> = vec![Vec::new()];

    for part in parts {
        match part {
            Part::Text(text) => {
                for (position, piece) in text.split('/').enumerate() {
                    if position > 0 {
                        segments.push(Vec::new());
                    }

                    if !piece.is_empty() {
                        segments
                            .last_mut()
                            .expect("segments start non-empty")
                            .push(Piece::Text(piece.to_owned()));
                    }
                }
            }
            Part::Hole(_) | Part::RepoRoot | Part::External => {
                let segment = segments.last_mut().expect("segments start non-empty");

                if segment.last() != Some(&Piece::Hole) {
                    segment.push(Piece::Hole);
                }
            }
        }
    }

    segments.retain(|segment| !segment.is_empty());

    segments
}

fn glob_of(segments: &[Vec<Piece>]) -> String {
    segments
        .iter()
        .map(|segment| {
            segment
                .iter()
                .map(|piece| match piece {
                    Piece::Text(text) => escape_glob(text),
                    Piece::Hole => "*".to_owned(),
                })
                .collect::<String>()
        })
        .collect::<Vec<String>>()
        .join("/")
}

fn folder_segment(segments: &[Vec<Piece>]) -> Option<usize> {
    let last = segments.len().checked_sub(1)?;

    segments[..last]
        .iter()
        .position(|segment| segment.as_slice() == [Piece::Hole])
}

fn escape_glob(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '*' | '?' | '[' | ']' | '{' | '}' | '\\' => format!("[{character}]"),
            character => character.to_string(),
        })
        .collect()
}

pub(crate) fn specifier_forms(path: &RepoPath) -> Vec<String> {
    let path = path.as_str();
    let mut forms = vec![path.to_owned()];
    let Some((stem, extension)) = path
        .rsplit_once('.')
        .filter(|(stem, _)| !stem.ends_with('/'))
    else {
        return forms;
    };
    let extension = format!(".{extension}");

    if MODULE_EXTENSIONS.contains(&extension.as_str()) {
        forms.push(stem.to_owned());

        if let Some(directory) = stem.strip_suffix("/index") {
            forms.push(directory.to_owned());
        }
    }

    for (javascript_extension, typescript_extensions) in TYPESCRIPT_EQUIVALENTS {
        if typescript_extensions.contains(&extension.as_str()) {
            forms.push(format!("{stem}{javascript_extension}"));
        }
    }

    forms
}

pub(crate) fn match_globs(globs: &[String], candidates: &[RepoPath]) -> Result<Vec<Vec<RepoPath>>> {
    let mut unique_globs: Vec<&str> = Vec::new();
    let mut glob_slots = Vec::with_capacity(globs.len());

    for glob in globs {
        let slot = match unique_globs
            .iter()
            .position(|unique_glob| *unique_glob == glob)
        {
            Some(slot) => slot,
            None => {
                unique_globs.push(glob);
                unique_globs.len() - 1
            }
        };

        glob_slots.push(slot);
    }

    let mut glob_set_builder = GlobSetBuilder::new();

    for unique_glob in &unique_globs {
        glob_set_builder.add(
            GlobBuilder::new(unique_glob)
                .literal_separator(true)
                .build()?,
        );
    }

    let glob_set = glob_set_builder.build()?;
    let mut unique_matches: Vec<Vec<RepoPath>> = vec![Vec::new(); unique_globs.len()];

    for candidate in candidates {
        for form in specifier_forms(candidate) {
            for slot in glob_set.matches(&form) {
                if unique_matches[slot].last() != Some(candidate) {
                    unique_matches[slot].push(candidate.clone());
                }
            }
        }
    }

    Ok(glob_slots
        .into_iter()
        .map(|slot| unique_matches[slot].clone())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn import_load(module: &str, patterns: Vec<Pattern>) -> Load {
        Load {
            specifier: Value::Strings(patterns),
            base: LoadBase::Module(RepoPath::new(module).unwrap()),
        }
    }

    fn hole() -> Pattern {
        Pattern::hole(Unknown::runtime_value())
    }

    fn pattern(parts: Vec<Pattern>) -> Pattern {
        Pattern::from_parts(
            parts
                .iter()
                .flat_map(|part| part.parts().to_vec())
                .collect(),
        )
    }

    fn glob(glob: &str, folder_segment: Option<usize>) -> Target {
        Target::Glob(GlobTarget {
            glob: glob.to_owned(),
            folder_segment,
        })
    }

    #[test]
    fn relative_specifier_with_a_hole_is_a_glob_from_the_module_directory() {
        let load = import_load(
            "src/plugins/loader.ts",
            vec![pattern(vec![
                Pattern::text("../extensions/"),
                hole(),
                Pattern::text("/index.js"),
            ])],
        );

        assert_eq!(
            targets_of(&load),
            [glob("src/extensions/*/index.js", Some(2))]
        );
    }

    #[test]
    fn literal_repository_path_is_resolved_as_one_path() {
        let load = import_load("src/a.ts", vec![Pattern::repo_path("src/b.runtime.js")]);

        assert_eq!(
            targets_of(&load),
            [Target::RepoPath("src/b.runtime.js".to_owned())]
        );
    }

    #[test]
    fn unknown_root_keeps_its_literal_tail() {
        let load = import_load(
            "src/a.ts",
            vec![pattern(vec![hole(), Pattern::text("/src/cli/profile.ts")])],
        );

        assert_eq!(targets_of(&load), [glob("**/src/cli/profile.ts", None)]);
    }

    #[test]
    fn unknown_root_without_a_literal_segment_is_too_wide() {
        let load = import_load(
            "src/a.ts",
            vec![pattern(vec![
                hole(),
                Pattern::text("/"),
                hole(),
                Pattern::text(".js"),
            ])],
        );

        assert_eq!(
            targets_of(&load),
            [Target::Unresolved(UnresolvedCause::TooWide)]
        );
    }

    #[test]
    fn packages_and_urls_leave_the_repository() {
        let load = import_load(
            "src/a.ts",
            vec![
                Pattern::text("sqlite-vec"),
                Pattern::text("data:text/javascript,1"),
                Pattern::external(),
            ],
        );

        assert_eq!(
            targets_of(&load),
            [
                Target::Bare {
                    importer: Some(RepoPath::new("src/a.ts").unwrap()),
                    specifier: "sqlite-vec".to_owned(),
                },
                Target::Outside,
            ]
        );
    }

    #[test]
    fn worker_paths_are_relative_to_an_unknown_working_directory() {
        let load = Load {
            specifier: Value::text("dist/worker.js"),
            base: LoadBase::WorkingDirectory,
        };

        assert_eq!(targets_of(&load), [glob("**/dist/worker.js", None)]);
    }

    #[test]
    fn unknown_specifier_keeps_its_cause() {
        let load = Load {
            specifier: Value::Unknown(Unknown::runtime_value()),
            base: LoadBase::WorkingDirectory,
        };

        assert_eq!(
            targets_of(&load),
            [Target::Unresolved(UnresolvedCause::RuntimeValue)]
        );
    }

    #[test]
    fn adjacent_holes_stay_within_one_segment() {
        let load = import_load(
            "src/a.ts",
            vec![pattern(vec![
                Pattern::repo_path("src/infra/"),
                hole(),
                hole(),
                Pattern::text(".js"),
            ])],
        );

        assert_eq!(targets_of(&load), [glob("src/infra/*.js", None)]);
    }

    #[test]
    fn glob_metacharacters_in_literal_text_are_escaped() {
        let load = import_load(
            "src/a.ts",
            vec![pattern(vec![Pattern::text("./[id]/"), hole()])],
        );

        assert_eq!(targets_of(&load), [glob("src/[[]id[]]/*", None)]);
    }

    #[test]
    fn a_module_is_matched_through_every_specifier_it_can_be_imported_by() {
        let forms = |path: &str| specifier_forms(&RepoPath::new(path).unwrap());

        assert_eq!(forms("src/a.ts"), ["src/a.ts", "src/a", "src/a.js"]);
        assert_eq!(
            forms("src/b/index.mts"),
            ["src/b/index.mts", "src/b/index", "src/b", "src/b/index.mjs"]
        );
        assert_eq!(
            forms("ui/App.tsx"),
            ["ui/App.tsx", "ui/App", "ui/App.js", "ui/App.jsx"]
        );
        assert_eq!(forms("src/c.json"), ["src/c.json", "src/c"]);
        assert_eq!(forms("README.md"), ["README.md"]);
        assert_eq!(forms("src/.env"), ["src/.env"]);
    }

    #[test]
    fn globs_match_module_files_through_extensions_index_files_and_typescript_sources() {
        let candidates: Vec<RepoPath> = [
            "src/plugins/a.ts",
            "src/plugins/b/index.tsx",
            "src/plugins/c.json",
            "src/plugins/nested/d.ts",
            "src/other.ts",
        ]
        .into_iter()
        .map(|path| RepoPath::new(path).unwrap())
        .collect();
        let globs = [
            "src/plugins/*".to_owned(),
            "src/plugins/*.js".to_owned(),
            "src/plugins/*".to_owned(),
        ];
        let matches = match_globs(&globs, &candidates).unwrap();
        let matched_paths: Vec<Vec<&str>> = matches
            .iter()
            .map(|paths| paths.iter().map(RepoPath::as_str).collect())
            .collect();

        assert_eq!(
            matched_paths,
            [
                vec![
                    "src/plugins/a.ts",
                    "src/plugins/b/index.tsx",
                    "src/plugins/c.json"
                ],
                vec!["src/plugins/a.ts"],
                vec![
                    "src/plugins/a.ts",
                    "src/plugins/b/index.tsx",
                    "src/plugins/c.json"
                ],
            ]
        );
    }
}
