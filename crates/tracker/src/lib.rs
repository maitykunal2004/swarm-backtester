//! Append-only experiment tracking.
//!
//! Each run is a directory with an immutable `run.json` + `config.json`,
//! append-only `metrics.jsonl` / `events.jsonl`, and a single `summary.json`
//! written at finalization. No file is silently overwritten.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const TRACKER_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum TrackerError {
    #[error("invalid run name: {0}")]
    InvalidName(String),

    #[error("run already exists: {0}")]
    RunExists(String),

    #[error("run is already finalized: {0}")]
    AlreadyFinalized(String),

    #[error("run not found: {0}")]
    RunNotFound(String),

    #[error("corrupt tracker record at {path}: {details}")]
    Corrupt { path: PathBuf, details: String },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewRun {
    pub name: String,
    pub config: serde_json::Value,
    pub dataset_fingerprint: Option<String>,
    pub notes: String,
    pub git_commit: Option<String>,
}

impl NewRun {
    pub fn new(name: impl Into<String>, config: serde_json::Value) -> Self {
        Self {
            name: name.into(),
            config,
            dataset_fingerprint: None,
            notes: String::new(),
            git_commit: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub schema_version: u32,
    pub run_id: String,
    pub name: String,
    pub created_at_ms: i64,
    pub git_commit: Option<String>,
    pub config_sha256: String,
    pub dataset_fingerprint: Option<String>,
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricRecord {
    pub logged_at_ms: i64,
    pub metrics: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunEvent {
    pub at_ms: i64,
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct ExperimentStore {
    root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RunHandle {
    store: ExperimentStore,
    run_id: String,
}

impl ExperimentStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.root.join("runs")
    }

    pub fn create_run(&self, request: NewRun) -> Result<RunHandle, TrackerError> {
        validate_name(&request.name)?;
        if !request.config.is_object() {
            return Err(TrackerError::Corrupt {
                path: self.root.clone(),
                details: "run config must be a JSON object".to_string(),
            });
        }
        fs::create_dir_all(self.runs_dir())?;
        let config_bytes = serde_json::to_vec_pretty(&request.config)?;
        let config_sha = hex_sha256(&config_bytes);
        let created_at_ms = now_ms();
        let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let base = format!(
            "{stamp}-{}-{}",
            sanitize_name(&request.name),
            &config_sha[..8]
        );

        let mut attempt = 0u32;
        let (run_id, run_dir) = loop {
            let candidate = if attempt == 0 {
                base.clone()
            } else {
                format!("{base}-v{attempt}")
            };
            let dir = self.runs_dir().join(&candidate);
            match fs::create_dir(&dir) {
                Ok(()) => break (candidate, dir),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    attempt += 1;
                    if attempt > 100 {
                        return Err(TrackerError::RunExists(base));
                    }
                }
                Err(error) => return Err(error.into()),
            }
        };

        let record = RunRecord {
            schema_version: TRACKER_SCHEMA_VERSION,
            run_id: run_id.clone(),
            name: request.name,
            created_at_ms,
            git_commit: request.git_commit.or_else(detect_git_commit),
            config_sha256: config_sha,
            dataset_fingerprint: request.dataset_fingerprint,
            notes: request.notes,
        };
        write_new(
            &run_dir.join("run.json"),
            &serde_json::to_vec_pretty(&record)?,
        )?;
        write_new(&run_dir.join("config.json"), &config_bytes)?;
        Ok(RunHandle {
            store: self.clone(),
            run_id,
        })
    }

    pub fn open_run(&self, run_id: &str) -> Result<RunHandle, TrackerError> {
        validate_name(run_id)?;
        let dir = self.runs_dir().join(run_id);
        if !dir.join("run.json").exists() {
            return Err(TrackerError::RunNotFound(run_id.to_string()));
        }
        Ok(RunHandle {
            store: self.clone(),
            run_id: run_id.to_string(),
        })
    }
}

impl ExperimentStore {
    pub fn list_runs(&self) -> Result<Vec<RunRecord>, TrackerError> {
        let dir = self.runs_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let run_path = entry.path().join("run.json");
            if !run_path.exists() {
                continue;
            }
            let bytes = fs::read(&run_path)?;
            let record: RunRecord =
                serde_json::from_slice(&bytes).map_err(|error| TrackerError::Corrupt {
                    path: run_path.clone(),
                    details: error.to_string(),
                })?;
            records.push(record);
        }
        records.sort_by_key(|record| record.created_at_ms);
        Ok(records)
    }
}

impl RunHandle {
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn dir(&self) -> PathBuf {
        self.store.runs_dir().join(&self.run_id)
    }

    pub fn record(&self) -> Result<RunRecord, TrackerError> {
        let path = self.dir().join("run.json");
        let bytes = fs::read(&path)?;
        serde_json::from_slice(&bytes).map_err(|error| TrackerError::Corrupt {
            path,
            details: error.to_string(),
        })
    }

    /// Append one metrics observation. Metrics must be a JSON object.
    pub fn log_metrics(&self, metrics: serde_json::Value) -> Result<(), TrackerError> {
        if !metrics.is_object() {
            return Err(TrackerError::Corrupt {
                path: self.dir(),
                details: "metrics must be a JSON object".to_string(),
            });
        }
        let record = MetricRecord {
            logged_at_ms: now_ms(),
            metrics,
        };
        append_jsonl(&self.dir().join("metrics.jsonl"), &record)
    }

    pub fn log_event(&self, kind: &str, message: &str) -> Result<(), TrackerError> {
        let event = RunEvent {
            at_ms: now_ms(),
            kind: kind.to_string(),
            message: message.to_string(),
        };
        append_jsonl(&self.dir().join("events.jsonl"), &event)
    }

    pub fn read_metrics(&self) -> Result<Vec<MetricRecord>, TrackerError> {
        let path = self.dir().join("metrics.jsonl");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let text = fs::read_to_string(&path)?;
        let mut records = Vec::new();
        for (line_number, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let record: MetricRecord =
                serde_json::from_str(line).map_err(|error| TrackerError::Corrupt {
                    path: path.clone(),
                    details: format!("line {}: {error}", line_number + 1),
                })?;
            records.push(record);
        }
        Ok(records)
    }

    pub fn is_finished(&self) -> bool {
        self.dir().join("summary.json").exists()
    }

    /// Write the final summary exactly once and append a finish event.
    pub fn finish(&self, summary: &serde_json::Value) -> Result<PathBuf, TrackerError> {
        if !summary.is_object() {
            return Err(TrackerError::Corrupt {
                path: self.dir(),
                details: "summary must be a JSON object".to_string(),
            });
        }
        let path = self.dir().join("summary.json");
        if path.exists() {
            return Err(TrackerError::AlreadyFinalized(self.run_id.clone()));
        }
        write_new(&path, &serde_json::to_vec_pretty(summary)?)?;
        self.log_event("finished", "summary.json written")?;
        Ok(path)
    }

    pub fn read_summary(&self) -> Result<Option<serde_json::Value>, TrackerError> {
        let path = self.dir().join("summary.json");
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
    }
}

fn validate_name(name: &str) -> Result<(), TrackerError> {
    if name.is_empty() || name.len() > 96 {
        return Err(TrackerError::InvalidName(format!(
            "length must be 1..=96, got {}",
            name.len()
        )));
    }
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
    {
        return Err(TrackerError::InvalidName(
            "only ASCII letters, digits, '-', '_' and '.' are allowed".to_string(),
        ));
    }
    Ok(())
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), TrackerError> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> Result<(), TrackerError> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(&line)?;
    file.sync_data()?;
    Ok(())
}

/// Best-effort git commit for provenance. Never fails a run.
pub fn detect_git_commit() -> Option<String> {
    if let Ok(commit) = std::env::var("MB_GIT_COMMIT") {
        let trimmed = commit.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if commit.is_empty() {
        None
    } else {
        Some(commit)
    }
}
