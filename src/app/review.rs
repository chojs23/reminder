use std::{
    collections::{BTreeMap, HashSet},
    env,
    fs::OpenOptions,
    io::{BufReader, ErrorKind, IsTerminal, Read, Write},
    mem,
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

use chrono::Utc;
use eframe::egui::{
    self, Color32, Context, FontId, Stroke,
    text::{LayoutJob, TextFormat},
};
use reqwest::blocking::Client;
use serde_json::Value;
use vt100::Parser as TerminalParser;

#[cfg(test)]
use super::MAX_REVIEW_OUTPUT_CHARS;
use super::time::format_local_timestamp;
use super::{
    CUSTOM_PR_DESCRIPTION_COMMAND_NAME, CUSTOM_REVIEW_COMMAND_NAME, repo_paths::canonical_repo_key,
};
use crate::domain::ReviewCommandSettings;

const SHELL_STREAM_DISABLED_NOTE: &str =
    "[Shell streaming unavailable. This window still renders common ANSI styles inline.]";
const GH_TOKEN_ENV_VAR: &str = "GH_TOKEN";
const GITHUB_TOKEN_ENV_VAR: &str = "GITHUB_TOKEN";
const REVIEW_TEXT_SIZE: f32 = 13.0;
const REVIEW_TERMINAL_ROWS: u16 = 1000;
const REVIEW_TERMINAL_COLS: u16 = 240;
const OPENCODE_SERVER_HOST: &str = "127.0.0.1";
const OPENCODE_SERVER_HEALTH_PATH: &str = "/global/health";
const OPENCODE_SERVER_STARTUP_RETRIES: usize = 50;
const OPENCODE_SERVER_STARTUP_DELAY_MS: u64 = 100;
const OPENCODE_SERVER_HEALTH_TIMEOUT_MS: u64 = 250;
const OPENCODE_SERVER_API_TIMEOUT_MS: u64 = 2_000;
const REVIEW_SESSION_SETTLE_POLL_MS: u64 = 500;
const REVIEW_SESSION_SETTLE_IDLE_GRACE_MS: u64 = 1_500;
const REVIEW_SESSION_SETTLE_TIMEOUT_MS: u64 = 600_000;

static SHELL_OUTPUT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReviewOutputKind {
    Review,
    PrDescription,
}

impl ReviewOutputKind {
    fn subject_label(self) -> &'static str {
        match self {
            Self::Review => "Review",
            Self::PrDescription => "PR description",
        }
    }

    pub(super) fn follow_up_command_label(self) -> &'static str {
        match self {
            Self::Review => "review follow-up",
            Self::PrDescription => "pr-description follow-up",
        }
    }

    fn unavailable_session_message(self) -> &'static str {
        match self {
            Self::Review => "Unavailable because this review run did not expose a session ID.",
            Self::PrDescription => {
                "Unavailable because this description run did not expose a session ID."
            }
        }
    }
}

pub(super) struct ReviewOutputState {
    pub(super) thread_id: String,
    pub(super) target: String,
    pub(super) repo_path: String,
    pub(super) output_kind: ReviewOutputKind,
    pub(super) command_label: String,
    pub(super) captured_at: Option<chrono::DateTime<Utc>>,
    pub(super) session_id: Option<String>,
    pub(super) review_settings: ReviewCommandSettings,
    pub(super) follow_up_draft: String,
    pub(super) follow_up_error: Option<String>,
    pub(super) pending_follow_up_prompt: Option<String>,
    terminal: TerminalParser,
    styled_spans: Vec<ReviewStyledSpan>,
    pub(super) open: bool,
    pub(super) status: ReviewStatus,
    pub(super) dropped_chars: usize,
}

pub(super) struct ReviewServer {
    url: String,
    child: Child,
}

pub(super) enum ReviewServerHealth {
    Healthy,
    Unhealthy,
    Exited,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReviewStyledSpan {
    text: String,
    visible_chars: usize,
    style: ReviewTextStyle,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ReviewTextStyle {
    foreground: Option<Color32>,
    bold: bool,
    faint: bool,
    italic: bool,
    underline: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ReviewAnsiMode {
    #[default]
    Text,
    Escape,
    Csi,
    EscapeString,
    EscapeStringTerminator,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ReviewAnsiParserState {
    mode: ReviewAnsiMode,
    active_style: ReviewTextStyle,
    csi_buffer: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReviewStatus {
    Running,
    Cancelled,
    Failed,
}

pub(super) enum ReviewJobMessage {
    ServerReady {
        thread_id: String,
        server: ReviewServer,
    },
    Append {
        thread_id: String,
        bytes: Vec<u8>,
    },
    FinishedSuccess {
        thread_id: String,
        captured_at: chrono::DateTime<Utc>,
        session_id: Option<String>,
    },
    FinishedCancelled {
        thread_id: String,
        captured_at: chrono::DateTime<Utc>,
        session_id: Option<String>,
        _message: String,
    },
    FinishedFailure {
        thread_id: String,
        captured_at: chrono::DateTime<Utc>,
        session_id: Option<String>,
        message: String,
    },
}

pub(super) struct ReviewJob {
    thread_id: String,
    receiver: Receiver<ReviewJobMessage>,
    child: Arc<Mutex<Option<Child>>>,
    cancel_requested: Arc<AtomicBool>,
}

enum ReviewRunOutcome {
    Completed {
        session_id: Option<String>,
    },
    Cancelled {
        message: String,
        session_id: Option<String>,
    },
}

struct ReviewRunFailure {
    message: String,
    session_id: Option<String>,
}

#[derive(Default)]
struct ReviewCommandCapture {
    output: String,
    session_id: Option<String>,
    seen_part_ids: HashSet<String>,
}

enum ReviewShellDestination {
    ControllingTerminal(std::fs::File),
    Stdout,
}

struct ReviewShellMirror {
    destination: ReviewShellDestination,
    review_label: String,
}

struct ReviewRunContext<'a> {
    tx: &'a mpsc::Sender<ReviewJobMessage>,
    thread_id: &'a str,
    review_label: &'a str,
    attach_url: &'a str,
    child_handle: Arc<Mutex<Option<Child>>>,
    cancel_requested: Arc<AtomicBool>,
}

impl Drop for ReviewServer {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) | Err(_) => {}
            Ok(None) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
}

impl ReviewServer {
    pub(super) fn start(
        repo_path: &str,
        review_settings: &ReviewCommandSettings,
        github_token: &str,
    ) -> Result<Self, String> {
        let port = reserve_local_port()?;
        let url = format!("http://{OPENCODE_SERVER_HOST}:{port}");
        let mut command = Command::new("opencode");
        command.current_dir(repo_path);
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
        command.envs(review_command_envs(review_settings, github_token));
        command.arg("serve");
        command.arg("--hostname");
        command.arg(OPENCODE_SERVER_HOST);
        command.arg("--port");
        command.arg(port.to_string());

        let mut child = command
            .spawn()
            .map_err(|err| format!("Failed to start opencode serve: {err}"))?;
        if let Err(err) = wait_for_review_server(&url, &mut child) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }

        Ok(Self { url, child })
    }

    pub(super) fn url(&self) -> &str {
        &self.url
    }

    pub(super) fn health_status(&mut self) -> ReviewServerHealth {
        #[cfg(test)]
        if self.url.starts_with("test://") {
            return ReviewServerHealth::Healthy;
        }

        match self.child.try_wait() {
            Ok(Some(_)) | Err(_) => ReviewServerHealth::Exited,
            Ok(None) => {
                if review_server_healthy(&self.url) {
                    ReviewServerHealth::Healthy
                } else {
                    ReviewServerHealth::Unhealthy
                }
            }
        }
    }
}

#[cfg(test)]
pub(super) fn test_review_server(url: &str) -> ReviewServer {
    let child = Command::new("sh")
        .arg("-c")
        .arg("sleep 60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("test review server child should spawn");

    ReviewServer {
        url: url.to_owned(),
        child,
    }
}

impl ReviewShellMirror {
    fn connect(review_label: impl Into<String>) -> Result<Self, String> {
        let review_label = review_label.into();

        #[cfg(unix)]
        {
            if let Ok(terminal) = OpenOptions::new().write(true).open("/dev/tty") {
                return Ok(Self {
                    destination: ReviewShellDestination::ControllingTerminal(terminal),
                    review_label,
                });
            }
        }

        if std::io::stdout().is_terminal() {
            return Ok(Self {
                destination: ReviewShellDestination::Stdout,
                review_label,
            });
        }

        Err("No active terminal was available for shell streaming.".to_owned())
    }

    fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), String> {
        self.write_raw(chunk)
    }

    fn write_raw(&mut self, bytes: &[u8]) -> Result<(), String> {
        let _guard = shell_output_lock()
            .lock()
            .map_err(|_| "Review shell output lock poisoned unexpectedly.".to_owned())?;

        match &mut self.destination {
            ReviewShellDestination::ControllingTerminal(terminal) => {
                terminal.write_all(bytes).map_err(|err| {
                    format!("Failed to write review output to the terminal: {err}")
                })?;
                terminal
                    .flush()
                    .map_err(|err| format!("Failed to flush review output to the terminal: {err}"))
            }
            ReviewShellDestination::Stdout => {
                let mut stdout = std::io::stdout();
                stdout
                    .write_all(bytes)
                    .map_err(|err| format!("Failed to write review output to stdout: {err}"))?;
                stdout
                    .flush()
                    .map_err(|err| format!("Failed to flush review output to stdout: {err}"))
            }
        }
    }
}

fn shell_output_lock() -> &'static Mutex<()> {
    SHELL_OUTPUT_LOCK.get_or_init(|| Mutex::new(()))
}

fn review_shell_label(launch: &ReviewLaunchPlan) -> String {
    match launch {
        ReviewLaunchPlan::Custom {
            repo, pr_number, ..
        }
        | ReviewLaunchPlan::PrDescription {
            repo, pr_number, ..
        } => format!("{repo}#{pr_number}"),
        ReviewLaunchPlan::FollowUp { target, .. } => target.clone(),
    }
}

fn review_stream_start_banner(review_label: &str) -> Vec<u8> {
    format!("\n[reminder] review stream started for {review_label}\n").into_bytes()
}

fn review_stream_finish_banner(review_label: &str, status: &str) -> Vec<u8> {
    format!("\n[reminder] review stream {status} for {review_label}\n").into_bytes()
}

fn send_review_bytes(
    tx: &mpsc::Sender<ReviewJobMessage>,
    thread_id: &str,
    bytes: impl Into<Vec<u8>>,
) {
    let _ = tx.send(ReviewJobMessage::Append {
        thread_id: thread_id.to_owned(),
        bytes: bytes.into(),
    });
}

fn append_rendered_review_output(
    capture: &mut ReviewCommandCapture,
    shell_mirror: &Arc<Mutex<Option<ReviewShellMirror>>>,
    tx: &mpsc::Sender<ReviewJobMessage>,
    thread_id: &str,
    text: &str,
) {
    if text.is_empty() {
        return;
    }

    capture.output.push_str(text);
    let bytes = text.as_bytes();
    mirror_review_chunk(shell_mirror, tx, thread_id, bytes);
    send_review_bytes(tx, thread_id, bytes.to_vec());
}

fn mirror_review_chunk(
    shell_mirror: &Arc<Mutex<Option<ReviewShellMirror>>>,
    tx: &mpsc::Sender<ReviewJobMessage>,
    thread_id: &str,
    chunk: &[u8],
) {
    let Ok(mut shell_mirror_guard) = shell_mirror.lock() else {
        send_review_bytes(
            tx,
            thread_id,
            b"\n\n[Shell streaming stopped because the shell output lock became unavailable.]\n\n"
                .to_vec(),
        );
        return;
    };
    let Some(shell_mirror) = shell_mirror_guard.as_mut() else {
        return;
    };

    if let Err(err) = shell_mirror.write_chunk(chunk) {
        *shell_mirror_guard = None;
        send_review_bytes(
            tx,
            thread_id,
            format!(
                "\n\n[Shell streaming stopped: {err}. The window will keep rendering the review inline.]\n\n"
            )
            .into_bytes(),
        );
    }
}

fn finish_review_shell_stream(
    shell_mirror: &Arc<Mutex<Option<ReviewShellMirror>>>,
    tx: &mpsc::Sender<ReviewJobMessage>,
    thread_id: &str,
    status: &str,
) {
    let Ok(mut shell_mirror_guard) = shell_mirror.lock() else {
        return;
    };
    let Some(shell_mirror) = shell_mirror_guard.as_mut() else {
        return;
    };

    let finish_banner = review_stream_finish_banner(&shell_mirror.review_label, status);
    if let Err(err) = shell_mirror.write_raw(&finish_banner) {
        *shell_mirror_guard = None;
        send_review_bytes(
            tx,
            thread_id,
            format!(
                "\n\n[Shell streaming stopped while finishing: {err}. The window kept the remaining review output inline.]\n\n"
            )
            .into_bytes(),
        );
    } else {
        send_review_bytes(tx, thread_id, finish_banner);
    }
}

fn strip_ansi_escape_codes(text: &str) -> String {
    #[derive(Clone, Copy)]
    enum AnsiState {
        Text,
        Escape,
        Csi,
        EscapeString,
        EscapeStringTerminator,
    }

    let mut state = AnsiState::Text;
    let mut cleaned = String::with_capacity(text.len());

    for ch in text.chars() {
        state = match (state, ch) {
            (AnsiState::Text, '\u{1b}') => AnsiState::Escape,
            (AnsiState::Text, _) => {
                cleaned.push(ch);
                AnsiState::Text
            }
            (AnsiState::Escape, '[') => AnsiState::Csi,
            (AnsiState::Escape, ']')
            | (AnsiState::Escape, 'P')
            | (AnsiState::Escape, '^')
            | (AnsiState::Escape, '_') => AnsiState::EscapeString,
            (AnsiState::Escape, _) => AnsiState::Text,
            (AnsiState::Csi, '@'..='~') => AnsiState::Text,
            (AnsiState::Csi, _) => AnsiState::Csi,
            (AnsiState::EscapeString, '\u{7}') => AnsiState::Text,
            (AnsiState::EscapeString, '\u{1b}') => AnsiState::EscapeStringTerminator,
            (AnsiState::EscapeString, _) => AnsiState::EscapeString,
            (AnsiState::EscapeStringTerminator, '\\') => AnsiState::Text,
            (AnsiState::EscapeStringTerminator, '\u{1b}') => AnsiState::EscapeStringTerminator,
            (AnsiState::EscapeStringTerminator, _) => AnsiState::EscapeString,
        };
    }

    cleaned
}

fn review_json_string_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str()
}

