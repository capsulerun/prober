//! Path globs of `prober.toml` rules, with `{name}` captures matching one path segment or part of it.

use anyhow::{Result, anyhow, bail};
use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;

#[derive(Debug, Clone)]
pub struct PathPattern {
    text: String,
    matcher: Matcher,
}

#[derive(Debug, Clone)]
enum Matcher {
    Glob(GlobMatcher),
    Captures { regex: Regex, names: Vec<String> },
}

pub type Captures = Vec<(String, String)>;

impl PathPattern {
    pub fn new(text: &str) -> Result<Self> {
        let names = capture_names(text);

        if names.is_empty() {
            let glob = GlobBuilder::new(text)
                .literal_separator(true)
                .build()
                .map_err(|glob_error| anyhow!("invalid pattern {text:?}: {}", glob_error.kind()))?;

            return Ok(Self {
                text: text.to_owned(),
                matcher: Matcher::Glob(glob.compile_matcher()),
            });
        }

        let regex = Regex::new(&format!("^{}$", glob_regex(text, true)?))
            .map_err(|regex_error| anyhow!("invalid pattern {text:?}: {regex_error}"))?;

        Ok(Self {
            text: text.to_owned(),
            matcher: Matcher::Captures { regex, names },
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn capture_names(&self) -> &[String] {
        match &self.matcher {
            Matcher::Glob(_) => &[],
            Matcher::Captures { names, .. } => names,
        }
    }

    pub fn is_match(&self, path: &str) -> bool {
        match &self.matcher {
            Matcher::Glob(glob) => glob.is_match(path),
            Matcher::Captures { regex, .. } => regex.is_match(path),
        }
    }

    pub fn captures(&self, path: &str) -> Option<Captures> {
        match &self.matcher {
            Matcher::Glob(glob) => glob.is_match(path).then(Vec::new),
            Matcher::Captures { regex, names } => {
                let found = regex.captures(path)?;

                Some(
                    names
                        .iter()
                        .map(|name| (name.clone(), found[name.as_str()].to_owned()))
                        .collect(),
                )
            }
        }
    }

    pub fn with_captures(&self, captures: &Captures) -> Result<Self> {
        let mut text = self.text.clone();

        for (name, value) in captures {
            text = text.replace(&format!("{{{name}}}"), &escape_glob(value));
        }

        Self::new(&text)
    }
}

fn capture_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = text;

    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            break;
        };
        let name = &after[..end];

        if is_capture_name(name) && !names.iter().any(|known| known == name) {
            names.push(name.to_owned());
        }

        rest = &after[end + 1..];
    }

    names
}

fn is_capture_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn glob_regex(text: &str, allows_captures: bool) -> Result<String> {
    let characters: Vec<char> = text.chars().collect();
    let mut regex = String::new();
    let mut position = 0;
    let mut seen_names: Vec<String> = Vec::new();

    while position < characters.len() {
        match characters[position] {
            '*' if characters.get(position + 1) == Some(&'*') => {
                let starts_segment = position == 0 || characters[position - 1] == '/';
                let ends_segment = characters.get(position + 2).is_none_or(|&next| next == '/');

                if !starts_segment || !ends_segment {
                    regex.push_str("[^/]*");
                    position += 2;
                } else if characters.get(position + 2) == Some(&'/') {
                    regex.push_str("(?:[^/]*/)*");
                    position += 3;
                } else {
                    regex.push_str(".*");
                    position += 2;
                }
            }
            '*' => {
                regex.push_str("[^/]*");
                position += 1;
            }
            '?' => {
                regex.push_str("[^/]");
                position += 1;
            }
            '[' => {
                let end = characters[position + 1..]
                    .iter()
                    .position(|&character| character == ']')
                    .map(|offset| position + 1 + offset)
                    .ok_or_else(|| anyhow!("invalid pattern {text:?}: unclosed character class"))?;
                let class: String = characters[position + 1..end].iter().collect();
                let class = class
                    .strip_prefix('!')
                    .map_or(class.clone(), |negated| format!("^{negated}"));

                regex.push('[');
                regex.push_str(&class.replace('\\', "\\\\"));
                regex.push(']');
                position = end + 1;
            }
            '{' => {
                let end = characters[position + 1..]
                    .iter()
                    .position(|&character| character == '}')
                    .map(|offset| position + 1 + offset)
                    .ok_or_else(|| anyhow!("invalid pattern {text:?}: unclosed `{{`"))?;
                let group: String = characters[position + 1..end].iter().collect();

                if is_capture_name(&group) {
                    if !allows_captures {
                        bail!("invalid pattern {text:?}: captures cannot appear inside `{{…}}`");
                    }

                    if seen_names.contains(&group) {
                        bail!("invalid pattern {text:?}: capture {{{group}}} appears twice");
                    }

                    regex.push_str(&format!("(?P<{group}>[^/]+)"));
                    seen_names.push(group);
                } else {
                    let alternatives = group
                        .split(',')
                        .map(|alternative| glob_regex(alternative, false))
                        .collect::<Result<Vec<String>>>()?;

                    regex.push_str(&format!("(?:{})", alternatives.join("|")));
                }

                position = end + 1;
            }
            character => {
                regex.push_str(&regex::escape(&character.to_string()));
                position += 1;
            }
        }
    }

    Ok(regex)
}

fn escape_glob(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '*' | '?' | '[' | ']' | '{' | '}' => format!("[{character}]"),
            character => character.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn captured(pattern: &str, path: &str) -> Option<Vec<(String, String)>> {
        PathPattern::new(pattern).unwrap().captures(path)
    }

    #[test]
    fn plain_patterns_are_globs_whose_stars_stay_within_a_directory() {
        let pattern = PathPattern::new("fixtures/*.json").unwrap();

        assert!(pattern.is_match("fixtures/a.json"));
        assert!(!pattern.is_match("fixtures/nested/a.json"));
        assert_eq!(pattern.captures("fixtures/a.json"), Some(Vec::new()));
        assert!(
            PathPattern::new("fixtures/**")
                .unwrap()
                .is_match("fixtures/nested/a.json")
        );
    }

    #[test]
    fn captures_take_one_segment_or_part_of_one() {
        assert_eq!(
            captured("extensions/{id}/**", "extensions/telegram/src/send.ts"),
            Some(vec![("id".to_owned(), "telegram".to_owned())])
        );
        assert_eq!(
            captured("test/{id}.test.ts", "test/discord.test.ts"),
            Some(vec![("id".to_owned(), "discord".to_owned())])
        );
        assert_eq!(captured("extensions/{id}/**", "extensions/telegram"), None);
        assert_eq!(
            captured("**/{name}.{ts,tsx}", "src/ui/button.tsx"),
            Some(vec![("name".to_owned(), "button".to_owned())])
        );
    }

    #[test]
    fn captured_values_fill_the_other_side_of_a_link() {
        let to = PathPattern::new("test/plugins/{id}.test.ts").unwrap();
        let instantiated = to
            .with_captures(&vec![("id".to_owned(), "telegram".to_owned())])
            .unwrap();

        assert_eq!(instantiated.text(), "test/plugins/telegram.test.ts");
        assert!(instantiated.is_match("test/plugins/telegram.test.ts"));
        assert_eq!(to.capture_names(), ["id"]);
    }

    #[test]
    fn malformed_patterns_are_errors() {
        for pattern in ["src/[a", "src/{id", "{id}/{id}.ts"] {
            assert!(PathPattern::new(pattern).is_err(), "{pattern}");
        }
    }
}
