//! Slack message parsing, command routing, and slash command handling.
//!
//! Routes natural-language mentions and slash commands to agent actions.

use serde::{Deserialize, Serialize};

/// A parsed command from a Slack mention or slash command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SlackCommand {
    /// Find documentation about a topic.
    FindDocumentation {
        topic: String,
        repo: Option<String>,
    },
    /// Take a story/issue and implement it as a PR.
    TakeStory {
        repo: String,
        issue: String,
    },
    /// Review a PR given a repo and PR number.
    ReviewPullRequest {
        repo: String,
        pr_number: u64,
    },
    /// Analyze codebase for scope/effort estimation.
    Analyze {
        repo: String,
        goal: String,
    },
    /// Work on a repository (generic task creation).
    WorkOn {
        repo: String,
        instruction: String,
    },
    /// Use a specific skill.
    UseSkill {
        skill: String,
    },
    /// Check task status.
    Status,
    /// Pause a running task.
    Pause,
    /// Resume a paused task.
    Resume,
    /// Cancel a task.
    Cancel,
    /// Unknown or unparseable command.
    Unknown {
        text: String,
    },
}

/// Attempt to parse a command from natural-language text.
pub fn parse_command(text: &str) -> SlackCommand {
    let text = text.trim();

    // Strip bot mention prefix: "<@U0CFXJU438E> ..." or "@Mule Buddy ..."
    let text = text
        .strip_prefix("<@")
        .and_then(|t| t.split_once('>'))
        .map(|(_, rest)| rest.trim())
        .unwrap_or(text);

    let text = text
        .strip_prefix("@Mule Buddy")
        .or_else(|| text.strip_prefix("@groose"))
        .or_else(|| text.strip_prefix("mule buddy"))
        .map(|t| t.trim())
        .unwrap_or(text);

    // Work with original case for topic preservation, lowercase for matching
    let lower = text.to_lowercase();

    // ── find documentation ──────────────────────────────────────────
    let find_prefixes = ["find documentation for", "find docs for", "documentation for"];
    if let Some(_) = find_prefixes.iter().find(|p| lower.starts_with(*p)) {
        let prefix = find_prefixes.iter().find(|p| lower.starts_with(*p)).unwrap();
        let rest = text[prefix.len()..].trim();
        // Check " in " or " on " for repo specification
        if let Some(pos) = rest.find(" in ").or_else(|| rest.find(" on ")) {
            let sep = if rest[pos..].starts_with(" in ") { " in " } else { " on " };
            let topic = rest[..pos].trim().to_string();
            let repo = rest[pos + sep.len()..].trim().to_string();
            return SlackCommand::FindDocumentation { topic, repo: Some(repo) };
        }
        return SlackCommand::FindDocumentation { topic: rest.to_string(), repo: None };
    }

    // ── take story / implement issue ─────────────────────────────────
    let story_prefixes = ["take story", "take issue", "implement issue"];
    if let Some(prefix) = story_prefixes.iter().find(|p| lower.starts_with(*p)) {
        let rest = text[prefix.len()..].trim();

        // Extract issue number (starts with #) and repo
        let words: Vec<&str> = rest.split_whitespace().collect();
        let mut issue = String::new();
        let mut repo_parts = Vec::new();

        for w in words {
            if w.starts_with('#') {
                issue = w.to_string();
            } else if w == "on" || w == "in" {
                // skip prepositions
            } else {
                repo_parts.push(w);
            }
        }

        let repo = repo_parts.join(" ");
        let repo = if repo.is_empty() { "unknown".to_string() } else { repo };
        return SlackCommand::TakeStory { repo, issue };
    }

    // ── review pr / review pull request ──────────────────────────────
    let review_prefixes = ["review pr", "review pull request"];
    if let Some(prefix) = review_prefixes.iter().find(|p| lower.starts_with(*p)) {
        let rest = text[prefix.len()..].trim();

        let (repo_part, pr_part) = if let Some(pos) = rest.rfind('#') {
            let pr = rest[pos + 1..].trim().to_string();
            let r = rest[..pos].trim().to_string();
            (r, pr)
        } else {
            (rest.to_string(), String::new())
        };

        let repo = repo_part
            .strip_prefix("on")
            .map(|s| s.trim())
            .unwrap_or(&repo_part)
            .trim()
            .to_string();
        let pr_number = pr_part.parse::<u64>().unwrap_or(0);

        return SlackCommand::ReviewPullRequest { repo, pr_number };
    }

    // ── analyze / scope / estimate ───────────────────────────────────
    let analyze_prefixes = ["analyze", "scope", "estimate"];
    if let Some(prefix) = analyze_prefixes.iter().find(|p| lower.starts_with(*p)) {
        let rest = text[prefix.len()..].trim();

        // Split " for " or " to " for goal
        let (repo, goal) = if let Some(pos) = rest.find(" for ").or_else(|| rest.find(" to ")) {
            let sep = if rest[pos..].starts_with(" for ") { " for " } else { " to " };
            let r = rest[..pos].trim().to_string();
            let g = rest[pos + sep.len()..].trim().to_string();
            (r, g)
        } else {
            (rest.to_string(), String::new())
        };

        return SlackCommand::Analyze { repo, goal };
    }

    // ── work on ──────────────────────────────────────────────────────
    if lower.starts_with("work on") {
        let rest = text["work on".len()..].trim();
        if let Some((repo, instruction)) = rest.split_once(' ') {
            return SlackCommand::WorkOn {
                repo: repo.trim().to_string(),
                instruction: instruction.trim().to_string(),
            };
        }
        return SlackCommand::WorkOn {
            repo: rest.to_string(),
            instruction: String::new(),
        };
    }

    // ── use skill ────────────────────────────────────────────────────
    if lower.starts_with("use skill") {
        let skill = text["use skill".len()..].trim();
        return SlackCommand::UseSkill {
            skill: skill.to_string(),
        };
    }

    // ── status / pause / resume / cancel ─────────────────────────────
    match lower.as_str() {
        "status" | "what's the status" | "what is the status" | "progress" | "hello" | "hi" | "test" => {
            return SlackCommand::Status;
        }
        "pause" | "stop" => return SlackCommand::Pause,
        "resume" | "continue" | "start" => return SlackCommand::Resume,
        "cancel" | "cancel that" | "abort" => return SlackCommand::Cancel,
        _ => {}
    }

    if text.is_empty() {
        SlackCommand::Status
    } else {
        SlackCommand::Unknown { text: text.to_string() }
    }
}

