//! DSML tool-call execution for the coding engine.
//!
//! DeepSeek (and other models) emit tool calls in DSML format:
//!
//! ```text
//! <｜｜DSML｜｜ calls>
//! <｜｜DSML｜｜ invoke name="list_files">
//! <｜｜DSML｜｜ parameter name="path" string="true">.</｜｜DSML｜｜ parameter>
//! </｜｜DSML｜｜ invoke>
//! </｜｜DSML｜｜ calls>
//! ```
//!
//! The engine parses these, executes them against the workspace, feeds
//! results back to the model, and repeats until the model produces a
//! plain-text final answer (no DSML blocks).

use anyhow::Result;

use crate::workspace::Workspace;

/// One parsed tool invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub name: String,
    pub params: Vec<(String, String)>,
}

/// A model response split into plain text and tool calls.
#[derive(Debug, Clone)]
pub struct ParsedResponse {
    /// Text before/after the DSML block (the model's natural language).
    pub text: String,
    /// Tool calls to execute.
    pub tool_calls: Vec<ToolCall>,
}

/// Parse a model response into text + DSML tool calls.
pub fn parse_dsml(response: &str) -> ParsedResponse {
    let marker = "<｜｜DSML｜｜ calls>";
    let end_marker = "</｜｜DSML｜｜ calls>";

    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();
    let mut rest = response;

    while let Some(start) = rest.find(marker) {
        text_parts.push(rest[..start].trim().to_string());
        let after = &rest[start + marker.len()..];
        let end = match after.find(end_marker) {
            Some(e) => e,
            None => {
                // Unterminated block — treat remainder as text
                text_parts.push(after.trim().to_string());
                return ParsedResponse {
                    text: text_parts.join("\n").trim().to_string(),
                    tool_calls,
                };
            }
        };
        let block = &after[..end];
        tool_calls.extend(parse_invokes(block));
        rest = &after[end + end_marker.len()..];
    }
    text_parts.push(rest.trim().to_string());

    ParsedResponse {
        text: text_parts.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n"),
        tool_calls,
    }
}

/// Parse all `invoke` blocks inside a DSML calls section.
fn parse_invokes(block: &str) -> Vec<ToolCall> {
    let open = "<｜｜DSML｜｜ invoke name=\"";
    let close = "</｜｜DSML｜｜ invoke>";
    let mut calls = Vec::new();
    let mut rest = block;

    while let Some(start) = rest.find(open) {
        let after = &rest[start + open.len()..];
        let name_end = match after.find('"') {
            Some(e) => e,
            None => break,
        };
        let name = after[..name_end].to_string();

        let close_end = match rest.find(close) {
            Some(e) => e,
            None => break,
        };
        let invoke_body = &rest[..close_end];

        // Parse parameters inside this invoke
        let mut params = Vec::new();
        let param_open = "<｜｜DSML｜｜ parameter name=\"";
        let param_close = "</｜｜DSML｜｜ parameter>";
        let mut prest = invoke_body;
        while let Some(pstart) = prest.find(param_open) {
            let pafter = &prest[pstart + param_open.len()..];
            let pname_end = match pafter.find('"') {
                Some(e) => e,
                None => break,
            };
            let pname = pafter[..pname_end].to_string();
            // Find the value: after the closing '>' of the parameter tag
            let value_start = match pafter.find('>') {
                Some(e) => e + 1,
                None => break,
            };
            let after_tag = &pafter[value_start..];
            let pend = match after_tag.find(param_close) {
                Some(e) => e,
                None => break,
            };
            let value = after_tag[..pend].trim().to_string();
            params.push((pname, value));
            prest = &after_tag[pend + param_close.len()..];
        }

        calls.push(ToolCall { name, params });
        rest = &rest[close_end + close.len()..];
    }

    calls
}

/// Execute a single tool call against the workspace and return the result.
pub async fn execute_tool(call: &ToolCall, workspace: &dyn Workspace) -> String {
    let param = |key: &str, default: &str| -> String {
        call.params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| default.to_string())
    };

    match call.name.as_str() {
        "list_files" => {
            let path = param("path", ".");
            match workspace
                .run_command("sh", &["-c", &format!("ls -la {}", shell_escape(&path))])
                .await
            {
                Ok(out) => truncate(&out, 4000),
                Err(e) => format!("error: {}", e),
            }
        }
        "run_command" => {
            let cmd = param("command", "");
            match workspace.run_command("sh", &["-c", &cmd]).await {
                Ok(out) => truncate(&out, 4000),
                Err(e) => format!("error: {}", e),
            }
        }
        "read_file" => {
            let path = param("path", "");
            match workspace
                .read_file(std::path::Path::new(&path))
                .await
            {
                Ok(content) => truncate(&content, 8000),
                Err(e) => format!("error: {}", e),
            }
        }
        "write_file" => {
            let path = param("path", "");
            let content = param("content", "");
            match workspace
                .write_file(std::path::Path::new(&path), &content)
                .await
            {
                Ok(_) => format!("wrote {}", path),
                Err(e) => format!("error: {}", e),
            }
        }
        "git_status" => match workspace.run_command("git", &["status"]).await {
            Ok(out) => truncate(&out, 2000),
            Err(e) => format!("error: {}", e),
        },
        "git_diff" => match workspace.run_command("git", &["diff"]).await {
            Ok(out) => truncate(&out, 4000),
            Err(e) => format!("error: {}", e),
        },
        "search_code" => {
            let pattern = if call.params.iter().any(|(k, _)| k == "pattern") {
                param("pattern", "")
            } else {
                param("query", "")
            };
            match workspace
                .run_command("sh", &["-c", &format!("grep -rn {} . 2>/dev/null | head -50", shell_escape(&pattern))])
                .await
            {
                Ok(out) => truncate(&out, 4000),
                Err(e) => format!("error: {}", e),
            }
        }
        other => format!("error: unknown tool '{}'", other),
    }
}