fn review_event_session_id(event: &Value) -> Option<&str> {
    event
        .get("sessionID")
        .and_then(Value::as_str)
        .or_else(|| review_json_string_at(event, &["part", "sessionID"]))
}

fn review_event_part_id(event: &Value) -> Option<&str> {
    review_json_string_at(event, &["part", "id"])
}

fn format_review_tool_event(part: &Value) -> Option<String> {
    let tool = part.get("tool").and_then(Value::as_str).unwrap_or("tool");
    let title = review_json_string_at(part, &["state", "title"])
        .map(str::trim)
        .filter(|title| !title.is_empty());
    let output = review_json_string_at(part, &["state", "output"])
        .map(str::trim)
        .filter(|output| !output.is_empty());

    match (title, output) {
        (Some(title), Some(output)) => Some(format!("[{tool}] {title}\n{output}\n\n")),
        (Some(title), None) => Some(format!("[{tool}] {title}\n\n")),
        (None, Some(output)) => Some(format!("[{tool}]\n{output}\n\n")),
        (None, None) => None,
    }
}

fn render_review_part(part: &Value) -> Option<String> {
    let part_type = part.get("type")?.as_str()?;

    match part_type {
        "text" => {
            let text = review_json_string_at(part, &["text"])?;
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            Some(format!("{trimmed}\n\n"))
        }
        "tool" => format_review_tool_event(part),
        _ => None,
    }
}

fn render_review_json_event(event: &Value) -> Option<String> {
    let event_type = event.get("type")?.as_str()?;

    match event_type {
        "text" => event.get("part").and_then(render_review_part).or_else(|| {
            let text = review_json_string_at(event, &["part", "text"])?;
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            Some(format!("{trimmed}\n\n"))
        }),
        "tool_use" => event.get("part").and_then(format_review_tool_event),
        "reasoning" | "step_start" | "step_finish" => None,
        "error" => {
            let message = review_json_string_at(event, &["error", "data", "message"])
                .or_else(|| review_json_string_at(event, &["error", "message"]))
                .or_else(|| review_json_string_at(event, &["error", "name"]))?;
            Some(format!("[Error] {}\n\n", message.trim()))
        }
        _ => None,
    }
}

fn review_server_api_client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_millis(OPENCODE_SERVER_API_TIMEOUT_MS))
        .build()
        .map_err(|err| format!("Failed to build opencode server API client: {err}"))
}

fn read_review_server_json(client: &Client, url: &str) -> Result<Value, String> {
    client
        .get(url)
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(|err| format!("Failed to query opencode server at {url}: {err}"))?
        .json::<Value>()
        .map_err(|err| format!("Failed to decode opencode server response from {url}: {err}"))
}

fn review_session_messages_url(attach_url: &str, session_id: &str) -> String {
    format!("{attach_url}/session/{session_id}/message")
}

fn review_session_children_url(attach_url: &str, session_id: &str) -> String {
    format!("{attach_url}/session/{session_id}/children")
}

fn review_session_status_url(attach_url: &str) -> String {
    format!("{attach_url}/session/status")
}

fn collect_review_session_tree_ids(
    client: &Client,
    attach_url: &str,
    session_id: &str,
) -> Result<HashSet<String>, String> {
    let mut pending = vec![session_id.to_owned()];
    let mut session_ids = HashSet::new();

    while let Some(current_id) = pending.pop() {
        if !session_ids.insert(current_id.clone()) {
            continue;
        }

        let response = read_review_server_json(
            client,
            &review_session_children_url(attach_url, &current_id),
        )?;
        let Some(children) = response.as_array() else {
            continue;
        };

        for child in children {
            if let Some(child_id) = child.get("id").and_then(Value::as_str) {
                pending.push(child_id.to_owned());
            }
        }
    }

    Ok(session_ids)
}

