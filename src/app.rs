use std::{
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use chrono::Utc;
use eframe::{
    App, CreationContext, Frame,
    egui::{self, Context, FontData, FontDefinitions, FontFamily},
};
use egui_extras::{Column, TableBuilder};

use crate::{
    domain::{
        GitHubAccount, InboxSnapshot, MentionThread, NotificationItem, ReviewRequest, ReviewSummary,
    },
    github::{self, FetchError},
    storage::AccountStore,
};

pub const APP_NAME: &str = "GitHub Review Hub";
const KOREAN_FONT_NAME: &str = "noto_sans_kr";
const KOREAN_FONT_BYTES: &[u8] = include_bytes!("../assets/NotoSansKR-Regular.otf");
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
        if let Some(store) = &self.secret_store {
            if let Err(err) = store.forget(&login) {
                self.global_error =
                    Some(format!("Failed to remove credentials for {login}: {err}"));
            }
        }

        self.accounts.remove(idx);
    }

    fn poll_jobs(&mut self) {
        for account in &mut self.accounts {
            account.poll_job();
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
        if let Some(error) = &self.global_error {
            ui.colored_label(ui.visuals().error_fg_color, error);
            ui.add_space(8.0);
        }

        if self.accounts.is_empty() {
            ui.centered_and_justified(|center| {
                center.label("Add at least one GitHub account to start aggregating notifications.");
            });
            return;
        }

        egui::ScrollArea::vertical().show(ui, |area| {
            for account in &mut self.accounts {
                area.push_id(&account.profile.login, |ui| {
                    ui.group(|group| {
                        group.horizontal(|row| {
                            row.heading(format!("Account: {}", account.profile.login));
                            let toggle_label = if account.expanded {
                                "Hide notifications"
                            } else {
                                "Show notifications"
                            };
                            if row.small_button(toggle_label).clicked() {
                                account.expanded = !account.expanded;
                            }
                        });
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

                        if account.expanded {
                            if let Some(inbox) = &account.inbox {
                                group.separator();
                                group.label("Unanswered review requests");
                                draw_review_requests(group, &inbox.review_requests);
                                group.separator();
                                group.label("Mentions");
                                draw_mentions(group, &inbox.mentions);
                                group.separator();
                                group.label("Recently reviewed pull requests");
                                draw_recent_reviews(group, &inbox.recent_reviews);
                                group.separator();
                                group.label("Notifications");
                                draw_notifications(group, &inbox.notifications);
                            }
                        } else if account.inbox.is_none() {
                            group.separator();
                            group.weak("No data loaded yet.");
                        }
                    });
                    ui.add_space(12.0);
                });
            }
        });
    }
}

// -----------------------------------------------------------------------------
// Font configuration
// -----------------------------------------------------------------------------

fn install_international_fonts(ctx: &Context) {
    // Korean reviewers reported tofu glyphs because egui's built-in Latin fonts
    // do not cover Hangul. Ship a bundled Noto Sans KR face and put it at the
    // front of each family so every widget can render those glyphs.
    let mut definitions = FontDefinitions::default();
    definitions.font_data.insert(
        KOREAN_FONT_NAME.to_owned(),
        FontData::from_static(KOREAN_FONT_BYTES).into(),
    );

    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        definitions
            .families
            .entry(family)
            .or_default()
            .insert(0, KOREAN_FONT_NAME.to_owned());
    }

    ctx.set_fonts(definitions);
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
    expanded: bool,
}

