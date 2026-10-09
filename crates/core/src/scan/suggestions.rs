//! Config lines worth writing, as ready-to-paste TOML: links declaring what the most widening unresolved sites load, and ignores of the widest inferred links.

use std::collections::BTreeMap;

use crate::scan::{InferredSite, UnresolvedSiteReach};

const SUGGESTED_RULES: usize = 3;

const MIN_WIDENED_TESTS: usize = 10;

const MIN_WIDE_LINKS: usize = 1_000;

const GLOB_KINDS: [&str; 2] = ["glob", "file glob"];

const DIRECTORY_KINDS: [&str; 2] = ["directory listing", "file copy"];

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
    let mut suggestions = link_suggestions(unresolved_sites, test_count);

    suggestions.extend(ignore_suggestions(inferred_sites, file_count));

    suggestions
}

fn link_suggestions(
    unresolved_sites: &[UnresolvedSiteReach],
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

    widest.sort_by(|left, right| right.1.tests.cmp(&left.1.tests).then(left.0.cmp(right.0)));
    widest
        .into_iter()
        .take(SUGGESTED_RULES)
        .map(|(file, unresolved_site)| Suggestion {
            reason: format!(
                "{} widens to {} test files: {} {}; declare what it loads and the unresolved sites of {file} stop widening",
                unresolved_site.site,
                unresolved_site.tests,
                unresolved_site.expression,
                unresolved_site.cause.description()
            ),
            toml: format!(
                "[[link]]\nfrom = \"path/to/what/it/loads/**\"\nto = {file:?}\n"
            ),
        })
        .collect()
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
                    reason: "src/plugins/loader.ts:12 widens to 900 test files: import(entry) comes from a function parameter; declare what it loads and the unresolved sites of src/plugins/loader.ts stop widening".to_owned(),
                    toml: "[[link]]\nfrom = \"path/to/what/it/loads/**\"\nto = \"src/plugins/loader.ts\"\n".to_owned(),
                },
                Suggestion {
                    reason: "test/inventory.test-support.ts:4 links 1200 files (file glob **/*.test.ts); if test/inventory.test-support.ts does not depend on all of them".to_owned(),
                    toml: "[[ignore]]\nfrom = \"**/*.test.ts\"\nto = \"test/inventory.test-support.ts\"\n".to_owned(),
                },
            ]
        );
    }
}
