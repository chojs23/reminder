use std::{collections::BTreeMap, fs};

pub(super) fn canonical_repo_key(repo: &str) -> Option<String> {
    let parts: Vec<_> = repo.trim().split('/').collect();
    if parts.len() != 2 || parts.iter().any(|part| part.is_empty()) {
        return None;
    }

    Some(format!(
        "{}/{}",
        parts[0].to_lowercase(),
        parts[1].to_lowercase()
    ))
}

pub(super) fn normalize_hydrated_repo_paths(
    repo_paths: BTreeMap<String, String>,
) -> (BTreeMap<String, String>, usize) {
    let mut normalized = BTreeMap::new();
    let mut dropped = 0;

    for (repo, path) in repo_paths {
        let Some(repo_key) = canonical_repo_key(&repo) else {
            dropped += 1;
            continue;
        };

        let Ok(canonical_path) = fs::canonicalize(&path) else {
            dropped += 1;
            continue;
        };

        if !canonical_path.is_dir() {
            dropped += 1;
            continue;
        }

        normalized.insert(repo_key, canonical_path.display().to_string());
    }

    (normalized, dropped)
}

pub(super) fn normalize_hydrated_repo_path_accounts(
    repo_path_accounts: BTreeMap<String, String>,
    repo_paths: &BTreeMap<String, String>,
    account_logins: &[String],
) -> (BTreeMap<String, String>, usize) {
    let mut normalized = BTreeMap::new();
    let mut dropped = 0;

    for (repo, login) in repo_path_accounts {
        let Some(repo_key) = canonical_repo_key(&repo) else {
            dropped += 1;
            continue;
        };
        if !repo_paths.contains_key(&repo_key) {
            dropped += 1;
            continue;
        }

        let trimmed_login = login.trim();
        let Some(canonical_login) = account_logins
            .iter()
            .find(|candidate| candidate.eq_ignore_ascii_case(trimmed_login))
        else {
            dropped += 1;
            continue;
        };

        normalized.insert(repo_key, canonical_login.clone());
    }

    (normalized, dropped)
}

#[cfg(test)]
mod tests {
    use super::{
        canonical_repo_key, normalize_hydrated_repo_path_accounts, normalize_hydrated_repo_paths,
    };
    use std::{
        collections::BTreeMap,
        env, fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn canonical_repo_key_normalizes_case() {
        assert_eq!(
            canonical_repo_key("Acme/Widgets"),
            Some(String::from("acme/widgets"))
        );
    }

    #[test]
    fn normalize_repo_path_accounts_matches_existing_login_case_insensitively() {
        let repo_paths = BTreeMap::from([(String::from("acme/repo"), String::from("/tmp/repo"))]);
        let repo_path_accounts = BTreeMap::from([(String::from("Acme/Repo"), String::from("Neo"))]);
        let account_logins = vec![String::from("neo")];

        let (normalized, dropped) =
            normalize_hydrated_repo_path_accounts(repo_path_accounts, &repo_paths, &account_logins);

        assert_eq!(normalized.get("acme/repo"), Some(&String::from("neo")));
        assert_eq!(dropped, 0);
    }

    #[test]
    fn normalize_hydrated_repo_paths_accepts_plain_directories() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let temp_dir = env::temp_dir().join(format!(
            "reminder-repo-path-dir-only-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&temp_dir).expect("temp dir");

        let (normalized, dropped) = normalize_hydrated_repo_paths(BTreeMap::from([(
            String::from("Acme/Repo"),
            temp_dir.display().to_string(),
        )]));

        assert_eq!(dropped, 0);
        assert_eq!(
            normalized.get("acme/repo"),
            Some(
                &fs::canonicalize(&temp_dir)
                    .expect("canonical dir")
                    .display()
                    .to_string()
            )
        );

        fs::remove_dir_all(temp_dir).expect("cleanup temp dir");
    }
}
