use std::sync::Arc;

use anyhow::Result;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde_json::Value;
use std::net::SocketAddr;
use tower_http::cors::CorsLayer;
use tracing_subscriber::EnvFilter;

use groose_integrations::{
    github::{GitHubConfig, GitHubEventVerifier},
    slack::{
        commands::{parse_command, SlashCommandPayload, SlackCommand},
        SlackConfig, SlackEventVerifier,
    },
    trust::{AllowlistAuthorizer, Deduplicator, TracingAudit},
};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

struct AppState {
    slack_verifier: Option<Arc<SlackEventVerifier>>,
    github_verifier: Option<Arc<GitHubEventVerifier>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let allowed_repos: Vec<String> = env_or("GROOSE_ALLOWED_REPOS", "")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let allowed_github_users: Vec<String> = env_or("GROOSE_ALLOWED_GITHUB_USERS", "")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let allowed_slack_users: Vec<String> = env_or("GROOSE_ALLOWED_SLACK_USERS", "")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let allowed_slack_teams: Vec<String> = env_or("GROOSE_ALLOWED_SLACK_TEAMS", "")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut authorizer = AllowlistAuthorizer::new();
    for u in &allowed_github_users { authorizer = authorizer.allow_user(u); }
    for u in &allowed_slack_users { authorizer = authorizer.allow_user(u); }
    for r in &allowed_repos { authorizer = authorizer.allow_repo(r); }
    for t in &allowed_slack_teams { authorizer = authorizer.allow_team(t); }
    let authorizer = Arc::new(authorizer);
    let audit = Arc::new(TracingAudit);

    let slack_verifier = match (
        env_opt("SLACK_SIGNING_SECRET"),
        env_opt("SLACK_BOT_TOKEN"),
        env_opt("SLACK_APP_ID"),
        env_opt("SLACK_BOT_USER_ID"),
    ) {
        (Some(ss), Some(bt), Some(ai), Some(bu)) => {
            tracing::info!("Slack integration enabled");
            Some(Arc::new(SlackEventVerifier::new(
                SlackConfig { signing_secret: ss, bot_token: bt, app_id: ai, bot_user_id: bu },
                authorizer.clone(),
                Deduplicator::new(10_000),
                audit.clone(),
            )))
        }
        _ => {
            tracing::warn!("Slack: set SLACK_SIGNING_SECRET, SLACK_BOT_TOKEN, SLACK_APP_ID, SLACK_BOT_USER_ID");
            None
        }
    };

    let github_verifier = match (
        env_opt("GITHUB_WEBHOOK_SECRET"),
        env_opt("GITHUB_APP_ID"),
        env_opt("GITHUB_PRIVATE_KEY_PATH"),
    ) {
        (Some(ws), Some(ai), Some(pk_path)) => {
            match std::fs::read_to_string(&pk_path) {
                Ok(pem) => {
                    tracing::info!("GitHub integration enabled");
                    Some(Arc::new(GitHubEventVerifier::new(
                        GitHubConfig {
                            app_id: ai, webhook_secret: ws, private_key_pem: pem,
                            self_installation_id: None,
                            api_base_url: "https://api.github.com".to_string(),
                        },
                        authorizer.clone(),
                        Deduplicator::new(10_000),
                        audit.clone(),
                    )))
                }
                Err(e) => {
                    tracing::warn!("GitHub: cannot read private key at {}: {}", pk_path, e);
                    None
                }
            }
        }
        _ => {
            tracing::warn!("GitHub: set GITHUB_WEBHOOK_SECRET, GITHUB_APP_ID, GITHUB_PRIVATE_KEY_PATH");
            None
        }
    };

    let state = Arc::new(AppState { slack_verifier, github_verifier });

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/slack/events", post(receive_slack))
        .route("/api/slack/commands", post(receive_slash_command))
        .route("/api/github/webhook", post(receive_github))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let bind_addr: SocketAddr = env_or("GROOSE_BIND", "0.0.0.0:8080")
        .parse()
        .expect("invalid GROOSE_BIND address");
    tracing::info!("groose-webhooks listening on {}", bind_addr);

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// ── Health ───────────────────────────────────────────────────────────────

async fn health() -> Json<Value> {
    Json(serde_json::json!({
        "status": "ok",
        "service": "groose-webhooks",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

// ── Slack Events Handler (public & private channels) ─────────────────────

async fn receive_slack(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // Handle Slack URL verification challenge — needs no config
    if let Ok(payload) = serde_json::from_str::<Value>(&body) {
        if payload.get("type").and_then(|t| t.as_str()) == Some("url_verification") {
            if let Some(challenge) = payload.get("challenge").and_then(|c| c.as_str()) {
                return Ok(Json(serde_json::json!({ "challenge": challenge })));
            }
        }
    }

    let verifier = match state.slack_verifier.as_ref() {
        Some(v) => v,
        None => return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Slack not configured"})),
        )),
    };

    let signature = headers
        .get("x-slack-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing X-Slack-Signature"})),
        ))?;

    let timestamp = headers
        .get("x-slack-request-timestamp")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing X-Slack-Request-Timestamp"})),
        ))?;

    match verifier.verify(body.as_bytes(), signature, timestamp).await {
        Ok(Some(event)) => {
            tracing::info!(
                event_id = %event.event_id,
                event_type = %event.event_type,
                channel = ?event.channel_id,
                user = ?event.user_id,
                "verified Slack event"
            );

            // Parse the command and handle it
            let cmd = parse_command(&event.text);
            handle_slack_command(&event, &cmd, &state).await;

            Ok(Json(serde_json::json!({ "ok": true, "event_id": event.event_id })))
        }
        Ok(None) => Ok(Json(serde_json::json!({ "ok": true }))),
        Err(e) => {
            tracing::warn!("Slack event rejected: {:?}", e);
            Err((StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "verification failed"}))))
        }
    }
}

