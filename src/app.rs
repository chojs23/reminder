use std::{
    collections::HashSet,
    fs,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use chrono::Utc;
use eframe::{
    App, CreationContext, Frame,
    egui::{self, Context, FontData, FontDefinitions, FontFamily, Layout, RichText},
};
use egui_extras::{Column, TableBuilder};

use crate::{
    domain::{GitHubAccount, InboxSnapshot, NotificationItem},
    github::{self, FetchError},
    storage::AccountStore,
};

pub const APP_NAME: &str = "Reminder";

pub const CJK_FONT_NAME: &str = "CJK_Fallback_Font";

#[cfg(target_os = "macos")]
const SYSTEM_FONT_CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/Supplemental/AppleSDGothicNeo.ttc",
    "/System/Library/Fonts/AppleSDGothicNeo.ttc",
    "/System/Library/Fonts/Supplemental/NotoSansCJK-Regular.ttc",
];

#[cfg(target_os = "windows")]
const SYSTEM_FONT_CANDIDATES: &[&str] = &[
    "C:\\Windows\\Fonts\\malgun.ttf",
    "C:\\Windows\\Fonts\\malgunbd.ttf",
    "C:\\Windows\\Fonts\\YuGothM.ttc",
];

#[cfg(target_os = "linux")]
const SYSTEM_FONT_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/noto/NotoSansKR-Regular.otf",
];

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
const SYSTEM_FONT_CANDIDATES: &[&str] = &[];
const AUTO_REFRESH_INTERVAL_SECS: u64 = 180;

pub struct ReminderApp {
    account_form: AccountForm,
    accounts: Vec<AccountState>,
    secret_store: Option<AccountStore>,
    storage_warning: Option<String>,
    global_error: Option<String>,
    auto_refresh: BatchRefreshScheduler,
}

impl ReminderApp {
    pub fn new(cc: &CreationContext<'_>) -> Self {
        install_international_fonts(&cc.egui_ctx);

        let mut app = Self {
            account_form: AccountForm::default(),
            accounts: Vec::new(),
            secret_store: None,
            storage_warning: None,
            global_error: None,
            auto_refresh: BatchRefreshScheduler::new(Duration::from_secs(
                AUTO_REFRESH_INTERVAL_SECS,
            )),
        };

        match AccountStore::initialize() {
            Ok(store) => {
                match store.hydrate() {
                    Ok(outcome) => {
                        for profile in outcome.profiles {
                            let mut state = AccountState::new(profile);
                            state.start_refresh();
                            app.accounts.push(state);
                        }
                    }
                    Err(err) => {
                        app.storage_warning =
                            Some(format!("Failed to restore saved accounts: {err}"))
                    }
                }
                app.secret_store = Some(store);
            }
            Err(err) => {
                app.storage_warning = Some(format!(
                    "Local token storage is unavailable; tokens cannot be persisted ({err})."
                ));
            }
        }

        app.auto_refresh.mark_triggered();

        app
    }

    fn add_account(&mut self) {
        if self.account_form.login.trim().is_empty() || self.account_form.token.trim().is_empty() {
            self.account_form.form_error =
                Some("Both the login and a Personal Access Token are required.".to_owned());
            return;
        }

        let profile = GitHubAccount {
            login: self.account_form.login.trim().to_owned(),
            token: self.account_form.token.trim().to_owned(),
        };

        if let Some(store) = &self.secret_store {
            if let Err(err) = store.persist_profile(&profile) {
                self.account_form.form_error =
                    Some(format!("Unable to persist credentials locally: {err}"));
                return;
            }
        } else {
            self.account_form.form_error = Some(
                "Local token storage is not available; cannot add new accounts right now."
                    .to_owned(),
            );
            return;
        }

        let mut state = AccountState::new(profile);
        state.start_refresh();
        self.auto_refresh.mark_triggered();
        self.accounts.push(state);
        self.account_form = AccountForm::default();
    }

