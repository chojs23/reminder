use std::collections::BTreeMap;

use eframe::egui::{self, Layout, RichText};

use super::{
    super::{
        AccountAction, AccountViewMode,
        review::{resolve_review_launch, review_summary_text},
        search::SearchFilter,
        state::AccountState,
        time::format_local_timestamp,
    },
    layout::uses_stacked_account_header,
    notifications::{render_bucket_sections, render_unified_inbox_section},
};

pub(in crate::app) fn render_account_card(
    ui: &mut egui::Ui,
    account: &mut AccountState,
    repo_paths: &BTreeMap<String, String>,
    custom_review_command: bool,
) {
    ui.group(|group| {
        render_account_header(group, account);
        render_account_status(group, account);
        render_account_body(group, account, repo_paths, custom_review_command);
    });
    ui.add_space(12.0);
}

fn render_account_header(group: &mut egui::Ui, account: &mut AccountState) {
    if uses_stacked_account_header(group.available_width()) {
        group.vertical(|column| {
            column.horizontal_wrapped(|row| {
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
            });
            render_view_mode_toggle(column, account);
            let search_width = column.available_width();
            column.add(
                egui::TextEdit::singleline(&mut account.search_query)
                    .hint_text("Search…")
                    .desired_width(search_width),
            );
        });
    } else {
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
                lane.add_space(8.0);
                render_view_mode_toggle(lane, account);
            });
        });
    }
}

fn render_view_mode_toggle(ui: &mut egui::Ui, account: &mut AccountState) {
    ui.selectable_value(&mut account.view_mode, AccountViewMode::Grouped, "Grouped")
        .on_hover_text("Group notifications into review requests, mentions, and everything else.");
    ui.selectable_value(
        &mut account.view_mode,
        AccountViewMode::Inbox,
        "Unified inbox",
    )
    .on_hover_text("Show every GitHub notification in one list, like GitHub's inbox.");
}

fn render_account_status(group: &mut egui::Ui, account: &mut AccountState) {
    if let Some(inbox) = &account.inbox {
        group.label(format!(
            "Last synced {}",
            format_local_timestamp(inbox.fetched_at, "%Y-%m-%d %H:%M:%S %:z")
        ));
    } else {
        group.label("No data fetched yet.");
    }

    if let Some(err) = &account.last_error {
        group.colored_label(group.visuals().error_fg_color, err);
    } else if account.pending_job.is_some() {
        group.label("Fetching latest notifications...");
    }

    for review_output in account.review_outputs.values() {
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

fn render_account_body(
    group: &mut egui::Ui,
    account: &mut AccountState,
    repo_paths: &BTreeMap<String, String>,
    custom_review_command: bool,
) {
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
        let actions = match account.view_mode {
            AccountViewMode::Inbox => render_unified_inbox_section(
                group,
                account,
                &filter,
                repo_paths,
                custom_review_command,
            ),
            AccountViewMode::Grouped => {
                render_bucket_sections(group, account, &filter, repo_paths, custom_review_command)
            }
        };
        for action in actions {
            match action {
                AccountAction::Done(id) => account.request_mark_done(id),
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
                        account.last_error = Some(
                            "Custom `review-pr` is unavailable for this repository.".to_owned(),
                        );
                    }
                }
                AccountAction::StopReview(id) => account.cancel_review(&id),
                AccountAction::ToggleReviewWindow(id) => {
                    account.toggle_review_window_for_thread(&id)
                }
                AccountAction::Seen(id) => account.mark_notification_seen(&id),
                AccountAction::Read(id) => account.request_mark_read(id),
            }
        }
    }
}