fn append_late_review_session_output(
    capture: &mut ReviewCommandCapture,
    shell_mirror: &Arc<Mutex<Option<ReviewShellMirror>>>,
    tx: &mpsc::Sender<ReviewJobMessage>,
    thread_id: &str,
    client: &Client,
    attach_url: &str,
    session_id: &str,
) -> Result<bool, String> {
    let response =
        read_review_server_json(client, &review_session_messages_url(attach_url, session_id))?;
    let Some(messages) = response.as_array() else {
        return Ok(false);
    };

    let mut appended = false;

    for message in messages {
        if message
            .get("info")
            .and_then(|info| info.get("role"))
            .and_then(Value::as_str)
            != Some("assistant")
        {
            continue;
        }

        let Some(parts) = message.get("parts").and_then(Value::as_array) else {
            continue;
        };

        for part in parts {
            let Some(part_id) = part.get("id").and_then(Value::as_str) else {
                continue;
            };
            if !capture.seen_part_ids.insert(part_id.to_owned()) {
                continue;
            }

            if let Some(rendered) = render_review_part(part) {
                append_rendered_review_output(capture, shell_mirror, tx, thread_id, &rendered);
                appended = true;
            }
        }
    }

    Ok(appended)
}

fn review_sessions_all_idle(statuses: &Value, session_ids: &HashSet<String>) -> bool {
    session_ids.iter().all(|session_id| {
        statuses
            .get(session_id)
            .and_then(|status| status.get("type"))
            .and_then(Value::as_str)
            == Some("idle")
    })
}

fn wait_for_review_session_settle(
    capture: &mut ReviewCommandCapture,
    shell_mirror: &Arc<Mutex<Option<ReviewShellMirror>>>,
    tx: &mpsc::Sender<ReviewJobMessage>,
    thread_id: &str,
    attach_url: &str,
    session_id: &str,
    cancel_requested: &Arc<AtomicBool>,
) -> Result<(), String> {
    let client = review_server_api_client()?;
    let deadline = Instant::now() + Duration::from_millis(REVIEW_SESSION_SETTLE_TIMEOUT_MS);
    let mut idle_since: Option<Instant> = None;

    loop {
        if cancel_requested.load(Ordering::SeqCst) {
            return Ok(());
        }

        let appended = append_late_review_session_output(
            capture,
            shell_mirror,
            tx,
            thread_id,
            &client,
            attach_url,
            session_id,
        )?;
        let session_ids = collect_review_session_tree_ids(&client, attach_url, session_id)?;
        let statuses = read_review_server_json(&client, &review_session_status_url(attach_url))?;
        let now = Instant::now();

        if appended {
            idle_since = None;
        } else if review_sessions_all_idle(&statuses, &session_ids) {
            if let Some(started_idle_at) = idle_since {
                if now.duration_since(started_idle_at)
                    >= Duration::from_millis(REVIEW_SESSION_SETTLE_IDLE_GRACE_MS)
                {
                    return Ok(());
                }
            } else {
                idle_since = Some(now);
            }
        } else {
            idle_since = None;
        }

        if now >= deadline {
            append_rendered_review_output(
                capture,
                shell_mirror,
                tx,
                thread_id,
                "\n[reminder] review output is still settling in opencode; more activity may continue there.\n\n",
            );
            return Ok(());
        }

        thread::sleep(Duration::from_millis(REVIEW_SESSION_SETTLE_POLL_MS));
    }
}

fn new_review_terminal() -> TerminalParser {
    TerminalParser::new(REVIEW_TERMINAL_ROWS, REVIEW_TERMINAL_COLS, 0)
}

fn append_text_span(spans: &mut Vec<ReviewStyledSpan>, text: String, style: ReviewTextStyle) {
    if text.is_empty() {
        return;
    }

    let visible_chars = text.chars().count();
    if visible_chars == 0 {
        return;
    }

    if let Some(last_span) = spans.last_mut()
        && last_span.style == style
    {
        last_span.text.push_str(&text);
        last_span.visible_chars += visible_chars;
    } else {
        spans.push(ReviewStyledSpan {
            text,
            visible_chars,
            style,
        });
    }
}

fn apply_sgr_code(style: &mut ReviewTextStyle, code: u16) {
    match code {
        0 => *style = ReviewTextStyle::default(),
        1 => style.bold = true,
        2 => style.faint = true,
        3 => style.italic = true,
        4 => style.underline = true,
        22 => {
            style.bold = false;
            style.faint = false;
        }
        23 => style.italic = false,
        24 => style.underline = false,
        30..=37 => style.foreground = Some(ansi_color_from_4bit(code - 30, false)),
        39 => style.foreground = None,
        90..=97 => style.foreground = Some(ansi_color_from_4bit(code - 90, true)),
        _ => {}
    }
}

fn apply_sgr_sequence(style: &mut ReviewTextStyle, sequence: &str) {
    let sequence = sequence.strip_suffix('m').unwrap_or(sequence);
    if sequence.is_empty() {
        *style = ReviewTextStyle::default();
        return;
    }

    let codes: Vec<_> = sequence
        .split(';')
        .map(|part| {
            if part.is_empty() {
                Some(0)
            } else {
                part.parse::<u16>().ok()
            }
        })
        .collect();
    let mut idx = 0;

    while idx < codes.len() {
        let Some(code) = codes[idx] else {
            idx += 1;
            continue;
        };

        match code {
            38 => {
                if let Some((color, consumed)) = ansi_extended_color(&codes[idx + 1..]) {
                    style.foreground = Some(color);
                    idx += consumed + 1;
                } else {
                    idx += 1;
                }
            }
            39 => {
                style.foreground = None;
                idx += 1;
            }
            48 => {
                if let Some((_, consumed)) = ansi_extended_color(&codes[idx + 1..]) {
                    idx += consumed + 1;
                } else {
                    idx += 1;
                }
            }
            _ => {
                apply_sgr_code(style, code);
                idx += 1;
            }
        }
    }
}

fn ansi_extended_color(codes: &[Option<u16>]) -> Option<(Color32, usize)> {
    match codes {
        [Some(5), Some(code), ..] => Some((ansi_color_from_8bit(*code), 2)),
        [Some(2), Some(r), Some(g), Some(b), ..] => {
            Some((Color32::from_rgb(*r as u8, *g as u8, *b as u8), 4))
        }
        _ => None,
    }
}

fn ansi_color_from_4bit(code: u16, bright: bool) -> Color32 {
    match (code, bright) {
        (0, false) => Color32::from_rgb(28, 28, 28),
        (1, false) => Color32::from_rgb(205, 49, 49),
        (2, false) => Color32::from_rgb(13, 188, 121),
        (3, false) => Color32::from_rgb(229, 229, 16),
        (4, false) => Color32::from_rgb(36, 114, 200),
        (5, false) => Color32::from_rgb(188, 63, 188),
        (6, false) => Color32::from_rgb(17, 168, 205),
        (7, false) => Color32::from_rgb(229, 229, 229),
        (0, true) => Color32::from_rgb(102, 102, 102),
        (1, true) => Color32::from_rgb(241, 76, 76),
        (2, true) => Color32::from_rgb(35, 209, 139),
        (3, true) => Color32::from_rgb(245, 245, 67),
        (4, true) => Color32::from_rgb(59, 142, 234),
        (5, true) => Color32::from_rgb(214, 112, 214),
        (6, true) => Color32::from_rgb(41, 184, 219),
        (7, true) => Color32::from_rgb(255, 255, 255),
        _ => Color32::LIGHT_GRAY,
    }
}

fn ansi_color_from_8bit(code: u16) -> Color32 {
    if code < 8 {
        return ansi_color_from_4bit(code, false);
    }
    if code < 16 {
        return ansi_color_from_4bit(code - 8, true);
    }
    if (16..=231).contains(&code) {
        let cube = code - 16;
        let red = cube / 36;
        let green = (cube % 36) / 6;
        let blue = cube % 6;
        return Color32::from_rgb(
            ansi_cube_component(red),
            ansi_cube_component(green),
            ansi_cube_component(blue),
        );
    }
    if (232..=255).contains(&code) {
        let value = 8 + ((code - 232) as u8 * 10);
        return Color32::from_gray(value);
    }

    Color32::LIGHT_GRAY
}

fn ansi_cube_component(value: u16) -> u8 {
    match value {
        0 => 0,
        1 => 95,
        2 => 135,
        3 => 175,
        4 => 215,
        _ => 255,
    }
}

fn append_ansi_snapshot(
    spans: &mut Vec<ReviewStyledSpan>,
    parser_state: &mut ReviewAnsiParserState,
    snapshot: &str,
) {
    let mut plain_buffer = String::new();

    for ch in snapshot.chars() {
        match parser_state.mode {
            ReviewAnsiMode::Text => {
                if ch == '\u{1b}' {
                    if !plain_buffer.is_empty() {
                        let text = mem::take(&mut plain_buffer);
                        append_text_span(spans, text, parser_state.active_style);
                    }
                    parser_state.mode = ReviewAnsiMode::Escape;
                } else {
                    plain_buffer.push(ch);
                }
            }
            ReviewAnsiMode::Escape => match ch {
                '[' => {
                    parser_state.csi_buffer.clear();
                    parser_state.mode = ReviewAnsiMode::Csi;
                }
                ']' | 'P' | '^' | '_' => {
                    parser_state.mode = ReviewAnsiMode::EscapeString;
                }
                _ => {
                    parser_state.mode = ReviewAnsiMode::Text;
                }
            },
            ReviewAnsiMode::Csi => {
                parser_state.csi_buffer.push(ch);
                if ('@'..='~').contains(&ch) {
                    if ch == 'm' {
                        let sequence = mem::take(&mut parser_state.csi_buffer);
                        apply_sgr_sequence(&mut parser_state.active_style, &sequence);
                    } else {
                        parser_state.csi_buffer.clear();
                    }
                    parser_state.mode = ReviewAnsiMode::Text;
                }
            }
            ReviewAnsiMode::EscapeString => {
                if ch == '\u{7}' {
                    parser_state.mode = ReviewAnsiMode::Text;
                } else if ch == '\u{1b}' {
                    parser_state.mode = ReviewAnsiMode::EscapeStringTerminator;
                }
            }
            ReviewAnsiMode::EscapeStringTerminator => match ch {
                '\\' => parser_state.mode = ReviewAnsiMode::Text,
                '\u{1b}' => {}
                _ => parser_state.mode = ReviewAnsiMode::EscapeString,
            },
        }
    }

    if !plain_buffer.is_empty() {
        append_text_span(spans, plain_buffer, parser_state.active_style);
    }
}

