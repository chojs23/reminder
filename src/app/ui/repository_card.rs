use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use eframe::egui::{self, Layout, RichText};
use egui_extras::{Column, TableBuilder};

use crate::domain::{PullRequestKey, RepoPullRequest};

use super::{
    super::{
        AccountAction,
        notification_state::{base_notification_state, is_review_request},
        repo_state::{RepoSortMode, RepoState},
        review::{
            custom_review_available_for_repo, pr_description_command_available,
            resolve_pr_description_launch, resolve_review_launch, review_summary_text,
        },
        search::SearchFilter,
        state::AccountState,
        time::format_local_timestamp,
    },
    layout::{uses_compact_notifications, uses_stacked_account_header},
};

pub(in crate::app) fn render_repository_card(
    ui: &mut egui::Ui,
    account: &mut AccountState,
    repo_state: &mut RepoState,
    repo_paths: &BTreeMap<String, String>,
    custom_review_command: bool,
) {
    ui.group(|group| {
        render_repository_header(group, account, repo_state);
        render_repository_status(group, account, repo_state);
        render_repository_body(
            group,
            account,
            repo_state,
            repo_paths,
            custom_review_command,
        );
    });
    ui.add_space(12.0);
}

fn render_repository_header(
    group: &mut egui::Ui,
    account: &AccountState,
    repo_state: &mut RepoState,
) {
    let mut refresh_requested = false;
    if uses_stacked_account_header(group.available_width()) {
        group.vertical(|column| {
            column.horizontal_wrapped(|row| {
                row.heading(format!("Repository: {}", repo_state.repo));
                if row.small_button("Refresh").clicked() {
                    refresh_requested = true;
                }
            });
            column.small(format!("Using account: {}", account.profile.login));
            let search_width = column.available_width();
            column.add(
                egui::TextEdit::singleline(&mut repo_state.search_query)
                    .hint_text("Search pull requests…")
                    .desired_width(search_width),
            );
            render_sort_mode_toggle(column, repo_state);
        });
    } else {
        group.vertical(|column| {
            column.horizontal(|row| {
                row.heading(format!("Repository: {}", repo_state.repo));
                if row.small_button("Refresh").clicked() {
                    refresh_requested = true;
                }
                row.with_layout(Layout::right_to_left(egui::Align::Center), |lane| {
                    lane.add(
                        egui::TextEdit::singleline(&mut repo_state.search_query)
                            .hint_text("Search pull requests…")
                            .desired_width(180.0),
                    );
                    lane.add_space(8.0);
                    lane.small(format!("Using account: {}", account.profile.login));
                });
            });
            render_sort_mode_toggle(column, repo_state);
        });
    }

    if refresh_requested {
        repo_state.start_refresh(account.profile.clone());
    }
}

fn render_sort_mode_toggle(ui: &mut egui::Ui, repo_state: &mut RepoState) {
    ui.horizontal_wrapped(|row| {
        row.small("Sort");
        row.radio_value(&mut repo_state.sort_mode, RepoSortMode::Default, "default")
            .on_hover_text("Keep the repository view's default order.");
        row.radio_value(
            &mut repo_state.sort_mode,
            RepoSortMode::ReviewRequest,
            "Review request",
        )
        .on_hover_text("Put the most recent active review requests first.");
        row.radio_value(&mut repo_state.sort_mode, RepoSortMode::Updated, "updated")
            .on_hover_text("Put unread or newly updated pull requests first.");
    });
}

