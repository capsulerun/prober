//! What changed in one module between the base revision and head: its symbols and members compared by name, kind, token hash and uses, what calling its functions or loading it runs apart from the callbacks it only hands on, its exports, and the modules its initialisation loads.

use std::collections::{BTreeMap, BTreeSet};

use prober_core::symbols::{SymbolChangeKind, SymbolKind};

use crate::symbols::{
    ExportTarget, Held, Imported, Keeping, ModuleSymbols, Place, Runs, Shape, Use,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Changed {
    Symbol {
        index: usize,
        kind: SymbolChangeKind,
    },
    Held {
        index: usize,
        groups: Vec<usize>,
    },
    Runs {
        index: usize,
        groups: Option<Vec<usize>>,
    },
    Removed(String),
    Export(String),
    StarExport(String),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum UseKey<'m> {
    Local(&'m str),
    Import {
        specifier: &'m str,
        imported: &'m Imported,
        member: Option<&'m str>,
    },
    Module(&'m str),
    Load(&'m str),
    Member(&'m str, &'m str),
    This(&'m str, &'m str, Place),
    Super(&'m str, &'m str, Place),
    Call(Box<UseKey<'m>>),
    New(Box<UseKey<'m>>),
    Keyed(Box<UseKey<'m>>),
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ExportKey<'m> {
    Local(&'m str),
    Import(&'m str, &'m Imported),
    Reexport(&'m str, &'m str),
    Namespace(&'m str),
}

type KeepingKey<'m> = (UseKey<'m>, usize, bool);

type HeldKey<'m> = BTreeSet<KeepingKey<'m>>;

#[derive(Debug, PartialEq, Eq)]
struct HeldCode<'m> {
    hash: u64,
    uses: BTreeSet<UseKey<'m>>,
}

#[derive(Debug, PartialEq, Eq)]
struct Signature<'m> {
    kind: SymbolKind,
    hash: u64,
    uses: BTreeSet<UseKey<'m>>,
    member: Option<(&'m str, Place, BTreeSet<&'m str>)>,
    loads: BTreeSet<&'m str>,
}

enum Difference {
    Same,
    Code,
    Held(Vec<usize>),
}

pub(crate) fn module_changes(base: &ModuleSymbols, head: &ModuleSymbols) -> Vec<Changed> {
    let base_names: BTreeMap<&str, usize> = names(base);
    let head_names: BTreeMap<&str, usize> = names(head);
    let mut changed = Vec::new();
    let mut added = Vec::new();

    for (&name, &index) in &head_names {
        let Some(&base_index) = base_names.get(name) else {
            added.push(index);

            continue;
        };

        if signature(base, base_index) != signature(head, index) {
            changed.push(Changed::Symbol {
                index,
                kind: SymbolChangeKind::Modified,
            });
        } else if index == ModuleSymbols::INIT {
            match held_difference(base, &base.held, head, &head.held) {
                Difference::Same => {}
                Difference::Code => changed.push(Changed::Symbol {
                    index,
                    kind: SymbolChangeKind::Modified,
                }),
                Difference::Held(groups) => changed.push(Changed::Held { index, groups }),
            }
        }

        match runs_difference(base, base_index, head, index) {
            Difference::Same => {}
            Difference::Code => changed.push(Changed::Runs {
                index,
                groups: None,
            }),
            Difference::Held(groups) => changed.push(Changed::Runs {
                index,
                groups: Some(groups),
            }),
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

    let mut owners: BTreeSet<usize> = BTreeSet::new();

    for changed in &changed {
        if let Changed::Symbol {
            index,
            kind: SymbolChangeKind::Added | SymbolChangeKind::Renamed { .. },
        } = changed
            && let Some(member) = &head.symbols[*index].member
        {
            owners.insert(member.owner);
        }
    }

    for &base_index in &removed {
        if let Some(member) = &base.symbols[base_index].member
            && let Some(&owner) = head_names.get(base.symbols[member.owner].name.as_str())
        {
            owners.insert(owner);
        }
    }

    let whole_owners: BTreeSet<usize> = changed
        .iter()
        .filter_map(|changed| match changed {
            Changed::Symbol {
                index,
                kind: SymbolChangeKind::Modified,
            } if head.symbols[*index].kind == SymbolKind::Class
                || no_longer_open(base, &base_names, head, *index) =>
            {
                Some(*index)
            }
            _ => None,
        })
        .collect();
    let mut modified: BTreeSet<usize> = changed
        .iter()
        .filter_map(|changed| match changed {
            Changed::Symbol { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    let members_of_whole_owners: Vec<usize> = head
        .symbols
        .iter()
        .enumerate()
        .filter(|(_, symbol)| {
            symbol
                .member
                .as_ref()
                .is_some_and(|member| whole_owners.contains(&member.owner))
        })
        .map(|(index, _)| index)
        .collect();

    for index in owners.into_iter().chain(members_of_whole_owners) {
        if modified.insert(index) {
            changed.push(Changed::Symbol {
                index,
                kind: SymbolChangeKind::Modified,
            });
        }
    }

    let changed_names: BTreeSet<&str> = changed
        .iter()
        .filter_map(|changed| match changed {
            Changed::Symbol { index, .. } => Some(head.symbols[*index].name.as_str()),
            Changed::Held { .. }
            | Changed::Runs { .. }
            | Changed::Removed(_)
            | Changed::Export(_)
            | Changed::StarExport(_) => None,
        })
        .collect();
    let mut removed_names: BTreeSet<&str> = removed
        .into_iter()
        .filter(|&index| base.symbols[index].member.is_none())
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

fn no_longer_open(
    base: &ModuleSymbols,
    base_names: &BTreeMap<&str, usize>,
    head: &ModuleSymbols,
    index: usize,
) -> bool {
    let symbol = &head.symbols[index];

    symbol.shape == Some(Shape::Object { open: false })
        && base_names
            .get(symbol.name.as_str())
            .is_some_and(|&base_index| {
                base.symbols[base_index].shape == Some(Shape::Object { open: true })
            })
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
    let loads = if index == ModuleSymbols::INIT {
        module.loads.iter().map(String::as_str).collect()
    } else {
        BTreeSet::new()
    };

    Signature {
        kind: symbol.kind,
        hash: symbol.hash,
        uses: use_keys(module, &symbol.uses),
        member: symbol.member.as_ref().map(|member| {
            (
                member.key.as_str(),
                member.place,
                member.assigns.iter().map(String::as_str).collect(),
            )
        }),
        loads,
    }
}

fn runs_difference(
    base: &ModuleSymbols,
    base_index: usize,
    head: &ModuleSymbols,
    index: usize,
) -> Difference {
    match (&base.symbols[base_index].runs, &head.symbols[index].runs) {
        (None, None) => Difference::Same,
        (Some(base_runs), Some(head_runs)) => {
            if runs_code(base, base_runs) != runs_code(head, head_runs) {
                return Difference::Code;
            }

            held_difference(base, &base_runs.held, head, &head_runs.held)
        }
        _ => Difference::Code,
    }
}

fn runs_code<'m>(module: &'m ModuleSymbols, runs: &'m Runs) -> HeldCode<'m> {
    HeldCode {
        hash: runs.hash,
        uses: use_keys(module, &runs.uses),
    }
}

fn held_difference(
    base: &ModuleSymbols,
    base_held: &[Held],
    head: &ModuleSymbols,
    head_held: &[Held],
) -> Difference {
    let base_groups: BTreeMap<HeldKey, HeldCode> = base_held
        .iter()
        .map(|held| held_entry(base, held))
        .collect();
    let mut head_keys = BTreeSet::new();
    let mut groups = Vec::new();

    for (group, held) in head_held.iter().enumerate() {
        let (key, code) = held_entry(head, held);

        if base_groups.get(&key) != Some(&code) {
            groups.push(group);
        }

        head_keys.insert(key);
    }

    if base_groups.keys().any(|key| !head_keys.contains(key)) {
        return Difference::Code;
    }

    if groups.is_empty() {
        Difference::Same
    } else {
        Difference::Held(groups)
    }
}

fn held_entry<'m>(module: &'m ModuleSymbols, held: &'m Held) -> (HeldKey<'m>, HeldCode<'m>) {
    let key = held
        .kept_by
        .iter()
        .map(|keeping| keeping_key(module, keeping))
        .collect();

    (
        key,
        HeldCode {
            hash: held.hash,
            uses: use_keys(module, &held.uses),
        },
    )
}

fn keeping_key<'m>(module: &'m ModuleSymbols, keeping: &'m Keeping) -> KeepingKey<'m> {
    (
        use_key(module, &keeping.callee),
        keeping.index,
        keeping.fresh_target,
    )
}

fn use_keys<'m>(module: &'m ModuleSymbols, uses: &'m [Use]) -> BTreeSet<UseKey<'m>> {
    uses.iter()
        .map(|symbol_use| use_key(module, symbol_use))
        .collect()
}

fn use_key<'m>(module: &'m ModuleSymbols, symbol_use: &'m Use) -> UseKey<'m> {
    let name = |index: usize| module.symbols[index].name.as_str();

    match symbol_use {
        Use::Local(index) => UseKey::Local(name(*index)),
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
        Use::Member { symbol, member } => UseKey::Member(name(*symbol), member),
        Use::This {
            class,
            member,
            place,
        } => UseKey::This(name(*class), member, *place),
        Use::Super {
            class,
            member,
            place,
        } => UseKey::Super(name(*class), member, *place),
        Use::Call(called) => UseKey::Call(Box::new(use_key(module, called))),
        Use::New(constructed) => UseKey::New(Box::new(use_key(module, constructed))),
        Use::Keyed(read) => UseKey::Keyed(Box::new(use_key(module, read))),
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
                        kind => format!("{name} {}", format!("{kind:?}").to_lowercase()),
                    }
                }
                Changed::Held { index, groups } => {
                    format!("{} held {groups:?}", head_symbols.symbols[index].name)
                }
                Changed::Runs { index, groups } => {
                    let name = &head_symbols.symbols[index].name;

                    match groups {
                        Some(groups) => format!("{name}() held {groups:?}"),
                        None => format!("{name}() modified"),
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
            [
                "<module> modified",
                "load modified",
                "load() modified",
                "save modified"
            ]
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

    #[test]
    fn class_head_changes_reach_every_member_and_a_removed_member_its_owner() {
        let base = r#"
            export class Store extends Base {
                add(item: string) { return item; }
                clear() { return 0; }
            }
        "#;
        let new_base = base.replace("extends Base", "extends Other");
        let without_clear = base.replace("clear() { return 0; }", "");

        assert_eq!(
            described_changes(base, &new_base),
            [
                "Store modified",
                "Store.add modified",
                "Store.clear modified"
            ]
        );
        assert_eq!(described_changes(base, &without_clear), ["Store modified"]);
    }

    #[test]
    fn an_object_no_longer_spreading_changes_every_property() {
        let base = "export const o = { a: 1, b: 2, ...extra };\nconst extra = { a: 3 };\n";
        let closed = base.replace(", ...extra", "");
        let added = base.replace("b: 2,", "b: 2, c: 4,");

        assert_eq!(
            described_changes(base, &closed),
            ["o modified", "o.a modified", "o.b modified"]
        );
        assert_eq!(described_changes(base, &added), ["o modified", "o.c added"]);
    }

    #[test]
    fn code_created_or_handed_on_at_load_changes_only_its_own_symbols() {
        let base = r#"
            import { wrap } from "./wrap";
            export function createHandlers() {
                prepare();
                return { greet: (name: string) => `hi ${name}` };
            }
            export const handler = wrap(() => compute(1));
        "#;
        let inside_created = base.replace("`hi ${name}`", "`hello ${name}`");
        let inside_handed_on = base.replace("compute(1)", "compute(2)");
        let run_when_called = base.replace("prepare();", "prepare(1);");

        assert_eq!(
            described_changes(base, &inside_created),
            ["createHandlers modified"]
        );
        assert_eq!(
            described_changes(base, &inside_handed_on),
            ["<module> held [0]", "handler modified"]
        );
        assert_eq!(
            described_changes(base, &run_when_called),
            ["createHandlers modified", "createHandlers() modified"]
        );
    }
}
