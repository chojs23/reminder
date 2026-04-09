mod fonts;
mod notification_state;
mod repo_paths;
mod repo_state;
mod review;
mod scheduler;
mod search;
mod state;
mod time;
mod ui;

use std::{collections::BTreeMap, fs, time::Duration};

use eframe::{
    App, CreationContext, Frame,
    egui::{self, Color32, Context},
};

use self::{
    fonts::install_international_fonts,
    repo_paths::{
        canonical_repo_key, normalize_hydrated_repo_path_accounts, normalize_hydrated_repo_paths,
    },
    repo_state::RepoState,
    review::{
        ReviewWindowAction, custom_review_command_available,
        default_pr_description_prompt_md_path_display, default_review_prompt_md_path_display,
        render_review_window, review_prompt_command_available,
    },
    scheduler::BatchRefreshScheduler,
    state::AccountState,
    ui::{
        account_overview, render_account_card, render_repository_card,
        render_tracked_account_badges, responsive_accounts_panel_width, tracked_account_heading,
        uses_compact_account_rows,
    },
};

use crate::{
    domain::{GitHubAccount, PullRequestReviewer, ReviewCommandSettings},
    storage::AccountStore,
};

pub const APP_NAME: &str = "Reminder";

pub const CJK_FONT_NAME: &str = "CJK_Fallback_Font";
const ACCOUNTS_PANEL_MIN_WIDTH: f32 = 140.0;
const ACCOUNTS_PANEL_MAX_WIDTH: f32 = 240.0;
const ACCOUNTS_PANEL_WIDTH_RATIO: f32 = 0.24;
const COMPACT_ACCOUNT_ROW_WIDTH: f32 = 180.0;
const STACKED_ACCOUNT_HEADER_WIDTH: f32 = 620.0;
const COMPACT_NOTIFICATION_WIDTH: f32 = 640.0;
const CUSTOM_REVIEW_COMMAND_NAME: &str = "review-pr";
const CUSTOM_PR_DESCRIPTION_COMMAND_NAME: &str = "pr-description";
#[cfg(test)]
const MAX_REVIEW_OUTPUT_CHARS: usize = 20_000;
const ACTIVE_REVIEW_REPAINT_MS: u64 = 50;
const REVIEW_REQUEST_REASON: &str = "review_requested";
const MENTION_REASONS: &[&str] = &["mention", "team_mention"];
const PENDING_REVIEW_LABEL_COLOR: Color32 = Color32::from_rgb(120, 200, 255);

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
    repo_path_form: RepoPathForm,
    account_delete_confirmation: Option<AccountDeleteConfirmation>,
    review_settings_editor: Option<AccountReviewSettingsEditor>,
    repo_path_account_editor: Option<RepoPathAccountEditor>,
    accounts: Vec<AccountState>,
    repo_paths: BTreeMap<String, String>,
    repo_path_accounts: BTreeMap<String, String>,
    repo_views: BTreeMap<String, RepoState>,
    selected_account_login: Option<String>,
    selected_repo: Option<String>,
    repo_path_filter_login: Option<String>,
    show_all_accounts: bool,
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
            repo_path_form: RepoPathForm::default(),
            account_delete_confirmation: None,
            review_settings_editor: None,
            repo_path_account_editor: None,
            accounts: Vec::new(),
            repo_paths: BTreeMap::new(),
            repo_path_accounts: BTreeMap::new(),
            repo_views: BTreeMap::new(),
            selected_account_login: None,
            selected_repo: None,
            repo_path_filter_login: None,
            show_all_accounts: true,
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
                        let (repo_paths, dropped_repo_paths) =
                            normalize_hydrated_repo_paths(outcome.repo_paths);
                        app.repo_paths = repo_paths;
                        let account_logins: Vec<_> = app
                            .accounts
                            .iter()
                            .map(|account| account.profile.login.clone())
                            .collect();
                        let (repo_path_accounts, dropped_repo_path_accounts) =
                            normalize_hydrated_repo_path_accounts(
                                outcome.repo_path_accounts,
                                &app.repo_paths,
                                &account_logins,
                            );
                        app.repo_path_accounts = repo_path_accounts;
                        if dropped_repo_paths > 0 || dropped_repo_path_accounts > 0 {
                            app.storage_warning = Some(format!(
                                "Skipped {} invalid local repo path setting(s) while restoring settings.",
                                dropped_repo_paths + dropped_repo_path_accounts
                            ));
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

        app.ensure_selected_account();
        app.auto_refresh.mark_triggered();

        app
    }

    fn add_account(&mut self) {
        if self.account_form.login.trim().is_empty() || self.account_form.token.trim().is_empty() {
            self.account_form.form_error =
                Some("Both the login and a Personal Access Token are required.".to_owned());
            return;
        }

        if self.accounts.iter().any(|account| {
            account
                .profile
                .login
                .eq_ignore_ascii_case(self.account_form.login.trim())
        }) {
            self.account_form.form_error =
                Some("This GitHub login is already being tracked.".to_owned());
            return;
        }

        let profile = GitHubAccount {
            login: self.account_form.login.trim().to_owned(),
            token: self.account_form.token.trim().to_owned(),
            review_settings: ReviewCommandSettings::default(),
        };
        let selected_login = profile.login.clone();

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
        self.selected_account_login = Some(selected_login);
        self.account_form = AccountForm::default();
    }

    fn remove_account_at(&mut self, idx: usize) {
        if idx >= self.accounts.len() {
            return;
        }

        let login = self.accounts[idx].profile.login.clone();
        let linked_repos: Vec<_> = self
            .repo_path_accounts
            .iter()
            .filter(|(_, linked_login)| *linked_login == &login)
            .map(|(repo, _)| repo.clone())
            .collect();
        if let Some(store) = &self.secret_store
            && let Err(err) = store.forget(&login)
        {
            self.global_error = Some(format!("Failed to remove credentials for {login}: {err}"));
            return;
        }
        if let Some(store) = &self.secret_store {
            for repo in &linked_repos {
                if let Err(err) = store.clear_repo_path_account(repo) {
                    self.global_error = Some(format!(
                        "Failed to clear repo path account for {repo}: {err}"
                    ));
                    return;
                }
            }
        }

        if self
            .review_settings_editor
            .as_ref()
            .is_some_and(|editor| editor.login == login)
        {
            self.review_settings_editor = None;
        }
        if self
            .account_delete_confirmation
            .as_ref()
            .is_some_and(|confirmation| confirmation.login == login)
        {
            self.account_delete_confirmation = None;
        }
        if self
            .repo_path_account_editor
            .as_ref()
            .is_some_and(|editor| editor.repo_login == login)
        {
            self.repo_path_account_editor = None;
        }

        self.accounts.remove(idx);
        for repo in linked_repos {
            self.repo_path_accounts.remove(&repo);
        }
        if self.repo_path_filter_login.as_deref() == Some(login.as_str()) {
            self.repo_path_filter_login = None;
        }
        self.ensure_selected_account();
    }

    fn open_account_delete_confirmation(&mut self, login: &str) {
        self.account_delete_confirmation = Some(AccountDeleteConfirmation {
            login: login.to_owned(),
            typed_login: String::new(),
        });
    }

    fn open_repo_path_account_editor(&mut self, repo: &str) {
        if !self.repo_paths.contains_key(repo) {
            self.global_error = Some(format!("Cannot find repo path for {repo}."));
            return;
        }

        self.repo_path_account_editor = Some(RepoPathAccountEditor {
            repo: repo.to_owned(),
            repo_login: self
                .repo_path_accounts
                .get(repo)
                .cloned()
                .unwrap_or_default(),
            form_error: None,
        });
    }

    fn assign_repo_path_account(&mut self, repo: &str, login: &str) {
        if !self.repo_paths.contains_key(repo) {
            self.global_error = Some(format!("Cannot find repo path for {repo}."));
            return;
        }
        let Some(account_login) = self
            .accounts
            .iter()
            .find(|account| account.profile.login == login)
            .map(|account| account.profile.login.clone())
        else {
            if let Some(editor) = &mut self.repo_path_account_editor {
                editor.form_error = Some(format!("Cannot find tracked account {login}."));
            }
            return;
        };

        if let Some(store) = &self.secret_store {
            if let Err(err) = store.persist_repo_path_account(repo, &account_login) {
                if let Some(editor) = &mut self.repo_path_account_editor {
                    editor.form_error = Some(format!("Unable to update repo path account: {err}"));
                }
                return;
            }
        } else if let Some(editor) = &mut self.repo_path_account_editor {
            editor.form_error = Some(
                "Local storage is not available; cannot update repo path accounts right now."
                    .to_owned(),
            );
            return;
        }

        self.repo_path_accounts
            .insert(repo.to_owned(), account_login.clone());
        self.repo_path_account_editor = None;

        if self.selected_repo.as_deref() == Some(repo) {
            self.selected_account_login = Some(account_login.clone());
            let repo = repo.to_owned();
            self.select_repo(repo);
        }
    }

    fn confirm_account_delete_matches(expected_login: &str, typed_login: &str) -> bool {
        typed_login.trim() == expected_login
    }

    fn remove_account_by_login(&mut self, login: &str) {
        if let Some(idx) = self
            .accounts
            .iter()
            .position(|account| account.profile.login == login)
        {
            self.remove_account_at(idx);
        } else {
            self.account_delete_confirmation = None;
            self.global_error = Some(format!("Cannot find tracked account {login} to remove."));
        }
    }

    fn save_repo_path(&mut self) {
        let Some(repo) = canonical_repo_key(&self.repo_path_form.repo) else {
            self.repo_path_form.form_error =
                Some("Repository must be in the form owner/repo.".to_owned());
            return;
        };

        let raw_path = self.repo_path_form.path.trim();
        if raw_path.is_empty() {
            self.repo_path_form.form_error = Some("A local checkout path is required.".to_owned());
            return;
        }

        let canonical_path = match fs::canonicalize(raw_path) {
            Ok(path) => path,
            Err(err) => {
                self.repo_path_form.form_error =
                    Some(format!("Unable to resolve checkout path: {err}"));
                return;
            }
        };

        if !canonical_path.is_dir() {
            self.repo_path_form.form_error =
                Some("Local checkout path must point to a directory.".to_owned());
            return;
        }

        let canonical_path = canonical_path.display().to_string();
        let linked_login = self.selected_account_login.clone().or_else(|| {
            self.accounts
                .first()
                .map(|account| account.profile.login.clone())
        });
        if linked_login.is_none() {
            self.repo_path_form.form_error =
                Some("Add at least one tracked account before saving a repo path.".to_owned());
            return;
        }

        if let Some(store) = &self.secret_store {
            if let Err(err) =
                store.persist_repo_path(&repo, &canonical_path, linked_login.as_deref())
            {
                self.repo_path_form.form_error =
                    Some(format!("Unable to persist local repo path: {err}"));
                return;
            }
        } else {
            self.repo_path_form.form_error = Some(
                "Local storage is not available; cannot save repo paths right now.".to_owned(),
            );
            return;
        }

        self.repo_paths.insert(repo.clone(), canonical_path);
        if let Some(login) = linked_login {
            self.repo_path_accounts.insert(repo, login);
        }
        self.repo_path_form = RepoPathForm::default();
    }

    fn open_review_settings_editor(&mut self, login: &str) {
        let Some(account) = self
            .accounts
            .iter()
            .find(|account| account.profile.login == login)
        else {
            self.global_error = Some(format!("Cannot find account settings for {login}."));
            return;
        };

        self.review_settings_editor = Some(AccountReviewSettingsEditor {
            login: account.profile.login.clone(),
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
    }

    fn save_review_settings(&mut self) {
        let Some(editor) = self.review_settings_editor.as_ref() else {
            return;
        };

        let env_vars = match parse_review_env_vars(&editor.env_vars_text) {
            Ok(env_vars) => env_vars,
            Err(err) => {
                if let Some(editor) = &mut self.review_settings_editor {
                    editor.form_error = Some(err);
                }
                return;
            }
        };

        let additional_args = parse_review_additional_args(&editor.additional_args_text);
        let login = editor.login.clone();
        let review_settings = ReviewCommandSettings {
            env_vars,
            additional_args,
            review_prompt_md_path: normalize_optional_path(&editor.review_prompt_md_path_text),
            pr_description_md_path: normalize_optional_path(&editor.pr_description_md_path_text),
        };

        let Some(account_idx) = self
            .accounts
            .iter()
            .position(|account| account.profile.login == login)
        else {
            self.review_settings_editor = None;
            self.global_error = Some(format!("Cannot find account settings for {login}."));
            return;
        };

        let mut profile = self.accounts[account_idx].profile.clone();
        profile.review_settings = review_settings.clone();

        if let Some(store) = &self.secret_store {
            if let Err(err) = store.persist_profile(&profile) {
                if let Some(editor) = &mut self.review_settings_editor {
                    editor.form_error = Some(format!("Unable to save review settings: {err}"));
                }
                return;
            }
        } else if let Some(editor) = &mut self.review_settings_editor {
            editor.form_error = Some(
                "Local storage is not available; cannot save review settings right now.".to_owned(),
            );
            return;
        }

        self.accounts[account_idx].profile.review_settings = review_settings;
        self.review_settings_editor = None;
    }

    fn render_review_settings_window(&mut self, ctx: &Context) {
        let Some(editor) = self.review_settings_editor.as_mut() else {
            return;
        };

        let mut open = true;
        let mut save_requested = false;
        let mut cancel_requested = false;
        let title = format!("Review settings: {}", editor.login);
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size(egui::vec2(520.0, 360.0))
            .show(ctx, |ui| {
                ui.label("Environment variables (one KEY=VALUE per line)");
                ui.add(
                    egui::TextEdit::multiline(&mut editor.env_vars_text)
                        .desired_rows(8)
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(8.0);
                ui.label("Additional args (split on whitespace)");
                ui.add(
                    egui::TextEdit::multiline(&mut editor.additional_args_text)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .hint_text("--lang korean"),
                );
                ui.add_space(8.0);
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

                if let Some(error) = &editor.form_error {
                    ui.add_space(8.0);
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }

                ui.add_space(12.0);
                ui.horizontal(|row| {
                    if row.button("Save").clicked() {
                        save_requested = true;
                    }
                    if row.button("Cancel").clicked() {
                        cancel_requested = true;
                    }
                });
            });

        if save_requested {
            self.save_review_settings();
        } else if cancel_requested || !open {
            self.review_settings_editor = None;
        }
    }

    fn render_account_delete_confirmation_window(&mut self, ctx: &Context) {
        let Some(confirmation) = self.account_delete_confirmation.as_mut() else {
            return;
        };

        let mut open = true;
        let mut delete_requested = false;
        let mut cancel_requested = false;
        let typed_matches =
            Self::confirm_account_delete_matches(&confirmation.login, &confirmation.typed_login);
        let title = format!("Delete account: {}", confirmation.login);
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_size(egui::vec2(420.0, 180.0))
            .show(ctx, |ui| {
                ui.label("Type the exact account name to confirm deletion.");
                ui.monospace(&confirmation.login);
                ui.add_space(8.0);
                ui.label("Account name");
                ui.add(
                    egui::TextEdit::singleline(&mut confirmation.typed_login)
                        .desired_width(f32::INFINITY),
                );
                if !typed_matches && !confirmation.typed_login.is_empty() {
                    ui.add_space(8.0);
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        "Typed account name does not match.",
                    );
                }
                ui.add_space(12.0);
                ui.horizontal(|row| {
                    if row
                        .add_enabled(typed_matches, egui::Button::new("Delete account"))
                        .clicked()
                    {
                        delete_requested = true;
                    }
                    if row.button("Cancel").clicked() {
                        cancel_requested = true;
                    }
                });
            });

        if delete_requested {
            let login = confirmation.login.clone();
            self.remove_account_by_login(&login);
        } else if cancel_requested || !open {
            self.account_delete_confirmation = None;
        }
    }

    fn render_repo_path_account_editor_window(&mut self, ctx: &Context) {
        let Some(editor) = self.repo_path_account_editor.as_mut() else {
            return;
        };

        let mut open = true;
        let mut selected_login = None;
        let mut cancel_requested = false;
        let title = format!("Repo path account: {}", editor.repo);
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_size(egui::vec2(360.0, 220.0))
            .show(ctx, |ui| {
                ui.label("Choose which tracked account this local repo path depends on.");
                if editor.repo_login.is_empty() {
                    ui.small("Current account: unassigned");
                } else {
                    ui.small(format!("Current account: {}", editor.repo_login));
                }
                ui.add_space(8.0);

                if self.accounts.is_empty() {
                    ui.weak("No tracked accounts available.");
                } else {
                    for account in &self.accounts {
                        if ui.button(&account.profile.login).clicked() {
                            selected_login = Some(account.profile.login.clone());
                        }
                    }
                }

                if let Some(error) = &editor.form_error {
                    ui.add_space(8.0);
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }

                ui.add_space(12.0);
                if ui.button("Cancel").clicked() {
                    cancel_requested = true;
                }
            });

        if let Some(login) = selected_login {
            let repo = editor.repo.clone();
            self.assign_repo_path_account(&repo, &login);
        } else if cancel_requested || !open {
            self.repo_path_account_editor = None;
        }
    }

    fn render_review_request_windows(&mut self, ctx: &Context) {
        for idx in 0..self.accounts.len() {
            let mut request_login = None;
            let mut renotify_login = None;
            let mut remove_login = None;
            let mut cancel_requested = false;

            {
                let account = &mut self.accounts[idx];
                let account_login = account.profile.login.clone();
                let Some(editor) = account.review_request_editor.as_mut() else {
                    continue;
                };

                let mut open = true;
                let title = format!("Request review: {}#{}", editor.repo, editor.pr_number);
                let current_reviewers = editor.current_reviewers.clone();
                let current_reviewer_logins: Vec<_> = current_reviewers
                    .iter()
                    .map(|reviewer| reviewer.login.clone())
                    .collect();
                let history_reviewers: Vec<_> = editor
                    .reviewer_history
                    .iter()
                    .filter(|login| {
                        !current_reviewer_logins
                            .iter()
                            .any(|current| current.eq_ignore_ascii_case(login))
                    })
                    .cloned()
                    .collect();
                let busy = editor.pending_load || editor.pending_action;

                egui::Window::new(title)
                    .open(&mut open)
                    .collapsible(false)
                    .resizable(true)
                    .default_size(egui::vec2(520.0, 360.0))
                    .show(ctx, |ui| {
                        ui.label(format!("Account: {account_login}"));
                        ui.small(editor.pr_title.as_str());
                        ui.add_space(8.0);

                        ui.label("Reviewer");
                        ui.add(
                            egui::TextEdit::singleline(&mut editor.reviewer_login)
                                .desired_width(f32::INFINITY)
                                .hint_text("octocat"),
                        );

                        if editor.pending_load {
                            ui.add_space(8.0);
                            ui.label("Loading reviewers...");
                        } else if editor.pending_action {
                            ui.add_space(8.0);
                            ui.label("Updating reviewers...");
                        }

                        if let Some(message) = &editor.status_message {
                            ui.add_space(8.0);
                            ui.colored_label(ui.visuals().hyperlink_color, message);
                        }
                        if let Some(error) = &editor.form_error {
                            ui.add_space(8.0);
                            ui.colored_label(ui.visuals().error_fg_color, error);
                        }

                        ui.add_space(12.0);
                        ui.separator();
                        ui.add_space(8.0);

                        ui.label("Current reviewers");
                        if current_reviewers.is_empty() {
                            ui.weak("No current reviewers yet.");
                        } else {
                            ui.horizontal_wrapped(|row| {
                                for reviewer in &current_reviewers {
                                    let status_label = reviewer.status.label();
                                    if row
                                        .small_button(format!(
                                            "{} {}",
                                            reviewer.login,
                                            reviewer.status.emoji()
                                        ))
                                        .on_hover_text(status_label)
                                        .clicked()
                                    {
                                        editor.reviewer_login = reviewer.login.clone();
                                    }
                                    let is_requested = editor
                                        .requested_reviewers
                                        .iter()
                                        .any(|login| login.eq_ignore_ascii_case(&reviewer.login));
                                    if row
                                        .add_enabled(
                                            !busy && is_requested && reviewer.status.is_pending(),
                                            egui::Button::new("🗑"),
                                        )
                                        .on_hover_text("Remove this reviewer request.")
                                        .clicked()
                                    {
                                        remove_login = Some(reviewer.login.clone());
                                    }
                                }
                            });
                        }

                        ui.add_space(8.0);
                        ui.label("History");
                        if history_reviewers.is_empty() {
                            ui.weak("No previous reviewers yet.");
                        } else {
                            ui.horizontal_wrapped(|row| {
                                for login in &history_reviewers {
                                    if row.small_button(login).clicked() {
                                        editor.reviewer_login = login.clone();
                                    }
                                    if row
                                        .add_enabled(!busy, egui::Button::new("+"))
                                        .on_hover_text("Request review from this reviewer again.")
                                        .clicked()
                                    {
                                        request_login = Some(login.clone());
                                    }
                                }
                            });
                        }

                        ui.add_space(12.0);
                        let reviewer_login = editor.reviewer_login.trim().to_owned();
                        let has_reviewer_login = !reviewer_login.is_empty();
                        ui.horizontal(|row| {
                            if row
                                .add_enabled(
                                    has_reviewer_login && !busy,
                                    egui::Button::new("Request"),
                                )
                                .clicked()
                            {
                                request_login = Some(reviewer_login.clone());
                            }
                            if row
                                .add_enabled(
                                    has_reviewer_login && !busy,
                                    egui::Button::new("Re-notify"),
                                )
                                .clicked()
                            {
                                renotify_login = Some(reviewer_login.clone());
                            }
                            if row.button("Cancel").clicked() {
                                cancel_requested = true;
                            }
                        });
                    });

                if !open {
                    cancel_requested = true;
                }
            }

            let account = &mut self.accounts[idx];
            if let Some(login) = remove_login {
                account.remove_pull_request_review(login);
            } else if let Some(login) = request_login {
                account.request_pull_request_review(login);
            } else if let Some(login) = renotify_login {
                account.renotify_pull_request_review(login);
            } else if cancel_requested {
                account.close_review_request_editor();
            }
        }
    }

    fn remove_repo_path(&mut self, repo: &str) {
        if let Some(store) = &self.secret_store
            && let Err(err) = store.forget_repo_path(repo)
        {
            self.global_error = Some(format!("Failed to remove repo path for {repo}: {err}"));
            return;
        }

        self.repo_paths.remove(repo);
        self.repo_path_accounts.remove(repo);
        self.repo_views.remove(repo);
        if self.selected_repo.as_deref() == Some(repo) {
            self.selected_repo = None;
        }
        if self
            .repo_path_account_editor
            .as_ref()
            .is_some_and(|editor| editor.repo == repo)
        {
            self.repo_path_account_editor = None;
        }
    }

    fn ensure_selected_account(&mut self) {
        let Some(selected_login) = self.selected_account_login.as_deref() else {
            self.selected_account_login = self
                .accounts
                .first()
                .map(|account| account.profile.login.clone());
            return;
        };

        if self
            .accounts
            .iter()
            .any(|account| account.profile.login == selected_login)
        {
            return;
        }

        self.selected_account_login = self
            .accounts
            .first()
            .map(|account| account.profile.login.clone());
    }

    fn ensure_selected_repo(&mut self) {
        let Some(selected_repo) = self.selected_repo.as_deref() else {
            return;
        };

        if self.repo_paths.contains_key(selected_repo) {
            return;
        }

        self.selected_repo = None;
    }

    fn poll_jobs(&mut self) {
        for account in &mut self.accounts {
            account.poll_job();
            account.poll_action_jobs();
            account.poll_review_job();
            account.poll_review_request_jobs();
        }
        for repo_view in self.repo_views.values_mut() {
            repo_view.poll_job();
        }
    }

    fn selected_account_index(&self) -> Option<usize> {
        let selected_login = self.selected_account_login.as_deref()?;
        self.accounts
            .iter()
            .position(|account| account.profile.login == selected_login)
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

        let Some(selected_repo) = self.selected_repo.clone() else {
            return;
        };
        let Some(selected_idx) = self.selected_account_index() else {
            return;
        };

        let profile = self.accounts[selected_idx].profile.clone();
        let repo_view = self
            .repo_views
            .entry(selected_repo.clone())
            .or_insert_with(|| RepoState::new(selected_repo));
        if repo_view.should_refresh_with(
            &profile.login,
            Duration::from_secs(AUTO_REFRESH_INTERVAL_SECS),
        ) {
            repo_view.start_refresh(profile);
            self.auto_refresh.mark_triggered();
        }
    }

    fn select_account(&mut self, login: String) {
        self.selected_account_login = Some(login);
        self.selected_repo = None;
        self.repo_path_filter_login = self.selected_account_login.clone();
        self.show_all_accounts = false;
    }

    fn show_all_repo_paths(&mut self) {
        self.repo_path_filter_login = None;
        self.selected_repo = None;
        self.show_all_accounts = true;
    }

    fn select_repo(&mut self, repo: String) {
        if !self.repo_paths.contains_key(&repo) {
            return;
        }

        if let Some(linked_login) = self.repo_path_accounts.get(&repo)
            && self
                .accounts
                .iter()
                .any(|account| account.profile.login == *linked_login)
        {
            self.selected_account_login = Some(linked_login.clone());
        }

        self.selected_repo = Some(repo.clone());
        self.show_all_accounts = false;
        self.ensure_selected_account();
        let Some(selected_idx) = self.selected_account_index() else {
            return;
        };

        let profile = self.accounts[selected_idx].profile.clone();
        let repo_view = self
            .repo_views
            .entry(repo.clone())
            .or_insert_with(|| RepoState::new(repo));
        if repo_view.should_refresh_with(
            &profile.login,
            Duration::from_secs(AUTO_REFRESH_INTERVAL_SECS),
        ) {
            repo_view.start_refresh(profile);
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
        ui.horizontal(|row| {
            row.label("Tracked accounts");
            let showing_all = self.repo_path_filter_login.is_none();
            if row.selectable_label(showing_all, "All").clicked() {
                self.show_all_repo_paths();
            }
        });
        if self.accounts.is_empty() {
            ui.weak("No accounts yet.");
        } else {
            let compact_rows = uses_compact_account_rows(ui.available_width());
            let mut selected_login = None;
            let mut refresh_idx = None;
            let mut remove_login = None;
            let mut settings_login = None;
            for (idx, account) in self.accounts.iter_mut().enumerate() {
                let overview = account_overview(account);
                let pending = account.pending_job.is_some();
                let has_error = account.last_error.is_some();
                let is_selected =
                    self.repo_path_filter_login.as_deref() == Some(account.profile.login.as_str());
                let heading = tracked_account_heading(ui, account, is_selected, overview);

                ui.group(|group| {
                    if group.selectable_label(is_selected, heading).clicked() {
                        selected_login = Some(account.profile.login.clone());
                    }

                    if compact_rows {
                        render_tracked_account_badges(group, overview, pending, has_error);
                        group.horizontal_wrapped(|row| {
                            if row.small_button("Refresh").clicked() {
                                refresh_idx = Some(idx);
                            }
                            if row.small_button("Settings").clicked() {
                                settings_login = Some(account.profile.login.clone());
                            }
                            if row.small_button("Remove").clicked() {
                                remove_login = Some(account.profile.login.clone());
                            }
                        });
                    } else {
                        group.horizontal_wrapped(|row| {
                            render_tracked_account_badges(row, overview, pending, has_error);
                            if row.small_button("Refresh").clicked() {
                                refresh_idx = Some(idx);
                            }
                            if row.small_button("Settings").clicked() {
                                settings_login = Some(account.profile.login.clone());
                            }
                            if row.small_button("Remove").clicked() {
                                remove_login = Some(account.profile.login.clone());
                            }
                        });
                    }
                });
            }
            if let Some(login) = selected_login {
                self.select_account(login);
            }
            if let Some(login) = settings_login {
                self.open_review_settings_editor(&login);
            }
            if let Some(login) = remove_login {
                self.open_account_delete_confirmation(&login);
            }
            if let Some(idx) = refresh_idx
                && let Some(account) = self.accounts.get_mut(idx)
            {
                account.start_refresh();
                self.auto_refresh.mark_triggered();
            }
        }

        ui.separator();
        ui.label("Local repo paths");
        if custom_review_command_available() {
            ui.small(
                "Mapped repos can use your custom `review-pr` opencode command. You can override the prompt path per account in Settings.",
            );
        } else {
            ui.small(
                "No default custom `review-pr` command detected. You can still set an override path per account in Settings.",
            );
        }
        ui.label("Repository (owner/repo)");
        ui.text_edit_singleline(&mut self.repo_path_form.repo);
        ui.label("Local checkout path");
        ui.text_edit_singleline(&mut self.repo_path_form.path);

        let save_repo_path_enabled = !self.repo_path_form.repo.trim().is_empty()
            && !self.repo_path_form.path.trim().is_empty();
        if ui
            .add_enabled(save_repo_path_enabled, egui::Button::new("Save repo path"))
            .clicked()
        {
            self.save_repo_path();
        }

        if let Some(error) = &self.repo_path_form.form_error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }

        if self.repo_paths.is_empty() {
            ui.weak("No local repos configured yet.");
        } else {
            let mut remove_repo = None;
            let mut selected_repo = None;
            let mut edit_repo = None;
            let visible_repos: Vec<_> = self
                .repo_paths
                .iter()
                .filter(|(repo, _)| {
                    self.repo_path_filter_login
                        .as_ref()
                        .is_none_or(|filter_login| {
                            self.repo_path_accounts.get(*repo) == Some(filter_login)
                        })
                })
                .collect();
            if visible_repos.is_empty() {
                if let Some(filter_login) = self.repo_path_filter_login.as_deref() {
                    ui.weak(format!("No local repos linked to {filter_login}."));
                } else {
                    ui.weak("No local repos configured yet.");
                }
            }
            for (repo, path) in visible_repos {
                let repo_state = self.repo_views.get(repo);
                let open_count = repo_state
                    .and_then(|repo_state| repo_state.snapshot.as_ref())
                    .map(|snapshot| snapshot.pull_requests.len());
                let pending = repo_state.is_some_and(|repo_state| repo_state.pending_job.is_some());
                let has_error =
                    repo_state.is_some_and(|repo_state| repo_state.last_error.is_some());
                let is_selected = self.selected_repo.as_deref() == Some(repo.as_str());
                let linked_login = self.repo_path_accounts.get(repo);
                ui.group(|group| {
                    if group.selectable_label(is_selected, repo).clicked() {
                        selected_repo = Some(repo.clone());
                    }
                    group.small(path);
                    if let Some(linked_login) = linked_login {
                        group.small(format!("Tracked account: {linked_login}"));
                    } else {
                        group.small(
                            egui::RichText::new("Tracked account: unassigned")
                                .color(group.visuals().weak_text_color()),
                        );
                    }
                    group.horizontal_wrapped(|row| {
                        if let Some(open_count) = open_count {
                            row.small(format!("open {open_count}"));
                        } else if pending {
                            row.small(
                                egui::RichText::new("Loading…")
                                    .color(row.visuals().weak_text_color()),
                            );
                        } else {
                            row.small(
                                egui::RichText::new("No data yet")
                                    .color(row.visuals().weak_text_color()),
                            );
                        }
                        if has_error {
                            row.small(
                                egui::RichText::new("sync failed")
                                    .color(row.visuals().error_fg_color),
                            );
                        }
                    });
                    group.horizontal(|row| {
                        row.with_layout(egui::Layout::right_to_left(egui::Align::Center), |row| {
                            if row.small_button("✏").clicked() {
                                edit_repo = Some(repo.clone());
                            }
                            if row.small_button("Remove path").clicked() {
                                remove_repo = Some(repo.clone());
                            }
                        });
                    });
                });
            }
            if let Some(repo) = selected_repo {
                self.select_repo(repo);
            }
            if let Some(repo) = edit_repo {
                self.open_repo_path_account_editor(&repo);
            }
            if let Some(repo) = remove_repo {
                self.remove_repo_path(&repo);
            }
        }
    }

    fn render_dashboard(&mut self, ui: &mut egui::Ui) {
        self.render_global_error(ui);

        if let Some(selected_repo) = self.selected_repo.clone() {
            if self.accounts.is_empty() {
                ui.centered_and_justified(|center| {
                    center
                        .label("Add at least one GitHub account to load repository pull requests.");
                });
                return;
            }

            let Some(selected_idx) = self.selected_account_index() else {
                ui.centered_and_justified(|center| {
                    center.label("Select an account on the left to load repository pull requests.");
                });
                return;
            };

            let repo_key = selected_repo.clone();
            let repo_paths = &self.repo_paths;
            let account = &mut self.accounts[selected_idx];
            let custom_review_command =
                review_prompt_command_available(&account.profile.review_settings);
            let repo_state = self
                .repo_views
                .entry(selected_repo.clone())
                .or_insert_with(|| RepoState::new(selected_repo));

            egui::ScrollArea::vertical().show(ui, |area| {
                area.push_id(format!("repo-{repo_key}"), |ui| {
                    render_repository_card(
                        ui,
                        account,
                        repo_state,
                        repo_paths,
                        custom_review_command,
                    );
                });
            });
            return;
        }

        if self.show_all_accounts {
            if self.accounts.is_empty() {
                ui.centered_and_justified(|center| {
                    center.label(
                        "Add at least one GitHub account to start aggregating notifications.",
                    );
                });
                return;
            }

            egui::ScrollArea::vertical().show(ui, |area| {
                area.push_id("all-accounts-dashboard", |ui| {
                    ui.group(|group| {
                        group.heading("Account: All");
                        group.small(format!(
                            "Showing notifications from {} tracked account(s).",
                            self.accounts.len()
                        ));
                    });
                    ui.add_space(12.0);

                    for account in &mut self.accounts {
                        account.clear_new_notifications();
                        let account_id = account.profile.login.clone();
                        let custom_review_command =
                            review_prompt_command_available(&account.profile.review_settings);
                        ui.push_id(account_id, |ui| {
                            render_account_card(
                                ui,
                                account,
                                &self.repo_paths,
                                custom_review_command,
                            );
                        });
                    }
                });
            });
            return;
        }

        if self.accounts.is_empty() {
            ui.centered_and_justified(|center| {
                center.label("Add at least one GitHub account to start aggregating notifications.");
            });
            return;
        }

        if self.selected_account_login.is_none() {
            ui.centered_and_justified(|center| {
                center.label("Select an account on the left to view notifications.");
            });
            return;
        }

        let Some(selected_idx) = self.selected_account_index() else {
            ui.centered_and_justified(|center| {
                center.label("Select an account on the left to view notifications.");
            });
            return;
        };

        egui::ScrollArea::vertical().show(ui, |area| {
            let account = &mut self.accounts[selected_idx];
            account.clear_new_notifications();
            let account_id = account.profile.login.clone();
            let custom_review_command =
                review_prompt_command_available(&account.profile.review_settings);
            area.push_id(account_id, |ui| {
                render_account_card(ui, account, &self.repo_paths, custom_review_command);
            });
        });
    }

    fn render_global_error(&self, ui: &mut egui::Ui) {
        if let Some(error) = &self.global_error {
            ui.colored_label(ui.visuals().error_fg_color, error);
            ui.add_space(8.0);
        }
    }
}

impl App for ReminderApp {
    fn update(&mut self, ctx: &Context, _frame: &mut Frame) {
        self.poll_jobs();
        self.ensure_selected_account();
        self.ensure_selected_repo();
        self.maybe_auto_refresh();

        let accounts_panel_width = responsive_accounts_panel_width(ctx.available_rect().width());

        egui::SidePanel::left("accounts_panel")
            .exact_width(accounts_panel_width)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.render_side_panel(ui));
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            self.render_dashboard(ui);
        });

        self.render_account_delete_confirmation_window(ctx);
        self.render_review_settings_window(ctx);
        self.render_repo_path_account_editor_window(ctx);
        self.render_review_request_windows(ctx);

        for account in &mut self.accounts {
            let mut review_window_actions = Vec::new();
            for review_output in account.review_outputs.values_mut() {
                if let Some(action) =
                    render_review_window(ctx, &account.profile.login, review_output)
                {
                    review_window_actions.push(action);
                }
            }

            for action in review_window_actions {
                match action {
                    ReviewWindowAction::SendFollowUp { thread_id } => {
                        account.request_review_follow_up(&thread_id)
                    }
                }
            }
        }

        let repaint_ms = if self.accounts.iter().any(AccountState::review_in_progress) {
            ACTIVE_REVIEW_REPAINT_MS
        } else {
            500
        };
        ctx.request_repaint_after(Duration::from_millis(repaint_ms));
    }
}

