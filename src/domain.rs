use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// Domain data structures shared across modules.

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCommandSettings {
    #[serde(default)]
    pub env_vars: BTreeMap<String, String>,
    #[serde(default)]
    pub additional_args: Vec<String>,
}

#[derive(Clone)]
pub struct GitHubAccount {
    pub login: String,
    pub token: String,
    pub review_settings: ReviewCommandSettings,
}

#[derive(Clone, Debug)]
pub struct InboxSnapshot {
    pub notifications: Vec<NotificationItem>,
    pub review_requests: Vec<ReviewRequest>,
    #[allow(dead_code)]
    pub mentions: Vec<MentionThread>,
    pub recent_reviews: Vec<ReviewSummary>,
    pub fetched_at: DateTime<Utc>,
}

pub type PullRequestKey = (String, u64);

#[derive(Clone, Debug)]
pub struct NotificationItem {
    pub thread_id: String,
    pub repo: String,
    pub title: String,
    pub url: Option<String>,
    pub reason: String,
    pub updated_at: DateTime<Utc>,
    pub last_read_at: Option<DateTime<Utc>>,
    pub unread: bool,
}

impl NotificationItem {
    pub fn pull_request_url(&self) -> Option<&str> {
        let url = self.url.as_deref()?;
        pull_request_number_from_url(url).map(|_| url)
    }

    pub fn pull_request_number(&self) -> Option<u64> {
        self.url.as_deref().and_then(pull_request_number_from_url)
    }

    pub fn pull_request_key(&self) -> Option<PullRequestKey> {
        Some((self.repo.clone(), self.pull_request_number()?))
    }

    pub fn thread_number(&self) -> Option<u64> {
        self.url.as_deref().and_then(thread_number_from_url)
    }

    pub fn display_title(&self) -> String {
        match self.thread_number() {
            Some(number) => {
                let prefix = format!("#{number} ");
                if self.title.starts_with(&prefix) {
                    self.title.clone()
                } else {
                    format!("{}{}", prefix, self.title)
                }
            }
            None => self.title.clone(),
        }
    }
}

fn pull_request_number_from_url(url: &str) -> Option<u64> {
    thread_number_from_known_segment(url, "/pull/")
}

fn issue_number_from_url(url: &str) -> Option<u64> {
    thread_number_from_known_segment(url, "/issues/")
}

fn thread_number_from_url(url: &str) -> Option<u64> {
    pull_request_number_from_url(url).or_else(|| issue_number_from_url(url))
}

fn thread_number_from_known_segment(url: &str, segment: &str) -> Option<u64> {
    let (_, suffix) = url.split_once(segment)?;
    let number = suffix.split(['/', '?', '#']).next()?;
    number.parse().ok()
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

impl ReviewRequest {
    pub fn pull_request_number(&self) -> Option<u64> {
        pull_request_number_from_url(&self.url)
    }

    pub fn pull_request_key(&self) -> Option<PullRequestKey> {
        Some((self.repo.clone(), self.pull_request_number()?))
    }
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

impl ReviewSummary {
    pub fn pull_request_number(&self) -> Option<u64> {
        pull_request_number_from_url(&self.url)
    }

    pub fn pull_request_key(&self) -> Option<PullRequestKey> {
        Some((self.repo.clone(), self.pull_request_number()?))
    }
}

#[cfg(test)]
mod tests {
    use super::NotificationItem;
    use chrono::Utc;

    fn notification(url: Option<&str>) -> NotificationItem {
        NotificationItem {
            thread_id: "thread-1".into(),
            repo: "acme/repo".into(),
            title: "Title".into(),
            url: url.map(str::to_owned),
            reason: "review_requested".into(),
            updated_at: Utc::now(),
            last_read_at: None,
            unread: true,
        }
    }

    #[test]
    fn pull_request_number_is_parsed_from_github_url() {
        let item = notification(Some("https://github.com/acme/repo/pull/123"));

        assert_eq!(
            super::pull_request_number_from_url(item.pull_request_url().expect("PR URL")),
            Some(123)
        );
        assert_eq!(
            item.pull_request_url(),
            Some("https://github.com/acme/repo/pull/123")
        );
    }

    #[test]
    fn pull_request_number_ignores_non_pr_urls() {
        let item = notification(Some("https://github.com/acme/repo/issues/123"));

        assert_eq!(item.pull_request_url(), None);
        assert_eq!(
            super::pull_request_number_from_url("https://github.com/acme/repo/issues/123"),
            None
        );
    }

    #[test]
    fn pull_request_number_handles_nested_pr_urls() {
        let item = notification(Some("https://github.com/acme/repo/pull/456/files#diff-1"));

        assert_eq!(
            super::pull_request_number_from_url(item.pull_request_url().expect("PR URL")),
            Some(456)
        );
    }

    #[test]
    fn thread_number_is_parsed_from_issue_url() {
        let item = notification(Some(
            "https://github.com/acme/repo/issues/789#issuecomment-1",
        ));

        assert_eq!(item.thread_number(), Some(789));
        assert_eq!(
            super::issue_number_from_url("https://github.com/acme/repo/issues/789"),
            Some(789)
        );
    }

    #[test]
    fn display_title_appends_pull_request_number_as_suffix() {
        let item = notification(Some("https://github.com/acme/repo/pull/123"));

        assert_eq!(item.display_title(), "#123 Title");
    }

    #[test]
    fn display_title_appends_issue_number_as_suffix() {
        let item = notification(Some("https://github.com/acme/repo/issues/321"));

        assert_eq!(item.display_title(), "#321 Title");
    }

    #[test]
    fn display_title_leaves_non_issue_or_pr_titles_unchanged() {
        let item = notification(Some("https://github.com/acme/repo/discussions/99"));

        assert_eq!(item.display_title(), "Title");
    }
}
