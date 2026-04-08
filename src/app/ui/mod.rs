mod account_card;
mod layout;
mod notifications;
mod repository_card;
mod sidebar;

pub(super) use account_card::render_account_card;
pub(super) use layout::{responsive_accounts_panel_width, uses_compact_account_rows};
pub(super) use repository_card::render_repository_card;
pub(super) use sidebar::{
    account_overview, render_tracked_account_badges, tracked_account_heading,
};

#[cfg(test)]
pub(in crate::app) use layout::{uses_compact_notifications, uses_stacked_account_header};

#[cfg(test)]
pub(in crate::app) use notifications::{
    NotificationRenderState, notification_state, render_bucket_sections,
};
