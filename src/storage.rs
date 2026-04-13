use std::{collections::BTreeMap, env, fs, io, path::PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{GitHubAccount, ReviewCommandSettings};

const STORAGE_DIR_NAME: &str = ".reminder";
const REGISTRY_FILE: &str = "accounts.json";

#[derive(Default, Serialize, Deserialize, Clone)]
pub struct StoredAccounts {
    #[serde(default)]
    pub accounts: Vec<StoredAccount>,
    #[serde(default)]
    pub repo_paths: BTreeMap<String, String>,
    #[serde(default)]
    pub repo_path_accounts: BTreeMap<String, String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct StoredAccount {
    pub login: String,
    pub token: String,
    #[serde(default)]
    pub review_settings: ReviewCommandSettings,
}

impl StoredAccounts {
    fn upsert(&mut self, profile: &GitHubAccount) {
        if let Some(existing) = self
            .accounts
            .iter_mut()
            .find(|entry| entry.login == profile.login)
        {
            existing.token = profile.token.clone();
            existing.review_settings = profile.review_settings.clone();
        } else {
            self.accounts.push(StoredAccount {
                login: profile.login.clone(),
                token: profile.token.clone(),
                review_settings: profile.review_settings.clone(),
            });
            self.accounts.sort_by(|a, b| a.login.cmp(&b.login));
        }
    }

    fn remove(&mut self, login: &str) {
        self.accounts.retain(|entry| entry.login != login);
    }

    fn upsert_repo_path(&mut self, repo: &str, path: &str) {
        self.repo_paths.insert(repo.to_owned(), path.to_owned());
    }

    fn upsert_repo_path_account(&mut self, repo: &str, login: &str) {
        self.repo_path_accounts
            .insert(repo.to_owned(), login.to_owned());
    }

    fn remove_repo_path(&mut self, repo: &str) {
        self.repo_paths.remove(repo);
        self.repo_path_accounts.remove(repo);
    }

    fn remove_repo_path_account(&mut self, repo: &str) {
        self.repo_path_accounts.remove(repo);
    }
}

pub struct AccountStore {
    registry_path: PathBuf,
}

pub struct HydrationOutcome {
    pub profiles: Vec<GitHubAccount>,
    pub repo_paths: BTreeMap<String, String>,
    pub repo_path_accounts: BTreeMap<String, String>,
}

impl AccountStore {
    pub fn initialize() -> Result<Self, SecretStoreError> {
        let home = env::var("HOME").map_err(|_| SecretStoreError::HomeDirMissing)?;
        let dir = PathBuf::from(home).join(STORAGE_DIR_NAME);
        if !dir.exists() {
            fs::create_dir_all(&dir)?;
        }
        Ok(Self {
            registry_path: dir.join(REGISTRY_FILE),
        })
    }

    pub fn hydrate(&self) -> Result<HydrationOutcome, SecretStoreError> {
        let registry = self.read_registry()?;
        let profiles = registry
            .accounts
            .into_iter()
            .map(|entry| GitHubAccount {
                login: entry.login,
                token: entry.token,
                review_settings: entry.review_settings,
            })
            .collect();

        Ok(HydrationOutcome {
            profiles,
            repo_paths: registry.repo_paths,
            repo_path_accounts: registry.repo_path_accounts,
        })
    }

    pub fn persist_profile(&self, profile: &GitHubAccount) -> Result<(), SecretStoreError> {
        let mut registry = self.read_registry()?;
        registry.upsert(profile);
        self.write_registry(&registry)?;
        Ok(())
    }

    pub fn forget(&self, login: &str) -> Result<(), SecretStoreError> {
        let mut registry = self.read_registry()?;
        registry.remove(login);
        self.write_registry(&registry)?;
        Ok(())
    }

    pub fn persist_repo_path(
        &self,
        repo: &str,
        path: &str,
        login: Option<&str>,
    ) -> Result<(), SecretStoreError> {
        let mut registry = self.read_registry()?;
        registry.upsert_repo_path(repo, path);
        if let Some(login) = login {
            registry.upsert_repo_path_account(repo, login);
        } else {
            registry.remove_repo_path_account(repo);
        }
        self.write_registry(&registry)?;
        Ok(())
    }

    pub fn persist_repo_path_account(
        &self,
        repo: &str,
        login: &str,
    ) -> Result<(), SecretStoreError> {
        let mut registry = self.read_registry()?;
        registry.upsert_repo_path_account(repo, login);
        self.write_registry(&registry)?;
        Ok(())
    }

    pub fn clear_repo_path_account(&self, repo: &str) -> Result<(), SecretStoreError> {
        let mut registry = self.read_registry()?;
        registry.remove_repo_path_account(repo);
        self.write_registry(&registry)?;
        Ok(())
    }

    pub fn forget_repo_path(&self, repo: &str) -> Result<(), SecretStoreError> {
        let mut registry = self.read_registry()?;
        registry.remove_repo_path(repo);
        self.write_registry(&registry)?;
        Ok(())
    }

    fn read_registry(&self) -> Result<StoredAccounts, SecretStoreError> {
        match fs::read_to_string(&self.registry_path) {
            Ok(contents) => Ok(serde_json::from_str(&contents)?),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(StoredAccounts::default()),
            Err(err) => Err(err.into()),
        }
    }

    fn write_registry(&self, registry: &StoredAccounts) -> Result<(), SecretStoreError> {
        let data = serde_json::to_string_pretty(registry)?;
        fs::write(&self.registry_path, data)?;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum SecretStoreError {
    #[error("HOME environment variable is not set; cannot store tokens under ~/.reminder")]
    HomeDirMissing,
    #[error("I/O error while handling stored accounts: {0}")]
    Io(#[from] io::Error),
    #[error("Failed to serialize stored accounts: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::StoredAccounts;

    #[test]
    fn stored_accounts_defaults_missing_review_settings() {
        let stored: StoredAccounts = serde_json::from_str(
            r#"{
                "accounts": [{"login": "neo", "token": "secret"}],
                "repo_paths": {"acme/repo": "/tmp/acme-repo"}
            }"#,
        )
        .expect("stored accounts");

        assert_eq!(stored.accounts.len(), 1);
        assert!(stored.accounts[0].review_settings.env_vars.is_empty());
        assert!(
            stored.accounts[0]
                .review_settings
                .additional_args
                .is_empty()
        );
        assert!(
            stored.accounts[0]
                .review_settings
                .review_prompt_md_path
                .is_none()
        );
        assert!(
            stored.accounts[0]
                .review_settings
                .pr_description_md_path
                .is_none()
        );
        assert!(stored.repo_path_accounts.is_empty());
    }
}
