use std::{
    collections::BTreeMap,
    env,
    fs::OpenOptions,
    io::{BufReader, ErrorKind, IsTerminal, Read, Write},
    mem,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
};

use chrono::Utc;
use eframe::egui::{
    self, Color32, Context, FontId, Stroke,
    text::{LayoutJob, TextFormat},
};
use serde_json::Value;
use vt100::Parser as TerminalParser;

#[cfg(test)]
use super::MAX_REVIEW_OUTPUT_CHARS;
use super::time::format_local_timestamp;
use super::{CUSTOM_REVIEW_COMMAND_NAME, repo_paths::canonical_repo_key};
use crate::domain::ReviewCommandSettings;

const SHELL_STREAM_DISABLED_NOTE: &str =
    "[Shell streaming unavailable. This window still renders common ANSI styles inline.]";
const REVIEW_TEXT_SIZE: f32 = 13.0;
const REVIEW_TERMINAL_ROWS: u16 = 1000;
const REVIEW_TERMINAL_COLS: u16 = 240;

static SHELL_OUTPUT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub(super) struct ReviewOutputState {
    pub(super) thread_id: String,
    pub(super) target: String,
    pub(super) command_label: String,
    pub(super) captured_at: Option<chrono::DateTime<Utc>>,
    terminal: TerminalParser,
    styled_spans: Vec<ReviewStyledSpan>,
    pub(super) open: bool,
    pub(super) status: ReviewStatus,
    pub(super) dropped_chars: usize,
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
    Completed,
    Cancelled,
    Failed,
}

