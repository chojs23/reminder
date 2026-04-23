use std::collections::{BTreeMap, HashSet};

use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};

use crate::domain::{NotificationItem, PullRequestReviewerStatus};

const APPROVED_TITLE_CHECK_COLOR: Color32 = Color32::from_rgb(80, 170, 90);

use super::super::{
    AccountAction, PENDING_REVIEW_LABEL_COLOR, SectionKind,
    notification_state::{
        NotificationVisualState, base_notification_state, is_mention, is_other_notification,
        is_review_request, pending_review_request_ids, summarize_counts,
    },
    review::custom_review_available_for_repo,
    search::SearchFilter,
    state::AccountState,
    time::format_local_timestamp,
};
use super::layout::uses_compact_notifications;

pub(in crate::app) struct NotificationRenderState<'a> {
    pub(in crate::app) inflight_done: &'a HashSet<String>,
    pub(in crate::app) pending_review_ids: &'a HashSet<String>,
    pub(in crate::app) active_review_thread_ids: &'a HashSet<String>,
    pub(in crate::app) review_output_thread_ids: &'a HashSet<String>,
    pub(in crate::app) open_review_window_thread_ids: &'a HashSet<String>,
    pub(in crate::app) custom_review_command: bool,
    pub(in crate::app) repo_paths: &'a BTreeMap<String, String>,
}

pub(super) fn render_unified_inbox_section(
    group: &mut egui::Ui,
    account: &mut AccountState,
    filter: &SearchFilter,
    repo_paths: &BTreeMap<String, String>,
    custom_review_command: bool,
) -> Vec<AccountAction> {
    let inflight_done = account.inflight_done.clone();
    let inbox = account.inbox.as_ref().expect("checked by caller");
    let pending_review_ids = pending_review_request_ids(inbox);
    let active_review_thread_ids = account.active_review_thread_ids();
    let review_output_thread_ids: HashSet<_> = account.review_outputs.keys().cloned().collect();
    let open_review_window_thread_ids: HashSet<_> = account
        .review_outputs
        .iter()
        .filter_map(|(thread_id, review_output)| review_output.open.then_some(thread_id.clone()))
        .collect();
    let render_state = NotificationRenderState {
        inflight_done: &inflight_done,
        pending_review_ids: &pending_review_ids,
        active_review_thread_ids: &active_review_thread_ids,
        review_output_thread_ids: &review_output_thread_ids,
        open_review_window_thread_ids: &open_review_window_thread_ids,
        custom_review_command,
        repo_paths,
    };
    let notifications: Vec<_> = inbox.notifications.iter().collect();

    let (actions, cleared_highlight) = render_notification_section(
        group,
        "Inbox",
        notifications,
        "You're all caught up 🎉",
        filter,
        &render_state,
        account.highlights.contains(&SectionKind::Inbox),
    );
    if cleared_highlight {
        account.highlights.remove(&SectionKind::Inbox);
    }
    actions
}

pub(in crate::app) fn render_bucket_sections(
    group: &mut egui::Ui,
    account: &mut AccountState,
    filter: &SearchFilter,
    repo_paths: &BTreeMap<String, String>,
    custom_review_command: bool,
) -> Vec<AccountAction> {
    let mut actions = Vec::new();
    let inflight_done = account.inflight_done.clone();
    let inbox = account.inbox.as_ref().expect("checked by caller");
    let pending_review_ids = pending_review_request_ids(inbox);
    let active_review_thread_ids = account.active_review_thread_ids();
    let review_output_thread_ids: HashSet<_> = account.review_outputs.keys().cloned().collect();
    let open_review_window_thread_ids: HashSet<_> = account
        .review_outputs
        .iter()
        .filter_map(|(thread_id, review_output)| review_output.open.then_some(thread_id.clone()))
        .collect();
    let render_state = NotificationRenderState {
        inflight_done: &inflight_done,
        pending_review_ids: &pending_review_ids,
        active_review_thread_ids: &active_review_thread_ids,
        review_output_thread_ids: &review_output_thread_ids,
        open_review_window_thread_ids: &open_review_window_thread_ids,
        custom_review_command,
        repo_paths,
    };

    let review_requests: Vec<_> = inbox
        .notifications
        .iter()
        .filter(|item| is_review_request(item))
        .collect();

    let (section_actions, cleared_highlight) = render_notification_section(
        group,
        "Review requests",
        review_requests,
        "No pending review requests.",
        filter,
        &render_state,
        account.highlights.contains(&SectionKind::ReviewRequests),
    );
    actions.extend(section_actions);
    if cleared_highlight {
        account.highlights.remove(&SectionKind::ReviewRequests);
    }
    group.separator();

    let mentions: Vec<_> = inbox
        .notifications
        .iter()
        .filter(|item| is_mention(item))
        .collect();
    let (section_actions, cleared_highlight) = render_notification_section(
        group,
        "Mentions",
        mentions,
        "No recent mentions.",
        filter,
        &render_state,
        account.highlights.contains(&SectionKind::Mentions),
    );
    actions.extend(section_actions);
    if cleared_highlight {
        account.highlights.remove(&SectionKind::Mentions);
    }
    group.separator();

    let other: Vec<_> = inbox
        .notifications
        .iter()
        .filter(|item| is_other_notification(item))
        .collect();
    let (section_actions, cleared_highlight) = render_notification_section(
        group,
        "Notifications",
        other,
        "You're all caught up 🎉",
        filter,
        &render_state,
        account.highlights.contains(&SectionKind::Notifications),
    );
    actions.extend(section_actions);
    if cleared_highlight {
        account.highlights.remove(&SectionKind::Notifications);
    }

    actions
}

