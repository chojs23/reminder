use chrono::{DateTime, Utc};

// Domain data structures shared across modules.

#[derive(Clone)]
pub struct GitHubAccount {
    pub login: String,
    pub token: String,
}

#[derive(Clone, Debug)]
pub struct InboxSnapshot {
    pub notifications: Vec<NotificationItem>,
    #[allow(dead_code)]
    pub review_requests: Vec<ReviewRequest>,
    #[allow(dead_code)]
    pub mentions: Vec<MentionThread>,
    #[allow(dead_code)]
    pub recent_reviews: Vec<ReviewSummary>,
    pub fetched_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct NotificationItem {
    pub thread_id: String,
    pub repo: String,
    pub title: String,
    pub url: Option<String>,
    pub reason: String,
    pub updated_at: DateTime<Utc>,
    pub unread: bool,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct ReviewRequest {
    pub _id: u64,
    pub repo: String,
    pub title: String,
    pub url: String,
    pub updated_at: DateTime<Utc>,
    pub requested_by: Option<String>,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct MentionThread {
    pub _id: u64,
    pub repo: String,
    pub title: String,
    pub url: String,
    pub updated_at: DateTime<Utc>,
    pub kind: MentionKind,
}

#[derive(Clone, Debug)]
pub enum MentionKind {
    Issue,
    PullRequest,
}

impl MentionKind {
    #[allow(dead_code)]
    pub fn label(&self) -> &'static str {
        match self {
            MentionKind::Issue => "Issue",
            MentionKind::PullRequest => "Pull request",
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct ReviewSummary {
    pub _id: u64,
    pub repo: String,
    pub title: String,
    pub url: String,
    pub updated_at: DateTime<Utc>,
    pub state: String,
}
