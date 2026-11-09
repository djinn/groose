# Connecting groose to Slack and GitHub

> **Phase 3 (GitHub App) and Phase 4 (Slack App) integration guide.**
> Covers setup, configuration, and deployment of the Slack and GitHub Apps
> that allow groose to receive events and post results.

---

## Overview

Once the standalone engine is running (`groose run --spec task.json --workspace ./repo`),
the next step is to wire up **instruction transports** so that groose can
respond to Slack mentions and GitHub webhooks.

The architecture is:

```
┌──────────────┐   webhooks   ┌──────────────────┐   TaskSpec   ┌──────────────┐
│ Slack App    │ ──────────>  │  groose-webhooks  │ ──────────>  │  groose task │
│ (Events API) │              │  (verification,   │              │  orchestrator│
└──────────────┘              │   auth, dedup)    │              └──────────────┘
                              │                   │                    │
┌──────────────┐   webhooks   │  trust gates:     │                    ▼
│ GitHub App   │ ──────────>  │  1. Timestamp     │              ┌──────────────┐
│ (Webhooks)   │              │  2. HMAC-SHA256   │              │  coding engine│
└──────────────┘              │  3. Bot-loop      │              │  (workspace)  │
                              │     prevention    │              └──────────────┘
                              │  4. Deduplication │                    │
                              │  5. Authorization │                    ▼
                              │  6. Audit trail   │              ┌──────────────┐
                              └──────────────────┘              │  GitHub PR / │
                                                                │  Slack msg   │
                                                                └──────────────┘
```

Slack and GitHub are **interchangeable input adapters**. The coding engine
never sees a Slack message or a GitHub webhook — only a canonical `TaskSpec`.

---

## 1. GitHub App Setup (Phase 3)

### 1.1 Create the GitHub App

1. Go to **GitHub Settings → Developer settings → GitHub Apps → New GitHub App**
   (or visit `https://github.com/settings/apps/new`).

2. Fill in:

   | Field | Value |
   |-------|-------|
   | GitHub App name | `groose-org` (use your org name) |
   | Homepage URL | `https://github.com/your-org/groose` |
   | Webhook URL | `https://groose.example.com/api/github/webhook` |
   | Webhook secret | Generate a strong random secret: `openssl rand -hex 32` |
   | Webhook active | ✅ **checked** |

3. **Repository permissions:**

   | Permission | Access |
   |-----------|--------|
   | Contents | **Read & write** |
   | Issues | **Read & write** |
   | Pull requests | **Read & write** |
   | Checks | **Read** |
   | Metadata | **Read** (always granted) |

4. **Subscribe to events:**

   - [x] Issues
   - [x] Pull request
   - [x] Pull request review
   - [x] Pull request review comment
   - [x] Check run
   - [x] Check suite

5. **Where can this GitHub App be installed?**
   - Select **Any account** or **Only this account** depending on your
     deployment scope.

6. Click **Create GitHub App**.

7. **After creation:**
   - Generate a **private key** (PEM file). Save it securely — it is
     needed for installation token generation.
   - Note your **App ID** (a numeric ID shown on the app settings page).

### 1.2 Install the GitHub App

1. On the GitHub App settings page, click **Install App** → **Install**.
2. Select the repositories the app should access.
3. Note the **Installation ID** (visible in the URL after installation).

### 1.3 Configure groose

```yaml
# config/github-app.yaml
app_id: "123456"
webhook_secret: "your-openssl-hex-32-secret"
private_key_path: "/etc/groose/secrets/github-private-key.pem"
self_installation_id: "9876543"        # optional, for bot-loop prevention
api_base_url: "https://api.github.com" # default
```

Verify the webhook works:

```bash
curl -X POST https://groose.example.com/api/github/webhook \
  -H "Content-Type: application/json" \
  -H "X-GitHub-Event: ping" \
  -H "X-Hub-Signature-256: sha256=..." \
  -d '{"zen": "testing"}'
```

### 1.4 GitHub Event Flow

```
1. Issue created / assigned / labeled
2. GitHub sends webhook to groose-webhooks
3. groose-webhooks:
   a. Verifies HMAC-SHA256 signature
   b. Checks timestamp freshness (5 min window)
   c. Deduplicates by X-GitHub-Delivery GUID
   d. Classifies event (issues.assigned → IssuesAssigned)
   e. Checks authorization (user + repo allowlists)
   f. Records audit event
4. Creates TaskSpec and submits to orchestrator
5. Orchestrator queues and schedules coding engine execution
6. Coding engine:
   a. Creates isolated workspace (git worktree in sandbox)
   b. Runs Observe → Plan → Edit → Test → Evaluate → Repair loop
   c. Checkpoints progress at safe boundaries
   d. Verifies acceptance criteria
7. Engine delivers:
   a. Creates pull request on GitHub
   b. Posts comment on the triggering issue
   c. Emits event for Slack notification outbox
```