fn rebuild_review_output_from_terminal(review_output: &mut ReviewOutputState) {
    let screen = review_output.terminal.screen();
    let (rows, cols) = screen.size();
    let plain_rows: Vec<_> = screen.rows(0, cols).collect();
    let formatted_rows: Vec<_> = screen.rows_formatted(0, cols).collect();
    let cursor_row = usize::from(screen.cursor_position().0);
    let last_non_empty_row = plain_rows
        .iter()
        .enumerate()
        .rev()
        .find(|(_, row)| !row.is_empty())
        .map(|(idx, _)| idx);

    review_output.styled_spans.clear();
    review_output.dropped_chars = 0;

    let Some(last_row) =
        last_non_empty_row.map_or(Some(cursor_row), |idx| Some(idx.max(cursor_row)))
    else {
        return;
    };

    let mut parser_state = ReviewAnsiParserState::default();
    let last_row = last_row.min(usize::from(rows.saturating_sub(1)));
    for (row_idx, formatted_row) in formatted_rows.iter().enumerate().take(last_row + 1) {
        let formatted_row = String::from_utf8_lossy(formatted_row);
        append_ansi_snapshot(
            &mut review_output.styled_spans,
            &mut parser_state,
            formatted_row.as_ref(),
        );

        if row_idx < last_row && !screen.row_wrapped(row_idx as u16) {
            append_text_span(
                &mut review_output.styled_spans,
                "\n".to_owned(),
                ReviewTextStyle::default(),
            );
        }
    }
}

fn review_text_format(ui: &egui::Ui, style: ReviewTextStyle) -> TextFormat {
    let mut color = style
        .foreground
        .unwrap_or_else(|| ui.visuals().text_color());
    if style.faint {
        color = color.gamma_multiply(0.7);
    }
    if style.bold {
        color = color.gamma_multiply(1.15);
    }

    TextFormat {
        font_id: FontId::monospace(REVIEW_TEXT_SIZE),
        color,
        italics: style.italic,
        underline: if style.underline {
            Stroke::new(1.0, color)
        } else {
            Stroke::NONE
        },
        ..Default::default()
    }
}

fn review_output_layout_job(review_output: &ReviewOutputState, ui: &egui::Ui) -> LayoutJob {
    let mut job = LayoutJob::default();

    for span in &review_output.styled_spans {
        job.append(&span.text, 0.0, review_text_format(ui, span.style));
    }

    job
}

#[cfg(test)]
pub(super) fn review_output_plain_text(review_output: &ReviewOutputState) -> String {
    review_output
        .styled_spans
        .iter()
        .map(|span| span.text.as_str())
        .collect()
}

impl ReviewJob {
    pub(super) fn spawn(
        thread_id: String,
        launch: ReviewLaunchPlan,
        github_token: String,
        attach_url: Option<String>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let worker_thread_id = thread_id.clone();
        let child = Arc::new(Mutex::new(None));
        let cancel_requested = Arc::new(AtomicBool::new(false));
        let worker_child = Arc::clone(&child);
        let worker_cancel_requested = Arc::clone(&cancel_requested);
        thread::spawn(move || {
            let attach_url = match attach_url {
                Some(attach_url) => attach_url,
                None => {
                    let server = match review_server_for_launch(&launch, &github_token) {
                        Ok(server) => server,
                        Err(message) => {
                            let _ = tx.send(ReviewJobMessage::FinishedFailure {
                                thread_id: worker_thread_id.clone(),
                                captured_at: Utc::now(),
                                session_id: None,
                                message,
                            });
                            return;
                        }
                    };

                    if worker_cancel_requested.load(Ordering::SeqCst) {
                        let _ = tx.send(ReviewJobMessage::FinishedCancelled {
                            thread_id: worker_thread_id.clone(),
                            captured_at: Utc::now(),
                            session_id: None,
                            _message: "Review canceled by user.".to_owned(),
                        });
                        return;
                    }

                    let attach_url = server.url().to_owned();
                    if tx
                        .send(ReviewJobMessage::ServerReady {
                            thread_id: worker_thread_id.clone(),
                            server,
                        })
                        .is_err()
                    {
                        return;
                    }
                    attach_url
                }
            };

            let outcome = run_review_stream(
                &tx,
                &worker_thread_id,
                &launch,
                &github_token,
                &attach_url,
                worker_child,
                worker_cancel_requested,
            );
            let message = match outcome {
                Ok(ReviewRunOutcome::Completed { session_id }) => {
                    ReviewJobMessage::FinishedSuccess {
                        thread_id: worker_thread_id.clone(),
                        captured_at: Utc::now(),
                        session_id,
                    }
                }
                Ok(ReviewRunOutcome::Cancelled {
                    message,
                    session_id,
                }) => ReviewJobMessage::FinishedCancelled {
                    thread_id: worker_thread_id.clone(),
                    captured_at: Utc::now(),
                    session_id,
                    _message: message,
                },
                Err(ReviewRunFailure {
                    message,
                    session_id,
                }) => ReviewJobMessage::FinishedFailure {
                    thread_id: worker_thread_id,
                    captured_at: Utc::now(),
                    session_id,
                    message,
                },
            };
            let _ = tx.send(message);
        });
        Self {
            thread_id,
            receiver: rx,
            child,
            cancel_requested,
        }
    }

    pub(super) fn cancel(&self) -> Result<bool, String> {
        self.cancel_requested.store(true, Ordering::SeqCst);
        let mut child = self
            .child
            .lock()
            .map_err(|_| "Review process lock poisoned unexpectedly.".to_owned())?;
        let Some(child) = child.as_mut() else {
            return Ok(true);
        };

        if child
            .try_wait()
            .map_err(|err| format!("Failed to inspect review process state: {err}"))?
            .is_some()
        {
            return Ok(false);
        }

        match child.kill() {
            Ok(()) => Ok(true),
            Err(err) => {
                if child
                    .try_wait()
                    .map_err(|wait_err| {
                        format!("Failed to inspect review process after stop failure: {wait_err}")
                    })?
                    .is_some()
                    || err.kind() == ErrorKind::InvalidInput
                {
                    Ok(false)
                } else {
                    Err(format!("Failed to stop review: {err}"))
                }
            }
        }
    }

