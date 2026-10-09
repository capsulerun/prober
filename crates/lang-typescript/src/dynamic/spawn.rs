//! What a process launch runs: the script given to a runtime, a shell or a package manager, or the program name it calls, read from the evaluated arguments of `spawn`, `exec`, `fork` and their wrappers.

use std::collections::HashMap;

use prober_core::files::RepoPath;
use serde_json::Value as JsonValue;

use crate::dynamic::value::{Part, Pattern, SpawnKind, Value, property};
use crate::presets::patterns::join_directory;
use crate::workspaces::parent_directory;

const RUNTIMES: [&str; 13] = [
    "babel-node",
    "bun",
    "deno",
    "esno",
    "jiti",
    "node",
    "nodejs",
    "sucrase-node",
    "swc-node",
    "ts-node",
    "ts-node-esm",
    "tsx",
    "vite-node",
];

const RUNTIME_SUBCOMMANDS: [&str; 2] = ["run", "watch"];

const EVAL_OPTIONS: [&str; 4] = ["-e", "--eval", "-p", "--print"];

const PRELOAD_OPTIONS: [&str; 5] = [
    "-r",
    "--require",
    "--import",
    "--loader",
    "--experimental-loader",
];

const VALUE_OPTIONS: [&str; 6] = [
    "-C",
    "--conditions",
    "--env-file",
    "--title",
    "--filter",
    "--dir",
];

const SHELLS: [&str; 5] = ["bash", "dash", "ksh", "sh", "zsh"];

const ENVIRONMENT_COMMANDS: [&str; 2] = ["env", "cross-env"];

const PACKAGE_RUNNERS: [&str; 3] = ["bunx", "npx", "pnpx"];

const PACKAGE_MANAGERS: [&str; 3] = ["npm", "pnpm", "yarn"];

const PACKAGE_EXECUTORS: [&str; 3] = ["dlx", "exec", "x"];

const PACKAGE_SCRIPT_RUNNERS: [&str; 2] = ["run", "run-script"];

const MAX_COMMAND_DEPTH: usize = 4;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Spawned {
    Script(Value),
    Program(String),
}

#[derive(Debug, Default)]
pub(crate) struct Programs {
    scripts: HashMap<(String, String), String>,
    bins: HashMap<String, Vec<(RepoPath, String)>>,
}

impl Programs {
    pub fn new<'m>(manifests: impl IntoIterator<Item = (&'m RepoPath, JsonValue)>) -> Self {
        let mut programs = Self::default();

        for (manifest, document) in manifests {
            let directory = parent_directory(manifest);

            if let Some(JsonValue::Object(scripts)) = document.get("scripts") {
                for (name, command_line) in scripts {
                    if let Some(command_line) = command_line.as_str() {
                        programs.scripts.insert(
                            (directory.to_owned(), name.clone()),
                            command_line.to_owned(),
                        );
                    }
                }
            }

            let package_name = document
                .get("name")
                .and_then(JsonValue::as_str)
                .map(|name| name.rsplit('/').next().unwrap_or(name));
            let bins: Vec<(&str, &str)> = match document.get("bin") {
                Some(JsonValue::String(target)) => package_name
                    .map(|name| (name, target.as_str()))
                    .into_iter()
                    .collect(),
                Some(JsonValue::Object(targets)) => targets
                    .iter()
                    .filter_map(|(name, target)| Some((name.as_str(), target.as_str()?)))
                    .collect(),
                _ => Vec::new(),
            };

            for (name, target) in bins {
                if let Some(target_path) = join_directory(directory, target) {
                    programs
                        .bins
                        .entry(name.to_owned())
                        .or_default()
                        .push((manifest.clone(), target_path));
                }
            }
        }

        programs
    }

    pub fn script(&self, directory: &str, name: &str) -> Option<&str> {
        self.scripts
            .get(&(directory.to_owned(), name.to_owned()))
            .map(String::as_str)
    }

    pub fn bins(&self, name: &str) -> &[(RepoPath, String)] {
        self.bins.get(name).map_or(&[], Vec::as_slice)
    }
}

pub(crate) struct Launch {
    pub spawned: Vec<Spawned>,
    pub directories: Vec<Pattern>,
}

