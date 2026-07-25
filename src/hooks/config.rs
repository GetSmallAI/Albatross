use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

use super::HookEventName;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct HookConfig {
    #[serde(default, rename = "SessionStart")]
    pub session_start: Vec<HookGroupConfig>,
    #[serde(default, rename = "UserPromptSubmit")]
    pub user_prompt_submit: Vec<HookGroupConfig>,
    #[serde(default, rename = "PreToolUse")]
    pub pre_tool_use: Vec<HookGroupConfig>,
    #[serde(default, rename = "PermissionRequest")]
    pub permission_request: Vec<HookGroupConfig>,
    #[serde(default, rename = "PostToolUse")]
    pub post_tool_use: Vec<HookGroupConfig>,
    #[serde(default, rename = "PreCompact")]
    pub pre_compact: Vec<HookGroupConfig>,
    #[serde(default, rename = "PostCompact")]
    pub post_compact: Vec<HookGroupConfig>,
    #[serde(default, rename = "PlanUpdated")]
    pub plan_updated: Vec<HookGroupConfig>,
    #[serde(default, rename = "SubagentStart")]
    pub subagent_start: Vec<HookGroupConfig>,
    #[serde(default, rename = "SubagentStop")]
    pub subagent_stop: Vec<HookGroupConfig>,
    #[serde(default, rename = "Stop")]
    pub stop: Vec<HookGroupConfig>,
    #[serde(default, rename = "SessionEnd")]
    pub session_end: Vec<HookGroupConfig>,
}

impl HookConfig {
    pub fn groups_for(&self, event: HookEventName) -> &[HookGroupConfig] {
        match event {
            HookEventName::SessionStart => &self.session_start,
            HookEventName::UserPromptSubmit => &self.user_prompt_submit,
            HookEventName::PreToolUse => &self.pre_tool_use,
            HookEventName::PermissionRequest => &self.permission_request,
            HookEventName::PostToolUse => &self.post_tool_use,
            HookEventName::PreCompact => &self.pre_compact,
            HookEventName::PostCompact => &self.post_compact,
            HookEventName::PlanUpdated => &self.plan_updated,
            HookEventName::SubagentStart => &self.subagent_start,
            HookEventName::SubagentStop => &self.subagent_stop,
            HookEventName::Stop => &self.stop,
            HookEventName::SessionEnd => &self.session_end,
        }
    }

