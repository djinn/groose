# GitHub App Setup Guide

Use these values when creating your GitHub App at
https://github.com/settings/apps/new

## Basic Information

| Field | Value |
|-------|-------|
| **GitHub App name** | `groose-agent` |
| **Description** | AI-powered coding agent that implements stories, reviews PRs, and analyzes codebases from Slack |
| **Homepage URL** | `https://agent.spaceswordai.com` |

## User Authorization

| Field | Value |
|-------|-------|
| **Callback URL** | `https://agent.spaceswordai.com/api/github/callback` |
| **Expire user authorization tokens** | ✅ Checked (recommended) |
| **Request user authorization (OAuth) during installation** | ❌ Unchecked (we use installation tokens, not user tokens) |
| **Enable Device Flow** | ❌ Unchecked (not needed) |

## Post Installation

| Field | Value |
|-------|-------|
| **Setup URL** | Leave empty |
| **Redirect on update** | ❌ Unchecked |

## Webhook

| Field | Value |
|-------|-------|
| **Active** | ✅ Checked |
| **Webhook URL** | `https://agent.spaceswordai.com/api/github/webhook` |
| **Secret** | Generate a random 32+ character string (e.g. `openssl rand -hex 32`) |

## Repository Permissions

| Permission | Access | Why |
|------------|--------|-----|
| **Contents** | Read & write | Clone repos, create branches, commit code |
| **Issues** | Read & write | Read issue descriptions, comment on issues |
| **Pull requests** | Read & write | Create PRs, review PRs, comment on PRs |
| **Checks** | Read | Read CI status (optional but recommended) |
| **Metadata** | Read | Automatic — required by all apps |

## Organization Permissions

| Permission | Access | Why |
|------------|--------|-----|
| **Members** | Read | Check if user is in org (optional) |

## Account Permissions

None needed.

## Enterprise Permissions

None needed.

## Subscribe to Events

Check these:

- [x] **Issues** — New issues assigned to the agent
- [x] **Pull request** — PR opened, ready for review
- [x] **Pull request review** — PR review requested
- [x] **Pull request review comment** — Comments on PR reviews
- [x] **Check run** — CI completes (optional)
- [x] **Check suite** — CI suite completes (optional)
- [ ] **Push** — Not needed (we clone on demand)
- [ ] **Release** — Not needed

## Where can this GitHub App be installed?

| Option | Recommendation |
|--------|----------------|
| **Only on this account** | ✅ Recommended for production — restrict to your org |
| **Any account** | Only if you want others to use it |

## After Creating

1. **Generate a Private Key** (scroll to bottom, click "Generate a private key")
   - Download the `.pem` file
   - Upload to server: `scp app.private-key.pem root@209.38.24.106:/opt/groose/secrets/github-private-key.pem`

2. **Note the App ID** (shown at the top of the settings page)

3. **Install the App** on your repositories

4. **Update server config:**
   ```bash
   ssh root@209.38.24.106
   cat >> /etc/groose/env << 'EOF'
   GITHUB_WEBHOOK_SECRET=<your webhook secret>
   GITHUB_APP_ID=<your app id>
   GITHUB_PRIVATE_KEY_PATH=/opt/groose/secrets/github-private-key.pem
   GROOSE_ALLOWED_REPOS=org/repo1,org/repo2
   EOF
   systemctl restart groose-webhooks
   ```

5. **Verify:**
   ```bash
   curl -s https://agent.spaceswordai.com/health
   # Should show GitHub integration enabled in logs
   ssh root@209.38.24.106 "journalctl -u groose-webhooks --since '1 min ago' | grep -i github"
   ```
