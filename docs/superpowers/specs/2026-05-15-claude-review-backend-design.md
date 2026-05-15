# Claude Code review backend

## Background

`reminder` currently runs PR reviews exclusively through `opencode`: it spawns `opencode serve` as a local HTTP server, then runs `opencode run --attach <url> --format json --command review-pr ...` and streams JSONL events to the UI (`src/app/review.rs:244-273`, `:1368-1403`). The opencode review path requires an LLM provider credential configured inside opencode (Anthropic API key, OpenAI key, GitHub Copilot OAuth, etc.).

For a Claude Max subscriber there is no usable provider in opencode — Max grants access to claude.ai and Claude Code, not the Anthropic API — so the review action fails with `Personal Access Tokens are not supported for this endpoint` when opencode falls back to the GitHub PAT.

Goal: let an account run reviews through `claude -p` (Claude Code) instead, using the Max-authenticated local install, without disturbing accounts that still want to use opencode.

## Decisions (locked in brainstorming)

1. **Per-account backend choice.** A new field on `ReviewCommandSettings` selects opencode vs claude per account.
2. **Slash commands stay user-owned.** Claude path invokes `claude -p "/review-pr <args>"`; the user is expected to maintain `~/.claude/commands/review-pr.md` and `~/.claude/commands/pr-description.md` themselves (mirrors the existing opencode convention).
3. **Stream-json output.** Claude path runs with `--output-format stream-json --verbose` so events stream live to the UI and session IDs are captured for follow-ups.
4. **Opencode is the default.** Missing `backend` field in `accounts.json` deserialises to `Opencode`, so existing config files load unchanged.
5. **Minimal restructure.** No module split. Add one new file `src/app/review_claude.rs` and small dispatch branches inside the existing `src/app/review.rs`.

## Data model changes (`src/domain.rs`)

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewBackend {
    #[default]
    Opencode,
    Claude,
}

pub struct ReviewCommandSettings {
    #[serde(default)] pub backend: ReviewBackend,            // NEW
    #[serde(default)] pub env_vars: BTreeMap<String, String>,
    #[serde(default)] pub additional_args: Vec<String>,
    #[serde(default)] pub review_prompt_md_path: Option<String>,
    #[serde(default)] pub pr_description_md_path: Option<String>,
}
```

- Old `accounts.json` files load unchanged: missing `backend` → `Opencode`.
- `review_prompt_md_path` / `pr_description_md_path` are opencode-only configuration; they remain in the struct but are ignored when `backend == Claude`. The Settings UI greys them out with a tooltip in the claude case.

## UI changes (Settings)

The per-account Settings panel gets one new control above the existing review-command fields:

```
Review backend:  ( • ) Opencode   (   ) Claude Code
```

- Radio toggle bound to `ReviewCommandSettings::backend`.
- When `Claude Code` is selected:
  - `review_prompt_md_path` / `pr_description_md_path` rows are disabled with helper text *"Claude Code uses ~/.claude/commands/ instead."*
  - `env_vars` / `additional_args` remain editable (still useful — passed through to the `claude` subprocess).
- Change takes effect on the next review run; no restart of the reminder process required.

No other UI surfaces change. Notification / repository cards keep their "Run review-pr" button; the backend choice is invisible at the trigger point.

## Dispatch in `src/app/review.rs`

The three `run_*` entry points each gain a single early-return branch at the top:

```rust
fn run_custom_review(
    context: ReviewRunContext<'_>,
    repo_path: &str,
    pr_number: u64,
    pr_url: &str,
    review_settings: &ReviewCommandSettings,
    github_token: &str,
) -> Result<ReviewRunOutcome, ReviewRunFailure> {
    if matches!(review_settings.backend, ReviewBackend::Claude) {
        return crate::app::review_claude::run_custom_review(
            context, repo_path, pr_number, pr_url, review_settings, github_token,
        );
    }
    // ... existing opencode body unchanged ...
}
```

Same pattern for `run_pr_description` and `run_review_follow_up`.

The worker thread that spawns `ReviewServer::start` (around `review.rs:1100-1150`) gains a backend check: when `Claude`, skip server startup, pass an empty `attach_url` string, and proceed directly to `run_review_stream`. The claude backend ignores `attach_url`.

`mod.rs` for `src/app/` gains `pub(super) mod review_claude;`.

No other change to `review.rs`.

## `src/app/review_claude.rs` (new)

Public surface mirrors the three opencode entry points:

```rust
pub(super) fn run_custom_review(
    context: ReviewRunContext<'_>,
    repo_path: &str,
    pr_number: u64,
    pr_url: &str,
    review_settings: &ReviewCommandSettings,
    github_token: &str,
) -> Result<ReviewRunOutcome, ReviewRunFailure>;