    pub(super) fn drain_messages(&mut self) -> (Vec<ReviewJobMessage>, bool) {
        let mut messages = Vec::new();
        let mut finished = false;

        loop {
            match self.receiver.try_recv() {
                Ok(message) => {
                    finished = matches!(
                        message,
                        ReviewJobMessage::FinishedSuccess { .. }
                            | ReviewJobMessage::FinishedCancelled { .. }
                            | ReviewJobMessage::FinishedFailure { .. }
                    );
                    messages.push(message);
                    if finished {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    messages.push(ReviewJobMessage::FinishedFailure {
                        thread_id: self.thread_id.clone(),
                        captured_at: Utc::now(),
                        session_id: None,
                        message: "Review worker disconnected unexpectedly.".to_owned(),
                    });
                    finished = true;
                    break;
                }
            }
        }

        (messages, finished)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ReviewLaunchPlan {
    Custom {
        repo: String,
        repo_path: String,
        pr_number: u64,
        pr_url: String,
        review_settings: ReviewCommandSettings,
    },
    PrDescription {
        repo: String,
        repo_path: String,
        pr_number: u64,
        pr_url: String,
        review_settings: ReviewCommandSettings,
    },
    FollowUp {
        target: String,
        repo_path: String,
        session_id: String,
        prompt: String,
        review_settings: ReviewCommandSettings,
    },
}

pub(super) enum ReviewWindowAction {
    SendFollowUp { thread_id: String },
}

fn run_review_stream(
    tx: &mpsc::Sender<ReviewJobMessage>,
    thread_id: &str,
    launch: &ReviewLaunchPlan,
    github_token: &str,
    attach_url: &str,
    child_handle: Arc<Mutex<Option<Child>>>,
    cancel_requested: Arc<AtomicBool>,
) -> Result<ReviewRunOutcome, ReviewRunFailure> {
    let review_label = review_shell_label(launch);
    let context = ReviewRunContext {
        tx,
        thread_id,
        review_label: &review_label,
        attach_url,
        child_handle,
        cancel_requested,
    };

    match launch {
        ReviewLaunchPlan::Custom {
            repo_path,
            pr_number,
            pr_url,
            review_settings,
            ..
        } => run_custom_review(
            context,
            repo_path,
            *pr_number,
            pr_url,
            review_settings,
            github_token,
        ),
        ReviewLaunchPlan::PrDescription {
            repo_path,
            pr_number,
            pr_url,
            review_settings,
            ..
        } => run_pr_description(
            context,
            repo_path,
            *pr_number,
            pr_url,
            review_settings,
            github_token,
        ),
        ReviewLaunchPlan::FollowUp {
            repo_path,
            session_id,
            prompt,
            review_settings,
            ..
        } => run_review_follow_up(
            context,
            repo_path,
            session_id,
            prompt,
            review_settings,
            github_token,
        ),
    }
}

fn run_custom_review(
    context: ReviewRunContext<'_>,
    repo_path: &str,
    pr_number: u64,
    pr_url: &str,
    review_settings: &ReviewCommandSettings,
    github_token: &str,
) -> Result<ReviewRunOutcome, ReviewRunFailure> {
    let mut command = Command::new("opencode");
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.envs(review_command_envs(review_settings, github_token));
    command.arg("run");
    command.arg("--attach");
    command.arg(context.attach_url);
    command.arg("--dir");
    command.arg(repo_path);
    command.arg("--format");
    command.arg("json");
    command.arg("--command");
    command.arg(CUSTOM_REVIEW_COMMAND_NAME);
    command.arg(custom_command_prompt_message(pr_url, pr_number));
    command.arg("--");
    command.args(&review_settings.additional_args);
    println!("Running custom review command for {}", context.review_label);
    stream_review_command(
        context.tx,
        context.thread_id,
        context.review_label,
        context.attach_url,
        command,
        context.child_handle,
        context.cancel_requested,
    )
}

fn run_pr_description(
    context: ReviewRunContext<'_>,
    repo_path: &str,
    pr_number: u64,
    pr_url: &str,
    review_settings: &ReviewCommandSettings,
    github_token: &str,
) -> Result<ReviewRunOutcome, ReviewRunFailure> {
    let mut command = Command::new("opencode");
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.envs(review_command_envs(review_settings, github_token));
    command.arg("run");
    command.arg("--attach");
    command.arg(context.attach_url);
    command.arg("--dir");
    command.arg(repo_path);
    command.arg("--format");
    command.arg("json");
    command.arg("--command");
    command.arg(CUSTOM_PR_DESCRIPTION_COMMAND_NAME);
    command.arg(custom_command_prompt_message(pr_url, pr_number));
    command.arg("--");
    command.args(&review_settings.additional_args);
    println!(
        "Running PR description command for {}",
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
    )
}

fn run_review_follow_up(
    context: ReviewRunContext<'_>,
    repo_path: &str,
    session_id: &str,
    prompt: &str,
    review_settings: &ReviewCommandSettings,
    github_token: &str,
) -> Result<ReviewRunOutcome, ReviewRunFailure> {
    let mut command = Command::new("opencode");
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.envs(review_command_envs(review_settings, github_token));
    command.arg("run");
    command.arg("--attach");
    command.arg(context.attach_url);
    command.arg("--dir");
    command.arg(repo_path);
    command.arg("--format");
    command.arg("json");
    command.arg("--session");
    command.arg(session_id);
    command.arg("--");
    command.arg(prompt);
    println!(
        "Running review follow-up command for {}",
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
    )
}

fn read_review_stream(
    tx: &mpsc::Sender<ReviewJobMessage>,
    thread_id: &str,
    reader: impl Read,
    shell_mirror: &Arc<Mutex<Option<ReviewShellMirror>>>,
    cancel_requested: &Arc<AtomicBool>,
    stream_label: &str,
) -> Result<String, String> {
    let mut capture = String::new();
    let mut reader = BufReader::new(reader);
    let mut buffer = [0_u8; 4096];

    loop {
        let bytes = match reader.read(&mut buffer) {
            Ok(bytes) => bytes,
            Err(_err) if cancel_requested.load(Ordering::SeqCst) => break,
            Err(err) => return Err(format!("Failed to read {stream_label}: {err}")),
        };

        if bytes == 0 {
            break;
        }

        let chunk = &buffer[..bytes];
        mirror_review_chunk(shell_mirror, tx, thread_id, chunk);
        send_review_bytes(tx, thread_id, chunk.to_vec());
        capture.push_str(&strip_ansi_escape_codes(&String::from_utf8_lossy(chunk)));
    }

    Ok(capture)
}

fn read_review_json_stream(
    tx: &mpsc::Sender<ReviewJobMessage>,
    thread_id: &str,
    reader: impl Read,
    shell_mirror: &Arc<Mutex<Option<ReviewShellMirror>>>,
    cancel_requested: &Arc<AtomicBool>,
    stream_label: &str,
) -> Result<ReviewCommandCapture, String> {
    let mut capture = ReviewCommandCapture::default();
    let mut reader = BufReader::new(reader);
    let mut buffer = [0_u8; 4096];
    let mut pending = String::new();

    loop {
        let bytes = match reader.read(&mut buffer) {
            Ok(bytes) => bytes,
            Err(_err) if cancel_requested.load(Ordering::SeqCst) => break,
            Err(err) => return Err(format!("Failed to read {stream_label}: {err}")),
        };

        if bytes == 0 {
            break;
        }

        pending.push_str(&String::from_utf8_lossy(&buffer[..bytes]));

        while let Some(newline_index) = pending.find('\n') {
            let line: String = pending.drain(..=newline_index).collect();
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let rendered = if let Ok(event) = serde_json::from_str::<Value>(trimmed) {
                if capture.session_id.is_none() {
                    capture.session_id = review_event_session_id(&event).map(str::to_owned);
                }
                if let Some(part_id) = review_event_part_id(&event) {
                    capture.seen_part_ids.insert(part_id.to_owned());
                }
                render_review_json_event(&event)
            } else {
                Some(format!("{trimmed}\n"))
            };
            if let Some(rendered) = rendered {
                append_rendered_review_output(&mut capture, shell_mirror, tx, thread_id, &rendered);
            }
        }
    }

    let trailing = pending.trim();
    if !trailing.is_empty() {
        let rendered = if let Ok(event) = serde_json::from_str::<Value>(trailing) {
            if capture.session_id.is_none() {
                capture.session_id = review_event_session_id(&event).map(str::to_owned);
            }
            if let Some(part_id) = review_event_part_id(&event) {
                capture.seen_part_ids.insert(part_id.to_owned());
            }
            render_review_json_event(&event)
        } else {
            Some(format!("{trailing}\n"))
        };
        if let Some(rendered) = rendered {
            append_rendered_review_output(&mut capture, shell_mirror, tx, thread_id, &rendered);
        }
    }

    Ok(capture)
}

fn stream_review_command(
    tx: &mpsc::Sender<ReviewJobMessage>,
    thread_id: &str,
    review_label: &str,
    attach_url: &str,
    mut command: Command,
    child_handle: Arc<Mutex<Option<Child>>>,
    cancel_requested: Arc<AtomicBool>,
) -> Result<ReviewRunOutcome, ReviewRunFailure> {
    let mut child = command.spawn().map_err(|err| ReviewRunFailure {
        message: format!("Failed to start review: {err}"),
        session_id: None,
    })?;
    let stdout = child.stdout.take().ok_or_else(|| ReviewRunFailure {
        message: "Failed to capture stdout.".to_owned(),
        session_id: None,
    })?;
    let stderr = child.stderr.take().ok_or_else(|| ReviewRunFailure {
        message: "Failed to capture stderr.".to_owned(),
        session_id: None,
    })?;

    let shell_mirror = match ReviewShellMirror::connect(review_label.to_owned()) {
        Ok(mut shell_mirror) => {
            let start_banner = review_stream_start_banner(review_label);
            if let Err(err) = shell_mirror.write_raw(&start_banner) {
                send_review_bytes(
                    tx,
                    thread_id,
                    format!("{SHELL_STREAM_DISABLED_NOTE} {err}\n\n").into_bytes(),
                );
                Arc::new(Mutex::new(None))
            } else {
                send_review_bytes(tx, thread_id, start_banner);
                Arc::new(Mutex::new(Some(shell_mirror)))
            }
        }
        Err(err) => {
            send_review_bytes(
                tx,
                thread_id,
                format!("{SHELL_STREAM_DISABLED_NOTE} {err}\n\n").into_bytes(),
            );
            Arc::new(Mutex::new(None))
        }
    };

    {
        let mut shared_child = child_handle.lock().map_err(|_| ReviewRunFailure {
            message: "Review process lock poisoned unexpectedly.".to_owned(),
            session_id: None,
        })?;
        *shared_child = Some(child);
        if cancel_requested.load(Ordering::SeqCst)
            && let Some(child) = shared_child.as_mut()
        {
            match child.kill() {
                Ok(()) => {}
                Err(err)
                    if err.kind() == ErrorKind::InvalidInput
                        || child
                            .try_wait()
                            .map_err(|wait_err| {
                                ReviewRunFailure {
                                    message: format!(
                                        "Failed to inspect review process after early stop failure: {wait_err}"
                                    ),
                                    session_id: None,
                                }
                    })?
                            .is_some() => {}
                Err(err) => {
                    return Err(ReviewRunFailure {
                        message: format!("Failed to stop review: {err}"),
                        session_id: None,
                    });
                }
            }
        }
    }

    let stderr_cancel_requested = Arc::clone(&cancel_requested);
    let stderr_shell_mirror = Arc::clone(&shell_mirror);
    let stderr_tx = tx.clone();
    let stderr_thread_id = thread_id.to_owned();
    let stderr_handle = thread::spawn(move || -> Result<String, String> {
        read_review_stream(
            &stderr_tx,
            &stderr_thread_id,
            stderr,
            &stderr_shell_mirror,
            &stderr_cancel_requested,
            "diagnostics",
        )
    });

    let mut stdout_capture = read_review_json_stream(
        tx,
        thread_id,
        stdout,
        &shell_mirror,
        &cancel_requested,
        "output",
    )
    .map_err(|message| ReviewRunFailure {
        message,
        session_id: None,
    })?;

    let status = {
        let mut shared_child = child_handle.lock().map_err(|_| ReviewRunFailure {
            message: "Review process lock poisoned unexpectedly.".to_owned(),
            session_id: stdout_capture.session_id.clone(),
        })?;
        let mut child = shared_child.take().ok_or_else(|| ReviewRunFailure {
            message: "Review process handle missing while waiting.".to_owned(),
            session_id: stdout_capture.session_id.clone(),
        })?;
        match child.wait() {
            Ok(status) => status,
            Err(_err) if cancel_requested.load(Ordering::SeqCst) => {
                return Ok(ReviewRunOutcome::Cancelled {
                    message: "Review canceled by user.".to_owned(),
                    session_id: stdout_capture.session_id.clone(),
                });
            }
            Err(err) => {
                return Err(ReviewRunFailure {
                    message: format!("Failed while waiting for review: {err}"),
                    session_id: stdout_capture.session_id.clone(),
                });
            }
        }
    };
    let stderr_capture = match stderr_handle.join() {
        Ok(Ok(stderr_capture)) => stderr_capture,
        Ok(Err(err)) => {
            if cancel_requested.load(Ordering::SeqCst) {
                String::new()
            } else {
                return Err(ReviewRunFailure {
                    message: err,
                    session_id: stdout_capture.session_id.clone(),
                });
            }
        }
        Err(_) => {
            if cancel_requested.load(Ordering::SeqCst) {
                String::new()
            } else {
                return Err(ReviewRunFailure {
                    message: "Failed to join opencode diagnostics reader.".to_owned(),
                    session_id: stdout_capture.session_id.clone(),
                });
            }
        }
    };

    if cancel_requested.load(Ordering::SeqCst) && !status.success() {
        finish_review_shell_stream(&shell_mirror, tx, thread_id, "cancelled");
        return Ok(ReviewRunOutcome::Cancelled {
            message: "Review canceled by user.".to_owned(),
            session_id: stdout_capture.session_id.clone(),
        });
    }

    if status.success() {
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
        finish_review_shell_stream(&shell_mirror, tx, thread_id, "session ready");
        return Ok(ReviewRunOutcome::Completed {
            session_id: stdout_capture.session_id,
        });
    }

    finish_review_shell_stream(&shell_mirror, tx, thread_id, "failed");

    Err(ReviewRunFailure {
        message: format_review_failure_output(
            &status.to_string(),
            &stdout_capture.output,
            &stderr_capture,
        ),
        session_id: stdout_capture.session_id,
    })
}

#[cfg(test)]
pub(super) fn format_review_success_output(stdout: &str, stderr: &str) -> String {
    let stdout = stdout.trim();
    let stderr = stderr.trim();

    match (stdout.is_empty(), stderr.is_empty()) {
        (false, true) => stdout.to_owned(),
        (false, false) => format!("{stdout}\n\nDiagnostics:\n{stderr}"),
        (true, false) => format!("Diagnostics:\n{stderr}"),
        (true, true) => "opencode review session ended with no output.".to_owned(),
    }
}

pub(super) fn format_review_failure_output(status: &str, stdout: &str, stderr: &str) -> String {
    let stdout = stdout.trim();
    let stderr = stderr.trim();

    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => format!("opencode review exited unsuccessfully: {status}"),
        (false, true) => format!("opencode review failed after partial output: {status}"),
        (true, false) => format!("opencode review failed:\n{stderr}"),
        (false, false) => format!("opencode review failed after partial output. stderr:\n{stderr}"),
    }
}

#[cfg(test)]
pub(super) fn truncate_review_output(text: &str) -> String {
    let char_count = text.chars().count();
    if char_count <= MAX_REVIEW_OUTPUT_CHARS {
        return text.to_owned();
    }

    let truncated: String = text.chars().take(MAX_REVIEW_OUTPUT_CHARS).collect();
    format!(
        "{truncated}\n\n[truncated {} trailing characters]",
        char_count - MAX_REVIEW_OUTPUT_CHARS
    )
}

pub(super) fn initial_review_output_state(
    thread_id: String,
    launch: &ReviewLaunchPlan,
) -> ReviewOutputState {
    match launch {
        ReviewLaunchPlan::Custom {
            repo,
            repo_path,
            pr_number,
            review_settings,
            ..
        } => ReviewOutputState {
            thread_id,
            target: format!("{repo}#{pr_number}"),
            repo_path: repo_path.clone(),
            output_kind: ReviewOutputKind::Review,
            command_label: String::from(CUSTOM_REVIEW_COMMAND_NAME),
            captured_at: None,
            session_id: None,
            review_settings: review_settings.clone(),
            follow_up_draft: String::new(),
            follow_up_error: None,
            pending_follow_up_prompt: None,
            terminal: new_review_terminal(),
            styled_spans: Vec::new(),
            open: true,
            status: ReviewStatus::Running,
            dropped_chars: 0,
        },
        ReviewLaunchPlan::PrDescription {
            repo,
            repo_path,
            pr_number,
            review_settings,
            ..
        } => ReviewOutputState {
            thread_id,
            target: format!("{repo}#{pr_number}"),
            repo_path: repo_path.clone(),
            output_kind: ReviewOutputKind::PrDescription,
            command_label: String::from(CUSTOM_PR_DESCRIPTION_COMMAND_NAME),
            captured_at: None,
            session_id: None,
            review_settings: review_settings.clone(),
            follow_up_draft: String::new(),
            follow_up_error: None,
            pending_follow_up_prompt: None,
            terminal: new_review_terminal(),
            styled_spans: Vec::new(),
            open: true,
            status: ReviewStatus::Running,
            dropped_chars: 0,
        },
        ReviewLaunchPlan::FollowUp { .. } => {
            unreachable!("follow-up launches reuse the existing review state")
        }
    }
}

pub(super) fn append_review_chunk(review_output: &mut ReviewOutputState, chunk: impl AsRef<[u8]>) {
    review_output.terminal.process(chunk.as_ref());
    rebuild_review_output_from_terminal(review_output);
}

pub(super) fn review_process_active(review_output: &ReviewOutputState) -> bool {
    review_output.status == ReviewStatus::Running && review_output.captured_at.is_none()
}

pub(super) fn review_session_ready(review_output: &ReviewOutputState) -> bool {
    review_output.status == ReviewStatus::Running
        && review_output.captured_at.is_some()
        && review_output.session_id.is_some()
}

pub(super) fn review_summary_text(review_output: &ReviewOutputState) -> String {
    let status = match review_output.status {
        ReviewStatus::Running if review_process_active(review_output) => {
            format!("{} running", review_output.output_kind.subject_label())
        }
        ReviewStatus::Running if review_output.session_id.is_some() => {
            format!(
                "{} session active",
                review_output.output_kind.subject_label()
            )
        }
        ReviewStatus::Running => {
            format!("{} output ready", review_output.output_kind.subject_label())
        }
        ReviewStatus::Cancelled => {
            format!("{} canceled", review_output.output_kind.subject_label())
        }
        ReviewStatus::Failed => format!("{} failed", review_output.output_kind.subject_label()),
    };

    match review_output.captured_at {
        Some(captured_at) => format!(
            "{status}: {} via {} at {}",
            review_output.target,
            review_output.command_label,
            format_local_timestamp(captured_at, "%Y-%m-%d %H:%M:%S %:z")
        ),
        None => format!(
            "{status}: {} via {}",
            review_output.target, review_output.command_label
        ),
    }
}

pub(super) fn append_review_follow_up_prompt(review_output: &mut ReviewOutputState, prompt: &str) {
    let formatted = format!("\n\n[Follow-up]\n{}\n\n", prompt.trim());
    append_review_chunk(review_output, formatted);
}

pub(super) fn render_review_window(
    ctx: &Context,
    account_login: &str,
    review_output: &mut ReviewOutputState,
) -> Option<ReviewWindowAction> {
    if !review_output.open {
        return None;
    }

    let title = match review_output.status {
        ReviewStatus::Running if review_process_active(review_output) => format!(
            "{} in progress: {}",
            review_output.output_kind.subject_label(),
            review_output.target
        ),
        ReviewStatus::Running if review_output.session_id.is_some() => format!(
            "{} session active: {}",
            review_output.output_kind.subject_label(),
            review_output.target
        ),
        ReviewStatus::Running => format!(
            "{} output ready: {}",
            review_output.output_kind.subject_label(),
            review_output.target
        ),
        ReviewStatus::Cancelled => format!(
            "{} canceled: {}",
            review_output.output_kind.subject_label(),
            review_output.target
        ),
        ReviewStatus::Failed => format!(
            "{} failed: {}",
            review_output.output_kind.subject_label(),
            review_output.target
        ),
    };
    let mut open = review_output.open;
    let status_line = review_summary_text(review_output);
    let can_send_follow_up = review_session_ready(review_output);
    let mut send_follow_up = false;

    egui::Window::new(title)
        .id(egui::Id::new((
            "review-window",
            account_login,
            &review_output.thread_id,
        )))
        .open(&mut open)
        .collapsible(true)
        .resizable(true)
        .default_size(egui::vec2(720.0, 420.0))
        .show(ctx, |ui| {
            ui.small(status_line);
            if review_output.dropped_chars > 0 {
                ui.small(format!(
                    "[trimmed {} leading characters]",
                    review_output.dropped_chars
                ));
            }

            let composer_reserved_height = if review_output.follow_up_error.is_some() {
                164.0
            } else {
                136.0
            };
            let output_max_height =
                (ui.available_height() - composer_reserved_height).clamp(120.0, 420.0);

            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .max_height(output_max_height)
                .stick_to_bottom(review_process_active(review_output))
                .show(ui, |scroll| {
                    let content_job = review_output_layout_job(review_output, scroll);
                    scroll.add(egui::Label::new(content_job).extend().selectable(true));
                });

            ui.separator();
            ui.add_space(8.0);

            if review_output.session_id.is_none() {
                ui.small(review_output.output_kind.unavailable_session_message());
            }

            let draft_is_empty = review_output.follow_up_draft.trim().is_empty();
            ui.add_enabled_ui(can_send_follow_up, |ui| {
                let mut enter_pressed = false;
                let mut send_clicked = false;

                ui.horizontal(|row| {
                    let send_button_width = 112.0;
                    let input_width = (row.available_width() - send_button_width - 8.0).max(160.0);
                    let response = row.add_sized(
                        [input_width, 0.0],
                        egui::TextEdit::singleline(&mut review_output.follow_up_draft)
                            .hint_text("Ask a follow-up"),
                    );
                    if response.changed() {
                        review_output.follow_up_error = None;
                    }

                    enter_pressed = response.lost_focus()
                        && row.input(|input| input.key_pressed(egui::Key::Enter));
                    send_clicked = row
                        .add_enabled(!draft_is_empty, egui::Button::new("Send message"))
                        .clicked();
                });

                if send_clicked || (enter_pressed && !draft_is_empty) {
                    send_follow_up = true;
                }
            });

            if let Some(error) = &review_output.follow_up_error {
                ui.add_space(8.0);
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
        });

    review_output.open = open;

    if send_follow_up {
        Some(ReviewWindowAction::SendFollowUp {
            thread_id: review_output.thread_id.clone(),
        })
    } else {
        None
    }
}

pub(super) fn custom_review_available_for_repo(
    repo_paths: &BTreeMap<String, String>,
    custom_review_command: bool,
    repo: &str,
) -> bool {
    custom_review_command
        && canonical_repo_key(repo)
            .as_ref()
            .is_some_and(|repo_key| repo_paths.contains_key(repo_key))
}

pub(super) fn resolve_review_launch(
    repo_paths: &BTreeMap<String, String>,
    custom_review_command: bool,
    repo: &str,
    pr_number: u64,
    review_settings: &ReviewCommandSettings,
    pr_url: &str,
) -> Option<ReviewLaunchPlan> {
    if !custom_review_available_for_repo(repo_paths, custom_review_command, repo) {
        return None;
    }

    let repo_key = canonical_repo_key(repo).expect("custom review availability checked");
    let repo_path = repo_paths
        .get(&repo_key)
        .expect("custom review availability checked");

    Some(ReviewLaunchPlan::Custom {
        repo: repo.to_owned(),
        repo_path: repo_path.clone(),
        pr_number,
        pr_url: pr_url.to_owned(),
        review_settings: review_settings.clone(),
    })
}

pub(super) fn resolve_pr_description_launch(
    repo_paths: &BTreeMap<String, String>,
    pr_description_available: bool,
    repo: &str,
    pr_number: u64,
    review_settings: &ReviewCommandSettings,
    pr_url: &str,
) -> Option<ReviewLaunchPlan> {
    if !custom_review_available_for_repo(repo_paths, pr_description_available, repo) {
        return None;
    }

    let repo_key = canonical_repo_key(repo).expect("custom review availability checked");
    let repo_path = repo_paths
        .get(&repo_key)
        .expect("custom review availability checked");

    Some(ReviewLaunchPlan::PrDescription {
        repo: repo.to_owned(),
        repo_path: repo_path.clone(),
        pr_number,
        pr_url: pr_url.to_owned(),
        review_settings: review_settings.clone(),
    })
}

fn custom_command_prompt_message(pr_url: &str, pr_number: u64) -> String {
    format!("PR URL: {pr_url}\nPR number: {pr_number}")
}

fn review_command_envs(
    review_settings: &ReviewCommandSettings,
    github_token: &str,
) -> Vec<(String, String)> {
    let mut envs: Vec<_> = review_settings
        .env_vars
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();

    if github_token.trim().is_empty() {
        return envs;
    }

    if !review_settings_has_env_var(review_settings, GH_TOKEN_ENV_VAR) {
        envs.push((String::from(GH_TOKEN_ENV_VAR), github_token.to_owned()));
    }
    if !review_settings_has_env_var(review_settings, GITHUB_TOKEN_ENV_VAR) {
        envs.push((String::from(GITHUB_TOKEN_ENV_VAR), github_token.to_owned()));
    }

    envs
}

fn review_settings_has_env_var(review_settings: &ReviewCommandSettings, key: &str) -> bool {
    review_settings
        .env_vars
        .keys()
        .any(|candidate| candidate.eq_ignore_ascii_case(key))
}

pub(super) fn review_launch_repo_path_and_settings(
    launch: &ReviewLaunchPlan,
) -> Option<(&str, &ReviewCommandSettings)> {
    match launch {
        ReviewLaunchPlan::Custom {
            repo_path,
            review_settings,
            ..
        }
        | ReviewLaunchPlan::PrDescription {
            repo_path,
            review_settings,
            ..
        } => Some((repo_path.as_str(), review_settings)),
        ReviewLaunchPlan::FollowUp { .. } => None,
    }
}

fn review_server_for_launch(
    launch: &ReviewLaunchPlan,
    github_token: &str,
) -> Result<ReviewServer, String> {
    let Some((repo_path, review_settings)) = review_launch_repo_path_and_settings(launch) else {
        return Err("Unexpected follow-up launch while starting a new review backend.".to_owned());
    };

    ReviewServer::start(repo_path, review_settings, github_token)
}

fn reserve_local_port() -> Result<u16, String> {
    let listener = TcpListener::bind((OPENCODE_SERVER_HOST, 0))
        .map_err(|err| format!("Failed to reserve a local opencode server port: {err}"))?;
    listener
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|err| format!("Failed to inspect reserved opencode server port: {err}"))
}

fn review_server_health_client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_millis(OPENCODE_SERVER_HEALTH_TIMEOUT_MS))
        .build()
        .map_err(|err| format!("Failed to build opencode server health client: {err}"))
}

