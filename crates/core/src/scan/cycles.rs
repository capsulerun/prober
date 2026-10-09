//! Import cycles: modules that reach each other through imports, the tests they pull in, and what is left of them at symbol level.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use crate::cycles::strongly_connected_components;
use crate::files::{FileId, RepoPath};
use crate::graph::{ContentChange, Edge, EdgeKind, Graph};
use crate::scan::tests_depending_on;
use crate::symbols::{FallbackCause, SymbolId, SymbolTarget, WholeModuleCause};

const MEASURED_SYMBOL_CYCLES: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolCycle {
    pub symbols: usize,
    pub modules: usize,
    pub tests: usize,
    pub leading_in: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolCycles {
    pub symbols: usize,
    pub cycles: Vec<SymbolCycle>,
    pub file_level: Vec<(RepoPath, FallbackCause)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportCycle {
    pub modules: Vec<RepoPath>,
    pub tests: usize,
    pub symbols: Option<SymbolCycles>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SymbolSummary {
    pub symbols: usize,
    pub modules: usize,
    pub file_level: Vec<(RepoPath, FallbackCause)>,
    pub whole_module_uses: BTreeMap<WholeModuleCause, usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Node {
    Symbol(SymbolId),
    File(FileId),
}

pub(crate) fn symbol_summary(graph: &Graph) -> SymbolSummary {
    let mut whole_module_uses: BTreeMap<WholeModuleCause, usize> = BTreeMap::new();

    for edge in graph.symbol_edges() {
        if let SymbolTarget::Module { file, cause } = edge.dependency
            && graph.is_module(graph.path(file))
        {
            *whole_module_uses.entry(cause).or_default() += 1;
        }
    }

    SymbolSummary {
        symbols: graph.symbol_count(),
        modules: graph.analysed_module_count(),
        file_level: file_level_modules(graph, |_| true),
        whole_module_uses,
    }
}

pub(crate) fn import_cycles(graph: &Graph, repo_files: &[RepoPath]) -> Vec<ImportCycle> {
    let mut module_paths: Vec<&RepoPath> = repo_files
        .iter()
        .filter(|path| graph.is_module(path))
        .collect();

    module_paths.sort();
    module_paths.dedup();

    let modules: Vec<FileId> = module_paths
        .into_iter()
        .filter_map(|path| graph.file_id(path))
        .filter(|&file| !graph.is_test(file))
        .collect();
    let module_indices: HashMap<FileId, usize> = modules
        .iter()
        .enumerate()
        .map(|(index, &file)| (file, index))
        .collect();
    let components = strongly_connected_components(modules.len(), |index, successors| {
        successors.extend(
            graph
                .dependencies(modules[index])
                .filter(|edge| is_code_edge(edge.kind))
                .filter_map(|edge| module_indices.get(&edge.dependency).copied()),
        );
    });

    components
        .into_iter()
        .map(|component| {
            let files: Vec<FileId> = component.into_iter().map(|index| modules[index]).collect();
            let mut module_paths: Vec<RepoPath> =
                files.iter().map(|&file| graph.path(file).clone()).collect();

            module_paths.sort();

            ImportCycle {
                modules: module_paths,
                tests: tests_depending_on(graph, files.iter().copied()).len(),
                symbols: symbol_cycles(graph, &files),
            }
        })
        .collect()
}

fn is_code_edge(kind: EdgeKind) -> bool {
    matches!(kind, EdgeKind::Import | EdgeKind::Dynamic)
}

fn file_level_modules(
    graph: &Graph,
    includes: impl Fn(FileId) -> bool,
) -> Vec<(RepoPath, FallbackCause)> {
    let mut file_level: Vec<(RepoPath, FallbackCause)> = graph
        .fallbacks()
        .iter()
        .filter(|fallback| includes(fallback.file))
        .map(|fallback| (graph.path(fallback.file).clone(), fallback.cause))
        .collect();

    file_level.sort();

    file_level
}

fn symbol_cycles(graph: &Graph, files: &[FileId]) -> Option<SymbolCycles> {
    let in_cycle: HashSet<FileId> = files.iter().copied().collect();

    if files.iter().all(|&file| graph.symbols_of(file).is_empty()) {
        return None;
    }

    let mut nodes: Vec<Node> = Vec::new();

    for &file in files {
        nodes.push(Node::File(file));
        nodes.extend(
            graph
                .symbols_of(file)
                .iter()
                .map(|&symbol| Node::Symbol(symbol)),
        );
    }

    let node_indices: HashMap<Node, usize> = nodes
        .iter()
        .enumerate()
        .map(|(index, &node)| (node, index))
        .collect();
    let dynamic_loads = attributed_dynamic_loads(graph, files, &in_cycle);
    let successors = |index: usize, successors: &mut Vec<usize>| {
        let node_successors = node_successors(graph, nodes[index], &in_cycle, &dynamic_loads);

        successors
            .extend(node_successors.filter_map(|successor| node_indices.get(&successor).copied()));
    };
    let mut components = strongly_connected_components(nodes.len(), successors);
    let symbol_count = |component: &[usize]| {
        component
            .iter()
            .filter(|&&index| matches!(nodes[index], Node::Symbol(_)))
            .count()
    };

    components.sort_by_key(|component| Reverse(symbol_count(component)));
    components.retain(|component| symbol_count(component) > 1);
    components.truncate(MEASURED_SYMBOL_CYCLES);

    let leading_into = |cycle: &[usize]| {
        let mut reached: HashSet<usize> = cycle.iter().copied().collect();
        let mut queue: VecDeque<usize> = cycle.iter().copied().collect();
        let mut successor_indices = Vec::new();

        while let Some(index) = queue.pop_front() {
            successor_indices.clear();
            successors(index, &mut successor_indices);

            for &successor in &successor_indices {
                if reached.insert(successor) {
                    queue.push_back(successor);
                }
            }
        }

        reached
            .into_iter()
            .filter(|&index| matches!(nodes[index], Node::Symbol(_)))
            .count()
    };
    let mut cycles: Vec<SymbolCycle> = components
        .into_iter()
        .map(|component| {
            let modules: HashSet<FileId> = component
                .iter()
                .map(|&index| match nodes[index] {
                    Node::Symbol(symbol) => graph.symbol(symbol).file,
                    Node::File(file) => file,
                })
                .collect();

            SymbolCycle {
                symbols: symbol_count(&component),
                modules: modules.len(),
                tests: tests_at_symbol_level(graph, component.iter().map(|&index| nodes[index])),
                leading_in: leading_into(&component),
            }
        })
        .collect();

    cycles.sort_by_key(|cycle| Reverse((cycle.tests, cycle.symbols)));

    Some(SymbolCycles {
        symbols: nodes
            .iter()
            .filter(|node| matches!(node, Node::Symbol(_)))
            .count(),
        cycles,
        file_level: file_level_modules(graph, |file| in_cycle.contains(&file)),
    })
}

fn tests_at_symbol_level(graph: &Graph, starts: impl IntoIterator<Item = Node>) -> usize {
    let mut reached: HashSet<Node> = HashSet::new();
    let mut queue: VecDeque<Node> = VecDeque::new();
    let mut tests: HashSet<FileId> = HashSet::new();

    for start in starts {
        if reached.insert(start) {
            queue.push_back(start);
        }
    }

    while let Some(node) = queue.pop_front() {
        let mut next: Vec<Node> = Vec::new();
        let file = match node {
            Node::Symbol(symbol) => {
                let file = graph.symbol(symbol).file;

                next.extend(
                    graph
                        .symbol_dependents(symbol)
                        .filter(|edge| edge.kind.selects_tests())
                        .map(|edge| Node::Symbol(edge.dependent)),
                );
                next.push(Node::File(file));

                file
            }
            Node::File(file) => {
                next.extend(
                    graph
                        .module_dependents(file)
                        .filter(|edge| edge.kind.selects_tests())
                        .map(|edge| Node::Symbol(edge.dependent)),
                );
                next.extend(file_dependents_at_symbol_level(graph, file));

                file
            }
        };

        if graph.is_test(file) {
            tests.insert(file);
        }

        for node in next {
            if reached.insert(node) {
                queue.push_back(node);
            }
        }
    }

    tests.len()
}

fn file_dependents_at_symbol_level(graph: &Graph, file: FileId) -> Vec<Node> {
    let mut dependents = Vec::new();

    for edge in graph.dependents(file) {
        if !edge.kind.selects_tests() || !edge.kind.follows(ContentChange::None) {
            continue;
        }

        if graph.symbols_of(edge.dependent).is_empty() {
            dependents.push(Node::File(edge.dependent));
        } else if !matches!(edge.kind, EdgeKind::Import | EdgeKind::Asset) {
            dependents.extend(linking_symbols(graph, &edge).into_iter().map(Node::Symbol));
        }
    }

    dependents
}

fn attributed_dynamic_loads(
    graph: &Graph,
    files: &[FileId],
    in_cycle: &HashSet<FileId>,
) -> HashMap<SymbolId, Vec<FileId>> {
    let mut dynamic_loads: HashMap<SymbolId, Vec<FileId>> = HashMap::new();

    for &file in files {
        if graph.symbols_of(file).is_empty() {
            continue;
        }

        let loads = graph
            .dependencies(file)
            .filter(|edge| edge.kind == EdgeKind::Dynamic && in_cycle.contains(&edge.dependency));

        for edge in loads {
            for symbol in linking_symbols(graph, &edge) {
                dynamic_loads
                    .entry(symbol)
                    .or_default()
                    .push(edge.dependency);
            }
        }
    }

    dynamic_loads
}

fn linking_symbols(graph: &Graph, edge: &Edge) -> Vec<SymbolId> {
    match graph.inference(edge) {
        Some(inference) => graph.symbols_at(edge.dependent, inference.line),
        None => graph.symbols_of(edge.dependent).to_vec(),
    }
}

fn node_successors<'g>(
    graph: &'g Graph,
    node: Node,
    in_cycle: &'g HashSet<FileId>,
    dynamic_loads: &'g HashMap<SymbolId, Vec<FileId>>,
) -> Box<dyn Iterator<Item = Node> + 'g> {
    match node {
        Node::Symbol(symbol) => {
            let uses = graph
                .symbol_dependencies(symbol)
                .iter()
                .filter(|edge| edge.kind == EdgeKind::Import)
                .map(|edge| match edge.dependency {
                    SymbolTarget::Symbol(dependency) => Node::Symbol(dependency),
                    SymbolTarget::Module { file, .. } => Node::File(file),
                });
            let loads = dynamic_loads
                .get(&symbol)
                .into_iter()
                .flatten()
                .map(|&file| Node::File(file));

            Box::new(uses.chain(loads))
        }
        Node::File(file) if !graph.symbols_of(file).is_empty() => Box::new(
            graph
                .symbols_of(file)
                .iter()
                .map(|&symbol| Node::Symbol(symbol)),
        ),
        Node::File(file) => Box::new(
            graph
                .dependencies(file)
                .filter(|edge| is_code_edge(edge.kind) && in_cycle.contains(&edge.dependency))
                .map(|edge| Node::File(edge.dependency)),
        ),
    }
}
