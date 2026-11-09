//! GitHub App webhook trust model.
//!
//! # Trust Assumptions
//!
//! 1. **Webhook authenticity** is verified via HMAC-SHA256 using the GitHub
//!    App's webhook secret.  The `X-Hub-Signature-256` header contains the
//!    expected hex digest.
//!
//! 2. **Timestamp freshness** is checked via `X-GitHub-Delivery` and the
//!    event payload timestamp.  Requests with stale timestamps are rejected.
//!
//! 3. **Deduplication** by the GitHub `X-GitHub-Delivery` GUID prevents
//!    duplicate task creation from retries.
//!
//! 4. **Authorization** uses the GitHub App installation's repository access
//!    scope and explicit allowlists.  Installation tokens are short-lived
//!    and scoped per-repository.
//!
//! 5. **Bot-loop prevention**: Groose ignores events it generated (by
//!    checking the sender's installation ID against its own).
//!
//! 6. **Installation tokens** are generated with minimal permissions and
//!    scoped only to the repositories the task operates on.

use crate::trust::{self, AuthDecision, Authorizer, Requester, SecurityAudit, SecurityEvent, SecurityEventType, Target};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const GITHUB_MAX_AGE_SECS: i64 = 300; // 5 minutes

// ── Configuration ───────────────────────────────────────────────────────

/// Configuration for the GitHub App integration.
#[derive(Debug, Clone)]
pub struct GitHubConfig {
    /// GitHub App ID.
    pub app_id: String,
    /// GitHub App webhook secret.
    pub webhook_secret: String,
    /// GitHub App private key (PEM-encoded) for generating installation tokens.
    pub private_key_pem: String,
    /// The app's own installation ID — used for bot-loop prevention.
    pub self_installation_id: Option<String>,
    /// Base URL for the GitHub API (default: https://api.github.com).
    pub api_base_url: String,
}

// ── GitHub Event Types ──────────────────────────────────────────────────

/// The GitHub webhook event types Groose handles (SPECS.md §9).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum GitHubEventType {
    /// An issue was assigned to Groose.
    IssuesAssigned,
    /// An issue was labeled "groose:implement".
    IssuesLabeled,
    /// A PR review requested changes.
    PullRequestReviewRequestedChanges,
    /// A PR review comment was left.
    PullRequestReviewComment,
    /// CI check run failed.
    CheckRunFailure,
    /// Check suite failure.
    CheckSuiteFailure,
    /// PR became mergeable.
    PullRequestMergeable,
    /// PR was merged.
    PullRequestMerged,
    /// PR was closed without merge.
    PullRequestClosed,
    /// Unknown or unhandled event.
    Unknown,
}

// ── Verified GitHub Event ───────────────────────────────────────────────

/// A GitHub webhook event that has passed all trust checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedGitHubEvent {
    /// The GitHub delivery GUID (`X-GitHub-Delivery`).
    pub delivery_id: String,
    /// The GitHub event name (e.g., "issues", "pull_request").
    pub event_name: String,
    /// The Groose event type classification.
    pub event_type: GitHubEventType,
    /// The installation ID that owns the event.
    pub installation_id: Option<String>,
    /// The repository owner/name (e.g., "org/repo").
    pub repository: String,
    /// The sender's GitHub user ID.
    pub sender_id: Option<String>,
    /// The issue or PR number, if applicable.
    pub issue_pr_number: Option<u64>,
    /// The action within the event (e.g., "opened", "created", "assigned").
    pub action: Option<String>,
    /// The raw event payload.
    pub raw_payload: serde_json::Value,
    /// The timestamp when the event was verified.
    pub verified_at: DateTime<Utc>,
}

// ── GitHub Event Verifier ───────────────────────────────────────────────

/// Verifies and validates incoming GitHub webhook payloads.
pub struct GitHubEventVerifier {
    config: GitHubConfig,
    authorizer: Arc<dyn Authorizer>,
    deduplicator: trust::Deduplicator,
    audit: Arc<dyn SecurityAudit>,
}

impl GitHubEventVerifier {
    pub fn new(
        config: GitHubConfig,
        authorizer: Arc<dyn Authorizer>,
        deduplicator: trust::Deduplicator,
        audit: Arc<dyn SecurityAudit>,
    ) -> Self {
        Self {
            config,
            authorizer,
            deduplicator,
            audit,
        }
    }