    fn remove_account_at(&mut self, idx: usize) {
        if idx >= self.accounts.len() {
            return;
        }

        let login = self.accounts[idx].profile.login.clone();
        if let Some(store) = &self.secret_store
            && let Err(err) = store.forget(&login)
        {
            self.global_error = Some(format!("Failed to remove credentials for {login}: {err}"));
        }

        self.accounts.remove(idx);
    }

    fn poll_jobs(&mut self) {
        for account in &mut self.accounts {
            account.poll_job();
            account.poll_action_jobs();
        }
    }

    fn maybe_auto_refresh(&mut self) {
        if !self.auto_refresh.should_trigger() {
            return;
        }

        let mut triggered = false;
        let stale_after = Duration::from_secs(AUTO_REFRESH_INTERVAL_SECS);
        for account in &mut self.accounts {
            if account.pending_job.is_some() {
                continue;
            }
            if account.needs_refresh(stale_after) {
                account.start_refresh();
                triggered = true;
            }
        }

        if triggered {
            self.auto_refresh.mark_triggered();
        }
    }

    fn render_side_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Accounts");
        ui.separator();

        if let Some(warning) = &self.storage_warning {
            ui.colored_label(ui.visuals().warn_fg_color, warning);
            ui.separator();
        }

        ui.label("GitHub username");
        ui.text_edit_singleline(&mut self.account_form.login);

        ui.label("Personal access token");
        ui.add(
            egui::TextEdit::singleline(&mut self.account_form.token)
                .password(true)
                .hint_text("ghp_..."),
        );

        let add_enabled = !self.account_form.login.trim().is_empty()
            && !self.account_form.token.trim().is_empty();
        if ui
            .add_enabled(add_enabled, egui::Button::new("Add account"))
            .clicked()
        {
            self.add_account();
        }

        if let Some(error) = &self.account_form.form_error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }

        ui.separator();
        ui.label("Tracked accounts");
        if self.accounts.is_empty() {
            ui.weak("No accounts yet.");
        } else {
            let mut remove_idx = None;
            for (idx, account) in self.accounts.iter_mut().enumerate() {
                ui.horizontal(|row| {
                    row.label(&account.profile.login);
                    if row.small_button("Refresh").clicked() {
                        account.start_refresh();
                        self.auto_refresh.mark_triggered();
                    }
                    if row.small_button("Remove").clicked() {
                        remove_idx = Some(idx);
                    }
                });
            }
            if let Some(idx) = remove_idx {
                self.remove_account_at(idx);
            }
        }
    }

    fn render_dashboard(&mut self, ui: &mut egui::Ui) {
        self.render_global_error(ui);

        if self.accounts.is_empty() {
            ui.centered_and_justified(|center| {
                center.label("Add at least one GitHub account to start aggregating notifications.");
            });
            return;
        }

        egui::ScrollArea::vertical().show(ui, |area| {
            for account in &mut self.accounts {
                let account_id = account.profile.login.clone();
                area.push_id(account_id, |ui| {
                    render_account_card(ui, account);
                });
            }
        });
    }

    fn render_global_error(&self, ui: &mut egui::Ui) {
        if let Some(error) = &self.global_error {
            ui.colored_label(ui.visuals().error_fg_color, error);
            ui.add_space(8.0);
        }
    }
}

fn render_account_card(ui: &mut egui::Ui, account: &mut AccountState) {
    ui.group(|group| {
        render_account_header(group, account);
        render_account_status(group, account);
        render_account_body(group, account);
    });
    ui.add_space(12.0);
}

fn render_account_header(group: &mut egui::Ui, account: &mut AccountState) {
    group.horizontal(|row| {
        row.heading(format!("Account: {}", account.profile.login));
        if row
            .small_button(if account.expanded {
                "Hide notifications"
            } else {
                "Show notifications"
            })
            .clicked()
        {
            account.expanded = !account.expanded;
        }
        row.with_layout(Layout::right_to_left(egui::Align::Center), |lane| {
            lane.add(
                egui::TextEdit::singleline(&mut account.search_query)
                    .hint_text("Search…")
                    .desired_width(160.0),
            );
        });
    });
}

