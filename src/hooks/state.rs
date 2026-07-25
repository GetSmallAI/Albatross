use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::HookState;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HookStateFile {
    #[serde(default)]
    pub hooks: BTreeMap<String, HookState>,
}

pub fn hook_state_file_path() -> Option<PathBuf> {
    hook_state_file_path_from_env(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

pub fn hook_state_file_path_from_env(xdg: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let base = if let Some(xdg) = xdg {
        PathBuf::from(xdg)
    } else {
        PathBuf::from(home?).join(".config")
    };
    Some(base.join("albatross").join("hooks-state.json"))
}

pub fn load_hook_state_file_from(path: &Path) -> Result<HookStateFile> {
    if !path.exists() {
        return Ok(HookStateFile::default());
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(HookStateFile::default());
    }
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn save_hook_state_file_to(path: &Path, state: &HookStateFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(state)? + "\n";
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))
}
