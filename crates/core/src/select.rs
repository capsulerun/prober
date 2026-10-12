use std::cmp::Reverse;
use std::collections::hash_map::Entry;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};

use crate::changes::FileChange;
use crate::files::{FileId, RepoPath};
use crate::graph::{ContentChange, Edge, EdgeKind, Graph, UnresolvedSite};
use crate::symbols::{SymbolEdge, SymbolId, SymbolKind, SymbolTarget};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Widening {
    Scoped,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relaxation {
    TestSupport,
    Ignores,
    Mocks,
    Content,
    TypeOnly,
    Assets,
    FileLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    Edge {
        kind: EdgeKind,
        dependency: FileId,
    },
    Uses(SymbolEdge),
    Loads {
        dependency: FileId,
        dependent: SymbolId,
    },
    ChangedSymbol(usize),
    ChangedPart(usize),
    UnresolvedSite(usize),
    SetAsideSite(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hop {
    pub dependent: FileId,
    pub link: Link,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    pub change: FileChange,
    pub hops: Vec<Hop>,
}

impl Reason {
    pub fn widening_site(&self) -> Option<usize> {
        match self.hops.first()?.link {
            Link::UnresolvedSite(site_index) => Some(site_index),
            Link::Edge { .. }
            | Link::Uses(_)
            | Link::Loads { .. }
            | Link::ChangedSymbol(_)
            | Link::ChangedPart(_)
            | Link::SetAsideSite(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct HubCrossing {
    pub fanout: usize,
    pub hub: Option<FileId>,
}

impl HubCrossing {
    fn leaving(self, file: FileId, fanout: usize) -> Self {
        if fanout > self.fanout {
            Self {
                fanout,
                hub: Some(file),
            }
        } else {
            self
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedTest {
    pub file: FileId,
    pub reason: Reason,
    distance: usize,
    hub_crossing: HubCrossing,
}

impl SelectedTest {
    pub fn distance(&self) -> usize {
        self.distance
    }

    pub fn hub_crossing(&self) -> HubCrossing {
        self.hub_crossing
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Node {
    Symbol(SymbolId),
    Module(FileId),
    File(FileId),
    Content(FileId),
}

#[derive(Clone, Copy)]
enum Via {
    Origin,
    Seed(Link),
    Step { from: Node, link: Option<Link> },
}

#[derive(Clone, Copy)]
struct Step {
    node: Node,
    link: Option<Link>,
    content: ContentChange,
}

#[derive(Clone, Copy)]
struct Traversal<'g> {
    graph: &'g Graph,
    relaxation: Option<Relaxation>,
}

impl<'g> Traversal<'g> {
    fn is_relaxed(self, relaxation: Relaxation) -> bool {
        self.relaxation == Some(relaxation)
    }

    fn follows(self, kind: EdgeKind, content: ContentChange) -> bool {
        let selects = kind.selects_tests()
            || kind == EdgeKind::TypeOnly && self.is_relaxed(Relaxation::TypeOnly);

        selects && (kind.follows(content) || self.is_relaxed(Relaxation::Content))
    }

    fn is_analysed(self, file: FileId) -> bool {
        !self.is_relaxed(Relaxation::FileLevel) && !self.graph.symbols_of(file).is_empty()
    }

    fn module_changes(self, file: FileId) -> Option<&'g [usize]> {
        if self.is_relaxed(Relaxation::FileLevel) {
            return None;
        }

        self.graph.module_changes(file)
    }

    fn file_of(self, node: Node) -> FileId {
        match node {
            Node::Symbol(symbol) => self.graph.symbol(symbol).file,
            Node::Module(file) | Node::File(file) | Node::Content(file) => file,
        }
    }

    fn cost(self, from: Option<Node>, to: Node, link: Option<Link>) -> usize {
        match link {
            None | Some(Link::ChangedSymbol(_)) => 0,
            Some(Link::Uses(_)) => {
                usize::from(from.map(|from| self.file_of(from)) != Some(self.file_of(to)))
            }
            Some(_) => 1,
        }
    }

    fn carries(self, content: ContentChange, link: Option<Link>) -> bool {
        match link {
            Some(Link::Edge { kind, .. }) => self.follows(kind, content),
            _ => true,
        }
    }

    fn dependents(self, file: FileId) -> impl Iterator<Item = (Edge, bool)> + 'g {
        let ignored = self
            .is_relaxed(Relaxation::Ignores)
            .then(|| self.graph.ignored_dependents(file))
            .into_iter()
            .flatten()
            .map(|ignored_edge| (ignored_edge.edge, true));

        self.graph
            .dependents(file)
            .map(|edge| (edge, false))
            .chain(ignored)
    }

    fn site_can_load(self, site: &UnresolvedSite, path: &RepoPath) -> bool {
        if self.is_relaxed(Relaxation::TestSupport) {
            return self.graph.can_load(site.language, path);
        }

        self.graph.site_can_load(site, path)
    }

    fn widening_sites(self) -> impl Iterator<Item = (Link, &'g UnresolvedSite)> {
        let set_aside = if self.is_relaxed(Relaxation::Ignores) {
            self.graph.set_aside_sites()
        } else {
            &[]
        };

        self.graph
            .unresolved_sites()
            .iter()
            .enumerate()
            .map(|(site_index, site)| (Link::UnresolvedSite(site_index), site))
            .chain(
                set_aside
                    .iter()
                    .enumerate()
                    .map(|(site_index, set_aside_site)| {
                        (Link::SetAsideSite(site_index), &set_aside_site.site)
                    }),
            )
    }

    fn successors(self, node: Node, content: ContentChange, steps: &mut Vec<Step>) {
        match node {
            Node::Symbol(symbol) => {
                let users = self
                    .graph
                    .symbol_dependents(symbol)
                    .filter(|edge| edge.kind.selects_tests())
                    .map(|edge| Step {
                        node: Node::Symbol(edge.dependent),
                        link: Some(Link::Uses(*edge)),
                        content: ContentChange::None,
                    });

                steps.extend(users);
                steps.push(Step {
                    node: Node::Module(self.graph.symbol(symbol).file),
                    link: None,
                    content: ContentChange::None,
                });
            }
            Node::Module(file) => self.module_successors(file, content, steps),
            Node::File(file) => {
                if self.is_analysed(file) {
                    steps.extend(self.graph.symbols_of(file).iter().map(|&symbol| Step {
                        node: Node::Symbol(symbol),
                        link: None,
                        content: ContentChange::None,
                    }));
                }

                steps.push(Step {
                    node: Node::Module(file),
                    link: None,
                    content,
                });
            }
            Node::Content(file) => {
                let readers = self.dependents(file).filter(|(edge, _)| {
                    matches!(edge.kind, EdgeKind::Reads | EdgeKind::Lists)
                        && self.follows(edge.kind, content)
                });

                steps.extend(readers.map(|(edge, _)| file_step(file, edge)));
            }
        }
    }

    fn module_successors(self, file: FileId, content: ContentChange, steps: &mut Vec<Step>) {
        if !self.is_relaxed(Relaxation::FileLevel) {
            let whole_module_users = self
                .graph
                .module_dependents(file)
                .filter(|edge| edge.kind.selects_tests())
                .map(|edge| Step {
                    node: Node::Symbol(edge.dependent),
                    link: Some(Link::Uses(*edge)),
                    content: ContentChange::None,
                });

            steps.extend(whole_module_users);
        }

        for (edge, ignored) in self.dependents(file) {
            if !self.follows(edge.kind, content) {
                continue;
            }

            let in_code = !ignored && self.is_analysed(edge.dependent);

            match edge.kind {
                EdgeKind::Import | EdgeKind::Asset if in_code => {}
                EdgeKind::Dynamic if in_code => {
                    let Some(inference) = self.graph.inference(&edge) else {
                        continue;
                    };

                    for symbol in self.graph.symbols_at(edge.dependent, inference.line) {
                        steps.push(Step {
                            node: Node::Symbol(symbol),
                            link: Some(Link::Loads {
                                dependency: file,
                                dependent: symbol,
                            }),
                            content: ContentChange::None,
                        });
                    }
                }
                _ => steps.push(file_step(file, edge)),
            }
        }
    }

    fn predecessors(self, node: Node, steps: &mut Vec<(Node, Option<Link>)>) {
        match node {
            Node::Symbol(symbol) => {
                let file = self.graph.symbol(symbol).file;
                let used = self
                    .graph
                    .symbol_dependencies(symbol)
                    .iter()
                    .filter(|edge| edge.kind.selects_tests())
                    .map(|edge| {
                        let dependency = match edge.dependency {
                            SymbolTarget::Symbol(dependency) => Node::Symbol(dependency),
                            SymbolTarget::Module { file, .. } => Node::Module(file),
                        };

                        (dependency, Some(Link::Uses(*edge)))
                    });

                steps.extend(used);

                for edge in self
                    .graph
                    .dependencies(file)
                    .filter(|edge| edge.kind == EdgeKind::Dynamic)
                {
                    let loads_here = self.graph.inference(&edge).is_some_and(|inference| {
                        self.graph
                            .symbols_at(file, inference.line)
                            .contains(&symbol)
                    });

                    if loads_here {
                        let link = Link::Loads {
                            dependency: edge.dependency,
                            dependent: symbol,
                        };

                        steps.push((Node::Module(edge.dependency), Some(link)));
                    }
                }

                steps.push((Node::File(file), None));
            }
            Node::Module(file) => {
                if self.is_analysed(file) {
                    steps.extend(
                        self.graph
                            .symbols_of(file)
                            .iter()
                            .map(|&symbol| (Node::Symbol(symbol), None)),
                    );
                }

                steps.push((Node::File(file), None));
            }
            Node::File(file) => {
                let in_code = self.is_analysed(file);

                for edge in self.graph.dependencies(file) {
                    if in_code
                        && matches!(
                            edge.kind,
                            EdgeKind::Import | EdgeKind::Asset | EdgeKind::Dynamic
                        )
                    {
                        continue;
                    }

                    let link = Some(Link::Edge {
                        kind: edge.kind,
                        dependency: edge.dependency,
                    });

                    steps.push((Node::Module(edge.dependency), link));

                    if matches!(edge.kind, EdgeKind::Reads | EdgeKind::Lists) {
                        steps.push((Node::Content(edge.dependency), link));
                    }
                }
            }
            Node::Content(_) => {}
        }
    }
}

fn file_step(dependency: FileId, edge: Edge) -> Step {
    Step {
        node: Node::File(edge.dependent),
        link: Some(Link::Edge {
            kind: edge.kind,
            dependency,
        }),
        content: edge.kind.dependent_content(),
    }
}

#[derive(Clone, Copy)]
struct Discovery<'a> {
    change: &'a FileChange,
    via: Via,
    content: ContentChange,
    distance: usize,
    widened: bool,
}

impl Discovery<'_> {
    fn improves_on(&self, earlier: &Discovery) -> bool {
        self.content > earlier.content
            || self.content == earlier.content
                && self.widened == earlier.widened
                && self.distance < earlier.distance
    }
}

#[derive(Default)]
struct Discoveries<'a> {
    nodes: HashMap<Node, Discovery<'a>>,
    origins: HashMap<Node, Discovery<'a>>,
    queue: VecDeque<(Node, usize)>,
}

impl<'a> Discoveries<'a> {
    fn discover_origin(&mut self, node: Node, discovery: Discovery<'a>) {
        self.origins.entry(node).or_insert(discovery);
        self.discover(node, discovery, false);
    }

    fn discover(&mut self, node: Node, discovery: Discovery<'a>, at_front: bool) {
        match self.nodes.entry(node) {
            Entry::Vacant(entry) => {
                entry.insert(discovery);
            }
            Entry::Occupied(mut entry) if discovery.improves_on(entry.get()) => {
                entry.insert(discovery);
            }
            Entry::Occupied(_) => return,
        }

        if at_front {
            self.queue.push_front((node, discovery.distance));
        } else {
            self.queue.push_back((node, discovery.distance));
        }
    }

    fn visit(&mut self, traversal: Traversal) {
        let mut steps = Vec::new();

        while let Some((node, distance)) = self.queue.pop_front() {
            let discovery = self.nodes[&node];

            if discovery.distance != distance {
                continue;
            }

            steps.clear();
            traversal.successors(node, discovery.content, &mut steps);

            for step in &steps {
                let cost = traversal.cost(Some(node), step.node, step.link);
                let reached = Discovery {
                    change: discovery.change,
                    via: Via::Step {
                        from: node,
                        link: step.link,
                    },
                    content: step.content,
                    distance: distance + cost,
                    widened: discovery.widened,
                };

                self.discover(step.node, reached, cost == 0);
            }
        }
    }
}

pub fn select_tests(
    graph: &Graph,
    changes: &[FileChange],
    widening: Widening,
) -> Vec<SelectedTest> {
    select_relaxed(graph, changes, widening, None)
}

pub fn select_relaxed(
    graph: &Graph,
    changes: &[FileChange],
    widening: Widening,
    relaxation: Option<Relaxation>,
) -> Vec<SelectedTest> {
    let traversal = Traversal { graph, relaxation };
    let (asset_changes, code_changes): (Vec<FileChange>, Vec<FileChange>) =
        changes.iter().cloned().partition(|change| {
            !traversal.is_relaxed(Relaxation::Assets) && is_asset_change(graph, change)
        });
    let code_discoveries = discover_dependents(traversal, &code_changes, widening);
    let asset_discoveries = discover_dependents(traversal, &asset_changes, Widening::Off);
    let mut selected_tests = tests_reached(traversal, &code_discoveries, |_| true);
    let selected_files: HashSet<FileId> = selected_tests
        .iter()
        .map(|selected_test| selected_test.file)
        .collect();
    let asset_tests = tests_reached(traversal, &asset_discoveries, |test| {
        !graph.is_asset_blind(test) && !selected_files.contains(&test)
    });

    selected_tests.extend(asset_tests);
    selected_tests.sort_by(|left, right| {
        let left_path = graph.path(left.file);
        let right_path = graph.path(right.file);

        left.distance
            .cmp(&right.distance)
            .then(left_path.cmp(right_path))
    });

    selected_tests
}

fn is_asset_change(graph: &Graph, change: &FileChange) -> bool {
    graph
        .file_id(&change.path)
        .is_some_and(|changed_file| graph.is_asset(changed_file))
}

fn discover_dependents<'a>(
    traversal: Traversal,
    changes: &'a [FileChange],
    widening: Widening,
) -> Discoveries<'a> {
    let graph = traversal.graph;
    let mut discoveries = Discoveries::default();
    let mut changes_by_path: Vec<&FileChange> = changes.iter().collect();

    changes_by_path.sort_by(|left, right| left.path.cmp(&right.path));

    for &change in &changes_by_path {
        let Some(changed_file) = graph.file_id(&change.path) else {
            continue;
        };
        let origin = Discovery {
            change,
            via: Via::Origin,
            content: ContentChange::Direct,
            distance: 0,
            widened: false,
        };

        match traversal.module_changes(changed_file) {
            None => discoveries.discover_origin(Node::File(changed_file), origin),
            Some([]) => discoveries.discover_origin(Node::Content(changed_file), origin),
            Some(symbol_changes) => {
                discoveries.discover_origin(Node::Module(changed_file), origin);

                for &change_index in symbol_changes {
                    let seed = Discovery {
                        via: Via::Seed(Link::ChangedSymbol(change_index)),
                        content: ContentChange::None,
                        ..origin
                    };

                    for &target in &graph.symbol_changes()[change_index].reaches {
                        let node = match target {
                            SymbolTarget::Symbol(symbol) => Node::Symbol(symbol),
                            SymbolTarget::Module { file, .. } => Node::Module(file),
                        };

                        discoveries.discover_origin(node, seed);
                    }
                }
            }
        }
    }

    for (part_index, changed_part) in graph.changed_parts().iter().enumerate() {
        let Some(&change) = changes_by_path
            .iter()
            .find(|change| change.path == changed_part.change_path)
        else {
            continue;
        };

        discoveries.discover_origin(
            Node::File(changed_part.node),
            Discovery {
                change,
                via: Via::Seed(Link::ChangedPart(part_index)),
                content: ContentChange::None,
                distance: 1,
                widened: false,
            },
        );
    }

    discoveries.visit(traversal);

    if widening == Widening::Off {
        return discoveries;
    }

    let widening_sites = traversal
        .widening_sites()
        .filter(|(_, unresolved_site)| unresolved_site.cause.widens());

    for (site_link, unresolved_site) in widening_sites {
        let loadable_change = changes_by_path.iter().find(|change| {
            change
                .touched_paths()
                .any(|path| traversal.site_can_load(unresolved_site, path))
        });
        let Some(&change) = loadable_change else {
            continue;
        };
        let seed = Discovery {
            change,
            via: Via::Seed(site_link),
            content: ContentChange::None,
            distance: 1,
            widened: true,
        };

        if traversal.is_analysed(unresolved_site.file) {
            for symbol in graph.symbols_at(unresolved_site.file, unresolved_site.line) {
                discoveries.discover_origin(Node::Symbol(symbol), seed);
            }
        } else {
            discoveries.discover_origin(Node::File(unresolved_site.file), seed);
        }
    }

    discoveries.visit(traversal);

    discoveries
}

fn tests_reached(
    traversal: Traversal,
    discoveries: &Discoveries,
    keeps: impl Fn(FileId) -> bool,
) -> Vec<SelectedTest> {
    let graph = traversal.graph;
    let hub_crossings = HubSearch::new(traversal).run(discoveries);
    let mut nodes_by_test: HashMap<FileId, Vec<Node>> = HashMap::new();

    for (&node, discovery) in &discoveries.nodes {
        let file = traversal.file_of(node);
        let runs_test = match node {
            Node::Content(_) => false,
            Node::File(_) => true,
            Node::Symbol(symbol) => {
                !traversal.is_analysed(file) || graph.symbol(symbol).kind == SymbolKind::ModuleInit
            }
            Node::Module(_) => !traversal.is_analysed(file) || matches!(discovery.via, Via::Origin),
        };

        if runs_test && graph.is_test(file) && keeps(file) {
            nodes_by_test.entry(file).or_default().push(node);
        }
    }

    nodes_by_test
        .into_iter()
        .filter_map(|(test_file, mut nodes)| {
            nodes.sort_by_key(|node| {
                let discovery = &discoveries.nodes[node];

                (discovery.widened, discovery.distance, *node)
            });

            let (reason, distance) = reason_past_mocks(traversal, discoveries, test_file, &nodes)?;
            let hub_crossing = nodes
                .iter()
                .filter_map(|node| hub_crossings.get(node))
                .min()
                .copied()
                .unwrap_or_default();

            Some(SelectedTest {
                file: test_file,
                reason,
                distance,
                hub_crossing,
            })
        })
        .collect()
}

struct HubSearch<'g> {
    traversal: Traversal<'g>,
    fanouts: HashMap<FileId, usize>,
    reached: HashMap<Node, (ContentChange, HubCrossing)>,
    entries: HashMap<Node, HubCrossing>,
    queue: BinaryHeap<Reverse<(HubCrossing, Node)>>,
}

impl<'g> HubSearch<'g> {
    fn new(traversal: Traversal<'g>) -> Self {
        Self {
            traversal,
            fanouts: HashMap::new(),
            reached: HashMap::new(),
            entries: HashMap::new(),
            queue: BinaryHeap::new(),
        }
    }

    fn run(mut self, discoveries: &Discoveries) -> HashMap<Node, HubCrossing> {
        let graph = self.traversal.graph;
        let mut steps = Vec::new();

        for (&node, origin) in &discoveries.origins {
            self.reach(node, origin.content, HubCrossing::default());

            if graph.is_test(self.traversal.file_of(node)) {
                self.enter(node, HubCrossing::default());
            }
        }

        while let Some(Reverse((crossing, node))) = self.queue.pop() {
            let (content, best_crossing) = self.reached[&node];

            if best_crossing != crossing {
                continue;
            }

            let file = self.traversal.file_of(node);

            steps.clear();
            self.traversal.successors(node, content, &mut steps);

            for step in &steps {
                let next_file = self.traversal.file_of(step.node);

                if next_file == file {
                    self.reach(step.node, step.content, crossing);

                    continue;
                }

                if graph.is_test(next_file) {
                    self.enter(step.node, crossing);
                }

                let fanout = self.fanout(file);

                self.reach(step.node, step.content, crossing.leaving(file, fanout));
            }
        }

        self.spread_in_test_files()
    }

    fn reach(&mut self, node: Node, content: ContentChange, crossing: HubCrossing) {
        let improves = match self.reached.get(&node) {
            None => true,
            Some(&(earlier_content, earlier_crossing)) => {
                content > earlier_content
                    || content == earlier_content && crossing < earlier_crossing
            }
        };

        if improves {
            self.reached.insert(node, (content, crossing));
            self.queue.push(Reverse((crossing, node)));
        }
    }

    fn enter(&mut self, node: Node, crossing: HubCrossing) {
        self.entries
            .entry(node)
            .and_modify(|earlier| *earlier = (*earlier).min(crossing))
            .or_insert(crossing);
    }

    fn fanout(&mut self, file: FileId) -> usize {
        let graph = self.traversal.graph;

        *self.fanouts.entry(file).or_insert_with(|| {
            let mut dependents: Vec<FileId> = graph
                .dependents(file)
                .filter(|edge| edge.kind.selects_tests())
                .map(|edge| edge.dependent)
                .collect();

            dependents.sort_unstable();
            dependents.dedup();

            dependents.len()
        })
    }

    fn spread_in_test_files(self) -> HashMap<Node, HubCrossing> {
        let mut crossings = self.entries;
        let mut queue: BinaryHeap<Reverse<(HubCrossing, Node)>> = crossings
            .iter()
            .map(|(&node, &crossing)| Reverse((crossing, node)))
            .collect();
        let mut steps = Vec::new();

        while let Some(Reverse((crossing, node))) = queue.pop() {
            if crossings[&node] != crossing {
                continue;
            }

            let file = self.traversal.file_of(node);
            let content = self
                .reached
                .get(&node)
                .map_or(ContentChange::None, |&(content, _)| content);

            steps.clear();
            self.traversal.successors(node, content, &mut steps);

            for step in &steps {
                if self.traversal.file_of(step.node) != file {
                    continue;
                }

                let improves = crossings
                    .get(&step.node)
                    .is_none_or(|&earlier| crossing < earlier);

                if improves {
                    crossings.insert(step.node, crossing);
                    queue.push(Reverse((crossing, step.node)));
                }
            }
        }

        crossings
    }
}

#[derive(Clone, Copy)]
struct Mocks<'g> {
    traversal: Traversal<'g>,
    mocked_modules: &'g [FileId],
}

impl Mocks<'_> {
    fn replace(self, file: FileId) -> bool {
        !self.traversal.is_relaxed(Relaxation::Mocks)
            && self.mocked_modules.binary_search(&file).is_ok()
            && !self.traversal.graph.has_changed_contract(file)
    }

    fn cut(self, node: Node, link: Option<Link>) -> bool {
        let through = match link {
            Some(Link::Uses(edge)) => self.traversal.graph.symbol_edge_through(&edge),
            _ => &[],
        };

        self.replace(self.traversal.file_of(node)) || through.iter().any(|&file| self.replace(file))
    }
}

fn reason_past_mocks(
    traversal: Traversal,
    discoveries: &Discoveries,
    test_file: FileId,
    test_nodes: &[Node],
) -> Option<(Reason, usize)> {
    let mocks = Mocks {
        traversal,
        mocked_modules: traversal.graph.mocked_modules(test_file),
    };

    if let Some(&node) = test_nodes
        .iter()
        .find(|&&node| trace_avoids(node, &discoveries.nodes, mocks))
    {
        return Some(trace_reason(traversal, node, &discoveries.nodes));
    }

    let mut dependents_on_path: HashMap<Node, (Node, Option<Link>)> = HashMap::new();

    if let Some((&node, origin)) = test_nodes
        .iter()
        .find_map(|node| Some((node, discoveries.origins.get(node)?)))
    {
        return Some(reason_from_origin(
            traversal,
            node,
            origin,
            &dependents_on_path,
        ));
    }

    let mut nodes_to_visit: VecDeque<Node> = test_nodes.iter().copied().collect();
    let mut visited: HashSet<Node> = test_nodes.iter().copied().collect();
    let mut steps = Vec::new();

    while let Some(node) = nodes_to_visit.pop_front() {
        steps.clear();
        traversal.predecessors(node, &mut steps);

        for &(dependency, link) in &steps {
            let carries_the_change = discoveries
                .nodes
                .get(&dependency)
                .is_some_and(|discovery| traversal.carries(discovery.content, link));

            if !carries_the_change || mocks.cut(dependency, link) || !visited.insert(dependency) {
                continue;
            }

            dependents_on_path.insert(dependency, (node, link));

            if trace_avoids(dependency, &discoveries.nodes, mocks) {
                let (reason, distance) = trace_reason(traversal, dependency, &discoveries.nodes);

                return Some(extended_to_the_test(
                    traversal,
                    reason,
                    distance,
                    dependency,
                    &dependents_on_path,
                ));
            }

            if let Some(origin) = discoveries
                .origins
                .get(&dependency)
                .filter(|origin| traversal.carries(origin.content, link))
            {
                return Some(reason_from_origin(
                    traversal,
                    dependency,
                    origin,
                    &dependents_on_path,
                ));
            }

            nodes_to_visit.push_back(dependency);
        }
    }

    None
}

fn trace_avoids(node: Node, discoveries: &HashMap<Node, Discovery>, mocks: Mocks) -> bool {
    let mut node = node;

    loop {
        match discoveries[&node].via {
            Via::Step { from, link } if !mocks.cut(node, link) => node = from,
            Via::Step { .. } => return false,
            Via::Origin | Via::Seed(_) => return !mocks.cut(node, None),
        }
    }
}

fn trace_reason(
    traversal: Traversal,
    reached: Node,
    discoveries: &HashMap<Node, Discovery>,
) -> (Reason, usize) {
    let mut hops = Vec::new();
    let mut distance = 0;
    let mut node = reached;

    loop {
        match discoveries[&node].via {
            Via::Origin => break,
            Via::Seed(link) => {
                hops.push(Hop {
                    dependent: traversal.file_of(node),
                    link,
                });
                distance += traversal.cost(None, node, Some(link));

                break;
            }
            Via::Step { from, link } => {
                if let Some(link) = link {
                    hops.push(Hop {
                        dependent: traversal.file_of(node),
                        link,
                    });
                }

                distance += traversal.cost(Some(from), node, link);
                node = from;
            }
        }
    }

    hops.reverse();

    let reason = Reason {
        change: discoveries[&reached].change.clone(),
        hops,
    };

    (reason, distance)
}

fn reason_from_origin(
    traversal: Traversal,
    node: Node,
    origin: &Discovery,
    dependents_on_path: &HashMap<Node, (Node, Option<Link>)>,
) -> (Reason, usize) {
    let mut reason = Reason {
        change: origin.change.clone(),
        hops: Vec::new(),
    };
    let mut distance = 0;

    if let Via::Seed(link) = origin.via {
        reason.hops.push(Hop {
            dependent: traversal.file_of(node),
            link,
        });
        distance += traversal.cost(None, node, Some(link));
    }

    extended_to_the_test(traversal, reason, distance, node, dependents_on_path)
}

fn extended_to_the_test(
    traversal: Traversal,
    mut reason: Reason,
    mut distance: usize,
    node: Node,
    dependents_on_path: &HashMap<Node, (Node, Option<Link>)>,
) -> (Reason, usize) {
    let mut dependency = node;

    while let Some(&(dependent, link)) = dependents_on_path.get(&dependency) {
        if let Some(link) = link {
            reason.hops.push(Hop {
                dependent: traversal.file_of(dependent),
                link,
            });
        }

        distance += traversal.cost(Some(dependency), dependent, link);
        dependency = dependent;
    }

    (reason, distance)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::ChangeKind;
    use crate::files::RepoPath;
    use crate::graph::EdgeKind::{Asset, Family, Import, Lists, Reads, TypeOnly};
    use crate::graph::{Edge, GraphBuilder, UnresolvedCause, UnresolvedSite};
    use crate::report::describe_reason;
    use crate::symbols::{
        LineRange, MODULE_INIT_NAME, Symbol, SymbolChange, SymbolChangeKind, SymbolKind,
        WholeModuleCause,
    };

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn build_graph(test_files: &[&str], edges: &[(&str, EdgeKind, &str)]) -> Graph {
        let mut builder = GraphBuilder::default();

        for test_file in test_files {
            builder.add_test_file(repo_path(test_file), "typescript");
        }

        for &(dependent_path, kind, dependency_path) in edges {
            let dependent = builder.add_file(repo_path(dependent_path));
            let dependency = builder.add_file(repo_path(dependency_path));

            builder.add_edge(Edge {
                dependent,
                dependency,
                kind,
            });
        }

        builder.build()
    }

    fn graph_with_unresolved_sites(
        test_files: &[&str],
        edges: &[(&str, EdgeKind, &str)],
        unresolved_sites: &[(&str, u32)],
    ) -> Graph {
        let mut builder = GraphBuilder::default();

        builder.add_language("typescript", |path| {
            path.as_str().ends_with(".ts") && !path.as_str().ends_with(".test.ts")
        });
        builder.add_test_support("typescript", |path| {
            path.as_str().contains(".test-support.")
        });

        for test_file in test_files {
            builder.add_test_file(repo_path(test_file), "typescript");
        }

        for &(dependent_path, kind, dependency_path) in edges {
            let dependent = builder.add_file(repo_path(dependent_path));
            let dependency = builder.add_file(repo_path(dependency_path));

            builder.add_edge(Edge {
                dependent,
                dependency,
                kind,
            });
        }

        for &(site_path, line) in unresolved_sites {
            let file = builder.add_file(repo_path(site_path));

            builder.add_unresolved_site(UnresolvedSite {
                file,
                line,
                expression: "request.moduleUrl".to_owned(),
                cause: UnresolvedCause::Parameter,
                language: "typescript",
            });
        }

        builder.build()
    }

    fn widening_sites(graph: &Graph, changes: &[FileChange]) -> Vec<(String, Option<usize>)> {
        select_tests(graph, changes, Widening::Scoped)
            .iter()
            .map(|selected_test| {
                (
                    graph.path(selected_test.file).as_str().to_owned(),
                    selected_test.reason.widening_site(),
                )
            })
            .collect()
    }

    fn modified(path: &str) -> FileChange {
        FileChange {
            path: repo_path(path),
            kind: ChangeKind::Modified,
        }
    }

    fn selected_with_distances<'g>(
        graph: &'g Graph,
        changes: &[FileChange],
    ) -> Vec<(&'g str, usize)> {
        select_tests(graph, changes, Widening::Scoped)
            .iter()
            .map(|selected_test| {
                (
                    graph.path(selected_test.file).as_str(),
                    selected_test.distance(),
                )
            })
            .collect()
    }

    fn reason_files<'g>(graph: &'g Graph, selected_test: &'g SelectedTest) -> Vec<&'g str> {
        let dependents = selected_test
            .reason
            .hops
            .iter()
            .map(|hop| graph.path(hop.dependent).as_str());

        std::iter::once(selected_test.reason.change.path.as_str())
            .chain(dependents)
            .collect()
    }

    #[test]
    fn changed_file_selects_the_tests_importing_it() {
        let graph = build_graph(
            &["src/a.test.ts", "src/unrelated.test.ts"],
            &[
                ("src/a.test.ts", Import, "src/a.ts"),
                ("src/unrelated.test.ts", Import, "src/unrelated.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/a.ts")]),
            [("src/a.test.ts", 1)]
        );
    }

    #[test]
    fn transitive_dependents_are_selected_with_their_distance() {
        let graph = build_graph(
            &["src/api.test.ts"],
            &[
                ("src/store.ts", Import, "src/db.ts"),
                ("src/api.ts", Import, "src/store.ts"),
                ("src/api.test.ts", Import, "src/api.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/db.ts")]),
            [("src/api.test.ts", 3)]
        );
    }

    #[test]
    fn reason_lists_every_file_from_the_change_to_the_test() {
        let graph = build_graph(
            &["src/api.test.ts"],
            &[
                ("src/store.ts", Import, "src/db.ts"),
                ("src/api.ts", Import, "src/store.ts"),
                ("src/api.test.ts", Import, "src/api.ts"),
            ],
        );
        let selected_tests = select_tests(&graph, &[modified("src/db.ts")], Widening::Scoped);

        assert_eq!(selected_tests[0].reason.change, modified("src/db.ts"));
        assert_eq!(
            reason_files(&graph, &selected_tests[0]),
            ["src/db.ts", "src/store.ts", "src/api.ts", "src/api.test.ts"]
        );
    }

    #[test]
    fn distance_is_the_shortest_path_to_a_change() {
        let graph = build_graph(
            &["src/api.test.ts"],
            &[
                ("src/store.ts", Import, "src/db.ts"),
                ("src/api.ts", Import, "src/store.ts"),
                ("src/api.test.ts", Import, "src/api.ts"),
                ("src/api.test.ts", Import, "src/db.ts"),
            ],
        );
        let selected_tests = select_tests(&graph, &[modified("src/db.ts")], Widening::Scoped);

        assert_eq!(
            reason_files(&graph, &selected_tests[0]),
            ["src/db.ts", "src/api.test.ts"]
        );
    }

    #[test]
    fn type_only_import_never_selects_a_test() {
        let graph = build_graph(
            &["src/a.test.ts", "src/b.test.ts"],
            &[
                ("src/a.test.ts", TypeOnly, "src/types.ts"),
                ("src/b.ts", TypeOnly, "src/types.ts"),
                ("src/b.test.ts", Import, "src/b.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/types.ts")]),
            []
        );
    }

    #[test]
    fn test_with_both_type_only_and_runtime_paths_is_selected_through_the_runtime_path() {
        let graph = build_graph(
            &["src/a.test.ts"],
            &[
                ("src/a.test.ts", TypeOnly, "src/model.ts"),
                ("src/a.test.ts", Import, "src/a.ts"),
                ("src/a.ts", Import, "src/model.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/model.ts")]),
            [("src/a.test.ts", 2)]
        );
    }

    #[test]
    fn changed_test_file_is_always_selected_at_distance_zero() {
        let graph = build_graph(&["src/standalone.test.ts"], &[]);
        let added_test = FileChange {
            path: repo_path("src/standalone.test.ts"),
            kind: ChangeKind::Added,
        };

        assert_eq!(
            selected_with_distances(&graph, &[added_test]),
            [("src/standalone.test.ts", 0)]
        );
    }

    #[test]
    fn renamed_file_is_traced_from_its_new_path() {
        let graph = build_graph(
            &["src/client.test.ts"],
            &[("src/client.test.ts", Import, "src/http/client.ts")],
        );
        let renamed = FileChange {
            path: repo_path("src/http/client.ts"),
            kind: ChangeKind::Renamed {
                from: repo_path("src/client.ts"),
            },
        };
        let selected_tests = select_tests(&graph, std::slice::from_ref(&renamed), Widening::Scoped);

        assert_eq!(selected_tests.len(), 1);
        assert_eq!(selected_tests[0].reason.change, renamed);
    }

    #[test]
    fn changes_outside_the_graph_select_nothing() {
        let graph = build_graph(&["src/a.test.ts"], &[("src/a.test.ts", Import, "src/a.ts")]);
        let deleted = FileChange {
            path: repo_path("src/removed.ts"),
            kind: ChangeKind::Deleted,
        };

        assert_eq!(
            selected_with_distances(&graph, &[modified("README.md"), deleted]),
            []
        );
    }

    #[test]
    fn tests_are_ordered_by_distance_then_path() {
        let graph = build_graph(
            &[
                "src/z.test.ts",
                "src/b.test.ts",
                "src/a.test.ts",
                "src/far.test.ts",
            ],
            &[
                ("src/z.test.ts", Import, "src/core.ts"),
                ("src/b.test.ts", Import, "src/core.ts"),
                ("src/feature.ts", Import, "src/core.ts"),
                ("src/a.test.ts", Import, "src/feature.ts"),
                ("src/page.ts", Import, "src/feature.ts"),
                ("src/far.test.ts", Import, "src/page.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/core.ts")]),
            [
                ("src/b.test.ts", 1),
                ("src/z.test.ts", 1),
                ("src/a.test.ts", 2),
                ("src/far.test.ts", 3),
            ]
        );
    }

    #[test]
    fn import_cycles_are_traversed_once() {
        let graph = build_graph(
            &["src/a.test.ts"],
            &[
                ("src/a.ts", Import, "src/b.ts"),
                ("src/b.ts", Import, "src/a.ts"),
                ("src/a.test.ts", Import, "src/a.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/b.ts")]),
            [("src/a.test.ts", 2)]
        );
    }

    #[test]
    fn traversal_continues_through_test_files() {
        let graph = build_graph(
            &["src/base.test.ts", "src/extended.test.ts"],
            &[
                ("src/base.test.ts", Import, "src/a.ts"),
                ("src/extended.test.ts", Import, "src/base.test.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/a.ts")]),
            [("src/base.test.ts", 1), ("src/extended.test.ts", 2)]
        );
    }

    #[test]
    fn module_change_widens_to_the_dependents_of_each_unresolved_site() {
        let graph = graph_with_unresolved_sites(
            &["src/worker.test.ts", "src/pool.test.ts", "src/a.test.ts"],
            &[
                ("src/worker.test.ts", Import, "src/worker.ts"),
                ("src/pool.ts", Import, "src/worker.ts"),
                ("src/pool.test.ts", Import, "src/pool.ts"),
                ("src/a.test.ts", Import, "src/a.ts"),
            ],
            &[("src/worker.ts", 313)],
        );
        let selected_tests = select_tests(&graph, &[modified("src/a.ts")], Widening::Scoped);

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/a.ts")]),
            [
                ("src/a.test.ts", 1),
                ("src/worker.test.ts", 2),
                ("src/pool.test.ts", 3),
            ]
        );
        assert_eq!(
            selected_tests[1].reason.hops[0],
            Hop {
                dependent: graph.file_id(&repo_path("src/worker.ts")).unwrap(),
                link: Link::UnresolvedSite(0),
            }
        );
        assert_eq!(
            reason_files(&graph, &selected_tests[2]),
            [
                "src/a.ts",
                "src/worker.ts",
                "src/pool.ts",
                "src/pool.test.ts"
            ]
        );
    }

    #[test]
    fn changes_the_language_cannot_load_never_widen() {
        let graph = graph_with_unresolved_sites(
            &["src/worker.test.ts", "src/a.test.ts"],
            &[("src/worker.test.ts", Import, "src/worker.ts")],
            &[("src/worker.ts", 313)],
        );
        let docs_and_tests = [modified("docs/guide.md"), modified("src/a.test.ts")];

        assert_eq!(
            widening_sites(&graph, &docs_and_tests),
            [("src/a.test.ts".to_owned(), None)]
        );
    }

    #[test]
    fn deleted_and_renamed_modules_widen_too() {
        let graph = graph_with_unresolved_sites(
            &["src/worker.test.ts"],
            &[("src/worker.test.ts", Import, "src/worker.ts")],
            &[("src/worker.ts", 313)],
        );
        let deleted = FileChange {
            path: repo_path("src/removed.ts"),
            kind: ChangeKind::Deleted,
        };
        let renamed_to_markdown = FileChange {
            path: repo_path("docs/notes.md"),
            kind: ChangeKind::Renamed {
                from: repo_path("src/notes.ts"),
            },
        };

        assert_eq!(
            widening_sites(&graph, &[deleted]),
            [("src/worker.test.ts".to_owned(), Some(0))]
        );
        assert_eq!(
            widening_sites(&graph, &[renamed_to_markdown]),
            [("src/worker.test.ts".to_owned(), Some(0))]
        );
    }

    #[test]
    fn test_support_triggers_only_the_unresolved_sites_in_test_code() {
        let graph = graph_with_unresolved_sites(
            &[
                "src/worker.test.ts",
                "src/a.test.ts",
                "src/load.test.ts",
                "src/fresh.test.ts",
            ],
            &[
                ("src/worker.test.ts", Import, "src/worker.ts"),
                ("src/a.test.ts", Import, "src/a.test-support.ts"),
                ("src/load.test.ts", Import, "src/load.test-support.ts"),
            ],
            &[
                ("src/worker.ts", 313),
                ("src/load.test-support.ts", 4),
                ("src/fresh.test.ts", 12),
            ],
        );
        let test_support_change = [modified("src/a.test-support.ts")];
        let relaxed: Vec<(&str, Option<usize>)> = select_relaxed(
            &graph,
            &test_support_change,
            Widening::Scoped,
            Some(Relaxation::TestSupport),
        )
        .iter()
        .map(|selected_test| {
            (
                graph.path(selected_test.file).as_str(),
                selected_test.reason.widening_site(),
            )
        })
        .collect();

        assert_eq!(
            widening_sites(&graph, &test_support_change),
            [
                ("src/a.test.ts".to_owned(), None),
                ("src/fresh.test.ts".to_owned(), Some(0)),
                ("src/load.test.ts".to_owned(), Some(1)),
            ]
        );
        assert_eq!(
            widening_sites(&graph, &[modified("src/b.ts")]),
            [
                ("src/fresh.test.ts".to_owned(), Some(0)),
                ("src/load.test.ts".to_owned(), Some(1)),
                ("src/worker.test.ts".to_owned(), Some(2)),
            ]
        );
        assert_eq!(
            relaxed,
            [
                ("src/a.test.ts", None),
                ("src/fresh.test.ts", Some(0)),
                ("src/load.test.ts", Some(1)),
                ("src/worker.test.ts", Some(2)),
            ]
        );
    }

    #[test]
    fn widening_off_selects_only_through_the_graph() {
        let graph = graph_with_unresolved_sites(
            &["src/worker.test.ts", "src/a.test.ts"],
            &[
                ("src/worker.test.ts", Import, "src/worker.ts"),
                ("src/a.test.ts", Import, "src/a.ts"),
            ],
            &[("src/worker.ts", 313)],
        );
        let selected_tests = select_tests(&graph, &[modified("src/a.ts")], Widening::Off);

        assert_eq!(selected_tests.len(), 1);
        assert_eq!(graph.path(selected_tests[0].file).as_str(), "src/a.test.ts");
    }

    #[test]
    fn tests_already_selected_keep_their_ordinary_reason() {
        let graph = graph_with_unresolved_sites(
            &["src/worker.test.ts"],
            &[
                ("src/worker.ts", Import, "src/a.ts"),
                ("src/worker.test.ts", Import, "src/worker.ts"),
            ],
            &[("src/worker.ts", 313)],
        );

        assert_eq!(
            widening_sites(&graph, &[modified("src/a.ts")]),
            [("src/worker.test.ts".to_owned(), None)]
        );
    }

    #[test]
    fn unresolved_site_in_a_test_file_widens_to_that_test() {
        let graph = graph_with_unresolved_sites(
            &["src/fresh.test.ts", "src/a.test.ts"],
            &[("src/a.test.ts", Import, "src/a.ts")],
            &[("src/fresh.test.ts", 12), ("src/fresh.test.ts", 30)],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/a.ts")]),
            [("src/a.test.ts", 1), ("src/fresh.test.ts", 1)]
        );
        assert_eq!(
            widening_sites(&graph, &[modified("src/a.ts")])[1],
            ("src/fresh.test.ts".to_owned(), Some(0))
        );
    }

    #[test]
    fn changed_part_selects_through_its_node_instead_of_the_whole_file() {
        let mut builder = GraphBuilder::default();
        let lodash = builder.add_file(repo_path("node_modules/lodash"));
        let a = builder.add_file(repo_path("src/a.ts"));
        let a_test = builder.add_test_file(repo_path("src/a.test.ts"), "typescript");
        let b = builder.add_file(repo_path("src/b.ts"));
        let b_test = builder.add_test_file(repo_path("src/b.test.ts"), "typescript");
        let import_edges = [(a, lodash), (a_test, a), (b_test, b)];

        for (dependent, dependency) in import_edges {
            builder.add_edge(Edge {
                dependent,
                dependency,
                kind: Import,
            });
        }

        builder.add_changed_part(
            repo_path("pnpm-lock.yaml"),
            repo_path("node_modules/lodash"),
            "package lodash changed (4.17.20 -> 4.17.21)".to_owned(),
        );

        let graph = builder.build();
        let selected_tests = select_tests(&graph, &[modified("pnpm-lock.yaml")], Widening::Scoped);

        assert_eq!(selected_tests.len(), 1);
        assert_eq!(
            reason_files(&graph, &selected_tests[0]),
            [
                "pnpm-lock.yaml",
                "node_modules/lodash",
                "src/a.ts",
                "src/a.test.ts"
            ]
        );
        assert_eq!(
            selected_tests[0].reason.hops[0],
            Hop {
                dependent: lodash,
                link: Link::ChangedPart(0),
            }
        );
        assert_eq!(selected_tests[0].reason.widening_site(), None);
    }

    #[test]
    fn changed_part_of_a_file_outside_the_diff_selects_nothing() {
        let mut builder = GraphBuilder::default();
        let a = builder.add_file(repo_path("src/a.ts"));
        let a_test = builder.add_test_file(repo_path("src/a.test.ts"), "typescript");

        builder.add_edge(Edge {
            dependent: a_test,
            dependency: a,
            kind: Import,
        });
        builder.add_changed_part(
            repo_path("package.json"),
            repo_path("src/a.ts"),
            "src/a.ts is affected by field \"type\"".to_owned(),
        );

        let graph = builder.build();

        assert_eq!(
            selected_with_distances(&graph, &[modified("pnpm-lock.yaml")]),
            []
        );
        assert_eq!(
            selected_with_distances(&graph, &[modified("package.json")]),
            [("src/a.test.ts", 2)]
        );
    }

    fn graph_with_mocks(
        edges: &[(&str, EdgeKind, &str)],
        mocks: &[(&str, &str)],
        changed_contracts: &[&str],
    ) -> Graph {
        let mut builder = GraphBuilder::default();

        for &(dependent_path, kind, dependency_path) in edges {
            let dependent = if dependent_path.ends_with(".test.ts") {
                builder.add_test_file(repo_path(dependent_path), "typescript")
            } else {
                builder.add_file(repo_path(dependent_path))
            };
            let dependency = builder.add_file(repo_path(dependency_path));

            builder.add_edge(Edge {
                dependent,
                dependency,
                kind,
            });
        }

        for &(test, mocked_module) in mocks {
            let test = builder.add_file(repo_path(test));
            let mocked_module = builder.add_file(repo_path(mocked_module));

            builder.add_mock(test, mocked_module);
        }

        for &module in changed_contracts {
            let module = builder.add_file(repo_path(module));

            builder.mark_changed_contract(module);
        }

        builder.build()
    }

    const SERVICE_WITH_MOCKED_DB: &[(&str, EdgeKind, &str)] = &[
        ("src/db.ts", Import, "src/sql.ts"),
        ("src/service.ts", Import, "src/db.ts"),
        ("src/service.test.ts", Import, "src/service.ts"),
        ("src/service.test.ts", Import, "src/db.ts"),
        ("src/db.test.ts", Import, "src/db.ts"),
    ];

    #[test]
    fn test_cut_from_a_change_by_its_mock_is_still_selected_by_widening() {
        let mut builder = GraphBuilder::default();
        let catalog_test = builder.add_test_file(repo_path("src/catalog.test.ts"), "typescript");
        let loader = builder.add_file(repo_path("src/loader.ts"));
        let locale = builder.add_file(repo_path("src/locale.ts"));

        builder.add_language("typescript", |path| path.as_str().ends_with(".ts"));
        builder.add_edge(Edge {
            dependent: catalog_test,
            dependency: loader,
            kind: Import,
        });
        builder.add_edge(Edge {
            dependent: loader,
            dependency: locale,
            kind: Import,
        });
        builder.add_mock(catalog_test, loader);
        builder.add_unresolved_site(UnresolvedSite {
            file: catalog_test,
            line: 92,
            expression: "import(entry)".to_owned(),
            cause: UnresolvedCause::RuntimeValue,
            language: "typescript",
        });

        let graph = builder.build();
        let selected_tests = select_tests(&graph, &[modified("src/locale.ts")], Widening::Scoped);

        assert_eq!(selected_tests.len(), 1);
        assert_eq!(selected_tests[0].reason.widening_site(), Some(0));
        assert_eq!(
            select_tests(&graph, &[modified("src/locale.ts")], Widening::Off),
            []
        );
    }

    #[test]
    fn test_mocking_a_module_is_not_selected_by_changes_behind_the_mock() {
        let graph = graph_with_mocks(
            SERVICE_WITH_MOCKED_DB,
            &[("src/service.test.ts", "src/db.ts")],
            &[],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/db.ts")]),
            [("src/db.test.ts", 1)]
        );
        assert_eq!(
            selected_with_distances(&graph, &[modified("src/sql.ts")]),
            [("src/db.test.ts", 2)]
        );
        assert_eq!(
            selected_with_distances(&graph, &[modified("src/service.ts")]),
            [("src/service.test.ts", 1)]
        );
    }

    #[test]
    fn changed_export_signature_of_a_mocked_module_selects_its_mockers() {
        let graph = graph_with_mocks(
            SERVICE_WITH_MOCKED_DB,
            &[("src/service.test.ts", "src/db.ts")],
            &["src/db.ts"],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/db.ts")]),
            [("src/db.test.ts", 1), ("src/service.test.ts", 1)]
        );
        assert_eq!(
            selected_with_distances(&graph, &[modified("src/sql.ts")]),
            [("src/db.test.ts", 2), ("src/service.test.ts", 2)]
        );
    }

    #[test]
    fn path_around_the_mock_still_selects_the_test_with_that_path_as_reason() {
        let edges = [
            ("src/db.ts", Import, "src/config.ts"),
            ("src/service.ts", Import, "src/db.ts"),
            ("src/logger.ts", Import, "src/config.ts"),
            ("src/service.ts", Import, "src/logger.ts"),
            ("src/service.test.ts", Import, "src/service.ts"),
        ];
        let graph = graph_with_mocks(&edges, &[("src/service.test.ts", "src/db.ts")], &[]);
        let selected_tests = select_tests(&graph, &[modified("src/config.ts")], Widening::Scoped);

        assert_eq!(selected_tests.len(), 1);
        assert_eq!(
            reason_files(&graph, &selected_tests[0]),
            [
                "src/config.ts",
                "src/logger.ts",
                "src/service.ts",
                "src/service.test.ts"
            ]
        );
    }

    #[test]
    fn asset_change_reaches_only_tests_that_render_assets() {
        let mut builder = GraphBuilder::default();
        let styles = builder.add_file(repo_path("src/button.css"));
        let button = builder.add_file(repo_path("src/button.ts"));
        let unit_test = builder.add_test_file(repo_path("src/button.test.ts"), "typescript");
        let browser_test =
            builder.add_test_file(repo_path("src/button.browser.test.ts"), "typescript");
        let edges = [
            (button, styles, Asset),
            (unit_test, button, Import),
            (browser_test, button, Import),
        ];

        for (dependent, dependency, kind) in edges {
            builder.add_edge(Edge {
                dependent,
                dependency,
                kind,
            });
        }

        builder.mark_asset_blind(unit_test);

        let graph = builder.build();

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/button.css")]),
            [("src/button.browser.test.ts", 2)]
        );
        assert_eq!(
            selected_with_distances(
                &graph,
                &[modified("src/button.css"), modified("src/button.ts")]
            ),
            [("src/button.browser.test.ts", 1), ("src/button.test.ts", 1)]
        );
        assert_eq!(
            reason_files(
                &graph,
                &select_tests(&graph, &[modified("src/button.css")], Widening::Scoped)[0]
            ),
            [
                "src/button.css",
                "src/button.ts",
                "src/button.browser.test.ts"
            ]
        );
    }

    #[test]
    fn reason_ties_are_broken_by_change_path_whatever_the_change_order() {
        let graph = build_graph(
            &["src/ab.test.ts"],
            &[
                ("src/ab.test.ts", Import, "src/a.ts"),
                ("src/ab.test.ts", Import, "src/b.ts"),
            ],
        );
        let selected_tests = select_tests(
            &graph,
            &[modified("src/b.ts"), modified("src/a.ts")],
            Widening::Scoped,
        );

        assert_eq!(selected_tests[0].reason.change, modified("src/a.ts"));
    }

    #[test]
    fn a_read_selects_through_the_content_of_the_file_it_reads_not_through_its_imports() {
        let graph = build_graph(
            &["src/agent.test.ts"],
            &[
                ("src/agent.test.ts", Import, "src/prompts.ts"),
                ("src/prompts.ts", Reads, "src/sample.ts"),
                ("src/sample.ts", Import, "src/helper.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/sample.ts")]),
            [("src/agent.test.ts", 2)]
        );
        assert_eq!(
            selected_with_distances(&graph, &[modified("src/helper.ts")]),
            []
        );
    }

    #[test]
    fn a_listing_passes_the_changes_of_the_listed_files_on_to_its_readers() {
        let graph = build_graph(
            &["src/discovery.test.ts"],
            &[
                ("src/discovery.test.ts", Import, "src/discovery.ts"),
                ("src/discovery.ts", Reads, "extensions/a/plugin.json"),
                ("extensions/a/plugin.json", Lists, "extensions/a/index.ts"),
                ("extensions/a/index.ts", Import, "extensions/a/send.ts"),
            ],
        );
        let selected_tests = select_tests(
            &graph,
            &[modified("extensions/a/index.ts")],
            Widening::Scoped,
        );

        assert_eq!(
            reason_files(&graph, &selected_tests[0]),
            [
                "extensions/a/index.ts",
                "extensions/a/plugin.json",
                "src/discovery.ts",
                "src/discovery.test.ts"
            ]
        );
        assert_eq!(
            selected_with_distances(&graph, &[modified("extensions/a/send.ts")]),
            []
        );
    }

    #[test]
    fn a_listing_of_a_listing_passes_nothing_on() {
        let graph = build_graph(
            &["test/workflows.test.ts"],
            &[
                (
                    "test/workflows.test.ts",
                    Reads,
                    ".github/workflows/scorecard.yml",
                ),
                (".github/workflows/scorecard.yml", Lists, "taxonomy.yaml"),
                ("taxonomy.yaml", Lists, "docs/logging.md"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("taxonomy.yaml")]),
            [("test/workflows.test.ts", 2)]
        );
        assert_eq!(
            selected_with_distances(&graph, &[modified("docs/logging.md")]),
            []
        );
    }

    #[test]
    fn a_file_reached_first_through_behaviour_is_revisited_when_a_listing_reaches_it() {
        let graph = build_graph(
            &["src/reader.test.ts"],
            &[
                ("src/reader.test.ts", Import, "src/reader.ts"),
                ("src/reader.ts", Reads, "data/list.json"),
                ("data/list.json", Import, "data/a.ts"),
                ("data/list.json", Lists, "data/z.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("data/a.ts"), modified("data/z.ts")]),
            [("src/reader.test.ts", 3)]
        );
    }

    #[test]
    fn a_naming_family_selects_its_test_only_when_the_named_file_changes() {
        let graph = build_graph(
            &["src/channel.e2e.test.ts"],
            &[
                ("src/channel.e2e.test.ts", Family, "src/channel.ts"),
                ("src/channel.ts", Import, "src/transport.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/channel.ts")]),
            [("src/channel.e2e.test.ts", 1)]
        );
        assert_eq!(
            selected_with_distances(&graph, &[modified("src/transport.ts")]),
            []
        );
    }

    struct Modules {
        builder: GraphBuilder,
        symbols: HashMap<String, SymbolId>,
    }

    impl Modules {
        fn new(test_files: &[&str]) -> Self {
            let mut builder = GraphBuilder::default();

            builder.add_language("typescript", |path| {
                path.as_str().ends_with(".ts") && !path.as_str().ends_with(".test.ts")
            });

            for test_file in test_files {
                builder.add_test_file(repo_path(test_file), "typescript");
            }

            Self {
                builder,
                symbols: HashMap::new(),
            }
        }

        fn module(mut self, path: &str, names: &[&str]) -> Self {
            let file = self.builder.add_file(repo_path(path));

            for (index, name) in std::iter::once(MODULE_INIT_NAME)
                .chain(names.iter().copied())
                .enumerate()
            {
                let first_line = index as u32 * 10 + 1;
                let symbol = self.builder.add_symbol(Symbol {
                    file,
                    name: name.to_owned(),
                    kind: if index == 0 {
                        SymbolKind::ModuleInit
                    } else {
                        SymbolKind::Function
                    },
                    lines: vec![LineRange {
                        start: first_line,
                        end: first_line + 8,
                    }],
                    hash: 0,
                });

                self.symbols.insert(format!("{path}#{name}"), symbol);
            }

            self
        }

        fn test(mut self, path: &str) -> Self {
            self.builder.add_test_file(repo_path(path), "typescript");

            self
        }

        fn symbol(&self, path: &str, name: &str) -> SymbolId {
            self.symbols[&format!("{path}#{name}")]
        }

        fn imports(mut self, dependent: &str, dependency: &str) -> Self {
            let dependent_file = self.builder.add_file(repo_path(dependent));
            let dependency_file = self.builder.add_file(repo_path(dependency));

            self.builder.add_edge(Edge {
                dependent: dependent_file,
                dependency: dependency_file,
                kind: Import,
            });
            self.uses(dependent, MODULE_INIT_NAME, dependency, MODULE_INIT_NAME)
        }

        fn uses(mut self, path: &str, name: &str, used_path: &str, used_name: &str) -> Self {
            let edge = SymbolEdge {
                dependent: self.symbol(path, name),
                dependency: SymbolTarget::Symbol(self.symbol(used_path, used_name)),
                kind: Import,
            };

            self.builder.add_symbol_edge(edge);

            self
        }

        fn uses_whole(mut self, path: &str, name: &str, module: &str) -> Self {
            let file = self.builder.add_file(repo_path(module));
            let edge = SymbolEdge {
                dependent: self.symbol(path, name),
                dependency: SymbolTarget::Module {
                    file,
                    cause: WholeModuleCause::EscapingNamespace,
                },
                kind: Import,
            };

            self.builder.add_symbol_edge(edge);

            self
        }

        fn edge(mut self, dependent: &str, kind: EdgeKind, dependency: &str) -> Self {
            let dependent = self.builder.add_file(repo_path(dependent));
            let dependency = self.builder.add_file(repo_path(dependency));

            self.builder.add_edge(Edge {
                dependent,
                dependency,
                kind,
            });

            self
        }

        fn changed(mut self, path: &str, names: &[&str]) -> Self {
            let file = self.builder.add_file(repo_path(path));
            let changes = names
                .iter()
                .map(|&name| SymbolChange {
                    file,
                    name: name.to_owned(),
                    kind: SymbolChangeKind::Modified,
                    reaches: vec![SymbolTarget::Symbol(self.symbol(path, name))],
                })
                .collect();

            self.builder.add_module_changes(file, changes);

            self
        }

        fn build(self) -> Graph {
            self.builder.build()
        }
    }

    fn hub_and_its_users() -> Modules {
        Modules::new(&["src/a.test.ts", "src/b.test.ts", "src/hub.test.ts"])
            .module("src/hub.ts", &["a", "b", "c"])
            .module("src/use-a.ts", &["runA"])
            .module("src/use-b.ts", &["runB"])
            .module("src/a.test.ts", &[])
            .module("src/b.test.ts", &[])
            .module("src/hub.test.ts", &[])
            .imports("src/use-a.ts", "src/hub.ts")
            .imports("src/use-b.ts", "src/hub.ts")
            .imports("src/a.test.ts", "src/use-a.ts")
            .imports("src/b.test.ts", "src/use-b.ts")
            .imports("src/hub.test.ts", "src/hub.ts")
            .uses("src/hub.ts", "c", "src/hub.ts", "a")
            .uses("src/use-a.ts", "runA", "src/hub.ts", "c")
            .uses("src/use-b.ts", "runB", "src/hub.ts", "b")
            .uses("src/a.test.ts", MODULE_INIT_NAME, "src/use-a.ts", "runA")
            .uses("src/b.test.ts", MODULE_INIT_NAME, "src/use-b.ts", "runB")
            .uses("src/hub.test.ts", MODULE_INIT_NAME, "src/hub.ts", "b")
    }

    #[test]
    fn changed_symbol_selects_the_tests_of_its_users_with_distances_in_files_crossed() {
        let graph = hub_and_its_users().changed("src/hub.ts", &["a"]).build();
        let selected_tests = select_tests(&graph, &[modified("src/hub.ts")], Widening::Scoped);

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/hub.ts")]),
            [("src/a.test.ts", 2)]
        );
        assert_eq!(
            describe_reason(&graph, &selected_tests[0].reason),
            [
                "src/hub.ts (modified)",
                "-> a changed",
                "-> a used by c in src/hub.ts",
                "-> c used by runA in src/use-a.ts",
                "-> runA used at load by src/a.test.ts",
            ]
        );
    }

    #[test]
    fn changed_initialisation_selects_every_test_loading_the_module() {
        let graph = hub_and_its_users()
            .changed("src/hub.ts", &[MODULE_INIT_NAME])
            .build();

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/hub.ts")]),
            [
                ("src/hub.test.ts", 1),
                ("src/a.test.ts", 2),
                ("src/b.test.ts", 2)
            ]
        );
    }

    #[test]
    fn any_symbol_change_reaches_whole_module_users_and_the_files_linked_to_the_module() {
        let graph = hub_and_its_users()
            .test("src/registry.test.ts")
            .test("src/hub-source.test.ts")
            .module("src/registry.ts", &["all"])
            .module("src/registry.test.ts", &[])
            .imports("src/registry.ts", "src/hub.ts")
            .imports("src/registry.test.ts", "src/registry.ts")
            .uses_whole("src/registry.ts", "all", "src/hub.ts")
            .uses(
                "src/registry.test.ts",
                MODULE_INIT_NAME,
                "src/registry.ts",
                "all",
            )
            .edge("src/hub-source.test.ts", Reads, "src/hub.ts")
            .changed("src/hub.ts", &["a"])
            .build();

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/hub.ts")]),
            [
                ("src/hub-source.test.ts", 1),
                ("src/a.test.ts", 2),
                ("src/registry.test.ts", 2)
            ]
        );
    }

    #[test]
    fn change_without_changed_symbols_reaches_only_the_readers_of_the_file() {
        let mut modules = hub_and_its_users().test("src/source.test.ts");
        let hub = modules.builder.add_file(repo_path("src/hub.ts"));

        modules.builder.add_module_changes(hub, Vec::new());

        let graph = modules
            .edge("src/source.test.ts", Reads, "src/hub.ts")
            .edge("src/hub.test.ts", Family, "src/hub.ts")
            .build();

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/hub.ts")]),
            [("src/source.test.ts", 1)]
        );
    }

    #[test]
    fn file_outside_the_analysis_reaches_the_users_of_every_symbol_of_its_reader() {
        let graph = hub_and_its_users()
            .edge("src/hub.ts", Reads, "src/config.json")
            .build();

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/config.json")]),
            [
                ("src/hub.test.ts", 2),
                ("src/a.test.ts", 3),
                ("src/b.test.ts", 3)
            ]
        );
    }

    #[test]
    fn mocked_module_cuts_symbol_paths_unless_another_path_avoids_it() {
        let mut modules = hub_and_its_users().changed("src/hub.ts", &["a"]);
        let a_test = modules.builder.add_file(repo_path("src/a.test.ts"));
        let use_a = modules.builder.add_file(repo_path("src/use-a.ts"));

        modules.builder.add_mock(a_test, use_a);

        let mocked = modules.build();
        let mut modules = hub_and_its_users()
            .uses("src/a.test.ts", MODULE_INIT_NAME, "src/hub.ts", "c")
            .changed("src/hub.ts", &["a"]);
        let a_test = modules.builder.add_file(repo_path("src/a.test.ts"));
        let use_a = modules.builder.add_file(repo_path("src/use-a.ts"));

        modules.builder.add_mock(a_test, use_a);

        let mocked_with_a_direct_use = modules.build();

        assert_eq!(
            selected_with_distances(&mocked, &[modified("src/hub.ts")]),
            []
        );
        assert_eq!(
            selected_with_distances(&mocked_with_a_direct_use, &[modified("src/hub.ts")]),
            [("src/a.test.ts", 1)]
        );
    }

    #[test]
    fn widening_reaches_the_users_of_the_symbol_holding_the_site_or_every_importer_at_load() {
        let site_graph = |line: u32| {
            let mut modules = Modules::new(&["src/load.test.ts", "src/other.test.ts"])
                .module("src/loader.ts", &["load", "other"])
                .module("src/load.test.ts", &[])
                .module("src/other.test.ts", &[])
                .imports("src/load.test.ts", "src/loader.ts")
                .imports("src/other.test.ts", "src/loader.ts")
                .uses(
                    "src/load.test.ts",
                    MODULE_INIT_NAME,
                    "src/loader.ts",
                    "load",
                )
                .uses(
                    "src/other.test.ts",
                    MODULE_INIT_NAME,
                    "src/loader.ts",
                    "other",
                );
            let file = modules.builder.add_file(repo_path("src/loader.ts"));

            modules.builder.add_file(repo_path("plugins/a.ts"));
            modules.builder.add_unresolved_site(UnresolvedSite {
                file,
                line,
                expression: "request.moduleUrl".to_owned(),
                cause: UnresolvedCause::Parameter,
                language: "typescript",
            });

            modules.build()
        };
        let inside_load = site_graph(15);
        let at_top_level = site_graph(5);

        assert_eq!(
            widening_sites(&inside_load, &[modified("plugins/a.ts")]),
            [("src/load.test.ts".to_owned(), Some(0))]
        );
        assert_eq!(
            widening_sites(&at_top_level, &[modified("plugins/a.ts")]),
            [
                ("src/load.test.ts".to_owned(), Some(0)),
                ("src/other.test.ts".to_owned(), Some(0))
            ]
        );
    }

    #[test]
    fn file_level_relaxation_follows_imports_whatever_the_symbols() {
        let graph = hub_and_its_users().changed("src/hub.ts", &["a"]).build();
        let relaxed: Vec<&str> = select_relaxed(
            &graph,
            &[modified("src/hub.ts")],
            Widening::Scoped,
            Some(Relaxation::FileLevel),
        )
        .iter()
        .map(|selected_test| graph.path(selected_test.file).as_str())
        .collect();

        assert_eq!(
            relaxed,
            ["src/hub.test.ts", "src/a.test.ts", "src/b.test.ts"]
        );
    }

    fn hub_crossings<'g>(
        graph: &'g Graph,
        changes: &[FileChange],
    ) -> Vec<(&'g str, usize, Option<&'g str>)> {
        select_tests(graph, changes, Widening::Scoped)
            .iter()
            .map(|selected_test| {
                let hub_crossing = selected_test.hub_crossing();

                (
                    graph.path(selected_test.file).as_str(),
                    hub_crossing.fanout,
                    hub_crossing.hub.map(|hub| graph.path(hub).as_str()),
                )
            })
            .collect()
    }

    #[test]
    fn hub_crossing_is_the_largest_fanout_left_before_the_tests_own_dependency() {
        let graph = build_graph(
            &["src/direct.test.ts", "src/use.test.ts", "src/far.test.ts"],
            &[
                ("src/direct.test.ts", Import, "src/hub.ts"),
                ("src/use.ts", Import, "src/hub.ts"),
                ("src/other.ts", Import, "src/hub.ts"),
                ("src/use.test.ts", Import, "src/use.ts"),
                ("src/far.ts", Import, "src/use.ts"),
                ("src/far.test.ts", Import, "src/far.ts"),
            ],
        );

        assert_eq!(
            hub_crossings(&graph, &[modified("src/hub.ts")]),
            [
                ("src/direct.test.ts", 0, None),
                ("src/use.test.ts", 3, Some("src/hub.ts")),
                ("src/far.test.ts", 3, Some("src/hub.ts"))
            ]
        );
    }

    #[test]
    fn hub_crossing_follows_the_path_past_the_smallest_hubs_not_the_closest() {
        let graph = build_graph(
            &["src/api.test.ts"],
            &[
                ("src/hub.ts", Import, "src/db.ts"),
                ("src/store.ts", Import, "src/db.ts"),
                ("src/api.ts", Import, "src/hub.ts"),
                ("src/one.ts", Import, "src/hub.ts"),
                ("src/two.ts", Import, "src/hub.ts"),
                ("src/three.ts", Import, "src/hub.ts"),
                ("src/cache.ts", Import, "src/store.ts"),
                ("src/session.ts", Import, "src/cache.ts"),
                ("src/api.test.ts", Import, "src/api.ts"),
                ("src/api.test.ts", Import, "src/session.ts"),
            ],
        );

        assert_eq!(
            selected_with_distances(&graph, &[modified("src/db.ts")]),
            [("src/api.test.ts", 3)]
        );
        assert_eq!(
            hub_crossings(&graph, &[modified("src/db.ts")]),
            [("src/api.test.ts", 2, Some("src/db.ts"))]
        );
    }

    #[test]
    fn hub_crossing_counts_files_crossed_by_symbol_uses() {
        let graph = hub_and_its_users().changed("src/hub.ts", &["a"]).build();

        assert_eq!(
            hub_crossings(&graph, &[modified("src/hub.ts")]),
            [("src/a.test.ts", 3, Some("src/hub.ts"))]
        );
    }

    #[test]
    fn changed_test_file_crosses_no_hub() {
        let graph = build_graph(&["src/a.test.ts"], &[("src/a.test.ts", Import, "src/a.ts")]);

        assert_eq!(
            hub_crossings(&graph, &[modified("src/a.test.ts")]),
            [("src/a.test.ts", 0, None)]
        );
    }
}
