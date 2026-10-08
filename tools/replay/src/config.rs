use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

const DEFAULT_PRE_EXISTING_WINDOW_HOURS: u64 = 24;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessConfig {
    pub repository: String,
    pub workflow: String,
    pub base_branch: String,
    #[serde(default)]
    pub test_jobs: Vec<String>,
    #[serde(default)]
    pub junit_artifacts: Vec<String>,
    #[serde(default)]
    pub runner_directory: Option<String>,
    #[serde(default)]
    pub ci_workspace: Option<PathBuf>,
    #[serde(default = "default_pre_existing_window_hours")]
    pub pre_existing_window_hours: u64,
}

impl HarnessConfig {
    pub fn read(config_path: &Path) -> Result<Self> {
        let config_text = fs::read_to_string(config_path)
            .with_context(|| format!("cannot read {}", config_path.display()))?;

        toml::from_str(&config_text)
            .map_err(|error| anyhow!("invalid {}: {}", config_path.display(), error.message()))
    }

    pub fn test_job_matcher(&self) -> Result<NameMatcher> {
        NameMatcher::new(&self.test_jobs)
    }

    pub fn junit_artifact_matcher(&self) -> Result<NameMatcher> {
        NameMatcher::new(&self.junit_artifacts)
    }

    pub fn ci_workspace(&self) -> PathBuf {
        let repository_name = self
            .repository
            .rsplit('/')
            .next()
            .unwrap_or(&self.repository);

        self.ci_workspace.clone().unwrap_or_else(|| {
            PathBuf::from(format!(
                "/home/runner/work/{repository_name}/{repository_name}"
            ))
        })
    }
}

pub struct NameMatcher {
    patterns: GlobSet,
    matches_every_name: bool,
}

impl NameMatcher {
    fn new(patterns: &[String]) -> Result<Self> {
        let mut glob_set_builder = GlobSetBuilder::new();

        for pattern in patterns {
            glob_set_builder
                .add(Glob::new(pattern).with_context(|| format!("invalid pattern {pattern:?}"))?);
        }

        Ok(Self {
            patterns: glob_set_builder.build()?,
            matches_every_name: patterns.is_empty(),
        })
    }

    pub fn matches(&self, name: &str) -> bool {
        self.matches_every_name || self.patterns.is_match(name)
    }
}

fn default_pre_existing_window_hours() -> u64 {
    DEFAULT_PRE_EXISTING_WINDOW_HOURS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(extra_lines: &str) -> HarnessConfig {
        toml::from_str(&format!(
            "repository = \"acme/app\"\nworkflow = \"CI\"\nbase_branch = \"main\"\n{extra_lines}"
        ))
        .unwrap()
    }

    #[test]
    fn test_jobs_are_matched_by_glob_and_every_job_matches_when_none_is_configured() {
        let configured_matcher = config_with("test_jobs = [\"checks-*\", \"e2e\"]\n")
            .test_job_matcher()
            .unwrap();
        let unconfigured_matcher = config_with("").test_job_matcher().unwrap();

        assert!(configured_matcher.matches("checks-node-changed-5"));
        assert!(configured_matcher.matches("e2e"));
        assert!(!configured_matcher.matches("check-lint-core-3"));
        assert!(unconfigured_matcher.matches("anything"));
    }

    #[test]
    fn ci_workspace_defaults_to_the_github_hosted_runner_layout() {
        assert_eq!(
            config_with("").ci_workspace(),
            PathBuf::from("/home/runner/work/app/app")
        );
    }

    #[test]
    fn unknown_config_keys_are_rejected() {
        let config_text =
            "repository = \"acme/app\"\nworkflow = \"CI\"\nbase_branch = \"main\"\ntest_job = []\n";

        assert!(toml::from_str::<HarnessConfig>(config_text).is_err());
    }
}