pub(crate) fn launch(kind: SpawnKind, arguments: &[Value], working_directory: &Value) -> Launch {
    let first = arguments
        .first()
        .cloned()
        .unwrap_or_else(Value::runtime_value);
    let (listed_arguments, options) = match arguments.get(1) {
        Some(Value::Array(items)) => (items.clone(), arguments.get(2)),
        Some(Value::Object(_)) => (Vec::new(), arguments.get(1)),
        Some(listed_arguments) => (vec![listed_arguments.clone()], arguments.get(2)),
        None => (Vec::new(), None),
    };
    let spawned = match kind {
        SpawnKind::Node => vec![Spawned::Script(first)],
        SpawnKind::Shell => command_line_programs(&first, 0),
        SpawnKind::Command => match first.single_literal() {
            Some(command) if command.contains(char::is_whitespace) => {
                command_line_programs(&first, 0)
            }
            _ => {
                let mut words = vec![first];

                words.extend(listed_arguments);

                programs(&words, 0)
            }
        },
    };
    let working_directories = anchored(working_directory.patterns());
    let directories = options
        .and_then(|options| property(options, "cwd"))
        .map_or_else(
            || working_directories.clone(),
            |cwd| anchored(located(&cwd, &working_directories)),
        );
    let spawned = spawned
        .into_iter()
        .map(|spawned| match spawned {
            Spawned::Script(script) => Spawned::Script(Value::Strings(
                script
                    .patterns()
                    .iter()
                    .flat_map(|pattern| in_working_directory(pattern, &working_directories))
                    .collect(),
            )),
            program => program,
        })
        .collect();

    Launch {
        spawned,
        directories,
    }
}

pub(crate) fn command_line_programs(command_line: &Value, depth: usize) -> Vec<Spawned> {
    if depth > MAX_COMMAND_DEPTH {
        return Vec::new();
    }

    let mut spawned: Vec<Spawned> = command_line
        .patterns()
        .iter()
        .flat_map(shell_commands)
        .flat_map(|words| {
            let words: Vec<Value> = words
                .into_iter()
                .map(|word| Value::Strings(vec![word]))
                .collect();

            programs(&words, depth + 1)
        })
        .collect();

    for parameter in command_line
        .unknowns()
        .into_iter()
        .filter(|unknown| unknown.parameter_of.is_some())
    {
        let script = Spawned::Script(Value::Unknown(parameter));

        if !spawned.contains(&script) {
            spawned.push(script);
        }
    }

    spawned
}

fn programs(words: &[Value], depth: usize) -> Vec<Spawned> {
    if depth > MAX_COMMAND_DEPTH {
        return Vec::new();
    }

    let Some((command, rest)) = words.split_first() else {
        return Vec::new();
    };
    let Some(command_text) = command.single_literal() else {
        return vec![Spawned::Script(command.clone())];
    };
    let name = command_text.rsplit('/').next().unwrap_or(&command_text);

    if is_assignment(&command_text) {
        return programs(rest, depth);
    }

    if ENVIRONMENT_COMMANDS.contains(&name) {
        let start = rest
            .iter()
            .position(|word| {
                !word
                    .single_literal()
                    .is_some_and(|text| text.starts_with('-'))
            })
            .unwrap_or(rest.len());

        return programs(&rest[start..], depth + 1);
    }

    if RUNTIMES.contains(&name) {
        return runtime_scripts(name, rest);
    }

    if SHELLS.contains(&name) {
        return shell_scripts(rest, depth);
    }

    if PACKAGE_RUNNERS.contains(&name) {
        return programs(skip_options(rest), depth + 1);
    }

    if PACKAGE_MANAGERS.contains(&name) {
        return package_manager_programs(rest, depth);
    }

    if is_path(command) {
        return vec![Spawned::Script(command.clone())];
    }

    if is_program_name(name) {
        return vec![Spawned::Program(name.to_owned())];
    }

    Vec::new()
}