/// Parse a slash command payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommandPayload {
    pub command: String,
    pub text: String,
    pub user_id: String,
    pub channel_id: String,
    pub team_id: String,
    pub response_url: String,
}

impl SlashCommandPayload {
    /// Convert a slash command to a SlackCommand.
    pub fn to_slack_command(&self) -> SlackCommand {
        match self.command.as_str() {
            "/find-docs" | "/find-documentation" | "/docs" => {
                let text = format!("find documentation for {}", self.text);
                parse_command(&text)
            }
            "/take-story" | "/implement" | "/take" => {
                let text = format!("take story {}", self.text);
                parse_command(&text)
            }
            "/review-pr" | "/review" => {
                let text = format!("review pr {}", self.text);
                parse_command(&text)
            }
            "/analyze" | "/scope" | "/estimate" => {
                let text = format!("analyze {}", self.text);
                parse_command(&text)
            }
            "/status" | "/buddystatus" => SlackCommand::Status,
            "/pause" => SlackCommand::Pause,
            "/resume" | "/continue" => SlackCommand::Resume,
            "/cancel" | "/abort" => SlackCommand::Cancel,
            _ => {
                if self.text.is_empty() {
                    SlackCommand::Status
                } else {
                    parse_command(&self.text)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_documentation() {
        let cmd = parse_command("find documentation for MuleSoft API gateway");
        assert!(matches!(cmd, SlackCommand::FindDocumentation { .. }));
        if let SlackCommand::FindDocumentation { topic, repo } = cmd {
            assert_eq!(topic, "MuleSoft API gateway");
            assert!(repo.is_none());
        }
    }

    #[test]
    fn test_find_documentation_in_repo() {
        let cmd = parse_command("find documentation for OAuth in org/payments");
        assert!(matches!(cmd, SlackCommand::FindDocumentation { .. }));
        if let SlackCommand::FindDocumentation { topic, repo } = cmd {
            assert_eq!(topic, "OAuth");
            assert_eq!(repo.unwrap(), "org/payments");
        }
    }

    #[test]
    fn test_find_documentation_on_repo() {
        let cmd = parse_command("find documentation for OAuth on org/payments");
        assert!(matches!(cmd, SlackCommand::FindDocumentation { .. }));
        if let SlackCommand::FindDocumentation { topic, repo } = cmd {
            assert_eq!(topic, "OAuth");
            assert_eq!(repo.unwrap(), "org/payments");
        }
    }

    #[test]
    fn test_take_story() {
        let cmd = parse_command("take story on org/payments #142");
        assert!(matches!(cmd, SlackCommand::TakeStory { .. }));
        if let SlackCommand::TakeStory { repo, issue } = cmd {
            assert_eq!(repo, "org/payments");
            assert_eq!(issue, "#142");
        }
    }

    #[test]
    fn test_implement_issue() {
        let cmd = parse_command("implement issue #142 on org/payments");
        assert!(matches!(cmd, SlackCommand::TakeStory { .. }));
        if let SlackCommand::TakeStory { repo, issue } = cmd {
            assert_eq!(repo, "org/payments");
            assert_eq!(issue, "#142");
        }
    }

    #[test]
    fn test_review_pr() {
        let cmd = parse_command("review pr org/payments#42");
        assert!(matches!(cmd, SlackCommand::ReviewPullRequest { .. }));
        if let SlackCommand::ReviewPullRequest { repo, pr_number } = cmd {
            assert_eq!(repo, "org/payments");
            assert_eq!(pr_number, 42);
        }
    }

    #[test]
    fn test_review_pr_on() {
        let cmd = parse_command("review pr on org/payments #42");
        assert!(matches!(cmd, SlackCommand::ReviewPullRequest { .. }));
        if let SlackCommand::ReviewPullRequest { repo, pr_number } = cmd {
            assert_eq!(repo, "org/payments");
            assert_eq!(pr_number, 42);
        }
    }

    #[test]
    fn test_analyze() {
        let cmd = parse_command("analyze org/payments for adding refund workflow");
        assert!(matches!(cmd, SlackCommand::Analyze { .. }));
        if let SlackCommand::Analyze { repo, goal } = cmd {
            assert_eq!(repo, "org/payments");
            assert_eq!(goal, "adding refund workflow");
        }
    }

    #[test]
    fn test_status() {
        assert!(matches!(parse_command("status"), SlackCommand::Status));
        assert!(matches!(parse_command("progress"), SlackCommand::Status));
    }

    #[test]
    fn test_pause_resume_cancel() {
        assert!(matches!(parse_command("pause"), SlackCommand::Pause));
        assert!(matches!(parse_command("resume"), SlackCommand::Resume));
        assert!(matches!(parse_command("cancel"), SlackCommand::Cancel));
    }

    #[test]
    fn test_slash_commands() {
        let payload = SlashCommandPayload {
            command: "/find-docs".to_string(),
            text: "MuleSoft API gateway".to_string(),
            user_id: "U01".to_string(),
            channel_id: "C01".to_string(),
            team_id: "T01".to_string(),
            response_url: "https://hooks.slack.com".to_string(),
        };
        let cmd = payload.to_slack_command();
        assert!(matches!(cmd, SlackCommand::FindDocumentation { .. }));
    }

    #[test]
    fn test_strip_mention() {
        let cmd = parse_command("<@U0CFXJU438E> find documentation for MuleSoft");
        assert!(matches!(cmd, SlackCommand::FindDocumentation { .. }));
    }

    #[test]
    fn test_hello() {
        assert!(matches!(parse_command("hello"), SlackCommand::Status));
        assert!(matches!(parse_command("test"), SlackCommand::Status));
    }
}