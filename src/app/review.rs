use std::{
    collections::BTreeMap,
    env,
    io::ErrorKind,
    io::{BufRead, BufReader, Read},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
};

use chrono::Utc;
use eframe::egui::{self, Context};

use super::time::format_local_timestamp;
use super::{CUSTOM_REVIEW_COMMAND_NAME, MAX_REVIEW_OUTPUT_CHARS, repo_paths::canonical_repo_key};
use crate::domain::ReviewCommandSettings;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReviewOutputState {
    pub(super) thread_id: String,
    pub(super) target: String,
    pub(super) command_label: String,
    pub(super) captured_at: Option<chrono::DateTime<Utc>>,
    pub(super) content: String,
    pub(super) open: bool,
    pub(super) status: ReviewStatus,
    pub(super) dropped_chars: usize,
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
        text: String,
    },
    FinishedSuccess {
        thread_id: String,
        captured_at: chrono::DateTime<Utc>,
    },
    FinishedCancelled {
        thread_id: String,
        captured_at: chrono::DateTime<Utc>,
        message: String,
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
    Cancelled(String),
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
                Ok(ReviewRunOutcome::Cancelled(message)) => ReviewJobMessage::FinishedCancelled {
                    thread_id: worker_thread_id.clone(),
                    captured_at: Utc::now(),
                    message,
                },
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
    command.arg("--command");
    command.arg(CUSTOM_REVIEW_COMMAND_NAME);
    command.arg(pr_number.to_string());
    command.arg("--");
    command.args(&review_settings.additional_args);
    println!("Running custom review command: {:?}", command);
    stream_review_command(tx, thread_id, command, child_handle, cancel_requested)
}

fn stream_review_command(
    tx: &mpsc::Sender<ReviewJobMessage>,
    thread_id: &str,
    mut command: Command,
    child_handle: Arc<Mutex<Option<Child>>>,
    cancel_requested: Arc<AtomicBool>,
) -> Result<ReviewRunOutcome, String> {
    let mut child = command
        .spawn()
        .map_err(|err| format!("Failed to start opencode review: {err}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture opencode stdout.".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture opencode stderr.".to_owned())?;

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
                Err(err) => return Err(format!("Failed to stop review: {err}")),
            }
        }
    }

    let stderr_cancel_requested = Arc::clone(&cancel_requested);
    let stderr_handle = thread::spawn(move || -> Result<String, String> {
        let mut diagnostics = String::new();
        let mut reader = BufReader::new(stderr);
        match reader.read_to_string(&mut diagnostics) {
            Ok(_) => {}
            Err(err) if stderr_cancel_requested.load(Ordering::SeqCst) => {}
            Err(err) => return Err(format!("Failed to read opencode diagnostics: {err}")),
        }
        Ok(diagnostics)
    });

    let mut stdout_capture = String::new();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = match reader.read_line(&mut line) {
            Ok(bytes) => bytes,
            Err(err) => {
                if cancel_requested.load(Ordering::SeqCst) {
                    return Ok(ReviewRunOutcome::Cancelled(
                        "Review canceled by user.".to_owned(),
                    ));
                }
                return Err(format!("Failed to read opencode output: {err}"));
            }
        };
        if bytes == 0 {
            break;
        }
        stdout_capture.push_str(&line);
        let _ = tx.send(ReviewJobMessage::Append {
            thread_id: thread_id.to_owned(),
            text: line.clone(),
        });
    }

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
                return Ok(ReviewRunOutcome::Cancelled(
                    "Review canceled by user.".to_owned(),
                ));
            }
            Err(err) => return Err(format!("Failed while waiting for opencode review: {err}")),
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
        return Ok(ReviewRunOutcome::Cancelled(
            "Review canceled by user.".to_owned(),
        ));
    }

    if status.success() {
        if stdout_capture.trim().is_empty() && stderr_capture.trim().is_empty() {
            let _ = tx.send(ReviewJobMessage::Append {
                thread_id: thread_id.to_owned(),
                text: "opencode review completed with no output.".to_owned(),
            });
            return Ok(ReviewRunOutcome::Completed);
        }
        let diagnostics = format_review_success_output("", &stderr_capture);
        if diagnostics != "opencode review completed with no output." && !diagnostics.is_empty() {
            let prefix = if stdout_capture.trim().is_empty() {
                ""
            } else {
                "\n\n"
            };
            let _ = tx.send(ReviewJobMessage::Append {
                thread_id: thread_id.to_owned(),
                text: format!("{prefix}{diagnostics}"),
            });
        }
        return Ok(ReviewRunOutcome::Completed);
    }

    Err(format_review_failure_output(
        &status.to_string(),
        &stdout_capture,
        &stderr_capture,
    ))
}

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
            content: String::new(),
            open: true,
            status: ReviewStatus::Running,
            dropped_chars: 0,
        },
    }
}

pub(super) fn append_review_chunk(review_output: &mut ReviewOutputState, chunk: &str) {
    review_output.content.push_str(chunk);
    let current_chars = review_output.content.chars().count();
    if current_chars <= MAX_REVIEW_OUTPUT_CHARS {
        return;
    }

    let overflow = current_chars - MAX_REVIEW_OUTPUT_CHARS;
    review_output.content = review_output.content.chars().skip(overflow).collect();
    review_output.dropped_chars += overflow;
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
    let mut content = if review_output.dropped_chars > 0 {
        format!(
            "[trimmed {} leading characters]\n\n{}",
            review_output.dropped_chars, review_output.content
        )
    } else {
        review_output.content.clone()
    };
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
            egui::ScrollArea::vertical()
                .stick_to_bottom(review_output.status == ReviewStatus::Running)
                .show(ui, |scroll| {
                    scroll.add(
                        egui::TextEdit::multiline(&mut content)
                            .desired_rows(20)
                            .desired_width(f32::INFINITY)
                            .interactive(false),
                    );
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
