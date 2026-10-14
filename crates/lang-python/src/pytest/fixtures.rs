//! Fixtures by name: each test uses the fixtures it requests, resolved as pytest resolves them through its module, the conftests above it and its plugins, so a change to one fixture selects the tests using it while load-time code, hooks and autouse fixtures still reach every test in scope.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::Result;
use prober_core::changes::{BaseRevision, ChangeKind, FileChange};
use prober_core::files::{FileId, RepoPath};
use prober_core::graph::{EdgeKind, GraphBuilder};
use prober_core::symbols::{
    LineRange, MODULE_INIT_NAME, Symbol, SymbolChange, SymbolChangeKind, SymbolEdge, SymbolId,
    SymbolKind, SymbolTarget, WholeModuleCause,
};

use crate::conventions::{CONFTEST, join_directory, parent_directory, split_path};
use crate::modules::ModuleIndex;
use crate::pytest::items::{PytestItems, parse_items};
use crate::summary::ModuleSummary;
use crate::symbols::AnalysedModule;

const WHOLE_FILE: LineRange = LineRange {
    start: 1,
    end: u32::MAX,
};

const MAX_PLUGIN_DEPTH: usize = 8;

pub(crate) struct ModuleEdge {
    pub dependency: RepoPath,
    pub kind: EdgeKind,
}

pub(crate) struct FixtureScopes<'f> {
    pub summaries: &'f HashMap<RepoPath, ModuleSummary>,
    pub index: &'f ModuleIndex,
    pub tests: &'f BTreeSet<RepoPath>,
    pub global_plugins: Vec<RepoPath>,
    pub scoped_requests: Vec<(String, Vec<String>)>,
    pub module_edges: &'f HashMap<RepoPath, Vec<ModuleEdge>>,
    pub analysed: &'f HashMap<RepoPath, AnalysedModule>,
}

struct Provider {
    file: FileId,
    module: SymbolId,
    fixtures: HashMap<String, SymbolId>,
}