fn render_repository_status(group: &mut egui::Ui, account: &AccountState, repo_state: &RepoState) {
    if let Some(snapshot) = &repo_state.snapshot {
        group.label(format!(
            "Last synced {}",
            format_local_timestamp(snapshot.fetched_at, "%Y-%m-%d %H:%M:%S %:z")
        ));
    } else {
        group.label("No pull requests loaded yet.");
    }

    if let Some(login) = repo_state.loaded_by_login.as_deref() {
        group.small(format!("Loaded with account: {login}"));
    }

    if let Some(err) = &repo_state.last_error {
        group.colored_label(group.visuals().error_fg_color, err);
    } else if repo_state.pending_job.is_some() {
        group.label("Fetching open pull requests...");
    }

    let repo_prefix = format!("{}#", repo_state.repo);
    for review_output in account
        .review_outputs
        .values()
        .filter(|review_output| review_output.target.starts_with(&repo_prefix))
    {
        let summary = review_summary_text(review_output);
        let dropped_chars = review_output.dropped_chars;
        group.horizontal_wrapped(|row| {
            row.label(summary);
            if dropped_chars > 0 {
                row.small(
                    RichText::new(format!("Trimmed {} chars", dropped_chars))
                        .color(row.visuals().warn_fg_color),
                );
            }
            if !review_output.open {
                row.small(RichText::new("Window hidden").color(row.visuals().weak_text_color()));
            }
        });
    }
}

fn render_repository_body(
    group: &mut egui::Ui,
    account: &mut AccountState,
    repo_state: &RepoState,
    repo_paths: &BTreeMap<String, String>,
    custom_review_command: bool,
) {
    group.separator();

    let Some(snapshot) = &repo_state.snapshot else {
        if repo_state.pending_job.is_none() {
            group.weak("Select this repo to load its open pull requests.");
        }
        return;
    };

    if let Some(path) = repo_paths.get(&repo_state.repo) {
        group.small(format!("Local path: {path}"));
        group.add_space(6.0);
    }

    let context = build_repo_context_info(account);
    let filter = SearchFilter::new(&repo_state.search_query);
    let mut matching_rows: Vec<_> = snapshot
        .pull_requests
        .iter()
        .filter(|pull_request| pull_request_matches_search(pull_request, &filter))
        .collect();
    sort_pull_requests(&mut matching_rows, repo_state.sort_mode, &context);

    let total_count = snapshot.pull_requests.len();
    group.strong(format!("Open pull requests ({total_count})"));
    group.add_space(6.0);

    if matching_rows.is_empty() {
        if total_count == 0 {
            group.weak("No open pull requests.");
        } else {
            group.weak("No matches for current search.");
        }
        return;
    }

    let active_review_thread_ids = account.active_review_thread_ids();
    let review_output_thread_ids: HashSet<_> = account.review_outputs.keys().cloned().collect();
    let open_review_window_thread_ids: HashSet<_> = account
        .review_outputs
        .iter()
        .filter_map(|(thread_id, review_output)| review_output.open.then_some(thread_id.clone()))
        .collect();
    let pr_description_prompt_available =
        pr_description_command_available(&account.profile.review_settings);

    let actions = if uses_compact_notifications(group.available_width()) {
        render_pull_request_cards(
            group,
            &matching_rows,
            &active_review_thread_ids,
            &review_output_thread_ids,
            &open_review_window_thread_ids,
            &context,
            repo_paths,
            custom_review_command,
            pr_description_prompt_available,
        )
    } else {
        render_pull_request_table(
            group,
            &matching_rows,
            &active_review_thread_ids,
            &review_output_thread_ids,
            &open_review_window_thread_ids,
            &context,
            repo_paths,
            custom_review_command,
            pr_description_prompt_available,
        )
    };

    for action in actions {
        match action {
            AccountAction::Review {
                thread_id,
                repo,
                pr_number,
                pr_url,
            } => {
                if let Some(launch) = resolve_review_launch(
                    repo_paths,
                    custom_review_command,
                    &repo,
                    pr_number,
                    &account.profile.review_settings,
                    &pr_url,
                ) {
                    account.request_review(thread_id, launch)
                } else {
                    account.last_error =
                        Some("Custom `review-pr` is unavailable for this repository.".to_owned());
                }
            }
            AccountAction::PrDescription {
                thread_id,
                repo,
                pr_number,
                pr_url,
            } => {
                if let Some(launch) = resolve_pr_description_launch(
                    repo_paths,
                    pr_description_prompt_available,
                    &repo,
                    pr_number,
                    &account.profile.review_settings,
                    &pr_url,
                ) {
                    account.request_review(thread_id, launch)
                } else {
                    account.last_error = Some(
                        "Custom `pr-description` is unavailable for this repository.".to_owned(),
                    );
                }
            }
            AccountAction::OpenReviewRequest {
                repo,
                pr_number,
                pr_title,
            } => account.open_review_request_editor(repo, pr_number, pr_title),
            AccountAction::StopReview(id) => account.cancel_review(&id),
            AccountAction::ToggleReviewWindow(id) => account.toggle_review_window_for_thread(&id),
            AccountAction::Done(_) | AccountAction::Seen(_) | AccountAction::Read(_) => {}
        }
    }
}