fn render_account_status(group: &mut egui::Ui, account: &AccountState) {
    if let Some(inbox) = &account.inbox {
        group.label(format!(
            "Last synced {} UTC",
            inbox.fetched_at.format("%Y-%m-%d %H:%M:%S")
        ));
    } else {
        group.label("No data fetched yet.");
    }

    if let Some(err) = &account.last_error {
        group.colored_label(group.visuals().error_fg_color, err);
    } else if account.pending_job.is_some() {
        group.label("Fetching latest notifications...");
    }
}

fn render_account_body(group: &mut egui::Ui, account: &mut AccountState) {
    if !account.expanded {
        if account.inbox.is_none() {
            group.separator();
            group.weak("No data loaded yet.");
        }
        return;
    }

    if account.inbox.is_some() {
        group.separator();
        let filter = SearchFilter::new(&account.search_query);
        let actions = {
            let inflight_done = account.inflight_done.clone();
            let inbox = account.inbox.as_ref().expect("checked above");
            render_account_sections(group, inbox, &filter, &inflight_done)
        };
        for action in actions {
            match action {
                AccountAction::MarkNotificationDone(id) => account.request_mark_done(id),
                AccountAction::MarkNotificationSeen(id) => account.mark_notification_seen(&id),
                AccountAction::MarkNotificationRead(id) => account.request_mark_read(id),
            }
        }
    }
}

fn render_account_sections(
    group: &mut egui::Ui,
    inbox: &InboxSnapshot,
    filter: &SearchFilter,
    inflight_done: &HashSet<String>,
) -> Vec<AccountAction> {
    const REVIEW_REQUEST_REASON: &str = "review_requested";
    const MENTION_REASONS: &[&str] = &["mention", "team_mention"];

    // Show both seen and unseen items in their contextual buckets; the Done section
    // is temporarily disabled to avoid splitting the feed.
    let mut actions = Vec::new();
    let review_requests: Vec<_> = inbox
        .notifications
        .iter()
        .filter(|item| item.reason == REVIEW_REQUEST_REASON)
        .collect();

    actions.extend(render_notification_section(
        group,
        "Review requests",
        review_requests,
        "No pending review requests.",
        filter,
        inflight_done,
        true,
    ));
    group.separator();

    let mentions: Vec<_> = inbox
        .notifications
        .iter()
        .filter(|item| MENTION_REASONS.contains(&item.reason.as_str()))
        .collect();
    actions.extend(render_notification_section(
        group,
        "Mentions",
        mentions,
        "No recent mentions.",
        filter,
        inflight_done,
        true,
    ));
    group.separator();

    let other: Vec<_> = inbox
        .notifications
        .iter()
        .filter(|item| {
            item.reason != REVIEW_REQUEST_REASON && !MENTION_REASONS.contains(&item.reason.as_str())
        })
        .collect();
    actions.extend(render_notification_section(
        group,
        "Notifications",
        other,
        "You're all caught up 🎉",
        filter,
        inflight_done,
        true,
    ));

    actions
}

// -----------------------------------------------------------------------------
// Font configuration
// -----------------------------------------------------------------------------

fn install_international_fonts(ctx: &Context) {
    // CJK reviewers reported tofu glyphs because egui's built-in Latin fonts
    // do not cover Hangul. Prefer system-provided CJK families to avoid bloating
    // the binary, but fall back to the bundled font when the optional
    let Some(font_data) = resolve_cjk_font_data() else {
        eprintln!("Warning: no CJK-capable font found; Some glyphs may fail to render.");
        return;
    };

    let mut definitions = FontDefinitions::default();
    definitions
        .font_data
        .insert(CJK_FONT_NAME.to_owned(), font_data.into());

    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        definitions
            .families
            .entry(family)
            .or_default()
            .insert(0, CJK_FONT_NAME.to_owned());
    }

    ctx.set_fonts(definitions);
}