    pub fn merge_from(&mut self, mut other: HookConfig) {
        self.session_start.append(&mut other.session_start);
        self.user_prompt_submit
            .append(&mut other.user_prompt_submit);
        self.pre_tool_use.append(&mut other.pre_tool_use);
        self.permission_request
            .append(&mut other.permission_request);
        self.post_tool_use.append(&mut other.post_tool_use);
        self.pre_compact.append(&mut other.pre_compact);
        self.post_compact.append(&mut other.post_compact);
        self.plan_updated.append(&mut other.plan_updated);
        self.subagent_start.append(&mut other.subagent_start);
        self.subagent_stop.append(&mut other.subagent_stop);
        self.stop.append(&mut other.stop);
        self.session_end.append(&mut other.session_end);
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct HookGroupConfig {
    #[serde(default)]
    pub matcher: Option<String>,
    #[serde(default)]
    pub hooks: Vec<HookCommandConfig>,
}

#[derive(Debug, Clone)]
pub struct HookCommandConfig {
    pub command: String,
    pub timeout_sec: u64,
    pub command_windows: Option<String>,
    pub status_message: Option<String>,
    pub async_handler: bool,
    pub env: BTreeMap<String, String>,
    pub env_vars: Vec<String>,
}

pub fn default_hook_timeout_sec() -> u64 {
    600
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct HookCommandConfigWire {
    #[serde(rename = "type")]
    handler_type: String,
    #[serde(default)]
    command: String,
    #[serde(default = "default_hook_timeout_sec")]
    timeout_sec: u64,
    #[serde(default)]
    command_windows: Option<String>,
    #[serde(default)]
    status_message: Option<String>,
    #[serde(default, rename = "async")]
    async_handler: bool,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    env_vars: Vec<String>,
}

impl<'de> Deserialize<'de> for HookCommandConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = HookCommandConfigWire::deserialize(deserializer)?;
        if wire.handler_type == "prompt" || wire.handler_type == "agent" {
            return Ok(Self {
                command: String::new(),
                timeout_sec: wire.timeout_sec,
                command_windows: wire.command_windows,
                status_message: wire.status_message,
                async_handler: wire.async_handler,
                env: BTreeMap::new(),
                env_vars: Vec::new(),
            });
        }
        if wire.handler_type != "command" {
            return Err(serde::de::Error::custom(format!(
                "unsupported hook type `{}`",
                wire.handler_type
            )));
        }
        validate_hook_env(&wire.env, &wire.env_vars).map_err(serde::de::Error::custom)?;
        Ok(Self {
            command: wire.command,
            timeout_sec: wire.timeout_sec,
            command_windows: wire.command_windows,
            status_message: wire.status_message,
            async_handler: wire.async_handler,
            env: wire.env,
            env_vars: wire.env_vars,
        })
    }
}

fn validate_hook_env(env: &BTreeMap<String, String>, env_vars: &[String]) -> Result<(), String> {
    for (name, value) in env {
        validate_hook_env_name("env", name)?;
        if value.contains('\0') {
            return Err(format!("hook env `{name}` value must not contain NUL"));
        }
    }
    for name in env_vars {
        validate_hook_env_name("envVars", name)?;
    }
    Ok(())
}

fn validate_hook_env_name(field: &str, name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(format!("hook {field} entries must not be empty"));
    }
    if name.contains('=') || name.contains('\0') {
        return Err(format!(
            "hook {field} entry `{name}` must not contain `=` or NUL"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HookState {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub trusted_hash: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ManagedHookConfig {
    pub source_label: String,
    pub hooks: HookConfig,
}

#[derive(Deserialize)]
struct WrappedManagedHookConfig {
    #[serde(default)]
    source: Option<String>,
    hooks: HookConfig,
}

pub fn load_managed_hooks_from_env(
    env_json: Option<&str>,
    env_file: Option<&str>,
) -> Result<Option<ManagedHookConfig>> {
    let mut merged = HookConfig::default();
    let mut source_label = None;
    let mut loaded_any = false;

    if let Some(env_json) = env_json.map(str::trim).filter(|s| !s.is_empty()) {
        let (source, hooks) = parse_managed_hook_document(env_json)
            .context("parsing ALBATROSS_MANAGED_HOOKS_JSON")?;
        if source_label.is_none() {
            source_label = source;
        }
        merged.merge_from(hooks);
        loaded_any = true;
    }

    if let Some(path) = env_file.map(str::trim).filter(|s| !s.is_empty()) {
        let (source, hooks) = read_managed_hook_file(PathBuf::from(path))?;
        if source_label.is_none() {
            source_label = source;
        }
        merged.merge_from(hooks);
        loaded_any = true;
    }

    if loaded_any {
        Ok(Some(ManagedHookConfig {
            source_label: source_label.unwrap_or_else(|| "launch".into()),
            hooks: merged,
        }))
    } else {
        Ok(None)
    }
}

fn read_managed_hook_file(path: PathBuf) -> Result<(Option<String>, HookConfig)> {
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    parse_managed_hook_document(&text).with_context(|| format!("parsing {}", path.display()))
}

fn parse_managed_hook_document(text: &str) -> Result<(Option<String>, HookConfig)> {
    let value: serde_json::Value = serde_json::from_str(text)?;
    let is_wrapped = value.get("source").is_some() || value.get("hooks").is_some();
    if is_wrapped {
        let wrapped: WrappedManagedHookConfig = serde_json::from_value(value)?;
        return Ok((wrapped.source, wrapped.hooks));
    }
    let hooks = serde_json::from_value(value)?;
    Ok((None, hooks))
}
