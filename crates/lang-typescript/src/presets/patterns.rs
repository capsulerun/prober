//! Test file patterns as runners write them: micromatch globs with extglobs, Jest regular expressions, and the directories they are relative to.

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use prober_core::files::RepoPath;
use regex::RegexSet;

const MAX_EXPANDED_GLOBS: usize = 64;

#[derive(Debug, Clone)]
pub(crate) struct TestPattern {
    include: GlobSet,
    include_regex: Option<RegexSet>,
    exclude: GlobSet,
    exclude_regex: Option<RegexSet>,
    roots: Vec<String>,
}

#[derive(Debug, Default)]
pub(crate) struct TestPatternBuilder {
    include: Vec<String>,
    include_regex: Vec<String>,
    exclude: Vec<String>,
    exclude_regex: Vec<String>,
    roots: Vec<String>,
}

impl TestPatternBuilder {
    pub fn include(mut self, globs: impl IntoIterator<Item = String>) -> Self {
        self.include.extend(globs);

        self
    }

    pub fn include_regex(mut self, expressions: impl IntoIterator<Item = String>) -> Self {
        self.include_regex.extend(expressions);

        self
    }

    pub fn exclude(mut self, globs: impl IntoIterator<Item = String>) -> Self {
        self.exclude.extend(globs);

        self
    }

    pub fn exclude_regex(mut self, expressions: impl IntoIterator<Item = String>) -> Self {
        self.exclude_regex.extend(expressions);

        self
    }

    pub fn roots(mut self, roots: impl IntoIterator<Item = String>) -> Self {
        self.roots.extend(roots);

        self
    }

    pub fn build(self) -> Option<TestPattern> {
        let include_regex = (!self.include_regex.is_empty())
            .then(|| RegexSet::new(&self.include_regex).ok())
            .map_or(Some(None), |regex_set| regex_set.map(Some))?;
        let exclude_regex = (!self.exclude_regex.is_empty())
            .then(|| RegexSet::new(&self.exclude_regex).ok())
            .map_or(Some(None), |regex_set| regex_set.map(Some))?;

        Some(TestPattern {
            include: glob_set(&self.include)?,
            include_regex,
            exclude: glob_set(&self.exclude)?,
            exclude_regex,
            roots: self.roots,
        })
    }
}

impl TestPattern {
    pub fn builder() -> TestPatternBuilder {
        TestPatternBuilder::default()
    }

    pub fn matches(&self, path: &RepoPath) -> bool {
        let path = path.as_str();
        let rooted_path = format!("/{path}");
        let is_under_a_root = self.roots.is_empty()
            || self
                .roots
                .iter()
                .any(|root| root.is_empty() || path.starts_with(&format!("{root}/")));
        let is_included = match &self.include_regex {
            Some(include_regex) => include_regex.is_match(&rooted_path),
            None => self.include.is_match(path),
        };
        let is_excluded = self.exclude.is_match(path)
            || self
                .exclude_regex
                .as_ref()
                .is_some_and(|exclude_regex| exclude_regex.is_match(&rooted_path));

        is_under_a_root && is_included && !is_excluded
    }
}

pub(crate) fn relative_globs(directory: &str, pattern: &str) -> Option<Vec<String>> {
    let (negated, pattern) = match pattern.strip_prefix('!') {
        Some(pattern) => (true, pattern),
        None => (false, pattern),
    };

    if negated {
        return None;
    }

    let pattern = pattern
        .strip_prefix("<rootDir>/")
        .unwrap_or(pattern)
        .trim_start_matches("./");
    let joined = join_directory(directory, pattern)?;

    expand_extglobs(&joined)
}

pub(crate) fn join_directory(directory: &str, path: &str) -> Option<String> {
    let mut segments: Vec<&str> = directory
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();

    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            segment => segments.push(segment),
        }
    }

    Some(segments.join("/"))
}

fn expand_extglobs(pattern: &str) -> Option<Vec<String>> {
    let Some((start, operator)) = pattern.char_indices().find_map(|(offset, character)| {
        let is_extglob = "?*+@!".contains(character) && pattern[offset + 1..].starts_with('(');

        is_extglob.then_some((offset, character))
    }) else {
        return Some(vec![pattern.to_owned()]);
    };
    let group_start = start + 2;
    let group_end = closing_parenthesis(pattern, group_start)?;
    let choices = split_top_level(&pattern[group_start..group_end]);
    let prefix = &pattern[..start];
    let suffix = &pattern[group_end + 1..];
    let alternatives: Vec<&str> = match operator {
        '?' | '*' => std::iter::once("").chain(choices).collect(),
        '+' | '@' => choices,
        _ => return None,
    };
    let mut expanded = Vec::new();

    for alternative in alternatives {
        for rest in expand_extglobs(&format!("{alternative}{suffix}"))? {
            expanded.push(format!("{prefix}{rest}"));

            if expanded.len() > MAX_EXPANDED_GLOBS {
                return None;
            }
        }
    }

    Some(expanded)
}

