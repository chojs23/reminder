use std::{
    collections::{BTreeMap, HashSet},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::Duration,
};

use chrono::Utc;

use crate::{
    domain::{GitHubAccount, InboxSnapshot, PullRequestReviewers},
    github::{self, FetchError},
};

use super::{
    AccountViewMode, ReviewRequestEditor, SectionKind,
    notification_state::{collect_new_notification_ids, section_stats},
    review::{
        ReviewJob, ReviewJobMessage, ReviewLaunchPlan, ReviewOutputState, ReviewStatus,
        append_review_chunk, append_review_follow_up_prompt, initial_review_output_state,
    },
};

pub(super) struct AccountState {
    pub(super) profile: GitHubAccount,
    pub(super) inbox: Option<InboxSnapshot>,
    pub(super) new_notification_ids: HashSet<String>,
    pub(super) review_outputs: BTreeMap<String, ReviewOutputState>,
    pub(super) last_error: Option<String>,
    pub(super) pending_job: Option<PendingJob>,
    pending_actions: Vec<NotificationActionJob>,
    pending_review_jobs: BTreeMap<String, ReviewJob>,
    pub(super) review_request_editor: Option<ReviewRequestEditor>,
    pending_review_request_load: Option<ReviewRequestLoadJob>,
    pending_review_request_action: Option<ReviewRequestActionJob>,
    pub(super) expanded: bool,
    pub(super) view_mode: AccountViewMode,
    pub(super) search_query: String,
    pub(super) inflight_done: HashSet<String>,
    pub(super) highlights: HashSet<SectionKind>,
}

impl AccountState {
    pub(super) fn new(profile: GitHubAccount) -> Self {
        Self {
            profile,
            inbox: None,
            new_notification_ids: HashSet::new(),
            review_outputs: BTreeMap::new(),
            last_error: None,
            pending_job: None,
            pending_actions: Vec::new(),
            pending_review_jobs: BTreeMap::new(),
            review_request_editor: None,
            pending_review_request_load: None,
            pending_review_request_action: None,
            expanded: true,
            view_mode: AccountViewMode::Inbox,
            search_query: String::new(),
            inflight_done: HashSet::new(),
            highlights: HashSet::new(),
        }
    }

    pub(super) fn start_refresh(&mut self) {
        let profile = self.profile.clone();
        self.last_error = None;
        self.pending_job = Some(PendingJob::spawn(profile));
    }

    pub(super) fn poll_job(&mut self) {
        if let Some(job) = &mut self.pending_job
            && let Some(result) = job.try_take()
        {
            self.pending_job = None;
            match result {
                Ok(inbox) => {
                    let new_notification_ids =
                        collect_new_notification_ids(self.inbox.as_ref(), &inbox);
                    let current_ids: HashSet<_> = inbox
                        .notifications
                        .iter()
                        .map(|item| item.thread_id.as_str())
                        .collect();
                    self.new_notification_ids
                        .retain(|thread_id| current_ids.contains(thread_id.as_str()));
                    self.new_notification_ids.extend(new_notification_ids);
                    let previous_stats = self.inbox.as_ref().map(section_stats);
                    let next_stats = section_stats(&inbox);
                    if let Some(old) = previous_stats {
                        if next_stats.inbox.bumped_since(&old.inbox) {
                            self.highlights.insert(SectionKind::Inbox);
                        }
                        if next_stats
                            .review_requests
                            .bumped_since(&old.review_requests)
                        {
                            self.highlights.insert(SectionKind::ReviewRequests);
                        }
                        if next_stats.mentions.bumped_since(&old.mentions) {
                            self.highlights.insert(SectionKind::Mentions);
                        }
                        if next_stats.notifications.bumped_since(&old.notifications) {
                            self.highlights.insert(SectionKind::Notifications);
                        }
                    }

                    self.inbox = Some(inbox);
                    self.last_error = None;
                }
                Err(err) => {
                    self.last_error = Some(err.to_string());
                }
            }
        }
    }