// ── Slash Commands Handler ───────────────────────────────────────────────

async fn receive_slash_command(
    State(state): State<Arc<AppState>>,
    _headers: HeaderMap,
    body: String,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let _verifier = match state.slack_verifier.as_ref() {
        Some(v) => v,
        None => return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Slack not configured"})),
        )),
    };

    // Slack sends slash commands as URL-encoded form data
    let params: std::collections::HashMap<String, String> = body
        .split('&')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let key = urlencoding::decode(parts.next()?).ok()?.into_owned();
            let val = urlencoding::decode(parts.next()?).ok()?.into_owned();
            Some((key, val))
        })
        .collect();

    let command = params.get("command").cloned().unwrap_or_default();
    let text = params.get("text").cloned().unwrap_or_default();
    let user_id = params.get("user_id").cloned().unwrap_or_default();
    let channel_id = params.get("channel_id").cloned().unwrap_or_default();
    let team_id = params.get("team_id").cloned().unwrap_or_default();
    let response_url = params.get("response_url").cloned().unwrap_or_default();

    tracing::info!(
        command = %command,
        text = %text,
        user = %user_id,
        channel = %channel_id,
        "slash command received"
    );

    let slash_payload = SlashCommandPayload {
        command,
        text,
        user_id,
        channel_id,
        team_id,
        response_url,
    };

    let _cmd = slash_payload.to_slack_command();

    // Return an immediate acknowledgement
    // Slash commands must respond within 3 seconds; we use response_url for delayed response
    let response = format!("Processing your request... I'll follow up with results shortly.");
    Ok(Json(serde_json::json!({
        "response_type": "ephemeral",
        "text": response,
    })))
}

// ── GitHub Webhooks Handler ──────────────────────────────────────────────

async fn receive_github(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let verifier = match state.github_verifier.as_ref() {
        Some(v) => v,
        None => return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "GitHub not configured"})),
        )),
    };

    let signature_256 = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing X-Hub-Signature-256"})),
        ))?;

    let delivery_id = headers
        .get("x-github-delivery")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing X-GitHub-Delivery"})),
        ))?;

    let event_name = headers.get("x-github-event").and_then(|v| v.to_str().ok()).unwrap_or("unknown");

    match verifier.verify(body.as_bytes(), signature_256, delivery_id, event_name).await {
        Ok(Some(event)) => {
            tracing::info!(
                delivery_id = %event.delivery_id,
                event_type = ?event.event_type,
                repo = %event.repository,
                "verified GitHub event"
            );
            Ok(Json(serde_json::json!({ "ok": true, "delivery_id": event.delivery_id })))
        }
        Ok(None) => Ok(Json(serde_json::json!({ "ok": true }))),
        Err(e) => {
            tracing::warn!("GitHub event rejected: {:?}", e);
            Err((StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "verification failed"}))))
        }
    }
}

// ── Command Router ───────────────────────────────────────────────────────