fn review_server_healthy(url: &str) -> bool {
    let Ok(client) = review_server_health_client() else {
        return false;
    };

    client
        .get(format!("{url}{OPENCODE_SERVER_HEALTH_PATH}"))
        .send()
        .map(|response| response.status().is_success())
        .unwrap_or(false)
}

fn wait_for_review_server(url: &str, child: &mut Child) -> Result<(), String> {
    for _ in 0..OPENCODE_SERVER_STARTUP_RETRIES {
        if review_server_healthy(url) {
            return Ok(());
        }

        if let Some(status) = child
            .try_wait()
            .map_err(|err| format!("Failed to inspect opencode serve startup: {err}"))?
        {
            return Err(format!(
                "opencode serve exited before it became healthy: {status}"
            ));
        }

        thread::sleep(Duration::from_millis(OPENCODE_SERVER_STARTUP_DELAY_MS));
    }

    Err(format!(
        "Timed out waiting for opencode serve at {url} to become healthy."
    ))
}

pub(super) fn custom_review_command_available() -> bool {
    default_review_prompt_md_path().is_some_and(|path| path.exists())
}

pub(super) fn review_prompt_command_available(review_settings: &ReviewCommandSettings) -> bool {
    review_prompt_md_path(review_settings).is_some_and(|path| path.exists())
}