impl FixtureScopes<'_> {
    pub fn add_to(
        &self,
        graph: &mut GraphBuilder,
        changes: &[FileChange],
        base: &dyn BaseRevision,
    ) -> Result<()> {
        let conftests: HashSet<&str> = self
            .summaries
            .keys()
            .map(RepoPath::as_str)
            .filter(|path| split_path(path).1 == CONFTEST)
            .collect();
        let mut providers: HashMap<RepoPath, Provider> = HashMap::new();

        for test in self.tests {
            let Some(items) = self.items(test) else {
                continue;
            };
            let Chain {
                providers: chain,
                unparsed,
                ..
            } = self.chain(test, &conftests);
            let test_symbol = match self.analysed.get(test) {
                Some(analysed) => analysed.init,
                None => {
                    let test_file = graph.add_file(test.clone());

                    graph.add_symbol(Symbol {
                        file: test_file,
                        name: MODULE_INIT_NAME.to_owned(),
                        kind: SymbolKind::ModuleInit,
                        lines: vec![WHOLE_FILE],
                        hash: 0,
                    })
                }
            };

            for provider in &chain {
                if !providers.contains_key(provider) {
                    let registered = self.register(provider, graph);

                    providers.insert(provider.clone(), registered);
                }

                let provider = &providers[provider];

                graph.add_symbol_edge(SymbolEdge {
                    dependent: test_symbol,
                    dependency: SymbolTarget::Symbol(provider.module),
                    kind: EdgeKind::Import,
                });
            }

            for fixture in self.used_fixtures(test, items, &chain) {
                if let Some(&symbol) = providers
                    .get(&chain[fixture.provider])
                    .and_then(|provider| provider.fixtures.get(&fixture.name))
                {
                    graph.add_symbol_edge(SymbolEdge {
                        dependent: test_symbol,
                        dependency: SymbolTarget::Symbol(symbol),
                        kind: EdgeKind::Import,
                    });
                }
            }

            self.mirror_module_edges(test, test_symbol, graph);

            for unparsed_provider in unparsed {
                let file = graph.add_file(unparsed_provider);

                graph.add_symbol_edge(SymbolEdge {
                    dependent: test_symbol,
                    dependency: SymbolTarget::Module {
                        file,
                        cause: WholeModuleCause::NotAnalysed,
                    },
                    kind: EdgeKind::Import,
                });
            }
        }

        let mut provider_paths: Vec<&RepoPath> = providers.keys().collect();

        provider_paths.sort();

        for path in provider_paths {
            self.mirror_module_edges(path, providers[path].module, graph);
        }

        providers.retain(|path, _| !self.analysed.contains_key(path));

        add_provider_changes(&providers, self.summaries, changes, base, graph)
    }

    fn items(&self, module: &RepoPath) -> Option<&PytestItems> {
        self.summaries
            .get(module)
            .map(|summary| &summary.pytest)
            .filter(|items| items.parsed)
    }

    fn chain(&self, test: &RepoPath, conftests: &HashSet<&str>) -> Chain {
        let mut chain = Chain::default();
        let mut directory = parent_directory(test.as_str());

        self.add_plugins(test, &mut chain, 0);

        loop {
            let conftest = join_directory(directory, CONFTEST);

            if conftests.contains(conftest.as_str())
                && let Ok(conftest) = RepoPath::new(conftest)
            {
                self.add_provider(&conftest, &mut chain, 0);
            }

            if directory.is_empty() {
                break;
            }

            directory = parent_directory(directory);
        }

        for plugin in &self.global_plugins {
            self.add_provider(plugin, &mut chain, 0);
        }

        chain
    }

    fn add_provider(&self, provider: &RepoPath, chain: &mut Chain, depth: usize) {
        if !chain.seen.insert(provider.clone()) {
            return;
        }

        if self.items(provider).is_none() {
            chain.unparsed.push(provider.clone());

            return;
        }

        chain.providers.push(provider.clone());
        self.add_plugins(provider, chain, depth);
    }

    fn add_plugins(&self, declarer: &RepoPath, chain: &mut Chain, depth: usize) {
        let Some(items) = self.items(declarer) else {
            return;
        };

        if depth >= MAX_PLUGIN_DEPTH {
            return;
        }

        for plugin in &items.plugins {
            if let Some(resolved) = self
                .index
                .resolve_module(declarer, plugin)
                .into_iter()
                .next()
            {
                self.add_provider(&resolved.module, chain, depth + 1);
            }
        }
    }

    fn used_fixtures(
        &self,
        test: &RepoPath,
        items: &PytestItems,
        chain: &[RepoPath],
    ) -> Vec<UsedFixture> {
        let scopes: Vec<&PytestItems> = std::iter::once(items)
            .chain(chain.iter().filter_map(|provider| self.items(provider)))
            .collect();
        let mut requests: Vec<(String, usize)> = items
            .requests
            .iter()
            .map(|name| (name.clone(), 0))
            .collect();

        for scope in &scopes {
            requests.extend(
                scope
                    .fixtures
                    .iter()
                    .filter(|fixture| fixture.autouse)
                    .map(|fixture| (fixture.name.clone(), 0)),
            );
        }

        for (directory, names) in &self.scoped_requests {
            if directory.is_empty() || test.as_str().starts_with(&format!("{directory}/")) {
                requests.extend(names.iter().map(|name| (name.clone(), 0)));
            }
        }

        let mut used = Vec::new();
        let mut visited = HashSet::new();
        let mut dynamic = items.dynamic_requests;

        while let Some((name, start)) = requests.pop() {
            let Some(position) =
                (start..scopes.len()).find(|&position| scopes[position].fixture(&name).is_some())
            else {
                continue;
            };

            if !visited.insert((position, name.clone())) {
                continue;
            }

            let Some(fixture) = scopes[position].fixture(&name) else {
                continue;
            };

            dynamic |= fixture.dynamic_requests;

            for request in &fixture.requests {
                let start = if *request == name { position + 1 } else { 0 };

                requests.push((request.clone(), start));
            }

            if position > 0 {
                used.push(UsedFixture {
                    provider: position - 1,
                    name,
                });
            }
        }

        if dynamic {
            for (position, scope) in scopes.iter().enumerate().skip(1) {
                used.extend(scope.fixtures.iter().map(|fixture| UsedFixture {
                    provider: position - 1,
                    name: fixture.name.clone(),
                }));
            }
        }

        used
    }

    fn register(&self, provider: &RepoPath, graph: &mut GraphBuilder) -> Provider {
        if let Some(analysed) = self.analysed.get(provider) {
            return Provider {
                file: graph.add_file(provider.clone()),
                module: analysed.init,
                fixtures: analysed.fixtures.clone(),
            };
        }

        let items = self
            .items(provider)
            .expect("providers are registered only with parsed items");
        let file = graph.add_file(provider.clone());
        let module = graph.add_symbol(Symbol {
            file,
            name: MODULE_INIT_NAME.to_owned(),
            kind: SymbolKind::ModuleInit,
            lines: items.module_lines.clone(),
            hash: items.module_hash,
        });
        let fixtures = items
            .fixtures
            .iter()
            .map(|fixture| {
                let symbol = graph.add_symbol(Symbol {
                    file,
                    name: fixture.name.clone(),
                    kind: SymbolKind::Fixture,
                    lines: vec![fixture.lines],
                    hash: fixture.hash,
                });

                (fixture.name.clone(), symbol)
            })
            .collect();

        Provider {
            file,
            module,
            fixtures,
        }
    }

    fn mirror_module_edges(&self, module: &RepoPath, symbol: SymbolId, graph: &mut GraphBuilder) {
        if self.analysed.contains_key(module) {
            return;
        }

        for edge in self.module_edges.get(module).into_iter().flatten() {
            let file = graph.add_file(edge.dependency.clone());

            graph.add_symbol_edge(SymbolEdge {
                dependent: symbol,
                dependency: SymbolTarget::Module {
                    file,
                    cause: WholeModuleCause::Imported,
                },
                kind: edge.kind,
            });
        }
    }
}