// -----------------------------------------------------------------------------
// Supporting structs
// -----------------------------------------------------------------------------

#[allow(dead_code)]
enum AccountAction {
    Done(String),
    Review {
        thread_id: String,
        repo: String,
        pr_number: u64,
        pr_url: String,
    },
    PrDescription {
        thread_id: String,
        repo: String,
        pr_number: u64,
        pr_url: String,
    },
    OpenReviewRequest {
        repo: String,
        pr_number: u64,
        pr_title: String,
    },
    StopReview(String),
    ToggleReviewWindow(String),
    Seen(String),
    Read(String),
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum SectionKind {
    Inbox,
    ReviewRequests,
    Mentions,
    Notifications,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccountViewMode {
    Inbox,
    Grouped,
}

#[derive(Default)]
struct AccountForm {
    login: String,
    token: String,
    form_error: Option<String>,
}

#[derive(Default)]
struct RepoPathForm {
    repo: String,
    path: String,
    form_error: Option<String>,
}

struct ReviewRequestEditor {
    repo: String,
    pr_number: u64,
    pr_title: String,
    reviewer_login: String,
    requested_reviewers: Vec<String>,
    current_reviewers: Vec<PullRequestReviewer>,
    reviewer_history: Vec<String>,
    pending_load: bool,
    pending_action: bool,
    form_error: Option<String>,
    status_message: Option<String>,
}

struct AccountReviewSettingsEditor {
    login: String,
    env_vars_text: String,
    additional_args_text: String,
    review_prompt_md_path_text: String,
    pr_description_md_path_text: String,
    form_error: Option<String>,
}

struct RepoPathAccountEditor {
    repo: String,
    repo_login: String,
    form_error: Option<String>,
}

struct AccountDeleteConfirmation {
    login: String,
    typed_login: String,
}

fn format_review_env_vars(settings: &ReviewCommandSettings) -> String {
    settings
        .env_vars
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_review_additional_args(settings: &ReviewCommandSettings) -> String {
    settings.additional_args.join(" ")
}

fn normalize_optional_path(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn parse_review_env_vars(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut env_vars = BTreeMap::new();

    for (idx, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        let Some((raw_key, raw_value)) = line.split_once('=') else {
            return Err(format!(
                "Environment variable line {} must be in KEY=VALUE form.",
                idx + 1
            ));
        };
        let key = raw_key.trim();
        let value = raw_value.trim();
        if key.is_empty() {
            return Err(format!(
                "Environment variable line {} must include a non-empty key.",
                idx + 1
            ));
        }
        if key.contains('=') || key.contains('\0') || key.chars().any(char::is_whitespace) {
            return Err(format!(
                "Environment variable line {} has an invalid key.",
                idx + 1
            ));
        }
        if value.contains('\0') {
            return Err(format!(
                "Environment variable line {} has an invalid value.",
                idx + 1
            ));
        }
        env_vars.insert(key.to_owned(), value.to_owned());
    }

    Ok(env_vars)
}

fn parse_review_additional_args(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, NaiveDateTime, Utc};
    use eframe::egui;
    use eframe::egui::collapsing_header::CollapsingState;
    use std::{collections::HashSet, time::Instant};

    use crate::domain::{InboxSnapshot, NotificationItem};

    use super::{
        notification_state::{
            SectionCounts, base_notification_state, collect_new_notification_ids,
            pending_review_request_ids, section_stats,
        },
        review::{
            ReviewLaunchPlan, ReviewStatus, append_review_chunk, format_review_failure_output,
            format_review_success_output, initial_review_output_state, resolve_review_launch,
            review_output_plain_text, review_summary_text, truncate_review_output,
        },
        search::SearchFilter,
        ui::{
            NotificationRenderState, notification_state, render_bucket_sections,
            responsive_accounts_panel_width, uses_compact_account_rows, uses_compact_notifications,
            uses_stacked_account_header,
        },
    };

    fn parse_utc(ts: &str) -> DateTime<Utc> {
        NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S")
            .unwrap()
            .and_utc()
    }

    fn notif(thread_id: &str, reason: &str, unread: bool, updated: &str) -> NotificationItem {
        NotificationItem {
            thread_id: thread_id.to_string(),
            repo: "acme/repo".into(),
            title: "Title".into(),
            url: None,
            reason: reason.into(),
            updated_at: parse_utc(updated),
            last_read_at: None,
            unread,
        }
    }

    fn inbox_with_notifications(notifications: Vec<NotificationItem>) -> InboxSnapshot {
        InboxSnapshot {
            notifications,
            review_requests: Vec::new(),
            mentions: Vec::new(),
            recent_reviews: Vec::new(),
            fetched_at: Utc::now(),
        }
    }

    fn notif_with_url(
        thread_id: &str,
        reason: &str,
        unread: bool,
        updated: &str,
        url: &str,
    ) -> NotificationItem {
        NotificationItem {
            url: Some(url.into()),
            ..notif(thread_id, reason, unread, updated)
        }
    }

    fn review_request(repo: &str, url: &str) -> crate::domain::ReviewRequest {
        crate::domain::ReviewRequest {
            _id: 1,
            repo: repo.into(),
            title: "#123 Review me".into(),
            url: url.into(),
            updated_at: parse_utc("2024-01-01 00:00:00"),
            requested_by: Some("octocat".into()),
        }
    }

    fn review_summary(repo: &str, url: &str) -> crate::domain::ReviewSummary {
        crate::domain::ReviewSummary {
            _id: 1,
            repo: repo.into(),
            title: "#123 Reviewed".into(),
            url: url.into(),
            updated_at: parse_utc("2024-01-01 00:00:00"),
            state: "open".into(),
        }
    }

    fn dummy_profile() -> GitHubAccount {
        GitHubAccount {
            login: "user".into(),
            token: "token".into(),
            review_settings: ReviewCommandSettings::default(),
        }
    }

    fn make_account(login: &str) -> AccountState {
        AccountState::new(GitHubAccount {
            login: login.into(),
            token: "token".into(),
            review_settings: ReviewCommandSettings::default(),
        })
    }

    fn app_with_accounts(logins: &[&str]) -> ReminderApp {
        ReminderApp {
            account_form: AccountForm::default(),
            repo_path_form: RepoPathForm::default(),
            account_delete_confirmation: None,
            review_settings_editor: None,
            repo_path_account_editor: None,
            accounts: logins.iter().map(|login| make_account(login)).collect(),
            repo_paths: BTreeMap::new(),
            repo_path_accounts: BTreeMap::new(),
            repo_views: BTreeMap::new(),
            selected_account_login: None,
            selected_repo: None,
            repo_path_filter_login: None,
            show_all_accounts: true,
            secret_store: None,
            storage_warning: None,
            global_error: None,
            auto_refresh: BatchRefreshScheduler::new(Duration::from_secs(1)),
        }
    }

    #[test]
    fn section_stats_groups_by_reason() {
        let inbox = inbox_with_notifications(vec![
            notif("1", "review_requested", true, "2024-01-01 00:00:00"),
            notif("2", "mention", true, "2024-01-01 00:00:00"),
            notif("3", "subscribed", false, "2024-01-01 00:00:00"),
        ]);
        let stats = section_stats(&inbox);
        assert_eq!(stats.inbox.unseen, 2);
        assert_eq!(stats.review_requests.unseen, 1);
        assert_eq!(stats.mentions.unseen, 1);
        assert_eq!(stats.notifications.unseen, 0);
    }

    #[test]
    fn new_accounts_start_in_inbox_view() {
        let account = AccountState::new(dummy_profile());
        assert_eq!(account.view_mode, AccountViewMode::Inbox);
    }

    #[test]
    fn section_counts_bumped_on_unseen_increase() {
        let old = SectionCounts::new(1, 0);
        let new = SectionCounts::new(2, 0);
        assert!(new.bumped_since(&old));
    }

    #[test]
    fn section_counts_not_bumped_when_same() {
        let old = SectionCounts::new(1, 1);
        let new = SectionCounts::new(1, 1);
        assert!(!new.bumped_since(&old));
    }

    #[test]
    fn search_filter_matches_case_insensitive() {
        let filter = SearchFilter::new("Repo");
        assert!(filter.matches_any(&["my/repo"]));
        assert!(!filter.matches_any(&["other/project"]));
    }

    #[test]
    fn batch_scheduler_triggers_after_interval() {
        let mut scheduler = BatchRefreshScheduler::new(Duration::from_secs(1));
        assert!(scheduler.should_trigger());
        scheduler.mark_triggered();
        scheduler.last_run = Some(Instant::now() - Duration::from_secs(2));
        assert!(scheduler.should_trigger());
    }

    #[test]
    fn accounts_panel_width_is_clamped() {
        assert_eq!(
            responsive_accounts_panel_width(300.0),
            ACCOUNTS_PANEL_MIN_WIDTH
        );
        assert_eq!(
            responsive_accounts_panel_width(2_000.0),
            ACCOUNTS_PANEL_MAX_WIDTH
        );
    }

    #[test]
    fn narrow_layout_helpers_switch_at_breakpoints() {
        assert!(uses_compact_account_rows(COMPACT_ACCOUNT_ROW_WIDTH - 1.0));
        assert!(uses_stacked_account_header(
            STACKED_ACCOUNT_HEADER_WIDTH - 1.0
        ));
        assert!(uses_compact_notifications(COMPACT_NOTIFICATION_WIDTH - 1.0));
        assert!(!uses_compact_notifications(
            COMPACT_NOTIFICATION_WIDTH + 1.0
        ));
    }

    #[test]
    fn ensure_selected_account_falls_back_to_first_account() {
        let mut app = app_with_accounts(&["alpha", "beta"]);
        app.selected_account_login = Some("missing".into());

        app.ensure_selected_account();

        assert_eq!(app.selected_account_login.as_deref(), Some("alpha"));
    }

    #[test]
    fn ensure_selected_repo_clears_missing_repo() {
        let mut app = app_with_accounts(&["alpha"]);
        app.selected_repo = Some(String::from("acme/repo"));

        app.ensure_selected_repo();

        assert!(app.selected_repo.is_none());
    }

    #[test]
    fn select_account_clears_repo_selection() {
        let mut app = app_with_accounts(&["alpha"]);
        app.selected_repo = Some(String::from("acme/repo"));
        app.show_all_accounts = true;

        app.select_account(String::from("alpha"));

        assert_eq!(app.selected_account_login.as_deref(), Some("alpha"));
        assert!(app.selected_repo.is_none());
        assert_eq!(app.repo_path_filter_login.as_deref(), Some("alpha"));
        assert!(!app.show_all_accounts);
    }

    #[test]
    fn show_all_repo_paths_clears_repo_path_filter() {
        let mut app = app_with_accounts(&["alpha"]);
        app.repo_path_filter_login = Some(String::from("alpha"));
        app.selected_repo = Some(String::from("acme/repo"));

        app.show_all_repo_paths();

        assert!(app.repo_path_filter_login.is_none());
        assert!(app.selected_repo.is_none());
        assert!(app.show_all_accounts);
    }

    #[test]
    fn selecting_repo_keeps_repo_path_filter_login() {
        let mut app = app_with_accounts(&["alpha"]);
        app.repo_paths
            .insert(String::from("acme/repo"), String::from("/tmp/acme-repo"));
        app.repo_path_accounts
            .insert(String::from("acme/repo"), String::from("alpha"));
        app.repo_path_filter_login = Some(String::from("alpha"));

        app.select_repo(String::from("acme/repo"));

        assert_eq!(app.repo_path_filter_login.as_deref(), Some("alpha"));
        assert!(!app.show_all_accounts);
    }

    #[test]
    fn confirm_account_delete_matches_trimmed_exact_login() {
        assert!(ReminderApp::confirm_account_delete_matches(
            "alpha", "alpha"
        ));
        assert!(ReminderApp::confirm_account_delete_matches(
            "alpha", " alpha "
        ));
        assert!(!ReminderApp::confirm_account_delete_matches(
            "alpha", "beta"
        ));
    }

    #[test]
    fn remove_account_at_clears_matching_delete_confirmation() {
        let mut app = app_with_accounts(&["alpha", "beta"]);
        app.account_delete_confirmation = Some(AccountDeleteConfirmation {
            login: String::from("alpha"),
            typed_login: String::from("alpha"),
        });

        app.remove_account_at(0);

        assert!(app.account_delete_confirmation.is_none());
    }

    #[test]
    fn collect_new_notification_ids_ignores_initial_sync() {
        let next =
            inbox_with_notifications(vec![notif("1", "subscribed", true, "2024-01-01 00:00:00")]);

        assert!(collect_new_notification_ids(None, &next).is_empty());
    }

    #[test]
    fn collect_new_notification_ids_detects_added_threads() {
        let previous = inbox_with_notifications(vec![
            notif("1", "subscribed", true, "2024-01-01 00:00:00"),
            notif("2", "mention", true, "2024-01-01 00:00:00"),
        ]);
        let next = inbox_with_notifications(vec![
            notif("2", "mention", true, "2024-01-01 00:00:00"),
            notif("3", "review_requested", true, "2024-01-01 00:00:00"),
        ]);

        let new_ids = collect_new_notification_ids(Some(&previous), &next);

        assert_eq!(new_ids.len(), 1);
        assert!(new_ids.contains("3"));
    }

    #[test]
    fn account_overview_reports_sidebar_counts() {
        let mut account = make_account("alpha");
        account.inbox = Some(inbox_with_notifications(vec![
            notif("1", "subscribed", true, "2024-01-01 00:00:00"),
            notif("2", "subscribed", false, "2024-01-02 00:00:00"),
        ]));
        account.new_notification_ids = [String::from("2"), String::from("3")].into_iter().collect();
        if let Some(inbox) = &mut account.inbox {
            inbox.notifications[1].last_read_at = Some(parse_utc("2024-01-01 00:00:00"));
        }

        let overview = account_overview(&account).expect("overview should exist");

        assert_eq!(overview.new_notifications, 2);
        assert_eq!(overview.unseen, 1);
        assert_eq!(overview.updated, 1);
    }

    #[test]
    fn clear_new_notifications_resets_sidebar_badge_state() {
        let mut account = make_account("alpha");
        account.new_notification_ids = [String::from("1")].into_iter().collect();

        account.clear_new_notifications();

        assert!(account.new_notification_ids.is_empty());
    }

    #[test]
    fn resolve_review_launch_prefers_custom_command_for_mapped_repo() {
        let repo_paths =
            BTreeMap::from([(String::from("acme/repo"), String::from("/tmp/acme-repo"))]);
        let review_settings = ReviewCommandSettings {
            env_vars: BTreeMap::from([(String::from("FOO"), String::from("bar"))]),
            additional_args: vec![String::from("--lang"), String::from("korean")],
            ..ReviewCommandSettings::default()
        };

        let launch = resolve_review_launch(
            &repo_paths,
            true,
            "Acme/Repo",
            123,
            &review_settings,
            "https://github.com/acme/repo/pull/123",
        );

        assert_eq!(
            launch,
            Some(ReviewLaunchPlan::Custom {
                repo: String::from("Acme/Repo"),
                repo_path: String::from("/tmp/acme-repo"),
                pr_number: 123,
                pr_url: String::from("https://github.com/acme/repo/pull/123"),
                review_settings,
            })
        );
    }

    #[test]
    fn resolve_review_launch_returns_none_without_repo_mapping() {
        let launch = resolve_review_launch(
            &BTreeMap::new(),
            true,
            "acme/repo",
            123,
            &ReviewCommandSettings::default(),
            "https://github.com/acme/repo/pull/123",
        );

        assert!(launch.is_none());
    }

    #[test]
    fn format_review_success_output_prefers_stdout_and_keeps_diagnostics() {
        let output = format_review_success_output("final review body", "warning details");

        assert!(output.starts_with("final review body"));
        assert!(output.contains("Diagnostics:\nwarning details"));
    }

    #[test]
    fn format_review_success_output_labels_stderr_only_as_diagnostics() {
        let output = format_review_success_output("", "warning details");

        assert_eq!(output, "Diagnostics:\nwarning details");
    }

    #[test]
    fn format_review_failure_output_avoids_repeating_streamed_stdout() {
        let output = format_review_failure_output("1", "partial stdout", "error stderr");

        assert!(output.contains("stderr:\nerror stderr"));
        assert!(!output.contains("partial stdout"));
    }

    #[test]
    fn truncate_review_output_adds_notice_when_content_is_large() {
        let text = "a".repeat(MAX_REVIEW_OUTPUT_CHARS + 5);

        let truncated = truncate_review_output(&text);

        assert!(truncated.contains("[truncated 5 trailing characters]"));
        assert!(truncated.starts_with(&"a".repeat(32)));
    }

    #[test]
    fn append_review_chunk_keeps_latest_tail_visible() {
        let mut review_output = initial_review_output_state(
            String::from("thread-1"),
            &ReviewLaunchPlan::Custom {
                repo: String::from("acme/repo"),
                repo_path: String::from("/tmp/acme-repo"),
                pr_number: 123,
                pr_url: String::from("https://github.com/acme/repo/pull/123"),
                review_settings: ReviewCommandSettings::default(),
            },
        );

        append_review_chunk(&mut review_output, "a".repeat(MAX_REVIEW_OUTPUT_CHARS));
        append_review_chunk(&mut review_output, "TAIL");

        assert_eq!(review_output.dropped_chars, 0);
        assert!(review_output_plain_text(&review_output).ends_with("TAIL"));
    }

    #[test]
    fn initial_review_output_state_tracks_thread_id_for_custom_reviews() {
        let review_output = initial_review_output_state(
            String::from("thread-42"),
            &ReviewLaunchPlan::Custom {
                repo: String::from("acme/repo"),
                repo_path: String::from("/tmp/acme-repo"),
                pr_number: 42,
                pr_url: String::from("https://github.com/acme/repo/pull/42"),
                review_settings: ReviewCommandSettings::default(),
            },
        );

        assert_eq!(review_output.thread_id, "thread-42");
        assert_eq!(review_output.command_label, "review-pr");
        assert_eq!(review_output.status, ReviewStatus::Running);
    }

    #[test]
    fn toggle_review_window_for_thread_only_toggles_matching_review() {
        let mut account = make_account("alpha");
        account.review_outputs.insert(
            String::from("thread-1"),
            initial_review_output_state(
                String::from("thread-1"),
                &ReviewLaunchPlan::Custom {
                    repo: String::from("acme/repo"),
                    repo_path: String::from("/tmp/acme-repo"),
                    pr_number: 123,
                    pr_url: String::from("https://github.com/acme/repo/pull/123"),
                    review_settings: ReviewCommandSettings::default(),
                },
            ),
        );

        account.toggle_review_window_for_thread("thread-2");
        assert!(
            account
                .review_outputs
                .get("thread-1")
                .is_some_and(|review_output| review_output.open)
        );

        account.toggle_review_window_for_thread("thread-1");
        assert!(
            account
                .review_outputs
                .get("thread-1")
                .is_some_and(|review_output| !review_output.open)
        );
    }

    #[test]
    fn active_review_thread_ids_returns_only_running_reviews() {
        let mut account = make_account("alpha");
        account.review_outputs.insert(
            String::from("thread-1"),
            initial_review_output_state(
                String::from("thread-1"),
                &ReviewLaunchPlan::Custom {
                    repo: String::from("acme/repo"),
                    repo_path: String::from("/tmp/acme-repo"),
                    pr_number: 123,
                    pr_url: String::from("https://github.com/acme/repo/pull/123"),
                    review_settings: ReviewCommandSettings::default(),
                },
            ),
        );
        account.review_outputs.insert(
            String::from("thread-2"),
            initial_review_output_state(
                String::from("thread-2"),
                &ReviewLaunchPlan::Custom {
                    repo: String::from("acme/repo"),
                    repo_path: String::from("/tmp/acme-repo"),
                    pr_number: 456,
                    pr_url: String::from("https://github.com/acme/repo/pull/456"),
                    review_settings: ReviewCommandSettings::default(),
                },
            ),
        );

        let active = account.active_review_thread_ids();
        assert!(active.contains("thread-1"));
        assert!(active.contains("thread-2"));

        if let Some(review_output) = account.review_outputs.get_mut("thread-2") {
            review_output.status = ReviewStatus::Cancelled;
        }

        let active = account.active_review_thread_ids();
        assert!(active.contains("thread-1"));
        assert!(!active.contains("thread-2"));
    }

    #[test]
    fn review_summary_text_reports_cancelled_status() {
        let mut review_output = initial_review_output_state(
            String::from("thread-1"),
            &ReviewLaunchPlan::Custom {
                repo: String::from("acme/repo"),
                repo_path: String::from("/tmp/acme-repo"),
                pr_number: 123,
                pr_url: String::from("https://github.com/acme/repo/pull/123"),
                review_settings: ReviewCommandSettings::default(),
            },
        );
        review_output.status = ReviewStatus::Cancelled;

        let summary = review_summary_text(&review_output);

        assert!(summary.starts_with("Review canceled:"));
    }

    #[test]
    fn parse_review_env_vars_parses_key_value_lines() {
        let env_vars = parse_review_env_vars("FOO=bar\nBAZ=qux").expect("env vars");

        assert_eq!(env_vars.get("FOO"), Some(&String::from("bar")));
        assert_eq!(env_vars.get("BAZ"), Some(&String::from("qux")));
    }

    #[test]
    fn parse_review_additional_args_splits_whitespace_tokens() {
        let args = parse_review_additional_args("--lang korean --mode fast");

        assert_eq!(args, vec!["--lang", "korean", "--mode", "fast"]);
    }

    #[test]
    fn notification_state_detects_revisit() {
        let mut item = notif("1", "subscribed", false, "2024-01-02 00:00:00");
        item.last_read_at = Some(parse_utc("2024-01-01 00:00:00"));
        let visual = base_notification_state(&item);
        assert!(visual.needs_revisit);
        assert!(!visual.seen);
        assert!(!visual.pending_review);
    }

    #[test]
    fn pending_review_request_ids_marks_open_requests_without_my_review() {
        let pr_url = "https://github.com/acme/repo/pull/123";
        let inbox = InboxSnapshot {
            notifications: vec![notif_with_url(
                "1",
                "review_requested",
                true,
                "2024-01-01 00:00:00",
                pr_url,
            )],
            review_requests: vec![review_request("acme/repo", pr_url)],
            mentions: Vec::new(),
            recent_reviews: Vec::new(),
            fetched_at: Utc::now(),
        };

        let pending = pending_review_request_ids(&inbox);

        assert!(pending.contains("1"));
    }

    #[test]
    fn pending_review_request_ids_highlights_matching_pr_notifications_even_with_other_reason() {
        let pr_url = "https://github.com/acme/repo/pull/123";
        let inbox = InboxSnapshot {
            notifications: vec![notif_with_url(
                "1",
                "subscribed",
                true,
                "2024-01-01 00:00:00",
                pr_url,
            )],
            review_requests: vec![review_request("acme/repo", pr_url)],
            mentions: Vec::new(),
            recent_reviews: Vec::new(),
            fetched_at: Utc::now(),
        };

        let pending = pending_review_request_ids(&inbox);

        assert!(pending.contains("1"));
    }

    #[test]
    fn pending_review_request_ids_skips_prs_i_already_reviewed() {
        let pr_url = "https://github.com/acme/repo/pull/123";
        let inbox = InboxSnapshot {
            notifications: vec![notif_with_url(
                "1",
                "review_requested",
                true,
                "2024-01-01 00:00:00",
                pr_url,
            )],
            review_requests: vec![review_request("acme/repo", pr_url)],
            mentions: Vec::new(),
            recent_reviews: vec![review_summary("acme/repo", pr_url)],
            fetched_at: Utc::now(),
        };

        let pending = pending_review_request_ids(&inbox);

        assert!(pending.is_empty());
    }

    #[test]
    fn notification_state_marks_pending_review_requests_from_render_state() {
        let item = notif_with_url(
            "1",
            "review_requested",
            true,
            "2024-01-01 00:00:00",
            "https://github.com/acme/repo/pull/123",
        );
        let inflight_done = HashSet::new();
        let pending_review_ids = [String::from("1")].into_iter().collect();
        let active_review_thread_ids = HashSet::new();
        let review_output_thread_ids = HashSet::new();
        let open_review_window_thread_ids = HashSet::new();
        let render_state = NotificationRenderState {
            inflight_done: &inflight_done,
            pending_review_ids: &pending_review_ids,
            active_review_thread_ids: &active_review_thread_ids,
            review_output_thread_ids: &review_output_thread_ids,
            open_review_window_thread_ids: &open_review_window_thread_ids,
            custom_review_command: false,
            repo_paths: &BTreeMap::new(),
        };

        let visual = notification_state(&item, &render_state);

        assert!(visual.pending_review);
    }

    #[test]
    fn highlight_clears_after_rendering_section() {
        let ctx = egui::Context::default();
        let mut account = AccountState::new(dummy_profile());
        account.inbox = Some(inbox_with_notifications(vec![notif(
            "t1",
            "subscribed",
            true,
            "2024-01-01 00:00:00",
        )]));
        account.highlights.insert(SectionKind::Notifications);
        let filter = SearchFilter::new("");

        ctx.begin_pass(Default::default());
        egui::CentralPanel::default().show(&ctx, |ui| {
            let _ = render_bucket_sections(ui, &mut account, &filter, &BTreeMap::new(), false);
        });
        let _ = ctx.end_pass();

        assert!(
            !account.highlights.contains(&SectionKind::Notifications),
            "Highlight should clear after section is rendered"
        );
    }

    #[test]
    fn collapsing_header_state_persists_across_frames() {
        let ctx = egui::Context::default();
        let mut account = AccountState::new(dummy_profile());
        account.inbox = Some(inbox_with_notifications(vec![notif(
            "t1",
            "subscribed",
            true,
            "2024-01-01 00:00:00",
        )]));
        let filter = SearchFilter::new("");

        // Frame 1: render and manually collapse the notifications section.
        ctx.begin_pass(Default::default());
        egui::CentralPanel::default().show(&ctx, |ui| {
            let _ = render_bucket_sections(ui, &mut account, &filter, &BTreeMap::new(), false);
        });
        let id = egui::Id::new("notification-section-Notifications");
        let mut state = CollapsingState::load_with_default_open(&ctx, id, true);
        state.set_open(false);
        state.store(&ctx);
        let _ = ctx.end_pass();

        // Frame 2: re-render; section should remain collapsed because ID is stable.
        ctx.begin_pass(Default::default());
        let mut stayed_collapsed = true;
        egui::CentralPanel::default().show(&ctx, |ui| {
            let response =
                render_bucket_sections(ui, &mut account, &filter, &BTreeMap::new(), false);
            let state = CollapsingState::load_with_default_open(ui.ctx(), id, true);
            stayed_collapsed = !state.is_open();
            assert!(response.is_empty(), "Rendering should not trigger actions");
        });
        let _ = ctx.end_pass();

        assert!(
            stayed_collapsed,
            "Collapse state should persist across frames"
        );
    }
}
