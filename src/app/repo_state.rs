use std::{
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::Duration,
};

use chrono::Utc;

use crate::{
    domain::{GitHubAccount, RepoPullRequestSnapshot},
    github::{self, FetchError},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum RepoSortMode {
    #[default]
    Default,
    ReviewRequest,
    Updated,
}

pub(super) struct RepoState {
    pub(super) repo: String,
    pub(super) snapshot: Option<RepoPullRequestSnapshot>,
    pub(super) last_error: Option<String>,
    pub(super) pending_job: Option<PendingRepoJob>,
    pub(super) search_query: String,
    pub(super) sort_mode: RepoSortMode,
    pub(super) loaded_by_login: Option<String>,
    pending_login: Option<String>,
}

impl RepoState {
    pub(super) fn new(repo: String) -> Self {
        Self {
            repo,
            snapshot: None,
            last_error: None,
            pending_job: None,
            search_query: String::new(),
            sort_mode: RepoSortMode::Default,
            loaded_by_login: None,
            pending_login: None,
        }
    }

    pub(super) fn start_refresh(&mut self, profile: GitHubAccount) {
        self.last_error = None;
        self.pending_login = Some(profile.login.clone());
        self.pending_job = Some(PendingRepoJob::spawn(profile, self.repo.clone()));
    }

    pub(super) fn poll_job(&mut self) {
        if let Some(job) = &mut self.pending_job
            && let Some(result) = job.try_take()
        {
            self.pending_job = None;
            match result {
                Ok(snapshot) => {
                    self.snapshot = Some(snapshot);
                    self.loaded_by_login = self.pending_login.take();
                    self.last_error = None;
                }
                Err(err) => {
                    self.pending_login = None;
                    self.last_error = Some(err.to_string());
                }
            }
        }
    }

    pub(super) fn needs_refresh(&self, threshold: Duration) -> bool {
        match &self.snapshot {
            None => true,
            Some(snapshot) => match chrono::Duration::from_std(threshold) {
                Ok(delta) => (Utc::now() - snapshot.fetched_at) >= delta,
                Err(_) => true,
            },
        }
    }

    pub(super) fn should_refresh_with(&self, login: &str, threshold: Duration) -> bool {
        self.pending_job.is_none()
            && (self.loaded_by_login.as_deref() != Some(login) || self.needs_refresh(threshold))
    }
}

pub(super) struct PendingRepoJob {
    receiver: Receiver<github::RepoFetchOutcome>,
}

impl PendingRepoJob {
    fn spawn(profile: GitHubAccount, repo: String) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let outcome = (|| -> github::RepoFetchOutcome {
                let client = github::build_client()?;
                github::fetch_repo_pull_requests(&client, &profile, &repo)
            })();
            let _ = tx.send(outcome);
        });
        Self { receiver: rx }
    }

    fn try_take(&self) -> Option<github::RepoFetchOutcome> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(FetchError::BackgroundWorkerGone)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RepoState;
    use crate::domain::RepoPullRequestSnapshot;
    use chrono::Utc;
    use std::time::Duration;

    #[test]
    fn repo_state_refreshes_when_account_changes() {
        let mut state = RepoState::new(String::from("acme/repo"));
        state.loaded_by_login = Some(String::from("neo"));
        state.snapshot = Some(RepoPullRequestSnapshot {
            pull_requests: Vec::new(),
            fetched_at: Utc::now(),
        });

        assert!(state.should_refresh_with("trinity", Duration::from_secs(60)));
        assert!(!state.should_refresh_with("neo", Duration::from_secs(60)));
    }
}