pub(super) fn pr_description_command_available(review_settings: &ReviewCommandSettings) -> bool {
    pr_description_prompt_md_path(review_settings).is_some_and(|path| path.exists())
}

pub(super) fn default_review_prompt_md_path_display() -> String {
    default_review_prompt_md_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "Unavailable (set HOME or XDG_CONFIG_HOME)".to_owned())
}

pub(super) fn default_pr_description_prompt_md_path_display() -> String {
    default_pr_description_prompt_md_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "Unavailable (set HOME or XDG_CONFIG_HOME)".to_owned())
}

fn review_prompt_md_path(review_settings: &ReviewCommandSettings) -> Option<PathBuf> {
    configured_prompt_md_path(
        review_settings.review_prompt_md_path.as_deref(),
        default_review_prompt_md_path,
    )
}

fn pr_description_prompt_md_path(review_settings: &ReviewCommandSettings) -> Option<PathBuf> {
    configured_prompt_md_path(
        review_settings.pr_description_md_path.as_deref(),
        default_pr_description_prompt_md_path,
    )
}

fn configured_prompt_md_path(
    configured_path: Option<&str>,
    default_path: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    configured_path
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(default_path)
}

fn default_review_prompt_md_path() -> Option<PathBuf> {
    if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(config_home).join("opencode/commands/review-pr.md"));
    }

    env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".config/opencode/commands/review-pr.md"))
}

