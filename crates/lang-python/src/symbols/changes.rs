//! What changed in one module between the base revision and head: its symbols compared by name, kind, fingerprint and what the names they use are bound to, load-time code with the modules it loads, and the names bound by imports.

use std::collections::{BTreeMap, BTreeSet};

use prober_core::symbols::{SymbolChangeKind, SymbolKind};

use crate::symbols::{Binding, Imported, ModuleSymbols};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Changed {
    Symbol {
        index: usize,
        kind: SymbolChangeKind,
    },
    Removed {
        name: String,
        keys: Vec<String>,
        class: Option<String>,
    },
    Binding(String),
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum BindingKey<'m> {
    Local(&'m str),
    Import(&'m Imported),
    Unbound,
}

#[derive(Debug, PartialEq, Eq)]
struct Signature<'m> {
    kind: SymbolKind,
    hash: u64,
    uses: BTreeSet<(Vec<BindingKey<'m>>, &'m [String])>,
}

pub(crate) fn module_changes(
    base: &ModuleSymbols,
    base_loads: &BTreeSet<String>,
    head: &ModuleSymbols,
    head_loads: &BTreeSet<String>,
) -> Vec<Changed> {
    let base_names = symbol_names(base);
    let head_names = symbol_names(head);
    let mut changed = Vec::new();

    for (&name, &index) in &head_names {
        let Some(&base_index) = base_names.get(name) else {
            changed.push(Changed::Symbol {
                index,
                kind: SymbolChangeKind::Added,
            });

            continue;
        };
        let init_changed = index == ModuleSymbols::INIT
            && (base_loads != head_loads || base.star_imports != head.star_imports);

        if init_changed || signature(base, base_index) != signature(head, index) {
            changed.push(Changed::Symbol {
                index,
                kind: SymbolChangeKind::Modified,
            });
        }
    }

    changed.extend(
        base_names
            .iter()
            .filter(|(name, _)| !head_names.contains_key(*name))
            .map(|(name, &index)| {
                let member = base.symbols[index].member.as_ref();

                Changed::Removed {
                    name: (*name).to_owned(),
                    keys: member.map(|member| member.keys.clone()).unwrap_or_default(),
                    class: member.map(|member| base.symbols[member.class].name.clone()),
                }
            }),
    );

    let base_imports = import_bindings(base);
    let head_imports = import_bindings(head);
    let bound: BTreeSet<&str> = base_imports
        .keys()
        .chain(head_imports.keys())
        .copied()
        .collect();

    changed.extend(
        bound
            .into_iter()
            .filter(|name| base_imports.get(name) != head_imports.get(name))
            .map(|name| Changed::Binding(name.to_owned())),
    );

    changed
}

fn symbol_names(module: &ModuleSymbols) -> BTreeMap<&str, usize> {
    module
        .symbols
        .iter()
        .enumerate()
        .map(|(index, symbol)| (symbol.name.as_str(), index))
        .collect()
}

fn import_bindings(module: &ModuleSymbols) -> BTreeMap<&str, BTreeSet<&Imported>> {
    let mut bindings: BTreeMap<&str, BTreeSet<&Imported>> = BTreeMap::new();

    for (name, bound) in &module.names {
        for binding in bound {
            if let Binding::Import(imported) = binding {
                bindings.entry(name.as_str()).or_default().insert(imported);
            }
        }
    }

    bindings
}

fn signature(module: &ModuleSymbols, index: usize) -> Signature<'_> {
    let symbol = &module.symbols[index];
    let uses = symbol
        .uses
        .iter()
        .map(|symbol_use| {
            let keys = match symbol.imports.get(&symbol_use.name) {
                Some(imported) => imported.iter().map(BindingKey::Import).collect(),
                None => match module.names.get(&symbol_use.name) {
                    Some(bindings) => bindings
                        .iter()
                        .map(|binding| match binding {
                            Binding::Local(local) => {
                                BindingKey::Local(module.symbols[*local].name.as_str())
                            }
                            Binding::Import(imported) => BindingKey::Import(imported),
                        })
                        .collect(),
                    None => vec![BindingKey::Unbound],
                },
            };

            (keys, symbol_use.attributes.as_slice())
        })
        .collect();

    Signature {
        kind: symbol.kind,
        hash: symbol.hash,
        uses,
    }
}