fn closing_parenthesis(pattern: &str, from: usize) -> Option<usize> {
    let mut depth = 1;

    for (offset, character) in pattern[from..].char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;

                if depth == 0 {
                    return Some(from + offset);
                }
            }
            _ => {}
        }
    }

    None
}

fn split_top_level(group: &str) -> Vec<&str> {
    let mut choices = Vec::new();
    let mut depth = 0;
    let mut choice_start = 0;

    for (offset, character) in group.char_indices() {
        match character {
            '(' | '{' => depth += 1,
            ')' | '}' => depth -= 1,
            '|' if depth == 0 => {
                choices.push(&group[choice_start..offset]);
                choice_start = offset + 1;
            }
            _ => {}
        }
    }

    choices.push(&group[choice_start..]);

    choices
}

fn glob_set(globs: &[String]) -> Option<GlobSet> {
    let mut builder = GlobSetBuilder::new();

    for glob in globs {
        builder.add(
            GlobBuilder::new(glob)
                .literal_separator(true)
                .build()
                .ok()?,
        );
    }

    builder.build().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    fn pattern(directory: &str, include: &[&str], exclude: &[&str]) -> TestPattern {
        let globs = |patterns: &[&str]| -> Vec<String> {
            patterns
                .iter()
                .flat_map(|pattern| relative_globs(directory, pattern).unwrap())
                .collect()
        };

        TestPattern::builder()
            .include(globs(include))
            .exclude(globs(exclude))
            .build()
            .unwrap()
    }

    #[test]
    fn extglobs_expand_into_plain_globs() {
        assert_eq!(
            expand_extglobs("**/*.{test,spec}.?(c|m)[jt]s?(x)").unwrap(),
            [
                "**/*.{test,spec}.[jt]s",
                "**/*.{test,spec}.[jt]sx",
                "**/*.{test,spec}.c[jt]s",
                "**/*.{test,spec}.c[jt]sx",
                "**/*.{test,spec}.m[jt]s",
                "**/*.{test,spec}.m[jt]sx",
            ]
        );
        assert_eq!(
            expand_extglobs("**/?(*.)+(spec|test).js").unwrap(),
            ["**/spec.js", "**/test.js", "**/*.spec.js", "**/*.test.js"]
        );
        assert_eq!(expand_extglobs("**/!(fixtures)/*.ts"), None);
    }

    #[test]
    fn vitest_patterns_are_relative_to_the_project_directory() {
        let unit = pattern(
            "packages/app",
            &["**/*.{test,spec}.?(c|m)[jt]s?(x)", "tests/**/*.ts"],
            &["**/node_modules/**", "**/e2e/**"],
        );

        assert!(unit.matches(&repo_path("packages/app/src/a.test.ts")));
        assert!(unit.matches(&repo_path("packages/app/tests/smoke.ts")));
        assert!(unit.matches(&repo_path("packages/app/a.spec.mjs")));
        assert!(!unit.matches(&repo_path("packages/app/src/e2e/flow.test.ts")));
        assert!(!unit.matches(&repo_path("packages/other/src/a.test.ts")));
        assert!(!unit.matches(&repo_path("packages/app/src/a.ts")));
    }

    #[test]
    fn jest_regular_expressions_match_rooted_paths_under_its_roots() {
        let jest = TestPattern::builder()
            .include_regex(["(/__tests__/.*|(\\.|/)(test|spec))\\.[jt]sx?$".to_owned()])
            .exclude_regex(["/node_modules/".to_owned(), "/fixtures/".to_owned()])
            .roots(["src".to_owned()])
            .build()
            .unwrap();

        assert!(jest.matches(&repo_path("src/__tests__/helpers.ts")));
        assert!(jest.matches(&repo_path("src/a.test.tsx")));
        assert!(!jest.matches(&repo_path("src/fixtures/a.test.ts")));
        assert!(!jest.matches(&repo_path("lib/a.test.ts")));
    }

    #[test]
    fn negated_and_escaping_patterns_are_not_test_patterns() {
        assert_eq!(relative_globs("", "!**/*.d.ts"), None);
        assert_eq!(relative_globs("", "../outside/*.ts"), None);
        assert_eq!(
            relative_globs("app", "<rootDir>/src/**/*.test.js"),
            Some(vec!["app/src/**/*.test.js".to_owned()])
        );
    }
}
