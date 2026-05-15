// Claude Code (`claude -p`) review backend.
//
// Mirrors the three entry points in `super::review` but spawns
// `claude -p ... --output-format stream-json --verbose` and parses the
// Anthropic SDK stream JSON event shape. No HTTP server; no session-tree
// polling — `claude -p` terminates on its own `result` event.

use serde_json::Value;

pub(super) fn render_event(event: &Value) -> Option<String> {
    let _ = event;
    None
}

pub(super) fn session_id(event: &Value) -> Option<&str> {
    let _ = event;
    None
}