#[derive(Default)]
struct Chain {
    providers: Vec<RepoPath>,
    unparsed: Vec<RepoPath>,
    seen: HashSet<RepoPath>,
}

struct UsedFixture {
    provider: usize,
    name: String,
}

fn add_provider_changes(
    providers: &HashMap<RepoPath, Provider>,
    summaries: &HashMap<RepoPath, ModuleSummary>,
    changes: &[FileChange],
    base: &dyn BaseRevision,
    graph: &mut GraphBuilder,
) -> Result<()> {
    for change in changes
        .iter()
        .filter(|change| change.kind == ChangeKind::Modified)
    {
        let (Some(provider), Some(summary)) =
            (providers.get(&change.path), summaries.get(&change.path))
        else {
            continue;
        };
        let Some(base_bytes) = base.read_file(&change.path)? else {
            continue;
        };
        let base_items = parse_items(&String::from_utf8_lossy(&base_bytes));

        if !base_items.parsed {
            continue;
        }

        let head_items = &summary.pytest;
        let mut symbol_changes = Vec::new();
        let module_change = |kind: SymbolChangeKind, name: &str| SymbolChange {
            file: provider.file,
            name: name.to_owned(),
            kind,
            reaches: vec![SymbolTarget::Symbol(provider.module)],
        };

        if base_items.module_hash != head_items.module_hash
            || base_items.plugins != head_items.plugins
        {
            symbol_changes.push(module_change(SymbolChangeKind::Modified, MODULE_INIT_NAME));
        }

        for fixture in &head_items.fixtures {
            let kind = match base_items.fixture(&fixture.name) {
                Some(before) if before.hash == fixture.hash => continue,
                Some(_) => SymbolChangeKind::Modified,
                None => SymbolChangeKind::Added,
            };
            let Some(&symbol) = provider.fixtures.get(&fixture.name) else {
                continue;
            };

            symbol_changes.push(SymbolChange {
                file: provider.file,
                name: fixture.name.clone(),
                kind,
                reaches: vec![SymbolTarget::Symbol(symbol)],
            });
        }

        for removed in base_items
            .fixtures
            .iter()
            .filter(|fixture| head_items.fixture(&fixture.name).is_none())
        {
            symbol_changes.push(module_change(SymbolChangeKind::Removed, &removed.name));
        }

        graph.add_module_changes(provider.file, symbol_changes);
    }

    Ok(())
}
