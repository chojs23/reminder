use std::{env, fs, io, path::PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::GitHubAccount;

const STORAGE_DIR_NAME: &str = ".reminder";
const REGISTRY_FILE: &str = "accounts.json";

#[derive(Default, Serialize, Deserialize, Clone)]
pub struct StoredAccounts {
    pub accounts: Vec<StoredAccount>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct StoredAccount {
    pub login: String,
    pub token: String,
}

impl StoredAccounts {
    fn upsert(&mut self, login: &str, token: &str) {
        if let Some(existing) = self.accounts.iter_mut().find(|entry| entry.login == login) {
            existing.token = token.to_owned();
        } else {
            self.accounts.push(StoredAccount {
                login: login.to_owned(),
                token: token.to_owned(),
            });
            self.accounts.sort_by(|a, b| a.login.cmp(&b.login));
        }
    }

    fn remove(&mut self, login: &str) {
        self.accounts.retain(|entry| entry.login != login);
    }
}

pub struct AccountStore {
    registry_path: PathBuf,
}

pub struct HydrationOutcome {
    pub profiles: Vec<GitHubAccount>,
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
            })
            .collect();

        Ok(HydrationOutcome { profiles })
    }

    pub fn persist_profile(&self, profile: &GitHubAccount) -> Result<(), SecretStoreError> {
        let mut registry = self.read_registry()?;
        registry.upsert(&profile.login, &profile.token);
        self.write_registry(&registry)?;
        Ok(())
    }

    pub fn forget(&self, login: &str) -> Result<(), SecretStoreError> {
        let mut registry = self.read_registry()?;
        registry.remove(login);
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
