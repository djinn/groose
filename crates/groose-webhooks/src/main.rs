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

    match cmd {
        SlackCommand::FindDocumentation { topic, repo } => {
            tracing::info!(
                command = "find_documentation",
                topic = %topic,
                repo = ?repo,
                channel = %channel_id,
                user = %user_id,
                "SME: routing through groose engine"
            );
            post_slack_message(bot_token, channel_id, &format!(
                "📚 *Searching for documentation on:* `{}`\n🔍 *In repository:* `{}`\n\nStarting groose agent with DeepSeek...",
                topic, repo.as_deref().unwrap_or("(all repositories)")
            )).await;

            run_groose_engine(
                bot_token, channel_id, &deepseek_key,
                &format!("Find documentation about: {}. Search the codebase, README files, docs directories, and explain the topic thoroughly with examples.", topic),
            ).await;
        }

        SlackCommand::TakeStory { repo, issue } => {
            tracing::info!(
                command = "take_story",
                repo = %repo,
                issue = %issue,
                channel = %channel_id,
                user = %user_id,
                "SME: routing through groose engine"
            );
            post_slack_message(bot_token, channel_id, &format!(
                "🎯 *Taking story:* {} on `{}`\n⏳ Starting groose agent to analyze and implement...",
                issue, repo
            )).await;

            run_groose_engine(
                bot_token, channel_id, &deepseek_key,
                &format!("Implement issue {} on repository {}. Clone the repo, analyze the code, create a plan, implement the changes, write tests, and prepare a PR.", issue, repo),
            ).await;
        }

        SlackCommand::ReviewPullRequest { repo, pr_number } => {
            tracing::info!(
                command = "review_pr",
                repo = %repo,
                pr_number = %pr_number,
                channel = %channel_id,
                user = %user_id,
                "SME: routing through groose engine"
            );
            post_slack_message(bot_token, channel_id, &format!(
                "🔍 *Reviewing PR #{}* on `{}`\n⏳ Starting groose agent to analyze changes...",
                pr_number, repo
            )).await;

            run_groose_engine(
                bot_token, channel_id, &deepseek_key,
                &format!("Review pull request #{} on repository {}. Examine the code changes, run tests, check for security issues, and provide detailed feedback.", pr_number, repo),
            ).await;
        }

        SlackCommand::Analyze { repo, goal } => {
            tracing::info!(
                command = "analyze",
                repo = %repo,
                goal = %goal,
                channel = %channel_id,
                user = %user_id,
                "SME: routing through groose engine"
            );
            post_slack_message(bot_token, channel_id, &format!(
                "📊 *Analyzing:* `{}`\n🎯 *Goal:* {}\n⏳ Starting groose agent for scope estimation...",
                repo, goal
            )).await;

            run_groose_engine(
                bot_token, channel_id, &deepseek_key,
                &format!("Analyze repository {} for the following goal: {}. Trace through the codebase, identify affected modules, assess complexity, estimate effort, and identify risks.", repo, goal),
            ).await;
        }

        SlackCommand::WorkOn { repo, instruction } => {
            post_slack_message(bot_token, channel_id, &format!(
                "⚡ *Working on:* `{}`\n📝 *Instruction:* {}\n\nStarting groose agent...", repo, instruction
            )).await;
            run_groose_engine(bot_token, channel_id, &deepseek_key, &format!("On repository {}: {}", repo, instruction)).await;
        }

        SlackCommand::UseSkill { skill } => {
            post_slack_message(bot_token, channel_id, &format!("🧠 *Skill activated:* `{}`", skill)).await;
        }

        SlackCommand::Status => {
            post_slack_message(bot_token, channel_id, "📋 *Status*\nNo active tasks in this channel.").await;
        }

        SlackCommand::Pause => {
            post_slack_message(bot_token, channel_id, "⏸️ *Pausing task...*").await;
        }

        SlackCommand::Resume => {
            post_slack_message(bot_token, channel_id, "▶️ *Resuming task...*").await;
        }

        SlackCommand::Cancel => {
            post_slack_message(bot_token, channel_id, "🛑 *Cancelling task...*").await;
        }

        SlackCommand::Unknown { text } => {
            tracing::info!(command = "unknown", text = %text, "falling back to groose agent");
            run_groose_engine(bot_token, channel_id, &deepseek_key, text).await;
        }
    }
}

/// Run the groose coding engine with DeepSeek as the model provider.
async fn run_groose_engine(
    bot_token: &Option<String>,
    channel_id: &str,
    deepseek_key: &str,
    goal: &str,
) {
    let model = crate::deepseek::DeepSeekProvider::new(deepseek_key.to_string(), None);

    // Build a simple TaskSpec from the user's goal
    let task = TaskSpec {
        schema_version: "1.0".to_string(),
        task_id: format!("slack-{}", uuid::Uuid::new_v4()),
        r#type: "implement".to_string(),
        repository: groose_task::RepositorySpec {
            provider: "github".to_string(),
            name: "djinn/groose".to_string(),
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
        workspace_root: std::path::PathBuf::from("/opt/groose/workspace"),
        max_iterations: 10,
        max_cost_usd: 2.0,
        timeout: std::time::Duration::from_secs(1800),
        checkpoint_every: 3,
    };

    let workspace_root = std::path::PathBuf::from("/opt/groose/workspace");
    let workspace = groose_execution::workspace::LocalWorkspace::new(workspace_root);
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