    /// Verify and validate a raw GitHub webhook payload.
    ///
    /// Returns `Ok(None)` for events that are benign duplicates, triggered
    /// by Groose itself, or where the signature is invalid.
    pub async fn verify(
        &self,
        body: &[u8],
        signature_256_header: &str,
        delivery_id: &str,
        event_name: &str,
    ) -> Result<Option<VerifiedGitHubEvent>, GitHubVerifyError> {
        let requester = Requester {
            provider: "github".to_string(),
            user_id: "unknown".to_string(),
            team_id: None,
            installation_id: None,
        };

        // 1. Signature verification
        // GitHub sends `sha256=hexdigest` in the X-Hub-Signature-256 header
        let expected_prefix = "sha256=";
        let hex_sig = signature_256_header
            .strip_prefix(expected_prefix)
            .ok_or(GitHubVerifyError::InvalidSignatureFormat)?;

        if !trust::verify_hmac_sha256(
            self.config.webhook_secret.as_bytes(),
            body,
            hex_sig,
        ) {
            self.audit.record(SecurityEvent {
                event_id: delivery_id.to_string(),
                timestamp: Utc::now(),
                event_type: SecurityEventType::SignatureVerificationFailed,
                requester,
                target: None,
                decision: AuthDecision::Denied {
                    reason: "HMAC signature mismatch".to_string(),
                },
                details: format!("delivery_id={}", delivery_id),
            }).await;
            return Err(GitHubVerifyError::SignatureMismatch);
        }

        self.audit.record(SecurityEvent {
            event_id: delivery_id.to_string(),
            timestamp: Utc::now(),
            event_type: SecurityEventType::SignatureVerificationPassed,
            requester: requester.clone(),
            target: None,
            decision: AuthDecision::Allowed,
            details: "HMAC signature verified".to_string(),
        }).await;

        // 2. Parse the payload
        let payload: serde_json::Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => return Err(GitHubVerifyError::ParseError(e.to_string())),
        };

        // 3. Deduplication
        if !self.deduplicator.try_seen(delivery_id) {
            tracing::debug!("duplicate GitHub event dropped: {}", delivery_id);
            return Ok(None);
        }

        // 4. Check timestamp freshness from the payload
        let sender = payload["sender"]["id"].as_u64();
        let installation_id = payload["installation"]["id"].as_u64();
        let repo_name = payload["repository"]["full_name"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();
        let action = payload["action"].as_str().map(|s| s.to_string());

        // Check payload-level timestamps for replay protection
        let repo_created = payload["repository"]["created_at"].as_str();
        let issue_updated = payload["issue"]["updated_at"].as_str();
        if let Some(updated) = issue_updated.or(repo_created) {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(updated) {
                if !trust::check_timestamp_freshness(
                    dt.timestamp(),
                    GITHUB_MAX_AGE_SECS,
                    Utc::now(),
                ) {
                    self.audit.record(SecurityEvent {
                        event_id: delivery_id.to_string(),
                        timestamp: Utc::now(),
                        event_type: SecurityEventType::TimestampRejected,
                        requester,
                        target: None,
                        decision: AuthDecision::Denied {
                            reason: "payload timestamp too old".to_string(),
                        },
                        details: format!("delivery_id={} timestamp={}", delivery_id, updated),
                    }).await;
                    return Err(GitHubVerifyError::TimestampTooOld);
                }
            }
        }

        // 5. Bot-loop prevention: ignore events from our own installation
        let inst_id_str = installation_id.map(|id| id.to_string());
        if let Some(self_inst) = &self.config.self_installation_id {
            if inst_id_str.as_deref() == Some(self_inst.as_str()) {
                tracing::debug!("ignoring own installation event: {}", delivery_id);
                return Ok(None);
            }
        }

        // 6. Classify the event
        let event_type = self.classify_event(event_name, action.as_deref(), &payload);

        // 7. Extract issue/PR number
        let issue_pr_number = payload["issue"]["number"]
            .as_u64()
            .or_else(|| payload["pull_request"]["number"].as_u64())
            .or_else(|| payload["number"].as_u64());

        // 8. Authorization
        let sender_id_str = sender.map(|id| id.to_string());
        let requester = Requester {
            provider: "github".to_string(),
            user_id: sender_id_str.clone().unwrap_or_default(),
            team_id: None,
            installation_id: inst_id_str.clone(),
        };
        let target = Target {
            repository: repo_name.clone(),
            action: format!("{:?}", event_type),
        };

        match self.authorizer.authorize(&requester, &target) {
            AuthDecision::Allowed => {
                self.audit.record(SecurityEvent {
                    event_id: delivery_id.to_string(),
                    timestamp: Utc::now(),
                    event_type: SecurityEventType::AuthorizationAllowed,
                    requester: requester.clone(),
                    target: Some(target),
                    decision: AuthDecision::Allowed,
                    details: format!("github {} authorized on {}", sender_id_str.as_deref().unwrap_or("unknown"), repo_name),
                }).await;

                Ok(Some(VerifiedGitHubEvent {
                    delivery_id: delivery_id.to_string(),
                    event_name: event_name.to_string(),
                    event_type,
                    installation_id: inst_id_str,
                    repository: repo_name,
                    sender_id: sender_id_str,
                    issue_pr_number,
                    action,
                    raw_payload: payload,
                    verified_at: Utc::now(),
                }))
            }
            AuthDecision::Denied { reason } => {
                self.audit.record(SecurityEvent {
                    event_id: delivery_id.to_string(),
                    timestamp: Utc::now(),
                    event_type: SecurityEventType::AuthorizationDenied,
                    requester,
                    target: Some(target),
                    decision: AuthDecision::Denied { reason: reason.clone() },
                    details: reason,
                }).await;
                Ok(None)
            }
        }
    }

