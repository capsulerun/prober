//! Config lines worth writing, as ready-to-paste TOML: links declaring what the most widening unresolved sites load (the repository's plugin folders when it has some) with ignores setting those sites aside, and ignores of the widest inferred links.

use std::collections::BTreeMap;

use crate::scan::{InferredSite, UnresolvedSiteReach};

const SUGGESTED_RULES: usize = 3;

const MIN_WIDENED_TESTS: usize = 10;

const MIN_WIDE_LINKS: usize = 1_000;

const GLOB_KINDS: [&str; 2] = ["glob", "file glob"];

const DIRECTORY_KINDS: [&str; 2] = ["directory listing", "file copy"];

const PLUGIN_FOLDER_KIND: &str = "plugin folder";

const LOADED_PLACEHOLDER: &str = "path/to/what/it/loads/**";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub reason: String,
    pub toml: String,
}

pub(crate) fn suggestions(
    unresolved_sites: &[UnresolvedSiteReach],
    inferred_sites: &[InferredSite],
    test_count: usize,
    file_count: usize,
) -> Vec<Suggestion> {
    let mut suggestions = link_suggestions(
        unresolved_sites,
        plugin_folders(inferred_sites).as_deref(),
        test_count,
    );

    suggestions.extend(ignore_suggestions(inferred_sites, file_count));

    suggestions
}

fn link_suggestions(
    unresolved_sites: &[UnresolvedSiteReach],
    plugin_folders: Option<&str>,
    test_count: usize,
) -> Vec<Suggestion> {
    let min_tests = MIN_WIDENED_TESTS.max(test_count / 100);
    let mut widest_by_file: BTreeMap<&str, &UnresolvedSiteReach> = BTreeMap::new();

    for unresolved_site in unresolved_sites
        .iter()
        .filter(|unresolved_site| unresolved_site.tests >= min_tests)
    {
        let file = site_file(&unresolved_site.site);
        let is_wider = widest_by_file
            .get(file)
            .is_none_or(|widest| unresolved_site.tests > widest.tests);

        if is_wider {
            widest_by_file.insert(file, unresolved_site);
        }
    }

    let mut widest: Vec<(&str, &UnresolvedSiteReach)> = widest_by_file.into_iter().collect();
    let (loaded, plugin_note) = match plugin_folders {
        Some(pattern) => (
            folder_glob(pattern),
            format!(" (the repository's plugin folders {pattern}, if it loads plugins)"),
        ),
        None => (LOADED_PLACEHOLDER.to_owned(), String::new()),
    };

    widest.sort_by(|left, right| right.1.tests.cmp(&left.1.tests).then(left.0.cmp(right.0)));
    widest
        .into_iter()
        .take(SUGGESTED_RULES)
        .map(|(file, unresolved_site)| Suggestion {
            reason: format!(
                "{} widens to {} test files: {} {}; declare what {file} loads{plugin_note}, then set its unresolved sites aside",
                unresolved_site.site,
                unresolved_site.tests,
                unresolved_site.expression,
                unresolved_site.cause.description()
            ),
            toml: format!(
                "[[link]]\nfrom = {loaded:?}\nto = {file:?}\n\n[[ignore]]\nsite = {file:?}\n"
            ),
        })
        .collect()
}

fn plugin_folders(inferred_sites: &[InferredSite]) -> Option<String> {
    let mut links_by_pattern: BTreeMap<&str, usize> = BTreeMap::new();

    for inferred_site in inferred_sites {
        for pattern in inferred_site
            .patterns
            .iter()
            .filter(|pattern| pattern.kind == PLUGIN_FOLDER_KIND)
        {
            *links_by_pattern.entry(&pattern.pattern).or_default() += inferred_site.links;
        }
    }

    links_by_pattern
        .into_iter()
        .max_by(|left, right| left.1.cmp(&right.1).then(right.0.cmp(left.0)))
        .map(|(pattern, _)| pattern.to_owned())
}

fn folder_glob(pattern: &str) -> String {
    let mut glob = String::new();
    let mut rest = pattern;

    while let Some((before, after)) = rest.split_once('{') {
        glob.push_str(before);
        glob.push('*');
        rest = after.split_once('}').map_or("", |(_, after)| after);
    }

    glob.push_str(rest.trim_end_matches('/'));

    format!("{glob}/**")
}