fn resolve_cjk_font_data() -> Option<FontData> {
    load_system_cjk_font()
}

fn load_system_cjk_font() -> Option<FontData> {
    for candidate in SYSTEM_FONT_CANDIDATES {
        if let Ok(bytes) = fs::read(candidate) {
            return Some(FontData::from_owned(bytes));
        }
    }
    None
}

impl App for ReminderApp {
    fn update(&mut self, ctx: &Context, _frame: &mut Frame) {
        self.poll_jobs();
        self.maybe_auto_refresh();

        egui::SidePanel::left("accounts_panel")
            .default_width(260.0)
            .show(ctx, |ui| self.render_side_panel(ui));

        egui::CentralPanel::default().show(ctx, |ui| {
            self.render_dashboard(ui);
        });

        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

// -----------------------------------------------------------------------------
// Account state & background jobs
// -----------------------------------------------------------------------------

struct AccountState {
    profile: GitHubAccount,
    inbox: Option<InboxSnapshot>,
    last_error: Option<String>,
    pending_job: Option<PendingJob>,
    pending_actions: Vec<NotificationActionJob>,
    expanded: bool,
    search_query: String,
    inflight_done: HashSet<String>,
}

impl AccountState {
    fn new(profile: GitHubAccount) -> Self {
        Self {
            profile,
            inbox: None,
            last_error: None,
            pending_job: None,
            pending_actions: Vec::new(),
            expanded: true,
            search_query: String::new(),
            inflight_done: HashSet::new(),
        }
    }

    fn start_refresh(&mut self) {
        let profile = self.profile.clone();
        self.last_error = None;
        self.pending_job = Some(PendingJob::spawn(profile));
    }

    fn poll_job(&mut self) {
        if let Some(job) = &mut self.pending_job {
            if let Some(result) = job.try_take() {
                self.pending_job = None;
                match result {
                    Ok(inbox) => {
                        self.inbox = Some(inbox);
                        self.last_error = None;
                    }
                    Err(err) => {
                        self.last_error = Some(err.to_string());
                    }
                }
            }
        }
    }

    fn poll_action_jobs(&mut self) {
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
                Ok(thread_id) => self.handle_action_success(&thread_id),
                Err((thread_id, err)) => {
                    self.last_error = Some(err);
                    if let Some(id) = thread_id {
                        self.inflight_done.remove(&id);
                    }
                }
            }
        }
    }

    fn handle_action_success(&mut self, thread_id: &str) {
        if let Some(inbox) = &mut self.inbox {
            if let Some(item) = inbox
                .notifications
                .iter_mut()
                .find(|item| item.thread_id == thread_id)
            {
                item.unread = false;
                // Consider the thread freshly read at the current timestamp so the
                // "Updated" badge clears unless new events arrive.
                item.last_read_at = Some(Utc::now());
            }
        }
        self.inflight_done.remove(thread_id);
    }

    /// Mark a thread as seen the moment the user opens it so the UI reflects
    /// the visit without waiting for the next GitHub sync cycle.
    fn mark_notification_seen(&mut self, thread_id: &str) {
        if let Some(inbox) = &mut self.inbox {
            if let Some(item) = inbox
                .notifications
                .iter_mut()
                .find(|item| item.thread_id == thread_id)
            {
                item.unread = false;
                // Advance the local last_read_at to the newest update to clear the
                // "Updated" badge unless more activity arrives later.
                item.last_read_at = Some(item.updated_at);
            }
        }
    }

    fn request_mark_read(&mut self, thread_id: String) {
        if self.inflight_done.contains(&thread_id) {
            return;
        }
        let profile = self.profile.clone();
        let job = NotificationActionJob::mark_read(profile, thread_id.clone());
        self.pending_actions.push(job);
        self.inflight_done.insert(thread_id);
    }

    fn request_mark_done(&mut self, thread_id: String) {
        if self.inflight_done.contains(&thread_id) {
            return;
        }
        let profile = self.profile.clone();
        let job = NotificationActionJob::mark_done(profile, thread_id.clone());
        self.pending_actions.push(job);
        self.inflight_done.insert(thread_id);
    }