/// Route a parsed Slack command to the appropriate action.
async fn handle_slack_command(
    event: &groose_integrations::slack::VerifiedSlackEvent,
    cmd: &SlackCommand,
    state: &Arc<AppState>,
) {
    let channel_id = event.channel_id.as_deref().unwrap_or("unknown");
    let user_id = event.user_id.as_deref().unwrap_or("unknown");
    let bot_token = state.slack_verifier.as_ref().map(|v| v.config().bot_token.clone());

    match cmd {
        SlackCommand::FindDocumentation { topic, repo } => {
            tracing::info!(
                command = "find_documentation",
                topic = %topic,
                repo = ?repo,
                channel = %channel_id,
                user = %user_id,
                "SME: finding documentation"
            );
            post_slack_message(
                &bot_token,
                channel_id,
                &format!(
                    "📚 *Finding documentation for:* `{}`\n🔍 *In repository:* `{}`\n\nI'll search the codebase and documentation now.",
                    topic,
                    repo.as_deref().unwrap_or("(all repositories)")
                ),
            ).await;
        }

        SlackCommand::TakeStory { repo, issue } => {
            tracing::info!(
                command = "take_story",
                repo = %repo,
                issue = %issue,
                channel = %channel_id,
                user = %user_id,
                "SME: taking story"
            );
            post_slack_message(
                &bot_token,
                channel_id,
                &format!(
                    "🎯 *Taking story:* {} on `{}`\n⏳ Creating implementation plan...\n\nI'll analyze the issue, implement the changes, run tests, and open a PR.",
                    issue, repo
                ),
            ).await;
        }

        SlackCommand::ReviewPullRequest { repo, pr_number } => {
            tracing::info!(
                command = "review_pr",
                repo = %repo,
                pr_number = %pr_number,
                channel = %channel_id,
                user = %user_id,
                "SME: reviewing PR"
            );
            post_slack_message(
                &bot_token,
                channel_id,
                &format!(
                    "🔍 *Reviewing PR #{}* on `{}`\n\nI'll examine the changes, run tests, check for issues, and provide a detailed review.",
                    pr_number, repo
                ),
            ).await;
        }

        SlackCommand::Analyze { repo, goal } => {
            tracing::info!(
                command = "analyze",
                repo = %repo,
                goal = %goal,
                channel = %channel_id,
                user = %user_id,
                "SME: analyzing codebase"
            );
            post_slack_message(
                &bot_token,
                channel_id,
                &format!(
                    "📊 *Analyzing:* `{}`\n🎯 *Goal:* {}\n\nI'll examine the codebase structure, identify affected modules, and provide a scope estimate.",
                    repo, goal
                ),
            ).await;
        }

        SlackCommand::WorkOn { repo, instruction } => {
            tracing::info!(
                command = "work_on",
                repo = %repo,
                instruction = %instruction,
                channel = %channel_id,
                user = %user_id,
                "creating task"
            );
            post_slack_message(
                &bot_token,
                channel_id,
                &format!(
                    "⚡ *Working on:* `{}`\n📝 *Instruction:* {}\n\nStarting implementation...",
                    repo, instruction
                ),
            ).await;
        }

        SlackCommand::UseSkill { skill } => {
            post_slack_message(
                &bot_token,
                channel_id,
                &format!("🧠 *Skill activated:* `{}`", skill),
            ).await;
        }

        SlackCommand::Status => {
            post_slack_message(
                &bot_token,
                channel_id,
                "📋 *Status*\nNo active tasks in this channel.",
            ).await;
        }

        SlackCommand::Pause => {
            post_slack_message(
                &bot_token,
                channel_id,
                "⏸️ *Pausing task...*",
            ).await;
        }

        SlackCommand::Resume => {
            post_slack_message(
                &bot_token,
                channel_id,
                "▶️ *Resuming task...*",
            ).await;
        }

        SlackCommand::Cancel => {
            post_slack_message(
                &bot_token,
                channel_id,
                "🛑 *Cancelling task...*",
            ).await;
        }

        SlackCommand::Unknown { text } => {
            tracing::info!(
                command = "unknown",
                text = %text,
                channel = %channel_id,
                user = %user_id,
                "unrecognized command"
            );
            post_slack_message(
                &bot_token,
                channel_id,
                &format!(
                    "🤔 I didn't understand that. Try:\n\
                    • `find documentation for <topic>`\n\
                    • `take story on org/repo #123`\n\
                    • `review pr org/repo#42`\n\
                    • `analyze org/repo for <goal>`\n\
                    • `status` / `pause` / `resume` / `cancel`\n\n\
                    Or use slash commands like `/find-docs`, `/take-story`, `/review-pr`, `/analyze`"
                ),
            ).await;
        }
    }
}

// ── Slack Message Posting ────────────────────────────────────────────────

/// Post a message to a Slack channel using the bot token.
async fn post_slack_message(bot_token: &Option<String>, channel: &str, text: &str) {
    let token = match bot_token {
        Some(t) => t,
        None => return,
    };

    let client = reqwest::Client::new();
    let payload = serde_json::json!({
        "channel": channel,
        "text": text,
        "mrkdwn": true,
    });

    match client
        .post("https://slack.com/api/chat.postMessage")
        .header("Authorization", format!("Bearer {}", token))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) => {
            let body: Value = resp.json().await.unwrap_or_default();
            if body.get("ok").and_then(|o| o.as_bool()) != Some(true) {
                tracing::warn!("Slack API error: {:?}", body.get("error"));
            }
        }
        Err(e) => {
            tracing::warn!("Failed to post Slack message: {:?}", e);
        }
    }
}