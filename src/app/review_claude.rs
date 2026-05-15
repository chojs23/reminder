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
