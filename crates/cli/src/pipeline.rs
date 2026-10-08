//! The selection pipeline: config, changes, global files, graph, selection, and the fallback to every test on error.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use prober_core::changes::FileChange;
use prober_core::config::{AlwaysAll, Config, config_hash};
use prober_core::files::RepoPath;
use prober_core::graph::{Graph, GraphBuilder};
use prober_core::report::{ModeReason, Report};
use prober_core::select::select_tests;
use prober_lang_typescript::{ALWAYS_ALL_DEFAULTS, LANGUAGE, TypeScriptLinker, is_test_file};
use prober_vcs::GitRepository;

const CONFIG_FILE_NAME: &str = "prober.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OnError {
    All,
    Fail,
}

pub struct Revisions {
    pub base: String,
    pub head: String,
}

pub struct SelectionRun {
    pub report: Report,
    pub warnings: Vec<String>,
}

pub fn run(directory: &Path, revisions: &Revisions, on_error: OnError) -> Result<SelectionRun> {
    let workspace = Workspace::open(directory, &revisions.head)?;
    let config_text = workspace.read_config();
    let config_hash = config_hash(config_text.as_deref().unwrap_or_default());

    match workspace.select(config_text, revisions, config_hash.clone()) {
        Ok(selection_run) => Ok(selection_run),
        Err(error) if on_error == OnError::All => {
            let error_reason = ModeReason::Error {
                message: format!("{error:#}"),
            };

            Ok(SelectionRun {
                report: workspace.run_all(vec![error_reason], config_hash),
                warnings: Vec::new(),
            })
        }
        Err(error) => Err(error),
    }
}

struct Workspace {
    repository: GitRepository,
    repo_root: PathBuf,
    repo_files: Vec<RepoPath>,
}

impl Workspace {
    fn open(directory: &Path, head: &str) -> Result<Self> {
        let repository = GitRepository::discover(directory)?;
        let repo_root = repository.work_dir()?.to_path_buf();
        let repo_files = repository.files_at(head)?;

        Ok(Self {
            repository,
            repo_root,
            repo_files,
        })
    }

    fn read_config(&self) -> Result<String> {
        match fs::read_to_string(self.repo_root.join(CONFIG_FILE_NAME)) {
            Ok(config_text) => Ok(config_text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error).context("cannot read prober.toml"),
        }
    }

    fn select(
        &self,
        config_text: Result<String>,
        revisions: &Revisions,
        config_hash: String,
    ) -> Result<SelectionRun> {
        let config = Config::parse(&config_text?)?;
        let changes = self.changes(revisions)?;
        let resolved_revisions = self
            .repository
            .resolve_revisions(&revisions.base, &revisions.head)?;
        let always_all_reasons = always_all_reasons(&config, &changes)?;

        if !always_all_reasons.is_empty() {
            let report = self
                .run_all(always_all_reasons, config_hash)
                .with_revisions(resolved_revisions);

            return Ok(SelectionRun {
                report,
                warnings: Vec::new(),
            });
        }

        let linker = TypeScriptLinker::new(&self.repo_root)?;
        let graph = self.link_graph(&linker)?;
        let selected_tests = select_tests(&graph, &changes);
        let report = Report::from_selection(&graph, &changes, &selected_tests, config_hash)
            .with_revisions(resolved_revisions);

        Ok(SelectionRun {
            report,
            warnings: linker.missing_dependencies_warning().into_iter().collect(),
        })
    }

    fn changes(&self, revisions: &Revisions) -> Result<Vec<FileChange>> {
        if !self.repository.is_checked_out(&revisions.head)? {
            bail!(
                "--head {:?} is not the checked-out commit: Prober reads files from the working tree, check it out first",
                revisions.head
            );
        }

        self.repository
            .changes_between(&revisions.base, &revisions.head)
    }

    fn link_graph(&self, linker: &TypeScriptLinker) -> Result<Graph> {
        let mut builder = GraphBuilder::default();

        linker.link(&self.repo_files, &mut builder)?;

        Ok(builder.build())
    }

    fn run_all(&self, reasons: Vec<ModeReason>, config_hash: String) -> Report {
        let mut builder = GraphBuilder::default();

        for test_file in self.repo_files.iter().filter(|path| is_test_file(path)) {
            builder.add_test_file(test_file.clone(), LANGUAGE);
        }

        Report::all(&builder.build(), reasons, config_hash)
    }
}

fn always_all_reasons(config: &Config, changes: &[FileChange]) -> Result<Vec<ModeReason>> {
    let built_in_patterns = ALWAYS_ALL_DEFAULTS
        .iter()
        .map(|&pattern| pattern.to_owned())
        .collect();
    let built_in_always_all = AlwaysAll::new(built_in_patterns)?;
    let mut always_all_matches = config.always_all.matching_changes(changes);

    always_all_matches.extend(built_in_always_all.matching_changes(changes));
    always_all_matches.sort_by(|left, right| left.change.path.cmp(&right.change.path));
    always_all_matches.dedup_by(|later, earlier| later.change.path == earlier.change.path);

    Ok(always_all_matches
        .into_iter()
        .map(ModeReason::from)
        .collect())
}