fn pull_request_matches_search(pull_request: &RepoPullRequest, filter: &SearchFilter) -> bool {
    let number_alias = pull_request.number.to_string();
    let hash_alias = format!("#{}", pull_request.number);
    let repo_number_alias = format!("{}#{}", pull_request.repo, pull_request.number);
    let title = pull_request.display_title();
    let author = pull_request.author_login.as_deref().unwrap_or("");
    let fields = [
        pull_request.repo.as_str(),
        title.as_str(),
        pull_request.title.as_str(),
        pull_request.url.as_str(),
        author,
        number_alias.as_str(),
        hash_alias.as_str(),
        repo_number_alias.as_str(),
    ];
    filter.matches_any(&fields)
}

#[derive(Clone, Copy, Debug, Default)]
struct RepoSortSignals {
    pending_review_request: bool,
    latest_review_request_at: Option<DateTime<Utc>>,
    needs_attention: bool,
    latest_attention_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default)]
struct RepoContextInfo {
    signals: RepoSortSignals,
    review_requester: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PullRequestActionAvailability {
    review: bool,
    pr_description: bool,
}

fn pull_request_action_availability(
    repo_paths: &BTreeMap<String, String>,
    custom_review_command: bool,
    pr_description_prompt_available: bool,
    repo: &str,
) -> PullRequestActionAvailability {
    PullRequestActionAvailability {
        review: custom_review_available_for_repo(repo_paths, custom_review_command, repo),
        pr_description: custom_review_available_for_repo(
            repo_paths,
            pr_description_prompt_available,
            repo,
        ),
    }
}

fn pr_description_unavailable_hover_text(pr_description_prompt_available: bool) -> &'static str {
    if pr_description_prompt_available {
        "Custom `pr-description` is unavailable for this repository. Add a local repo path to enable it."
    } else {
        "Custom `pr-description` prompt is unavailable for this account."
    }
}

fn build_repo_context_info(account: &AccountState) -> BTreeMap<PullRequestKey, RepoContextInfo> {
    let Some(inbox) = account.inbox.as_ref() else {
        return BTreeMap::new();
    };

    let reviewed_prs: HashSet<_> = inbox
        .recent_reviews
        .iter()
        .filter_map(|review| review.pull_request_key())
        .collect();
    let mut context = BTreeMap::new();

    for review_request in &inbox.review_requests {
        let Some(key) = review_request.pull_request_key() else {
            continue;
        };
        let entry = context
            .entry(key.clone())
            .or_insert_with(RepoContextInfo::default);
        let replaced = merge_latest_timestamp(
            &mut entry.signals.latest_review_request_at,
            review_request.updated_at,
        );
        if replaced && review_request.requested_by.is_some() {
            entry.review_requester = review_request.requested_by.clone();
        }
        if !reviewed_prs.contains(&key) {
            entry.signals.pending_review_request = true;
        }
    }

    for item in &inbox.notifications {
        let Some(key) = item.pull_request_key() else {
            continue;
        };
        let entry = context.entry(key).or_insert_with(RepoContextInfo::default);
        if is_review_request(item) {
            let _ = merge_latest_timestamp(
                &mut entry.signals.latest_review_request_at,
                item.updated_at,
            );
        }

        let visual = base_notification_state(item);
        if item.unread || visual.needs_revisit {
            entry.signals.needs_attention = true;
            let _ = merge_latest_timestamp(&mut entry.signals.latest_attention_at, item.updated_at);
        }
    }

    context
}

