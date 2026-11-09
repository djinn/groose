use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A durable checkpoint of engine execution state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ExecutionCheckpoint {
    pub task_id: String,
    pub version: u64,
    pub timestamp: DateTime<Utc>,
    /// The iteration count when this checkpoint was taken.
    pub iteration: u32,
    /// Serialized engine state (e.g., conversation, plan, context).
    pub engine_state: serde_json::Value,
    /// The diff / working changes at checkpoint time.
    pub workspace_diff: Option<String>,
    /// Pending amendments that have not yet been applied.
    pub pending_amendments: Vec<serde_json::Value>,
}

/// A store for execution checkpoints.
#[async_trait]
pub trait CheckpointStore: Send + Sync {
    /// Save a checkpoint.
    async fn save(&self, checkpoint: &ExecutionCheckpoint) -> Result<()>;

    /// Load the latest checkpoint for a task.
    async fn load_latest(&self, task_id: &str) -> Result<Option<ExecutionCheckpoint>>;

    /// List all checkpoints for a task.
    async fn list(&self, task_id: &str) -> Result<Vec<ExecutionCheckpoint>>;

    /// Delete a checkpoint.
    async fn delete(&self, task_id: &str, version: u64) -> Result<()>;
}

// ── Filesystem Checkpoint Store ─────────────────────────────────────────

/// Stores checkpoints as JSON files in a directory.
pub struct FileCheckpointStore {
    dir: PathBuf,
}

impl FileCheckpointStore {
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn checkpoint_path(&self, task_id: &str, version: u64) -> PathBuf {
        self.dir.join(format!("{}-v{}.json", task_id, version))
    }

    fn latest_path(&self, task_id: &str) -> PathBuf {
        self.dir.join(format!("{}-latest.json", task_id))
    }
}

#[async_trait]
impl CheckpointStore for FileCheckpointStore {
    async fn save(&self, checkpoint: &ExecutionCheckpoint) -> Result<()> {
        let json = serde_json::to_string_pretty(checkpoint)?;
        std::fs::write(
            &self.checkpoint_path(&checkpoint.task_id, checkpoint.version),
            &json,
        )?;
        std::fs::write(&self.latest_path(&checkpoint.task_id), &json)?;
        Ok(())
    }

    async fn load_latest(&self, task_id: &str) -> Result<Option<ExecutionCheckpoint>> {
        let path = self.latest_path(task_id);
        if !path.exists() {
            return Ok(None);
        }
        let json = std::fs::read_to_string(&path)?;
        let checkpoint = serde_json::from_str(&json)?;
        Ok(Some(checkpoint))
    }

    async fn list(&self, task_id: &str) -> Result<Vec<ExecutionCheckpoint>> {
        let mut checkpoints = Vec::new();
        if !self.dir.exists() {
            return Ok(checkpoints);
        }
        for entry in std::fs::read_dir(&self.dir)? {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&format!("{}-v", task_id)) && name.ends_with(".json") {
                let json = std::fs::read_to_string(&self.dir.join(name.as_ref()))?;
                checkpoints.push(serde_json::from_str(&json)?);
            }
        }
        checkpoints.sort_by(|a, b| a.version.cmp(&b.version));
        Ok(checkpoints)
    }

    async fn delete(&self, task_id: &str, version: u64) -> Result<()> {
        let path = self.checkpoint_path(task_id, version);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }
}