fn render_notification_section(
    group: &mut egui::Ui,
    title: &str,
    subset: Vec<&NotificationItem>,
    empty_label: &'static str,
    filter: &SearchFilter,
    render_state: &NotificationRenderState<'_>,
    highlight: bool,
) -> (Vec<AccountAction>, bool) {
    let (unseen_count, updated_count) = summarize_counts(&subset);
    let heading = format!(
        "{title} ({} unseen, {} updated)",
        unseen_count, updated_count
    );
    let heading_text = if highlight {
        RichText::new(heading.clone())
            .strong()
            .color(group.visuals().warn_fg_color)
    } else {
        RichText::new(heading.clone()).strong()
    };
    let header = egui::CollapsingHeader::new(heading_text)
        .id_salt(format!("notification-section-{title}"))
        .default_open(true);

    if subset.is_empty() {
        let response = header.show(group, |section| {
            section.weak(empty_label);
        });
        return (Vec::new(), response.body_returned.is_some() && highlight);
    }

    let mut actions = Vec::new();
    let response = header.show(group, |section| {
        actions.extend(draw_notifications(section, &subset, filter, render_state));
    });
    (actions, response.body_returned.is_some() && highlight)
}

pub(in crate::app) fn notification_state(
    item: &NotificationItem,
    render_state: &NotificationRenderState<'_>,
) -> NotificationVisualState {
    let mut visual = base_notification_state(item);
    visual.pending_review = render_state.pending_review_ids.contains(&item.thread_id);
    visual
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

fn pending_review_badge(ui: &mut egui::Ui) {
    ui.small(
        RichText::new("Pending review")
            .strong()
            .color(PENDING_REVIEW_LABEL_COLOR),
    );
}

fn reviewing_button(ui: &mut egui::Ui) -> egui::Response {
    let response = ui.add(egui::Button::new("    Reviewing"));
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

fn draw_notifications(
    ui: &mut egui::Ui,
    items: &[&NotificationItem],
    filter: &SearchFilter,
    render_state: &NotificationRenderState<'_>,
) -> Vec<AccountAction> {
    let rows: Vec<_> = items
        .iter()
        .copied()
        .filter(|item| notification_matches_search(item, filter))
        .collect();
    if rows.is_empty() {
        ui.weak("No matches for current search.");
        return Vec::new();
    }

    if uses_compact_notifications(ui.available_width()) {
        return draw_notification_cards(ui, &rows, render_state);
    }

    draw_notification_table(ui, &rows, render_state)
}

fn notification_matches_search(item: &NotificationItem, filter: &SearchFilter) -> bool {
    let display_title = item.display_title();
    let merge_direction = item.merge_direction_text();
    let number_alias = item.thread_number().map(|number| number.to_string());
    let hash_alias = item.thread_number().map(|number| format!("#{number}"));
    let repo_number_alias = item
        .thread_number()
        .map(|number| format!("{}#{number}", item.repo));

    let mut fields = vec![
        item.repo.as_str(),
        display_title.as_str(),
        item.reason.as_str(),
    ];
    if let Some(head_ref) = item.head_ref.as_deref() {
        fields.push(head_ref);
    }
    if let Some(base_ref) = item.base_ref.as_deref() {
        fields.push(base_ref);
    }
    if let Some(direction) = merge_direction.as_deref() {
        fields.push(direction);
    }
    if let Some(url) = item.url.as_deref() {
        fields.push(url);
    }
    if let Some(alias) = number_alias.as_deref() {
        fields.push(alias);
    }
    if let Some(alias) = hash_alias.as_deref() {
        fields.push(alias);
    }
    if let Some(alias) = repo_number_alias.as_deref() {
        fields.push(alias);
    }

    filter.matches_any(&fields)
}

fn draw_notification_cards(
    ui: &mut egui::Ui,
    rows: &[&NotificationItem],
    render_state: &NotificationRenderState<'_>,
) -> Vec<AccountAction> {
    let mut actions = Vec::new();

    for item in rows {
        let visual = notification_state(item, render_state);
        ui.group(|card| {
            card.vertical(|column| {
                column.horizontal_wrapped(|row| {
                    row.label(notification_text(row, &item.repo, visual));
                    row.separator();
                    row.label(notification_text(
                        row,
                        format_local_timestamp(item.updated_at, "%Y-%m-%d %H:%M"),
                        visual,
                    ));
                    if visual.needs_revisit {
                        row.small(
                            RichText::new("Updated")
                                .strong()
                                .color(row.visuals().warn_fg_color),
                        );
                    }
                    if visual.pending_review {
                        pending_review_badge(row);
                    }
                });

                let display_title = item.display_title();
                if let Some(url) = &item.url {
                    let resp = render_notification_title_link(
                        column,
                        item,
                        notification_text(column, display_title.as_str(), visual),
                        url,
                    );
                    if resp.clicked() {
                        actions.push(AccountAction::Seen(item.thread_id.clone()));
                    }
                } else {
                    let resp = render_notification_title_label(
                        column,
                        item,
                        notification_text(column, display_title.as_str(), visual),
                    );
                    if resp.clicked() {
                        actions.push(AccountAction::Seen(item.thread_id.clone()));
                    }
                }

                render_notification_branch_direction(column, item, visual);

                column.small(notification_text(
                    column,
                    format!("Reason: {}", &item.reason),
                    visual,
                ));

                column.horizontal_wrapped(|row| {
                    let busy = render_state.inflight_done.contains(&item.thread_id);
                    let review_active = render_state
                        .active_review_thread_ids
                        .contains(&item.thread_id);
                    let already_read = !item.unread && !visual.needs_revisit;

                    if row
                        .add_enabled(!busy && !already_read, egui::Button::new("Mark read"))
                        .clicked()
                    {
                        actions.push(AccountAction::Read(item.thread_id.clone()));
                    }

                    if let (Some(pr_url), Some(pr_number)) =
                        (item.pull_request_url(), item.pull_request_number())
                    {
                        let custom_review_available = custom_review_available_for_repo(
                            render_state.repo_paths,
                            render_state.custom_review_command,
                            &item.repo,
                        );
                        if review_active {
                            if reviewing_button(row)
                                .on_hover_text("Click to stop this review.")
                                .clicked()
                            {
                                actions.push(AccountAction::StopReview(item.thread_id.clone()));
                            }
                        } else if custom_review_available
                            && row
                                .add_enabled(!busy, egui::Button::new("Review"))
                                .clicked()
                        {
                            actions.push(AccountAction::Review {
                                thread_id: item.thread_id.clone(),
                                repo: item.repo.clone(),
                                pr_number,
                                pr_url: pr_url.to_owned(),
                            });
                        } else if !custom_review_available {
                            row.add_enabled(false, egui::Button::new("Review"))
                                .on_hover_text(
                                    "Custom `review-pr` is unavailable for this repository.",
                                );
                        }

                        if render_state
                            .review_output_thread_ids
                            .contains(&item.thread_id)
                        {
                            let window_label = if render_state
                                .open_review_window_thread_ids
                                .contains(&item.thread_id)
                            {
                                "Hide review"
                            } else {
                                "Show review"
                            };
                            if row.small_button(window_label).clicked() {
                                actions.push(AccountAction::ToggleReviewWindow(
                                    item.thread_id.clone(),
                                ));
                            }
                        }
                    }

                    if busy && !review_active {
                        row.spinner();
                    }
                });
            });
        });
        ui.add_space(8.0);
    }

    actions
}

fn draw_notification_table(
    ui: &mut egui::Ui,
    rows: &[&NotificationItem],
    render_state: &NotificationRenderState<'_>,
) -> Vec<AccountAction> {
    let mut actions = Vec::new();

    egui::ScrollArea::horizontal()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            TableBuilder::new(ui)
                .striped(true)
                .column(Column::initial(120.0).resizable(true))
                .column(Column::remainder().at_least(140.0))
                .column(Column::initial(130.0).resizable(true))
                .column(Column::initial(100.0))
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
                        let visual = notification_state(item, render_state);
                        body.row(24.0, |mut row| {
                            row.col(|ui| {
                                ui.label(notification_text(ui, &item.repo, visual));
                            });
                            row.col(|ui| {
                                ui.horizontal(|row_ui| {
                                    let display_title = item.display_title();
                                    let subject =
                                        notification_text(row_ui, display_title.as_str(), visual);
                                    if let Some(url) = &item.url {
                                        let resp = render_notification_title_link(row_ui, item, subject, url);
                                        if resp.clicked() {
                                            actions
                                                .push(AccountAction::Seen(item.thread_id.clone()));
                                            }
                                    } else {
                                        let resp = render_notification_title_label(row_ui, item, subject);
                                        if resp.clicked() {
                                            actions
                                                .push(AccountAction::Seen(item.thread_id.clone()));
                                        }
                                    }
                                    if visual.needs_revisit {
                                        row_ui.small(
                                            RichText::new("Updated")
                                                .strong()
                                                .color(row_ui.visuals().warn_fg_color),
                                        );
                                    }
                                    if visual.pending_review {
                                        pending_review_badge(row_ui);
                                    }
                                });
                                render_notification_branch_direction(ui, item, visual);
                                ui.small(notification_text(
                                    ui,
                                    format!("Reason: {}", &item.reason),
                                    visual,
                                ));
                            });
                            row.col(|ui| {
                                ui.label(notification_text(
                                    ui,
                                    format_local_timestamp(item.updated_at, "%Y-%m-%d %H:%M"),
                                    visual,
                                ));
                            });
                            row.col(|ui| {
                                let busy = render_state.inflight_done.contains(&item.thread_id);
                                let review_active = render_state
                                    .active_review_thread_ids
                                    .contains(&item.thread_id);
                                let already_read = !item.unread && !visual.needs_revisit;

                                ui.horizontal_wrapped(|row_ui| {
                                    if row_ui
                                        .add_enabled(
                                            !busy && !already_read,
                                            egui::Button::new("Mark read"),
                                        )
                                        .clicked()
                                    {
                                        actions.push(AccountAction::Read(item.thread_id.clone()));
                                    }

                                    if let (Some(pr_url), Some(pr_number)) =
                                        (item.pull_request_url(), item.pull_request_number())
                                    {
                                        let custom_review_available =
                                            custom_review_available_for_repo(
                                                render_state.repo_paths,
                                                render_state.custom_review_command,
                                                &item.repo,
                                            );
                                        if review_active {
                                            if reviewing_button(row_ui)
                                                .on_hover_text("Click to stop this review.")
                                                .clicked()
                                            {
                                                actions.push(AccountAction::StopReview(
                                                    item.thread_id.clone(),
                                                ));
                                            }
                                        } else if custom_review_available
                                            && row_ui
                                            .add_enabled(!busy, egui::Button::new("Review"))
                                            .clicked()
                                        {
                                            actions.push(AccountAction::Review {
                                                thread_id: item.thread_id.clone(),
                                                repo: item.repo.clone(),
                                                pr_number,
                                                pr_url: pr_url.to_owned(),
                                            });
                                        } else if !custom_review_available {
                                            row_ui
                                                .add_enabled(false, egui::Button::new("Review"))
                                                .on_hover_text(
                                                    "Custom `review-pr` is unavailable for this repository.",
                                                );
                                        }

                                        if render_state
                                            .review_output_thread_ids
                                            .contains(&item.thread_id)
                                        {
                                            let window_label = if render_state
                                                .open_review_window_thread_ids
                                                .contains(&item.thread_id)
                                            {
                                                "Hide review"
                                            } else {
                                                "Show review"
                                            };
                                            if row_ui.small_button(window_label).clicked() {
                                                actions.push(AccountAction::ToggleReviewWindow(
                                                    item.thread_id.clone(),
                                                ));
                                            }
                                        }
                                    }

                                    if busy && !review_active {
                                        row_ui.spinner();
                                    }
                                });
                            });
                        });
                    }
                });
        });
    actions
}

