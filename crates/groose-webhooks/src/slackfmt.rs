//! Convert standard Markdown to Slack's mrkdwn format.
//!
//! Slack's mrkdwn differs from GitHub Markdown:
//! - Headers (# ##) → bold text
//! - Bold **text** → *text*
//! - Tables → bulleted lists
//! - Links [text](url) → <url|text>
//! - Horizontal rules --- → divider (handled by caller via blocks)

/// Convert Markdown text to Slack-compatible mrkdwn.
pub fn to_slack_mrkdwn(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut in_code_block = false;
    let mut in_table = false;
    let mut table_lines: Vec<String> = Vec::new();

    for line in md.lines() {
        let trimmed = line.trim();

        // Code block fences — pass through unchanged
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            out.push_str(trimmed);
            out.push('\n');
            continue;
        }
        if in_code_block {
            out.push_str(line);
            out.push('\n');
            continue;
        }

        // Table rows — buffer and convert at table end
        if trimmed.starts_with('|') && trimmed.ends_with('|') {
            in_table = true;
            table_lines.push(trimmed.to_string());
            continue;
        }

        // Flush any buffered table
        if in_table {
            out.push_str(&convert_table(&table_lines));
            table_lines.clear();
            in_table = false;
        }

        // Headers → bold
        if let Some(rest) = trimmed.strip_prefix("#### ") {
            out.push('*');
            out.push_str(rest);
            out.push_str("*\n");
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("### ") {
            out.push('*');
            out.push_str(rest);
            out.push_str("*\n");
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            out.push('*');
            out.push_str(rest);
            out.push_str("*\n");
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            out.push('*');
            out.push_str(rest);
            out.push_str("*\n");
            continue;
        }

        // Horizontal rule
        if trimmed == "---" || trimmed == "***" || trimmed == "___" {
            out.push_str("──────────\n");
            continue;
        }

        // Bold **text** → *text*
        let converted = convert_bold(line);
        // Links [text](url) → <url|text>
        let converted = convert_links(&converted);
        // Lists: keep - and * as-is (Slack renders them fine as bullets)
        out.push_str(&converted);
        out.push('\n');
    }

    // Flush trailing table
    if in_table {
        out.push_str(&convert_table(&table_lines));
    }

    out
}

/// Convert Markdown bold (**text**) to Slack bold (*text*).
fn convert_bold(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '*' && chars.peek() == Some(&'*') {
            chars.next(); // consume second *
            out.push('*');
        } else {
            out.push(c);
        }
    }
    out
}

/// Convert Markdown links [text](url) to Slack links <url|text>.
fn convert_links(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find('[') {
        out.push_str(&rest[..start]);
        if let Some(mid) = rest[start..].find("](") {
            let label = &rest[start + 1..start + mid];
            let after_mid = &rest[start + mid + 2..];
            if let Some(end) = after_mid.find(')') {
                let url = &after_mid[..end];
                out.push('<');
                out.push_str(url);
                out.push('|');
                out.push_str(label);
                out.push('>');
                rest = &after_mid[end + 1..];
                continue;
            }
        }
        // Not a valid link — pass through
        out.push_str(&rest[start..start + 1]);
        rest = &rest[start + 1..];
    }
    out.push_str(rest);
    out
}

/// Convert a Markdown table to a Slack-friendly list format.
fn convert_table(rows: &[String]) -> String {
    if rows.len() < 2 {
        return rows.join("\n") + "\n";
    }

    // Parse header row
    let headers: Vec<String> = rows[0]
        .split('|')
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();

    // Skip the separator row (|---|---|)
    let data_rows: Vec<&String> = rows[1..]
        .iter()
        .filter(|r| !r.contains("---"))
        .collect();

    let mut out = String::new();
    for row in data_rows {
        let cells: Vec<String> = row
            .split('|')
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect();

        if cells.len() >= 2 {
            // Format as: *Header1*: value — *Header2*: value
            let mut parts = Vec::new();
            for (i, cell) in cells.iter().enumerate() {
                if i < headers.len() {
                    parts.push(format!("*{}*: {}", headers[i], cell));
                } else {
                    parts.push(cell.clone());
                }
            }
            out.push_str(&format!("• {}\n", parts.join("  |  ")));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_headers() {
        let md = "# Hello World\n## Section 1\n";
        let out = to_slack_mrkdwn(md);
        assert!(out.contains("*Hello World*"));
        assert!(out.contains("*Section 1*"));
        assert!(!out.contains("# "));
    }

    #[test]
    fn test_bold() {
        let md = "This is **bold** text";
        let out = to_slack_mrkdwn(md);
        assert_eq!(out.trim(), "This is *bold* text");
    }

    #[test]
    fn test_links() {
        let md = "See [the docs](https://example.com)";
        let out = to_slack_mrkdwn(md);
        assert!(out.contains("<https://example.com|the docs>"));
    }

    #[test]
    fn test_table() {
        let md = "| Name | Value |\n|---|---|\n| Payload | data |\n| Attributes | meta |\n";
        let out = to_slack_mrkdwn(md);
        assert!(out.contains("*Name*: Payload"));
        assert!(out.contains("*Value*: data"));
    }

    #[test]
    fn test_code_block_preserved() {
        let md = "```\n**not bold**\n```\n";
        let out = to_slack_mrkdwn(md);
        assert!(out.contains("**not bold**"));
    }

    #[test]
    fn test_hr() {
        let md = "above\n---\nbelow";
        let out = to_slack_mrkdwn(md);
        assert!(out.contains("──────────"));
    }
}
