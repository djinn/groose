//! DeepSeek API integration for the groose agent.
//!
//! Calls the DeepSeek chat completions endpoint to process SME commands
//! with context from the agent's skills.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A message in the chat conversation.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// Request to the DeepSeek API.
#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f64,
    max_tokens: u32,
}

/// Response from the DeepSeek API.
#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ChatMessage,
}

/// Call the DeepSeek API with a prompt and return the response text.
pub async fn call_deepseek(
    api_key: &str,
    system_prompt: &str,
    user_message: &str,
) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    let request = ChatRequest {
        model: "deepseek-flash".to_string(),
        messages: vec![
            ChatMessage {
                role: "system".to_string(),
                content: system_prompt.to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: user_message.to_string(),
            },
        ],
        temperature: 0.3,
        max_tokens: 2048,
    };

    let resp = client
        .post("https://api.deepseek.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("DeepSeek API error {}: {}", status, body);
    }

    let chat_resp: ChatResponse = resp.json().await?;
    let text = chat_resp
        .choices
        .first()
        .map(|c| c.message.content.clone())
        .unwrap_or_default();

    tracing::debug!(response_len = text.len(), "DeepSeek API response received");

    Ok(text)
}

/// Get the system prompt for a given SME skill.
pub fn get_system_prompt(skill: &str) -> &'static str {
    match skill {
        "documentation" => {
            "You are a Subject Matter Expert on software documentation. \
             Given a user's question about a topic, find and explain relevant \
             documentation. Be specific, cite sources, and provide code examples \
             where applicable. If the topic is unclear, ask clarifying questions."
        }
        "implementer" => {
            "You are a Senior Software Engineer implementing features. \
             Given a story/issue description, create a plan, implement the changes, \
             write tests, and prepare a PR. Output clear code changes and explain \
             your design decisions."
        }
        "reviewer" => {
            "You are a Senior Code Reviewer. Given a pull request, review the \
             changes for correctness, security, performance, and style. Provide \
             specific line-level feedback and an overall assessment: \
             Approve / Changes Requested / Blocked."
        }
        "analyst" => {
            "You are a Technical Architect analyzing codebases. Given a goal, \
             trace through the affected modules, assess complexity, estimate effort, \
             and identify risks. Be thorough and specific."
        }
        _ => {
            "You are a helpful software engineering assistant. Answer the user's \
             question concisely and accurately."
        }
    }
}

/// Format a Slack command into a prompt for the DeepSeek model.
pub fn format_prompt(cmd_type: &str, text: &str) -> String {
    match cmd_type {
        "find_documentation" => {
            format!(
                "The user is asking about: {}\n\n\
                 Please search your knowledge and explain the topic thoroughly. \
                 Include code examples, best practices, and links to official documentation.",
                text
            )
        }
        "analyze" => {
            format!(
                "Analyze the following request and provide a scope/effort estimate:\n\n{}",
                text
            )
        }
        "review_pr" => {
            format!(
                "Review the following pull request changes:\n\n{}",
                text
            )
        }
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_prompts() {
        assert!(get_system_prompt("documentation").contains("Subject Matter Expert"));
        assert!(get_system_prompt("implementer").contains("Senior Software Engineer"));
        assert!(get_system_prompt("reviewer").contains("Senior Code Reviewer"));
        assert!(get_system_prompt("analyst").contains("Technical Architect"));
    }

    #[test]
    fn test_format_prompt() {
        let prompt = format_prompt("find_documentation", "MuleSoft API gateway");
        assert!(prompt.contains("MuleSoft API gateway"));
    }
}