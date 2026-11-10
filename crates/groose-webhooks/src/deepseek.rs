//! DeepSeek ModelProvider implementation for the groose coding engine.
//!
//! Implements the `ModelProvider` trait so DeepSeek can be used by the
//! `CodingEngine` with full tool-calling capabilities.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Simple direct DeepSeek call for general chat responses.
pub async fn call_deepseek(api_key: &str, system_prompt: &str, user_message: &str) -> Result<String> {
    tracing::info!("call_deepseek: starting with key={}.., prompt_len={}", &api_key[..8], user_message.len());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    tracing::info!("call_deepseek: client built");

    let request = serde_json::json!({
        "model": "deepseek-flash",
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_message}
        ],
        "temperature": 0.3,
        "max_tokens": 2048,
    });

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

    let chat_resp: serde_json::Value = resp.json().await?;
    tracing::info!("call_deepseek: full response keys={:?}", chat_resp.as_object().map(|o| o.keys().collect::<Vec<_>>()));
    let message = &chat_resp["choices"][0]["message"];
    let content = message["content"].as_str().unwrap_or("").to_string();
    let reasoning = message["reasoning_content"].as_str().unwrap_or("").to_string();

    let text = if content.is_empty() && !reasoning.is_empty() {
        tracing::info!("call_deepseek: using reasoning_content instead of content");
        reasoning
    } else {
        content
    };

    if text.is_empty() {
        tracing::warn!("DeepSeek returned empty content");
        anyhow::bail!("DeepSeek returned empty response");
    }

    Ok(text)
}

/// DeepSeek model provider implementing the CodingEngine's ModelProvider trait.
pub struct DeepSeekProvider {
    name: String,
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl DeepSeekProvider {
    pub fn new(api_key: String, model: Option<String>) -> Self {
        let model = model.unwrap_or_else(|| "deepseek-flash".to_string());
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .expect("valid reqwest client");
        Self {
            name: format!("deepseek-{}", model),
            api_key,
            model,
            client,
        }
    }
}

#[async_trait::async_trait]
impl groose_execution::engine::ModelProvider for DeepSeekProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn complete(&self, prompt: &str, tools: &[String]) -> Result<String> {
        // Build user message with tool info if tools are provided
        let user_message = if !tools.is_empty() {
            format!(
                "{}\n\nAvailable tools:\n{}",
                prompt,
                tools
                    .iter()
                    .map(|t| format!("- `{}`", t))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        } else {
            prompt.to_string()
        };

        let request = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: SYSTEM_PROMPT.to_string(),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: user_message,
                },
            ],
            temperature: 0.3,
            max_tokens: 4096,
        };

        let resp = self
            .client
            .post("https://api.deepseek.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", self.api_key))
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

        Ok(text)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f64,
    max_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ChatMessage,
}

const SYSTEM_PROMPT: &str = r#"You are a Senior Software Engineering Agent (groose).
You have access to tools that let you:
- Read and write files on the filesystem
- Run shell commands
- Use git (clone, commit, push, create branches, open PRs)
- Search codebases and documentation

Given a task, you should:
1. Understand the goal and acceptance criteria
2. Form a plan with specific files to modify
3. Execute the plan step by step
4. Verify your changes with tests
5. Deliver the result

Be thorough, write tests, and follow best practices."#;
