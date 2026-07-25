use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::str::FromStr;

const RELEASES_URL: &str = "https://api.github.com/repos/GetSmallAI/Albatross/releases/latest";
const USER_AGENT: &str = concat!("albatross/", env!("CARGO_PKG_VERSION"));
const CACHE_TTL_HOURS: i64 = 24;
const HTTP_TIMEOUT_SECS: u64 = 4;
const OPT_OUT_ENV: &str = "ALBATROSS_NO_UPDATE_CHECK";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateCache {
    /// RFC3339 string so we don't have to enable chrono's serde feature.
    pub checked_at: String,
    pub latest_version: String,
}

impl UpdateCache {
    fn checked_at_dt(&self) -> Option<DateTime<Utc>> {
        DateTime::<Utc>::from_str(&self.checked_at).ok()
    }
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
}

/// `$XDG_CACHE_HOME/albatross/update-check.json`, falling back to
/// `~/.cache/albatross/update-check.json`. Returns None on a host with
/// no usable home — we just skip the update check entirely there.
pub fn cache_path() -> Option<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".cache")
    } else {
        return None;
    };
    Some(base.join("albatross").join("update-check.json"))
}

pub fn opted_out() -> bool {
    std::env::var(OPT_OUT_ENV)
        .map(|v| {
            matches!(
                v.trim().to_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn read_cache(path: &PathBuf) -> Option<UpdateCache> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_cache(path: &PathBuf, cache: &UpdateCache) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(cache)? + "\n";
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Notice to display in the banner when the cached latest is newer than the
/// current build. None when up to date, opted out, or cache absent.
pub fn pending_notice(current: &str) -> Option<String> {
    if opted_out() {
        return None;
    }
    let path = cache_path()?;
    let cache = read_cache(&path)?;
    if version_is_newer(&cache.latest_version, current) {
        Some(format!(
            "Update available: {} → {} (https://github.com/GetSmallAI/Albatross/releases/latest)",
            current, cache.latest_version
        ))
    } else {
        None
    }
}

/// Refresh the cache if it's missing or older than CACHE_TTL_HOURS.
/// Failure is silent — this is a best-effort polish feature, not a critical
/// path. Designed to be spawned as a background task at startup.
pub async fn refresh_cache_if_stale(http: &reqwest::Client) {
    if opted_out() {
        return;
    }
    let Some(path) = cache_path() else { return };
    if let Some(existing) = read_cache(&path) {
        if let Some(checked_at) = existing.checked_at_dt() {
            let age = Utc::now().signed_duration_since(checked_at);
            if age < Duration::hours(CACHE_TTL_HOURS) {
                return;
            }
        }
    }
    let req = http
        .get(RELEASES_URL)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS));
    let Ok(resp) = req.send().await else { return };
    if !resp.status().is_success() {
        return;
    }
    let Ok(release) = resp.json::<GhRelease>().await else {
        return;
    };
    let latest_version = release.tag_name.trim_start_matches('v').to_string();
    let cache = UpdateCache {
        checked_at: Utc::now().to_rfc3339(),
        latest_version,
    };
    let _ = write_cache(&path, &cache);
}

/// Naive semver compare for `major.minor.patch` (optionally with extra
/// dot-separated parts). Non-numeric segments compare lexicographically as
/// a tiebreaker — good enough for "is this a newer release tag?" without
/// pulling in a semver dep.
fn version_is_newer(candidate: &str, current: &str) -> bool {
    let cand: Vec<&str> = candidate.split('.').collect();
    let curr: Vec<&str> = current.split('.').collect();
    let len = cand.len().max(curr.len());
    for i in 0..len {
        let a = cand.get(i).copied().unwrap_or("0");
        let b = curr.get(i).copied().unwrap_or("0");
        let parsed = (a.parse::<u64>().ok(), b.parse::<u64>().ok());
        match parsed {
            (Some(av), Some(bv)) => match av.cmp(&bv) {
                std::cmp::Ordering::Greater => return true,
                std::cmp::Ordering::Less => return false,
                std::cmp::Ordering::Equal => continue,
            },
            _ => match a.cmp(b) {
                std::cmp::Ordering::Greater => return true,
                std::cmp::Ordering::Less => return false,
                std::cmp::Ordering::Equal => continue,
            },
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_patch_triggers_notice() {
        assert!(version_is_newer("0.2.3", "0.2.2"));
        assert!(version_is_newer("0.3.0", "0.2.99"));
        assert!(version_is_newer("1.0.0", "0.99.99"));
    }

    #[test]
    fn same_or_older_does_not_trigger() {
        assert!(!version_is_newer("0.2.2", "0.2.2"));
        assert!(!version_is_newer("0.2.1", "0.2.2"));
        assert!(!version_is_newer("0.1.99", "0.2.0"));
    }

    #[test]
    fn extra_dotted_parts_compared_in_order() {
        // 0.2.2 == 0.2.2.0 (missing parts default to "0")
        assert!(!version_is_newer("0.2.2", "0.2.2.0"));
        assert!(version_is_newer("0.2.2.1", "0.2.2"));
    }

    #[test]
    fn cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.json");
        let cache = UpdateCache {
            checked_at: Utc::now().to_rfc3339(),
            latest_version: "0.9.0".into(),
        };
        write_cache(&path, &cache).unwrap();
        let loaded = read_cache(&path).unwrap();
        assert_eq!(loaded.latest_version, "0.9.0");
        assert!(loaded.checked_at_dt().is_some());
    }

    #[test]
    fn missing_cache_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.json");
        assert!(read_cache(&path).is_none());
    }
}
