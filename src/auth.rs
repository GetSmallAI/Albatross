use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::dir_migration::Migration;

/// Persistent on-disk credential store.
///
/// Legacy files used `{"<provider>": "<api-key>"}`.  New files may also store
/// typed entries so OAuth providers (`openai-codex`, `grok`, …) that use a
/// subscription login rather than an API key can live beside API keys without
/// changing existing user config.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AuthStore {
    #[serde(flatten)]
    pub credentials: BTreeMap<String, StoredCredential>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StoredCredential {
    /// Backward-compatible `"sk-..."` value from pre-OAuth auth.json files.
    LegacyApiKey(String),
    ApiKey(ApiKeyCredential),
    OAuth(OAuthCredential),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyCredential {
    #[serde(rename = "type")]
    pub credential_type: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredential {
    #[serde(rename = "type")]
    pub credential_type: String,
    pub access: String,
    pub refresh: String,
    /// Unix timestamp in seconds when the access token expires.
    pub expires: u64,
    #[serde(
        rename = "accountId",
        alias = "account_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
}

/// Providers that have a single API-key dimension and a fixed env var.
/// Add to this when new cloud backends land.
pub const KNOWN_PROVIDERS: &[(&str, &str)] = &[
    ("openai", "OPENAI_API_KEY"),
    ("openrouter", "OPENROUTER_API_KEY"),
];

/// Returns the env var name that pairs with `provider`, if known.
pub fn env_var_for(provider: &str) -> Option<&'static str> {
    KNOWN_PROVIDERS
        .iter()
        .find(|(name, _)| *name == provider)
        .map(|(_, env)| *env)
}

/// Directory name under the config base. `LEGACY_CONFIG_DIR_NAME` is the
/// pre-Albatross name, kept only so an existing install can be migrated once.
const CONFIG_DIR_NAME: &str = "albatross";
const LEGACY_CONFIG_DIR_NAME: &str = "small-harness";

/// `$XDG_CONFIG_HOME`, falling back to `~/.config`. `None` when neither
/// XDG_CONFIG_HOME nor HOME is set ("no persistence layer available," not an
/// error).
fn config_base() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        Some(PathBuf::from(xdg))
    } else if let Ok(home) = std::env::var("HOME") {
        Some(PathBuf::from(home).join(".config"))
    } else {
        None
    }
}

/// `$XDG_CONFIG_HOME/albatross`, falling back to `~/.config/albatross`.
pub fn config_dir() -> Option<PathBuf> {
    config_base().map(|base| base.join(CONFIG_DIR_NAME))
}

/// `$XDG_CONFIG_HOME/albatross/auth.json`, falling back to
/// `~/.config/albatross/auth.json`. Returns `None` when neither
/// XDG_CONFIG_HOME nor HOME is set (`None` means "no persistence layer
/// available," not an error).
pub fn auth_file_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("auth.json"))
}

/// Move a pre-rename `small-harness` config directory to `albatross` once, so
/// an existing install keeps its OAuth tokens, API keys, and scorecard history
/// instead of silently appearing logged out after the rename.
///
/// Must run before anything reads the config directory.
pub fn migrate_legacy_config_dir() -> Option<Migration> {
    migrate_config_dir_in(&config_base()?)
}

/// Migration logic against an explicit base, so tests don't mutate process env.
fn migrate_config_dir_in(base: &Path) -> Option<Migration> {
    crate::dir_migration::migrate_dir(
        base.join(LEGACY_CONFIG_DIR_NAME),
        base.join(CONFIG_DIR_NAME),
    )
}