fn merge_latest_timestamp(slot: &mut Option<DateTime<Utc>>, value: DateTime<Utc>) -> bool {
    let keep_current = slot.as_ref().is_some_and(|current| current >= &value);
    if !keep_current {
        *slot = Some(value);
        return true;
    }
    false
}

fn sort_pull_requests(
    pull_requests: &mut Vec<&RepoPullRequest>,
    sort_mode: RepoSortMode,
    context: &BTreeMap<PullRequestKey, RepoContextInfo>,
) {
    match sort_mode {
        RepoSortMode::Default => {}
        RepoSortMode::ReviewRequest => pull_requests.sort_by(|a, b| {
            let a_signals = pull_request_sort_signals(context, a);
            let b_signals = pull_request_sort_signals(context, b);
            b_signals
                .pending_review_request
                .cmp(&a_signals.pending_review_request)
                .then_with(|| {
                    b_signals
                        .latest_review_request_at
                        .cmp(&a_signals.latest_review_request_at)
                })
                .then_with(|| b_signals.needs_attention.cmp(&a_signals.needs_attention))
                .then_with(|| {
                    b_signals
                        .latest_attention_at
                        .cmp(&a_signals.latest_attention_at)
                })
                .then_with(|| b.updated_at.cmp(&a.updated_at))
                .then_with(|| b.number.cmp(&a.number))
        }),
        RepoSortMode::Updated => pull_requests.sort_by(|a, b| {
            let a_signals = pull_request_sort_signals(context, a);
            let b_signals = pull_request_sort_signals(context, b);
            b_signals
                .needs_attention
                .cmp(&a_signals.needs_attention)
                .then_with(|| {
                    b_signals
                        .latest_attention_at
                        .cmp(&a_signals.latest_attention_at)
                })
                .then_with(|| {
                    b_signals
                        .pending_review_request
                        .cmp(&a_signals.pending_review_request)
                })
                .then_with(|| {
                    b_signals
                        .latest_review_request_at
                        .cmp(&a_signals.latest_review_request_at)
                })
                .then_with(|| b.updated_at.cmp(&a.updated_at))
                .then_with(|| b.number.cmp(&a.number))
        }),
    }
}

fn pull_request_sort_signals(
    context: &BTreeMap<PullRequestKey, RepoContextInfo>,
    pull_request: &RepoPullRequest,
) -> RepoSortSignals {
    context
        .get(&(pull_request.repo.clone(), pull_request.number))
        .map(|context| context.signals)
        .unwrap_or_default()
}

fn pull_request_summary_text(
    context: &BTreeMap<PullRequestKey, RepoContextInfo>,
    pull_request: &RepoPullRequest,
) -> String {
    if let Some(requester) = context
        .get(&(pull_request.repo.clone(), pull_request.number))
        .and_then(|context| context.review_requester.as_deref())
    {
        format!("Review requested by {requester}")
    } else if pull_request_sort_signals(context, pull_request).pending_review_request {
        String::from("Review requested")
    } else {
        format!(
            "Opened by {}",
            pull_request.author_login.as_deref().unwrap_or("unknown")
        )
    }
}

fn render_pull_request_signal_badges(ui: &mut egui::Ui, signals: RepoSortSignals) {
    if signals.pending_review_request {
        ui.small(
            RichText::new("Review requested")
                .strong()
                .color(ui.visuals().warn_fg_color),
        );
    }
    if signals.needs_attention {
        ui.small(
            RichText::new("Updated")
                .strong()
                .color(ui.visuals().warn_fg_color),
        );
    }
}

