//! String anchors found in the code, reported without narrowing: objects read by a runtime key and the commands of command-line programs.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::files::RepoPath;
use crate::graph::Graph;
use crate::symbols::{SymbolId, SymbolKind, SymbolTarget};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registry {
    pub registry: String,
    pub file: RepoPath,
    pub keys: usize,
    pub readers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandFile {
    pub file: RepoPath,
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnchorSummary {
    pub registries: Vec<Registry>,
    pub commands: Vec<CommandFile>,
}

impl AnchorSummary {
    pub fn key_count(&self) -> usize {
        self.registries.iter().map(|registry| registry.keys).sum()
    }

    pub fn command_count(&self) -> usize {
        self.commands
            .iter()
            .map(|command_file| command_file.commands.len())
            .sum()
    }
}

pub(crate) fn anchor_summary(graph: &Graph) -> AnchorSummary {
    let mut keys: HashMap<SymbolId, usize> = HashMap::new();

    for (_, membership) in graph.memberships() {
        *keys.entry(membership.owner).or_default() += 1;
    }

    let mut readers: BTreeMap<SymbolId, BTreeSet<String>> = BTreeMap::new();

    for edge in graph.dispatch_edges() {
        let SymbolTarget::Symbol(registry) = edge.dependency else {
            continue;
        };
        let reader = graph.symbol(edge.dependent);
        let reader_file = graph.path(reader.file);
        let reader = if reader.kind == SymbolKind::ModuleInit {
            format!("{reader_file} at load")
        } else {
            format!("{} in {reader_file}", reader.name)
        };

        readers.entry(registry).or_default().insert(reader);
    }

    let mut registries: Vec<Registry> = readers
        .into_iter()
        .map(|(registry, readers)| {
            let symbol = graph.symbol(registry);

            Registry {
                registry: symbol.name.clone(),
                file: graph.path(symbol.file).clone(),
                keys: keys.get(&registry).copied().unwrap_or_default(),
                readers: readers.into_iter().collect(),
            }
        })
        .collect();

    registries.sort_by(|left, right| {
        right
            .keys
            .cmp(&left.keys)
            .then_with(|| left.file.cmp(&right.file))
            .then_with(|| left.registry.cmp(&right.registry))
    });

    let mut commands: Vec<CommandFile> = graph
        .commands()
        .map(|(file, paths)| CommandFile {
            file: graph.path(file).clone(),
            commands: paths
                .iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .cloned()
                .collect(),
        })
        .collect();

    commands.sort_by(|left, right| {
        right
            .commands
            .len()
            .cmp(&left.commands.len())
            .then_with(|| left.file.cmp(&right.file))
    });

    AnchorSummary {
        registries,
        commands,
    }
}
