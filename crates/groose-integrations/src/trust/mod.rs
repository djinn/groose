//! Shared security core for webhook verification, authorization, and
//! deduplication.  Provides the foundation that Slack, GitHub, and any
//! future integration build on.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

// ── Verification primitives ─────────────────────────────────────────────

/// Verifies an HMAC-SHA256 signature.
///
/// Both Slack and GitHub use HMAC-SHA256 over the request body, with
/// the signing secret as the key.  The expected hex signature is
/// supplied by the integration's `X-Slack-Signature` /
/// `X-Hub-Signature-256` header.
pub fn verify_hmac_sha256(secret: &[u8], body: &[u8], signature_hex: &str) -> bool {
    let computed = compute_hmac_sha256(secret, body);
    let expected = match hex::decode(signature_hex) {
        Ok(h) => h,
        Err(_) => return false,
    };
    // Constant-time comparison
    computed.len() == expected.len()
        && computed
            .iter()
            .zip(expected.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// Computes the HMAC-SHA256 hex digest for a payload.
///
/// Implements HMAC-SHA256 manually (RFC 2104) to avoid digest trait
/// compatibility issues between `hmac` 0.12 and `sha2`.
pub fn compute_hmac_sha256(secret: &[u8], body: &[u8]) -> Vec<u8> {
    const BLOCK_SIZE: usize = 64;

    let key = if secret.len() > BLOCK_SIZE {
        let mut hasher = Sha256::new();
        hasher.update(secret);
        hasher.finalize().to_vec()
    } else {
        secret.to_vec()
    };

    // ipad = key XOR 0x36
    let mut ipad = vec![0x36u8; BLOCK_SIZE];
    for (i, k) in key.iter().enumerate() {
        ipad[i] ^= k;
    }

    // opad = key XOR 0x5c
    let mut opad = vec![0x5cu8; BLOCK_SIZE];
    for (i, k) in key.iter().enumerate() {
        opad[i] ^= k;
    }

    // inner hash: H((key XOR ipad) || message)
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(body);
    let inner_hash = inner.finalize();

    // outer hash: H((key XOR opad) || inner_hash)
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(inner_hash);
    outer.finalize().to_vec()
}

/// Returns the HMAC-SHA256 hex digest.
pub fn compute_hmac_sha256_hex(secret: &[u8], body: &[u8]) -> String {
    hex::encode(compute_hmac_sha256(secret, body))
}

// ── Timestamp freshness ─────────────────────────────────────────────────

/// Rejects timestamps older than `max_age_secs`.
pub fn check_timestamp_freshness(
    timestamp_secs: i64,
    max_age_secs: i64,
    now: DateTime<Utc>,
) -> bool {
    let timestamp = match DateTime::from_timestamp(timestamp_secs, 0) {
        Some(t) => t,
        None => return false,
    };
    let age = now.signed_duration_since(timestamp);
    age.num_seconds().unsigned_abs() <= max_age_secs.unsigned_abs()
}

// ── Deduplication ───────────────────────────────────────────────────────

/// A lightweight, in-memory deduplication tracker.
pub struct Deduplicator {
    seen: std::sync::Mutex<lru::LruCache<String, ()>>,
}

impl Deduplicator {
    pub fn new(capacity: usize) -> Self {
        Self {
            seen: std::sync::Mutex::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(capacity)
                    .unwrap_or(std::num::NonZeroUsize::new(10_000).unwrap()),
            )),
        }
    }

    /// Returns `true` when this event has **not** been seen before.
    pub fn try_seen(&self, event_id: &str) -> bool {
        let mut cache = self.seen.lock().unwrap();
        if cache.contains(event_id) {
            false
        } else {
            cache.put(event_id.to_string(), ());
            true
        }
    }
}

// ── Authorization ───────────────────────────────────────────────────────

/// Describes who or what is requesting an action.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Requester {
    pub provider: String,
    pub user_id: String,
    pub team_id: Option<String>,
    pub installation_id: Option<String>,
}

/// Describes the target of an action.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Target {
    pub repository: String,
    pub action: String,
}

/// Outcome of an authorization check.
#[derive(Debug, Clone, serde::Serialize)]
pub enum AuthDecision {
    Allowed,
    Denied { reason: String },
}

/// An authorization policy that maps requesters to targets.
pub trait Authorizer: Send + Sync {
    fn authorize(&self, requester: &Requester, target: &Target) -> AuthDecision;
}

/// A simple allowlist-based authorizer.
pub struct AllowlistAuthorizer {
    allowed_users: Vec<String>,
    allowed_teams: Vec<String>,
    allowed_repos: Vec<String>,
    default_deny: bool,
}

impl AllowlistAuthorizer {
    pub fn new() -> Self {
        Self {
            allowed_users: Vec::new(),
            allowed_teams: Vec::new(),
            allowed_repos: Vec::new(),
            default_deny: true,
        }
    }