impl AccountState {
    fn new(profile: GitHubAccount) -> Self {
        Self {
            profile,
            inbox: None,
            last_error: None,
            pending_job: None,
            expanded: true,
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

// -----------------------------------------------------------------------------
// UI helpers
// -----------------------------------------------------------------------------

fn draw_notifications(ui: &mut egui::Ui, items: &[NotificationItem]) {
    if items.is_empty() {
        ui.weak("You're all caught up 🎉");
        return;
    }

    TableBuilder::new(ui)
        .striped(true)
        .column(Column::initial(120.0).resizable(true))
        .column(Column::remainder())
        .column(Column::initial(150.0))
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
        })
        .body(|mut body| {
            for item in items {
                body.row(24.0, |mut row| {
                    row.col(|ui| {
                        ui.label(&item.repo);
                    });
                    row.col(|ui| {
                        if let Some(url) = &item.url {
                            ui.hyperlink_to(&item.title, url);
                        } else {
                            ui.label(&item.title);
                        }
                        ui.small(format!("Reason: {}", &item.reason));
                    });
                    row.col(|ui| {
                        ui.label(item.updated_at.format("%Y-%m-%d %H:%M").to_string());
                    });
                });
            }
        });
}

fn draw_review_requests(ui: &mut egui::Ui, items: &[ReviewRequest]) {
    if items.is_empty() {
        ui.weak("No pending review requests.");
        return;
    }

    ui.push_id("review_requests_table", |ui| {
        TableBuilder::new(ui)
            .striped(true)
            .column(Column::initial(120.0).resizable(true))
            .column(Column::remainder())
            .column(Column::initial(150.0))
            .header(20.0, |mut header| {
                header.col(|ui| {
                    ui.strong("Repository");
                });
                header.col(|ui| {
                    ui.strong("Pull request");
                });
                header.col(|ui| {
                    ui.strong("Updated");
                });
            })
            .body(|mut body| {
                for item in items {
                    body.row(24.0, |mut row| {
                        row.col(|ui| {
                            ui.label(&item.repo);
                        });
                        row.col(|ui| {
                            ui.hyperlink_to(&item.title, &item.url);
                            if let Some(requester) = &item.requested_by {
                                ui.small(format!("Requested by {}", requester));
                            }
                        });
                        row.col(|ui| {
                            ui.label(item.updated_at.format("%Y-%m-%d %H:%M").to_string());
                        });
                    });
                }
            });
    });
}

fn draw_mentions(ui: &mut egui::Ui, items: &[MentionThread]) {
    if items.is_empty() {
        ui.weak("No recent mentions.");
        return;
    }

    ui.push_id("mentions_table", |ui| {
        TableBuilder::new(ui)
            .striped(true)
            .column(Column::initial(70.0))
            .column(Column::initial(140.0).resizable(true))
            .column(Column::remainder())
            .column(Column::initial(150.0))
            .header(20.0, |mut header| {
                header.col(|ui| {
                    ui.strong("Type");
                });
                header.col(|ui| {
                    ui.strong("Repository");
                });
                header.col(|ui| {
                    ui.strong("Thread");
                });
                header.col(|ui| {
                    ui.strong("Updated");
                });
            })
            .body(|mut body| {
                for item in items {
                    body.row(24.0, |mut row| {
                        row.col(|ui| {
                            ui.label(item.kind.label());
                        });
                        row.col(|ui| {
                            ui.label(&item.repo);
                        });
                        row.col(|ui| {
                            ui.hyperlink_to(&item.title, &item.url);
                        });
                        row.col(|ui| {
                            ui.label(item.updated_at.format("%Y-%m-%d %H:%M").to_string());
                        });
                    });
                }
            });
    });
}

fn draw_recent_reviews(ui: &mut egui::Ui, items: &[ReviewSummary]) {
    if items.is_empty() {
        ui.weak("No recently completed reviews.");
        return;
    }

    ui.push_id("recent_reviews_table", |ui| {
        TableBuilder::new(ui)
            .striped(true)
            .column(Column::initial(140.0).resizable(true))
            .column(Column::remainder())
            .column(Column::initial(90.0))
            .column(Column::initial(150.0))
            .header(20.0, |mut header| {
                header.col(|ui| {
                    ui.strong("Repository");
                });
                header.col(|ui| {
                    ui.strong("Pull request");
                });
                header.col(|ui| {
                    ui.strong("State");
                });
                header.col(|ui| {
                    ui.strong("Updated");
                });
            })
            .body(|mut body| {
                for item in items {
                    body.row(24.0, |mut row| {
                        row.col(|ui| {
                            ui.label(&item.repo);
                        });
                        row.col(|ui| {
                            ui.hyperlink_to(&item.title, &item.url);
                        });
                        row.col(|ui| {
                            ui.label(item.state.as_str());
                        });
                        row.col(|ui| {
                            ui.label(item.updated_at.format("%Y-%m-%d %H:%M").to_string());
                        });
                    });
                }
            });
    });
}

// -----------------------------------------------------------------------------
// Supporting structs
// -----------------------------------------------------------------------------

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
