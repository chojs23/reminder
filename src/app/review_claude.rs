// Claude Code (`claude -p`) review backend.
//
// Mirrors the three entry points in `super::review` but spawns
// `claude -p ... --output-format stream-json --verbose` and parses the
// Anthropic SDK stream JSON event shape. No HTTP server; no session-tree
// polling — `claude -p` terminates on its own `result` event.

use serde_json::Value;

pub(super) fn session_id(event: &Value) -> Option<&str> {
    event.get("session_id").and_then(Value::as_str)
}

pub(super) fn render_event(event: &Value) -> Option<String> {
    let event_type = event.get("type")?.as_str()?;
    match event_type {
        "assistant" => render_message_content(event.get("message")?),
        "user" => render_message_content(event.get("message")?),
        "system" | "result" | "reasoning" => None,
        _ => None,
    }
}

fn render_message_content(message: &Value) -> Option<String> {
    let content = message.get("content")?.as_array()?;
    let mut out = String::new();
    for part in content {
        if let Some(text) = render_part(part) {
            out.push_str(&text);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn render_part(part: &Value) -> Option<String> {
    let part_type = part.get("type")?.as_str()?;
    match part_type {
        "text" => {
            let text = part.get("text")?.as_str()?.trim();
            if text.is_empty() {
                None
            } else {
                Some(format!("{text}\n\n"))
            }
        }
        "tool_use" => {
            let name = part.get("name").and_then(Value::as_str).unwrap_or("tool");
            let input_summary = part
                .get("input")
                .map(summarize_tool_input)
                .unwrap_or_default();
            if input_summary.is_empty() {
                Some(format!("[{name}]\n\n"))
            } else {
                Some(format!("[{name}] {input_summary}\n\n"))
            }
        }
        "tool_result" => {
            let content = part.get("content")?;
            let text = match content {
                Value::String(s) => s.clone(),
                Value::Array(items) => items
                    .iter()
                    .filter_map(|item| {
                        item.get("text")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .or_else(|| item.as_str().map(str::to_owned))
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                other => other.to_string(),
            };
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(format!("{trimmed}\n\n"))
            }
        }
        _ => None,
    }
}

fn summarize_tool_input(input: &Value) -> String {
    // Prefer a `command`, `file_path`, or `description` field for readability;
    // fall back to the compact JSON serialisation truncated to 200 chars.
    for key in ["command", "file_path", "description", "query", "url"] {
        if let Some(value) = input.get(key).and_then(Value::as_str) {
            return value.to_owned();
        }
    }
    let compact = input.to_string();
    if compact.chars().count() > 200 {
        let truncated: String = compact.chars().take(200).collect();
        format!("{truncated}...")
    } else {
        compact
    }
}

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::domain::{ReviewBackend, ReviewCommandSettings};

use super::review::{
    ReviewRunContext, ReviewRunFailure, ReviewRunOutcome, review_command_envs,
    stream_review_command,
};
use super::{CUSTOM_PR_DESCRIPTION_COMMAND_NAME, CUSTOM_REVIEW_COMMAND_NAME};

fn custom_review_slash_prompt(pr_url: &str, pr_number: u64) -> String {
    format!("/{CUSTOM_REVIEW_COMMAND_NAME} {pr_url} {pr_number}")
}

fn pr_description_slash_prompt(pr_url: &str, pr_number: u64) -> String {
    format!("/{CUSTOM_PR_DESCRIPTION_COMMAND_NAME} {pr_url} {pr_number}")
}

pub(super) fn default_review_prompt_md_path_display() -> String {
    default_review_prompt_md_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "Unavailable (set HOME)".to_owned())
}

pub(super) fn default_pr_description_prompt_md_path_display() -> String {
    default_pr_description_prompt_md_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "Unavailable (set HOME)".to_owned())
}

fn default_review_prompt_md_path() -> Option<PathBuf> {
    env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".claude/commands/review-pr.md"))
}

fn default_pr_description_prompt_md_path() -> Option<PathBuf> {
    env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".claude/commands/pr-description.md"))
}

fn prompt_from_md_or_slash(
    md_path: Option<&str>,
    pr_url: &str,
    pr_number: u64,
    slash_fallback: impl FnOnce(&str, u64) -> String,
) -> Result<String, String> {
    let Some(path) = md_path.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(slash_fallback(pr_url, pr_number));
    };
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("Failed to read prompt file {path}: {err}"))?;
    let body = strip_yaml_frontmatter(&raw);
    Ok(format!(
        "{body}\n\nPR URL: {pr_url}\nPR number: {pr_number}"
    ))
}