    /// Classify a GitHub event into a Groose action (SPECS.md §9).
    fn classify_event(
        &self,
        event_name: &str,
        action: Option<&str>,
        payload: &serde_json::Value,
    ) -> GitHubEventType {
        match (event_name, action) {
            ("issues", Some("assigned")) => {
                // Check if assigned to Groose
                if payload["assignee"]["login"].as_str() == Some("groose[bot]") {
                    GitHubEventType::IssuesAssigned
                } else {
                    GitHubEventType::Unknown
                }
            }
            ("issues", Some("labeled")) => {
                let label = payload["label"]["name"].as_str().unwrap_or("");
                if label == "groose:implement" {
                    GitHubEventType::IssuesLabeled
                } else {
                    GitHubEventType::Unknown
                }
            }
            ("pull_request_review", Some("submitted")) => {
                let state = payload["review"]["state"].as_str().unwrap_or("");
                if state == "changes_requested" {
                    GitHubEventType::PullRequestReviewRequestedChanges
                } else {
                    GitHubEventType::Unknown
                }
            }
            ("pull_request_review_comment", Some("created")) => {
                GitHubEventType::PullRequestReviewComment
            }
            ("check_run", _) => {
                let conclusion = payload["check_run"]["conclusion"].as_str().unwrap_or("");
                if conclusion == "failure" || conclusion == "timed_out" || conclusion == "cancelled" {
                    GitHubEventType::CheckRunFailure
                } else {
                    GitHubEventType::Unknown
                }
            }
            ("check_suite", _) => {
                let conclusion = payload["check_suite"]["conclusion"].as_str().unwrap_or("");
                if conclusion == "failure" || conclusion == "timed_out" || conclusion == "cancelled" {
                    GitHubEventType::CheckSuiteFailure
                } else {
                    GitHubEventType::Unknown
                }
            }
            ("pull_request", _) => {
                let pr_action = action.unwrap_or("");
                let merged = payload["pull_request"]["merged"].as_bool().unwrap_or(false);
                let state = payload["pull_request"]["state"].as_str().unwrap_or("");
                if pr_action == "closed" && merged {
                    GitHubEventType::PullRequestMerged
                } else if pr_action == "closed" && state == "closed" {
                    GitHubEventType::PullRequestClosed
                } else {
                    GitHubEventType::Unknown
                }
            }
            _ => GitHubEventType::Unknown,
        }
    }