    pub(super) fn poll_review_job(&mut self) {
        let mut messages = Vec::new();

        for (thread_id, mut job) in std::mem::take(&mut self.pending_review_jobs) {
            let (drained, finished) = job.drain_messages();
            messages.extend(drained);
            if !finished {
                self.pending_review_jobs.insert(thread_id, job);
            }
        }

        for message in messages {
            match message {
                ReviewJobMessage::Append { thread_id, bytes } => {
                    if let Some(review_output) = self.review_outputs.get_mut(&thread_id) {
                        if let Some(prompt) = review_output.pending_follow_up_prompt.take() {
                            review_output.follow_up_draft.clear();
                            append_review_follow_up_prompt(review_output, &prompt);
                        }
                        append_review_chunk(review_output, &bytes);
                    }
                }
                ReviewJobMessage::FinishedSuccess {
                    thread_id,
                    captured_at,
                    session_id,
                } => {
                    if let Some(review_output) = self.review_outputs.get_mut(&thread_id) {
                        review_output.status = ReviewStatus::Completed;
                        review_output.captured_at = Some(captured_at);
                        if session_id.is_some() {
                            review_output.session_id = session_id;
                        }
                    }
                    self.inflight_done.remove(&thread_id);
                }
                ReviewJobMessage::FinishedCancelled {
                    thread_id,
                    captured_at,
                    session_id,
                    _message: _,
                } => {
                    if let Some(review_output) = self.review_outputs.get_mut(&thread_id) {
                        review_output.status = ReviewStatus::Cancelled;
                        review_output.captured_at = Some(captured_at);
                        if session_id.is_some() {
                            review_output.session_id = session_id;
                        }
                    }
                    self.inflight_done.remove(&thread_id);
                }
                ReviewJobMessage::FinishedFailure {
                    thread_id,
                    captured_at,
                    session_id,
                    message,
                } => {
                    self.last_error = Some(message.clone());
                    if let Some(review_output) = self.review_outputs.get_mut(&thread_id) {
                        review_output.status = ReviewStatus::Failed;
                        review_output.captured_at = Some(captured_at);
                        if session_id.is_some() {
                            review_output.session_id = session_id;
                        }
                    }
                    self.inflight_done.remove(&thread_id);
                }
            }
        }
    }

    pub(super) fn poll_action_jobs(&mut self) {
        let mut finished = Vec::new();
        self.pending_actions.retain(|job| match job.try_take() {
            None => true,
            Some(result) => {
                finished.push(result);
                false
            }
        });

        for outcome in finished {
            match outcome {
                Ok(NotificationActionOutcome::Done(thread_id))
                | Ok(NotificationActionOutcome::Read(thread_id)) => {
                    self.handle_action_success(&thread_id)
                }
                Err((thread_id, err)) => {
                    self.last_error = Some(err);
                    if let Some(id) = thread_id {
                        self.inflight_done.remove(&id);
                    }
                }
            }
        }
    }

    pub(super) fn poll_review_request_jobs(&mut self) {
        if let Some(job) = &self.pending_review_request_load
            && let Some(result) = job.try_take()
        {
            self.pending_review_request_load = None;
            match result {
                Ok(outcome) => {
                    let PullRequestReviewers {
                        requested_reviewers,
                        current_reviewers,
                        reviewer_history,
                    } = outcome.reviewers;
                    if let Some(editor) = &mut self.review_request_editor
                        && editor.repo == outcome.target.repo
                        && editor.pr_number == outcome.target.pr_number
                    {
                        editor.requested_reviewers = requested_reviewers;
                        editor.current_reviewers = current_reviewers;
                        editor.reviewer_history = reviewer_history;
                        editor.pending_load = false;
                        editor.form_error = None;
                    }
                }
                Err((target, err)) => {
                    if let Some(editor) = &mut self.review_request_editor
                        && editor.repo == target.repo
                        && editor.pr_number == target.pr_number
                    {
                        editor.pending_load = false;
                        editor.form_error = Some(err);
                    } else {
                        self.last_error = Some(err);
                    }
                }
            }
        }

        if let Some(job) = &self.pending_review_request_action
            && let Some(result) = job.try_take()
        {
            self.pending_review_request_action = None;
            match result {
                Ok(outcome) => {
                    let mut should_reload = false;
                    if let Some(editor) = &mut self.review_request_editor
                        && editor.repo == outcome.target.repo
                        && editor.pr_number == outcome.target.pr_number
                    {
                        editor.pending_action = false;
                        editor.status_message = Some(outcome.message);
                        editor.form_error = None;
                        should_reload = true;
                    }
                    if should_reload {
                        self.reload_review_request_editor();
                    }
                }
                Err((target, err)) => {
                    if let Some(editor) = &mut self.review_request_editor
                        && editor.repo == target.repo
                        && editor.pr_number == target.pr_number
                    {
                        editor.pending_action = false;
                        editor.form_error = Some(err);
                    } else {
                        self.last_error = Some(err);
                    }
                }
            }
        }
    }