fn render_pull_request_cards(
    ui: &mut egui::Ui,
    pull_requests: &[&RepoPullRequest],
    active_review_thread_ids: &HashSet<String>,
    review_output_thread_ids: &HashSet<String>,
    open_review_window_thread_ids: &HashSet<String>,
    context: &BTreeMap<PullRequestKey, RepoContextInfo>,
    repo_paths: &BTreeMap<String, String>,
    custom_review_command: bool,
    pr_description_prompt_available: bool,
) -> Vec<AccountAction> {
    let mut actions = Vec::new();

    for pull_request in pull_requests {
        let signals = pull_request_sort_signals(context, pull_request);
        ui.group(|card| {
            card.vertical(|column| {
                column.horizontal_wrapped(|row| {
                    let response = row.hyperlink_to(
                        RichText::new(pull_request.display_title()),
                        &pull_request.url,
                    );
                    if pull_request.draft {
                        row.small(RichText::new("Draft").strong());
                    }
                    if response.hovered() {
                        row.small(
                            RichText::new(format!("#{}", pull_request.number))
                                .color(row.visuals().weak_text_color()),
                        );
                    }
                    render_pull_request_signal_badges(row, signals);
                });
                column.small(pull_request_summary_text(context, pull_request));
                column.small(format!(
                    "Updated {}",
                    format_local_timestamp(pull_request.updated_at, "%Y-%m-%d %H:%M")
                ));

                render_pull_request_actions(
                    column,
                    pull_request,
                    active_review_thread_ids,
                    review_output_thread_ids,
                    open_review_window_thread_ids,
                    repo_paths,
                    custom_review_command,
                    pr_description_prompt_available,
                    &mut actions,
                );
            });
        });
        ui.add_space(8.0);
    }

    actions
}

fn render_pull_request_table(
    ui: &mut egui::Ui,
    pull_requests: &[&RepoPullRequest],
    active_review_thread_ids: &HashSet<String>,
    review_output_thread_ids: &HashSet<String>,
    open_review_window_thread_ids: &HashSet<String>,
    context: &BTreeMap<PullRequestKey, RepoContextInfo>,
    repo_paths: &BTreeMap<String, String>,
    custom_review_command: bool,
    pr_description_prompt_available: bool,
) -> Vec<AccountAction> {
    let mut actions = Vec::new();

    egui::ScrollArea::horizontal()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            TableBuilder::new(ui)
                .striped(true)
                .column(Column::remainder().at_least(220.0))
                .column(Column::initial(140.0).resizable(true))
                .column(Column::initial(150.0).resizable(true))
                .column(Column::initial(120.0))
                .header(20.0, |mut header| {
                    header.col(|ui| {
                        ui.strong("Pull request");
                    });
                    header.col(|ui| {
                        ui.strong("Author");
                    });
                    header.col(|ui| {
                        ui.strong("Updated");
                    });
                    header.col(|ui| {
                        ui.strong("Actions");
                    });
                })
                .body(|mut body| {
                    for pull_request in pull_requests {
                        let signals = pull_request_sort_signals(context, pull_request);
                        body.row(38.0, |mut row| {
                            row.col(|ui| {
                                ui.horizontal_wrapped(|row_ui| {
                                    row_ui.hyperlink_to(
                                        RichText::new(pull_request.display_title()),
                                        &pull_request.url,
                                    );
                                    if pull_request.draft {
                                        row_ui.small(RichText::new("Draft").strong());
                                    }
                                    render_pull_request_signal_badges(row_ui, signals);
                                });
                                ui.small(pull_request_summary_text(context, pull_request));
                            });
                            row.col(|ui| {
                                ui.label(pull_request.author_login.as_deref().unwrap_or("unknown"));
                            });
                            row.col(|ui| {
                                ui.label(format_local_timestamp(
                                    pull_request.updated_at,
                                    "%Y-%m-%d %H:%M",
                                ));
                            });
                            row.col(|ui| {
                                render_pull_request_actions(
                                    ui,
                                    pull_request,
                                    active_review_thread_ids,
                                    review_output_thread_ids,
                                    open_review_window_thread_ids,
                                    repo_paths,
                                    custom_review_command,
                                    pr_description_prompt_available,
                                    &mut actions,
                                );
                            });
                        });
                    }
                });
        });

    actions
}

