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

mod deepseek;
mod intent;
mod slackfmt;

use groose_integrations::{
    github::{GitHubConfig, GitHubEventVerifier},
    slack::{
        commands::{parse_command, SlashCommandPayload, SlackCommand},
        SlackConfig, SlackEventVerifier,
    },
    trust::{AllowlistAuthorizer, Deduplicator, TracingAudit},
};
use groose_execution::engine::{CodingEngine, ExecutionContext, ModelProvider, ExecutionOutcome};
use groose_execution::workspace::LocalWorkspace;
use groose_execution::control::NoopControlReceiver;
use groose_execution::checkpoints::FileCheckpointStore;
use groose_execution::verifier::BasicVerifier;
use groose_task::TaskSpec;

use crate::intent::ActionType;

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
            // Channel denylist — ignore events from excluded channels
            let denied: Vec<String> = env_or("GROOSE_DENIED_CHANNELS", "")
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if let Some(ref ch) = event.channel_id {
                if denied.contains(ch) {
                    tracing::info!(channel = %ch, "event ignored — channel is denylisted");
                    return Ok(Json(serde_json::json!({ "ok": true, "ignored": "denylist" })));
                }
            }

            tracing::info!(
                event_id = %event.event_id,
                event_type = %event.event_type,
                channel = ?event.channel_id,
                user = ?event.user_id,
                "verified Slack event"
            );

            // Parse the command and handle it — spawn in background
            // so we can respond to Slack immediately (within 3s window)
            let decoded = urlencoding::decode(&event.text).unwrap_or(std::borrow::Cow::Borrowed(&event.text));
            let cmd = parse_command(&decoded);
            let state_clone = state.clone();
            let event_clone = event.clone();
            tokio::spawn(async move {
                handle_slack_command(&event_clone, &cmd, &state_clone).await;
            });

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
        user_id: user_id.clone(),
        channel_id: channel_id.clone(),
        team_id,
        response_url,
    };

    let cmd = slash_payload.to_slack_command();

    // Channel denylist — ignore commands from excluded channels
    let denied: Vec<String> = env_or("GROOSE_DENIED_CHANNELS", "")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if denied.contains(&channel_id) {
        tracing::info!(channel = %channel_id, "slash command ignored — channel is denylisted");
        return Ok(Json(serde_json::json!({
            "response_type": "ephemeral",
            "text": "Mule Buddy isn't available in this channel.",
        })));
    }

    // Execute the command in background (Slack needs response within 3 seconds)
    let state_clone = state.clone();
    tokio::spawn(async move {
        handle_slash_command_impl(&cmd, &channel_id, &user_id, &state_clone).await;
    });

    // Return immediate acknowledgement
    Ok(Json(serde_json::json!({
        "response_type": "ephemeral",
        "text": "Processing your request... I'll follow up with results shortly.",
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

/// Route a parsed command from a Slack mention event.
async fn handle_slack_command(
    event: &groose_integrations::slack::VerifiedSlackEvent,
    cmd: &SlackCommand,
    state: &Arc<AppState>,
) {
    let channel_id = event.channel_id.as_deref().unwrap_or("unknown");
    let user_id = event.user_id.as_deref().unwrap_or("unknown");
    let bot_token = state.slack_verifier.as_ref().map(|v| v.config().bot_token.clone());
    handle_command_impl(cmd, channel_id, user_id, &bot_token, state).await;
}

/// Route a parsed command from a slash command.
async fn handle_slash_command_impl(
    cmd: &SlackCommand,
    channel_id: &str,
    user_id: &str,
    state: &Arc<AppState>,
) {
    let bot_token = state.slack_verifier.as_ref().map(|v| v.config().bot_token.clone());
    handle_command_impl(cmd, channel_id, user_id, &bot_token, state).await;
}

/// Core command handler shared by mentions and slash commands.
async fn handle_command_impl(
    cmd: &SlackCommand,
    channel_id: &str,
    user_id: &str,
    bot_token: &Option<String>,
    _state: &Arc<AppState>,
) {
    let deepseek_key = match std::env::var("DEEPSEEK_API_KEY") {
        Ok(k) => k,
        Err(_) => {
            post_slack_message(bot_token, channel_id, "⚠️ DeepSeek API key not configured. Set DEEPSEEK_API_KEY to enable AI processing.").await;
            return;
        }
    };

    // Convert the parsed command to a user message string
    let user_message = match cmd {
        SlackCommand::FindDocumentation { topic, repo } => {
            format!("find documentation for {} in {}", topic, repo.as_deref().unwrap_or("general knowledge"))
        }
        SlackCommand::TakeStory { repo, issue } => {
            format!("implement {} on {}", issue, repo)
        }
        SlackCommand::ReviewPullRequest { repo, pr_number } => {
            format!("review pr on {} #{}", repo, pr_number)
        }
        SlackCommand::Analyze { repo, goal } => {
            format!("analyze {} for {}", repo, goal)
        }
        SlackCommand::WorkOn { repo, instruction } => {
            format!("work on {}: {}", repo, instruction)
        }
        SlackCommand::UseSkill { skill } => {
            format!("use skill {}", skill)
        }
        SlackCommand::Status => "status".to_string(),
        SlackCommand::Pause => "pause".to_string(),
        SlackCommand::Resume => "resume".to_string(),
        SlackCommand::Cancel => "cancel".to_string(),
        SlackCommand::Unknown { text } => text.clone(),
    };

    // Classify intent using the model
    let intent = crate::intent::classify_intent(
        &user_message,
        Some(channel_id.to_string()),
        Some(user_id.to_string()),
        &deepseek_key,
    ).await;

    tracing::info!(
        action = ?intent.action,
        repo = ?intent.repo,
        needs_repo = intent.needs_repo,
        query = %intent.query,
        "classified intent"
    );

    // Show the user what we understood
    let emoji = intent.action.emoji();
    let label = intent.action.label();
    let repo_str = intent.repo.as_deref().unwrap_or("(no repo)");

    if intent.action == ActionType::Status || intent.action == ActionType::Pause 
        || intent.action == ActionType::Resume || intent.action == ActionType::Cancel {
        // Simple actions - no agent needed
        let msg = match intent.action {
            ActionType::Status => "📋 *Status*\nNo active tasks in this channel.".to_string(),
            ActionType::Pause => "⏸️ *Pausing task...*".to_string(),
            ActionType::Resume => "▶️ *Resuming task...*".to_string(),
            ActionType::Cancel => "🛑 *Cancelling task...*".to_string(),
            _ => unreachable!(),
        };
        post_slack_message(bot_token, channel_id, &msg).await;
        return;
    }

    // For general chat / unknown, just ask the model directly
    if intent.action == ActionType::GeneralChat {
        let system = "You are a helpful software engineering assistant. Answer concisely.";
        post_slack_message(bot_token, channel_id, "⏳ Thinking...").await;
        match crate::deepseek::call_deepseek(&deepseek_key, system, &user_message).await {
            Ok(answer) => {
                post_slack_message(bot_token, channel_id, &answer).await;
            }
            Err(e) => {
                tracing::warn!("DeepSeek general chat error: {:?}", e);
                post_slack_message(bot_token, channel_id, &format!("⚠️ Error: {}", e)).await;
            }
        }
        return;
    }

    // Everything else goes through the groose agent
    if !intent.needs_repo {
        // No repo needed — direct model call for fast answer
        let system = match intent.action {
            ActionType::FindDocumentation => {
                "You are a senior technical documentation expert. Respond with the final answer \
                 directly — clean, well-organized documentation the user can read as-is. \
                 Do NOT include your thinking process, plans, or meta-commentary. \
                 Start directly with the answer. Use Markdown structure."
            }
            _ => {
                "You are a helpful assistant. Respond with the final answer only — \
                 no thinking process, no planning text. Start directly with the answer."
            }
        };
        post_slack_message(bot_token, channel_id, &format!("⏳ Searching for: {}", intent.query)).await;
        match crate::deepseek::call_deepseek(&deepseek_key, system, &intent.query).await {
            Ok(answer) => {
                post_slack_message(bot_token, channel_id, &answer).await;
            }
            Err(e) => {
                tracing::warn!("DeepSeek error: {:?}", e);
                post_slack_message(bot_token, channel_id, &format!("⚠️ Error: {}", e)).await;
            }
        }
        return;
    }

    // Needs a repo — pre-flight access check, then the full groose coding engine
    if let Some(repo) = intent.repo.as_deref() {
        if !ensure_repo_access(bot_token, channel_id, repo).await {
            return;
        }
    }

    let prompt = match intent.action {
        ActionType::FindDocumentation => {
            format!(
                "{} *Searching for documentation...*\n📂 *Repository:* `{}`\n\nLet me research that...",
                emoji, repo_str
            )
        }
        ActionType::Implement => {
            format!(
                "{} *Implementing*\n📂 *Repository:* `{}`\n⏳ Starting agent...",
                emoji, repo_str
            )
        }
        ActionType::Review => {
            format!(
                "{} *Reviewing*\n📂 *Repository:* `{}`\n⏳ Starting agent...",
                emoji, repo_str
            )
        }
        ActionType::Analyze => {
            format!(
                "{} *Analyzing*\n📂 *Repository:* `{}`\n⏳ Starting agent...",
                emoji, repo_str
            )
        }
        ActionType::WorkOn => {
            format!(
                "{} *Working*\n📂 *Repository:* `{}`\n⏳ Starting agent...",
                emoji, repo_str
            )
        }
        _ => unreachable!(),
    };
    post_slack_message(bot_token, channel_id, &prompt).await;

    // Route to the agent
    let agent_prompt = if intent.needs_repo {
        format!(
            "Task: {}\n\nRepository: {}\n\n{}",
            intent.query,
            intent.repo.as_deref().unwrap_or("(not specified)"),
            if intent.action == ActionType::FindDocumentation {
                "Search the codebase and documentation. Be thorough and provide examples."
            } else if intent.action == ActionType::Implement {
                "Clone the repo, understand the code, implement the changes, write tests."
            } else if intent.action == ActionType::Review {
                "Review the code changes, check for issues, provide detailed feedback."
            } else if intent.action == ActionType::Analyze {
                "Analyze the codebase structure, estimate effort, identify risks."
            } else {
                "Complete the task."
            }
        )
    } else {
        format!(
            "Question: {}\n\nProvide a thorough answer with examples, best practices, and references.",
            intent.query
        )
    };

    run_groose_engine(bot_token, channel_id, &deepseek_key, &agent_prompt, intent.repo.as_deref()).await;
}

/// Check whether the GitHub App can access the repo; if not, post an
/// install link to Slack. Returns true when we have access.
async fn ensure_repo_access(
    bot_token: &Option<String>,
    channel_id: &str,
    repo: &str,
) -> bool {
    let (app_id, key_path) = match (
        std::env::var("GITHUB_APP_ID"),
        std::env::var("GITHUB_PRIVATE_KEY_PATH"),
    ) {
        (Ok(a), Ok(k)) if !a.is_empty() && !k.is_empty() => (a, k),
        _ => return true, // No GitHub App configured — let clone fail naturally
    };

    let api_base = std::env::var("GITHUB_API_BASE_URL")
        .unwrap_or_else(|_| "https://api.github.com".to_string());

    let accessible = match groose_execution::github_auth::check_repo_access(
        &app_id, &key_path, &api_base, repo,
    ).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(repo = %repo, error = %e, "repo access check failed, proceeding anyway");
            return true;
        }
    };

    if accessible {
        return true;
    }

    let install_url = "https://github.com/apps/groose-agent/installations/new";

    let blocks = serde_json::json!([
        {
            "type": "header",
            "text": { "type": "plain_text", "text": "🔒 Repository Access Required", "emoji": true }
        },
        { "type": "divider" },
        {
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": format!(
                    "I can't access `{}` yet. Install the *groose-agent* GitHub App on that repository, then ask again.",
                    repo
                )
            }
        },
        {
            "type": "actions",
            "elements": [
                {
                    "type": "button",
                    "text": { "type": "plain_text", "text": "Install groose-agent", "emoji": true },
                    "url": format!("https://github.com/apps/groose-agent/installations/new"),
                    "style": "primary"
                }
            ]
        }
    ]);

    post_slack_blocks(
        bot_token,
        channel_id,
        &format!("Install groose-agent to access {}", repo),
        blocks,
    ).await;

    tracing::info!(repo = %repo, install_url = %install_url, "posted repo install link");
    false
}