    fn handle_action_success(&mut self, thread_id: &str) {
        if let Some(inbox) = &mut self.inbox
            && let Some(item) = inbox
                .notifications
                .iter_mut()
                .find(|item| item.thread_id == thread_id)
        {
            item.unread = false;
            item.last_read_at = Some(Utc::now());
        }
        self.inflight_done.remove(thread_id);
    }

    pub(super) fn mark_notification_seen(&mut self, thread_id: &str) {
        if let Some(inbox) = &mut self.inbox
            && let Some(item) = inbox
                .notifications
                .iter_mut()
                .find(|item| item.thread_id == thread_id)
        {
            item.unread = false;
            item.last_read_at = Some(item.updated_at);
        }
    }

    pub(super) fn request_mark_read(&mut self, thread_id: String) {
        if self.inflight_done.contains(&thread_id) {
            return;
        }
        let profile = self.profile.clone();
        let job = NotificationActionJob::mark_read(profile, thread_id.clone());
        self.pending_actions.push(job);
        self.inflight_done.insert(thread_id);
    }

    pub(super) fn request_mark_done(&mut self, thread_id: String) {
        if self.inflight_done.contains(&thread_id) {
            return;
        }
        let profile = self.profile.clone();
        let job = NotificationActionJob::mark_done(profile, thread_id.clone());
        self.pending_actions.push(job);
        self.inflight_done.insert(thread_id);
    }

    pub(super) fn request_review(&mut self, thread_id: String, launch: ReviewLaunchPlan) {
        if self.inflight_done.contains(&thread_id) {
            return;
        }
        self.last_error = None;
        self.review_outputs.insert(
            thread_id.clone(),
            initial_review_output_state(thread_id.clone(), &launch),
        );
        let job = ReviewJob::spawn(thread_id.clone(), launch, self.profile.token.clone());
        self.pending_review_jobs.insert(thread_id.clone(), job);
        self.inflight_done.insert(thread_id);
    }

    pub(super) fn cancel_review(&mut self, thread_id: &str) {
        let Some(job) = self.pending_review_jobs.get(thread_id) else {
            return;
        };

        if let Err(err) = job.cancel() {
            self.last_error = Some(err);
        }
    }

    pub(super) fn review_is_running(&self, thread_id: &str) -> bool {
        self.pending_review_jobs.contains_key(thread_id)
            || self
                .review_outputs
                .get(thread_id)
                .is_some_and(|review_output| review_output.status == ReviewStatus::Running)
    }

    pub(super) fn can_send_review_follow_up(&self, thread_id: &str) -> bool {
        self.review_outputs
            .get(thread_id)
            .is_some_and(|review_output| {
                !self.review_is_running(thread_id) && review_output.session_id.is_some()
            })
    }

    pub(super) fn request_review_follow_up(&mut self, thread_id: &str) {
        if self.review_is_running(thread_id) {
            if let Some(review_output) = self.review_outputs.get_mut(thread_id) {
                review_output.follow_up_error = Some(
                    "Wait for the current run to finish before sending another message.".to_owned(),
                );
            }
            return;
        }

        if !self.can_send_review_follow_up(thread_id) {
            if let Some(review_output) = self.review_outputs.get_mut(thread_id) {
                review_output.follow_up_error = Some(
                    "This session cannot accept follow-up because no opencode session ID was captured."
                        .to_owned(),
                );
            }
            return;
        }

        let Some(review_output) = self.review_outputs.get_mut(thread_id) else {
            return;
        };

        let prompt = review_output.follow_up_draft.trim().to_owned();
        if prompt.is_empty() {
            review_output.follow_up_error = Some("Enter a message before sending.".to_owned());
            return;
        }
        let session_id = review_output
            .session_id
            .clone()
            .expect("follow-up eligibility checked before launching");

        let launch = ReviewLaunchPlan::FollowUp {
            target: review_output.target.clone(),
            repo_path: review_output.repo_path.clone(),
            session_id,
            prompt: prompt.clone(),
            review_settings: review_output.review_settings.clone(),
        };

        self.last_error = None;
        review_output.follow_up_error = None;
        review_output.open = true;
        review_output.status = ReviewStatus::Running;
        review_output.captured_at = None;
        review_output.command_label =
            String::from(review_output.output_kind.follow_up_command_label());
        review_output.pending_follow_up_prompt = Some(prompt);

        let job = ReviewJob::spawn(thread_id.to_owned(), launch, self.profile.token.clone());
        self.pending_review_jobs.insert(thread_id.to_owned(), job);
        self.inflight_done.insert(thread_id.to_owned());
    }

