use chrono::{DateTime, Utc};
use reqwest::{
    blocking::Client,
    header::{ACCEPT, USER_AGENT},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{
    GitHubAccount, InboxSnapshot, MentionKind, MentionThread, NotificationItem,
    PullRequestReviewers, RepoPullRequest, RepoPullRequestSnapshot, ReviewRequest, ReviewSummary,
};

const GH_NOTIFICATIONS: &str = "https://api.github.com/notifications";
const GH_NOTIFICATION_THREAD: &str = "https://api.github.com/notifications/threads";
const GH_REPOS: &str = "https://api.github.com/repos";
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

pub fn fetch_repo_pull_requests(
    client: &Client,
    profile: &GitHubAccount,
    repo: &str,
) -> Result<RepoPullRequestSnapshot, FetchError> {
    if profile.token.is_empty() {
        return Err(FetchError::MissingToken);
    }

    let url = format!("{GH_REPOS}/{repo}/pulls");
    let response: Vec<PullRequestResponse> = client
        .get(url)
        .query(&[
            ("state", "open"),
            ("sort", "updated"),
            ("direction", "desc"),
            ("per_page", "100"),
        ])
        .header(USER_AGENT, USER_AGENT_HEADER)
        .header(ACCEPT, "application/vnd.github+json")
        .bearer_auth(&profile.token)
        .send()?
        .error_for_status()?
        .json()?;

    let pull_requests = response
        .into_iter()
        .map(|item| RepoPullRequest {
            repo: repo.to_owned(),
            number: item.number,
            title: item.title,
            url: item.html_url,
            updated_at: item.updated_at,
            author_login: item.user.map(|user| user.login),
            draft: item.draft,
        })
        .collect();

    Ok(RepoPullRequestSnapshot {
        pull_requests,
        fetched_at: Utc::now(),
    })
}

pub fn fetch_pull_request_reviewers(
    client: &Client,
    profile: &GitHubAccount,
    repo: &str,
    pr_number: u64,
) -> Result<PullRequestReviewers, FetchError> {
    if profile.token.is_empty() {
        return Err(FetchError::MissingToken);
    }

    let requested_reviewers = fetch_requested_reviewers(client, profile, repo, pr_number)?;
    let issue_events = fetch_issue_events(client, profile, repo, pr_number)?;

    Ok(PullRequestReviewers {
        requested_reviewers,
        reviewer_history: review_request_history_from_issue_events(issue_events),
    })
}

pub fn request_pull_request_reviewer(
    client: &Client,
    profile: &GitHubAccount,
    repo: &str,
    pr_number: u64,
    reviewer_login: &str,
) -> Result<(), FetchError> {
    if profile.token.is_empty() {
        return Err(FetchError::MissingToken);
    }

    let url = format!("{GH_REPOS}/{repo}/pulls/{pr_number}/requested_reviewers");
    client
        .post(url)
        .header(USER_AGENT, USER_AGENT_HEADER)
        .header(ACCEPT, "application/vnd.github+json")
        .bearer_auth(&profile.token)
        .json(&ReviewRequestMutationBody::new(reviewer_login))
        .send()?
        .error_for_status()?;
    Ok(())
}

pub fn remove_pull_request_reviewer(
    client: &Client,
    profile: &GitHubAccount,
    repo: &str,
    pr_number: u64,
    reviewer_login: &str,
) -> Result<(), FetchError> {
    if profile.token.is_empty() {
        return Err(FetchError::MissingToken);
    }

    let url = format!("{GH_REPOS}/{repo}/pulls/{pr_number}/requested_reviewers");
    client
        .delete(url)
        .header(USER_AGENT, USER_AGENT_HEADER)
        .header(ACCEPT, "application/vnd.github+json")
        .bearer_auth(&profile.token)
        .json(&ReviewRequestMutationBody::new(reviewer_login))
        .send()?
        .error_for_status()?;
    Ok(())
}

fn fetch_requested_reviewers(
    client: &Client,
    profile: &GitHubAccount,
    repo: &str,
    pr_number: u64,
) -> Result<Vec<String>, FetchError> {
    let url = format!("{GH_REPOS}/{repo}/pulls/{pr_number}/requested_reviewers");
    let response = client
        .get(url)
        .header(USER_AGENT, USER_AGENT_HEADER)
        .header(ACCEPT, "application/vnd.github+json")
        .bearer_auth(&profile.token)
        .send()?
        .error_for_status()?
        .json::<RequestedReviewersResponse>()?;

    let mut requested_reviewers: Vec<_> = response
        .users
        .into_iter()
        .map(|user| user.login)
        .chain(
            response
                .teams
                .into_iter()
                .map(|team| team.display_identifier()),
        )
        .collect();
    requested_reviewers.sort_by_key(|login| login.to_ascii_lowercase());
    requested_reviewers.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    Ok(requested_reviewers)
}

fn fetch_issue_events(
    client: &Client,
    profile: &GitHubAccount,
    repo: &str,
    issue_number: u64,
) -> Result<Vec<IssueEventResponse>, FetchError> {
    let url = format!("{GH_REPOS}/{repo}/issues/{issue_number}/events");
    client
        .get(url)
        .query(&[("per_page", "100")])
        .header(USER_AGENT, USER_AGENT_HEADER)
        .header(ACCEPT, "application/vnd.github+json")
        .bearer_auth(&profile.token)
        .send()?
        .error_for_status()?
        .json()
        .map_err(FetchError::Http)
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

    let mut review_requests = Vec::with_capacity(response.items.len());
    for item in response.items {
        let repo = extract_repo_name(&item.repository_url);
        let requested_by = fetch_review_requester_for_user(client, profile, &repo, item.number)?;
        review_requests.push(ReviewRequest {
            _id: item.id,
            repo,
            title: format!("#{} {}", item.number, item.title),
            url: item.html_url,
            updated_at: item.updated_at,
            requested_by,
        });
    }

    Ok(review_requests)
}

fn fetch_review_requester_for_user(
    client: &Client,
    profile: &GitHubAccount,
    repo: &str,
    issue_number: u64,
) -> Result<Option<String>, FetchError> {
    Ok(review_requester_for_user_from_issue_events(
        fetch_issue_events(client, profile, repo, issue_number)?,
        &profile.login,
    ))
}

fn review_requester_for_user_from_issue_events(
    mut events: Vec<IssueEventResponse>,
    reviewer_login: &str,
) -> Option<String> {
    events.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    let mut current_requester = None;
    for event in events {
        let requested_reviewer = event
            .requested_reviewer
            .as_ref()
            .map(|user| user.login.as_str());
        if requested_reviewer != Some(reviewer_login) {
            continue;
        }

        match event.event.as_str() {
            "review_requested" => {
                current_requester = event
                    .review_requester
                    .or(event.actor)
                    .map(|user| user.login);
            }
            "review_request_removed" => current_requester = None,
            _ => {}
        }
    }

    current_requester
}

fn review_request_history_from_issue_events(events: Vec<IssueEventResponse>) -> Vec<String> {
    let mut reviewers: Vec<(String, DateTime<Utc>)> = Vec::new();
    for event in events {
        if !matches!(
            event.event.as_str(),
            "review_requested" | "review_request_removed"
        ) {
            continue;
        }
        let Some(requested_reviewer) = event.requested_reviewer else {
            continue;
        };

        if let Some(existing) = reviewers
            .iter_mut()
            .find(|(login, _)| login.eq_ignore_ascii_case(&requested_reviewer.login))
        {
            existing.0 = requested_reviewer.login;
            existing.1 = event.created_at;
        } else {
            reviewers.push((requested_reviewer.login, event.created_at));
        }
    }

    reviewers.sort_by(|(a_login, a_time), (b_login, b_time)| {
        b_time.cmp(a_time).then_with(|| {
            a_login
                .to_ascii_lowercase()
                .cmp(&b_login.to_ascii_lowercase())
        })
    });
    reviewers.into_iter().map(|(login, _)| login).collect()
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
pub type RepoFetchOutcome = Result<RepoPullRequestSnapshot, FetchError>;

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

#[derive(Debug, Deserialize)]
struct PullRequestResponse {
    html_url: String,
    number: u64,
    title: String,
    updated_at: DateTime<Utc>,
    user: Option<GitHubUser>,
    #[serde(default)]
    draft: bool,
}

#[derive(Debug, Deserialize)]
struct RequestedReviewersResponse {
    #[serde(default)]
    users: Vec<GitHubUser>,
    #[serde(default)]
    teams: Vec<GitHubTeam>,
}

#[derive(Debug, Deserialize)]
struct IssueEventResponse {
    event: String,
    created_at: DateTime<Utc>,
    actor: Option<GitHubUser>,
    requested_reviewer: Option<GitHubUser>,
    review_requester: Option<GitHubUser>,
}

#[derive(Debug, Serialize)]
struct ReviewRequestMutationBody {
    reviewers: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    team_reviewers: Vec<String>,
}

impl ReviewRequestMutationBody {
    fn new(reviewer_login: &str) -> Self {
        if let Some((_, team_slug)) = reviewer_login.split_once('/') {
            return Self {
                reviewers: Vec::new(),
                team_reviewers: vec![team_slug.to_owned()],
            };
        }

        Self {
            reviewers: vec![reviewer_login.to_owned()],
            team_reviewers: Vec::new(),
        }
    }
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
            review_settings: crate::domain::ReviewCommandSettings::default(),
        };
        let result = mark_notification_read(&client, &profile, "thread123");
        assert!(matches!(result, Err(FetchError::MissingToken)));
    }

    #[test]
    fn review_requester_for_user_tracks_latest_active_request() {
        let events = vec![
            IssueEventResponse {
                event: String::from("review_requested"),
                created_at: "2026-04-01T00:00:00Z".parse().unwrap(),
                actor: Some(GitHubUser {
                    login: String::from("author"),
                }),
                requested_reviewer: Some(GitHubUser {
                    login: String::from("neo"),
                }),
                review_requester: Some(GitHubUser {
                    login: String::from("alice"),
                }),
            },
            IssueEventResponse {
                event: String::from("review_request_removed"),
                created_at: "2026-04-02T00:00:00Z".parse().unwrap(),
                actor: Some(GitHubUser {
                    login: String::from("author"),
                }),
                requested_reviewer: Some(GitHubUser {
                    login: String::from("neo"),
                }),
                review_requester: Some(GitHubUser {
                    login: String::from("alice"),
                }),
            },
            IssueEventResponse {
                event: String::from("review_requested"),
                created_at: "2026-04-03T00:00:00Z".parse().unwrap(),
                actor: Some(GitHubUser {
                    login: String::from("author"),
                }),
                requested_reviewer: Some(GitHubUser {
                    login: String::from("neo"),
                }),
                review_requester: Some(GitHubUser {
                    login: String::from("bob"),
                }),
            },
        ];

        assert_eq!(
            review_requester_for_user_from_issue_events(events, "neo"),
            Some(String::from("bob"))
        );
    }

    #[test]
    fn review_request_history_tracks_unique_reviewers_by_latest_event() {
        let events = vec![
            IssueEventResponse {
                event: String::from("review_requested"),
                created_at: "2026-04-01T00:00:00Z".parse().unwrap(),
                actor: None,
                requested_reviewer: Some(GitHubUser {
                    login: String::from("neo"),
                }),
                review_requester: None,
            },
            IssueEventResponse {
                event: String::from("review_request_removed"),
                created_at: "2026-04-03T00:00:00Z".parse().unwrap(),
                actor: None,
                requested_reviewer: Some(GitHubUser {
                    login: String::from("trinity"),
                }),
                review_requester: None,
            },
            IssueEventResponse {
                event: String::from("review_requested"),
                created_at: "2026-04-04T00:00:00Z".parse().unwrap(),
                actor: None,
                requested_reviewer: Some(GitHubUser {
                    login: String::from("neo"),
                }),
                review_requester: None,
            },
        ];

        assert_eq!(
            review_request_history_from_issue_events(events),
            vec![String::from("neo"), String::from("trinity")]
        );
    }

    #[test]
    fn team_identifier_from_html_url_extracts_org_and_slug() {
        assert_eq!(
            team_identifier_from_html_url("https://github.com/orgs/acme/teams/platform"),
            Some(String::from("acme/platform"))
        );
    }

    #[test]
    fn review_request_mutation_body_uses_team_reviewers_for_team_names() {
        let body = ReviewRequestMutationBody::new("acme/platform");
        assert!(body.reviewers.is_empty());
        assert_eq!(body.team_reviewers, vec![String::from("platform")]);
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
    state: String,
}

#[derive(Clone, Debug, Deserialize)]
struct GitHubUser {
    login: String,
}

#[derive(Clone, Debug, Deserialize)]
struct GitHubTeam {
    slug: String,
    #[serde(default)]
    html_url: String,
}

impl GitHubTeam {
    fn display_identifier(&self) -> String {
        team_identifier_from_html_url(&self.html_url).unwrap_or_else(|| self.slug.clone())
    }
}

fn team_identifier_from_html_url(html_url: &str) -> Option<String> {
    let (_, suffix) = html_url.split_once("github.com/orgs/")?;
    let (org, rest) = suffix.split_once("/teams/")?;
    let slug = rest.split(['/', '?', '#']).next()?;
    Some(format!("{org}/{slug}"))
}
