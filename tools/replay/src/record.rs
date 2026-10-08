use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::process::Output;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::changes::Change;

#[derive(Debug, Serialize, Deserialize)]
pub struct ReplayRecord {
    pub id: String,
    pub base: String,
    pub head: String,
    pub error: Option<String>,
    pub seconds: Option<f64>,
    pub exit_code: Option<i32>,
    pub selection: Option<Value>,
    pub stderr: String,
}

impl ReplayRecord {
    pub fn failed(change: &Change, error: String) -> Self {
        Self {
            id: change.id.clone(),
            base: change.base.clone(),
            head: change.head.clone(),
            error: Some(error),
            seconds: None,
            exit_code: None,
            selection: None,
            stderr: String::new(),
        }
    }

    pub fn completed(change: &Change, elapsed: Duration, prober_output: &Output) -> Self {
        let exited_successfully = prober_output.status.success();

        Self {
            id: change.id.clone(),
            base: change.base.clone(),
            head: change.head.clone(),
            error: None,
            seconds: Some((elapsed.as_secs_f64() * 1_000.0).round() / 1_000.0),
            exit_code: prober_output.status.code(),
            selection: exited_successfully
                .then(|| serde_json::from_slice(&prober_output.stdout).ok())
                .flatten(),
            stderr: String::from_utf8_lossy(&prober_output.stderr).into_owned(),
        }
    }

    pub fn mode(&self) -> Option<&str> {
        self.selection.as_ref()?.get("mode")?.as_str()
    }

    pub fn selected_ratio(&self) -> Option<f64> {
        self.selection
            .as_ref()?
            .get("stats")?
            .get("selected_ratio")?
            .as_f64()
    }

    pub fn tests_added_by_widening(&self) -> u64 {
        self.stat("tests_added_by_widening").unwrap_or_default()
    }

    pub fn tests_selected(&self) -> u64 {
        self.stat("tests_selected").unwrap_or_default()
    }

    pub fn selected_ratio_without_widening(&self) -> Option<f64> {
        let selected_ratio = self.selected_ratio()?;
        let tests_total = self.stat("tests_total").unwrap_or_default();

        if tests_total == 0 {
            return Some(selected_ratio);
        }

        let widened_ratio = self.tests_added_by_widening() as f64 / tests_total as f64;

        Some((selected_ratio - widened_ratio).max(0.0))
    }

    pub fn widened_sites(&self) -> Vec<WidenedSiteRecord> {
        self.selection
            .as_ref()
            .and_then(|selection| selection.get("widened"))
            .and_then(|widened| serde_json::from_value(widened.clone()).ok())
            .unwrap_or_default()
    }

    fn stat(&self, name: &str) -> Option<u64> {
        self.selection.as_ref()?.get("stats")?.get(name)?.as_u64()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WidenedSiteRecord {
    pub site: String,
    pub cause: String,
    pub added: u64,
}

#[derive(Deserialize)]
struct RecordedId {
    id: String,
}

pub fn recorded_ids(output_path: &Path) -> Result<HashSet<String>> {
    let recorded_text = match fs::read_to_string(output_path) {
        Ok(recorded_text) => recorded_text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read {}", output_path.display()));
        }
    };

    recorded_text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let recorded_id: RecordedId = serde_json::from_str(line)
                .with_context(|| format!("invalid record in {}", output_path.display()))?;

            Ok(recorded_id.id)
        })
        .collect()
}

pub struct RecordWriter {
    file: File,
}

impl RecordWriter {
    pub fn append_to(output_path: &Path) -> Result<Self> {
        if let Some(parent_directory) = output_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent_directory)?;
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(output_path)
            .with_context(|| format!("cannot open {}", output_path.display()))?;

        Ok(Self { file })
    }

    pub fn write(&mut self, record: &ReplayRecord) -> Result<()> {
        let record_line = serde_json::to_string(record)?;

        writeln!(self.file, "{record_line}")?;
        self.file.flush()?;

        Ok(())
    }
}