/// Run the tool-call loop: model emits DSML → execute → feed back → repeat.
///
/// Returns the model's final plain-text answer.
pub async fn run_tool_loop(
    model: &dyn crate::engine::ModelProvider,
    workspace: &dyn Workspace,
    initial_prompt: &str,
    tools: &[String],
    max_rounds: u32,
) -> Result<String> {
    let mut prompt = initial_prompt.to_string();
    let mut collected_text = String::new();

    for round in 0..max_rounds {
        let response = model.complete(&prompt, tools).await?;
        let parsed = parse_dsml(&response);

        if !parsed.text.is_empty() {
            if !collected_text.is_empty() {
                collected_text.push_str("\n");
            }
            collected_text.push_str(&parsed.text);
        }

        if parsed.tool_calls.is_empty() {
            // No tool calls — model is done
            return Ok(if collected_text.is_empty() {
                parsed.text
            } else {
                collected_text
            });
        }

        tracing::info!(
            round = round,
            tool_calls = parsed.tool_calls.len(),
            "executing tool calls"
        );

        // Execute each tool call and collect results
        let mut results = String::new();
        for call in &parsed.tool_calls {
            let result = execute_tool(call, workspace).await;
            results.push_str(&format!(
                "<tool_result name=\"{}\">\n{}\n</tool_result>\n",
                call.name, result
            ));
        }

        // Feed results back to the model
        prompt = format!(
            "{}\n\n{}",
            prompt,
            format!(
                "Tool results:\n{}\n\nContinue based on these results. \
                 If you have enough information to answer, provide your final answer as plain text (no tool calls).",
                results
            )
        );
    }

    Ok(collected_text)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}… ({} bytes truncated)", &s[..max], s.len() - max)
    }
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dsml_single_call() {
        let response = r#"Let me look at the files.

<｜｜DSML｜｜ calls>
<｜｜DSML｜｜ invoke name="list_files">
<｜｜DSML｜｜ parameter name="path" string="true">.</｜｜DSML｜｜ parameter>
</｜｜DSML｜｜ invoke>
</｜｜DSML｜｜ calls>"#;

        let parsed = parse_dsml(response);
        assert_eq!(parsed.text, "Let me look at the files.");
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "list_files");
        assert_eq!(
            parsed.tool_calls[0].params,
            vec![("path".to_string(), ".".to_string())]
        );
    }

    #[test]
    fn test_parse_dsml_multiple_calls() {
        let response = r#"<｜｜DSML｜｜ calls>
<｜｜DSML｜｜ invoke name="list_files">
<｜｜DSML｜｜ parameter name="path" string="true">.</｜｜DSML｜｜ parameter>
</｜｜DSML｜｜ invoke>
<｜｜DSML｜｜ invoke name="run_command">
<｜｜DSML｜｜ parameter name="command" string="true">pwd && ls</｜｜DSML｜｜ parameter>
</｜｜DSML｜｜ invoke>
</｜｜DSML｜｜ calls>"#;

        let parsed = parse_dsml(response);
        assert_eq!(parsed.tool_calls.len(), 2);
        assert_eq!(parsed.tool_calls[0].name, "list_files");
        assert_eq!(parsed.tool_calls[1].name, "run_command");
        assert_eq!(
            parsed.tool_calls[1].params,
            vec![("command".to_string(), "pwd && ls".to_string())]
        );
    }

    #[test]
    fn test_parse_no_dsml() {
        let parsed = parse_dsml("Just a plain answer.");
        assert_eq!(parsed.text, "Just a plain answer.");
        assert!(parsed.tool_calls.is_empty());
    }

    #[test]
    fn test_parse_write_file() {
        let response = r#"<｜｜DSML｜｜ calls>
<｜｜DSML｜｜ invoke name="write_file">
<｜｜DSML｜｜ parameter name="path" string="true">src/main.rs</｜｜DSML｜｜ parameter>
<｜｜DSML｜｜ parameter name="content" string="true">fn main() {}</｜｜DSML｜｜ parameter>
</｜｜DSML｜｜ invoke>
</｜｜DSML｜｜ calls>"#;

        let parsed = parse_dsml(response);
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "write_file");
        assert_eq!(parsed.tool_calls[0].params.len(), 2);
    }

    #[test]
    fn test_shell_escape() {
        assert_eq!(shell_escape("hello"), "'hello'");
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }
}