impl AuthStore {
    pub fn load_from(path: &PathBuf) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let store: Self =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(store)
    }

    /// Read the store at the default path. Missing file -> empty store.
    pub fn load() -> Self {
        match auth_file_path() {
            Some(path) => Self::load_from(&path).unwrap_or_default(),
            None => Self::default(),
        }
    }

    pub fn save_to(&self, path: &PathBuf) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let body = serde_json::to_string_pretty(self)? + "\n";
        fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
        set_secret_permissions(path)?;
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        let path =
            auth_file_path().context("no auth file path (neither XDG_CONFIG_HOME nor HOME set)")?;
        self.save_to(&path)
    }

    pub fn set(&mut self, provider: impl Into<String>, key: impl Into<String>) {
        self.credentials.insert(
            provider.into(),
            StoredCredential::ApiKey(ApiKeyCredential {
                credential_type: "api_key".into(),
                key: key.into(),
            }),
        );
    }

    pub fn set_oauth(&mut self, provider: impl Into<String>, credential: OAuthCredential) {
        self.credentials
            .insert(provider.into(), StoredCredential::OAuth(credential));
    }

    pub fn clear(&mut self, provider: &str) -> bool {
        self.credentials.remove(provider).is_some()
    }

    pub fn get(&self, provider: &str) -> Option<&str> {
        match self.credentials.get(provider)? {
            StoredCredential::LegacyApiKey(key) => Some(key.as_str()),
            StoredCredential::ApiKey(credential) => Some(credential.key.as_str()),
            StoredCredential::OAuth(_) => None,
        }
    }

    pub fn get_oauth(&self, provider: &str) -> Option<&OAuthCredential> {
        match self.credentials.get(provider)? {
            StoredCredential::OAuth(credential) => Some(credential),
            _ => None,
        }
    }
}

#[cfg(unix)]
fn set_secret_permissions(path: &PathBuf) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_secret_permissions(_path: &PathBuf) -> Result<()> {
    // On Windows, ACLs are the right mechanism — leave them at default user
    // for now. The auth file still ends up under the user's profile dir.
    Ok(())
}

/// At startup, load the file and set any env vars that aren't already set.
/// Env vars in the process environment always win, so CI / scripted users
/// who already export OPENAI_API_KEY see no change in behavior.
pub fn hydrate_env_from_file() {
    let store = AuthStore::load();
    for provider in store.credentials.keys() {
        if let Some(env_name) = env_var_for(provider) {
            let already_set = std::env::var(env_name)
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            if !already_set {
                if let Some(key) = store.get(provider) {
                    std::env::set_var(env_name, key);
                }
            }
        }
    }
}

