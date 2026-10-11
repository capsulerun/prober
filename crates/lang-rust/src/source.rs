//! What one Rust file tells about its dependencies: module declarations, use trees, qualified paths and macro calls (also inside macro bodies), macro definitions, impl targets and the files it includes.

use std::collections::BTreeSet;

use ra_ap_syntax::ast::{self, HasAttrs, HasModuleItem, HasName, HasVisibility};
use ra_ap_syntax::{AstNode, AstToken, Edition, SourceFile, SyntaxKind, SyntaxToken, T};

const MANIFEST_DIRECTORY_VARIABLE: &str = "CARGO_MANIFEST_DIR";

const OUT_DIRECTORY_VARIABLE: &str = "OUT_DIR";

const RERUN_PREFIX: &str = "cargo:rerun-if-changed=";

const MAX_PATH_LENGTH: usize = 255;

const INCLUDE_MACROS: [(&str, IncludeKind); 3] = [
    ("include_str", IncludeKind::Data),
    ("include_bytes", IncludeKind::Data),
    ("include", IncludeKind::Code),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModDecl {
    pub scope: Vec<String>,
    pub name: String,
    pub path_attribute: Option<String>,
    pub inline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UseEntry {
    pub scope: Vec<String>,
    pub path: Vec<String>,
    pub binding: Option<String>,
    pub glob: bool,
    pub public: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImplTarget {
    pub scope: Vec<String>,
    pub self_type: Option<Vec<String>>,
    pub trait_path: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IncludeKind {
    Data,
    Code,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IncludeBase {
    File,
    ManifestDirectory,
    OutDirectory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Include {
    pub line: u32,
    pub kind: IncludeKind,
    pub base: IncludeBase,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScannedPath {
    pub line: u32,
    pub segments: Vec<String>,
    pub is_macro: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SourceFacts {
    pub mods: Vec<ModDecl>,
    pub uses: Vec<UseEntry>,
    pub paths: Vec<ScannedPath>,
    pub macro_rules: Vec<(String, bool)>,
    pub macro_use_crates: Vec<String>,
    pub impls: Vec<ImplTarget>,
    pub items: Vec<(Vec<String>, String)>,
    pub includes: Vec<Include>,
    pub strings: Vec<(u32, String)>,
    pub identifiers: BTreeSet<String>,
}

pub(crate) fn source_facts(text: &str) -> SourceFacts {
    let parse = SourceFile::parse(text, Edition::Edition2024);
    let file = parse.tree();
    let line_starts = line_starts(text);
    let mut facts = SourceFacts::default();

    collect_items(file.items(), &mut Vec::new(), &mut facts);
    scan_tokens(file.syntax(), &line_starts, &mut facts);

    facts
}

fn collect_items(
    items: ast::AstChildren<ast::Item>,
    scope: &mut Vec<String>,
    facts: &mut SourceFacts,
) {
    for item in items {
        match item {
            ast::Item::Module(module) => {
                let Some(name) = module.name().map(|name| name.text().to_string()) else {
                    continue;
                };
                let path_attribute = module
                    .attrs()
                    .find_map(|attribute| path_attribute(&attribute));
                let inline = module.item_list().is_some();

                facts.mods.push(ModDecl {
                    scope: scope.clone(),
                    name: name.clone(),
                    path_attribute,
                    inline,
                });

                if let Some(item_list) = module.item_list() {
                    scope.push(name);
                    collect_items(item_list.items(), scope, facts);
                    scope.pop();
                }
            }
            ast::Item::Use(use_item) => {
                let public = use_item.visibility().is_some();

                if let Some(tree) = use_item.use_tree() {
                    collect_use_tree(&tree, &[], scope, public, facts);
                }
            }
            ast::Item::MacroRules(macro_rules) => {
                let exported = macro_rules
                    .attrs()
                    .any(|attribute| attribute_name(&attribute).as_deref() == Some("macro_export"));

                if let Some(name) = macro_rules.name() {
                    facts.macro_rules.push((name.text().to_string(), exported));
                }
            }
            ast::Item::ExternCrate(extern_crate) => {
                let uses_macros = extern_crate
                    .attrs()
                    .any(|attribute| attribute_name(&attribute).as_deref() == Some("macro_use"));

                if let Some(name) = extern_crate.name_ref() {
                    let name = name.text().to_string();
                    let binding = extern_crate
                        .rename()
                        .and_then(|rename| rename.name())
                        .map_or_else(|| name.clone(), |name| name.text().to_string());

                    if uses_macros {
                        facts.macro_use_crates.push(name.clone());
                    }

                    facts.uses.push(UseEntry {
                        scope: scope.clone(),
                        path: vec![name],
                        binding: Some(binding),
                        glob: false,
                        public: extern_crate.visibility().is_some(),
                    });
                }
            }
            ast::Item::Impl(implementation) => {
                facts.impls.push(ImplTarget {
                    scope: scope.clone(),
                    self_type: implementation.self_ty().and_then(|ty| type_path(&ty)),
                    trait_path: implementation.trait_().and_then(|ty| type_path(&ty)),
                });
            }
            ast::Item::Struct(item) => add_item(item.name(), scope, facts),
            ast::Item::Enum(item) => add_item(item.name(), scope, facts),
            ast::Item::Union(item) => add_item(item.name(), scope, facts),
            ast::Item::TypeAlias(item) => add_item(item.name(), scope, facts),
            ast::Item::Trait(item) => add_item(item.name(), scope, facts),
            ast::Item::Fn(item) => add_item(item.name(), scope, facts),
            ast::Item::Const(item) => add_item(item.name(), scope, facts),
            ast::Item::Static(item) => add_item(item.name(), scope, facts),
            _ => {}
        }
    }
}

fn add_item(name: Option<ast::Name>, scope: &[String], facts: &mut SourceFacts) {
    if let Some(name) = name {
        facts.items.push((scope.to_vec(), name.text().to_string()));
    }
}

fn collect_use_tree(
    tree: &ast::UseTree,
    prefix: &[String],
    scope: &[String],
    public: bool,
    facts: &mut SourceFacts,
) {
    let mut path = prefix.to_vec();

    if let Some(tree_path) = tree.path() {
        path.extend(path_segments(&tree_path));
    }

    if let Some(list) = tree.use_tree_list() {
        for child in list.use_trees() {
            collect_use_tree(&child, &path, scope, public, facts);
        }

        return;
    }

    if path.is_empty() {
        return;
    }

    let glob = tree.star_token().is_some();
    let is_self = path.last().is_some_and(|segment| segment == "self");

    if is_self {
        path.pop();
    }

    let binding = if glob {
        None
    } else {
        match tree.rename() {
            Some(rename) => rename.name().map(|name| name.text().to_string()),
            None => path.last().cloned(),
        }
    };

    facts.uses.push(UseEntry {
        scope: scope.to_vec(),
        path,
        binding,
        glob,
        public,
    });
}

fn path_segments(path: &ast::Path) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = Some(path.clone());

    while let Some(path) = current {
        if let Some(segment) = path.segment() {
            segments.push(segment.syntax().text().to_string().trim().to_owned());
        }

        current = path.qualifier();
    }

    segments.reverse();
    segments
        .into_iter()
        .map(|segment| {
            segment
                .split(['<', '('])
                .next()
                .unwrap_or_default()
                .trim_start_matches("::")
                .trim()
                .to_owned()
        })
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn type_path(ty: &ast::Type) -> Option<Vec<String>> {
    let path = ty.syntax().descendants().find_map(ast::Path::cast)?;
    let mut outermost = path;

    while let Some(parent) = outermost.syntax().parent().and_then(ast::Path::cast) {
        outermost = parent;
    }

    let segments = path_segments(&outermost);

    (!segments.is_empty()).then_some(segments)
}

fn attribute_name(attribute: &ast::Attr) -> Option<String> {
    attribute.simple_name().map(|name| name.to_string())
}

fn path_attribute(attribute: &ast::Attr) -> Option<String> {
    let text = attribute.syntax().text().to_string();
    let inner = text.trim_start_matches("#[").trim_end_matches(']').trim();
    let value = inner
        .strip_prefix("path")?
        .trim_start()
        .strip_prefix('=')?
        .trim();

    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .map(str::to_owned)
}

fn scan_tokens(root: &ra_ap_syntax::SyntaxNode, line_starts: &[usize], facts: &mut SourceFacts) {
    let tokens: Vec<SyntaxToken> = root
        .descendants_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| !token.kind().is_trivia())
        .collect();
    let use_ranges: Vec<_> = root
        .descendants()
        .filter(|node| ast::Use::can_cast(node.kind()))
        .map(|node| node.text_range())
        .collect();

    facts.identifiers = tokens
        .iter()
        .filter(|token| token.kind() == SyntaxKind::IDENT)
        .filter(|token| {
            !use_ranges
                .iter()
                .any(|range| range.contains_range(token.text_range()))
        })
        .map(|token| token.text().to_owned())
        .collect();

    let line_of = |token: &SyntaxToken| {
        let offset = usize::from(token.text_range().start());

        line_starts.partition_point(|&start| start <= offset) as u32
    };
    let mut index = 0;

    while index < tokens.len() {
        let token = &tokens[index];

        if !is_path_start(token) || index > 0 && is_path_continuation(&tokens, index - 1) {
            index += 1;

            continue;
        }

        let mut segments = vec![token.text().to_owned()];
        let mut end = index + 1;

        while let Some(width) = separator_width(&tokens, end)
            && tokens.get(end + width).is_some_and(is_segment)
        {
            segments.push(tokens[end + width].text().to_owned());
            end += width + 1;
        }

        let is_macro = tokens.get(end).is_some_and(|next| next.kind() == T![!])
            && tokens
                .get(end + 1)
                .is_some_and(|next| matches!(next.kind(), T!['('] | T!['['] | T!['{']));

        if is_macro
            && segments.len() == 1
            && let Some((_, kind)) = INCLUDE_MACROS.iter().find(|(name, _)| *name == segments[0])
            && let Some(include) = include(&tokens[end + 1..], *kind, line_of(token))
        {
            facts.includes.push(include);
        }

        if segments.len() >= 2 || is_macro {
            facts.paths.push(ScannedPath {
                line: line_of(token),
                segments,
                is_macro,
            });
        }

        index = end.max(index + 1);
    }

    for token in tokens
        .iter()
        .filter(|token| token.kind() == SyntaxKind::STRING)
    {
        if let Some(value) = string_value(token) {
            let value = value.strip_prefix(RERUN_PREFIX).unwrap_or(&value);

            if value.len() <= MAX_PATH_LENGTH
                && value.contains(['.', '/'])
                && !value.contains(char::is_whitespace)
            {
                facts.strings.push((line_of(token), value.to_owned()));
            }
        }
    }

    for (position, token) in tokens.iter().enumerate() {
        if token.kind() == SyntaxKind::STRING
            && string_value(token).as_deref() == Some(MANIFEST_DIRECTORY_VARIABLE)
            && !inside_include(&tokens, position)
        {
            let literals = statement_literals(&tokens, position);

            if !literals.is_empty() {
                facts.includes.push(Include {
                    line: line_of(token),
                    kind: IncludeKind::Data,
                    base: IncludeBase::ManifestDirectory,
                    path: literals.join("/"),
                });
            }
        }
    }
}

fn is_path_start(token: &SyntaxToken) -> bool {
    matches!(
        token.kind(),
        SyntaxKind::IDENT | T![crate] | T![self] | T![super]
    )
}

fn is_segment(token: &SyntaxToken) -> bool {
    matches!(token.kind(), SyntaxKind::IDENT | T![super] | T![self])
}

fn is_path_continuation(tokens: &[SyntaxToken], previous: usize) -> bool {
    let separates = |position: usize| separator_width(tokens, position).is_some();

    tokens[previous].kind() == T![.]
        || separates(previous)
        || previous > 0 && separator_width(tokens, previous - 1) == Some(2)
}

fn separator_width(tokens: &[SyntaxToken], position: usize) -> Option<usize> {
    let token = tokens.get(position)?;

    if token.kind() == T![::] {
        return Some(1);
    }

    let next = tokens.get(position + 1)?;
    let joint = token.text_range().end() == next.text_range().start();

    (token.kind() == T![:] && next.kind() == T![:] && joint).then_some(2)
}

fn include(tokens: &[SyntaxToken], kind: IncludeKind, line: u32) -> Option<Include> {
    let close = matching_close(tokens)?;
    let inner = &tokens[1..close];
    let literals: Vec<String> = inner.iter().filter_map(string_value).collect();
    let base = if literals
        .iter()
        .any(|literal| literal == MANIFEST_DIRECTORY_VARIABLE)
    {
        IncludeBase::ManifestDirectory
    } else if literals
        .iter()
        .any(|literal| literal == OUT_DIRECTORY_VARIABLE)
    {
        IncludeBase::OutDirectory
    } else {
        IncludeBase::File
    };
    let path: String = literals
        .iter()
        .filter(|literal| {
            *literal != MANIFEST_DIRECTORY_VARIABLE && *literal != OUT_DIRECTORY_VARIABLE
        })
        .map(String::as_str)
        .collect();

    (!path.is_empty() || base == IncludeBase::OutDirectory).then(|| Include {
        line,
        kind,
        base,
        path: path.trim_start_matches('/').to_owned(),
    })
}

fn matching_close(tokens: &[SyntaxToken]) -> Option<usize> {
    let mut depth = 0;

    for (position, token) in tokens.iter().enumerate() {
        match token.kind() {
            T!['('] | T!['['] | T!['{'] => depth += 1,
            T![')'] | T![']'] | T!['}'] => {
                depth -= 1;

                if depth == 0 {
                    return Some(position);
                }
            }
            _ => {}
        }
    }

    None
}

fn inside_include(tokens: &[SyntaxToken], position: usize) -> bool {
    tokens[..position].iter().rev().take(8).any(|token| {
        token.kind() == SyntaxKind::IDENT
            && INCLUDE_MACROS.iter().any(|(name, _)| *name == token.text())
    })
}

fn statement_literals(tokens: &[SyntaxToken], position: usize) -> Vec<String> {
    let start = tokens[..position]
        .iter()
        .rposition(|token| matches!(token.kind(), T![;] | T!['{'] | T!['}']))
        .map_or(0, |start| start + 1);
    let end = tokens[position..]
        .iter()
        .position(|token| matches!(token.kind(), T![;] | T!['}']))
        .map_or(tokens.len(), |end| position + end);

    tokens[start..end]
        .iter()
        .filter(|token| token.kind() == SyntaxKind::STRING)
        .filter_map(string_value)
        .filter(|literal| literal != MANIFEST_DIRECTORY_VARIABLE)
        .map(|literal| literal.replace("{}", "").trim_matches('/').to_owned())
        .filter(|literal| !literal.is_empty())
        .collect()
}

fn string_value(token: &SyntaxToken) -> Option<String> {
    let string = ast::String::cast(token.clone())?;

    string.value().ok().map(|value| value.to_string())
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];

    starts.extend(
        text.bytes()
            .enumerate()
            .filter(|(_, byte)| *byte == b'\n')
            .map(|(offset, _)| offset + 1),
    );

    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"
mod parser;
#[path = "generated/table.rs"]
mod table;
pub mod api {
    pub use crate::parser::{Parser, tokens::*};
    mod inner;
}

use std::collections::HashMap;
use app_core::config::{self as settings, Config};

#[macro_export]
macro_rules! check { () => {} }

impl Display for Config {}

fn run() {
    let raw = include_str!("../fixtures/input.txt");
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("data");
    assert_eq!(app_core::limits::MAX, helpers::compute());
    check!();
}
"#;

    #[test]
    fn modules_uses_macros_and_impls() {
        let facts = source_facts(SOURCE);
        let mods: Vec<(&str, Option<&str>, bool)> = facts
            .mods
            .iter()
            .map(|module| {
                (
                    module.name.as_str(),
                    module.path_attribute.as_deref(),
                    module.inline,
                )
            })
            .collect();
        let uses: Vec<(String, Option<&str>, bool, bool)> = facts
            .uses
            .iter()
            .map(|entry| {
                (
                    entry.path.join("::"),
                    entry.binding.as_deref(),
                    entry.glob,
                    entry.public,
                )
            })
            .collect();

        assert_eq!(
            mods,
            [
                ("parser", None, false),
                ("table", Some("generated/table.rs"), false),
                ("api", None, true),
                ("inner", None, false),
            ]
        );
        assert_eq!(facts.mods[3].scope, ["api"]);
        assert_eq!(
            uses,
            [
                (
                    "crate::parser::Parser".to_owned(),
                    Some("Parser"),
                    false,
                    true
                ),
                ("crate::parser::tokens".to_owned(), None, true, true),
                (
                    "std::collections::HashMap".to_owned(),
                    Some("HashMap"),
                    false,
                    false
                ),
                (
                    "app_core::config".to_owned(),
                    Some("settings"),
                    false,
                    false
                ),
                (
                    "app_core::config::Config".to_owned(),
                    Some("Config"),
                    false,
                    false
                ),
            ]
        );
        assert_eq!(facts.macro_rules, [("check".to_owned(), true)]);
        assert_eq!(facts.impls[0].self_type, Some(vec!["Config".to_owned()]));
        assert_eq!(facts.impls[0].trait_path, Some(vec!["Display".to_owned()]));
    }

    #[test]
    fn paths_inside_macro_bodies_and_included_files() {
        let facts = source_facts(SOURCE);
        let paths: Vec<String> = facts
            .paths
            .iter()
            .map(|path| path.segments.join("::"))
            .collect();

        assert!(paths.contains(&"app_core::limits::MAX".to_owned()));
        assert!(paths.contains(&"helpers::compute".to_owned()));
        assert!(paths.contains(&"check".to_owned()));
        assert_eq!(
            facts.includes,
            [
                Include {
                    line: 19,
                    kind: IncludeKind::Data,
                    base: IncludeBase::File,
                    path: "../fixtures/input.txt".to_owned(),
                },
                Include {
                    line: 20,
                    kind: IncludeKind::Data,
                    base: IncludeBase::ManifestDirectory,
                    path: "tests/data".to_owned(),
                },
            ]
        );
    }
}