fn notification_has_approved_badge(item: &NotificationItem) -> bool {
    item.my_review_status == Some(PullRequestReviewerStatus::Approved)
}

fn render_notification_title_link(
    ui: &mut egui::Ui,
    item: &NotificationItem,
    text: RichText,
    url: &str,
) -> egui::Response {
    let response = ui.hyperlink_to(text, url);
    if notification_has_approved_badge(item) {
        ui.small(
            RichText::new("✓")
                .strong()
                .color(APPROVED_TITLE_CHECK_COLOR),
        )
        .on_hover_text("You approved this pull request.");
    }
    response
}

fn render_notification_title_label(
    ui: &mut egui::Ui,
    item: &NotificationItem,
    text: RichText,
) -> egui::Response {
    let response = ui.label(text);
    if notification_has_approved_badge(item) {
        ui.small(
            RichText::new("✓")
                .strong()
                .color(APPROVED_TITLE_CHECK_COLOR),
        )
        .on_hover_text("You approved this pull request.");
    }
    response
}

fn render_notification_branch_direction(
    ui: &mut egui::Ui,
    item: &NotificationItem,
    visual: NotificationVisualState,
) {
    let Some(direction) = item.merge_direction_text() else {
        return;
    };
    ui.small(notification_text(ui, direction, visual));
}

