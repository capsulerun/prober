//! What changed in one module between the base revision and head: its symbols compared by name, kind, token hash and uses, its exports, and the modules its initialisation loads.

use std::collections::{BTreeMap, BTreeSet};

use prober_core::symbols::{SymbolChangeKind, SymbolKind};

use crate::symbols::{ExportTarget, Imported, ModuleSymbols, Use};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Changed {
    Symbol {
        index: usize,
        kind: SymbolChangeKind,
    },
    Removed(String),
    Export(String),
    StarExport(String),
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum UseKey<'m> {
    Local(&'m str),
    Import {
        specifier: &'m str,
        imported: &'m Imported,
        member: Option<&'m str>,
    },
    Module(&'m str),
    Load(&'m str),
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ExportKey<'m> {
    Local(&'m str),
    Import(&'m str, &'m Imported),
    Reexport(&'m str, &'m str),
    Namespace(&'m str),
}

type ArgumentKey<'m> = (UseKey<'m>, usize, BTreeSet<UseKey<'m>>);

#[derive(Debug, PartialEq, Eq)]
struct Signature<'m> {
    kind: SymbolKind,
    hash: u64,
    uses: BTreeSet<UseKey<'m>>,
    loads: BTreeSet<&'m str>,
    load_arguments: BTreeSet<ArgumentKey<'m>>,
}

pub(crate) fn module_changes(base: &ModuleSymbols, head: &ModuleSymbols) -> Vec<Changed> {
    let base_names: BTreeMap<&str, usize> = names(base);
    let head_names: BTreeMap<&str, usize> = names(head);
    let mut changed = Vec::new();
    let mut added = Vec::new();

    for (&name, &index) in &head_names {
        match base_names.get(name) {
            Some(&base_index) if signature(base, base_index) == signature(head, index) => {}
            Some(_) => changed.push(Changed::Symbol {
                index,
                kind: SymbolChangeKind::Modified,
            }),
            None => added.push(index),
        }
    }

    let mut removed: Vec<usize> = base_names
        .iter()
        .filter(|(name, _)| !head_names.contains_key(*name))
        .map(|(_, &index)| index)
        .collect();

    for index in added {
        let head_signature = signature(head, index);
        let renamed_from = removed
            .iter()
            .position(|&base_index| signature(base, base_index) == head_signature);
        let kind = match renamed_from {
            Some(position) => SymbolChangeKind::Renamed {
                from: base.symbols[removed.remove(position)].name.clone(),
            },
            None => SymbolChangeKind::Added,
        };

        changed.push(Changed::Symbol { index, kind });
    }

    let changed_names: BTreeSet<&str> = changed
        .iter()
        .filter_map(|changed| match changed {
            Changed::Symbol { index, .. } => Some(head.symbols[*index].name.as_str()),
            Changed::Removed(_) | Changed::Export(_) | Changed::StarExport(_) => None,
        })
        .collect();
    let mut removed_names: BTreeSet<&str> = removed
        .into_iter()
        .map(|index| base.symbols[index].name.as_str())
        .collect();
    let base_exports = export_keys(base);
    let head_exports = export_keys(head);

    for (name, keys) in &head_exports {
        let follows_changed_symbols = keys
            .iter()
            .all(|key| matches!(key, ExportKey::Local(local) if changed_names.contains(local)));

        if base_exports.get(name) != Some(keys) && !follows_changed_symbols {
            changed.push(Changed::Export((*name).to_owned()));
        }
    }

    removed_names.extend(
        base_exports
            .keys()
            .filter(|name| !head_exports.contains_key(*name)),
    );

    let base_stars: BTreeSet<&str> = base.star_exports.iter().map(String::as_str).collect();
    let head_stars: BTreeSet<&str> = head.star_exports.iter().map(String::as_str).collect();

    changed.extend(
        head_stars
            .difference(&base_stars)
            .map(|specifier| Changed::StarExport((*specifier).to_owned())),
    );
    removed_names.extend(base_stars.difference(&head_stars));
    changed.extend(
        removed_names
            .into_iter()
            .map(|name| Changed::Removed(name.to_owned())),
    );

    changed
}

fn names(module: &ModuleSymbols) -> BTreeMap<&str, usize> {
    module
        .symbols
        .iter()
        .enumerate()
        .map(|(index, symbol)| (symbol.name.as_str(), index))
        .collect()
}

fn signature(module: &ModuleSymbols, index: usize) -> Signature<'_> {
    let symbol = &module.symbols[index];
    let is_init = index == ModuleSymbols::INIT;
    let loads = if is_init {
        module.loads.iter().map(String::as_str).collect()
    } else {
        BTreeSet::new()
    };
    let load_arguments = if is_init {
        module
            .load_arguments
            .iter()
            .map(|argument| {
                let uses = argument
                    .uses
                    .iter()
                    .map(|symbol_use| use_key(module, symbol_use))
                    .collect();

                (use_key(module, &argument.callee), argument.index, uses)
            })
            .collect()
    } else {
        BTreeSet::new()
    };

    Signature {
        kind: symbol.kind,
        hash: symbol.hash,
        uses: symbol
            .uses
            .iter()
            .map(|symbol_use| use_key(module, symbol_use))
            .collect(),
        loads,
        load_arguments,
    }
}

fn use_key<'m>(module: &'m ModuleSymbols, symbol_use: &'m Use) -> UseKey<'m> {
    match symbol_use {
        Use::Local(index) => UseKey::Local(&module.symbols[*index].name),
        Use::Import { binding, member } => {
            let binding = &module.imports[*binding];

            UseKey::Import {
                specifier: &binding.specifier,
                imported: &binding.imported,
                member: member.as_deref(),
            }
        }
        Use::Module(specifier) => UseKey::Module(specifier),
        Use::Load(specifier) => UseKey::Load(specifier),
    }
}

fn export_keys(module: &ModuleSymbols) -> BTreeMap<&str, BTreeSet<ExportKey<'_>>> {
    let mut keys: BTreeMap<&str, BTreeSet<ExportKey>> = BTreeMap::new();

    for (name, target) in &module.exports {
        let key = match target {
            ExportTarget::Local(index) => ExportKey::Local(&module.symbols[*index].name),
            ExportTarget::Import(binding) => {
                let binding = &module.imports[*binding];

                ExportKey::Import(&binding.specifier, &binding.imported)
            }
            ExportTarget::Reexport { specifier, name } => ExportKey::Reexport(specifier, name),
            ExportTarget::Namespace(specifier) => ExportKey::Namespace(specifier),
        };

        keys.entry(name).or_default().insert(key);
    }

    keys
}

#[cfg(test)]
mod tests {
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_parser::config::RuntimeParserConfig;
    use oxc_span::SourceType;

    use super::*;
    use crate::symbols::extract_symbols;

    fn module_symbols(source_text: &str) -> ModuleSymbols {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, source_text, SourceType::ts())
            .with_config(RuntimeParserConfig::new(true))
            .parse();

        extract_symbols(&parsed.program, source_text, &parsed.tokens).unwrap()
    }

    fn described_changes(base: &str, head: &str) -> Vec<String> {
        let head_symbols = module_symbols(head);

        module_changes(&module_symbols(base), &head_symbols)
            .into_iter()
            .map(|changed| match changed {
                Changed::Symbol { index, kind } => {
                    let name = &head_symbols.symbols[index].name;

                    match kind {
                        SymbolChangeKind::Renamed { from } => format!("{name} renamed from {from}"),
                        kind => format!("{name} {kind:?}").to_lowercase(),
                    }
                }
                Changed::Removed(name) => format!("{name} removed"),
                Changed::Export(name) => format!("export {name}"),
                Changed::StarExport(specifier) => format!("export * from {specifier}"),
            })
            .collect()
    }

    #[test]
    fn symbols_change_when_their_tokens_or_what_they_use_change() {
        let base = r#"
            import { fetch } from "./http";
            import { log } from "./log";

            export function load(id: string) { return fetch(id); }
            export function save(id: string) { log(id); }
            export function keep() { return 1; }
        "#;
        let head = r#"
            import { fetch } from "./http2";
            import { log } from "./log";

            // Saves the item.
            export function load(id: string) { return fetch(id); }
            export function save(id: string) { log(id, 1); }
            export function keep() {
                return 1;
            }
        "#;

        assert_eq!(
            described_changes(base, head),
            ["<module> modified", "load modified", "save modified"]
        );
    }

    #[test]
    fn symbols_are_added_removed_or_renamed_when_an_added_one_has_a_removed_ones_tokens() {
        let base = r#"
            export function makeChannel() { return 1; }
            export function drop() { return 2; }
        "#;
        let head = r#"
            export function createChannel() { return 1; }
            export function open() { return 3; }
        "#;

        assert_eq!(
            described_changes(base, head),
            [
                "createChannel renamed from makeChannel",
                "open added",
                "drop removed",
                "makeChannel removed",
            ]
        );
    }

    #[test]
    fn initialisation_changes_with_top_level_code_and_the_modules_it_loads_but_not_import_names() {
        let base = r#"
            import { a } from "./a";
            import "./polyfill";

            export const f = () => a();
            start();
        "#;
        let same_loads = r#"
            import { a, b } from "./a";
            import "./polyfill";

            export const f = () => a();
            start();
        "#;
        let other_loads = r#"
            import { a } from "./a";
            import "./other-polyfill";

            export const f = () => a();
            start();
        "#;
        let other_code = r#"
            import { a } from "./a";
            import "./polyfill";

            export const f = () => a();
            start(1);
        "#;

        assert_eq!(described_changes(base, same_loads), Vec::<String>::new());
        assert_eq!(described_changes(base, other_loads), ["<module> modified"]);
        assert_eq!(described_changes(base, other_code), ["<module> modified"]);
    }

    #[test]
    fn exports_change_when_a_name_points_elsewhere_and_star_exports_when_added_or_removed() {
        let base = r#"
            export { a } from "./a";
            export { b } from "./b";
            export * from "./c";
        "#;
        let head = r#"
            export { a } from "./a2";
            export { b } from "./b";
            export * from "./d";
        "#;

        assert_eq!(
            described_changes(base, head),
            [
                "<module> modified",
                "export a",
                "export * from ./d",
                "./c removed",
            ]
        );
    }
}
