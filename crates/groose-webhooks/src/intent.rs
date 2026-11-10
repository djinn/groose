//! Intent-based command routing.
//!
//! All user messages are classified into a structured intent by the model
//! itself, then routed to the groose agent. No hardcoded string matching.
//!
//! The intent tells the agent:
//! - What action the user wants
//! - Which repo (if any) was mentioned
//! - Whether a repo clone is needed
//!
//! This is a lightweight classification call — cheap, fast, and flexible.

use serde::{Deserialize, Serialize};

/// The user's classified intent after model-based parsing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserIntent {
    /// The primary action.
    pub action: ActionType,
    /// Repository if mentioned (e.g. "org/repo").
    pub repo: Option<String>,
    /// The core query cleaned of boilerplate.
    pub query: String,
    /// Whether this action requires cloning a git repo.
    pub needs_repo: bool,
    /// Slack context.
    pub channel_id: Option<String>,
    pub user_id: Option<String>,
}

/// Supported action types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ActionType {
    FindDocumentation,
    Implement,
    Review,
    Analyze,
    WorkOn,
    Status,
    Pause,
    Resume,
    Cancel,
    GeneralChat,
}

impl ActionType {
    pub fn needs_repo(&self) -> bool {
        matches!(self, ActionType::Implement | ActionType::Review | ActionType::Analyze | ActionType::WorkOn)
    }

    pub fn emoji(&self) -> &str {
        match self {
            ActionType::FindDocumentation => "📚",
            ActionType::Implement => "🎯",
            ActionType::Review => "🔍",
            ActionType::Analyze => "📊",
            ActionType::WorkOn => "⚡",
            ActionType::Status => "📋",
            ActionType::Pause => "⏸️",
            ActionType::Resume => "▶️",
            ActionType::Cancel => "🛑",
            ActionType::GeneralChat => "💬",
        }
    }

    pub fn label(&self) -> &str {
        match self {
            ActionType::FindDocumentation => "Documentation",
            ActionType::Implement => "Implementation",
            ActionType::Review => "Code Review",
            ActionType::Analyze => "Analysis",
            ActionType::WorkOn => "Task",
            ActionType::Status => "Status",
            ActionType::Pause => "Pause",
            ActionType::Resume => "Resume",
            ActionType::Cancel => "Cancel",
            ActionType::GeneralChat => "Chat",
        }
    }
}

/// Classify a user message into a structured intent.
///
/// Uses the model itself for classification (fast, ~200ms).
/// Falls back to simple heuristics if the model call fails.
pub async fn classify_intent(
    text: &str,
    channel_id: Option<String>,
    user_id: Option<String>,
    deepseek_key: &str,
) -> UserIntent {
    // Try model-based classification first
    if let Some(intent) = model_classify(text, deepseek_key).await {
        return UserIntent {
            needs_repo: intent.action.needs_repo() || intent.repo.is_some(),
            channel_id,
            user_id,
            ..intent
        };
    }

    // Fallback: heuristic classification
    let lower = text.to_lowercase();
    let action = if lower.contains("doc") || lower.contains("documentation") || lower.contains("how to") || lower.contains("explain") {
        ActionType::FindDocumentation
    } else if lower.contains("implement") || lower.contains("story") || lower.contains("issue") || lower.contains("create pr") || lower.contains("write code") {
        ActionType::Implement
    } else if lower.contains("review") || lower.contains("pr ") {
        ActionType::Review
    } else if lower.contains("analyze") || lower.contains("scope") || lower.contains("estimate") || lower.contains("impact") {
        ActionType::Analyze
    } else if lower.contains("work on") || lower.contains("fix") || lower.contains("feature") {
        ActionType::WorkOn
    } else if lower.contains("status") || lower.contains("progress") {
        ActionType::Status
    } else if lower.contains("pause") || lower.contains("stop") {
        ActionType::Pause
    } else if lower.contains("resume") || lower.contains("continue") {
        ActionType::Resume
    } else if lower.contains("cancel") || lower.contains("abort") {
        ActionType::Cancel
    } else {
        ActionType::GeneralChat
    };

    UserIntent {
        action: action.clone(),
        repo: extract_repo(text),
        query: text.to_string(),
        needs_repo: action.needs_repo(),
        channel_id,
        user_id,
    }
}

/// Use the model to classify intent.
async fn model_classify(text: &str, api_key: &str) -> Option<UserIntent> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;

    let system_prompt = r#"You classify user messages for a coding agent.
Return JSON with: action, repo (or null), query (cleaned), needs_repo (bool).

Actions: find_documentation, implement, review, analyze, work_on, status, pause, resume, cancel, general_chat

Rules:
- find_documentation: user asks about docs, how-tos, explanations → needs_repo=false
- implement: user wants code written, PR created, story implemented → needs_repo=true
- review: user wants code reviewed → needs_repo=true
- analyze: user wants scope/effort analysis → needs_repo=true
- work_on: general coding → needs_repo=true
- general_chat: anything else → needs_repo=false
- Extract repo if user mentions org/repo pattern

JSON only, no other text."#;

    let request = serde_json::json!({
        "model": "deepseek-flash",
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": text}
        ],
        "temperature": 0.1,
        "max_tokens": 256,
    });

    let resp = client
        .post("https://api.deepseek.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await
        .ok()?;

    let body: serde_json::Value = resp.json().await.ok()?;
    let content = body["choices"][0]["message"]["content"].as_str()?;
    serde_json::from_str::<UserIntent>(content).ok()
}

/// Extract a repo reference like "org/repo" from text.
fn extract_repo(text: &str) -> Option<String> {
    let re = regex::Regex::new(r"[a-zA-Z0-9_-]+/[a-zA-Z0-9_.-]+").ok()?;
    re.find(text).map(|m| m.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_repo() {
        assert_eq!(extract_repo("work on org/payments").unwrap(), "org/payments");
        assert_eq!(extract_repo("analyze my-org/my_repo").unwrap(), "my-org/my_repo");
        assert!(extract_repo("find docs for MuleSoft").is_none());
    }

    #[test]
    fn test_action_needs_repo() {
        assert!(!ActionType::FindDocumentation.needs_repo());
        assert!(ActionType::Implement.needs_repo());
        assert!(ActionType::Review.needs_repo());
        assert!(ActionType::Analyze.needs_repo());
        assert!(!ActionType::GeneralChat.needs_repo());
    }
}