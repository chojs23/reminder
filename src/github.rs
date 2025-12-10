use chrono::{DateTime, Utc};
use reqwest::{
    blocking::Client,
    header::{ACCEPT, USER_AGENT},
};
use serde::Deserialize;
use thiserror::Error;

use crate::domain::{
    GitHubAccount, InboxSnapshot, MentionKind, MentionThread, NotificationItem, ReviewRequest,
    ReviewSummary,
};

const GH_NOTIFICATIONS: &str = "https://api.github.com/notifications";
const GH_NOTIFICATION_THREAD: &str = "https://api.github.com/notifications/threads";
const GH_SEARCH_ISSUES: &str = "https://api.github.com/search/issues";
const USER_AGENT_HEADER: &str = "reminder-egui/0.1";

pub fn build_client() -> Result<Client, FetchError> {
    Client::builder()
        .user_agent(USER_AGENT_HEADER)
        .build()
        .map_err(FetchError::Http)
}

pub fn fetch_inbox(client: &Client, profile: &GitHubAccount) -> Result<InboxSnapshot, FetchError> {
    if profile.token.is_empty() {
        return Err(FetchError::MissingToken);
    }

    let notifications = fetch_notifications(client, profile)?;
    let review_requests = fetch_review_requests(client, profile)?;
    let mentions = fetch_mentions(client, profile)?;
    let recent_reviews = fetch_recent_reviews(client, profile)?;

    Ok(InboxSnapshot {
        notifications,
        review_requests,
        mentions,
        recent_reviews,
        fetched_at: Utc::now(),
    })
}

pub fn mark_notification_done(
    client: &Client,
    profile: &GitHubAccount,
    thread_id: &str,
) -> Result<(), FetchError> {
    // This endpoint remains for future use, but UI-triggered "Done" actions are
    // currently disabled because GitHub's notifications feed cannot be filtered
    // to exclude already-archived items. Removing the call entirely would make
    // re-enabling the workflow harder if GitHub adds proper server-side filtering.
    if profile.token.is_empty() {
        return Err(FetchError::MissingToken);
    }

    let url = format!("{GH_NOTIFICATION_THREAD}/{thread_id}");
    client
        .delete(url)
        .header(USER_AGENT, USER_AGENT_HEADER)
        .header(ACCEPT, "application/vnd.github+json")
        .bearer_auth(&profile.token)
        .send()?
        .error_for_status()?;
    Ok(())
}

pub fn mark_notification_read(
    client: &Client,
    profile: &GitHubAccount,
    thread_id: &str,
) -> Result<(), FetchError> {
    if profile.token.is_empty() {
        return Err(FetchError::MissingToken);
    }

    let url = format!("{GH_NOTIFICATION_THREAD}/{thread_id}");
    client
        .patch(url)
        .header(USER_AGENT, USER_AGENT_HEADER)
        .header(ACCEPT, "application/vnd.github+json")
        .bearer_auth(&profile.token)
        .send()?
        .error_for_status()?;
    Ok(())
}

fn fetch_notifications(
    client: &Client,
    profile: &GitHubAccount,
) -> Result<Vec<NotificationItem>, FetchError> {
    let response: Vec<NotificationResponse> = client
        .get(GH_NOTIFICATIONS)
        .query(&[("all", "true")])
        .header(USER_AGENT, USER_AGENT_HEADER)
        .header(ACCEPT, "application/vnd.github+json")
        .bearer_auth(&profile.token)
        .send()?
        .error_for_status()?
        .json()?;

    Ok(response
        .into_iter()
        .map(|item| NotificationItem {
            thread_id: item.id,
            repo: item.repository.full_name,
            title: item.subject.title,
            url: item.subject.url.as_deref().map(|url| {
                let mut html = url.replace("api.github.com/repos", "github.com");
                // GitHub API uses `/pulls/` in the notifications subject URL, but the
                // human-facing page lives at `/pull/`. Normalize so hyperlinks open
                // the right PR page instead of the list view.
                html = html.replace("/pulls/", "/pull/");
                html
            }),
            reason: item.reason,
            updated_at: item.updated_at,
            last_read_at: item.last_read_at,
            unread: item.unread,
        })
        .collect())
}

fn fetch_review_requests(
    client: &Client,
    profile: &GitHubAccount,
) -> Result<Vec<ReviewRequest>, FetchError> {
    let query = format!("is:pr state:open review-requested:{}", profile.login);
    let response: SearchResponse = client
        .get(GH_SEARCH_ISSUES)
        .query(&[("q", query.as_str())])
        .header(USER_AGENT, USER_AGENT_HEADER)
        .header(ACCEPT, "application/vnd.github+json")
        .bearer_auth(&profile.token)
        .send()?
        .error_for_status()?
        .json()?;

    Ok(response
        .items
        .into_iter()
        .map(|item| ReviewRequest {
            _id: item.id,
            repo: extract_repo_name(&item.repository_url),
            title: format!("#{} {}", item.number, item.title),
            url: item.html_url,
            updated_at: item.updated_at,
            requested_by: item.user.map(|user| user.login),
        })
        .collect())
}