fn ignore_suggestions(inferred_sites: &[InferredSite], file_count: usize) -> Vec<Suggestion> {
    let min_links = MIN_WIDE_LINKS.max(file_count / 10);

    inferred_sites
        .iter()
        .filter(|inferred_site| inferred_site.links >= min_links)
        .take(SUGGESTED_RULES)
        .map(|inferred_site| {
            let file = site_file(&inferred_site.site);
            let from = match inferred_site.patterns.as_slice() {
                [pattern] if GLOB_KINDS.contains(&pattern.kind) => pattern.pattern.clone(),
                [pattern] if DIRECTORY_KINDS.contains(&pattern.kind) => {
                    format!("{}/**", pattern.pattern.trim_end_matches('/'))
                }
                _ => "**".to_owned(),
            };
            let patterns: Vec<String> = inferred_site
                .patterns
                .iter()
                .map(|pattern| format!("{} {}", pattern.kind, pattern.pattern))
                .collect();

            Suggestion {
                reason: format!(
                    "{} links {} files ({}); if {file} does not depend on all of them",
                    inferred_site.site,
                    inferred_site.links,
                    patterns.join(", ")
                ),
                toml: format!("[[ignore]]\nfrom = {from:?}\nto = {file:?}\n"),
            }
        })
        .collect()
}

fn site_file(site: &str) -> &str {
    site.rsplit_once(':').map_or(site, |(file, _)| file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::UnresolvedCause;
    use crate::scan::InferredPattern;

    fn unresolved(site: &str, tests: usize) -> UnresolvedSiteReach {
        UnresolvedSiteReach {
            site: site.to_owned(),
            expression: "import(entry)".to_owned(),
            cause: UnresolvedCause::Parameter,
            tests,
            modules: 3_000,
            in_test_code: false,
        }
    }

    #[test]
    fn the_widest_unresolved_files_and_links_get_a_ready_to_paste_rule() {
        let unresolved_sites = [
            unresolved("src/plugins/loader.ts:12", 900),
            unresolved("src/plugins/loader.ts:30", 400),
            unresolved("src/cli/run.ts:8", 5),
        ];
        let inferred_sites = [InferredSite {
            site: "test/inventory.test-support.ts:4".to_owned(),
            patterns: vec![InferredPattern {
                kind: "file glob",
                pattern: "**/*.test.ts".to_owned(),
            }],
            links: 1_200,
        }];
        let suggestions = suggestions(&unresolved_sites, &inferred_sites, 1_000, 5_000);

        assert_eq!(
            suggestions,
            [
                Suggestion {
                    reason: "src/plugins/loader.ts:12 widens to 900 test files: import(entry) comes from a function parameter; declare what src/plugins/loader.ts loads, then set its unresolved sites aside".to_owned(),
                    toml: "[[link]]\nfrom = \"path/to/what/it/loads/**\"\nto = \"src/plugins/loader.ts\"\n\n[[ignore]]\nsite = \"src/plugins/loader.ts\"\n".to_owned(),
                },
                Suggestion {
                    reason: "test/inventory.test-support.ts:4 links 1200 files (file glob **/*.test.ts); if test/inventory.test-support.ts does not depend on all of them".to_owned(),
                    toml: "[[ignore]]\nfrom = \"**/*.test.ts\"\nto = \"test/inventory.test-support.ts\"\n".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn a_loader_is_suggested_the_plugin_folders_the_repository_loads_elsewhere() {
        let unresolved_sites = [unresolved("src/plugins/loader.ts:12", 900)];
        let plugin_folder_site = |site: &str, pattern: &str, links: usize| InferredSite {
            site: site.to_owned(),
            patterns: vec![InferredPattern {
                kind: PLUGIN_FOLDER_KIND,
                pattern: pattern.to_owned(),
            }],
            links,
        };
        let inferred_sites = [
            plugin_folder_site("src/channels.ts:4", "extensions/{id}/", 40),
            plugin_folder_site("src/themes.ts:9", "packages/*/themes/{id}/", 6),
            plugin_folder_site("src/bundled.ts:2", "extensions/{id}/", 40),
        ];

        assert_eq!(
            suggestions(&unresolved_sites, &inferred_sites, 1_000, 5_000),
            [Suggestion {
                reason: "src/plugins/loader.ts:12 widens to 900 test files: import(entry) comes from a function parameter; declare what src/plugins/loader.ts loads (the repository's plugin folders extensions/{id}/, if it loads plugins), then set its unresolved sites aside".to_owned(),
                toml: "[[link]]\nfrom = \"extensions/*/**\"\nto = \"src/plugins/loader.ts\"\n\n[[ignore]]\nsite = \"src/plugins/loader.ts\"\n".to_owned(),
            }]
        );
        assert_eq!(
            folder_glob("packages/*/themes/{id}/"),
            "packages/*/themes/*/**"
        );
    }
}