    fn needs_refresh(&self, threshold: Duration) -> bool {
        match &self.inbox {
            None => true,
            Some(inbox) => match chrono::Duration::from_std(threshold) {
                Ok(delta) => (Utc::now() - inbox.fetched_at) >= delta,
                Err(_) => true,
            },
        }
    }
}

struct PendingJob {
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

type NotificationActionResult = Result<String, (Option<String>, String)>;

struct NotificationActionJob {
    receiver: Receiver<NotificationActionResult>,
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
        Ok(thread_id)
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
        Ok(thread_id)
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

// -----------------------------------------------------------------------------
// UI helpers
// -----------------------------------------------------------------------------

fn render_notification_section<'a>(
    group: &mut egui::Ui,
    title: &str,
    subset: Vec<&'a NotificationItem>,
    empty_label: &'static str,
    filter: &SearchFilter,
    inflight_done: &HashSet<String>,
    allow_done_action: bool,
) -> Vec<AccountAction> {
    let (unseen_count, updated_count) = summarize_counts(&subset);
    let heading = format!(
        "{title} ({} unseen, {} updated)",
        unseen_count, updated_count
    );
    let header = egui::CollapsingHeader::new(heading)
        // Keep the collapsing state stable even as counts in the title change.
        .id_salt(format!("notification-section-{title}"))
        .default_open(true);

    if subset.is_empty() {
        header.show(group, |section| {
            section.weak(empty_label);
        });
        return Vec::new();
    }

    let mut actions = Vec::new();
    header.show(group, |section| {
        actions.extend(draw_notifications(
            section,
            &subset,
            filter,
            inflight_done,
            allow_done_action,
        ));
    });
    actions
}

fn summarize_counts(items: &[&NotificationItem]) -> (usize, usize) {
    let mut unseen = 0;
    let mut updated = 0;
    for item in items {
        let visual = notification_state(item);
        if item.unread {
            unseen += 1;
        }
        if visual.needs_revisit {
            updated += 1;
        }
    }
    (unseen, updated)
}

// Highlight notifications that churned after the last time we read the thread so
// they do not silently blend into the "seen" palette. GitHub surfaces
// `last_read_at` alongside `unread`, but clients may set `unread` to false while a
// thread continues to evolve.
#[derive(Clone, Copy)]
struct NotificationVisualState {
    seen: bool,
    needs_revisit: bool,
}

fn notification_state(item: &NotificationItem) -> NotificationVisualState {
    let needs_revisit = item
        .last_read_at
        .map(|last_read| item.updated_at > last_read)
        .unwrap_or(false);

    NotificationVisualState {
        // A thread counts as "seen" only if GitHub marks it read and no updates
        // landed after that read timestamp.
        seen: !item.unread && !needs_revisit,
        needs_revisit,
    }
}

fn notification_text(
    ui: &egui::Ui,
    text: impl Into<String>,
    visual: NotificationVisualState,
) -> RichText {
    let mut content = RichText::new(text.into());
    if visual.needs_revisit {
        content = content.color(ui.visuals().warn_fg_color);
    } else if visual.seen {
        content = content.color(ui.visuals().weak_text_color());
    }
    content
}

fn draw_notifications(
    ui: &mut egui::Ui,
    items: &[&NotificationItem],
    filter: &SearchFilter,
    inflight_done: &HashSet<String>,
    allow_done_action: bool,
) -> Vec<AccountAction> {
    let mut actions = Vec::new();
    let rows: Vec<_> = items
        .iter()
        .copied()
        .filter(|item| filter.matches_any(&[&item.repo, &item.title, &item.reason]))
        .collect();
    if rows.is_empty() {
        ui.weak("No matches for current search.");
        return actions;
    }

    egui::ScrollArea::horizontal()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            TableBuilder::new(ui)
                .striped(true)
                .column(Column::initial(140.0).resizable(true))
                .column(Column::remainder().at_least(180.0))
                .column(Column::initial(170.0).resizable(true))
                .column(Column::initial(110.0))
                .header(20.0, |mut header| {
                    header.col(|ui| {
                        ui.strong("Repository");
                    });
                    header.col(|ui| {
                        ui.strong("Subject");
                    });
                    header.col(|ui| {
                        ui.strong("Updated");
                    });
                    header.col(|ui| {
                        ui.strong("Actions");
                    });
                })
                .body(|mut body| {
                    for item in rows {
                        let _thread_id = &item.thread_id;
                        let visual = notification_state(item);
                        body.row(24.0, |mut row| {
                            row.col(|ui| {
                                ui.label(notification_text(ui, &item.repo, visual));
                            });
                            row.col(|ui| {
                                ui.horizontal(|row_ui| {
                                    let subject = notification_text(row_ui, &item.title, visual);
                                    if let Some(url) = &item.url {
                                        let resp = row_ui.hyperlink_to(subject, url);
                                        if resp.clicked() {
                                            actions.push(AccountAction::MarkNotificationSeen(
                                                item.thread_id.clone(),
                                            ));
                                        }
                                    } else {
                                        let resp = row_ui.label(subject);
                                        if resp.clicked() {
                                            actions.push(AccountAction::MarkNotificationSeen(
                                                item.thread_id.clone(),
                                            ));
                                        }
                                    }
                                    if visual.needs_revisit {
                                        row_ui.small(
                                            RichText::new("Updated")
                                                .strong()
                                                .color(row_ui.visuals().warn_fg_color),
                                        );
                                    }
                                });
                                ui.small(notification_text(
                                    ui,
                                    format!("Reason: {}", &item.reason),
                                    visual,
                                ));
                            });
                            row.col(|ui| {
                                ui.label(notification_text(
                                    ui,
                                    item.updated_at.format("%Y-%m-%d %H:%M").to_string(),
                                    visual,
                                ));
                            });
                            row.col(|ui| {
                                let busy = inflight_done.contains(&item.thread_id);
                                let already_read = !item.unread && !visual.needs_revisit;

                                if ui
                                    .add_enabled(
                                        !busy && !already_read,
                                        egui::Button::new("Mark read"),
                                    )
                                    .clicked()
                                {
                                    actions.push(AccountAction::MarkNotificationRead(
                                        item.thread_id.clone(),
                                    ));
                                }

                                // Keep layout width consistent even when disabled.
                                if busy {
                                    ui.spinner();
                                }
                                let _ = allow_done_action;
                            });
                        });
                    }
                });
        });
    actions
}

