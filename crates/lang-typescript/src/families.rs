//! Naming families: tests paired with the files they are named after, beside them or in a mirrored test tree, and with the fixtures named after them.

use std::collections::{BTreeSet, HashMap};

use prober_core::files::RepoPath;

use crate::conventions::is_asset_file;
use crate::workspaces::parent_directory;

const TEST_DIRECTORIES: [&str; 3] = ["__tests__", "test", "tests"];

const FIXTURE_DIRECTORIES: [&str; 2] = ["fixtures", "__fixtures__"];

const TEST_MARKERS: [&str; 2] = ["test", "spec"];

pub(crate) fn naming_families<'f>(
    repo_files: &'f [RepoPath],
    tests: &'f BTreeSet<RepoPath>,
) -> Vec<(&'f RepoPath, &'f RepoPath)> {
    let mut files_by_stem: HashMap<(&str, &str), Vec<&RepoPath>> = HashMap::new();
    let mut fixtures_by_name: HashMap<(&str, &str), Vec<&RepoPath>> = HashMap::new();

    for file in repo_files
        .iter()
        .filter(|file| !tests.contains(*file) && !is_asset_file(file))
    {
        let (directory, name) = split_path(file.as_str());
        let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);

        files_by_stem
            .entry((directory, stem))
            .or_default()
            .push(file);

        if let Some(fixture) = fixture_name(file.as_str()) {
            fixtures_by_name.entry(fixture).or_default().push(file);
        }
    }

    let mut pairs = Vec::new();

    for test in tests {
        let stems = family_stems(test.as_str());

        for directory in family_directories(parent_directory(test)) {
            for stem in &stems {
                for &file in files_by_stem
                    .get(&(directory.as_str(), *stem))
                    .into_iter()
                    .flatten()
                {
                    pairs.push((test, file));
                }
            }
        }
    }

    let sorted_tests: Vec<&RepoPath> = tests.iter().collect();

    for ((fixtures_parent, name), fixtures) in fixtures_by_name {
        let prefix = if fixtures_parent.is_empty() {
            String::new()
        } else {
            format!("{fixtures_parent}/")
        };
        let start = sorted_tests.partition_point(|test| test.as_str() < prefix.as_str());
        let named_tests = sorted_tests[start..]
            .iter()
            .take_while(|test| test.as_str().starts_with(&prefix))
            .filter(|test| family_stems(test.as_str()).contains(&name));

        for &test in named_tests {
            pairs.extend(fixtures.iter().map(|&fixture| (test, fixture)));
        }
    }

    pairs.sort();
    pairs.dedup();

    pairs
}

fn family_stems(test: &str) -> Vec<&str> {
    let (_, name) = split_path(test);
    let without_extension = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let base = match without_extension.rsplit_once('.') {
        Some((base, marker)) if TEST_MARKERS.contains(&marker) => base,
        _ => without_extension,
    };
    let mut stems = vec![base];

    for (offset, _) in base
        .match_indices(['.', '-'])
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        if offset > 0 {
            stems.push(&base[..offset]);
        }
    }

    stems
}

fn family_directories(test_directory: &str) -> Vec<String> {
    let segments: Vec<&str> = test_directory.split('/').collect();
    let mut directories = vec![test_directory.to_owned()];

    for (position, segment) in segments.iter().enumerate() {
        if !TEST_DIRECTORIES.contains(segment) {
            continue;
        }

        let mirrored: Vec<&str> = segments
            .iter()
            .enumerate()
            .filter(|(other_position, _)| *other_position != position)
            .map(|(_, other_segment)| *other_segment)
            .collect();

        directories.push(mirrored.join("/"));
    }

    directories
}

fn fixture_name(path: &str) -> Option<(&str, &str)> {
    let segments: Vec<&str> = path.split('/').collect();
    let position = segments
        .iter()
        .position(|segment| FIXTURE_DIRECTORIES.contains(segment))?;
    let entry = segments.get(position + 1)?;
    let name = if position + 2 == segments.len() {
        entry.split_once('.').map_or(*entry, |(name, _)| name)
    } else {
        entry
    };

    if name.is_empty() {
        return None;
    }

    let parent_length = segments[..position]
        .iter()
        .map(|segment| segment.len() + 1)
        .sum::<usize>()
        .saturating_sub(1);

    Some((&path[..parent_length], name))
}

fn split_path(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    #[test]
    fn a_test_is_named_after_its_base_and_every_prefix_ending_at_a_dot_or_dash() {
        assert_eq!(family_stems("src/channel.test.ts"), ["channel"]);
        assert_eq!(
            family_stems("src/channel-send.e2e.test.ts"),
            ["channel-send.e2e", "channel-send", "channel"]
        );
        assert_eq!(
            family_stems("cypress/e2e/login.cy.ts"),
            ["login.cy", "login"]
        );
        assert_eq!(
            family_stems("packages/legacy/__tests__/render.js"),
            ["render"]
        );
    }

    #[test]
    fn tests_look_beside_them_and_in_the_tree_their_test_directory_mirrors() {
        assert_eq!(family_directories("src"), ["src"]);
        assert_eq!(
            family_directories("src/__tests__"),
            ["src/__tests__", "src"]
        );
        assert_eq!(
            family_directories("test/scripts"),
            ["test/scripts", "scripts"]
        );
        assert_eq!(
            family_directories("packages/app/tests/unit"),
            ["packages/app/tests/unit", "packages/app/unit"]
        );
    }

    #[test]
    fn fixtures_are_named_by_the_entry_under_their_fixtures_directory() {
        assert_eq!(
            fixture_name("test/fixtures/basic/input.json"),
            Some(("test", "basic"))
        );
        assert_eq!(
            fixture_name("test/fixtures/basic.json"),
            Some(("test", "basic"))
        );
        assert_eq!(
            fixture_name("__fixtures__/legacy/a/b.ts"),
            Some(("", "legacy"))
        );
        assert_eq!(fixture_name("test/fixtures/.gitkeep"), None);
        assert_eq!(fixture_name("src/fixtures.ts"), None);
    }

    #[test]
    fn families_pair_tests_with_files_but_never_with_tests_or_assets() {
        let repo_files: Vec<RepoPath> = [
            "src/button.ts",
            "src/button.css",
            "src/button.test.ts",
            "src/button.stories.test.ts",
        ]
        .into_iter()
        .map(repo_path)
        .collect();
        let tests: BTreeSet<RepoPath> = [
            repo_path("src/button.test.ts"),
            repo_path("src/button.stories.test.ts"),
        ]
        .into_iter()
        .collect();
        let pairs: Vec<(&str, &str)> = naming_families(&repo_files, &tests)
            .into_iter()
            .map(|(test, file)| (test.as_str(), file.as_str()))
            .collect();

        assert_eq!(
            pairs,
            [
                ("src/button.stories.test.ts", "src/button.ts"),
                ("src/button.test.ts", "src/button.ts")
            ]
        );
    }
}