---

## 2. Slack App Setup (Phase 4)

### 2.1 Create the Slack App

1. Go to **https://api.slack.com/apps → Create New App → From scratch**.

2. Fill in:

   | Field | Value |
   |-------|-------|
   | App name | `groose` |
   | Workspace | Select your development workspace |

3. Click **Create App**.

### 2.2 Configure Bot Token Scopes

1. Navigate to **OAuth & Permissions** → **Bot Token Scopes**.
2. Add these scopes:

   | Scope | Purpose |
   |-------|---------|
   | `app_mentions:read` | Receive @groose mentions |
   | `channels:history` | Read channel messages |
   | `channels:read` | Read channel metadata |
   | `chat:write` | Post messages and updates |
   | `chat:write.public` | Post in public channels without being invited |
   | `links:read` | Read link previews |
   | `reactions:read` | Read emoji reactions |
   | `reactions:write` | Add emoji reactions for acknowledgements |
   | `users:read` | Read user display names |

3. **Install the app to your workspace:**
   - Click **Install to Workspace** → **Allow**.
   - Copy the **Bot User OAuth Token** (starts with `xoxb-`).

### 2.3 Enable Events API

1. Navigate to **Event Subscriptions** → **Enable Events** → **On**.

2. **Request URL:**
   ```
   https://groose.example.com/api/slack/events
   ```

3. **Subscribe to bot events** (minimum set):

   | Event | Purpose |
   |-------|---------|
   | `app_mention` | Handle @groose mentions |

   For Phase 4, `app_mention` alone is sufficient. Add `message.channels` and
   `message.groups` for enhanced context.

4. Click **Save Changes** and **Reinstall Your App** if prompted.

### 2.4 Configure Interactivity (optional but recommended)

1. Navigate to **Interactivity & Shortcuts** → **On**.
2. **Request URL:**
   ```
   https://groose.example.com/api/slack/interactions
   ```
3. Add shortcuts for:
   - Create task from selection
   - Show task status
   - Cancel task

### 2.5 Configure groose

```yaml
# config/slack-app.yaml
signing_secret: "your-slack-signing-secret"
bot_token: "xoxb-your-bot-token"
app_id: "A0123456789"
bot_user_id: "U0BOT12345"
```

Find these values in the Slack App settings:

| Value | Location |
|-------|----------|
| `signing_secret` | **Basic Information** → **App Credentials** → **Signing Secret** |
| `bot_token` | **OAuth & Permissions** → **Bot User OAuth Token** |
| `app_id` | **Basic Information** → **App Credentials** → **App ID** |
| `bot_user_id` | **OAuth & Permissions** → **Bot User ID** (appears after install) |

### 2.6 Slack Command Format

groose recognises natural-language mentions:

| Command | Purpose |
|---------|---------|
| `@groose work on org/repo. Implement issue #142.` | Create a new task |
| `@groose use skill developer` | Attach a skill to the current thread |
| `@groose status` | Report progress of the current thread's task |
| `@groose pause` | Pause task at the next safe checkpoint |
| `@groose resume` | Resume a paused task |
| `@groose cancel` | Cancel the current task |
| `@groose fix PR #42` | Address review or CI failures on PR #42 |
| `@groose integrate PR #42` | Inspect and merge PR #42 according to policy |
| `@groose explain` | Summarise the task's work and evidence |

**Thread binding:** Once a task is created from a Slack thread, every
subsequent reply in that thread is treated as an amendment to the same
task, unless it explicitly starts a new task.

---

## 3. Configuration Reference

### 3.1 Integration Config File

```yaml
# config/groose-integrations.yaml
integrations:
  github:
    enabled: true
    app:
      app_id: "123456"
      webhook_secret: "${GITHUB_WEBHOOK_SECRET}"
      private_key_path: "/etc/groose/secrets/github-private-key.pem"
      api_base_url: "https://api.github.com"
    authorization:
      allowed_users:
        - "github-user-1"
        - "github-user-2"
      allowed_repos:
        - "org/payments"
        - "org/identity"
      default_deny: true

  slack:
    enabled: true
    app:
      signing_secret: "${SLACK_SIGNING_SECRET}"
      bot_token: "${SLACK_BOT_TOKEN}"
      app_id: "A0123456789"
      bot_user_id: "U0BOT12345"
    authorization:
      allowed_users:
        - "U01ABCDEFG"
        - "U02HIJKLMN"
      allowed_teams:
        - "T0TEAM1234"
      default_deny: true

  server:
    bind: "0.0.0.0:8080"
    slack_events_path: "/api/slack/events"
    slack_interactions_path: "/api/slack/interactions"
    github_webhook_path: "/api/github/webhook"
    health_path: "/health"
    tls_cert_path: "/etc/groose/tls/cert.pem"
    tls_key_path: "/etc/groose/tls/key.pem"
```

