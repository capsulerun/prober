//! Naming families: tests paired with the modules they are named after, beside them or in a mirrored package, the modules of the directory their test directory mirrors, and the snapshots and fixtures named after them.

use std::collections::{BTreeSet, HashMap};

use prober_core::files::RepoPath;

use crate::conventions::{
    FIXTURE_DIRECTORIES, PACKAGE_INIT, SNAPSHOT_DIRECTORIES, is_module_file, join_directory,
    parent_directory, split_path,
};

const TEST_DIRECTORIES: [&str; 3] = ["test", "tests", "testing"];

pub(crate) fn naming_families<'f>(
    repo_files: &'f [RepoPath],
    tests: &'f BTreeSet<RepoPath>,
    top_level_packages: &[String],
) -> Vec<(&'f RepoPath, &'f RepoPath)> {
    let mut modules_by_directory: HashMap<&str, Vec<&RepoPath>> = HashMap::new();
    let mut named_files: HashMap<(&str, &str), Vec<&RepoPath>> = HashMap::new();

    for file in repo_files.iter().filter(|file| !tests.contains(*file)) {
        let (directory, name) = split_path(file.as_str());

        if is_module_file(file) {
            modules_by_directory
                .entry(directory)
                .or_default()
                .push(file);
        }

        let (parent, directory_name) = split_path(directory);

        if SNAPSHOT_DIRECTORIES.contains(&directory_name)
            || FIXTURE_DIRECTORIES.contains(&directory_name)
        {
            let stem = name.split_once('.').map_or(name, |(stem, _)| stem);

            named_files.entry((parent, stem)).or_default().push(file);
        }
    }

    let mut pairs = Vec::new();

    for test in tests {
        let (directory, name) = split_path(test.as_str());
        let test_stem = name.strip_suffix(".py").unwrap_or(name);
        let stem = test_stem
            .strip_prefix("test_")
            .or_else(|| test_stem.strip_suffix("_test"))
            .unwrap_or(test_stem);

        for &file in named_files
            .get(&(directory, test_stem))
            .into_iter()
            .flatten()
        {
            pairs.push((test, file));
        }

        for (mirrored, is_mirror) in family_directories(directory, top_level_packages) {
            for &module in modules_by_directory
                .get(mirrored.as_str())
                .into_iter()
                .flatten()
            {
                let is_named = split_path(module.as_str()).1.strip_suffix(".py") == Some(stem);

                if is_named || is_mirror {
                    pairs.push((test, module));
                }
            }

            let package = join_directory(&join_directory(&mirrored, stem), PACKAGE_INIT);

            if let Some(module) = modules_by_directory
                .get(parent_directory(&package))
                .into_iter()
                .flatten()
                .find(|module| module.as_str() == package)
            {
                pairs.push((test, module));
            }
        }
    }

    pairs.sort();
    pairs.dedup();

    pairs
}

fn family_directories(test_directory: &str, top_level_packages: &[String]) -> Vec<(String, bool)> {
    let segments: Vec<&str> = test_directory
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let mut directories = vec![(test_directory.to_owned(), false)];

    for (position, segment) in segments.iter().enumerate() {
        if !TEST_DIRECTORIES.contains(segment) {
            continue;
        }

        let before = segments[..position].join("/");
        let after = segments[position + 1..].join("/");

        let mirrors_a_directory = !after.is_empty();

        directories.push((join_directory(&before, &after), mirrors_a_directory));

        for package in top_level_packages {
            let within = before.is_empty() || package.starts_with(&format!("{before}/"));

            if within && package.as_str() != test_directory {
                directories.push((join_directory(package, &after), mirrors_a_directory));
            }
        }
    }

    directories.retain(|(directory, is_mirror)| !*is_mirror || directory != test_directory);
    directories.sort();
    directories.dedup();

    directories
}

#[cfg(test)]
mod tests {
    use super::*;

    fn families(files: &[&str], tests: &[&str], packages: &[&str]) -> Vec<(String, String)> {
        let repo_files: Vec<RepoPath> = files
            .iter()
            .chain(tests)
            .map(|path| RepoPath::new(*path).unwrap())
            .collect();
        let tests: BTreeSet<RepoPath> = tests
            .iter()
            .map(|path| RepoPath::new(*path).unwrap())
            .collect();
        let packages: Vec<String> = packages
            .iter()
            .map(|package| (*package).to_owned())
            .collect();

        naming_families(&repo_files, &tests, &packages)
            .into_iter()
            .map(|(test, file)| (test.to_string(), file.to_string()))
            .collect()
    }

    #[test]
    fn a_test_is_paired_with_its_module_beside_it_or_in_the_mirrored_package() {
        let pairs = families(
            &["app/api.py", "app/models.py", "tools/api.py"],
            &["tests/test_api.py", "tools/test_api.py"],
            &["app"],
        );

        assert!(pairs.contains(&("tests/test_api.py".to_owned(), "app/api.py".to_owned())));
        assert!(pairs.contains(&("tools/test_api.py".to_owned(), "tools/api.py".to_owned())));
        assert!(!pairs.contains(&("tools/test_api.py".to_owned(), "app/api.py".to_owned())));
    }

    #[test]
    fn a_mirrored_test_directory_pairs_its_tests_with_every_module_of_the_mirrored_directory() {
        let pairs = families(
            &[
                "app/components/light/__init__.py",
                "app/components/light/sensor.py",
                "app/components/fan/__init__.py",
            ],
            &["tests/components/light/test_init.py"],
            &["app", "tests"],
        );

        assert_eq!(
            pairs,
            [
                (
                    "tests/components/light/test_init.py".to_owned(),
                    "app/components/light/__init__.py".to_owned()
                ),
                (
                    "tests/components/light/test_init.py".to_owned(),
                    "app/components/light/sensor.py".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn snapshots_and_fixtures_named_after_a_test_belong_to_it() {
        let pairs = families(
            &[
                "tests/snapshots/test_api.ambr",
                "tests/fixtures/test_api.json",
                "tests/fixtures/other.json",
            ],
            &["tests/test_api.py"],
            &[],
        );

        assert_eq!(
            pairs,
            [
                (
                    "tests/test_api.py".to_owned(),
                    "tests/fixtures/test_api.json".to_owned()
                ),
                (
                    "tests/test_api.py".to_owned(),
                    "tests/snapshots/test_api.ambr".to_owned()
                ),
            ]
        );
    }
}