    pub(super) fn needs_refresh(&self, threshold: Duration) -> bool {
        match &self.inbox {
            None => true,
            Some(inbox) => match chrono::Duration::from_std(threshold) {
                Ok(delta) => (Utc::now() - inbox.fetched_at) >= delta,
                Err(_) => true,
            },
        }
    }

    pub(super) fn clear_new_notifications(&mut self) {
        self.new_notification_ids.clear();
    }

    pub(super) fn review_in_progress(&self) -> bool {
        !self.pending_review_jobs.is_empty()
    }

    pub(super) fn active_review_thread_ids(&self) -> HashSet<String> {
        self.review_outputs
            .values()
            .filter(|review_output| review_output.status == ReviewStatus::Running)
            .map(|review_output| review_output.thread_id.clone())
            .collect()
    }

    pub(super) fn toggle_review_window_for_thread(&mut self, thread_id: &str) {
        if let Some(review_output) = self.review_outputs.get_mut(thread_id) {
            review_output.open = !review_output.open;
        }
    }

    pub(super) fn open_review_request_editor(
        &mut self,
        repo: String,
        pr_number: u64,
        pr_title: String,
    ) {
        self.review_request_editor = Some(ReviewRequestEditor {
            repo: repo.clone(),
            pr_number,
            pr_title,
            reviewer_login: String::new(),
            requested_reviewers: Vec::new(),
            current_reviewers: Vec::new(),
            reviewer_history: Vec::new(),
            pending_load: true,
            pending_action: false,
            form_error: None,
            status_message: None,
        });
        self.pending_review_request_action = None;
        self.pending_review_request_load = Some(ReviewRequestLoadJob::spawn(
            self.profile.clone(),
            ReviewRequestTarget { repo, pr_number },
        ));
    }

    pub(super) fn close_review_request_editor(&mut self) {
        self.review_request_editor = None;
        self.pending_review_request_load = None;
        self.pending_review_request_action = None;
    }

    pub(super) fn request_pull_request_review(&mut self, reviewer_login: String) {
        let Some((target, reviewer_login)) =
            self.prepare_review_request_action(reviewer_login, ReviewRequestMutationKind::Request)
        else {
            return;
        };

        self.pending_review_request_action = Some(ReviewRequestActionJob::spawn(
            self.profile.clone(),
            target,
            reviewer_login,
            ReviewRequestMutationKind::Request,
            false,
        ));
    }

    pub(super) fn renotify_pull_request_review(&mut self, reviewer_login: String) {
        let Some((target, reviewer_login)) =
            self.prepare_review_request_action(reviewer_login, ReviewRequestMutationKind::ReNotify)
        else {
            return;
        };
        let already_requested = self.review_request_editor.as_ref().is_some_and(|editor| {
            reviewer_list_contains(&editor.requested_reviewers, &reviewer_login)
        });

        self.pending_review_request_action = Some(ReviewRequestActionJob::spawn(
            self.profile.clone(),
            target,
            reviewer_login,
            ReviewRequestMutationKind::ReNotify,
            already_requested,
        ));
    }