### 3.2 Trust Configuration

```yaml
# config/trust.yaml
trust:
  max_age_secs: 300
  deduplication_capacity: 10000
  default_deny: true
  audit:
    backend: "postgresql"
    table: "security_events"
```

### 3.3 Webhook Verification (Rust)

```rust
use groose_integrations::trust::{Deduplicator, AllowlistAuthorizer, TracingAudit};
use groose_integrations::slack::{SlackConfig, SlackEventVerifier};
use std::sync::Arc;

let config = SlackConfig {
    signing_secret: "your_signing_secret".into(),
    bot_token: "xoxb-...".into(),
    app_id: "A01...".into(),
    bot_user_id: "U0BOT...".into(),
};

let authorizer = Arc::new(
    AllowlistAuthorizer::new()
        .allow_user("U01...")
        .allow_team("T0..."),
);

let deduplicator = Deduplicator::new(10000);
let audit = Arc::new(TracingAudit);

let verifier = SlackEventVerifier::new(config, authorizer, deduplicator, audit);

// In your webhook handler:
let signature = request.headers().get("X-Slack-Signature").unwrap();
let timestamp = request.headers().get("X-Slack-Request-Timestamp").unwrap();
let body = request.bytes().await.unwrap();

match verifier.verify(&body, signature, timestamp).await {
    Ok(Some(event)) => handle_slack_event(event).await,
    Ok(None) => { /* duplicate or unauthorized — return 200 OK */ }
    Err(e) => { /* security failure — return 401 */ }
}
```

---

## 4. Deployment

### 4.1 Docker Compose

```yaml
version: "3.9"
services:
  webhooks:
    image: groose/webhooks:latest
    ports:
      - "8080:8080"
    environment:
      RUST_LOG: info
      GROOSE_GITHUB_APP_ID: "${GITHUB_APP_ID}"
      GROOSE_GITHUB_WEBHOOK_SECRET: "${GITHUB_WEBHOOK_SECRET}"
      GROOSE_SLACK_SIGNING_SECRET: "${SLACK_SIGNING_SECRET}"
      GROOSE_SLACK_BOT_TOKEN: "${SLACK_BOT_TOKEN}"
    volumes:
      - ./config:/etc/groose/config:ro
      - ./secrets:/etc/groose/secrets:ro
    depends_on:
      - postgres

  orchestrator:
    image: groose/orchestrator:latest
    environment:
      DATABASE_URL: "postgres://groose:password@postgres/groose"
    volumes:
      - ./config:/etc/groose/config:ro
    depends_on:
      - postgres

  worker:
    image: groose/worker:latest
    environment:
      DATABASE_URL: "postgres://groose:password@postgres/groose"
    volumes:
      - ./config:/etc/groose/config:ro
    depends_on:
      - postgres

  postgres:
    image: postgres:16
    environment:
      POSTGRES_USER: groose
      POSTGRES_PASSWORD: "${POSTGRES_PASSWORD}"
    volumes:
      - pgdata:/var/lib/postgresql/data
      - ./migrations:/docker-entrypoint-initdb.d

volumes:
  pgdata:
```

### 4.2 Environment Variables

| Variable | Description | Required |
|----------|-------------|----------|
| `GITHUB_APP_ID` | GitHub App numeric ID | Yes |
| `GITHUB_WEBHOOK_SECRET` | GitHub webhook secret | Yes |
| `GITHUB_PRIVATE_KEY_PATH` | Path to GitHub App PEM key | Yes |
| `SLACK_SIGNING_SECRET` | Slack App signing secret | Yes |
| `SLACK_BOT_TOKEN` | Slack Bot User OAuth token | Yes |
| `SLACK_APP_ID` | Slack App ID | Yes |
| `SLACK_BOT_USER_ID` | Slack Bot User ID | Yes |
| `DATABASE_URL` | PostgreSQL connection string | Phase 2+ |
| `RUST_LOG` | Log level (info, debug, trace) | No |

---

## 5. Testing the Integration

### 5.1 End-to-End Test Scenarios