fn runtime_scripts(runtime: &str, words: &[Value]) -> Vec<Spawned> {
    let mut spawned = Vec::new();
    let mut position = 0;

    while position < words.len() {
        let word = &words[position];
        let Some(text) = word.single_literal() else {
            spawned.push(Spawned::Script(word.clone()));

            if is_path(word) {
                return spawned;
            }

            position += 1;

            continue;
        };

        if position == 0 && runtime != "node" && RUNTIME_SUBCOMMANDS.contains(&text.as_str()) {
            position += 1;

            continue;
        }

        if EVAL_OPTIONS.contains(&text.as_str()) {
            return spawned;
        }

        if let Some((option, value)) = text.split_once('=')
            && PRELOAD_OPTIONS.contains(&option)
        {
            spawned.extend(preloaded(&Value::text(value)));
            position += 1;

            continue;
        }

        if PRELOAD_OPTIONS.contains(&text.as_str()) {
            spawned.extend(words.get(position + 1).and_then(preloaded));
            position += 2;

            continue;
        }

        if VALUE_OPTIONS.contains(&text.as_str()) {
            position += 2;

            continue;
        }

        if text.starts_with('-') {
            position += 1;

            continue;
        }

        if runtime == "bun" && !is_path(word) {
            spawned.push(Spawned::Program(text));
        } else {
            spawned.push(Spawned::Script(word.clone()));
        }

        return spawned;
    }

    spawned
}

fn preloaded(word: &Value) -> Option<Spawned> {
    let is_relative = word
        .single_literal()
        .is_none_or(|text| text.starts_with('.') || text.starts_with('/'));

    (is_relative && is_path(word)).then(|| Spawned::Script(word.clone()))
}

fn shell_scripts(words: &[Value], depth: usize) -> Vec<Spawned> {
    let mut position = 0;

    while position < words.len() {
        let Some(text) = words[position].single_literal() else {
            return vec![Spawned::Script(words[position].clone())];
        };

        if text.starts_with("--") {
            position += 1;

            continue;
        }

        if let Some(flags) = text.strip_prefix('-') {
            if flags.contains('c') {
                return words
                    .get(position + 1)
                    .map(|command_line| command_line_programs(command_line, depth + 1))
                    .unwrap_or_default();
            }

            position += if flags.ends_with('o') { 2 } else { 1 };

            continue;
        }

        return vec![Spawned::Script(words[position].clone())];
    }

    Vec::new()
}

fn package_manager_programs(words: &[Value], depth: usize) -> Vec<Spawned> {
    let words = skip_options(words);
    let Some((subcommand, rest)) = words.split_first() else {
        return Vec::new();
    };
    let Some(subcommand_text) = subcommand.single_literal() else {
        return Vec::new();
    };

    if PACKAGE_EXECUTORS.contains(&subcommand_text.as_str()) {
        return programs(skip_options(rest), depth + 1);
    }

    if PACKAGE_SCRIPT_RUNNERS.contains(&subcommand_text.as_str()) {
        return skip_options(rest)
            .first()
            .and_then(Value::single_literal)
            .filter(|name| is_program_name(name))
            .map(|name| vec![Spawned::Program(name)])
            .unwrap_or_default();
    }

    if RUNTIMES.contains(&subcommand_text.as_str()) {
        return programs(words, depth + 1);
    }

    if is_program_name(&subcommand_text) {
        return vec![Spawned::Program(subcommand_text)];
    }

    Vec::new()
}

fn skip_options(words: &[Value]) -> &[Value] {
    let mut position = 0;

    while let Some(text) = words.get(position).and_then(Value::single_literal) {
        if VALUE_OPTIONS.contains(&text.as_str()) {
            position += 2;
        } else if text.starts_with('-') {
            position += 1;
        } else {
            break;
        }
    }

    &words[position.min(words.len())..]
}

fn is_assignment(text: &str) -> bool {
    text.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
            && !name.starts_with(|character: char| character.is_ascii_digit())
    })
}

fn is_program_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':' | '@')
        })
}

fn is_path(word: &Value) -> bool {
    let Value::Strings(patterns) = word else {
        return false;
    };

    patterns
        .iter()
        .any(|pattern| match pattern.parts().first() {
            Some(Part::RepoRoot) => true,
            Some(Part::Hole(unknown)) => unknown.is_working_directory,
            Some(Part::Text(text)) => {
                let file_name = text.rsplit('/').next().unwrap_or(text);

                text.starts_with('.')
                    || text.contains('/')
                    || file_name
                        .rsplit_once('.')
                        .is_some_and(|(stem, extension)| !stem.is_empty() && !extension.is_empty())
            }
            _ => false,
        })
}