    pub(super) fn remove_pull_request_review(&mut self, reviewer_login: String) {
        let Some(editor) = self.review_request_editor.as_mut() else {
            return;
        };
        let reviewer_login = reviewer_login.trim().to_owned();
        if reviewer_login.is_empty() {
            editor.form_error = Some("Enter a reviewer login first.".to_owned());
            return;
        }
        if editor.pending_action {
            return;
        }

        editor.pending_action = true;
        editor.form_error = None;
        editor.status_message = None;
        let target = ReviewRequestTarget {
            repo: editor.repo.clone(),
            pr_number: editor.pr_number,
        };
        let profile = self.profile.clone();

        self.pending_review_request_action = Some(ReviewRequestActionJob::spawn(
            profile,
            target,
            reviewer_login,
            ReviewRequestMutationKind::Remove,
            false,
        ));
    }

    fn reload_review_request_editor(&mut self) {
        let Some(editor) = self.review_request_editor.as_mut() else {
            return;
        };

        editor.pending_load = true;
        let target = ReviewRequestTarget {
            repo: editor.repo.clone(),
            pr_number: editor.pr_number,
        };
        let profile = self.profile.clone();
        self.pending_review_request_load = Some(ReviewRequestLoadJob::spawn(profile, target));
    }

    fn prepare_review_request_action(
        &mut self,
        reviewer_login: String,
        action: ReviewRequestMutationKind,
    ) -> Option<(ReviewRequestTarget, String)> {
        let editor = self.review_request_editor.as_mut()?;
        let reviewer_login = reviewer_login.trim().to_owned();
        if reviewer_login.is_empty() {
            editor.form_error = Some("Enter a reviewer login first.".to_owned());
            return None;
        }
        if editor.pending_action {
            return None;
        }
        if matches!(action, ReviewRequestMutationKind::Request)
            && reviewer_list_contains(&editor.requested_reviewers, &reviewer_login)
        {
            editor.form_error = Some(
                "This reviewer is already requested. Use Re-notify or remove them first."
                    .to_owned(),
            );
            return None;
        }

        editor.pending_action = true;
        editor.reviewer_login = reviewer_login.clone();
        editor.form_error = None;
        editor.status_message = None;

        Some((
            ReviewRequestTarget {
                repo: editor.repo.clone(),
                pr_number: editor.pr_number,
            },
            reviewer_login,
        ))
    }
}

pub(super) struct PendingJob {
    receiver: Receiver<github::FetchOutcome>,
}

impl PendingJob {
    fn spawn(profile: GitHubAccount) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let outcome = (|| -> github::FetchOutcome {
                let client = github::build_client()?;
                github::fetch_inbox(&client, &profile)
            })();
            let _ = tx.send(outcome);
        });
        Self { receiver: rx }
    }

    fn try_take(&self) -> Option<github::FetchOutcome> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(FetchError::BackgroundWorkerGone)),
        }
    }
}

enum NotificationActionOutcome {
    Done(String),
    Read(String),
}

type NotificationActionResult = Result<NotificationActionOutcome, (Option<String>, String)>;

struct NotificationActionJob {
    receiver: Receiver<NotificationActionResult>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReviewRequestTarget {
    repo: String,
    pr_number: u64,
}

struct ReviewRequestLoadOutcome {
    target: ReviewRequestTarget,
    reviewers: PullRequestReviewers,
}

type ReviewRequestLoadResult = Result<ReviewRequestLoadOutcome, (ReviewRequestTarget, String)>;

struct ReviewRequestLoadJob {
    receiver: Receiver<ReviewRequestLoadResult>,
}

impl ReviewRequestLoadJob {
    fn spawn(profile: GitHubAccount, target: ReviewRequestTarget) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let outcome = Self::load_worker(profile, target);
            let _ = tx.send(outcome);
        });
        Self { receiver: rx }
    }

    fn load_worker(profile: GitHubAccount, target: ReviewRequestTarget) -> ReviewRequestLoadResult {
        let client = github::build_client().map_err(|err| (target.clone(), err.to_string()))?;
        let reviewers =
            github::fetch_pull_request_reviewers(&client, &profile, &target.repo, target.pr_number)
                .map_err(|err| (target.clone(), err.to_string()))?;
        Ok(ReviewRequestLoadOutcome { target, reviewers })
    }

    fn try_take(&self) -> Option<ReviewRequestLoadResult> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err((
                ReviewRequestTarget {
                    repo: String::new(),
                    pr_number: 0,
                },
                "Review request worker disconnected".to_owned(),
            ))),
        }
    }
}

