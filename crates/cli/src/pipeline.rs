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
use prober_core::frontend::{ChangeSet, Frontend};
use prober_core::graph::{Graph, GraphBuilder};
use prober_core::report::{
    AlwaysRunTest, CONFIG_FILE_NAME, ModeReason, Report, SELECTION_VARIABLE,
};
use prober_core::rules::ConfigWarning;
use prober_core::scan::ScanReport;
use prober_core::select::{Widening, select_tests};
use prober_core::unhandled::unhandled_changes;
use prober_lang_python::PythonLinker;
use prober_lang_typescript::TypeScriptLinker;
use prober_vcs::GitRepository;

const FORCED_SELECTION: &str = "all";

type TestFileNames = (&'static str, fn(&RepoPath) -> bool);

const TEST_FILE_NAMES: [TestFileNames; 2] = [
    (
        prober_lang_typescript::LANGUAGE,
        prober_lang_typescript::is_test_file,
    ),
    (
        prober_lang_python::LANGUAGE,
        prober_lang_python::is_test_file,
    ),
];

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
    let frontends = workspace.frontends(&config)?;
    let change_set = ChangeSet::unchanged(&workspace.repo_files);
    let mut builder = GraphBuilder::default();

    for frontend in &frontends {
        frontend.add_links(&change_set, &mut builder)?;
    }

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
        let mut frontends = self.frontends(&config)?;
        let merge_base = MergeBase {
            repository: &self.repository,
            commit: &resolved_revisions.merge_base,
        };
        let change_set = ChangeSet {
            repo_files: &self.repo_files,
            changes: &changes,
            base: &merge_base,
        };
        let built_in_patterns: Vec<&str> = frontends
            .iter()
            .flat_map(|frontend| frontend.always_all_patterns())
            .collect();
        let mut global_reasons = always_all_reasons(&config, &changes, &built_in_patterns)?;

        for frontend in &mut frontends {
            global_reasons.extend(frontend.diff_manifests(&change_set)?);
        }

        if !global_reasons.is_empty() {
            let report = self
                .run_all(Some(&frontends), global_reasons, config_hash)
                .with_revisions(resolved_revisions)
                .with_unhandled(unhandled_changes(None, &changes), changes.len());

            return Ok(Selection {
                report,
                warnings,
                analysis: None,
            });
        }

        let mut builder = GraphBuilder::default();
        let mut root_scope_reasons = Vec::new();

        for frontend in &frontends {
            root_scope_reasons.extend(frontend.add_links(&change_set, &mut builder)?);
        }

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

    fn frontends(&self, config: &Config) -> Result<Vec<Box<dyn Frontend>>> {
        let mut frontends: Vec<Box<dyn Frontend>> = Vec::new();

        if self.holds(prober_lang_typescript::is_source_file) {
            frontends.push(Box::new(self.typescript_linker(config)?));
        }

        if self.holds(prober_lang_python::is_source_file) {
            let linker = PythonLinker::new(&self.repo_root, &self.repo_files)?
                .with_inference(config.inference)
                .with_presets(&config.presets);

            frontends.push(Box::new(linker));
        }

        Ok(frontends)
    }

    fn holds(&self, is_source_file: fn(&RepoPath) -> bool) -> bool {
        self.repo_files.iter().any(is_source_file)
    }

    fn typescript_linker(&self, config: &Config) -> Result<TypeScriptLinker> {
        let mut linker = TypeScriptLinker::new(&self.repo_root, &self.repo_files)?
            .with_inference(config.inference);

        if config.inference.symbols {
            linker = linker.with_symbol_analysis();
        }

        if config.presets == Presets::Auto {
            return Ok(linker);
        }

        Ok(linker.with_presets(&config.presets, &self.repo_files))
    }

    fn run_all(
        &self,
        frontends: Option<&[Box<dyn Frontend>]>,
        reasons: Vec<ModeReason>,
        config_hash: String,
    ) -> Report {
        let default_frontends = frontends
            .is_none()
            .then(|| {
                let config = Config::parse("").ok()?;

                self.frontends(&config).ok()
            })
            .flatten();
        let mut builder = GraphBuilder::default();

        match frontends.or(default_frontends.as_deref()) {
            Some(frontends) => {
                for frontend in frontends {
                    for test_file in frontend.test_files(&self.repo_files) {
                        builder.add_test_file(test_file, frontend.language());
                    }
                }
            }
            None => {
                for (language, is_test_file) in TEST_FILE_NAMES {
                    for test_file in self.repo_files.iter().filter(|path| is_test_file(path)) {
                        builder.add_test_file(test_file.clone(), language);
                    }
                }
            }
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
    built_in_patterns: &[&str],
) -> Result<Vec<ModeReason>> {
    let built_in_patterns = built_in_patterns
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
