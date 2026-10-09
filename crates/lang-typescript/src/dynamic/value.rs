//! Values of the partial evaluation: string patterns with holes, and the path and URL operations on them.

use prober_core::files::RepoPath;
use prober_core::graph::UnresolvedCause;

use crate::dynamic::summary::FunctionIndex;

pub(crate) const MAX_ALTERNATIVES: usize = 64;

pub(crate) const OPEN_OBJECT: &str = "\u{0}open";

const EXTERNAL_SCHEMES: [&str; 5] = ["data:", "node:", "http:", "https:", "blob:"];

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FunctionRef {
    pub module: RepoPath,
    pub index: FunctionIndex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unknown {
    pub cause: UnresolvedCause,
    pub parameter_of: Option<FunctionRef>,
    pub is_working_directory: bool,
}

impl Unknown {
    pub fn runtime_value() -> Self {
        Self {
            cause: UnresolvedCause::RuntimeValue,
            parameter_of: None,
            is_working_directory: false,
        }
    }

    pub fn parameter_of(function: FunctionRef) -> Self {
        Self {
            cause: UnresolvedCause::Parameter,
            parameter_of: Some(function),
            is_working_directory: false,
        }
    }

    pub fn working_directory() -> Self {
        Self {
            is_working_directory: true,
            ..Self::runtime_value()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Part {
    RepoRoot,
    External,
    Text(String),
    Hole(Unknown),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pattern(Vec<Part>);

impl Pattern {
    pub fn text(text: &str) -> Self {
        Self(vec![Part::Text(text.to_owned())])
    }

    pub fn hole(unknown: Unknown) -> Self {
        Self(vec![Part::Hole(unknown)])
    }

    pub fn repo_path(path: &str) -> Self {
        let mut pattern = Self(vec![Part::RepoRoot]);

        if !path.is_empty() {
            pattern.push(Part::Text(format!("/{path}")));
        }

        pattern
    }

    pub fn external() -> Self {
        Self(vec![Part::External])
    }

    pub fn from_parts(parts: Vec<Part>) -> Self {
        let mut pattern = Self(Vec::new());

        for part in parts {
            pattern.push(part);
        }

        pattern
    }

    pub fn parts(&self) -> &[Part] {
        &self.0
    }

    pub fn unknowns(&self) -> impl Iterator<Item = &Unknown> {
        self.0.iter().filter_map(|part| match part {
            Part::Hole(unknown) => Some(unknown),
            _ => None,
        })
    }

    pub fn literal_text(&self) -> Option<String> {
        self.0
            .iter()
            .map(|part| match part {
                Part::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Option<Vec<&str>>>()
            .map(|texts| texts.concat())
    }

    fn push(&mut self, part: Part) {
        match (self.0.last_mut(), part) {
            (Some(Part::Text(previous)), Part::Text(text)) => previous.push_str(&text),
            (_, Part::Text(text)) if text.is_empty() => {}
            (_, part) => self.0.push(part),
        }
    }

    fn concat(&self, other: &Pattern) -> Pattern {
        let mut joined = self.clone();

        for part in &other.0 {
            joined.push(part.clone());
        }

        joined
    }

    fn starts_with_text(&self, prefix: &str) -> bool {
        matches!(self.0.first(), Some(Part::Text(text)) if text.starts_with(prefix))
    }

    pub fn is_relative(&self) -> bool {
        self.starts_with_text("./")
            || self.starts_with_text("../")
            || self
                .literal_text()
                .is_some_and(|text| text == "." || text == "..")
    }

    pub fn is_absolute(&self) -> bool {
        matches!(
            self.0.first(),
            Some(Part::RepoRoot | Part::External | Part::Hole(_))
        ) || self.starts_with_text("/")
    }

    pub fn is_external(&self) -> bool {
        matches!(self.0.first(), Some(Part::External))
            || EXTERNAL_SCHEMES
                .iter()
                .any(|scheme| self.starts_with_text(scheme))
    }

    pub fn without_file_scheme(&self) -> Pattern {
        match self.0.first() {
            Some(Part::Text(text)) if text.starts_with("file://") => {
                let mut stripped = Pattern(Vec::new());

                stripped.push(Part::Text(text["file://".len()..].to_owned()));

                for part in &self.0[1..] {
                    stripped.push(part.clone());
                }

                stripped
            }
            _ => self.clone(),
        }
    }

    pub fn directory(&self) -> Pattern {
        let mut parts = self.0.clone();

        while let Some(part) = parts.pop() {
            match part {
                Part::Text(text) => {
                    if let Some(separator) = text.rfind('/') {
                        let mut directory = Pattern(parts);
                        let kept = if separator == 0 && directory.0.is_empty() {
                            "/"
                        } else {
                            &text[..separator]
                        };

                        directory.push(Part::Text(kept.to_owned()));

                        return directory;
                    }
                }
                Part::Hole(_) => {}
                Part::RepoRoot | Part::External => return Pattern(vec![part]),
            }
        }

        if self.0.iter().any(|part| matches!(part, Part::Hole(_))) {
            return self.clone();
        }

        Pattern::text(".")
    }

    pub fn ends_with_separator(&self) -> bool {
        matches!(self.0.last(), Some(Part::Text(text)) if text.ends_with('/'))
    }

    pub fn normalized(&self) -> Pattern {
        let (anchor, rest) = match self.0.first() {
            Some(Part::RepoRoot) => (Some(Part::RepoRoot), &self.0[1..]),
            Some(Part::External) => return Pattern::external(),
            _ => (None, &self.0[..]),
        };
        let mut segments: Vec<Vec<Part>> = vec![Vec::new()];

        for part in rest {
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
                                .push(Part::Text(piece.to_owned()));
                        }
                    }
                }
                part => segments
                    .last_mut()
                    .expect("segments start non-empty")
                    .push(part.clone()),
            }
        }

        let is_absolute = anchor.is_some() || segments.len() > 1 && segments[0].is_empty();
        let mut kept: Vec<Vec<Part>> = Vec::new();

        for (position, segment) in segments.into_iter().enumerate() {
            match segment_text(&segment) {
                Some("") if position > 0 || is_absolute => {}
                Some(".") if position > 0 || is_absolute => {}
                Some("..") => {
                    let can_pop = kept.last().is_some_and(|previous| {
                        segment_text(previous)
                            .is_some_and(|text| text != ".." && text != "." && !text.is_empty())
                    });

                    if can_pop {
                        kept.pop();
                    } else if anchor.is_some() {
                        return Pattern::external();
                    } else if !is_absolute {
                        kept.push(segment);
                    }
                }
                _ => kept.push(segment),
            }
        }

        let mut normalized = Pattern(anchor.into_iter().collect());

        for (position, segment) in kept.into_iter().enumerate() {
            if position > 0 || is_absolute {
                normalized.push(Part::Text("/".to_owned()));
            }

            for part in segment {
                normalized.push(part);
            }
        }

        if normalized.0.is_empty() {
            return Pattern::text(if is_absolute { "/" } else { "." });
        }

        normalized
    }
}

fn segment_text(segment: &[Part]) -> Option<&str> {
    match segment {
        [] => Some(""),
        [Part::Text(text)] => Some(text),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathFunction {
    Join,
    Resolve,
    Dirname,
    Normalize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AccessKind {
    Read,
    List,
    Copy,
    Glob,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpawnKind {
    Command,
    Shell,
    Node,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Builtin {
    PathModule,
    Path(PathFunction),
    UrlModule,
    PathToFileUrl,
    FileUrlToPath,
    UrlConstructor,
    ModuleModule,
    CreateRequire,
    RequireResolve(Vec<Pattern>),
    StringFunction,
    ObjectConstructor,
    ObjectKeys,
    ObjectValues,
    DefineConfig,
    MergeConfig,
    FsModule,
    GlobModule,
    FileAccess(AccessKind),
    Process,
    ProcessCwd,
    ChildProcessModule,
    Spawn(SpawnKind),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Strings(Vec<Pattern>),
    Boolean(bool),
    Function(FunctionRef),
    Module(RepoPath),
    Builtin(Builtin),
    Require(Vec<Pattern>),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
    Method { receiver: Box<Value>, name: String },
    Alternatives(Vec<Value>),
    Unknown(Unknown),
}

impl Value {
    pub fn text(text: &str) -> Self {
        Value::Strings(vec![Pattern::text(text)])
    }

    pub fn runtime_value() -> Self {
        Value::Unknown(Unknown::runtime_value())
    }

    pub fn patterns(&self) -> Vec<Pattern> {
        match self {
            Value::Strings(patterns) => patterns.clone(),
            Value::Unknown(unknown) => vec![Pattern::hole(unknown.clone())],
            Value::Alternatives(values) => {
                let mut patterns: Vec<Pattern> = Vec::new();

                for pattern in values.iter().flat_map(Value::patterns) {
                    if !patterns.contains(&pattern) {
                        patterns.push(pattern);
                    }
                }

                patterns
            }
            _ => vec![Pattern::hole(Unknown::runtime_value())],
        }
    }

    pub fn unknowns(&self) -> Vec<Unknown> {
        match self {
            Value::Strings(patterns) => patterns
                .iter()
                .flat_map(|pattern| pattern.unknowns().cloned())
                .collect(),
            Value::Unknown(unknown) => vec![unknown.clone()],
            Value::Alternatives(values) => values.iter().flat_map(Value::unknowns).collect(),
            _ => Vec::new(),
        }
    }

    pub fn single_literal(&self) -> Option<String> {
        match self {
            Value::Strings(patterns) => match patterns.as_slice() {
                [pattern] => pattern.literal_text(),
                _ => None,
            },
            _ => None,
        }
    }
}

pub(crate) fn merge_objects(values: Vec<Value>) -> Value {
    let mut properties = Vec::new();

    for value in values {
        match value {
            Value::Object(spread_properties) => {
                for (key, value) in spread_properties {
                    set_property(&mut properties, key, value);
                }
            }
            Value::Alternatives(choices)
                if choices
                    .iter()
                    .all(|choice| matches!(choice, Value::Object(_))) =>
            {
                let mut keys: Vec<String> = Vec::new();

                for choice in &choices {
                    if let Value::Object(choice_properties) = choice {
                        for (key, _) in choice_properties {
                            if !keys.contains(key) {
                                keys.push(key.clone());
                            }
                        }
                    }
                }

                for key in keys {
                    let values = choices
                        .iter()
                        .map(|choice| property(choice, &key))
                        .collect::<Option<Vec<Value>>>();
                    let value = values.map_or_else(Value::runtime_value, alternatives);

                    set_property(&mut properties, key, value);
                }
            }
            _ => {
                for (_, value) in &mut properties {
                    *value = Value::runtime_value();
                }

                set_property(
                    &mut properties,
                    OPEN_OBJECT.to_owned(),
                    Value::runtime_value(),
                );
            }
        }
    }

    Value::Object(properties)
}

pub(crate) fn merge_config(defaults: Value, overrides: Value) -> Value {
    match (defaults, overrides) {
        (Value::Object(mut properties), Value::Object(override_properties)) => {
            for (key, value) in override_properties {
                let merged = match property(&Value::Object(properties.clone()), &key) {
                    Some(existing) if key != OPEN_OBJECT => merge_config(existing, value),
                    _ => value,
                };

                set_property(&mut properties, key, merged);
            }

            Value::Object(properties)
        }
        (Value::Array(mut items), Value::Array(more_items)) => {
            items.extend(more_items);

            Value::Array(items)
        }
        (Value::Array(mut items), item @ Value::Strings(_)) => {
            items.push(item);

            Value::Array(items)
        }
        (item @ Value::Strings(_), Value::Array(more_items)) => {
            let mut items = vec![item];

            items.extend(more_items);

            Value::Array(items)
        }
        (_, overrides) => overrides,
    }
}

pub(crate) fn property(object: &Value, key: &str) -> Option<Value> {
    match object {
        Value::Object(properties) => properties
            .iter()
            .find(|(property_key, _)| property_key == key)
            .map(|(_, value)| value.clone()),
        _ => None,
    }
}

pub(crate) fn is_open_object(object: &Value) -> bool {
    property(object, OPEN_OBJECT).is_some()
}

fn set_property(properties: &mut Vec<(String, Value)>, key: String, value: Value) {
    match properties
        .iter_mut()
        .find(|(property_key, _)| *property_key == key)
    {
        Some((_, existing)) => *existing = value,
        None => properties.push((key, value)),
    }
}

pub(crate) fn alternatives(values: Vec<Value>) -> Value {
    let mut patterns: Vec<Pattern> = Vec::new();
    let mut others: Vec<Value> = Vec::new();

    for value in values.into_iter().flat_map(|value| match value {
        Value::Alternatives(nested) => nested,
        value => vec![value],
    }) {
        match value {
            Value::Strings(_) | Value::Unknown(_) => {
                for pattern in value.patterns() {
                    if !patterns.contains(&pattern) {
                        patterns.push(pattern);
                    }
                }
            }
            value if !others.contains(&value) => others.push(value),
            _ => {}
        }
    }

    if others.is_empty() {
        return strings(patterns);
    }

    if patterns.is_empty() && others.len() == 1 {
        return others.pop().expect("one alternative");
    }

    if !patterns.is_empty() {
        others.push(strings(patterns));
    }

    if others.len() > MAX_ALTERNATIVES {
        return Value::runtime_value();
    }

    Value::Alternatives(others)
}

pub(crate) fn concatenation(values: &[Value]) -> Value {
    let mut combined = vec![Pattern(Vec::new())];

    for value in values {
        let pieces = value.patterns();

        if combined.len() * pieces.len() > MAX_ALTERNATIVES {
            return Value::runtime_value();
        }

        combined = combined
            .iter()
            .flat_map(|prefix| pieces.iter().map(move |piece| prefix.concat(piece)))
            .collect();
    }

    strings(combined)
}

pub(crate) fn path_join(values: &[Value]) -> Value {
    let mut separated = Vec::new();

    for (position, value) in values.iter().enumerate() {
        if position > 0 {
            separated.push(Value::text("/"));
        }

        separated.push(value.clone());
    }

    map_patterns(&concatenation(&separated), Pattern::normalized)
}

pub(crate) fn path_resolve(values: &[Value], working_directory: &Value) -> Value {
    let last_absolute = values.iter().rposition(|value| {
        let patterns = value.patterns();

        !patterns.is_empty() && patterns.iter().all(Pattern::is_absolute)
    });

    match last_absolute {
        Some(position) => path_join(&values[position..]),
        None => {
            let mut from_working_directory = vec![working_directory.clone()];

            from_working_directory.extend_from_slice(values);

            path_join(&from_working_directory)
        }
    }
}

pub(crate) fn path_dirname(value: &Value) -> Value {
    map_patterns(value, |pattern| pattern.normalized().directory())
}

pub(crate) fn file_url_to_path(value: &Value) -> Value {
    map_patterns(value, Pattern::without_file_scheme)
}

pub(crate) fn url(input: &Value, base: Option<&Value>) -> Value {
    let inputs = input.patterns();
    let bases = base.map(Value::patterns);
    let mut resolved = Vec::new();

    for input in inputs {
        let input = input.without_file_scheme();

        if input.is_absolute() || input.is_external() {
            resolved.push(input.normalized());

            continue;
        }

        let Some(bases) = &bases else {
            resolved.push(Pattern::hole(Unknown::runtime_value()));

            continue;
        };

        for base in bases {
            let base = base.without_file_scheme();
            let directory = if base.ends_with_separator() {
                base.normalized()
            } else {
                base.normalized().directory()
            };

            resolved.push(
                directory
                    .concat(&Pattern::text("/"))
                    .concat(&input)
                    .normalized(),
            );
        }
    }

    strings(resolved)
}

pub(crate) fn require_resolve(specifier: &Value, bases: &[Pattern]) -> Value {
    let mut resolved = Vec::new();

    for specifier in specifier.patterns() {
        if specifier.is_relative() {
            for base in bases {
                resolved.push(
                    base.normalized()
                        .directory()
                        .concat(&Pattern::text("/"))
                        .concat(&specifier)
                        .normalized(),
                );
            }
        } else if specifier.is_absolute() || specifier.is_external() {
            resolved.push(specifier.without_file_scheme().normalized());
        } else if specifier.literal_text().is_some() {
            resolved.push(Pattern::external());
        } else {
            resolved.push(Pattern::hole(Unknown::runtime_value()));
        }
    }

    strings(resolved)
}

fn map_patterns(value: &Value, transform: impl Fn(&Pattern) -> Pattern) -> Value {
    match value {
        Value::Strings(patterns) => strings(patterns.iter().map(transform).collect()),
        Value::Unknown(unknown) => Value::Unknown(unknown.clone()),
        _ => Value::runtime_value(),
    }
}

fn strings(patterns: Vec<Pattern>) -> Value {
    let mut unique: Vec<Pattern> = Vec::new();

    for pattern in patterns {
        if !unique.contains(&pattern) {
            unique.push(pattern);
        }
    }

    if unique.len() > MAX_ALTERNATIVES {
        return Value::runtime_value();
    }

    Value::Strings(unique)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(path: &str) -> Value {
        Value::Strings(vec![Pattern::repo_path(path)])
    }

    fn parameter() -> Value {
        Value::Unknown(Unknown::parameter_of(FunctionRef {
            module: RepoPath::new("src/load.ts").unwrap(),
            index: 0,
        }))
    }

    fn display(value: &Value) -> Vec<String> {
        value
            .patterns()
            .iter()
            .map(|pattern| {
                pattern
                    .parts()
                    .iter()
                    .map(|part| match part {
                        Part::RepoRoot => "<repo>".to_owned(),
                        Part::External => "<external>".to_owned(),
                        Part::Text(text) => text.clone(),
                        Part::Hole(_) => "*".to_owned(),
                    })
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn join_normalizes_dots_and_separators() {
        let joined = path_join(&[
            path_dirname(&module("src/cli/run.ts")),
            Value::text("../plugins//./loader.js"),
        ]);

        assert_eq!(display(&joined), ["<repo>/src/plugins/loader.js"]);
    }

    #[test]
    fn join_above_the_repository_root_leaves_the_repository() {
        let joined = path_join(&[module("src"), Value::text("../../outside.js")]);

        assert_eq!(display(&joined), ["<external>"]);
    }

    #[test]
    fn join_with_an_unknown_root_keeps_the_literal_tail() {
        let joined = path_join(&[
            Value::runtime_value(),
            Value::text("src"),
            Value::text("cli/profile.ts"),
        ]);

        assert_eq!(display(&joined), ["*/src/cli/profile.ts"]);
    }

    #[test]
    fn resolve_restarts_from_the_last_absolute_path_or_the_working_directory() {
        let unknown_directory = Value::runtime_value();
        let from_module = path_resolve(
            &[Value::text("ignored"), module("src"), Value::text("a.ts")],
            &unknown_directory,
        );
        let from_working_directory =
            path_resolve(&[Value::text("dist/index.js")], &unknown_directory);
        let from_repository_root = path_resolve(&[Value::text("dist/index.js")], &module(""));

        assert_eq!(display(&from_module), ["<repo>/src/a.ts"]);
        assert_eq!(display(&from_working_directory), ["*/dist/index.js"]);
        assert_eq!(display(&from_repository_root), ["<repo>/dist/index.js"]);
    }

    #[test]
    fn dirname_drops_the_last_segment() {
        assert_eq!(
            display(&path_dirname(&module("src/a/b.ts"))),
            ["<repo>/src/a"]
        );
        assert_eq!(display(&path_dirname(&module("b.ts"))), ["<repo>"]);
        assert_eq!(display(&path_dirname(&Value::text("./x/y.js"))), ["./x"]);
        assert_eq!(display(&path_dirname(&parameter())), ["*"]);
    }

    #[test]
    fn url_resolves_relative_input_against_the_base_directory() {
        let module_url = module("src/entry.ts");

        assert_eq!(
            display(&url(&Value::text("../node-host.mjs"), Some(&module_url))),
            ["<repo>/node-host.mjs"]
        );
        assert_eq!(
            display(&url(
                &Value::text("./workers/a.worker.ts"),
                Some(&module_url)
            )),
            ["<repo>/src/workers/a.worker.ts"]
        );
        assert_eq!(
            display(&url(&module("src/a.ts"), Some(&Value::runtime_value()))),
            ["<repo>/src/a.ts"]
        );
    }

    #[test]
    fn file_urls_are_paths() {
        let file_url = Value::text("file:///app/dist/index.js");

        assert_eq!(
            display(&file_url_to_path(&file_url)),
            ["/app/dist/index.js"]
        );
    }

    #[test]
    fn concatenation_multiplies_alternatives_and_keeps_holes() {
        let directory = Value::Strings(vec![Pattern::text("./a/"), Pattern::text("./b/")]);
        let concatenated = concatenation(&[directory, parameter(), Value::text(".js")]);

        assert_eq!(display(&concatenated), ["./a/*.js", "./b/*.js"]);
        assert_eq!(concatenated.unknowns().len(), 2);
    }

    #[test]
    fn too_many_alternatives_become_a_runtime_value() {
        let many: Vec<Pattern> = (0..MAX_ALTERNATIVES)
            .map(|index| Pattern::text(&index.to_string()))
            .collect();
        let doubled = concatenation(&[
            Value::Strings(many),
            Value::Strings(vec![Pattern::text("a"), Pattern::text("b")]),
        ]);

        assert_eq!(doubled, Value::runtime_value());
    }

    #[test]
    fn alternatives_keep_unknown_values_next_to_known_ones() {
        let object = Value::Object(vec![("dirName".to_owned(), Value::text("telegram"))]);
        let with_unknown = alternatives(vec![object.clone(), Value::runtime_value()]);
        let only_known = alternatives(vec![object.clone(), object.clone()]);

        assert_eq!(
            with_unknown,
            Value::Alternatives(vec![
                object.clone(),
                Value::Strings(vec![Pattern::hole(Unknown::runtime_value())])
            ])
        );
        assert_eq!(only_known, object);
        assert_eq!(display(&with_unknown), ["*"]);
    }

    #[test]
    fn require_resolve_keeps_relative_paths_and_marks_packages_external() {
        let base = [Pattern::repo_path("scripts/build.mjs")];

        assert_eq!(
            display(&require_resolve(&Value::text("./lib/plan.mts"), &base)),
            ["<repo>/scripts/lib/plan.mts"]
        );
        assert_eq!(
            display(&require_resolve(&Value::text("ws/package.json"), &base)),
            ["<external>"]
        );
    }
}
