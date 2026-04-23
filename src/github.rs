use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use reqwest::{
    blocking::Client,
    header::{ACCEPT, USER_AGENT},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{
    GitHubAccount, InboxSnapshot, MentionKind, MentionThread, NotificationItem, PullRequestKey,
    PullRequestReviewer, PullRequestReviewerStatus, PullRequestReviewers, RepoPullRequest,
    RepoPullRequestSnapshot, ReviewRequest, ReviewSummary,
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

pub fn fetch_notification_metadata_updates(
    client: &Client,
    profile: &GitHubAccount,
    notifications: &[NotificationItem],
) -> Result<Vec<NotificationMetadataUpdate>, FetchError> {
    let mut metadata_cache = BTreeMap::<PullRequestKey, NotificationPullRequestMetadata>::new();
    let mut updates = Vec::new();

    for item in notifications {
        let metadata = notification_pull_request_metadata(
            client,
            profile,
            &item.repo,
            item.url.as_deref(),
            &mut metadata_cache,
        )?;
        if metadata == NotificationPullRequestMetadata::default() {
            continue;
        }
        updates.push(NotificationMetadataUpdate {
            thread_id: item.thread_id.clone(),
            head_ref: metadata.head_ref,
            base_ref: metadata.base_ref,
            my_review_status: metadata.my_review_status,
        });
    }

    Ok(updates)
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

    let mut pull_requests = Vec::with_capacity(response.len());
    for item in response {
        let my_review_status =
            fetch_latest_review_status_for_user(client, profile, repo, item.number).unwrap_or(None);
        pull_requests.push(RepoPullRequest {
            repo: repo.to_owned(),
            number: item.number,
            title: item.title,
            url: item.html_url,
            head_ref: item.head.r#ref,
            base_ref: item.base.r#ref,
            updated_at: item.updated_at,
            author_login: item.user.map(|user| user.login),
            draft: item.draft,
            my_review_status,
        });
    }

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
    let latest_review_states = latest_submitted_reviews_by_reviewer(fetch_pull_request_reviews(
        client, profile, repo, pr_number,
    )?);
    let reviewer_history = review_request_history_from_issue_events(&issue_events);

    Ok(PullRequestReviewers {
        current_reviewers: build_current_reviewers(
            &requested_reviewers,
            &reviewer_history,
            &latest_review_request_times_from_issue_events(&issue_events),
            &latest_review_states,
        ),
        requested_reviewers,
        reviewer_history,
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

fn fetch_pull_request_reviews(
    client: &Client,
    profile: &GitHubAccount,
    repo: &str,
    pr_number: u64,
) -> Result<Vec<PullRequestReviewResponse>, FetchError> {
    let url = format!("{GH_REPOS}/{repo}/pulls/{pr_number}/reviews");
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
            url: item
                .subject
                .url
                .as_deref()
                .map(normalize_notification_subject_url),
            head_ref: None,
            base_ref: None,
            my_review_status: None,
            reason: item.reason,
            updated_at: item.updated_at,
            last_read_at: item.last_read_at,
            unread: item.unread,
        })
        .collect())
}

fn normalize_notification_subject_url(url: &str) -> String {
    let mut html = url.replace("api.github.com/repos", "github.com");
    html = html.replace("/pulls/", "/pull/");
    html
}

fn notification_pull_request_metadata(
    client: &Client,
    profile: &GitHubAccount,
    repo: &str,
    url: Option<&str>,
    cache: &mut BTreeMap<PullRequestKey, NotificationPullRequestMetadata>,
) -> Result<NotificationPullRequestMetadata, FetchError> {
    let Some(pr_number) = url.and_then(pull_request_number_from_html_url) else {
        return Ok(NotificationPullRequestMetadata::default());
    };
    let key = (repo.to_owned(), pr_number);
    if let Some(metadata) = cache.get(&key) {
        return Ok(metadata.clone());
    }

    let metadata = fetch_notification_pull_request_metadata(client, profile, repo, pr_number)?;
    cache.insert(key, metadata.clone());
    Ok(metadata)
}

fn pull_request_number_from_html_url(url: &str) -> Option<u64> {
    let (_, suffix) = url.split_once("/pull/")?;
    suffix.split(['/', '?', '#']).next()?.parse().ok()
}

fn fetch_notification_pull_request_metadata(
    client: &Client,
    profile: &GitHubAccount,
    repo: &str,
    pr_number: u64,
) -> Result<NotificationPullRequestMetadata, FetchError> {
    let pull_request = fetch_pull_request(client, profile, repo, pr_number)?;
    Ok(NotificationPullRequestMetadata {
        head_ref: Some(pull_request.head.r#ref),
        base_ref: Some(pull_request.base.r#ref),
        my_review_status: fetch_latest_review_status_for_user(client, profile, repo, pr_number)?,
    })
}

fn fetch_pull_request(
    client: &Client,
    profile: &GitHubAccount,
    repo: &str,
    pr_number: u64,
) -> Result<PullRequestResponse, FetchError> {
    let url = format!("{GH_REPOS}/{repo}/pulls/{pr_number}");
    client
        .get(url)
        .header(USER_AGENT, USER_AGENT_HEADER)
        .header(ACCEPT, "application/vnd.github+json")
        .bearer_auth(&profile.token)
        .send()?
        .error_for_status()?
        .json()
        .map_err(FetchError::Http)
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

fn review_request_history_from_issue_events(events: &[IssueEventResponse]) -> Vec<String> {
    let mut reviewers: Vec<(String, DateTime<Utc>)> = Vec::new();
    for event in events {
        if !matches!(
            event.event.as_str(),
            "review_requested" | "review_request_removed"
        ) {
            continue;
        }
        let Some(requested_reviewer) = event.requested_reviewer.as_ref() else {
            continue;
        };

        if let Some(existing) = reviewers
            .iter_mut()
            .find(|(login, _)| login.eq_ignore_ascii_case(&requested_reviewer.login))
        {
            existing.0 = requested_reviewer.login.clone();
            existing.1 = event.created_at;
        } else {
            reviewers.push((requested_reviewer.login.clone(), event.created_at));
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

fn latest_review_request_times_from_issue_events(
    events: &[IssueEventResponse],
) -> BTreeMap<String, DateTime<Utc>> {
    let mut latest_request_times = BTreeMap::new();
    for event in events {
        if event.event != "review_requested" {
            continue;
        }
        let Some(requested_reviewer) = event.requested_reviewer.as_ref() else {
            continue;
        };
        latest_request_times.insert(
            requested_reviewer.login.to_ascii_lowercase(),
            event.created_at,
        );
    }
    latest_request_times
}

fn latest_submitted_reviews_by_reviewer(
    reviews: Vec<PullRequestReviewResponse>,
) -> BTreeMap<String, SubmittedReviewState> {
    let mut latest_reviews: BTreeMap<String, SubmittedReviewState> = BTreeMap::new();
    for review in reviews {
        let Some(user) = review.user else {
            continue;
        };
        let Some(submitted_at) = review.submitted_at else {
            continue;
        };
        let Some(status) = reviewer_status_from_review_state(&review.state) else {
            continue;
        };
        let key = user.login.to_ascii_lowercase();
        let should_replace = match latest_reviews.get(&key) {
            None => true,
            Some(current) => {
                submitted_at > current.submitted_at
                    || (submitted_at == current.submitted_at && review.id > current.review_id)
            }
        };
        if should_replace {
            latest_reviews.insert(
                key,
                SubmittedReviewState {
                    review_id: review.id,
                    submitted_at,
                    status,
                },
            );
        }
    }
    latest_reviews
}

fn latest_submitted_review_for_user(
    reviews: Vec<PullRequestReviewResponse>,
    reviewer_login: &str,
) -> Option<PullRequestReviewResponse> {
    let reviewer_key = reviewer_login.to_ascii_lowercase();
    let mut latest_review: Option<PullRequestReviewResponse> = None;

    for review in reviews {
        let Some(user) = review.user.as_ref() else {
            continue;
        };
        if user.login.to_ascii_lowercase() != reviewer_key {
            continue;
        }
        let Some(submitted_at) = review.submitted_at else {
            continue;
        };
        let should_replace = match latest_review.as_ref() {
            None => true,
            Some(current) => {
                let current_submitted_at = current
                    .submitted_at
                    .expect("stored submitted review always keeps timestamp");
                submitted_at > current_submitted_at
                    || (submitted_at == current_submitted_at && review.id > current.id)
            }
        };
        if should_replace {
            latest_review = Some(review);
        }
    }

    latest_review
}

fn review_is_stale_after_re_request(
    review: &PullRequestReviewResponse,
    latest_request_times: &BTreeMap<String, DateTime<Utc>>,
    reviewer_login: &str,
) -> bool {
    latest_request_times
        .get(&reviewer_login.to_ascii_lowercase())
        .is_some_and(|requested_at| {
            review
                .submitted_at
                .is_some_and(|submitted_at| submitted_at < *requested_at)
        })
}

fn fetch_latest_review_status_for_user(
    client: &Client,
    profile: &GitHubAccount,
    repo: &str,
    pr_number: u64,
) -> Result<Option<PullRequestReviewerStatus>, FetchError> {
    let latest_review = latest_submitted_review_for_user(
        fetch_pull_request_reviews(client, profile, repo, pr_number)?,
        &profile.login,
    );
    let Some(review) = latest_review else {
        return Ok(None);
    };

    let latest_request_times = latest_review_request_times_from_issue_events(&fetch_issue_events(
        client, profile, repo, pr_number,
    )?);
    if review_is_stale_after_re_request(&review, &latest_request_times, &profile.login) {
        return Ok(None);
    }

    Ok(reviewer_status_from_review_state(&review.state))
}

fn build_current_reviewers(
    requested_reviewers: &[String],
    reviewer_history: &[String],
    latest_review_request_times: &BTreeMap<String, DateTime<Utc>>,
    latest_review_states: &BTreeMap<String, SubmittedReviewState>,
) -> Vec<PullRequestReviewer> {
    let requested_keys: HashSet<_> = requested_reviewers
        .iter()
        .map(|login| login.to_ascii_lowercase())
        .collect();
    let mut seen = HashSet::new();
    let mut current_reviewers = Vec::new();

    for login in requested_reviewers.iter().chain(reviewer_history.iter()) {
        let key = login.to_ascii_lowercase();
        if !seen.insert(key.clone()) {
            continue;
        }

        if requested_keys.contains(&key) {
            current_reviewers.push(PullRequestReviewer {
                login: login.clone(),
                status: PullRequestReviewerStatus::Pending,
            });
            continue;
        }

        let Some(review) = latest_review_states.get(&key) else {
            continue;
        };
        if latest_review_request_times
            .get(&key)
            .is_some_and(|requested_at| review.submitted_at < *requested_at)
        {
            continue;
        }

        current_reviewers.push(PullRequestReviewer {
            login: login.clone(),
            status: review.status,
        });
    }

    current_reviewers
}

fn reviewer_status_from_review_state(state: &str) -> Option<PullRequestReviewerStatus> {
    match state {
        "APPROVED" => Some(PullRequestReviewerStatus::Approved),
        "CHANGES_REQUESTED" => Some(PullRequestReviewerStatus::ChangesRequested),
        "COMMENTED" => Some(PullRequestReviewerStatus::Commented),
        _ => None,
    }
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
pub type NotificationMetadataOutcome = Result<Vec<NotificationMetadataUpdate>, FetchError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationMetadataUpdate {
    pub thread_id: String,
    pub head_ref: Option<String>,
    pub base_ref: Option<String>,
    pub my_review_status: Option<PullRequestReviewerStatus>,
}

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
    head: PullRequestBranchRef,
    base: PullRequestBranchRef,
    updated_at: DateTime<Utc>,
    user: Option<GitHubUser>,
    #[serde(default)]
    draft: bool,
}

#[derive(Debug, Deserialize)]
struct PullRequestBranchRef {
    r#ref: String,
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

#[derive(Debug, Deserialize)]
struct PullRequestReviewResponse {
    id: u64,
    state: String,
    user: Option<GitHubUser>,
    submitted_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug)]
struct SubmittedReviewState {
    review_id: u64,
    submitted_at: DateTime<Utc>,
    status: PullRequestReviewerStatus,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct NotificationPullRequestMetadata {
    head_ref: Option<String>,
    base_ref: Option<String>,
    my_review_status: Option<PullRequestReviewerStatus>,
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
            review_request_history_from_issue_events(&events),
            vec![String::from("neo"), String::from("trinity")]
        );
    }

    #[test]
    fn build_current_reviewers_keeps_completed_reviewers_visible() {
        let requested_reviewers = vec![String::from("trinity")];
        let reviewer_history = vec![String::from("trinity"), String::from("neo")];
        let latest_request_times = BTreeMap::from([
            (
                String::from("trinity"),
                "2026-04-03T00:00:00Z".parse().unwrap(),
            ),
            (String::from("neo"), "2026-04-01T00:00:00Z".parse().unwrap()),
        ]);
        let latest_review_states = BTreeMap::from([(
            String::from("neo"),
            SubmittedReviewState {
                review_id: 7,
                submitted_at: "2026-04-02T00:00:00Z".parse().unwrap(),
                status: PullRequestReviewerStatus::Approved,
            },
        )]);

        assert_eq!(
            build_current_reviewers(
                &requested_reviewers,
                &reviewer_history,
                &latest_request_times,
                &latest_review_states,
            ),
            vec![
                PullRequestReviewer {
                    login: String::from("trinity"),
                    status: PullRequestReviewerStatus::Pending,
                },
                PullRequestReviewer {
                    login: String::from("neo"),
                    status: PullRequestReviewerStatus::Approved,
                },
            ]
        );
    }

    #[test]
    fn build_current_reviewers_ignores_stale_reviews_after_re_request() {
        let requested_reviewers = Vec::new();
        let reviewer_history = vec![String::from("neo")];
        let latest_request_times =
            BTreeMap::from([(String::from("neo"), "2026-04-03T00:00:00Z".parse().unwrap())]);
        let latest_review_states = BTreeMap::from([(
            String::from("neo"),
            SubmittedReviewState {
                review_id: 7,
                submitted_at: "2026-04-02T00:00:00Z".parse().unwrap(),
                status: PullRequestReviewerStatus::Approved,
            },
        )]);

        assert!(
            build_current_reviewers(
                &requested_reviewers,
                &reviewer_history,
                &latest_request_times,
                &latest_review_states,
            )
            .is_empty()
        );
    }

    #[test]
    fn latest_submitted_review_for_user_tracks_latest_submitted_state() {
        let reviews = vec![
            PullRequestReviewResponse {
                id: 1,
                state: String::from("COMMENTED"),
                user: Some(GitHubUser {
                    login: String::from("neo"),
                }),
                submitted_at: Some("2026-04-01T00:00:00Z".parse().unwrap()),
            },
            PullRequestReviewResponse {
                id: 2,
                state: String::from("APPROVED"),
                user: Some(GitHubUser {
                    login: String::from("neo"),
                }),
                submitted_at: Some("2026-04-02T00:00:00Z".parse().unwrap()),
            },
        ];

        let review = latest_submitted_review_for_user(reviews, "neo").expect("review");

        assert_eq!(
            reviewer_status_from_review_state(&review.state),
            Some(PullRequestReviewerStatus::Approved)
        );
    }

    #[test]
    fn latest_submitted_review_for_user_clears_older_approval_after_unmapped_state() {
        let reviews = vec![
            PullRequestReviewResponse {
                id: 1,
                state: String::from("APPROVED"),
                user: Some(GitHubUser {
                    login: String::from("neo"),
                }),
                submitted_at: Some("2026-04-01T00:00:00Z".parse().unwrap()),
            },
            PullRequestReviewResponse {
                id: 2,
                state: String::from("DISMISSED"),
                user: Some(GitHubUser {
                    login: String::from("neo"),
                }),
                submitted_at: Some("2026-04-02T00:00:00Z".parse().unwrap()),
            },
        ];

        let review = latest_submitted_review_for_user(reviews, "neo").expect("review");

        assert_eq!(reviewer_status_from_review_state(&review.state), None);
    }

    #[test]
    fn review_is_stale_after_re_request_ignores_stale_approval() {
        let reviews = vec![PullRequestReviewResponse {
            id: 1,
            state: String::from("APPROVED"),
            user: Some(GitHubUser {
                login: String::from("neo"),
            }),
            submitted_at: Some("2026-04-01T00:00:00Z".parse().unwrap()),
        }];
        let latest_request_times = BTreeMap::from([(
            String::from("neo"),
            "2026-04-02T00:00:00Z".parse::<DateTime<Utc>>().unwrap(),
        )]);
        let latest_review = latest_submitted_review_for_user(reviews, "neo").expect("review");

        assert!(review_is_stale_after_re_request(
            &latest_review,
            &latest_request_times,
            "neo"
        ));
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
