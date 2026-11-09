//! Slack Events API trust model.
//!
//! # Trust Assumptions
//!
//! 1. **Webhook authenticity** is verified via HMAC-SHA256 using the Slack
//!    signing secret.
//!
//! 2. **Timestamp freshness** prevents replay attacks (default 5 minute window).
//!
//! 3. **Deduplication** by Slack `event_id` prevents duplicate tasks from retries.
//!
//! 4. **Authorization** checks users/teams against allowlists.
//!
//! 5. **Bot-loop prevention**: Groose ignores events triggered by its own bot.

pub mod commands;

use crate::trust::{self, AuthDecision, Authorizer, Requester, SecurityAudit, SecurityEvent, SecurityEventType, Target};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const SLACK_MAX_AGE_SECS: i64 = 300; // 5 minutes

#[derive(Debug, Clone)]
pub struct SlackConfig {
    pub signing_secret: String,
    pub bot_token: String,
    pub app_id: String,
    pub bot_user_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedSlackEvent {
    pub event_id: String,
    pub team_id: Option<String>,
    pub channel_id: Option<String>,
    pub user_id: Option<String>,
    pub thread_ts: Option<String>,
    pub event_type: String,
    pub text: String,
    pub raw_payload: serde_json::Value,
    pub verified_at: DateTime<Utc>,
}

/// Verifies and validates incoming Slack Events API payloads.
pub struct SlackEventVerifier {
    config: SlackConfig,
    authorizer: Arc<dyn Authorizer>,
    deduplicator: trust::Deduplicator,
    audit: Arc<dyn SecurityAudit>,
}

impl SlackEventVerifier {
    pub fn new(
        config: SlackConfig,
        authorizer: Arc<dyn Authorizer>,
        deduplicator: trust::Deduplicator,
        audit: Arc<dyn SecurityAudit>,
    ) -> Self {
        Self { config, authorizer, deduplicator, audit }
    }

    pub fn config(&self) -> &SlackConfig {
        &self.config
    }

    /// Verify a raw Slack webhook payload.
    pub async fn verify(
        &self,
        body: &[u8],
        signature_header: &str,
        timestamp_header: &str,
    ) -> Result<Option<VerifiedSlackEvent>, SlackVerifyError> {
        let event_id = uuid::Uuid::new_v4().to_string();
        let base_requester = Requester {
            provider: "slack".to_string(),
            user_id: "unknown".to_string(),
            team_id: None,
            installation_id: None,
        };

        // 1. Timestamp freshness
        let timestamp_secs: i64 = timestamp_header
            .parse()
            .map_err(|_| SlackVerifyError::InvalidTimestamp)?;

        if !trust::check_timestamp_freshness(timestamp_secs, SLACK_MAX_AGE_SECS, Utc::now()) {
            self.audit
                .record(SecurityEvent {
                    event_id: event_id.clone(),
                    timestamp: Utc::now(),
                    event_type: SecurityEventType::TimestampRejected,
                    requester: base_requester,
                    target: None,
                    decision: AuthDecision::Denied {
                        reason: "timestamp too old".to_string(),
                    },
                    details: format!("timestamp={} outside {}s window", timestamp_secs, SLACK_MAX_AGE_SECS),
                })
                .await;
            return Err(SlackVerifyError::TimestampTooOld);
        }

        // 2. Signature verification
        let sig_basestring = format!(
            "v0:{}:{}",
            timestamp_header,
            std::str::from_utf8(body).unwrap_or("")
        );
        let computed_hex = trust::compute_hmac_sha256_hex(
            self.config.signing_secret.as_bytes(),
            sig_basestring.as_bytes(),
        );
        let expected_sig = format!("v0={}", computed_hex);

        if !constant_time_compare(signature_header, &expected_sig) {
            self.audit
                .record(SecurityEvent {
                    event_id: event_id.clone(),
                    timestamp: Utc::now(),
                    event_type: SecurityEventType::SignatureVerificationFailed,
                    requester: base_requester,
                    target: None,
                    decision: AuthDecision::Denied {
                        reason: "HMAC signature mismatch".to_string(),
                    },
                    details: format!("expected={} got={}", expected_sig, signature_header),
                })
                .await;
            return Err(SlackVerifyError::SignatureMismatch);
        }

        self.audit
            .record(SecurityEvent {
                event_id: event_id.clone(),
                timestamp: Utc::now(),
                event_type: SecurityEventType::SignatureVerificationPassed,
                requester: base_requester,
                target: None,
                decision: AuthDecision::Allowed,
                details: "HMAC signature verified".to_string(),
            })
            .await;

        // 3. Parse payload
        let payload: serde_json::Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => return Err(SlackVerifyError::ParseError(e.to_string())),
        };
        let slack_event = &payload["event"];
        let event_type = slack_event["type"].as_str().unwrap_or("unknown").to_string();
        let user_id = slack_event["user"].as_str().map(|s| s.to_string());
        let channel_id = slack_event["channel"].as_str().map(|s| s.to_string());
        let team_id = payload["team_id"].as_str().map(|s| s.to_string());
        let thread_ts = slack_event["thread_ts"].as_str().map(|s| s.to_string());
        let text = slack_event["text"].as_str().unwrap_or("").to_string();
        let event_source_id = slack_event["event_id"].as_str().unwrap_or(&event_id).to_string();

