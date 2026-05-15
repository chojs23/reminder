# Claude Code Review Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a per-account "Claude Code" review backend so Max subscribers can run `/review-pr` through `claude -p` instead of `opencode`.

**Architecture:** Add a `ReviewBackend` enum on `ReviewCommandSettings` (default `Opencode` for backward compat). Keep the existing `src/app/review.rs` flow intact and add three early-return dispatch branches inside the existing entry points. All Claude-specific subprocess + stream parsing lives in a new `src/app/review_claude.rs` module. The Claude path skips `opencode serve` and `wait_for_review_session_settle` since `claude -p` is one-shot and emits a terminal `result` event.

**Tech Stack:** Rust 2024, eframe/egui (UI), `serde_json::Value` for JSONL parsing, `std::process::Command` for subprocess.

---

## File Structure

- **Modify** `src/domain.rs` — add `ReviewBackend` enum and `backend` field on `ReviewCommandSettings`.
- **Modify** `src/app.rs` — declare new `review_claude` module, surface backend in Settings UI editor.
- **Modify** `src/app/review.rs` — three dispatch branches in `run_custom_review`/`run_pr_description`/`run_review_follow_up`; one `if backend == Claude { skip server }` branch around `ReviewServer::start` in the worker thread.
- **Create** `src/app/review_claude.rs` — Claude subprocess spawn, stream-json event parser, three entry points mirroring opencode signatures.

---

## Task 1: Add `ReviewBackend` enum and backward-compat test