fn fetch_mentions(
    client: &Client,
    profile: &GitHubAccount,
) -> Result<Vec<MentionThread>, FetchError> {
    let query = format!("mentions:{} is:open", profile.login);
    let response: SearchResponse = client
        .get(GH_SEARCH_ISSUES)
        .query(&[
            ("q", query.as_str()),
            ("sort", "updated"),
            ("order", "desc"),
        ])
        .header(USER_AGENT, USER_AGENT_HEADER)
        .header(ACCEPT, "application/vnd.github+json")
        .bearer_auth(&profile.token)
        .send()?
        .error_for_status()?
        .json()?;

    Ok(response
        .items
        .into_iter()
        .map(|item| {
            let kind = classify_thread(&item.html_url);
            MentionThread {
                _id: item.id,
                repo: extract_repo_name(&item.repository_url),
                title: format!("#{} {}", item.number, item.title),
                url: item.html_url,
                updated_at: item.updated_at,
                kind,
            }
        })
        .collect())
}

fn fetch_recent_reviews(
    client: &Client,
    profile: &GitHubAccount,
) -> Result<Vec<ReviewSummary>, FetchError> {
    let query = format!("is:pr reviewed-by:{}", profile.login);
    let response: SearchResponse = client
        .get(GH_SEARCH_ISSUES)
        .query(&[
            ("q", query.as_str()),
            ("sort", "updated"),
            ("order", "desc"),
        ])
        .header(USER_AGENT, USER_AGENT_HEADER)
        .header(ACCEPT, "application/vnd.github+json")
        .bearer_auth(&profile.token)
        .send()?
        .error_for_status()?
        .json()?;

    Ok(response
        .items
        .into_iter()
        .map(|item| ReviewSummary {
            _id: item.id,
            repo: extract_repo_name(&item.repository_url),
            title: format!("#{} {}", item.number, item.title),
            url: item.html_url,
            updated_at: item.updated_at,
            state: item.state,
        })
        .collect())
}

fn classify_thread(url: &str) -> MentionKind {
    if url.contains("/pull/") {
        MentionKind::PullRequest
    } else {
        MentionKind::Issue
    }
}

fn extract_repo_name(api_url: &str) -> String {
    api_url
        .trim_start_matches("https://api.github.com/repos/")
        .to_owned()
}

pub type FetchOutcome = Result<InboxSnapshot, FetchError>;

#[derive(Error, Debug)]
pub enum FetchError {
    #[error("GitHub API request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Account token is missing")]
    MissingToken,
    #[error("Background worker disconnected before returning a result")]
    BackgroundWorkerGone,
}

// Response payloads ---------------------------------------------------------

#[derive(Debug, Deserialize)]
struct NotificationResponse {
    id: String,
    reason: String,
    updated_at: DateTime<Utc>,
    last_read_at: Option<DateTime<Utc>>,
    unread: bool,
    subject: NotificationSubject,
    repository: NotificationRepository,
}

#[derive(Debug, Deserialize)]
struct NotificationSubject {
    title: String,
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NotificationRepository {
    full_name: String,
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_thread_distinguishes_pr_and_issue() {
        assert!(matches!(
            classify_thread("https://api.github.com/repos/acme/r/pull/1"),
            MentionKind::PullRequest
        ));
        assert!(matches!(
            classify_thread("https://api.github.com/repos/acme/r/issues/2"),
            MentionKind::Issue
        ));
    }

    #[test]
    fn extract_repo_name_trims_prefix() {
        let repo = extract_repo_name("https://api.github.com/repos/acme/widgets");
        assert_eq!(repo, "acme/widgets");
    }

    #[test]
    fn mark_notification_read_requires_token() {
        let client = build_client().expect("client");
        let profile = GitHubAccount {
            login: "user".into(),
            token: String::new(),
        };
        let result = mark_notification_read(&client, &profile, "thread123");
        assert!(matches!(result, Err(FetchError::MissingToken)));
    }
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    items: Vec<SearchItem>,
}

#[derive(Debug, Deserialize)]
struct SearchItem {
    id: u64,
    html_url: String,
    repository_url: String,
    title: String,
    number: u64,
    updated_at: DateTime<Utc>,
    user: Option<GitHubUser>,
    state: String,
}

#[derive(Debug, Deserialize)]
struct GitHubUser {
    login: String,
}
