//! The selection pipeline: kill switch, config, changes, global files and manifests, graph and config rules, selection, and the fallback to every test on error.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use prober_core::changes::{BaseRevision, FileChange};
use prober_core::config::{AlwaysAll, Config, Presets, config_hash};
use prober_core::files::RepoPath;
use prober_core::graph::{Graph, GraphBuilder};
use prober_core::report::{
    AlwaysRunTest, CONFIG_FILE_NAME, ModeReason, Report, SELECTION_VARIABLE,
};
use prober_core::rules::ConfigWarning;
use prober_core::scan::ScanReport;
use prober_core::select::{Widening, select_tests};
use prober_core::unhandled::unhandled_changes;
use prober_lang_typescript::{
    ALWAYS_ALL_DEFAULTS, LANGUAGE, LinkChanges, MANIFEST_DEFAULTS, ManifestChanges,
    TypeScriptLinker, is_test_file,
};
use prober_vcs::GitRepository;

const FORCED_SELECTION: &str = "all";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OnError {
    All,
    Fail,
}

pub struct Revisions {
    pub base: String,
    pub head: String,
}

pub struct Selection {
    pub report: Report,
    pub warnings: Vec<ConfigWarning>,
    pub analysis: Option<GraphAnalysis>,
}

pub struct GraphAnalysis {
    pub graph: Graph,
    pub changes: Vec<FileChange>,
    pub widening: Widening,
    pub always_run: Vec<AlwaysRunTest>,
}

impl Selection {
    fn of(report: Report) -> Self {
        Self {
            report,
            warnings: Vec::new(),
            analysis: None,
        }
    }
}

pub fn run(directory: &Path, revisions: &Revisions, on_error: OnError) -> Result<Selection> {
    let workspace = Workspace::open(directory, &revisions.head)?;
    let config_text = workspace.read_config();
    let config_hash = config_hash(config_text.as_deref().unwrap_or_default());
    let forced = forced_selection();

    if let Ok(true) = forced {
        let report = workspace.run_all(None, vec![ModeReason::Forced], config_hash);
        let report = match workspace
            .repository
            .resolve_revisions(&revisions.base, &revisions.head)
        {
            Ok(resolved_revisions) => report.with_revisions(resolved_revisions),
            Err(_) => report,
        };

        return Ok(Selection::of(report));
    }

    let selection =
        forced.and_then(|_| workspace.select(config_text, revisions, config_hash.clone()));

    match selection {
        Ok(selection) => Ok(selection),
        Err(error) if on_error == OnError::All => {
            let error_reason = ModeReason::Error {
                message: format!("{error:#}"),
            };

            Ok(Selection::of(workspace.run_all(
                None,
                vec![error_reason],
                config_hash,
            )))
        }
        Err(error) => Err(error),
    }
}

pub fn scan(directory: &Path) -> Result<ScanReport> {
    let workspace = Workspace::open(directory, "HEAD")?;
    let config_text = workspace.read_config()?;
    let config = Config::parse(&config_text)?;
    let linker = workspace.linker(&config)?.with_symbol_analysis();
    let mut builder = GraphBuilder::default();

    linker.link(&workspace.repo_files, &mut builder)?;

    let rule_report = config.rules.apply(&mut builder, &workspace.repo_files, &[]);
    let graph = builder.build();

    Ok(
        ScanReport::new(&graph, &workspace.repo_files, config_hash(&config_text)).with_config(
            rule_report,
            config.always_all.warnings(&workspace.repo_files),
        ),
    )
}