#[derive(Clone, Copy)]
enum ReviewRequestMutationKind {
    Request,
    Remove,
    ReNotify,
}

struct ReviewRequestActionOutcome {
    target: ReviewRequestTarget,
    message: String,
}

type ReviewRequestActionResult = Result<ReviewRequestActionOutcome, (ReviewRequestTarget, String)>;

struct ReviewRequestActionJob {
    receiver: Receiver<ReviewRequestActionResult>,
}

impl ReviewRequestActionJob {
    fn spawn(
        profile: GitHubAccount,
        target: ReviewRequestTarget,
        reviewer_login: String,
        action: ReviewRequestMutationKind,
        already_requested: bool,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let outcome =
                Self::action_worker(profile, target, reviewer_login, action, already_requested);
            let _ = tx.send(outcome);
        });
        Self { receiver: rx }
    }

    fn action_worker(
        profile: GitHubAccount,
        target: ReviewRequestTarget,
        reviewer_login: String,
        action: ReviewRequestMutationKind,
        already_requested: bool,
    ) -> ReviewRequestActionResult {
        let client = github::build_client().map_err(|err| (target.clone(), err.to_string()))?;
        match action {
            ReviewRequestMutationKind::Request => {
                github::request_pull_request_reviewer(
                    &client,
                    &profile,
                    &target.repo,
                    target.pr_number,
                    &reviewer_login,
                )
                .map_err(|err| (target.clone(), err.to_string()))?;
                Ok(ReviewRequestActionOutcome {
                    target,
                    message: format!("Requested review from {reviewer_login}."),
                })
            }
            ReviewRequestMutationKind::Remove => {
                github::remove_pull_request_reviewer(
                    &client,
                    &profile,
                    &target.repo,
                    target.pr_number,
                    &reviewer_login,
                )
                .map_err(|err| (target.clone(), err.to_string()))?;
                Ok(ReviewRequestActionOutcome {
                    target,
                    message: format!("Removed review request for {reviewer_login}."),
                })
            }
            ReviewRequestMutationKind::ReNotify => {
                if already_requested {
                    github::remove_pull_request_reviewer(
                        &client,
                        &profile,
                        &target.repo,
                        target.pr_number,
                        &reviewer_login,
                    )
                    .map_err(|err| (target.clone(), err.to_string()))?;
                }
                github::request_pull_request_reviewer(
                    &client,
                    &profile,
                    &target.repo,
                    target.pr_number,
                    &reviewer_login,
                )
                .map_err(|err| (target.clone(), err.to_string()))?;
                Ok(ReviewRequestActionOutcome {
                    target,
                    message: format!("Re-notified {reviewer_login}."),
                })
            }
        }
    }

    fn try_take(&self) -> Option<ReviewRequestActionResult> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err((
                ReviewRequestTarget {
                    repo: String::new(),
                    pr_number: 0,
                },
                "Review request action worker disconnected".to_owned(),
            ))),
        }
    }
}

fn reviewer_list_contains(reviewers: &[String], reviewer_login: &str) -> bool {
    reviewers
        .iter()
        .any(|login| login.eq_ignore_ascii_case(reviewer_login))
}

