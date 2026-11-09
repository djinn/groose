use std::path::{Path, PathBuf};
use std::process::Output;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use tokio::process::Command;

/// Mode for the workspace isolation level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WorkspaceMode {
    /// Use a local Git worktree for isolation.
    Worktree,
    /// Use the existing directory directly (no isolation).
    Local,
    /// Use a temporary copy/clone of the repository.
    TempCopy,
}

/// A sandboxed workspace providing filesystem access, Git operations,
/// and command execution.
#[async_trait]
pub trait Workspace: Send + Sync {
    /// The path to the workspace root.
    fn root(&self) -> &Path;

    /// Execute a command in the workspace, returning stdout.
    async fn run_command(&self, cmd: &str, args: &[&str]) -> Result<String>;

    /// Execute a command in the workspace with optional env overrides.
    async fn run_command_with_env(
        &self,
        cmd: &str,
        args: &[&str],
        env: Vec<(String, String)>,
    ) -> Result<String>;

    /// Read a file from the workspace.
    async fn read_file(&self, path: &Path) -> Result<String>;

    /// Write a file in the workspace.
    async fn write_file(&self, path: &Path, content: &str) -> Result<()>;

    /// Check if a path exists in the workspace.
    async fn exists(&self, path: &Path) -> bool;

    /// Clone or initialize workspace from a repository reference.
    async fn initialize(&self, repo_url: &str, ref_name: &str) -> Result<()>;

    /// Get the current Git HEAD SHA.
    async fn head_sha(&self) -> Result<String>;

    /// Get the diff from the workspace.
    async fn diff(&self) -> Result<String>;
}

fn command_output(output: &Output) -> Result<String> {
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(anyhow!(
            "command failed (exit: {}): {}",
            output.status,
            stderr,
        ))
    }
}

// ── Local Workspace Implementation ──────────────────────────────────────

/// A workspace that operates directly on a local directory.
pub struct LocalWorkspace {
    root: PathBuf,
}

impl LocalWorkspace {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Workspace for LocalWorkspace {
    fn root(&self) -> &Path {
        &self.root
    }

    async fn run_command(&self, cmd: &str, args: &[&str]) -> Result<String> {
        self.run_command_with_env(cmd, args, vec![]).await
    }

    async fn run_command_with_env(
        &self,
        cmd: &str,
        args: &[&str],
        env: Vec<(String, String)>,
    ) -> Result<String> {
        command_output(
            &Command::new(cmd)
                .args(args)
                .current_dir(&self.root)
                .env_clear()
                .envs(env)
                .output()
                .await
                .with_context(|| format!("failed to execute {} {}", cmd, args.join(" ")))?,
        )
    }

    async fn read_file(&self, path: &Path) -> Result<String> {
        let full_path = self.root.join(path);
        std::fs::read_to_string(&full_path)
            .with_context(|| format!("failed to read {:?}", full_path))
    }

    async fn write_file(&self, path: &Path, content: &str) -> Result<()> {
        let full_path = self.root.join(path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full_path, content)
            .with_context(|| format!("failed to write {:?}", full_path))
    }

    async fn exists(&self, path: &Path) -> bool {
        self.root.join(path).exists()
    }

    async fn initialize(&self, _repo_url: &str, _ref_name: &str) -> Result<()> {
        Ok(())
    }

    async fn head_sha(&self) -> Result<String> {
        self.run_command("git", &["rev-parse", "HEAD"]).await
    }

    async fn diff(&self) -> Result<String> {
        self.run_command("git", &["diff"]).await
    }
}

// ── Temp Workspace ──────────────────────────────────────────────────────

/// Creates a temporary directory and clones the repository into it.
pub struct TempWorkspace {
    root: PathBuf,
    _temp_dir: tempfile::TempDir,
}

impl TempWorkspace {
    pub fn new() -> Result<(Self, PathBuf)> {
        let temp_dir = tempfile::tempdir()?;
        let root = temp_dir.path().to_path_buf();
        let root_clone = root.clone();
        Ok((
            Self {
                root,
                _temp_dir: temp_dir,
            },
            root_clone,
        ))
    }
}

#[async_trait]
impl Workspace for TempWorkspace {
    fn root(&self) -> &Path {
        &self.root
    }

    async fn run_command(&self, cmd: &str, args: &[&str]) -> Result<String> {
        command_output(
            &Command::new(cmd)
                .args(args)
                .current_dir(&self.root)
                .output()
                .await
                .with_context(|| format!("failed to execute {} {}", cmd, args.join(" ")))?,
        )
    }

    async fn run_command_with_env(
        &self,
        cmd: &str,
        args: &[&str],
        env: Vec<(String, String)>,
    ) -> Result<String> {
        command_output(
            &Command::new(cmd)
                .args(args)
                .current_dir(&self.root)
                .env_clear()
                .envs(env)
                .output()
                .await
                .with_context(|| format!("failed to execute {} {}", cmd, args.join(" ")))?,
        )
    }

    async fn read_file(&self, path: &Path) -> Result<String> {
        let full_path = self.root.join(path);
        std::fs::read_to_string(&full_path)
            .with_context(|| format!("failed to read {:?}", full_path))
    }

    async fn write_file(&self, path: &Path, content: &str) -> Result<()> {
        let full_path = self.root.join(path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full_path, content)
            .with_context(|| format!("failed to write {:?}", full_path))
    }

    async fn exists(&self, path: &Path) -> bool {
        self.root.join(path).try_exists().unwrap_or(false)
    }

    async fn initialize(&self, repo_url: &str, ref_name: &str) -> Result<()> {
        self.run_command("git", &["clone", repo_url, "."]).await?;
        self.run_command("git", &["checkout", ref_name]).await?;
        Ok(())
    }

    async fn head_sha(&self) -> Result<String> {
        self.run_command("git", &["rev-parse", "HEAD"]).await
    }

    async fn diff(&self) -> Result<String> {
        self.run_command("git", &["diff"]).await
    }
}
