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

use std::process::{Command, Stdio};

use crate::domain::{ReviewBackend, ReviewCommandSettings};

use super::review::{
    ReviewRunContext, ReviewRunFailure, ReviewRunOutcome, review_command_envs,
    stream_review_command,
};

const CUSTOM_REVIEW_COMMAND_NAME: &str = "review-pr";
const CUSTOM_PR_DESCRIPTION_COMMAND_NAME: &str = "pr-description";

fn custom_command_prompt_message(pr_url: &str, pr_number: u64) -> String {
    format!("/{CUSTOM_REVIEW_COMMAND_NAME} {pr_url} {pr_number}")
}

fn pr_description_prompt_message(pr_url: &str, pr_number: u64) -> String {
    format!("/{CUSTOM_PR_DESCRIPTION_COMMAND_NAME} {pr_url} {pr_number}")
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
    let mut command = base_claude_command(repo_path, review_settings, github_token);
    command.arg(custom_command_prompt_message(pr_url, pr_number));
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
    let mut command = base_claude_command(repo_path, review_settings, github_token);
    command.arg(pr_description_prompt_message(pr_url, pr_number));
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
}