impl NotificationActionJob {
    fn mark_done(profile: GitHubAccount, thread_id: String) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let outcome = Self::mark_done_worker(profile, thread_id);
            let _ = tx.send(outcome);
        });
        Self { receiver: rx }
    }

    fn mark_done_worker(profile: GitHubAccount, thread_id: String) -> NotificationActionResult {
        let client =
            github::build_client().map_err(|err| (Some(thread_id.clone()), err.to_string()))?;
        github::mark_notification_done(&client, &profile, &thread_id)
            .map_err(|err| (Some(thread_id.clone()), err.to_string()))?;
        Ok(NotificationActionOutcome::Done(thread_id))
    }

    fn mark_read(profile: GitHubAccount, thread_id: String) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let outcome = Self::mark_read_worker(profile, thread_id);
            let _ = tx.send(outcome);
        });
        Self { receiver: rx }
    }

    fn mark_read_worker(profile: GitHubAccount, thread_id: String) -> NotificationActionResult {
        let client =
            github::build_client().map_err(|err| (Some(thread_id.clone()), err.to_string()))?;
        github::mark_notification_read(&client, &profile, &thread_id)
            .map_err(|err| (Some(thread_id.clone()), err.to_string()))?;
        Ok(NotificationActionOutcome::Read(thread_id))
    }

    fn try_take(&self) -> Option<NotificationActionResult> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err((
                None,
                "Notification action worker disconnected".to_owned(),
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AccountState;
    use crate::{
        app::review::{
            ReviewLaunchPlan, ReviewStatus, append_review_chunk, append_review_follow_up_prompt,
            initial_review_output_state,
        },
        domain::{GitHubAccount, ReviewCommandSettings},
    };

    fn account_state() -> AccountState {
        AccountState::new(GitHubAccount {
            login: String::from("neo"),
            token: String::from("token"),
            review_settings: ReviewCommandSettings::default(),
        })
    }

    fn sample_review_output() -> crate::app::review::ReviewOutputState {
        initial_review_output_state(
            String::from("thread-1"),
            &ReviewLaunchPlan::Custom {
                repo: String::from("acme/repo"),
                repo_path: String::from("/tmp/acme-repo"),
                pr_number: 42,
                pr_url: String::from("https://github.com/acme/repo/pull/42"),
                review_settings: ReviewCommandSettings::default(),
            },
        )
    }

    #[test]
    fn can_send_review_follow_up_requires_finished_review_with_session_id() {
        let mut account = account_state();
        let review_output = sample_review_output();

        account
            .review_outputs
            .insert(String::from("thread-1"), review_output);
        assert!(!account.can_send_review_follow_up("thread-1"));

        let mut review_output = sample_review_output();
        review_output.status = ReviewStatus::Completed;
        review_output.session_id = Some(String::from("ses_123"));
        account
            .review_outputs
            .insert(String::from("thread-1"), review_output);

        assert!(account.can_send_review_follow_up("thread-1"));
    }

    #[test]
    fn review_is_running_checks_pending_jobs_and_status() {
        let mut account = account_state();
        let review_output = sample_review_output();

        account
            .review_outputs
            .insert(String::from("thread-1"), review_output);

        assert!(account.review_is_running("thread-1"));

        if let Some(review_output) = account.review_outputs.get_mut("thread-1") {
            review_output.status = ReviewStatus::Completed;
        }

        assert!(!account.review_is_running("thread-1"));
    }

    #[test]
    fn request_review_follow_up_keeps_draft_until_output_arrives() {
        let mut account = account_state();
        let mut review_output = sample_review_output();
        review_output.status = ReviewStatus::Completed;
        review_output.session_id = Some(String::from("ses_123"));
        review_output.follow_up_draft = String::from("Explain the main issue");
        account
            .review_outputs
            .insert(String::from("thread-1"), review_output);

        account.request_review_follow_up("thread-1");

        let review_output = account
            .review_outputs
            .get("thread-1")
            .expect("review output");
        assert_eq!(review_output.status, ReviewStatus::Running);
        assert_eq!(review_output.follow_up_draft, "Explain the main issue");
        assert_eq!(
            review_output.pending_follow_up_prompt.as_deref(),
            Some("Explain the main issue")
        );
        assert!(account.last_error.is_none());
    }

    #[test]
    fn append_clears_pending_follow_up_prompt_and_draft() {
        let mut account = account_state();
        let mut review_output = sample_review_output();
        review_output.pending_follow_up_prompt = Some(String::from("Explain the main issue"));
        review_output.follow_up_draft = String::from("Explain the main issue");
        account
            .review_outputs
            .insert(String::from("thread-1"), review_output);

        if let Some(review_output) = account.review_outputs.get_mut("thread-1") {
            if let Some(prompt) = review_output.pending_follow_up_prompt.take() {
                review_output.follow_up_draft.clear();
                append_review_follow_up_prompt(review_output, &prompt);
            }
            append_review_chunk(review_output, b"response");
        }

        let review_output = account
            .review_outputs
            .get("thread-1")
            .expect("review output");
        assert!(review_output.pending_follow_up_prompt.is_none());
        assert!(review_output.follow_up_draft.is_empty());
    }
}