        // 4. Bot-loop prevention
        if let Some(bot_id) = slack_event["bot_id"].as_str() {
            if slack_event["app_id"].as_str() == Some(&self.config.app_id)
                || bot_id == self.config.bot_user_id
            {
                tracing::debug!("ignoring own bot event: {}", event_source_id);
                return Ok(None);
            }
        }

        // 5. Duplicate check
        if !self.deduplicator.try_seen(&event_source_id) {
            tracing::debug!("duplicate Slack event dropped: {}", event_source_id);
            return Ok(None);
        }

        // 6. Authorization — clone user_id before building structured event
        let user_id_for_auth = user_id.clone().unwrap_or_default();
        let requester = Requester {
            provider: "slack".to_string(),
            user_id: user_id_for_auth.clone(),
            team_id: team_id.clone(),
            installation_id: None,
        };
        let target = Target {
            repository: "unknown".to_string(),
            action: "implement".to_string(),
        };

        match self.authorizer.authorize(&requester, &target) {
            AuthDecision::Allowed => {
                self.audit
                    .record(SecurityEvent {
                        event_id: event_source_id.clone(),
                        timestamp: Utc::now(),
                        event_type: SecurityEventType::AuthorizationAllowed,
                        requester,
                        target: Some(target),
                        decision: AuthDecision::Allowed,
                        details: format!("slack user {} authorized", user_id_for_auth),
                    })
                    .await;
                Ok(Some(VerifiedSlackEvent {
                    event_id: event_source_id,
                    team_id,
                    channel_id,
                    user_id,
                    thread_ts,
                    event_type,
                    text,
                    raw_payload: payload,
                    verified_at: Utc::now(),
                }))
            }
            AuthDecision::Denied { reason } => {
                self.audit
                    .record(SecurityEvent {
                        event_id: event_source_id,
                        timestamp: Utc::now(),
                        event_type: SecurityEventType::AuthorizationDenied,
                        requester,
                        target: Some(target),
                        decision: AuthDecision::Denied {
                            reason: reason.clone(),
                        },
                        details: reason,
                    })
                    .await;
                Ok(None)
            }
        }
    }
}

fn constant_time_compare(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Debug, thiserror::Error)]
pub enum SlackVerifyError {
    #[error("invalid timestamp header")]
    InvalidTimestamp,
    #[error("timestamp too old (possible replay attack)")]
    TimestampTooOld,
    #[error("HMAC signature mismatch")]
    SignatureMismatch,
    #[error("failed to parse event payload: {0}")]
    ParseError(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::AllowlistAuthorizer;

    fn test_config() -> SlackConfig {
        SlackConfig {
            signing_secret: "test_secret".to_string(),
            bot_token: "xoxb-test".to_string(),
            app_id: "A01".to_string(),
            bot_user_id: "U0BOT".to_string(),
        }
    }

    #[test]
    fn test_constant_time_compare() {
        assert!(constant_time_compare("abc123", "abc123"));
        assert!(!constant_time_compare("abc123", "abc124"));
        assert!(!constant_time_compare("abc123", "abc12"));
    }
}