#[cfg(test)]
mod tests {
    use super::{notification_has_approved_badge, notification_matches_search};
    use crate::{
        app::search::SearchFilter,
        domain::{NotificationItem, PullRequestReviewerStatus},
    };
    use chrono::Utc;

    fn notification_with_url(url: &str) -> NotificationItem {
        NotificationItem {
            thread_id: String::from("thread-1"),
            repo: String::from("acme/repo"),
            title: String::from("Fix search behavior"),
            url: Some(url.to_owned()),
            head_ref: Some(String::from("feature/search")),
            base_ref: Some(String::from("main")),
            my_review_status: None,
            reason: String::from("review_requested"),
            updated_at: Utc::now(),
            last_read_at: None,
            unread: true,
        }
    }

    #[test]
    fn notification_search_matches_pull_request_number_without_hash() {
        let item = notification_with_url("https://github.com/acme/repo/pull/123");

        assert!(notification_matches_search(
            &item,
            &SearchFilter::new("123")
        ));
    }

    #[test]
    fn notification_search_matches_pull_request_number_with_hash() {
        let item = notification_with_url("https://github.com/acme/repo/pull/123");

        assert!(notification_matches_search(
            &item,
            &SearchFilter::new("#123")
        ));
    }

    #[test]
    fn notification_search_ignores_unrelated_pull_request_number() {
        let item = notification_with_url("https://github.com/acme/repo/pull/123");

        assert!(!notification_matches_search(
            &item,
            &SearchFilter::new("456")
        ));
    }

    #[test]
    fn notification_search_matches_repo_scoped_pull_request_number() {
        let item = notification_with_url("https://github.com/acme/repo/pull/123");

        assert!(notification_matches_search(
            &item,
            &SearchFilter::new("acme/repo#123")
        ));
    }

    #[test]
    fn notification_search_matches_merge_direction() {
        let item = notification_with_url("https://github.com/acme/repo/pull/123");

        assert!(notification_matches_search(
            &item,
            &SearchFilter::new("feature/search -> main")
        ));
    }

    #[test]
    fn approved_badge_only_shows_for_approved_notifications() {
        let mut item = notification_with_url("https://github.com/acme/repo/pull/123");
        assert!(!notification_has_approved_badge(&item));

        item.my_review_status = Some(PullRequestReviewerStatus::Commented);
        assert!(!notification_has_approved_badge(&item));

        item.my_review_status = Some(PullRequestReviewerStatus::Approved);
        assert!(notification_has_approved_badge(&item));
    }
}
