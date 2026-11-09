# Groose Trust Model

## Overview

Groose provides a layered security architecture for receiving events from
Slack and GitHub and for issuing actions on behalf of users. Every event
passes through six trust gates before creating or modifying a task.

```
Inbound Webhook
     │
     ▼
┌──────────────────────┐
│ 1. Timestamp         │  Replay protection
│    Freshness         │  (max 5 minute window)
└────────┬─────────────┘
         ▼
┌──────────────────────┐
│ 2. HMAC Signature    │  Request body authenticity
│    Verification      │  (Slack: v0 HMAC-SHA256)
└────────┬─────────────┘   (GitHub: sha256 HMAC-SHA256)
         ▼
┌──────────────────────┐
│ 3. Payload Parsing   │  Well-formed JSON
└────────┬─────────────┘
         ▼
┌──────────────────────┐
│ 4. Bot-loop          │  Ignore events from
│    Prevention        │  our own app/bot user
└────────┬─────────────┘
         ▼
┌──────────────────────┐
│ 5. Deduplication     │  Slack event_id /
│                      │  GitHub X-GitHub-Delivery
└────────┬─────────────┘
         ▼
┌──────────────────────┐
│ 6. Authorization     │  Allowlist-based
│                      │  (user, team, repo)
└────────┬─────────────┘
         ▼
   Verified Event → TaskSpec
```

## Trust Gates in Detail

### 1. Timestamp Freshness (`trust::check_timestamp_freshness`)

Prevents replay attacks by rejecting events with timestamps older than
5 minutes (300 seconds).

- **Slack:** `X-Slack-Request-Timestamp` header value (Unix epoch seconds)
- **GitHub:** `created_at` / `updated_at` fields from the event payload

### 2. HMAC Signature Verification (`trust::verify_hmac_sha256`)

Ensures the request body was signed by the integration provider.

Uses a manual RFC 2104 HMAC-SHA256 implementation to avoid type-level
compatibility issues between the `hmac` and `sha2` crates. The
implementation:

1. Pads or hashes the key to 64 bytes (SHA-256 block size)
2. XORs with ipad (0x36) and opad (0x5c)
3. Computes inner hash: `SHA256((key ⊕ ipad) || message)`
4. Computes outer hash: `SHA256((key ⊕ opad) || inner_hash)`

**Slack** signature format: `v0=<hex>` over `v0:{timestamp}:{body}`

**GitHub** signature format: `sha256=<hex>` over the raw request body

Testing verified against RFC 4231 Test Case 2.

### 3. Bot-Loop Prevention

Groose never acts on events it generated:

- **Slack:** Compares `event.bot_id` and `event.app_id` against the
  configured bot identity
- **GitHub:** Compares the event's `installation.id` against the app's
  own installation ID

### 4. Deduplication (`trust::Deduplicator`)

Both Slack and GitHub may retry webhook deliveries. Each event carries an
idempotency key:

- **Slack:** `event.event_id` (from the event payload)
- **GitHub:** `X-GitHub-Delivery` header (a GUID)

Deduplication is enforced via an LRU cache (in-memory for Phase 1-2,
backed by PostgreSQL for Phase 3+).

### 5. Authorization (`trust::AllowlistAuthorizer`)

Three-tier allowlist model:

| Level | Identifier | Source |
|-------|-----------|--------|
| User | Slack user ID / GitHub user ID | `event.user` / `sender.id` |
| Team | Slack team ID / GitHub org | `team_id` / `installation.id` |
| Repo | `owner/repo` | `repository.full_name` |

Default is **deny**. All three lists are independently configured; an
empty list means "any value is accepted" for that dimension.

## Integration-Specific Trust Details

### Slack (`slack::SlackEventVerifier`)

Used when a user mentions the Groose Slack App in a channel or thread:

1. Verify HMAC-SHA256 with `v0:{timestamp}:{body}` signature base string
2. Parse the Slack Events API `event_callback` payload
3. Check `event.user` is not the bot's own user ID
4. Deduplicate by `event.event_id`
5. Check user and team against allowlists
6. Return `VerifiedSlackEvent` with extracted text and channel context

### GitHub (`github::GitHubEventVerifier`)

Used for all GitHub webhooks (issues, PRs, check runs):

1. Verify HMAC-SHA256 with the `sha256=<hex>` signature
2. Parse the webhook payload
3. Classify the event into a Groose action type (SPECS.md §9)
4. Deduplicate by `X-GitHub-Delivery` GUID
5. Check payload timestamps for replay protection
6. Check sender and repository against allowlists
7. Return `VerifiedGitHubEvent` with event classification

#### Classified Event Types

| GitHub Event | Action | Groose Type |
|---|---|---|
| `issues` | `assigned` (to groose[bot]) | `IssuesAssigned` |
| `issues` | `labeled` (with `groose:implement`) | `IssuesLabeled` |
| `pull_request_review` | `submitted` (changes_requested) | `PullRequestReviewRequestedChanges` |
| `pull_request_review_comment` | `created` | `PullRequestReviewComment` |
| `check_run` | (conclusion = failure/timeout/cancelled) | `CheckRunFailure` |
| `check_suite` | (conclusion = failure/timeout/cancelled) | `CheckSuiteFailure` |
| `pull_request` | `closed` + merged = true | `PullRequestMerged` |
| `pull_request` | `closed` + state = closed | `PullRequestClosed` |

### GitHub Installation Tokens

GitHub App installation tokens are generated with:

- Minimal repository permissions (contents: write, pull_requests: write,
  issues: read/write, checks: read)
- Short TTL (1 hour maximum)
- Scoped to a single repository per token

Implementation uses JWT authentication:

1. Sign a JWT with the app's private key (RS256)
2. Exchange JWT for an installation access token via GitHub API
3. Use token for all subsequent API calls for that task

## Audit Trail (`trust::SecurityAudit`)

Every security decision is recorded as a `SecurityEvent`:

```json
{
  "event_id": "uuid",
  "timestamp": "2026-11-09T00:00:00Z",
  "event_type": "signature_verification_passed",
  "requester": { "provider": "github", "user_id": "12345" },
  "decision": "allowed",
  "details": "HMAC signature verified"
}
```

Events include: webhook received, signature passed/failed, timestamp
rejected, duplicate dropped, authorization allowed/denied, task created/
cancelled, token issued/refreshed, rate limited.

## Crate Structure

```
groose-integrations/
├── src/
│   ├── lib.rs              — module exports
│   ├── trust/
│   │   └── mod.rs          — HMAC verification, timestamps, dedup,
│   │                          authorization, audit (shared core)
│   ├── slack/
│   │   └── mod.rs          — SlackEventVerifier, VerifiedSlackEvent
│   ├── github/
│   │   └── mod.rs          — GitHubEventVerifier, VerifiedGitHubEvent,
│   │                          event classification, token generation
│   └── policy/
│       └── mod.rs          — PolicyEvaluator, event→TaskSpec bridge
└── Cargo.toml
```

## Security Properties

| Property | Mechanism |
|---|---|
| Authenticity (webhook) | HMAC-SHA256 with provider signing secret |
| Integrity | HMAC covers entire request body |
| Replay protection | Timestamp freshness (5 min window) |
| Idempotency | Deduplication by event/delivery ID |
| Authorization | Three-tier allowlist (user, team, repo) |
| Self-triggering | Bot-loop prevention by app/user ID |
| Least privilege | Scoped installation tokens per task |
| Auditability | Structured SecurityEvent logging |