**Files:**
- Modify: `src/domain.rs:8-18`
- Test: `src/domain.rs` (inline `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Append to `src/domain.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_command_settings_defaults_backend_to_opencode_when_field_missing() {
        let json = r#"{"env_vars":{},"additional_args":[]}"#;
        let parsed: ReviewCommandSettings = serde_json::from_str(json).expect("parse");
        assert_eq!(parsed.backend, ReviewBackend::Opencode);
    }

    #[test]
    fn review_command_settings_round_trips_claude_backend() {
        let mut settings = ReviewCommandSettings::default();
        settings.backend = ReviewBackend::Claude;
        let json = serde_json::to_string(&settings).expect("serialize");
        let parsed: ReviewCommandSettings = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed.backend, ReviewBackend::Claude);
        assert!(json.contains("\"backend\":\"claude\""));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail to compile**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test --lib domain::tests`
Expected: compile error — `ReviewBackend` not defined.

- [ ] **Step 3: Add the enum and field**

Edit `src/domain.rs:8-18` to:

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewBackend {
    #[default]
    Opencode,
    Claude,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCommandSettings {
    #[serde(default)]
    pub backend: ReviewBackend,
    #[serde(default)]
    pub env_vars: BTreeMap<String, String>,
    #[serde(default)]
    pub additional_args: Vec<String>,
    #[serde(default)]
    pub review_prompt_md_path: Option<String>,
    #[serde(default)]
    pub pr_description_md_path: Option<String>,
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test --lib domain::tests`
Expected: 2 tests pass.

- [ ] **Step 5: Run full check + clippy to catch other call sites**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo check`
Expected: pass. (The struct gains one field with a default; existing code constructing `ReviewCommandSettings { .. }` literally without `..Default::default()` will fail — fix those call sites by adding `backend: ReviewBackend::Opencode,` to keep behaviour unchanged. There is one in `src/app.rs:488-493`.)

If `src/app.rs:488-493` errors, patch it to:

```rust
let review_settings = ReviewCommandSettings {
    backend: self
        .accounts
        .iter()
        .find(|account| account.profile.login == login)
        .map(|account| account.profile.review_settings.backend)
        .unwrap_or_default(),
    env_vars,
    additional_args,
    review_prompt_md_path: normalize_optional_path(&editor.review_prompt_md_path_text),
    pr_description_md_path: normalize_optional_path(&editor.pr_description_md_path_text),
};
```

(Editor doesn't yet expose `backend` — preserves existing account's backend until Task 7 wires the UI.)

Re-run: `PATH="$HOME/.cargo/bin:$PATH" cargo check`
Expected: pass.

- [ ] **Step 6: Commit**

```bash
git add src/domain.rs src/app.rs
git commit -m "feat(domain): add ReviewBackend enum with Opencode default"
```

---

## Task 2: Stub `review_claude` module and register it

**Files:**
- Create: `src/app/review_claude.rs`
- Modify: `src/app.rs:5`

- [ ] **Step 1: Create the stub file**

Write `src/app/review_claude.rs`:

```rust
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
```

- [ ] **Step 2: Register the module**

Edit `src/app.rs` line 5 area. Current:

```rust
mod review;
```

Change to:

```rust
mod review;
mod review_claude;
```

- [ ] **Step 3: Verify it compiles**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo check`
Expected: pass.

- [ ] **Step 4: Commit**

```bash
git add src/app.rs src/app/review_claude.rs
git commit -m "feat(review): scaffold review_claude module"
```

---

## Task 3: Implement Claude stream-json event renderer (TDD)

**Files:**
- Modify: `src/app/review_claude.rs`
- Test: `src/app/review_claude.rs` (inline `#[cfg(test)] mod tests`)

Claude Code emits one JSON object per stdout line in this shape:

```
{"type":"system","subtype":"init","session_id":"abc","cwd":"...","model":"..."}
{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"...","content":"file1\nfile2"}]}}
{"type":"result","subtype":"success","session_id":"abc","is_error":false}
{"type":"result","subtype":"error_during_execution","session_id":"abc","is_error":true}
```

- [ ] **Step 1: Write the failing tests**

Append to `src/app/review_claude.rs`:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test --lib app::review_claude::tests`
Expected: 9 tests fail (the stub renderer returns None for everything; the text/tool tests fail).

- [ ] **Step 3: Implement the renderer**

Replace the body of `src/app/review_claude.rs` (keep the top comment and tests):

```rust
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
                .map(|input| summarize_tool_input(input))
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
    if compact.len() > 200 {
        format!("{}...", &compact[..200])
    } else {
        compact
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test --lib app::review_claude::tests`
Expected: 9 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/app/review_claude.rs
git commit -m "feat(review_claude): parse Anthropic stream-json events"
```

---

## Task 4: Make `read_review_json_stream` backend-aware

The existing streaming loop in `src/app/review.rs:1516-1585` is generic except for two callouts: `review_event_session_id` and `render_review_json_event`. We thread `ReviewBackend` through and dispatch.

**Files:**
- Modify: `src/app/review.rs:1516-1585`
- Modify: `src/app/review.rs:1587-1605` (stream_review_command signature)
- Modify call sites of `stream_review_command` at `:1394-1402`, `:1434-1442`, `:1471-1479`.

- [ ] **Step 1: Add a backend parameter to `read_review_json_stream`**

Edit the signature at `src/app/review.rs:1516`:

```rust
fn read_review_json_stream(
    tx: &mpsc::Sender<ReviewJobMessage>,
    thread_id: &str,
    reader: impl Read,
    shell_mirror: &Arc<Mutex<Option<ReviewShellMirror>>>,
    cancel_requested: &Arc<AtomicBool>,
    stream_label: &str,
    backend: crate::domain::ReviewBackend,
) -> Result<ReviewCommandCapture, String> {
```

Inside the function, replace the two hard-coded calls. The session_id extraction (two occurrences at `:1551` and `:1570`) becomes:

```rust
if capture.session_id.is_none() {
    capture.session_id = match backend {
        crate::domain::ReviewBackend::Opencode => {
            review_event_session_id(&event).map(str::to_owned)
        }
        crate::domain::ReviewBackend::Claude => {
            crate::app::review_claude::session_id(&event).map(str::to_owned)
        }
    };
}
```

The rendering calls (two occurrences at `:1556` and `:1575`) become:

```rust
let rendered = match backend {
    crate::domain::ReviewBackend::Opencode => render_review_json_event(&event),
    crate::domain::ReviewBackend::Claude => crate::app::review_claude::render_event(&event),
};
```

(The opencode-specific `review_event_part_id` call at `:1553`/`:1572` only matters for opencode's late-message dedup — guard it the same way:

```rust
if matches!(backend, crate::domain::ReviewBackend::Opencode) {
    if let Some(part_id) = review_event_part_id(&event) {
        capture.seen_part_ids.insert(part_id.to_owned());
    }
}
```
)

- [ ] **Step 2: Add backend parameter to `stream_review_command`**

Edit the signature at `src/app/review.rs:1587`:

```rust
fn stream_review_command(
    tx: &mpsc::Sender<ReviewJobMessage>,
    thread_id: &str,
    review_label: &str,
    attach_url: &str,
    mut command: Command,
    child_handle: Arc<Mutex<Option<Child>>>,
    cancel_requested: Arc<AtomicBool>,
    backend: crate::domain::ReviewBackend,
) -> Result<ReviewRunOutcome, ReviewRunFailure> {
```

Inside, the single call to `read_review_json_stream` at `:1683` adds the trailing arg `backend,`.

Also: the call to `wait_for_review_session_settle` at `:1755` is opencode-only. Wrap:

```rust
if status.success() {
    if matches!(backend, crate::domain::ReviewBackend::Opencode) {
        if let Some(session_id) = stdout_capture.session_id.clone() {
            wait_for_review_session_settle(
                &mut stdout_capture,
                &shell_mirror,
                tx,
                thread_id,
                attach_url,
                &session_id,
                &cancel_requested,
            )
            .map_err(|message| ReviewRunFailure {
                message,
                session_id: Some(session_id),
            })?;
        }
    }
    // ... existing success-return path continues ...
```

(Keep the rest of the success branch identical.)

- [ ] **Step 3: Update opencode call sites of `stream_review_command`**

In `run_custom_review` (`:1394-1402`), `run_pr_description` (`:1434-1442`), and `run_review_follow_up` (`:1471-1479`), append the trailing argument:

```rust
stream_review_command(
    context.tx,
    context.thread_id,
    context.review_label,
    context.attach_url,
    command,
    context.child_handle,
    context.cancel_requested,
    crate::domain::ReviewBackend::Opencode,
)
```

- [ ] **Step 4: Verify it compiles**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo check`
Expected: pass.

- [ ] **Step 5: Run existing tests to check nothing regresses**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test --lib`
Expected: all existing tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/app/review.rs
git commit -m "refactor(review): thread ReviewBackend through stream reader"
```

---

## Task 5: Implement Claude entry points (custom + pr-description + follow-up)

**Files:**
- Modify: `src/app/review_claude.rs`

These mirror the three opencode entry points and call back into `super::stream_review_command` with a Claude-shaped `Command`.

- [ ] **Step 1: Make the helpers visible**

The three Claude functions need to call `super::stream_review_command`, plus types `ReviewRunContext`, `ReviewRunOutcome`, `ReviewRunFailure`. Confirm they're already `pub(super)` or `pub(crate)`-visible from `review.rs`. The `Run*` types are private at the moment.

Edit `src/app/review.rs` to mark them `pub(super)`:

```rust
pub(super) enum ReviewRunOutcome {
    Completed { session_id: Option<String> },
    Cancelled { message: String, session_id: Option<String> },
}

pub(super) struct ReviewRunFailure {
    pub(super) message: String,
    pub(super) session_id: Option<String>,
}

pub(super) struct ReviewRunContext<'a> {
    pub(super) tx: &'a mpsc::Sender<ReviewJobMessage>,
    pub(super) thread_id: &'a str,
    pub(super) review_label: &'a str,
    pub(super) attach_url: &'a str,
    pub(super) child_handle: Arc<Mutex<Option<Child>>>,
    pub(super) cancel_requested: Arc<AtomicBool>,
}
```

And `stream_review_command` becomes `pub(super)`:

```rust
pub(super) fn stream_review_command(
    // ... unchanged signature ...
```

- [ ] **Step 2: Write the Claude entry points**

Append to `src/app/review_claude.rs` (above the `#[cfg(test)] mod tests`):

```rust
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
```

- [ ] **Step 3: Confirm `review_command_envs` is visible**

Look at `src/app/review.rs` for `fn review_command_envs`. If it isn't `pub(super)`, mark it:

```rust
pub(super) fn review_command_envs(
    review_settings: &ReviewCommandSettings,
    github_token: &str,
) -> Vec<(String, String)> {
    // ... unchanged body ...
}
```

(Don't change behaviour — visibility only.)

- [ ] **Step 4: Verify it compiles**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo check`
Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add src/app/review.rs src/app/review_claude.rs
git commit -m "feat(review_claude): implement custom/pr-description/follow-up entry points"
```

---

## Task 6: Wire dispatch branches in `review.rs`

**Files:**
- Modify: `src/app/review.rs:1368-1403` (`run_custom_review`)
- Modify: `src/app/review.rs:1405-1443` (`run_pr_description`)
- Modify: `src/app/review.rs:1445-1480` (`run_review_follow_up`)
- Modify: the worker thread that calls `ReviewServer::start` (around `src/app/review.rs:1100-1150`)

- [ ] **Step 1: Add early-return in `run_custom_review`**

At the very top of the function body (immediately after the opening `{` at `:1375`), insert:

```rust
if matches!(review_settings.backend, crate::domain::ReviewBackend::Claude) {
    return crate::app::review_claude::run_custom_review(
        context,
        repo_path,
        pr_number,
        pr_url,
        review_settings,
        github_token,
    );
}
```

(Leave the existing opencode body untouched below it.)

- [ ] **Step 2: Mirror the branch in `run_pr_description`**

At top of `run_pr_description` body, insert:

```rust
if matches!(review_settings.backend, crate::domain::ReviewBackend::Claude) {
    return crate::app::review_claude::run_pr_description(
        context,
        repo_path,
        pr_number,
        pr_url,
        review_settings,
        github_token,
    );
}
```

- [ ] **Step 3: Mirror the branch in `run_review_follow_up`**

At top of `run_review_follow_up` body, insert:

```rust
if matches!(review_settings.backend, crate::domain::ReviewBackend::Claude) {
    return crate::app::review_claude::run_review_follow_up(
        context,
        repo_path,
        session_id,
        prompt,
        review_settings,
        github_token,
    );
}
```

- [ ] **Step 4: Skip `ReviewServer::start` for Claude backend in worker**

Locate the worker code around `src/app/review.rs:1100-1150` where `ReviewServer::start(...)` is called and a `ServerReady` message is sent.

The current shape (paraphrased) is:

```rust
let server = match ReviewServer::start(&repo_path, &review_settings, &github_token) {
    Ok(server) => server,
    Err(err) => { /* report and return */ }
};
let attach_url = server.url().to_owned();
if tx.send(ReviewJobMessage::ServerReady { thread_id, server }).is_err() {
    return;
}
```

Replace with a backend check:

```rust
let attach_url = if matches!(review_settings.backend, crate::domain::ReviewBackend::Claude) {
    String::new()
} else {
    let server = match ReviewServer::start(&repo_path, &review_settings, &github_token) {
        Ok(server) => server,
        Err(err) => {
            let _ = tx.send(ReviewJobMessage::FinishedFailure {
                thread_id: worker_thread_id,
                captured_at: Utc::now(),
                session_id: None,
                message: err,
            });
            return;
        }
    };
    let attach_url = server.url().to_owned();
    if tx.send(ReviewJobMessage::ServerReady { thread_id: worker_thread_id.clone(), server }).is_err() {
        return;
    }
    attach_url
};
```

(Match local variable names exactly to what's already in scope — `worker_thread_id`, `review_settings`, etc. The exact existing layout is at `:1100-1155`; preserve the surrounding control flow.)

- [ ] **Step 5: Verify it compiles**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo check`
Expected: pass.

- [ ] **Step 6: Run all tests**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test --lib`
Expected: pass. No existing behaviour relies on backend != default, so opencode path stays identical.

- [ ] **Step 7: Commit**

```bash
git add src/app/review.rs
git commit -m "feat(review): dispatch to claude backend per ReviewCommandSettings"
```

---

## Task 7: Settings UI — add backend radio toggle

**Files:**
- Modify: `src/app.rs:451-468` (`open_review_settings_editor`)
- Modify: `src/app.rs:486-493` (`save_review_settings`)
- Modify: `src/app.rs:526-591` (`render_review_settings_window`)
- Modify: `src/app.rs:1524-1531` (`struct AccountReviewSettingsEditor`)
- Modify: `src/app.rs:39-44` (use ReviewBackend)

- [ ] **Step 1: Add `ReviewBackend` to the imports**

Edit `src/app.rs:39-44`:

```rust
use crate::{
    domain::{
        GitHubAccount, PullRequestReviewer, PullRequestReviewerStatus, ReviewBackend,
        ReviewCommandSettings,
    },
    storage::AccountStore,
};
```

- [ ] **Step 2: Add `backend` to the editor struct**

Edit `src/app.rs:1524-1531`:

```rust
struct AccountReviewSettingsEditor {
    login: String,
    backend: ReviewBackend,
    env_vars_text: String,
    additional_args_text: String,
    review_prompt_md_path_text: String,
    pr_description_md_path_text: String,
    form_error: Option<String>,
}
```

- [ ] **Step 3: Initialise `backend` when opening the editor**

Edit `src/app.rs:451-468`. Insert `backend: account.profile.review_settings.backend,` right after `login:`:

```rust
self.review_settings_editor = Some(AccountReviewSettingsEditor {
    login: account.profile.login.clone(),
    backend: account.profile.review_settings.backend,
    env_vars_text: format_review_env_vars(&account.profile.review_settings),
    additional_args_text: format_review_additional_args(&account.profile.review_settings),
    review_prompt_md_path_text: account
        .profile
        .review_settings
        .review_prompt_md_path
        .clone()
        .unwrap_or_default(),
    pr_description_md_path_text: account
        .profile
        .review_settings
        .pr_description_md_path
        .clone()
        .unwrap_or_default(),
    form_error: None,
});
```

- [ ] **Step 4: Persist `backend` on save**

Edit `src/app.rs:488-493`. Replace the `ReviewCommandSettings { ... }` literal with:

```rust
let review_settings = ReviewCommandSettings {
    backend: editor.backend,
    env_vars,
    additional_args,
    review_prompt_md_path: normalize_optional_path(&editor.review_prompt_md_path_text),
    pr_description_md_path: normalize_optional_path(&editor.pr_description_md_path_text),
};
```

(Drop the temporary lookup from Task 1 Step 5.)

- [ ] **Step 5: Render the radio toggle**

Edit `src/app.rs:540-568` — at the top of the `.show(ctx, |ui| { ... })` closure (just above the existing `ui.label("Environment variables...")`), insert:

```rust
ui.label("Review backend");
ui.horizontal(|row| {
    row.radio_value(&mut editor.backend, ReviewBackend::Opencode, "Opencode");
    row.radio_value(&mut editor.backend, ReviewBackend::Claude, "Claude Code");
});
let claude_selected = matches!(editor.backend, ReviewBackend::Claude);
if claude_selected {
    ui.label("Claude Code uses ~/.claude/commands/ instead of the prompt paths below.");
}
ui.add_space(8.0);
```

Then wrap the "Review prompt md path" and "PR Description md path" rows in `ui.add_enabled_ui(!claude_selected, |ui| { ... })`:

```rust
ui.add_enabled_ui(!claude_selected, |ui| {
    ui.label("Review prompt md path");
    ui.add(
        egui::TextEdit::singleline(&mut editor.review_prompt_md_path_text)
            .desired_width(f32::INFINITY)
            .hint_text(default_review_prompt_md_path_display()),
    );
    ui.add_space(8.0);
    ui.label("PR Description md path");
    ui.add(
        egui::TextEdit::singleline(&mut editor.pr_description_md_path_text)
            .desired_width(f32::INFINITY)
            .hint_text(default_pr_description_prompt_md_path_display()),
    );
});
```

- [ ] **Step 6: Verify it compiles**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo check`
Expected: pass.

- [ ] **Step 7: Run all tests**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo test --lib`
Expected: pass.

- [ ] **Step 8: Run clippy**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo clippy --all-targets --all-features -- -D warnings`
Expected: pass.

- [ ] **Step 9: Commit**

```bash
git add src/app.rs
git commit -m "feat(ui): add review backend radio to Settings"
```

---

## Task 8: Manual smoke test

**Files:** none (manual verification).

- [ ] **Step 1: Build the release binary**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo run --release`
Expected: app launches.

- [ ] **Step 2: Prepare Claude Code commands**

Ensure these files exist (copy from `~/.config/opencode/commands/` as a starting point, then convert frontmatter to Claude Code's format if needed):
- `~/.claude/commands/review-pr.md`
- `~/.claude/commands/pr-description.md`

Verify: `claude -p "/review-pr help" --output-format stream-json --verbose` produces JSONL with at least one `{"type":"system",...}` line.

- [ ] **Step 3: Switch one account's backend to Claude**

In the app: Account → Settings → Review backend: select **Claude Code** → Save.

- [ ] **Step 4: Run a review**

Pick a PR, click the `review-pr` button. Confirm:
- Stream appears live in the review window.
- Session ID is captured (the follow-up input becomes enabled when the run finishes).
- A follow-up question runs through `claude -p --resume <id>`.

- [ ] **Step 5: Error-path checks**

- Rename `claude` temporarily (`mv ~/.local/bin/claude ~/.local/bin/claude.bak`), trigger a review, confirm the failure message contains "Failed to start review" with the OS-level not-found error. Restore `claude`.
- Delete `~/.claude/commands/review-pr.md`, trigger a review, confirm the surfaced error references the missing slash command (Claude emits this in its `result` event).
- Click Cancel mid-stream, confirm the `claude` process is killed and the run terminates with `Cancelled`.

- [ ] **Step 6: Verify opencode account still works**

For any account left on `Opencode`, run a review — must behave exactly as before this branch.

- [ ] **Step 7: Final clippy + tests**

Run: `PATH="$HOME/.cargo/bin:$PATH" cargo clippy --all-targets --all-features -- -D warnings && PATH="$HOME/.cargo/bin:$PATH" cargo test --lib`
Expected: pass.

- [ ] **Step 8: Push the branch and open a PR**

```bash
git push -u origin feat/claude-review-backend
gh pr create --fill
```