    pub fn allow_user(mut self, user: &str) -> Self {
        self.allowed_users.push(user.to_string());
        self
    }

    pub fn allow_team(mut self, team: &str) -> Self {
        self.allowed_teams.push(team.to_string());
        self
    }

    pub fn allow_repo(mut self, repo: &str) -> Self {
        self.allowed_repos.push(repo.to_string());
        self
    }

    pub fn with_default_deny(mut self, deny: bool) -> Self {
        self.default_deny = deny;
        self
    }
}

impl Default for AllowlistAuthorizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Authorizer for AllowlistAuthorizer {
    fn authorize(&self, requester: &Requester, target: &Target) -> AuthDecision {
        // Check repo allowlist
        if !self.allowed_repos.is_empty()
            && !self.allowed_repos.iter().any(|r| r == &target.repository)
        {
            return AuthDecision::Denied {
                reason: format!("repository '{}' is not in the allowlist", target.repository),
            };
        }

        // Check user allowlist
        if !self.allowed_users.is_empty()
            && !self.allowed_users.contains(&requester.user_id)
        {
            return AuthDecision::Denied {
                reason: format!("user '{}' is not in the allowlist", requester.user_id),
            };
        }

        // Check team allowlist
        if let Some(team_id) = &requester.team_id {
            if !self.allowed_teams.is_empty() && !self.allowed_teams.contains(team_id) {
                return AuthDecision::Denied {
                    reason: format!("team '{}' is not in the allowlist", team_id),
                };
            }
        }

        if self.default_deny && self.allowed_users.is_empty() && self.allowed_teams.is_empty() {
            return AuthDecision::Denied {
                reason: "no authorization rules configured".to_string(),
            };
        }

        AuthDecision::Allowed
    }
}

// ── Security Event Audit ────────────────────────────────────────────────

/// A record of a security-relevant event.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SecurityEvent {
    pub event_id: String,
    pub timestamp: DateTime<Utc>,
    pub event_type: SecurityEventType,
    pub requester: Requester,
    pub target: Option<Target>,
    pub decision: AuthDecision,
    pub details: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecurityEventType {
    WebhookReceived,
    SignatureVerificationFailed,
    SignatureVerificationPassed,
    TimestampRejected,
    DeduplicateRejected,
    AuthorizationDenied,
    AuthorizationAllowed,
    TaskCreated,
    TaskCancelled,
    TokenIssued,
    TokenRefreshed,
    RateLimited,
}

/// An audit log for security events.
#[async_trait::async_trait]
pub trait SecurityAudit: Send + Sync {
    async fn record(&self, event: SecurityEvent);
}

/// An audit log that writes to structured tracing output.
pub struct TracingAudit;

#[async_trait::async_trait]
impl SecurityAudit for TracingAudit {
    async fn record(&self, event: SecurityEvent) {
        tracing::info!(
            security.event_type = %serde_json::to_string(&event.event_type).unwrap_or_default(),
            security.event_id = %event.event_id,
            security.requester = %event.requester.user_id,
            security.details = %event.details,
            "security event"
        );
    }
}

// ── Unit tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hmac_manual_implementation() {
        // Test vector from RFC 4231, Test Case 2
        let key = b"Jefe";
        let data = b"what do ya want for nothing?";
        let expected = "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";

        let result = compute_hmac_sha256_hex(key, data);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_verify_hmac() {
        let key = b"secret";
        let body = b"hello world";
        let computed = compute_hmac_sha256_hex(key, body);
        assert!(verify_hmac_sha256(key, body, &computed));
        assert!(!verify_hmac_sha256(key, body, "deadbeef"));
    }

    #[test]
    fn test_timestamp_freshness() {
        let now = Utc::now();
        assert!(check_timestamp_freshness(now.timestamp(), 300, now));
        let old = now - chrono::Duration::minutes(10);
        assert!(!check_timestamp_freshness(old.timestamp(), 300, now));
    }

    #[test]
    fn test_deduplicator() {
        let d = Deduplicator::new(100);
        assert!(d.try_seen("event-1"));
        assert!(!d.try_seen("event-1"));
        assert!(d.try_seen("event-2"));
    }

    #[test]
    fn test_authorizer_allowlist() {
        let auth = AllowlistAuthorizer::new()
            .allow_user("U01")
            .allow_repo("org/payments");

        let requester = Requester {
            provider: "slack".to_string(),
            user_id: "U01".to_string(),
            team_id: None,
            installation_id: None,
        };
        let target = Target {
            repository: "org/payments".to_string(),
            action: "implement".to_string(),
        };

        assert!(matches!(auth.authorize(&requester, &target), AuthDecision::Allowed));

        let bad = Requester {
            provider: "slack".to_string(),
            user_id: "U02".to_string(),
            team_id: None,
            installation_id: None,
        };
        assert!(matches!(auth.authorize(&bad, &target), AuthDecision::Denied { .. }));
    }
}