/// Run the groose coding engine with DeepSeek as the model provider.
async fn run_groose_engine(
    bot_token: &Option<String>,
    channel_id: &str,
    deepseek_key: &str,
    goal: &str,
    repo: Option<&str>,
) {
    let model = crate::deepseek::DeepSeekProvider::new(deepseek_key.to_string(), None);

    // Build a simple TaskSpec from the user's goal
    let task = TaskSpec {
        schema_version: "1.0".to_string(),
        task_id: format!("slack-{}", uuid::Uuid::new_v4()),
        r#type: "implement".to_string(),
        repository: groose_task::RepositorySpec {
            provider: "github".to_string(),
            name: repo.unwrap_or("").to_string(),
            base_ref: "main".to_string(),
            base_sha: None,
        },
        instruction: groose_task::InstructionSpec {
            goal: goal.to_string(),
            acceptance_criteria: vec![],
        },
        agent: groose_task::AgentConfig {
            profile: "sme-engineer".to_string(),
            skills: vec!["sme-documentation".to_string()],
        },
        execution: groose_task::ExecutionConfig {
            mode: "autonomous".to_string(),
            max_iterations: 10,
            max_cost_usd: 2.0,
            timeout_minutes: 30,
        },
        delivery: groose_task::DeliveryConfig {
            strategy: "pull_request".to_string(),
            merge: "human_approval".to_string(),
        },
        origin: groose_task::OriginInfo {
            source: "slack".to_string(),
            event_id: channel_id.to_string(),
            conversation_id: String::new(),
        },
        admitted_at: None,
        requester_identity: None,
        installation_identity: None,
        version: 0,
    };

    let ctx = ExecutionContext {
        task: task.clone(),
        workspace_root: std::path::PathBuf::from("/tmp"),
        max_iterations: 10,
        max_cost_usd: 2.0,
        timeout: std::time::Duration::from_secs(1800),
        checkpoint_every: 3,
    };

    // Isolated temp workspace per task — created before the run,
    // deleted after (whether the task succeeds or fails).
    let (workspace, workspace_path) = match groose_execution::workspace::TempWorkspace::new() {
        Ok((ws, path)) => (ws, path),
        Err(e) => {
            tracing::warn!("Failed to create temp workspace: {:?}", e);
            post_slack_message(bot_token, channel_id, "⚠️ Failed to create workspace.").await;
            return;
        }
    };
    tracing::info!(workspace = %workspace_path.display(), "created temp workspace");

    let control = groose_execution::control::NoopControlReceiver;
    let events = groose_execution::LoggingEventSink;
    let checkpoints = match groose_execution::checkpoints::FileCheckpointStore::new(std::path::PathBuf::from("/opt/groose/checkpoints")) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Failed to create checkpoint store: {:?}", e);
            post_slack_message(bot_token, channel_id, "⚠️ Failed to initialize checkpoint store.").await;
            return;
        }
    };
    let verifier = groose_execution::verifier::BasicVerifier::new("cargo test", "cargo fmt --check");
    let engine = CodingEngine::new("groose-sme");

    post_slack_message(bot_token, channel_id, "🤖 *groose agent started* — I'll keep you updated as I make progress.").await;

    match engine.run(&task, &ctx, &workspace, &control, &events, &checkpoints, &verifier, &model).await {
        Ok(outcome) => {
            let msg = match &outcome {
                ExecutionOutcome::Completed { output } => {
                if let Some(text) = output {
                    text.to_string()
                } else {
                    "✅ *Task completed successfully!*".to_string()
                }
            }
                ExecutionOutcome::Failed { error } => format!("❌ *Task failed:* {}", error),
                ExecutionOutcome::BudgetExceeded { reason } => format!("⏱️ *Budget exceeded:* {}", reason),
                ExecutionOutcome::Cancelled { reason } => format!("🛑 *Cancelled:* {}", reason),
                ExecutionOutcome::PendingReview { pr_url } => {
                    if let Some(url) = pr_url {
                        format!("🔗 *PR created:* {}", url)
                    } else {
                        "📋 *Task ready for review*".to_string()
                    }
                }
            };
            post_slack_message(bot_token, channel_id, &msg).await;
        }
        Err(e) => {
            tracing::warn!("Groose engine error: {:?}", e);
            post_slack_message(bot_token, channel_id, &format!("⚠️ *Engine error:* {}", e)).await;
        }
    }
}