fn located(value: &Value, working_directories: &[Pattern]) -> Vec<Pattern> {
    value
        .patterns()
        .into_iter()
        .flat_map(|pattern| match pattern.parts().first() {
            Some(Part::RepoRoot) => vec![pattern.normalized()],
            Some(Part::Hole(unknown)) if unknown.is_working_directory => {
                in_working_directory(&pattern, working_directories)
            }
            Some(Part::Text(text)) if !text.starts_with('/') => working_directories
                .iter()
                .map(|directory| joined(directory, "/", pattern.parts()))
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

fn in_working_directory(pattern: &Pattern, working_directories: &[Pattern]) -> Vec<Pattern> {
    match pattern.parts().split_first() {
        Some((Part::Hole(unknown), rest))
            if unknown.is_working_directory && starts_with_a_name(rest) =>
        {
            working_directories
                .iter()
                .map(|directory| joined(directory, "", rest))
                .collect()
        }
        _ => vec![pattern.clone()],
    }
}

fn starts_with_a_name(parts: &[Part]) -> bool {
    match parts.first() {
        Some(Part::Text(text)) => text
            .trim_start_matches('/')
            .split('/')
            .next()
            .is_some_and(|name| !name.is_empty()),
        _ => false,
    }
}

fn joined(directory: &Pattern, separator: &str, rest: &[Part]) -> Pattern {
    let mut parts = directory.parts().to_vec();

    parts.push(Part::Text(separator.to_owned()));
    parts.extend(rest.iter().cloned());

    Pattern::from_parts(parts).normalized()
}

fn anchored(patterns: Vec<Pattern>) -> Vec<Pattern> {
    patterns
        .into_iter()
        .filter(|pattern| {
            matches!(
                pattern.parts(),
                [Part::RepoRoot] | [Part::RepoRoot, Part::Text(_)]
            )
        })
        .collect()
}

fn shell_commands(command_line: &Pattern) -> Vec<Vec<Pattern>> {
    let mut commands = Vec::new();
    let mut words = Vec::new();
    let mut word: Vec<Part> = Vec::new();
    let mut quote = None;

    for part in command_line.parts() {
        let Part::Text(text) = part else {
            word.push(part.clone());

            continue;
        };

        for character in text.chars() {
            match (quote, character) {
                (Some(open), character) if character == open => quote = None,
                (Some(_), character) => push_character(&mut word, character),
                (None, '\'' | '"') => quote = Some(character),
                (None, ';' | '|' | '&' | '\n') => {
                    end_word(&mut word, &mut words);
                    end_command(&mut words, &mut commands);
                }
                (None, character) if character.is_whitespace() => end_word(&mut word, &mut words),
                (None, character) => push_character(&mut word, character),
            }
        }
    }

    end_word(&mut word, &mut words);
    end_command(&mut words, &mut commands);

    commands
}

fn push_character(word: &mut Vec<Part>, character: char) {
    match word.last_mut() {
        Some(Part::Text(text)) => text.push(character),
        _ => word.push(Part::Text(character.to_string())),
    }
}

fn end_word(word: &mut Vec<Part>, words: &mut Vec<Pattern>) {
    if !word.is_empty() {
        words.push(Pattern::from_parts(std::mem::take(word)));
    }
}

fn end_command(words: &mut Vec<Pattern>, commands: &mut Vec<Vec<Pattern>>) {
    if !words.is_empty() {
        commands.push(std::mem::take(words));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dynamic::value::Unknown;

    fn text(text: &str) -> Value {
        Value::text(text)
    }

    fn texts(texts: &[&str]) -> Value {
        Value::Array(texts.iter().map(|word| Value::text(word)).collect())
    }

    fn root() -> Value {
        Value::Strings(vec![Pattern::repo_path("")])
    }

    fn scripts(launch: &Launch) -> Vec<String> {
        launch
            .spawned
            .iter()
            .map(|spawned| match spawned {
                Spawned::Script(Value::Strings(patterns)) => match patterns[0].parts() {
                    [Part::RepoRoot, Part::Text(path)] => format!("<root>{path}"),
                    [Part::Hole(_)] => "<unknown>".to_owned(),
                    _ => patterns[0].literal_text().unwrap_or_default(),
                },
                Spawned::Script(_) => "<unknown>".to_owned(),
                Spawned::Program(name) => format!("program {name}"),
            })
            .collect()
    }

    fn command(arguments: &[Value]) -> Vec<String> {
        scripts(&launch(SpawnKind::Command, arguments, &root()))
    }

    fn shell(command_line: &str) -> Vec<String> {
        scripts(&launch(SpawnKind::Shell, &[text(command_line)], &root()))
    }

    #[test]
    fn runtimes_run_their_first_positional_argument_and_preloaded_modules() {
        let launcher = Value::Strings(vec![Pattern::repo_path("bin/cli.mjs")]);

        assert_eq!(
            command(&[
                text("node"),
                Value::Array(vec![
                    Value::runtime_value(),
                    text("--import"),
                    text("./register.mjs"),
                    launcher,
                    text("agents"),
                ])
            ]),
            ["<unknown>", "./register.mjs", "<root>/bin/cli.mjs"]
        );
        assert_eq!(
            command(&[
                text("node"),
                texts(&["--import", "tsx", "scripts/a.ts", "b.ts"])
            ]),
            ["scripts/a.ts"]
        );
        assert_eq!(
            command(&[text("node"), texts(&["-e", "process.exit(0)"])]),
            Vec::<String>::new()
        );
        assert_eq!(
            command(&[text("tsx"), texts(&["watch", "src/main.ts"])]),
            ["src/main.ts"]
        );
        assert_eq!(
            command(&[text("bun"), texts(&["run", "dev"])]),
            ["program dev"]
        );
    }

    #[test]
    fn commands_are_scripts_programs_or_external_tools() {
        assert_eq!(
            command(&[text("./scripts/setup.sh")]),
            ["./scripts/setup.sh"]
        );
        assert_eq!(command(&[text("cli"), texts(&["--help"])]), ["program cli"]);
        assert_eq!(
            command(&[text("bash"), texts(&["-euo", "pipefail", "scripts/ci.sh"])]),
            ["scripts/ci.sh"]
        );
        assert_eq!(
            command(&[
                text("sh"),
                texts(&["-c", "node scripts/a.mjs && git status"])
            ]),
            ["scripts/a.mjs", "program git"]
        );
        assert_eq!(
            command(&[Value::Unknown(Unknown::runtime_value()), texts(&["x"])]),
            ["<unknown>"]
        );
        assert_eq!(
            command(&[text("node"), Value::Unknown(Unknown::runtime_value())]),
            ["<unknown>"]
        );
    }

    #[test]
    fn package_managers_run_scripts_bins_and_runtimes() {
        assert_eq!(shell("pnpm run build --watch"), ["program build"]);
        assert_eq!(shell("pnpm --filter ui test"), ["program test"]);
        assert_eq!(shell("npx --yes tsx scripts/a.ts"), ["scripts/a.ts"]);
        assert_eq!(shell("pnpm exec cli status"), ["program cli"]);
        assert_eq!(shell("yarn tsx scripts/b.ts"), ["scripts/b.ts"]);
        assert_eq!(
            shell("NODE_ENV=test cross-env A=1 node 'scripts/with space.mjs'"),
            ["scripts/with space.mjs"]
        );
    }

    #[test]
    fn shell_lines_keep_evaluated_paths_inside_their_words() {
        let command_line = Value::Strings(vec![Pattern::from_parts(vec![
            Part::Text("node ".to_owned()),
            Part::RepoRoot,
            Part::Text("/scripts/run.mjs --flag".to_owned()),
        ])]);

        assert_eq!(
            scripts(&launch(SpawnKind::Shell, &[command_line], &root())),
            ["<root>/scripts/run.mjs"]
        );
    }

    #[test]
    fn the_cwd_option_replaces_the_working_directory() {
        let options = Value::Object(vec![("cwd".to_owned(), text("packages/cli"))]);
        let launch = launch(
            SpawnKind::Node,
            &[text("bin/run.js"), texts(&["a"]), options],
            &root(),
        );

        assert_eq!(scripts(&launch), ["bin/run.js"]);
        assert_eq!(launch.directories, [Pattern::repo_path("packages/cli")]);

        let temporary = Value::Object(vec![("cwd".to_owned(), Value::runtime_value())]);
        let launch = super::launch(
            SpawnKind::Command,
            &[text("node"), texts(&["a.js"]), temporary],
            &root(),
        );

        assert!(launch.directories.is_empty());
    }
}