fn render_pull_request_actions(
    ui: &mut egui::Ui,
    pull_request: &RepoPullRequest,
    active_review_thread_ids: &HashSet<String>,
    review_output_thread_ids: &HashSet<String>,
    open_review_window_thread_ids: &HashSet<String>,
    repo_paths: &BTreeMap<String, String>,
    custom_review_command: bool,
    pr_description_prompt_available: bool,
    actions: &mut Vec<AccountAction>,
) {
    let review_thread_id = pull_request.review_thread_id();
    let pr_description_thread_id = pull_request.pr_description_thread_id();
    let availability = pull_request_action_availability(
        repo_paths,
        custom_review_command,
        pr_description_prompt_available,
        &pull_request.repo,
    );
    let review_active = active_review_thread_ids.contains(&review_thread_id);
    let pr_description_active = active_review_thread_ids.contains(&pr_description_thread_id);

    ui.horizontal_wrapped(|row| {
        if review_active {
            if progress_button(row, "    Reviewing")
                .on_hover_text("Click to stop this review.")
                .clicked()
            {
                actions.push(AccountAction::StopReview(review_thread_id.clone()));
            }
        } else if availability.review
            && row
                .button("Review")
                .on_hover_text("Run your local PR review flow.")
                .clicked()
        {
            actions.push(AccountAction::Review {
                thread_id: review_thread_id.clone(),
                repo: pull_request.repo.clone(),
                pr_number: pull_request.number,
                pr_url: pull_request.url.clone(),
            });
        } else if !availability.review {
            row.add_enabled(false, egui::Button::new("Review"))
                .on_hover_text("Custom `review-pr` is unavailable for this repository.");
        }

        if review_output_thread_ids.contains(&review_thread_id) {
            let window_label = if open_review_window_thread_ids.contains(&review_thread_id) {
                "Hide review"
            } else {
                "Show review"
            };
            if row.small_button(window_label).clicked() {
                actions.push(AccountAction::ToggleReviewWindow(review_thread_id.clone()));
            }
        }

        row.label("|");
        if pr_description_active {
            if progress_button(row, "    Generating")
                .on_hover_text("Click to stop this PR description run.")
                .clicked()
            {
                actions.push(AccountAction::StopReview(pr_description_thread_id.clone()));
            }
        } else if availability.pr_description
            && row
                .button("PR description")
                .on_hover_text("Generate a PR description with your local flow.")
                .clicked()
        {
            actions.push(AccountAction::PrDescription {
                thread_id: pr_description_thread_id.clone(),
                repo: pull_request.repo.clone(),
                pr_number: pull_request.number,
                pr_url: pull_request.url.clone(),
            });
        } else if !availability.pr_description {
            row.add_enabled(false, egui::Button::new("PR description"))
                .on_hover_text(pr_description_unavailable_hover_text(
                    pr_description_prompt_available,
                ));
        }
        if review_output_thread_ids.contains(&pr_description_thread_id) {
            let window_label = if open_review_window_thread_ids.contains(&pr_description_thread_id)
            {
                "Hide description"
            } else {
                "Show description"
            };
            if row.small_button(window_label).clicked() {
                actions.push(AccountAction::ToggleReviewWindow(
                    pr_description_thread_id.clone(),
                ));
            }
        }
        if row
            .button("Reviewers")
            .on_hover_text("Open reviewer management for this pull request.")
            .clicked()
        {
            actions.push(AccountAction::OpenReviewRequest {
                repo: pull_request.repo.clone(),
                pr_number: pull_request.number,
                pr_title: pull_request.display_title(),
            });
        }
    });
}

fn progress_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let response = ui.add(egui::Button::new(label));
    let spinner_size = 10.0;
    let spinner_rect = egui::Rect::from_center_size(
        egui::pos2(response.rect.left() + 14.0, response.rect.center().y),
        egui::vec2(spinner_size, spinner_size),
    );
    egui::Spinner::new()
        .size(spinner_size)
        .paint_at(ui, spinner_rect);
    response
}

#[cfg(test)]
mod tests {
    use super::{
        PullRequestActionAvailability, RepoContextInfo, RepoSortMode, RepoSortSignals,
        pr_description_unavailable_hover_text, pull_request_action_availability,
        pull_request_matches_search, pull_request_summary_text, sort_pull_requests,
    };
    use crate::{
        app::search::SearchFilter,
        domain::{PullRequestKey, RepoPullRequest},
    };
    use chrono::Utc;
    use std::collections::BTreeMap;