// -----------------------------------------------------------------------------
// Supporting structs
// -----------------------------------------------------------------------------

#[allow(dead_code)]
enum AccountAction {
    MarkNotificationDone(String),
    MarkNotificationSeen(String),
    MarkNotificationRead(String),
}

#[derive(Default)]
struct AccountForm {
    login: String,
    token: String,
    form_error: Option<String>,
}

struct BatchRefreshScheduler {
    interval: Duration,
    last_run: Option<Instant>,
}

impl BatchRefreshScheduler {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_run: None,
        }
    }

    fn should_trigger(&self) -> bool {
        match self.last_run {
            None => true,
            Some(instant) => instant.elapsed() >= self.interval,
        }
    }

    fn mark_triggered(&mut self) {
        self.last_run = Some(Instant::now());
    }
}

// -----------------------------------------------------------------------------
// Search filtering
// -----------------------------------------------------------------------------

struct SearchFilter {
    needle: Option<String>,
}

impl SearchFilter {
    fn new(raw: &str) -> Self {
        let trimmed = raw.trim();
        let needle = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_lowercase())
        };
        Self { needle }
    }

    fn matches_any(&self, fields: &[&str]) -> bool {
        match &self.needle {
            None => true,
            Some(needle) => fields
                .iter()
                .any(|field| field.to_lowercase().contains(needle)),
        }
    }
}