// ── Slack Message Posting ────────────────────────────────────────────────

/// Post a message to a Slack channel using native Blocks API layout.
async fn post_slack_message(bot_token: &Option<String>, channel: &str, text: &str) {
    let token = match bot_token {
        Some(t) => t,
        None => return,
    };

    // Convert Markdown to Slack mrkdwn before posting
    let slack_text = crate::slackfmt::to_slack_mrkdwn(text);

    let client = reqwest::Client::new();
    let payload = serde_json::json!({
        "channel": channel,
        "text": strip_markdown(&slack_text),
        "blocks": render_blocks(&slack_text),
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

/// Render a message into Slack Blocks API layout based on content type.
fn render_blocks(text: &str) -> Vec<serde_json::Value> {
    use serde_json::json;
    let text = text.trim();

    // Detect message type by prefix emoji
    if text.starts_with("📚") || text.starts_with("📖") {
        let body = text
            .lines()
            .skip_while(|l| l.starts_with("📚") || l.starts_with("📖") || l.starts_with("🔍"))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();
        vec![
            json!({ "type": "header", "text": { "type": "plain_text", "text": "📚 Documentation", "emoji": true } }),
            json!({ "type": "divider" }),
            json!({ "type": "section", "text": { "type": "mrkdwn", "text": body } }),
        ]
    } else if text.starts_with("🎯") {
        vec![
            json!({ "type": "header", "text": { "type": "plain_text", "text": "🎯 Implementing Story", "emoji": true } }),
            json!({ "type": "divider" }),
            json!({ "type": "section", "text": { "type": "mrkdwn", "text": text } }),
        ]
    } else if text.starts_with("🔍") || text.starts_with("🔎") {
        let body = text.lines().skip(1).collect::<Vec<_>>().join("\n");
        vec![
            json!({ "type": "header", "text": { "type": "plain_text", "text": "🔍 PR Review", "emoji": true } }),
            json!({ "type": "divider" }),
            json!({ "type": "section", "text": { "type": "mrkdwn", "text": body } }),
        ]
    } else if text.starts_with("📊") {
        vec![
            json!({ "type": "header", "text": { "type": "plain_text", "text": "📊 Analysis & Estimate", "emoji": true } }),
            json!({ "type": "divider" }),
            json!({ "type": "section", "text": { "type": "mrkdwn", "text": text } }),
        ]
    } else if text.starts_with("🤖") || text.starts_with("⏳") || text.starts_with("✅") || text.starts_with("❌") || text.starts_with("⚠️") {
        let status_text = text
            .trim_start_matches(|c: char| !c.is_alphanumeric() && c != ' ')
            .trim();
        vec![
            json!({
                "type": "context",
                "elements": [{ "type": "mrkdwn", "text": format!("{} {}", text.chars().next().unwrap_or(' '), status_text) }]
            }),
        ]
    } else if text.starts_with("🤔") {
        vec![
            json!({ "type": "header", "text": { "type": "plain_text", "text": "🤔 Need Help?", "emoji": true } }),
            json!({ "type": "divider" }),
            json!({ "type": "section", "text": { "type": "mrkdwn", "text": text } }),
        ]
    } else {
        // Generic output from model — chunk into sections (Slack limit: 3000 chars per section)
        let mut blocks = Vec::new();
        let mut current = String::new();

        // Helper: split a long line into <=limit chunks at word boundaries
        fn split_line(line: &str, limit: usize) -> Vec<String> {
            if line.len() <= limit {
                return vec![line.to_string()];
            }
            let mut parts = Vec::new();
            let mut rest = line;
            while !rest.is_empty() {
                if rest.len() <= limit {
                    parts.push(rest.to_string());
                    break;
                }
                // Find last space within limit
                let end = match rest[..limit].rfind(' ') {
                    Some(pos) if pos > limit / 2 => pos,
                    _ => limit,
                };
                parts.push(rest[..end].to_string());
                rest = rest[end..].trim_start();
            }
            parts
        }

        for line in text.lines() {
            for part in split_line(line, 2900) {
                if current.len() + part.len() + 1 > 2900 {
                    if !current.is_empty() {
                        blocks.push(json!({ "type": "section", "text": { "type": "mrkdwn", "text": current.clone() } }));
                        current.clear();
                    }
                }
                if !current.is_empty() {
                    current.push('\n');
                }
                current.push_str(&part);
            }
        }
        if !current.is_empty() {
            blocks.push(json!({ "type": "section", "text": { "type": "mrkdwn", "text": current } }));
        }

        if blocks.is_empty() {
            blocks.push(json!({ "type": "section", "text": { "type": "mrkdwn", "text": text } }));
        }

        blocks
    }
}

/// Post a message to a Slack channel with a pre-built Blocks payload.
async fn post_slack_blocks(
    bot_token: &Option<String>,
    channel: &str,
    fallback_text: &str,
    blocks: serde_json::Value,
) {
    let token = match bot_token {
        Some(t) => t,
        None => return,
    };

    let client = reqwest::Client::new();
    let payload = serde_json::json!({
        "channel": channel,
        "text": fallback_text,
        "blocks": blocks,
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

/// Strip markdown for Slack's fallback text field (max 300 chars).
fn strip_markdown(text: &str) -> String {
    let stripped: String = text.replace('*', "")
        .replace('`', "")
        .replace('_', "")
        .replace('"', "'")
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect();

    if stripped.is_empty() {
        "Processing...".to_string()
    } else {
        stripped
    }
}