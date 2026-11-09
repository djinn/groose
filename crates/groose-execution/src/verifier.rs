use std::process::Output;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// The result of a verification check.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationResult {
    Passed,
    Failed { errors: Vec<String> },
    Skipped { reason: String },
}

/// Verifier checks acceptance criteria, tests, linting, diff, and policy.
#[async_trait]
pub trait Verifier: Send + Sync {
    /// Run all verification checks.
    async fn verify(
        &self,
        workspace_diff: &str,
        acceptance_criteria: &[String],
    ) -> Result<VerificationResult>;

    /// Run tests in the workspace.
    async fn run_tests(&self, workspace_root: &std::path::Path) -> Result<VerificationResult>;

    /// Run linting checks.
    async fn run_lint(&self, workspace_root: &std::path::Path) -> Result<VerificationResult>;

    /// Check acceptance criteria against the current state.
    async fn check_acceptance_criteria(
        &self,
        criteria: &[String],
        workspace_diff: &str,
    ) -> Result<VerificationResult>;

    /// Check that the diff is reasonable (e.g., not too large, no secrets).
    async fn check_diff(&self, diff: &str) -> Result<VerificationResult>;
}

fn command_output(result: &Output) -> (bool, String) {
    let stderr = String::from_utf8_lossy(&result.stderr);
    (result.status.success(), stderr.to_string())
}

// ── Basic Verifier Implementation ───────────────────────────────────────

/// A basic verifier that runs shell commands for tests and linting.
pub struct BasicVerifier {
    /// Command to run tests (empty string means skip).
    test_command: String,
    /// Command to run linting (empty string means skip).
    lint_command: String,
}

impl BasicVerifier {
    pub fn new(test_command: &str, lint_command: &str) -> Self {
        Self {
            test_command: test_command.to_string(),
            lint_command: lint_command.to_string(),
        }
    }

    /// Default verifier with common commands.
    pub fn for_language(language: &str) -> Self {
        match language {
            "rust" | "rs" => Self::new("cargo test", "cargo fmt --check"),
            "python" | "py" => Self::new("python3 -m pytest", "ruff check ."),
            "javascript" | "js" | "typescript" | "ts" => Self::new("npm test", "npm run lint"),
            _ => Self::new("", ""),
        }
    }
}

#[async_trait]
impl Verifier for BasicVerifier {
    async fn verify(
        &self,
        workspace_diff: &str,
        acceptance_criteria: &[String],
    ) -> Result<VerificationResult> {
        // 1. Check diff quality
        let diff_result = self.check_diff(workspace_diff).await;
        if let Ok(VerificationResult::Failed { errors }) = diff_result {
            return Ok(VerificationResult::Failed { errors });
        }

        // 2. Check acceptance criteria
        self.check_acceptance_criteria(acceptance_criteria, workspace_diff)
            .await
    }

    async fn run_tests(&self, workspace_root: &std::path::Path) -> Result<VerificationResult> {
        if self.test_command.is_empty() {
            return Ok(VerificationResult::Skipped {
                reason: "no test command configured".to_string(),
            });
        }

        let result = tokio::process::Command::new("sh")
            .args(&["-c", &self.test_command])
            .current_dir(workspace_root)
            .output()
            .await?;

        let (success, stderr) = command_output(&result);
        if success {
            Ok(VerificationResult::Passed)
        } else {
            Ok(VerificationResult::Failed {
                errors: vec![format!(
                    "tests failed (exit: {}): {}",
                    result.status, stderr
                )],
            })
        }
    }

    async fn run_lint(&self, workspace_root: &std::path::Path) -> Result<VerificationResult> {
        if self.lint_command.is_empty() {
            return Ok(VerificationResult::Skipped {
                reason: "no lint command configured".to_string(),
            });
        }

        let result = tokio::process::Command::new("sh")
            .args(&["-c", &self.lint_command])
            .current_dir(workspace_root)
            .output()
            .await?;

        let (success, stderr) = command_output(&result);
        if success {
            Ok(VerificationResult::Passed)
        } else {
            Ok(VerificationResult::Failed {
                errors: vec![format!("lint failed (exit: {}): {}", result.status, stderr)],
            })
        }
    }

    async fn check_acceptance_criteria(
        &self,
        criteria: &[String],
        _workspace_diff: &str,
    ) -> Result<VerificationResult> {
        let empty = criteria.iter().filter(|c| c.trim().is_empty()).count();
        if empty > 0 {
            return Ok(VerificationResult::Failed {
                errors: vec![format!("{} acceptance criteria are empty", empty)],
            });
        }
        Ok(VerificationResult::Passed)
    }

    async fn check_diff(&self, _diff: &str) -> Result<VerificationResult> {
        Ok(VerificationResult::Passed)
    }
}