fn default_pr_description_prompt_md_path() -> Option<PathBuf> {
    if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(config_home).join("opencode/commands/pr-description.md"));
    }

    env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".config/opencode/commands/pr-description.md"))
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use crate::domain::ReviewCommandSettings;
    use chrono::Utc;
    use serde_json::json;

    use super::{
        ReviewLaunchPlan, ansi_color_from_4bit, append_review_chunk,
        append_review_follow_up_prompt, custom_command_prompt_message, initial_review_output_state,
        pr_description_prompt_md_path, render_review_json_event, render_review_part,
        review_command_envs, review_event_session_id, review_output_plain_text,
        review_process_active, review_prompt_md_path, review_session_ready,
        strip_ansi_escape_codes,
    };

    fn review_state() -> super::ReviewOutputState {
        initial_review_output_state(
            String::from("thread-1"),
            &ReviewLaunchPlan::Custom {
                repo: String::from("acme/repo"),
                repo_path: String::from("/tmp/acme-repo"),
                pr_number: 123,
                pr_url: String::from("https://github.com/acme/repo/pull/123"),
                review_settings: ReviewCommandSettings::default(),
            },
        )
    }

    #[test]
    fn strip_ansi_escape_codes_removes_color_sequences() {
        let raw = "\u{1b}[32mreview ready\u{1b}[0m\n";

        assert_eq!(strip_ansi_escape_codes(raw), "review ready\n");
    }

    #[test]
    fn strip_ansi_escape_codes_removes_osc_hyperlinks() {
        let raw = "\u{1b}]8;;https://example.com\u{7}open pr\u{1b}]8;;\u{7}";

        assert_eq!(strip_ansi_escape_codes(raw), "open pr");
    }

    #[test]
    fn append_review_chunk_preserves_basic_ansi_styles_for_egui() {
        let mut review_output = review_state();

        append_review_chunk(&mut review_output, "\u{1b}[31mred\u{1b}[0m normal");

        assert_eq!(review_output_plain_text(&review_output), "red normal");
        assert_eq!(review_output.styled_spans.len(), 2);
        assert_eq!(
            review_output.styled_spans[0].style.foreground,
            Some(ansi_color_from_4bit(1, false))
        );
        assert_eq!(review_output.styled_spans[1].style.foreground, None);
    }

    #[test]
    fn append_review_chunk_handles_split_escape_sequences() {
        let mut review_output = review_state();

        append_review_chunk(&mut review_output, "\u{1b}[31");
        append_review_chunk(&mut review_output, "mred");

        assert_eq!(review_output_plain_text(&review_output), "red");
        assert_eq!(
            review_output.styled_spans[0].style.foreground,
            Some(ansi_color_from_4bit(1, false))
        );
    }

    #[test]
    fn review_prompt_path_prefers_configured_override() {
        let review_settings = ReviewCommandSettings {
            review_prompt_md_path: Some(String::from("/tmp/custom/review-pr.md")),
            ..ReviewCommandSettings::default()
        };

        assert_eq!(
            review_prompt_md_path(&review_settings),
            Some(PathBuf::from("/tmp/custom/review-pr.md"))
        );
    }

    #[test]
    fn pr_description_prompt_path_prefers_configured_override() {
        let review_settings = ReviewCommandSettings {
            pr_description_md_path: Some(String::from("/tmp/custom/pr-description.md")),
            ..ReviewCommandSettings::default()
        };

        assert_eq!(
            pr_description_prompt_md_path(&review_settings),
            Some(PathBuf::from("/tmp/custom/pr-description.md"))
        );
    }

    #[test]
    fn review_command_envs_inject_account_token_when_missing() {
        let review_settings = ReviewCommandSettings {
            env_vars: BTreeMap::from([(String::from("FOO"), String::from("bar"))]),
            ..ReviewCommandSettings::default()
        };

        let envs = review_command_envs(&review_settings, "selected-token");

        assert!(envs.contains(&(String::from("FOO"), String::from("bar"))));
        assert!(envs.contains(&(String::from("GH_TOKEN"), String::from("selected-token"))));
        assert!(envs.contains(&(String::from("GITHUB_TOKEN"), String::from("selected-token"))));
    }

    #[test]
    fn review_command_envs_preserve_explicit_github_auth_envs() {
        let review_settings = ReviewCommandSettings {
            env_vars: BTreeMap::from([
                (String::from("gh_token"), String::from("explicit-gh")),
                (
                    String::from("GITHUB_TOKEN"),
                    String::from("explicit-github"),
                ),
            ]),
            ..ReviewCommandSettings::default()
        };

        let envs = review_command_envs(&review_settings, "selected-token");

        assert!(envs.contains(&(String::from("gh_token"), String::from("explicit-gh"))));
        assert!(envs.contains(&(
            String::from("GITHUB_TOKEN"),
            String::from("explicit-github")
        )));
        assert!(!envs.contains(&(String::from("GH_TOKEN"), String::from("selected-token"))));
        assert!(!envs.contains(&(String::from("GITHUB_TOKEN"), String::from("selected-token"))));
    }

    #[test]
    fn custom_command_prompt_message_includes_pr_url() {
        let message = custom_command_prompt_message("https://github.com/acme/repo/pull/123", 123);

        assert!(message.contains("PR URL: https://github.com/acme/repo/pull/123"));
        assert!(message.contains("PR number: 123"));
    }

    #[test]
    fn append_review_chunk_overwrites_from_line_start_on_carriage_return() {
        let mut review_output = review_state();

        append_review_chunk(&mut review_output, "[Pasted ~2 lines]\rnext line");

        assert_eq!(
            review_output_plain_text(&review_output),
            "next line2 lines]"
        );
    }

    #[test]
    fn append_review_chunk_preserves_crlf_newlines() {
        let mut review_output = review_state();

        append_review_chunk(&mut review_output, "first line\r\nsecond line");

        assert_eq!(
            review_output_plain_text(&review_output),
            "first line\nsecond line"
        );
    }

    #[test]
    fn append_review_chunk_handles_split_carriage_return_rewrites() {
        let mut review_output = review_state();

        append_review_chunk(&mut review_output, "[Pasted ~2 lines]\r");
        append_review_chunk(&mut review_output, "next line");

        assert_eq!(
            review_output_plain_text(&review_output),
            "next line2 lines]"
        );
    }

    #[test]
    fn append_review_chunk_handles_clear_line_rewrite_sequences() {
        let mut review_output = review_state();

        append_review_chunk(&mut review_output, "temporary status\r\u{1b}[2Kreal output");

        assert_eq!(review_output_plain_text(&review_output), "real output");
    }

    #[test]
    fn render_review_json_event_renders_text_without_marking_completion() {
        let event = json!({
            "type": "text",
            "part": {
                "text": "Review posted successfully"
            }
        });

        let rendered = render_review_json_event(&event);

        assert_eq!(rendered.as_deref(), Some("Review posted successfully\n\n"));
    }

    #[test]
    fn render_review_json_event_formats_tool_output_without_marking_completion() {
        let event = json!({
            "type": "tool_use",
            "part": {
                "tool": "task",
                "state": {
                    "title": "Explore repo",
                    "output": "task_id: child-session"
                }
            }
        });

        let rendered = render_review_json_event(&event);

        assert_eq!(
            rendered.as_deref(),
            Some("[task] Explore repo\ntask_id: child-session\n\n")
        );
    }

    #[test]
    fn render_review_json_event_ignores_step_finish_markers() {
        let event = json!({
            "type": "step_finish",
            "part": {
                "reason": "stop",
                "snapshot": "abc123"
            }
        });

        assert!(render_review_json_event(&event).is_none());
    }

    #[test]
    fn render_review_part_formats_session_tool_parts() {
        let part = json!({
            "id": "prt_123",
            "type": "tool",
            "tool": "task",
            "state": {
                "status": "completed",
                "title": "Review summary",
                "output": "Posted GitHub review comment"
            }
        });

        assert_eq!(
            render_review_part(&part).as_deref(),
            Some("[task] Review summary\nPosted GitHub review comment\n\n")
        );
    }

    #[test]
    fn review_event_session_id_reads_top_level_value() {
        let event = json!({
            "type": "step_start",
            "sessionID": "ses_top_level",
            "part": {
                "sessionID": "ses_nested"
            }
        });

        assert_eq!(review_event_session_id(&event), Some("ses_top_level"));
    }

    #[test]
    fn append_review_follow_up_prompt_adds_visible_separator() {
        let mut review_output = review_state();

        append_review_follow_up_prompt(&mut review_output, "Explain the main blocker");

        assert!(review_output_plain_text(&review_output).contains("[Follow-up]"));
        assert!(review_output_plain_text(&review_output).contains("Explain the main blocker"));
    }

    #[test]
    fn review_summary_text_reports_session_active_status() {
        let mut review_output = review_state();
        review_output.captured_at = Some(Utc::now());
        review_output.session_id = Some(String::from("ses_123"));

        assert!(super::review_summary_text(&review_output).starts_with("Review session active:"));
        assert!(review_session_ready(&review_output));
    }

    #[test]
    fn review_summary_text_reports_output_ready_without_session_id() {
        let mut review_output = review_state();
        review_output.captured_at = Some(Utc::now());

        assert!(super::review_summary_text(&review_output).starts_with("Review output ready:"));
        assert!(!review_session_ready(&review_output));
    }

    #[test]
    fn review_process_active_requires_running_state_without_completion_time() {
        let mut review_output = review_state();

        assert!(review_process_active(&review_output));

        review_output.captured_at = Some(Utc::now());

        assert!(!review_process_active(&review_output));
    }
}
