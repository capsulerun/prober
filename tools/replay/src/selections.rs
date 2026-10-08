use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::changes::{Change, last_commits, read_changes};
use crate::record::{RecordWriter, ReplayRecord, recorded_ids};
use crate::worktree::Worktree;

const PROBER_CONFIG_FILE_NAME: &str = "prober.toml";

#[derive(Debug, Args)]
pub struct SelectionsArguments {
    #[arg(long, help = "Repository whose history is replayed")]
    repo: PathBuf,

    #[arg(long, help = "JSON lines file the records are appended to")]
    output: PathBuf,

    #[arg(
        long,
        conflicts_with = "changes",
        required_unless_present = "changes",
        help = "Replay the last N commits of --branch, each against its first parent"
    )]
    last: Option<usize>,

    #[arg(
        long,
        default_value = "HEAD",
        help = "Branch whose last commits are replayed"
    )]
    branch: String,

    #[arg(
        long,
        help = "JSON lines file of changes to replay, one {\"id\", \"base\", \"head\"} per line"
    )]
    changes: Option<PathBuf>,

    #[arg(long, default_value = "prober", help = "prober binary to run")]
    prober: PathBuf,

    #[arg(
        long,
        help = "Worktree the changes are checked out in, kept and reused across runs [default: prober-replay-<repo> in the temporary directory]"
    )]
    worktree: Option<PathBuf>,

    #[arg(
        long,
        help = "Shell command run once when the worktree is created, e.g. to install dependencies"
    )]
    setup: Option<String>,

    #[arg(
        long,
        help = "Fetch heads missing from the repository from its origin remote"
    )]
    fetch: bool,

    #[arg(
        long,
        help = "prober.toml placed in the worktree for every change, e.g. to switch an inference off"
    )]
    prober_config: Option<PathBuf>,
}

struct ReplaySettings<'a> {
    prober: &'a Path,
    prober_config: Option<String>,
    fetch_missing_heads: bool,
}

pub fn run(arguments: &SelectionsArguments) -> Result<String> {
    let changes = match (&arguments.changes, arguments.last) {
        (Some(changes_path), _) => read_changes(changes_path)?,
        (None, Some(count)) => last_commits(&arguments.repo, &arguments.branch, count)?,
        (None, None) => bail!("either --last or --changes is required"),
    };
    let already_recorded_ids = recorded_ids(&arguments.output)?;
    let pending_changes: Vec<&Change> = changes
        .iter()
        .filter(|change| !already_recorded_ids.contains(&change.id))
        .collect();
    let already_recorded_count = changes.len() - pending_changes.len();

    if pending_changes.is_empty() {
        return Ok(format!(
            "prober-replay: nothing to replay, {already_recorded_count} changes already recorded"
        ));
    }

    let worktree_path = arguments
        .worktree
        .clone()
        .unwrap_or_else(|| default_worktree_path(&arguments.repo));
    let prober_config = arguments
        .prober_config
        .as_ref()
        .map(|config_path| {
            fs::read_to_string(config_path)
                .with_context(|| format!("cannot read {}", config_path.display()))
        })
        .transpose()?;
    let replay_settings = ReplaySettings {
        prober: &arguments.prober,
        prober_config,
        fetch_missing_heads: arguments.fetch,
    };
    let worktree = Worktree::open(&arguments.repo, &worktree_path, arguments.setup.as_deref())?;
    let mut record_writer = RecordWriter::append_to(&arguments.output)?;
    let mut records = Vec::new();

    for change in pending_changes {
        let record = replay_change(&worktree, &replay_settings, change);

        record_writer.write(&record)?;
        records.push(record);
    }

    Ok(summary_line(&records, already_recorded_count))
}

fn replay_change(worktree: &Worktree, settings: &ReplaySettings, change: &Change) -> ReplayRecord {
    if settings.fetch_missing_heads
        && !worktree.has_commit(&change.head)
        && let Err(error) = worktree.fetch(&change.head)
    {
        return ReplayRecord::failed(change, format!("{error:#}"));
    }

    if let Err(error) = worktree.check_out(&change.head) {
        return ReplayRecord::failed(change, format!("{error:#}"));
    }

    let config_path = worktree.path().join(PROBER_CONFIG_FILE_NAME);

    if let Some(prober_config) = &settings.prober_config {
        if config_path.exists() {
            return ReplayRecord::failed(
                change,
                "the repository has its own prober.toml, which --prober-config would replace"
                    .to_owned(),
            );
        }

        if let Err(error) = fs::write(&config_path, prober_config) {
            return ReplayRecord::failed(change, format!("cannot write prober.toml: {error}"));
        }
    }

    let started = Instant::now();
    let prober_output = Command::new(settings.prober)
        .args(["--base", &change.base, "--format", "json"])
        .current_dir(worktree.path())
        .env_remove("GITHUB_STEP_SUMMARY")
        .output();
    let elapsed = started.elapsed();

    if settings.prober_config.is_some() {
        let _ = fs::remove_file(&config_path);
    }

    match prober_output {
        Ok(prober_output) => ReplayRecord::completed(change, elapsed, &prober_output),
        Err(error) => ReplayRecord::failed(
            change,
            format!("cannot run {}: {error}", settings.prober.display()),
        ),
    }
}

fn default_worktree_path(repository: &Path) -> PathBuf {
    let repository_name = repository
        .canonicalize()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "repository".to_owned());

    env::temp_dir().join(format!("prober-replay-{repository_name}"))
}

fn summary_line(records: &[ReplayRecord], already_recorded_count: usize) -> String {
    let count_of_mode = |mode: &str| {
        records
            .iter()
            .filter(|record| record.mode() == Some(mode))
            .count()
    };
    let failed_count = records
        .iter()
        .filter(|record| record.selection.is_none())
        .count();
    let mut selected_ratios: Vec<f64> = records
        .iter()
        .filter_map(ReplayRecord::selected_ratio)
        .collect();
    let replay_seconds: Vec<f64> = records.iter().filter_map(|record| record.seconds).collect();

    selected_ratios.sort_by(f64::total_cmp);

    let median_selected = selected_ratios.get(selected_ratios.len() / 2).map_or_else(
        || "n/a".to_owned(),
        |ratio| format!("{:.1}%", ratio * 100.0),
    );
    let mean_seconds = if replay_seconds.is_empty() {
        0.0
    } else {
        replay_seconds.iter().sum::<f64>() / replay_seconds.len() as f64
    };

    format!(
        "prober-replay: replayed {} changes, {already_recorded_count} already recorded, {failed_count} failed — {} selected, {} all, {} none; median {median_selected} of test files selected; {mean_seconds:.1} s per change on average",
        records.len(),
        count_of_mode("selected"),
        count_of_mode("all"),
        count_of_mode("none"),
    )
}