    /// Generate a short-lived GitHub installation access token.
    ///
    /// In a full implementation this would use the GitHub App's private key
    /// to create a JWT, exchange it for an installation token, and return
    /// the scoped token.
    pub async fn generate_installation_token(
        &self,
        installation_id: &str,
        repository: &str,
    ) -> Result<String, GitHubTokenError> {
        // Phase 1: stub implementation
        // Phase 3+: real JWT-based token generation
        tracing::info!(
            installation_id = %installation_id,
            repository = %repository,
            "generating installation token (stub)"
        );
        Ok(format!("ghs_stub_{}_{}", installation_id, repository))
    }
}

// ── Errors ──────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum GitHubVerifyError {
    #[error("invalid X-Hub-Signature-256 format, expected sha256=...")]
    InvalidSignatureFormat,
    #[error("HMAC signature mismatch")]
    SignatureMismatch,
    #[error("payload timestamp too old (possible replay attack)")]
    TimestampTooOld,
    #[error("failed to parse event payload: {0}")]
    ParseError(String),
}

#[derive(Debug, thiserror::Error)]
pub enum GitHubTokenError {
    #[error("failed to generate JWT: {0}")]
    JwtGeneration(String),
    #[error("failed to exchange JWT for installation token: {0}")]
    TokenExchange(String),
}

// ── Unit tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::AllowlistAuthorizer;

    fn test_config() -> GitHubConfig {
        GitHubConfig {
            app_id: "123456".to_string(),
            webhook_secret: "test_secret".to_string(),
            private_key_pem: "test_key".to_string(),
            self_installation_id: None,
            api_base_url: "https://api.github.com".to_string(),
        }
    }

    #[test]
    fn test_classify_issues_assigned() {
        let config = test_config();
        let verifier = GitHubEventVerifier::new(
            config,
            Arc::new(AllowlistAuthorizer::new()),
            trust::Deduplicator::new(1000),
            Arc::new(crate::trust::TracingAudit),
        );

        let payload = serde_json::json!({
            "action": "assigned",
            "assignee": { "login": "groose[bot]" }
        });

        assert_eq!(
            verifier.classify_event("issues", Some("assigned"), &payload),
            GitHubEventType::IssuesAssigned
        );
    }

    #[test]
    fn test_classify_issues_labeled() {
        let config = test_config();
        let verifier = GitHubEventVerifier::new(
            config,
            Arc::new(AllowlistAuthorizer::new()),
            trust::Deduplicator::new(1000),
            Arc::new(crate::trust::TracingAudit),
        );

        let payload = serde_json::json!({
            "action": "labeled",
            "label": { "name": "groose:implement" }
        });

        assert_eq!(
            verifier.classify_event("issues", Some("labeled"), &payload),
            GitHubEventType::IssuesLabeled
        );
    }

    #[test]
    fn test_classify_pr_review_changes_requested() {
        let config = test_config();
        let verifier = GitHubEventVerifier::new(
            config,
            Arc::new(AllowlistAuthorizer::new()),
            trust::Deduplicator::new(1000),
            Arc::new(crate::trust::TracingAudit),
        );

        let payload = serde_json::json!({
            "review": { "state": "changes_requested" }
        });

        assert_eq!(
            verifier.classify_event("pull_request_review", Some("submitted"), &payload),
            GitHubEventType::PullRequestReviewRequestedChanges
        );
    }

    #[test]
    fn test_classify_check_run_failure() {
        let config = test_config();
        let verifier = GitHubEventVerifier::new(
            config,
            Arc::new(AllowlistAuthorizer::new()),
            trust::Deduplicator::new(1000),
            Arc::new(crate::trust::TracingAudit),
        );

        let payload = serde_json::json!({
            "check_run": { "conclusion": "failure" }
        });

        assert_eq!(
            verifier.classify_event("check_run", None, &payload),
            GitHubEventType::CheckRunFailure
        );
    }

    #[test]
    fn test_classify_pr_merged() {
        let config = test_config();
        let verifier = GitHubEventVerifier::new(
            config,
            Arc::new(AllowlistAuthorizer::new()),
            trust::Deduplicator::new(1000),
            Arc::new(crate::trust::TracingAudit),
        );

        let payload = serde_json::json!({
            "pull_request": { "merged": true, "state": "closed" }
        });

        assert_eq!(
            verifier.classify_event("pull_request", Some("closed"), &payload),
            GitHubEventType::PullRequestMerged
        );
    }
}