| Scenario | How to Test |
|----------|-------------|
| GitHub issue triggers a task | Create an issue in an allowed repo, assign it to `groose[bot]` |
| Slack mention triggers a task | `@groose work on org/repo. Implement the CI check.` |
| Deduplication | Use Slack's "Send again" — only one task created |
| Bot-loop prevention | groose comments on a PR — no task is created from its own comment |
| Authorization | Unauthorised user mentions groose — event is silently dropped |
| Replay attack | Re-send an old webhook with the same signature — rejected |
| Crash recovery | Kill worker mid-task, restart — task resumes from checkpoint |

### 5.2 Local Testing with ngrok

```bash
# Start ngrok to expose localhost
ngrok http 8080

# Copy the ngrok URL (https://abc123.ngrok.io)
# Use this URL in:
#   - Slack App Event Subscriptions → Request URL
#   - GitHub App → Webhook URL

# Start groose webhooks
DATABASE_URL=postgres://localhost/groose \
  GITHUB_APP_ID=... \
  GITHUB_WEBHOOK_SECRET=... \
  SLACK_SIGNING_SECRET=... \
  SLACK_BOT_TOKEN=xoxb-... \
  cargo run -p groose-webhooks
```

### 5.3 Webhook Signature Verification

**Slack:**
```bash
TIMESTAMP=$(date +%s)
BODY='{"type":"url_verification","challenge":"test123"}'
SIG_BASESTRING="v0:${TIMESTAMP}:${BODY}"
SIGNATURE="v0=$(echo -n "$SIG_BASESTRING" | openssl dgst -sha256 -hmac "your_signing_secret" | cut -d' ' -f2)"

curl -X POST https://groose.example.com/api/slack/events \
  -H "Content-Type: application/json" \
  -H "X-Slack-Signature: $SIGNATURE" \
  -H "X-Slack-Request-Timestamp: $TIMESTAMP" \
  -d "$BODY"
```

**GitHub:**
```bash
BODY='{"action":"opened","issue":{"number":1},"repository":{"full_name":"org/repo"}}'
SIGNATURE="sha256=$(echo -n "$BODY" | openssl dgst -sha256 -hmac "your_webhook_secret" | cut -d' ' -f2)"

curl -X POST https://groose.example.com/api/github/webhook \
  -H "Content-Type: application/json" \
  -H "X-GitHub-Event: issues" \
  -H "X-Hub-Signature-256: $SIGNATURE" \
  -H "X-GitHub-Delivery: $(uuidgen)" \
  -d "$BODY"
```

---

## 6. Security Checklist

Before enabling integrations in production, verify each item:

- [ ] **HMAC verification** — All webhooks verified with provider signing secret
- [ ] **Timestamp freshness** — Events older than 5 minutes rejected
- [ ] **Deduplication** — Slack / GitHub retries do not create duplicate tasks
- [ ] **Bot-loop prevention** — groose does not act on its own events
- [ ] **Authorization** — Only allowlisted users, teams, repos; default deny
- [ ] **Installation tokens** — Short-lived, repository-scoped GitHub tokens
- [ ] **Secret management** — Secrets never logged, exposed in prompts, or written to PRs
- [ ] **Audit trail** — Every security decision recorded
- [ ] **Rate limiting** — Excessive webhook deliveries throttled
- [ ] **TLS** — Webhook endpoints served only over HTTPS

---

## 7. Troubleshooting

| Symptom | Likely Cause | Resolution |
|---------|-------------|------------|
| Slack URL verification fails | Signing secret mismatch | Verify `signing_secret` matches Slack App settings |
| GitHub 401 on token use | Private key expired or wrong installation ID | Regenerate key, reinstall app, verify ID |
| Duplicate tasks created | Deduplication not working | Check `event_id` / `X-GitHub-Delivery` uniqueness |
| groose responds to itself | Bot-loop prevention not configured | Set `app_id` and `bot_user_id` in config |
| Tasks queued but never start | Worker not running | Check worker logs, database, lease expiry |
| Webhook requests timeout | Network or TLS issues | Verify ngrok (dev) or reverse proxy (prod) |
| Event classified Unknown | Event not in classification table | Check SPECS.md §9 for supported types |

## References

- [Trust Model Documentation](./trust-model.md)
- [Groose Architecture](./architecture.md)
- [Slack Events API](https://api.slack.com/apis/connections/events-api)
- [GitHub App Webhooks](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/using-webhooks-with-github-apps)
- [GitHub App Installation Tokens](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-an-installation-access-token-for-a-github-app)
- [SPECS.md — Groose Implementation Specification](../SPECS.md)