fn forced_selection() -> Result<bool> {
    match env::var(SELECTION_VARIABLE) {
        Ok(value) if value == FORCED_SELECTION => Ok(true),
        Ok(value) if value.is_empty() => Ok(false),
        Ok(value) => {
            bail!("{SELECTION_VARIABLE} must be \"{FORCED_SELECTION}\" or unset, not {value:?}")
        }
        Err(_) => Ok(false),
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
    ) -> Result<Selection> {
        let config = Config::parse(&config_text?)?;
        let mut warnings = config.always_all.warnings(&self.repo_files);
        let changes = self.changes(revisions)?;
        let resolved_revisions = self
            .repository
            .resolve_revisions(&revisions.base, &revisions.head)?;
        let linker = self.linker(&config)?;
        let mut global_reasons =
            always_all_reasons(&config, &changes, &linker.global_config_patterns())?;
        let merge_base = MergeBase {
            repository: &self.repository,
            commit: &resolved_revisions.merge_base,
        };
        let manifest_changes = if config.inference.manifests {
            ManifestChanges::between(&self.repo_root, &self.repo_files, &changes, &merge_base)?
        } else {
            ManifestChanges::default()
        };

        global_reasons.extend_from_slice(manifest_changes.global_reasons());

        if !global_reasons.is_empty() {
            let report = self
                .run_all(Some(&linker), global_reasons, config_hash)
                .with_revisions(resolved_revisions)
                .with_unhandled(unhandled_changes(None, &changes), changes.len());

            return Ok(Selection {
                report,
                warnings,
                analysis: None,
            });
        }

        let link_changes = LinkChanges {
            changes: &changes,
            manifests: &manifest_changes,
            base: &merge_base,
        };
        let mut builder = GraphBuilder::default();
        let root_scope_reasons =
            linker.link_changes(&self.repo_files, &link_changes, &mut builder)?;
        let rule_report = config.rules.apply(&mut builder, &self.repo_files, &changes);

        warnings.extend(rule_report.warnings);

        let graph = builder.build();
        let unhandled = unhandled_changes(Some(&graph), &changes);
        let widening = if config.inference.widening {
            Widening::Scoped
        } else {
            Widening::Off
        };
        let always_run = config.rules.always_run_tests(&graph);
        let report = if root_scope_reasons.is_empty() {
            let selected_tests = select_tests(&graph, &changes, widening);

            Report::from_selection(&graph, &changes, &selected_tests, config_hash)
                .with_always_run(always_run.clone())
        } else {
            Report::all(&graph, root_scope_reasons, config_hash)
        };
        let report = report
            .with_revisions(resolved_revisions)
            .with_unhandled(unhandled, changes.len());

        Ok(Selection {
            report,
            warnings,
            analysis: Some(GraphAnalysis {
                graph,
                changes,
                widening,
                always_run,
            }),
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

    fn linker(&self, config: &Config) -> Result<TypeScriptLinker> {
        let linker = TypeScriptLinker::new(&self.repo_root, &self.repo_files)?
            .with_inference(config.inference);

        if config.presets == Presets::Auto {
            return Ok(linker);
        }

        Ok(linker.with_presets(&config.presets, &self.repo_files))
    }

    fn run_all(
        &self,
        linker: Option<&TypeScriptLinker>,
        reasons: Vec<ModeReason>,
        config_hash: String,
    ) -> Report {
        let default_linker = linker
            .is_none()
            .then(|| TypeScriptLinker::new(&self.repo_root, &self.repo_files).ok())
            .flatten();
        let test_files = match linker.or(default_linker.as_ref()) {
            Some(linker) => linker.test_files(&self.repo_files),
            None => self
                .repo_files
                .iter()
                .filter(|path| is_test_file(path))
                .cloned()
                .collect(),
        };
        let mut builder = GraphBuilder::default();

        for test_file in test_files {
            builder.add_test_file(test_file, LANGUAGE);
        }

        Report::all(&builder.build(), reasons, config_hash)
    }
}

struct MergeBase<'w> {
    repository: &'w GitRepository,
    commit: &'w str,
}

impl BaseRevision for MergeBase<'_> {
    fn read_file(&self, path: &RepoPath) -> Result<Option<Vec<u8>>> {
        self.repository.read_file(self.commit, path)
    }
}

fn always_all_reasons(
    config: &Config,
    changes: &[FileChange],
    runner_config_patterns: &[&str],
) -> Result<Vec<ModeReason>> {
    let manifest_patterns = if config.inference.manifests {
        &[][..]
    } else {
        MANIFEST_DEFAULTS
    };
    let built_in_patterns = ALWAYS_ALL_DEFAULTS
        .iter()
        .chain(manifest_patterns)
        .chain(runner_config_patterns)
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