fn strip_yaml_frontmatter(text: &str) -> &str {
    // Strip a leading `---\n...---\n` frontmatter block if present.
    // Slash command files use frontmatter for metadata that has no meaning
    // when the file body is passed to `claude -p`; leaving it in also causes
    // the CLI parser to treat a leading `---` as an unknown option flag.
    let Some(remainder) = text.strip_prefix("---\n") else {
        return text.trim_start();
    };
    if let Some(end) = remainder.find("\n---\n") {
        remainder[end + "\n---\n".len()..].trim_start()
    } else if let Some(end) = remainder.find("\n---") {
        remainder[end + "\n---".len()..].trim_start()
    } else {
        text.trim_start()
    }
}

fn base_claude_command(
    repo_path: &str,
    review_settings: &ReviewCommandSettings,
    github_token: &str,
) -> Command {
    let mut command = Command::new("claude");
    command.current_dir(repo_path);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.envs(review_command_envs(review_settings, github_token));
    command.arg("-p");
    command
}

fn apply_stream_flags(command: &mut Command) {
    command.arg("--output-format");
    command.arg("stream-json");
    command.arg("--verbose");
}

pub(super) fn run_custom_review(
    context: ReviewRunContext<'_>,
    repo_path: &str,
    pr_number: u64,
    pr_url: &str,
    review_settings: &ReviewCommandSettings,
    github_token: &str,
) -> Result<ReviewRunOutcome, ReviewRunFailure> {
    let prompt = prompt_from_md_or_slash(
        review_settings.review_prompt_md_path.as_deref(),
        pr_url,
        pr_number,
        custom_review_slash_prompt,
    )
    .map_err(|message| ReviewRunFailure {
        message,
        session_id: None,
    })?;
    let mut command = base_claude_command(repo_path, review_settings, github_token);
    command.arg(prompt);
    apply_stream_flags(&mut command);
    command.args(&review_settings.additional_args);
    println!(
        "Running claude review command for {}",
        context.review_label
    );
    stream_review_command(
        context.tx,
        context.thread_id,
        context.review_label,
        context.attach_url,
        command,
        context.child_handle,
        context.cancel_requested,
        ReviewBackend::Claude,
    )
}

pub(super) fn run_pr_description(
    context: ReviewRunContext<'_>,
    repo_path: &str,
    pr_number: u64,
    pr_url: &str,
    review_settings: &ReviewCommandSettings,
    github_token: &str,
) -> Result<ReviewRunOutcome, ReviewRunFailure> {
    let prompt = prompt_from_md_or_slash(
        review_settings.pr_description_md_path.as_deref(),
        pr_url,
        pr_number,
        pr_description_slash_prompt,
    )
    .map_err(|message| ReviewRunFailure {
        message,
        session_id: None,
    })?;
    let mut command = base_claude_command(repo_path, review_settings, github_token);
    command.arg(prompt);
    apply_stream_flags(&mut command);
    command.args(&review_settings.additional_args);
    println!(
        "Running claude PR description command for {}",
        context.review_label
    );
    stream_review_command(
        context.tx,
        context.thread_id,
        context.review_label,
        context.attach_url,
        command,
        context.child_handle,
        context.cancel_requested,
        ReviewBackend::Claude,
    )
}