pub(super) fn run_pr_description(/* same shape */) -> Result<...>;
pub(super) fn run_review_follow_up(
    context: ReviewRunContext<'_>,
    repo_path: &str,
    session_id: &str,
    prompt: &str,
    review_settings: &ReviewCommandSettings,
    github_token: &str,
) -> Result<ReviewRunOutcome, ReviewRunFailure>;
```

### Subprocess invocation

Custom review:

```bash
claude -p "/review-pr <pr_url> <pr_number>" \
  --output-format stream-json \
  --verbose \
  --cwd <repo_path> \
  <additional_args...>
```

PR description: identical except `/pr-description`.

Follow-up:

```bash
claude -p "<prompt>" \
  --resume <session_id> \
  --output-format stream-json \
  --verbose \
  --cwd <repo_path>
```

`stdin` is `Null`, `stdout`/`stderr` are `piped`. Env vars from `review_command_envs(review_settings, github_token)` are passed through unchanged so existing `GH_TOKEN` plumbing still works.

### Stream-json event mapping

Each stdout line is `serde_json::from_str::<Value>(line)`; events are dispatched by `type`:

| Event | Action |
|---|---|
| `{type:"system", subtype:"init", session_id}` | Send `ReviewJobMessage::SessionAssigned { session_id }` (or equivalent existing variant — match what opencode path emits) |
| `{type:"assistant", message:{content:[{type:"text", text}]}}` | Append `text` to output stream |
| `{type:"assistant", message:{content:[{type:"tool_use", name, input}]}}` | Append `[name] <one-line summary of input>` to output stream |
| `{type:"user", message:{content:[{type:"tool_result", content}]}}` | Append truncated tool result (mirror opencode's `format_review_tool_event` truncation) |
| `{type:"result", subtype:"success", session_id}` | Return `ReviewRunOutcome::Completed { session_id }` |
| `{type:"result", subtype:"error_*", session_id}` | Return `ReviewRunFailure { message, session_id }` |
| Unrecognised | Ignored |

The renderer is a single function `render_claude_stream_event(value: &Value) -> Option<String>` mirroring `render_review_json_event` in shape so the surrounding capture/mirror code can stay identical.

### Subprocess lifecycle

- Same `Arc<Mutex<Option<Child>>>` handle pattern as opencode for cancel support — `ReviewJob::cancel` keeps working.
- `cancel_requested` polled in the read loop; on cancel, kill child, return `ReviewRunOutcome::Cancelled`.
- On non-zero exit without a prior `result` event: return `ReviewRunFailure` with stderr tail as the message.
- No HTTP polling, no session-tree query, no settle wait — Claude Code's `--print` mode terminates exactly when the response is complete, so the stdout EOF / `result` event is authoritative.

### Error surfacing

| Cause | User-visible message |
|---|---|
| `claude` binary missing (spawn `NotFound`) | `"Claude Code CLI not found. Install: https://claude.com/code"` |
| `/review-pr` slash command missing | Pass through Claude's own error message (it emits `{type:"result", subtype:"error_..."}` with a helpful body) |
| `--resume <id>` fails (expired session) | Pass through Claude's error |
| Non-zero exit before `result` | Last 2KB of stderr |

## Backwards compatibility

- `accounts.json` schema: additive only. Old files load with `backend = Opencode`.
- Opencode code path: literally unchanged below the dispatch branch.
- Settings UI: existing fields stay; new radio appears above them.
- No migration step required.

## Out of scope

- Auto-detection of which slash commands exist in `~/.claude/commands/` (user is expected to maintain them).
- Translating opencode's `review-pr.md` to Claude Code frontmatter automatically.
- Per-account override of which model `claude -p` uses — accept whatever the user's Claude Code install is configured with.
- Showing tool call diff/output panels — text-only rendering, same as the opencode path today.

## Test plan

Unit:
- `render_claude_stream_event` covers each event shape above + an unknown event variant.
- `ReviewCommandSettings` deserialises from JSON without `backend` field → `Opencode` default.
- Dispatch in `run_custom_review` routes to `review_claude::run_custom_review` when backend is `Claude` (gated behind a small trait or function pointer to keep the test pure, or by extracting the dispatch decision into a helper).

Manual:
1. Existing account with `backend` absent — review still runs through opencode.
2. New Max account, backend = Claude, valid `~/.claude/commands/review-pr.md` — review streams live in the UI; session ID captured; follow-up resumes the session.
3. Max account without `~/.claude/commands/review-pr.md` — Claude's error message surfaces in the UI.
4. Cancel button mid-stream kills the `claude` subprocess.
5. `claude` not on PATH — friendly error appears.

## Rollout sequence

1. Add `ReviewBackend` enum and field; verify existing config loads.
2. Add `review_claude.rs` with subprocess + event parser; unit-test parser.
3. Wire dispatch branches in `review.rs`; skip `ReviewServer::start` for claude.
4. Add Settings UI radio + disabled-state styling.
5. Manual end-to-end with a real Max-authenticated install.