    fn pull_request() -> RepoPullRequest {
        RepoPullRequest {
            repo: String::from("acme/repo"),
            number: 123,
            title: String::from("Improve filters"),
            url: String::from("https://github.com/acme/repo/pull/123"),
            updated_at: Utc::now(),
            author_login: Some(String::from("neo")),
            draft: false,
        }
    }

    #[test]
    fn pull_request_search_matches_repo_scoped_number() {
        assert!(pull_request_matches_search(
            &pull_request(),
            &SearchFilter::new("acme/repo#123")
        ));
    }

    #[test]
    fn pull_request_search_matches_author_login() {
        assert!(pull_request_matches_search(
            &pull_request(),
            &SearchFilter::new("neo")
        ));
    }

    #[test]
    fn review_request_sort_prioritizes_pending_review_requests() {
        let pr_1 = pull_request();
        let mut pr_2 = pull_request();
        pr_2.number = 456;
        pr_2.title = String::from("Refine review ordering");

        let mut rows = vec![&pr_1, &pr_2];
        let mut context = BTreeMap::<PullRequestKey, RepoContextInfo>::new();
        context.insert(
            (String::from("acme/repo"), 456),
            RepoContextInfo {
                signals: RepoSortSignals {
                    pending_review_request: true,
                    latest_review_request_at: Some(Utc::now()),
                    needs_attention: false,
                    latest_attention_at: None,
                },
                review_requester: Some(String::from("alice")),
            },
        );

        sort_pull_requests(&mut rows, RepoSortMode::ReviewRequest, &context);

        assert_eq!(rows[0].number, 456);
    }

    #[test]
    fn updated_sort_prioritizes_attention_over_plain_recency() {
        let mut stale = pull_request();
        stale.number = 100;
        stale.updated_at = Utc::now();

        let mut needs_attention = pull_request();
        needs_attention.number = 200;
        needs_attention.updated_at = Utc::now() - chrono::Duration::days(1);

        let mut rows = vec![&stale, &needs_attention];
        let mut context = BTreeMap::<PullRequestKey, RepoContextInfo>::new();
        context.insert(
            (String::from("acme/repo"), 200),
            RepoContextInfo {
                signals: RepoSortSignals {
                    pending_review_request: false,
                    latest_review_request_at: None,
                    needs_attention: true,
                    latest_attention_at: Some(Utc::now()),
                },
                review_requester: None,
            },
        );

        sort_pull_requests(&mut rows, RepoSortMode::Updated, &context);

        assert_eq!(rows[0].number, 200);
    }

    #[test]
    fn summary_text_prefers_review_requester_when_available() {
        let pr = pull_request();
        let mut context = BTreeMap::<PullRequestKey, RepoContextInfo>::new();
        context.insert(
            (String::from("acme/repo"), 123),
            RepoContextInfo {
                signals: RepoSortSignals {
                    pending_review_request: true,
                    latest_review_request_at: Some(Utc::now()),
                    needs_attention: false,
                    latest_attention_at: None,
                },
                review_requester: Some(String::from("alice")),
            },
        );

        assert_eq!(
            pull_request_summary_text(&context, &pr),
            "Review requested by alice"
        );
    }

    #[test]
    fn action_availability_requires_local_repo_for_pr_description() {
        let availability =
            pull_request_action_availability(&BTreeMap::new(), true, true, "acme/repo");

        assert_eq!(
            availability,
            PullRequestActionAvailability {
                review: false,
                pr_description: false,
            }
        );
    }

    #[test]
    fn action_availability_enables_pr_description_when_prompt_and_repo_exist() {
        let mut repo_paths = BTreeMap::new();
        repo_paths.insert(String::from("acme/repo"), String::from("/tmp/acme-repo"));

        let availability = pull_request_action_availability(&repo_paths, true, true, "Acme/Repo");

        assert_eq!(
            availability,
            PullRequestActionAvailability {
                review: true,
                pr_description: true,
            }
        );
    }

    #[test]
    fn pr_description_hover_text_explains_missing_repo_path() {
        assert_eq!(
            pr_description_unavailable_hover_text(true),
            "Custom `pr-description` is unavailable for this repository. Add a local repo path to enable it."
        );
    }
}