pub(super) fn run_review_follow_up(
    context: ReviewRunContext<'_>,
    repo_path: &str,
    session_id: &str,
    prompt: &str,
    review_settings: &ReviewCommandSettings,
    github_token: &str,
) -> Result<ReviewRunOutcome, ReviewRunFailure> {
    let mut command = base_claude_command(repo_path, review_settings, github_token);
    command.arg(prompt);
    command.arg("--resume");
    command.arg(session_id);
    // Follow-ups omit additional_args to match the opencode follow-up shape (review.rs).
    apply_stream_flags(&mut command);
    println!(
        "Running claude follow-up for {}",
        context.review_label
    );
    stream_review_command(
        context.tx,
        context.thread_id,
        context.review_label,
        context.attach_url,
        command,
        context.child_handle,
        context.cancel_requested,
        ReviewBackend::Claude,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_id_extracted_from_init_event() {
        let event = json!({"type":"system","subtype":"init","session_id":"abc"});
        assert_eq!(session_id(&event), Some("abc"));
    }

    #[test]
    fn session_id_extracted_from_result_event() {
        let event = json!({"type":"result","subtype":"success","session_id":"xyz","is_error":false});
        assert_eq!(session_id(&event), Some("xyz"));
    }

    #[test]
    fn session_id_absent_returns_none() {
        let event = json!({"type":"assistant","message":{"content":[]}});
        assert!(session_id(&event).is_none());
    }

    #[test]
    fn render_init_event_returns_none() {
        let event = json!({"type":"system","subtype":"init","session_id":"abc"});
        assert!(render_event(&event).is_none());
    }

    #[test]
    fn render_assistant_text() {
        let event = json!({
            "type":"assistant",
            "message":{"content":[{"type":"text","text":"Looks good."}]}
        });
        assert_eq!(render_event(&event), Some("Looks good.\n\n".to_owned()));
    }

    #[test]
    fn render_assistant_tool_use_labels_tool() {
        let event = json!({
            "type":"assistant",
            "message":{"content":[{
                "type":"tool_use",
                "name":"Bash",
                "input":{"command":"gh pr diff 1"}
            }]}
        });
        let rendered = render_event(&event).expect("rendered");
        assert!(rendered.contains("[Bash]"));
        assert!(rendered.contains("gh pr diff 1"));
    }

    #[test]
    fn render_user_tool_result_uses_content() {
        let event = json!({
            "type":"user",
            "message":{"content":[{
                "type":"tool_result",
                "tool_use_id":"t1",
                "content":"output line"
            }]}
        });
        let rendered = render_event(&event).expect("rendered");
        assert!(rendered.contains("output line"));
    }

    #[test]
    fn render_result_success_returns_none() {
        // The streaming loop handles terminal status separately; render returns None.
        let event = json!({"type":"result","subtype":"success","session_id":"x","is_error":false});
        assert!(render_event(&event).is_none());
    }

    #[test]
    fn render_unknown_event_returns_none() {
        let event = json!({"type":"reasoning","data":"thinking..."});
        assert!(render_event(&event).is_none());
    }

    #[test]
    fn prompt_falls_back_to_slash_when_md_path_missing() {
        let prompt =
            prompt_from_md_or_slash(None, "https://example/pr/1", 1, custom_review_slash_prompt)
                .expect("prompt");
        assert_eq!(prompt, "/review-pr https://example/pr/1 1");
    }

    #[test]
    fn prompt_falls_back_to_slash_when_md_path_blank() {
        let prompt = prompt_from_md_or_slash(
            Some("   "),
            "https://example/pr/2",
            2,
            custom_review_slash_prompt,
        )
        .expect("prompt");
        assert_eq!(prompt, "/review-pr https://example/pr/2 2");
    }

    #[test]
    fn prompt_uses_file_contents_when_md_path_set() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "reminder-prompt-{}.md",
            std::process::id()
        ));
        std::fs::write(&path, "Custom review instructions.\n").expect("write");
        let prompt = prompt_from_md_or_slash(
            Some(&path.to_string_lossy()),
            "https://example/pr/3",
            3,
            custom_review_slash_prompt,
        )
        .expect("prompt");
        let _ = std::fs::remove_file(&path);
        assert!(prompt.starts_with("Custom review instructions."));
        assert!(prompt.contains("PR URL: https://example/pr/3"));
        assert!(prompt.contains("PR number: 3"));
    }

    #[test]
    fn strip_frontmatter_removes_leading_block() {
        let input = "---\ndescription: foo\n---\nBody line.\n";
        assert_eq!(strip_yaml_frontmatter(input), "Body line.\n");
    }

    #[test]
    fn strip_frontmatter_keeps_text_without_frontmatter() {
        let input = "No frontmatter here.\nSecond line.\n";
        assert_eq!(strip_yaml_frontmatter(input), input);
    }

    #[test]
    fn strip_frontmatter_handles_trailing_dashes_without_newline() {
        let input = "---\nkey: val\n---";
        assert_eq!(strip_yaml_frontmatter(input), "");
    }

    #[test]
    fn prompt_uses_file_contents_strips_frontmatter() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "reminder-prompt-fm-{}.md",
            std::process::id()
        ));
        std::fs::write(
            &path,
            "---\ndescription: foo\n---\nReview body.\n",
        )
        .expect("write");
        let prompt = prompt_from_md_or_slash(
            Some(&path.to_string_lossy()),
            "https://example/pr/5",
            5,
            custom_review_slash_prompt,
        )
        .expect("prompt");
        let _ = std::fs::remove_file(&path);
        assert!(!prompt.starts_with("---"));
        assert!(prompt.starts_with("Review body."));
        assert!(prompt.contains("PR URL: https://example/pr/5"));
    }

    #[test]
    fn prompt_returns_error_when_md_path_unreadable() {
        let err = prompt_from_md_or_slash(
            Some("/nonexistent/path/should-not-exist.md"),
            "https://example/pr/4",
            4,
            custom_review_slash_prompt,
        )
        .expect_err("expected read failure");
        assert!(err.starts_with("Failed to read prompt file"));
    }
}