/// Render an API key for display: keep the leading `sk-` (or first 3 chars)
/// and the last 4, mask the middle. Empty input renders as "(not set)".
pub fn mask_key(key: &str) -> String {
    if key.is_empty() {
        return "(not set)".into();
    }
    // Index by Unicode scalars so multi-byte keys cannot panic mid-character.
    let chars: Vec<char> = key.chars().collect();
    let head_len = if key.starts_with("sk-") {
        3
    } else {
        3.min(chars.len())
    };
    let tail_len = 4.min(chars.len().saturating_sub(head_len));
    if head_len + tail_len >= chars.len() {
        return "•".repeat(chars.len());
    }
    let head: String = chars[..head_len].iter().collect();
    let tail: String = chars[chars.len() - tail_len..].iter().collect();
    format!("{head}•••{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_providers_includes_openai_and_openrouter() {
        let names: Vec<&str> = KNOWN_PROVIDERS.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"openai"));
        assert!(names.contains(&"openrouter"));
    }

    #[test]
    fn env_var_lookup_works_for_known_providers() {
        assert_eq!(env_var_for("openai"), Some("OPENAI_API_KEY"));
        assert_eq!(env_var_for("openrouter"), Some("OPENROUTER_API_KEY"));
        assert_eq!(env_var_for("not-a-provider"), None);
    }

    #[test]
    fn mask_preserves_sk_prefix_and_last_four() {
        assert_eq!(mask_key("sk-abcdefgh1234"), "sk-•••1234");
        assert_eq!(mask_key(""), "(not set)");
    }

    #[test]
    fn mask_key_does_not_panic_on_multibyte_chars() {
        // CJK ideographs are 3 bytes; byte-index head/tail used to panic.
        let key = "日".repeat(12);
        assert_eq!(mask_key(&key), "日日日•••日日日日");
    }

    #[test]
    fn mask_short_key_is_fully_dotted() {
        // 6 chars: head=3 + tail=3 = covers the whole string; render as dots.
        assert_eq!(mask_key("abc123"), "••••••");
    }

    #[test]
    fn store_roundtrips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut store = AuthStore::default();
        store.set("openai", "sk-test-1");
        store.set("openrouter", "sk-or-2");
        store.save_to(&path).unwrap();

        let loaded = AuthStore::load_from(&path).unwrap();
        assert_eq!(loaded.get("openai"), Some("sk-test-1"));
        assert_eq!(loaded.get("openrouter"), Some("sk-or-2"));
    }

    #[test]
    fn legacy_string_api_keys_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(&path, r#"{"openai":"sk-legacy"}"#).unwrap();
        let loaded = AuthStore::load_from(&path).unwrap();
        assert_eq!(loaded.get("openai"), Some("sk-legacy"));
    }

    #[test]
    fn oauth_credentials_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut store = AuthStore::default();
        store.set_oauth(
            "openai-codex",
            OAuthCredential {
                credential_type: "oauth".into(),
                access: "access".into(),
                refresh: "refresh".into(),
                expires: 123,
                account_id: Some("acct".into()),
            },
        );
        store.save_to(&path).unwrap();
        let loaded = AuthStore::load_from(&path).unwrap();
        let oauth = loaded.get_oauth("openai-codex").unwrap();
        assert_eq!(oauth.access, "access");
        assert_eq!(oauth.account_id.as_deref(), Some("acct"));
        assert_eq!(loaded.get("openai-codex"), None);
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut store = AuthStore::default();
        store.set("openai", "sk-test");
        store.save_to(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected mode 0600, got {mode:o}");
    }

    #[test]
    fn missing_file_loads_to_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.json");
        let store = AuthStore::load_from(&path).unwrap();
        assert!(store.credentials.is_empty());
    }

    #[test]
    fn clear_returns_false_when_provider_absent() {
        let mut store = AuthStore::default();
        assert!(!store.clear("openai"));
        store.set("openai", "sk-x");
        assert!(store.clear("openai"));
    }

    #[test]
    fn migration_moves_legacy_config_dir_with_credentials() {
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join("small-harness");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("auth.json"), r#"{"openai":"sk-keep"}"#).unwrap();

        let current = base.path().join("albatross");
        migrate_config_dir_in(base.path()).expect("migration should run");
        assert!(!legacy.exists());

        // The credential survives the move — the whole point of migrating.
        let store = AuthStore::load_from(&current.join("auth.json")).unwrap();
        assert_eq!(store.get("openai"), Some("sk-keep"));
    }

    #[test]
    fn migration_keeps_a_live_credential_and_never_clobbers_it() {
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join("small-harness");
        let current = base.path().join("albatross");
        fs::create_dir_all(&legacy).unwrap();
        fs::create_dir_all(&current).unwrap();
        fs::write(legacy.join("auth.json"), r#"{"openai":"sk-stale"}"#).unwrap();
        fs::write(current.join("auth.json"), r#"{"openai":"sk-new"}"#).unwrap();

        // Both hold credentials: the renamed build's copy wins, untouched.
        assert!(migrate_config_dir_in(base.path()).is_none());
        let store = AuthStore::load_from(&current.join("auth.json")).unwrap();
        assert_eq!(store.get("openai"), Some("sk-new"));
        assert!(legacy.exists());
    }

    #[test]
    fn migration_rescues_a_credential_a_stray_cache_file_would_have_stranded() {
        // Regression: running the test suite (or any build that only meant to
        // read) used to create the new config directory holding nothing but a
        // cache file, which made the old skip-if-exists rule strand the login.
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join("small-harness");
        let current = base.path().join("albatross");
        fs::create_dir_all(&legacy).unwrap();
        fs::create_dir_all(&current).unwrap();
        fs::write(legacy.join("auth.json"), r#"{"openai":"sk-keep"}"#).unwrap();
        fs::write(current.join("grok-client-version"), "0.2.93\n").unwrap();

        migrate_config_dir_in(base.path()).expect("migration should run");
        let store = AuthStore::load_from(&current.join("auth.json")).unwrap();
        assert_eq!(store.get("openai"), Some("sk-keep"));
    }

    #[test]
    fn migration_is_a_no_op_for_a_fresh_install() {
        let base = tempfile::tempdir().unwrap();
        assert!(migrate_config_dir_in(base.path()).is_none());
        assert!(!base.path().join("albatross").exists());
    }
}