pub(super) enum ReviewJobMessage {
    Append {
        thread_id: String,
        bytes: Vec<u8>,
    },
    FinishedSuccess {
        thread_id: String,
        captured_at: chrono::DateTime<Utc>,
    },
    FinishedCancelled {
        thread_id: String,
        captured_at: chrono::DateTime<Utc>,
        _message: String,
    },
    FinishedFailure {
        thread_id: String,
        captured_at: chrono::DateTime<Utc>,
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
    Completed,
    Cancelled { message: String },
}

#[derive(Default)]
struct ReviewCommandCapture {
    output: String,
    saw_text_response: bool,
}

enum ReviewShellDestination {
    ControllingTerminal(std::fs::File),
    Stdout,
}

struct ReviewShellMirror {
    destination: ReviewShellDestination,
    review_label: String,
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
        } => format!("{repo}#{pr_number}"),
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

fn render_review_json_event(event: &Value, capture: &mut ReviewCommandCapture) -> Option<String> {
    let event_type = event.get("type")?.as_str()?;

    match event_type {
        "text" => {
            let text = review_json_string_at(event, &["part", "text"])?;
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            capture.saw_text_response = true;
            Some(format!("{trimmed}\n\n"))
        }
        "tool_use" => event.get("part").and_then(format_review_tool_event),
        "error" => {
            let message = review_json_string_at(event, &["error", "data", "message"])
                .or_else(|| review_json_string_at(event, &["error", "message"]))
                .or_else(|| review_json_string_at(event, &["error", "name"]))?;
            Some(format!("[Error] {}\n\n", message.trim()))
        }
        _ => None,
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
    for row_idx in 0..=last_row.min(usize::from(rows.saturating_sub(1))) {
        let formatted_row = String::from_utf8_lossy(&formatted_rows[row_idx]);
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
    pub(super) fn spawn(thread_id: String, launch: ReviewLaunchPlan) -> Self {
        let (tx, rx) = mpsc::channel();
        let worker_thread_id = thread_id.clone();
        let child = Arc::new(Mutex::new(None));
        let cancel_requested = Arc::new(AtomicBool::new(false));
        let worker_child = Arc::clone(&child);
        let worker_cancel_requested = Arc::clone(&cancel_requested);
        thread::spawn(move || {
            let outcome = run_review_stream(
                &tx,
                &worker_thread_id,
                &launch,
                worker_child,
                worker_cancel_requested,
            );
            let message = match outcome {
                Ok(ReviewRunOutcome::Completed) => ReviewJobMessage::FinishedSuccess {
                    thread_id: worker_thread_id.clone(),
                    captured_at: Utc::now(),
                },
                Ok(ReviewRunOutcome::Cancelled { message }) => {
                    ReviewJobMessage::FinishedCancelled {
                        thread_id: worker_thread_id.clone(),
                        captured_at: Utc::now(),
                        _message: message,
                    }
                }
                Err(message) => ReviewJobMessage::FinishedFailure {
                    thread_id: worker_thread_id,
                    captured_at: Utc::now(),
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
        review_settings: ReviewCommandSettings,
    },
}

fn run_review_stream(
    tx: &mpsc::Sender<ReviewJobMessage>,
    thread_id: &str,
    launch: &ReviewLaunchPlan,
    child_handle: Arc<Mutex<Option<Child>>>,
    cancel_requested: Arc<AtomicBool>,
) -> Result<ReviewRunOutcome, String> {
    let review_label = review_shell_label(launch);

    match launch {
        ReviewLaunchPlan::Custom {
            repo_path,
            pr_number,
            review_settings,
            ..
        } => run_custom_review(
            tx,
            thread_id,
            repo_path,
            *pr_number,
            review_settings,
            &review_label,
            child_handle,
            cancel_requested,
        ),
    }
}

fn run_custom_review(
    tx: &mpsc::Sender<ReviewJobMessage>,
    thread_id: &str,
    repo_path: &str,
    pr_number: u64,
    review_settings: &ReviewCommandSettings,
    review_label: &str,
    child_handle: Arc<Mutex<Option<Child>>>,
    cancel_requested: Arc<AtomicBool>,
) -> Result<ReviewRunOutcome, String> {
    let mut command = Command::new("opencode");
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.envs(review_settings.env_vars.iter());
    command.arg("run");
    command.arg("--dir");
    command.arg(repo_path);
    command.arg("--format");
    command.arg("json");
    command.arg("--command");
    command.arg(CUSTOM_REVIEW_COMMAND_NAME);
    command.arg(pr_number.to_string());
    command.arg("--");
    command.args(&review_settings.additional_args);
    println!("Running custom review command: {:?}", command);
    stream_review_command(
        tx,
        thread_id,
        review_label,
        command,
        child_handle,
        cancel_requested,
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
            Err(err) if cancel_requested.load(Ordering::SeqCst) => break,
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

            let rendered = serde_json::from_str::<Value>(trimmed)
                .ok()
                .and_then(|event| render_review_json_event(&event, &mut capture))
                .unwrap_or_else(|| format!("{trimmed}\n"));
            append_rendered_review_output(&mut capture, shell_mirror, tx, thread_id, &rendered);
        }
    }

    let trailing = pending.trim();
    if !trailing.is_empty() {
        let rendered = serde_json::from_str::<Value>(trailing)
            .ok()
            .and_then(|event| render_review_json_event(&event, &mut capture))
            .unwrap_or_else(|| format!("{trailing}\n"));
        append_rendered_review_output(&mut capture, shell_mirror, tx, thread_id, &rendered);
    }

    Ok(capture)
}

fn stream_review_command(
    tx: &mpsc::Sender<ReviewJobMessage>,
    thread_id: &str,
    review_label: &str,
    mut command: Command,
    child_handle: Arc<Mutex<Option<Child>>>,
    cancel_requested: Arc<AtomicBool>,
) -> Result<ReviewRunOutcome, String> {
    let mut child = command
        .spawn()
        .map_err(|err| format!("Failed to start review: {err}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture stdout.".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture stderr.".to_owned())?;

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
        let mut shared_child = child_handle
            .lock()
            .map_err(|_| "Review process lock poisoned unexpectedly.".to_owned())?;
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
                                format!(
                                    "Failed to inspect review process after early stop failure: {wait_err}"
                                )
                            })?
                            .is_some() => {}
                Err(err) => {
                    return Err(format!("Failed to stop review: {err}"));
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

    let stdout_capture = read_review_json_stream(
        tx,
        thread_id,
        stdout,
        &shell_mirror,
        &cancel_requested,
        "output",
    )?;

    let status = {
        let mut shared_child = child_handle
            .lock()
            .map_err(|_| "Review process lock poisoned unexpectedly.".to_owned())?;
        let mut child = shared_child
            .take()
            .ok_or_else(|| "Review process handle missing while waiting.".to_owned())?;
        match child.wait() {
            Ok(status) => status,
            Err(err) if cancel_requested.load(Ordering::SeqCst) => {
                return Ok(ReviewRunOutcome::Cancelled {
                    message: "Review canceled by user.".to_owned(),
                });
            }
            Err(err) => {
                return Err(format!("Failed while waiting for review: {err}"));
            }
        }
    };
    let stderr_capture = match stderr_handle.join() {
        Ok(Ok(stderr_capture)) => stderr_capture,
        Ok(Err(err)) => {
            if cancel_requested.load(Ordering::SeqCst) {
                String::new()
            } else {
                return Err(err);
            }
        }
        Err(_) => {
            if cancel_requested.load(Ordering::SeqCst) {
                String::new()
            } else {
                return Err("Failed to join opencode diagnostics reader.".to_owned());
            }
        }
    };

    if cancel_requested.load(Ordering::SeqCst) && !status.success() {
        finish_review_shell_stream(&shell_mirror, tx, thread_id, "cancelled");
        return Ok(ReviewRunOutcome::Cancelled {
            message: "Review canceled by user.".to_owned(),
        });
    }

    if status.success() {
        if !stdout_capture.saw_text_response {
            finish_review_shell_stream(&shell_mirror, tx, thread_id, "incomplete");
            return Err(format_review_incomplete_output(
                &stdout_capture.output,
                &stderr_capture,
            ));
        }
        finish_review_shell_stream(&shell_mirror, tx, thread_id, "completed");
        return Ok(ReviewRunOutcome::Completed);
    }

    finish_review_shell_stream(&shell_mirror, tx, thread_id, "failed");

    Err(format_review_failure_output(
        &status.to_string(),
        &stdout_capture.output,
        &stderr_capture,
    ))
}

fn format_review_incomplete_output(stdout: &str, stderr: &str) -> String {
    let stdout = stdout.trim();
    let stderr = stderr.trim();
    let guidance = "opencode review exited before producing a final assistant response. This usually means the review ended early or handed work off without returning a final result.";

    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => guidance.to_owned(),
        (false, true) => format!("{guidance}\n\nCaptured output:\n{stdout}"),
        (true, false) => format!("{guidance}\n\nDiagnostics:\n{stderr}"),
        (false, false) => {
            format!("{guidance}\n\nCaptured output:\n{stdout}\n\nDiagnostics:\n{stderr}")
        }
    }
}

#[cfg(test)]
pub(super) fn format_review_success_output(stdout: &str, stderr: &str) -> String {
    let stdout = stdout.trim();
    let stderr = stderr.trim();

    match (stdout.is_empty(), stderr.is_empty()) {
        (false, true) => stdout.to_owned(),
        (false, false) => format!("{stdout}\n\nDiagnostics:\n{stderr}"),
        (true, false) => format!("Diagnostics:\n{stderr}"),
        (true, true) => "opencode review completed with no output.".to_owned(),
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
            repo, pr_number, ..
        } => ReviewOutputState {
            thread_id,
            target: format!("{repo}#{pr_number}"),
            command_label: String::from(CUSTOM_REVIEW_COMMAND_NAME),
            captured_at: None,
            terminal: new_review_terminal(),
            styled_spans: Vec::new(),
            open: true,
            status: ReviewStatus::Running,
            dropped_chars: 0,
        },
    }
}

pub(super) fn append_review_chunk(review_output: &mut ReviewOutputState, chunk: impl AsRef<[u8]>) {
    review_output.terminal.process(chunk.as_ref());
    rebuild_review_output_from_terminal(review_output);
}

pub(super) fn review_summary_text(review_output: &ReviewOutputState) -> String {
    let status = match review_output.status {
        ReviewStatus::Running => "Review running",
        ReviewStatus::Completed => "Review ready",
        ReviewStatus::Cancelled => "Review canceled",
        ReviewStatus::Failed => "Review failed",
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

pub(super) fn render_review_window(
    ctx: &Context,
    account_login: &str,
    review_output: &mut ReviewOutputState,
) {
    if !review_output.open {
        return;
    }

    let title = match review_output.status {
        ReviewStatus::Running => format!("Review in progress: {}", review_output.target),
        ReviewStatus::Completed => format!("Review output: {}", review_output.target),
        ReviewStatus::Cancelled => format!("Review canceled: {}", review_output.target),
        ReviewStatus::Failed => format!("Review failed: {}", review_output.target),
    };
    let mut open = review_output.open;
    let status_line = review_summary_text(review_output);

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
            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .stick_to_bottom(review_output.status == ReviewStatus::Running)
                .show(ui, |scroll| {
                    let content_job = review_output_layout_job(review_output, scroll);
                    scroll.add(egui::Label::new(content_job).extend().selectable(true));
                });
        });

    review_output.open = open;
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
    _pr_url: &str,
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
        review_settings: review_settings.clone(),
    })
}

pub(super) fn custom_review_command_available() -> bool {
    custom_review_command_path().is_some_and(|path| path.exists())
}

fn custom_review_command_path() -> Option<PathBuf> {
    if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(config_home).join("opencode/commands/review-pr.md"));
    }

    env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".config/opencode/commands/review-pr.md"))
}

#[cfg(test)]
mod tests {
    use crate::domain::ReviewCommandSettings;
    use serde_json::json;

    use super::{
        ReviewCommandCapture, ReviewLaunchPlan, ansi_color_from_4bit, append_review_chunk,
        format_review_incomplete_output, initial_review_output_state, render_review_json_event,
        review_output_plain_text, strip_ansi_escape_codes,
    };

    fn review_state() -> super::ReviewOutputState {
        initial_review_output_state(
            String::from("thread-1"),
            &ReviewLaunchPlan::Custom {
                repo: String::from("acme/repo"),
                repo_path: String::from("/tmp/acme-repo"),
                pr_number: 123,
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
    fn render_review_json_event_marks_text_response_as_ready_signal() {
        let mut capture = ReviewCommandCapture::default();
        let event = json!({
            "type": "text",
            "part": {
                "text": "Review posted successfully"
            }
        });

        let rendered = render_review_json_event(&event, &mut capture);

        assert_eq!(rendered.as_deref(), Some("Review posted successfully\n\n"));
        assert!(capture.saw_text_response);
    }

    #[test]
    fn render_review_json_event_formats_tool_output_without_ready_signal() {
        let mut capture = ReviewCommandCapture::default();
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

        let rendered = render_review_json_event(&event, &mut capture);

        assert_eq!(
            rendered.as_deref(),
            Some("[task] Explore repo\ntask_id: child-session\n\n")
        );
        assert!(!capture.saw_text_response);
    }

    #[test]
    fn format_review_incomplete_output_explains_missing_final_response() {
        let message = format_review_incomplete_output("[task] spawned child", "");

        assert!(message.contains("exited before producing a final assistant response"));
        assert!(message.contains("[task] spawned child"));
    }
}
