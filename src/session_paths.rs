//! Session paths: branchable forks of a working session.
//!
//! Design note — why this is bespoke and not `git worktree`:
//! A path snapshots the *conversation and the workspace together*
//! (`PathSessionState` = messages + checkpoint stack + token/cost totals +
//! conversation summary + a `WorkspaceSnapshot`). `git worktree` only branches
//! files — it has no notion of chat history, costs, or the running summary — so
//! it cannot model `/path fork`/`switch`/`pick`. This store also reuses the
//! `turn_checkpoint` machinery and works in any directory, git repo or not.
//! That coupling is the feature; keep it rather than "simplifying" to worktrees.

use anyhow::{anyhow, Result};
use chrono::Utc;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::config::PathsConfig;
use crate::openai::ChatMessage;
use crate::tools::unified_diff;
use crate::turn_checkpoint::{
    restore_file_baselines, snapshot_file_into, CheckpointLimits, CheckpointStack, FileBaseline,
    RestoreReport, TurnCheckpoint,
};

pub const DEFAULT_PATH_ID: &str = "main";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotLimits {
    pub max_bytes: u64,
    pub max_file_bytes: u64,
}

impl From<&PathsConfig> for SnapshotLimits {
    fn from(config: &PathsConfig) -> Self {
        Self {
            max_bytes: config.max_snapshot_bytes,
            max_file_bytes: config.max_file_bytes,
        }
    }
}

impl SnapshotLimits {
    pub fn checkpoint_limits(&self) -> CheckpointLimits {
        CheckpointLimits {
            max_turns: 10,
            max_bytes: self.max_bytes,
            max_file_bytes: self.max_file_bytes,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSnapshot {
    pub files: BTreeMap<String, FileBaseline>,
    pub skipped: Vec<String>,
    pub snapshot_bytes: u64,
}

impl WorkspaceSnapshot {
    pub fn capture(workspace_root: &Path, limits: SnapshotLimits) -> Result<Self> {
        let mut checkpoint = TurnCheckpoint::new();
        let checkpoint_limits = limits.checkpoint_limits();
        for entry in workspace_files(workspace_root) {
            let rel = relative_slash_path(workspace_root, entry.path());
            if rel.is_empty() {
                continue;
            }
            snapshot_file_into(&mut checkpoint, workspace_root, &rel, checkpoint_limits)?;
        }
        Ok(Self {
            files: checkpoint.files,
            skipped: checkpoint.skipped,
            snapshot_bytes: checkpoint.snapshot_bytes,
        })
    }

    pub fn restore(&self, workspace_root: &Path) -> RestoreReport {
        restore_file_baselines(&self.files, &self.skipped, workspace_root)
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathSessionState {
    pub messages: Vec<ChatMessage>,
    pub checkpoint_stack: CheckpointStack,
    pub total_in: u32,
    pub total_out: u32,
    pub session_usd: f64,
    pub session_cost_has_unknown: bool,
    pub conversation_summary: Option<String>,
    pub workspace: WorkspaceSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathRecord {
    pub id: String,
    pub parent_id: Option<String>,
    pub fork_message_index: usize,
    pub created_at: String,
    pub updated_at: String,
    pub snapshot_bytes: u64,
    pub file_count: usize,
    pub message_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathRegistry {
    pub active_id: String,
    pub paths: Vec<PathRecord>,
}

impl Default for PathRegistry {
    fn default() -> Self {
        Self {
            active_id: DEFAULT_PATH_ID.to_string(),
            paths: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PickResult {
    pub applied: bool,
    pub dry_run: bool,
    pub files: Vec<String>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PathStore {
    pub root: PathBuf,
    pub base_session_path: PathBuf,
    pub registry: PathRegistry,
    pub config: PathsConfig,
    pub dirty: bool,
}

impl PathStore {
    pub fn store_root(session_dir: &str, session_path: &Path) -> PathBuf {
        let session_id = session_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session");
        Path::new(session_dir).join("paths").join(session_id)
    }

    pub fn new(session_dir: &str, session_path: &Path, config: &PathsConfig) -> Self {
        Self {
            root: Self::store_root(session_dir, session_path),
            base_session_path: session_path.to_path_buf(),
            registry: PathRegistry::default(),
            config: config.clone(),
            dirty: false,
        }
    }

    pub fn load(session_dir: &str, session_path: &Path, config: &PathsConfig) -> Self {
        let root = Self::store_root(session_dir, session_path);
        let registry_path = root.join("registry.json");
        let registry = if registry_path.exists() {
            fs::read_to_string(&registry_path)
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or_default()
        } else {
            PathRegistry::default()
        };
        Self {
            root,
            base_session_path: session_path.to_path_buf(),
            registry,
            config: config.clone(),
            dirty: false,
        }
    }

    pub fn path_count(&self) -> usize {
        if self.registry.paths.is_empty() {
            1
        } else {
            self.registry.paths.len()
        }
    }

    pub fn active_id(&self) -> &str {
        &self.registry.active_id
    }

    pub fn total_storage_bytes(&self) -> u64 {
        self.registry.paths.iter().map(|p| p.snapshot_bytes).sum()
    }

    pub fn transcript_path(&self, path_id: &str) -> PathBuf {
        if path_id == DEFAULT_PATH_ID {
            self.base_session_path.clone()
        } else {
            self.root.join(path_id).join("session.jsonl")
        }
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn path_dir(&self, path_id: &str) -> PathBuf {
        self.root.join(path_id)
    }

    fn save_registry(&self) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        fs::write(
            self.root.join("registry.json"),
            serde_json::to_string_pretty(&self.registry)? + "\n",
        )?;
        Ok(())
    }

    fn upsert_record(&mut self, meta: PathRecord) {
        if let Some(existing) = self.registry.paths.iter_mut().find(|p| p.id == meta.id) {
            *existing = meta;
        } else {
            self.registry.paths.push(meta);
        }
    }

    fn write_path_state(&self, meta: &PathRecord, state: &PathSessionState) -> Result<()> {
        let dir = self.path_dir(&meta.id);
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("state.json"),
            serde_json::to_string_pretty(state)? + "\n",
        )?;
        fs::write(
            dir.join("workspace.json"),
            serde_json::to_string_pretty(&state.workspace)? + "\n",
        )?;
        fs::write(
            dir.join("meta.json"),
            serde_json::to_string_pretty(meta)? + "\n",
        )?;
        Ok(())
    }

    fn build_record(
        &self,
        path_id: &str,
        state: &PathSessionState,
        parent_id: Option<String>,
        fork_message_index: usize,
        created_at: Option<String>,
    ) -> PathRecord {
        PathRecord {
            id: path_id.to_string(),
            parent_id,
            fork_message_index,
            created_at: created_at.unwrap_or_else(|| Utc::now().to_rfc3339()),
            updated_at: Utc::now().to_rfc3339(),
            snapshot_bytes: state.workspace.snapshot_bytes,
            file_count: state.workspace.file_count(),
            message_count: state.messages.len(),
        }
    }

    pub fn capture_state(
        state: &crate::app_state::AppState,
        workspace_root: &Path,
    ) -> Result<PathSessionState> {
        let limits = SnapshotLimits::from(&state.config.paths);
        Ok(PathSessionState {
            messages: state.messages.clone(),
            checkpoint_stack: state.checkpoint_stack.clone(),
            total_in: state.total_in,
            total_out: state.total_out,
            session_usd: state.session_usd,
            session_cost_has_unknown: state.session_cost_has_unknown,
            conversation_summary: state.conversation_summary.clone(),
            workspace: WorkspaceSnapshot::capture(workspace_root, limits)?,
        })
    }

    pub fn fork(
        &mut self,
        current: PathSessionState,
        session_path: &Path,
        name: Option<&str>,
        _workspace_root: &Path,
    ) -> Result<(String, PathSessionState)> {
        if !self.config.enabled {
            anyhow::bail!("session paths are disabled — set paths.enabled in agent.config.json");
        }
        let next_count = if self.registry.paths.is_empty() {
            2
        } else {
            self.registry.paths.len() + 1
        };
        if next_count > self.config.max_paths {
            anyhow::bail!(
                "path limit reached ({}) — /path drop an unused path first",
                self.config.max_paths
            );
        }

        self.save_path_state(&self.registry.active_id.clone(), &current)?;

        if self.registry.paths.is_empty() {
            let main_meta = self.build_record(
                DEFAULT_PATH_ID,
                &current,
                None,
                0,
                Some(Utc::now().to_rfc3339()),
            );
            self.write_path_state(&main_meta, &current)?;
            self.upsert_record(main_meta);
            self.save_registry()?;
        }

        let base_name = name
            .map(slugify_name)
            .unwrap_or_else(|| format!("path-{}", self.registry.paths.len()));
        let new_id = unique_path_id(&self.registry.paths, &base_name);
        let parent_id = self.registry.active_id.clone();
        let fork_message_index = current.messages.len();
        let new_state = current;

        let dir = self.path_dir(&new_id);
        fs::create_dir_all(&dir)?;
        if session_path.exists() {
            fs::copy(session_path, dir.join("session.jsonl"))?;
        }

        let meta = self.build_record(
            &new_id,
            &new_state,
            Some(parent_id),
            fork_message_index,
            None,
        );
        self.write_path_state(&meta, &new_state)?;
        self.upsert_record(meta);
        self.registry.active_id = new_id.clone();
        self.save_registry()?;
        self.dirty = false;
        Ok((new_id, new_state))
    }

    pub fn persist_active(&mut self, current: PathSessionState) -> Result<()> {
        if !self.config.enabled {
            return Ok(());
        }
        self.save_path_state(&self.registry.active_id.clone(), &current)?;
        self.dirty = false;
        Ok(())
    }

    pub fn flush_if_dirty(&mut self, current: PathSessionState) -> Result<()> {
        if self.dirty {
            self.persist_active(current)?;
        }
        Ok(())
    }

    pub fn switch_to(
        &mut self,
        target: &str,
        current: PathSessionState,
        workspace_root: &Path,
    ) -> Result<(PathSessionState, RestoreReport)> {
        if !self.config.enabled {
            anyhow::bail!("session paths are disabled");
        }
        let target = target.trim();
        if target == self.registry.active_id {
            anyhow::bail!("already on path `{target}`");
        }
        if target != DEFAULT_PATH_ID && !self.path_dir(target).join("state.json").exists() {
            anyhow::bail!("unknown path `{target}` — use /paths to list");
        }
        if target == DEFAULT_PATH_ID && !self.path_dir(DEFAULT_PATH_ID).join("state.json").exists()
        {
            anyhow::bail!("main path has no saved state — /path fork first");
        }

        self.save_path_state(&self.registry.active_id.clone(), &current)?;

        let path_state = self.load_path_state(target)?;
        let _ = Self::remove_files_not_in_snapshot(
            &current.workspace,
            &path_state.workspace,
            workspace_root,
        )?;
        let report = path_state.workspace.restore(workspace_root);
        self.registry.active_id = target.to_string();
        self.save_registry()?;
        self.dirty = false;
        Ok((path_state, report))
    }

    fn save_path_state(&mut self, path_id: &str, state: &PathSessionState) -> Result<()> {
        if !self.config.enabled {
            return Ok(());
        }
        let existing = self
            .registry
            .paths
            .iter()
            .find(|p| p.id == path_id)
            .cloned();
        let meta = self.build_record(
            path_id,
            state,
            existing.as_ref().and_then(|p| p.parent_id.clone()),
            existing.as_ref().map(|p| p.fork_message_index).unwrap_or(0),
            existing.map(|p| p.created_at),
        );
        self.write_path_state(&meta, state)?;
        self.upsert_record(meta);
        self.save_registry()?;
        Ok(())
    }

    pub fn diff_with(&self, other_id: &str, workspace_root: &Path) -> Result<String> {
        if other_id == self.registry.active_id {
            anyhow::bail!("pick a different path than the active one");
        }
        let other = self.load_path_state(other_id)?;
        let active = self.load_path_state(&self.registry.active_id)?;

        let mut paths: BTreeMap<String, ()> = BTreeMap::new();
        for key in active.workspace.files.keys() {
            paths.insert(key.clone(), ());
        }
        for key in other.workspace.files.keys() {
            paths.insert(key.clone(), ());
        }

        let mut out = Vec::new();
        for rel in paths.keys() {
            let left = read_baseline_or_disk(workspace_root, active.workspace.files.get(rel));
            let right = read_baseline_or_disk(workspace_root, other.workspace.files.get(rel));
            if left == right {
                continue;
            }
            out.push(unified_diff(&left, &right, rel));
        }
        Ok(out.join("\n\n"))
    }

    pub fn pick_from(
        &self,
        other_id: &str,
        workspace_root: &Path,
        dry_run: bool,
    ) -> Result<PickResult> {
        let other = self.load_path_state(other_id)?;
        let active = self.load_path_state(&self.registry.active_id)?;

        let mut result = PickResult {
            applied: false,
            dry_run,
            files: Vec::new(),
            skipped: Vec::new(),
            errors: Vec::new(),
        };

        let mut rel_paths: BTreeMap<String, ()> = BTreeMap::new();
        for key in active.workspace.files.keys() {
            rel_paths.insert(key.clone(), ());
        }
        for key in other.workspace.files.keys() {
            rel_paths.insert(key.clone(), ());
        }

        for rel in rel_paths.keys() {
            let current = read_baseline_or_disk(workspace_root, active.workspace.files.get(rel));
            let picked = read_baseline_or_disk(workspace_root, other.workspace.files.get(rel));
            if current == picked {
                continue;
            }
            if dry_run {
                result.files.push(rel.clone());
                continue;
            }
            match apply_picked_content(workspace_root, rel, &picked, other.workspace.files.get(rel))
            {
                Ok(()) => result.files.push(rel.clone()),
                Err(e) => result.errors.push(format!("{rel}: {e}")),
            }
        }

        result.applied = !dry_run && result.errors.is_empty() && !result.files.is_empty();
        Ok(result)
    }

    pub fn load_resume_state(
        &mut self,
        workspace_root: &Path,
        active_path_id: Option<&str>,
    ) -> Result<Option<(PathSessionState, RestoreReport)>> {
        if !self.config.enabled || !self.root.join("registry.json").exists() {
            return Ok(None);
        }
        if let Some(id) = active_path_id {
            self.registry.active_id = id.to_string();
        }
        let active = self.registry.active_id.clone();
        if !self.path_dir(&active).join("state.json").exists() {
            return Ok(None);
        }
        let path_state = self.load_path_state(&active)?;
        let report = path_state.workspace.restore(workspace_root);
        Ok(Some((path_state, report)))
    }

    fn load_path_state(&self, path_id: &str) -> Result<PathSessionState> {
        let state_path = self.path_dir(path_id).join("state.json");
        if !state_path.exists() {
            anyhow::bail!("path `{path_id}` has no saved state");
        }
        let text = fs::read_to_string(state_path)?;
        Ok(serde_json::from_str(&text)?)
    }

    fn remove_files_not_in_snapshot(
        current: &WorkspaceSnapshot,
        target: &WorkspaceSnapshot,
        workspace_root: &Path,
    ) -> Result<Vec<String>> {
        let current_keys: BTreeSet<_> = current.files.keys().collect();
        let target_keys: BTreeSet<_> = target.files.keys().collect();
        let mut removed = Vec::new();
        for rel in current_keys.difference(&target_keys) {
            if let Some(full) = resolve_under_workspace(workspace_root, rel) {
                if full.is_file() {
                    fs::remove_file(&full)?;
                    removed.push((*rel).clone());
                }
            }
        }
        Ok(removed)
    }

    pub fn drop_path(&mut self, name: &str) -> Result<()> {
        let target = name.trim();
        if target == DEFAULT_PATH_ID {
            anyhow::bail!("cannot drop the main path");
        }
        if target == self.registry.active_id {
            anyhow::bail!("cannot drop the active path — /path switch away first");
        }
        let dir = self.path_dir(target);
        if !dir.exists() {
            anyhow::bail!("unknown path `{target}`");
        }
        fs::remove_dir_all(&dir)?;
        self.registry.paths.retain(|p| p.id != target);
        self.save_registry()?;
        Ok(())
    }
}

pub fn apply_path_session_state(
    state: &mut crate::app_state::AppState,
    path_state: &PathSessionState,
    transcript_path: &Path,
) {
    state.messages = path_state.messages.clone();
    state.checkpoint_stack = path_state.checkpoint_stack.clone();
    state.total_in = path_state.total_in;
    state.total_out = path_state.total_out;
    state.session_usd = path_state.session_usd;
    state.session_cost_has_unknown = path_state.session_cost_has_unknown;
    state.conversation_summary = path_state.conversation_summary.clone();
    state.session_path = transcript_path.to_path_buf();
}

fn apply_picked_content(
    workspace_root: &Path,
    rel_path: &str,
    content: &str,
    baseline: Option<&FileBaseline>,
) -> Result<()> {
    let full = resolve_under_workspace(workspace_root, rel_path)
        .ok_or_else(|| anyhow!("{rel_path}: outside workspace"))?;
    if content.is_empty() {
        if baseline.map(|b| !b.existed).unwrap_or(true) && full.exists() {
            fs::remove_file(&full)?;
        }
        return Ok(());
    }
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&full, content)?;
    Ok(())
}

fn read_baseline_or_disk(workspace_root: &Path, baseline: Option<&FileBaseline>) -> String {
    match baseline {
        Some(b) if !b.existed => String::new(),
        Some(b) if b.content.is_some() => {
            String::from_utf8_lossy(b.content.as_ref().unwrap()).into()
        }
        Some(b) => resolve_under_workspace(workspace_root, &b.rel_path)
            .and_then(|p| fs::read_to_string(p).ok())
            .unwrap_or_default(),
        None => String::new(),
    }
}

fn slugify_name(raw: &str) -> String {
    let s: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = s.trim_matches('-');
    if trimmed.is_empty() {
        "path".to_string()
    } else {
        trimmed.to_string()
    }
}

fn unique_path_id(existing: &[PathRecord], base: &str) -> String {
    if !existing.iter().any(|p| p.id == base) {
        return base.to_string();
    }
    for n in 2..1000 {
        let candidate = format!("{base}-{n}");
        if !existing.iter().any(|p| p.id == candidate) {
            return candidate;
        }
    }
    format!("{base}-{}", Utc::now().timestamp_millis())
}

fn workspace_files(root: &Path) -> Vec<ignore::DirEntry> {
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(true)
        .hidden(false)
        .require_git(false);
    builder.filter_entry(|entry| !has_skipped_component(entry.path()));
    builder
        .build()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map(|ft| ft.is_file()).unwrap_or(false))
        .collect()
}

fn has_skipped_component(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some(".git" | ".sessions" | "target" | "node_modules")
        )
    })
}

fn relative_slash_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    };
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

pub fn resolve_under_workspace(workspace_root: &Path, path: &str) -> Option<PathBuf> {
    let workspace_root = crate::path_security::canonical_root(workspace_root);
    let joined = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        workspace_root.join(path)
    };
    let normalized = crate::path_security::resolve_existing_prefix(&joined);
    if normalized.starts_with(&workspace_root) {
        Some(normalized)
    } else {
        None
    }
}

pub fn workspace_root_path(config: &crate::config::AgentConfig) -> PathBuf {
    normalize_path(Path::new(&config.workspace_root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::ApprovalCache;
    use crate::backends::backend;
    use crate::config::AgentConfig;
    use crate::renderer::TuiRenderer;
    use std::process::Command;

    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn test_state(dir: &Path) -> crate::app_state::AppState {
        let session_dir = dir.join(".sessions").display().to_string();
        fs::create_dir_all(&session_dir).unwrap();
        let session_path = Path::new(&session_dir).join("test.jsonl");
        let trace_path = session_path.clone();
        fs::write(&session_path, "").unwrap();
        let mut config = AgentConfig {
            workspace_root: dir.display().to_string(),
            session_dir: session_dir.clone(),
            ..Default::default()
        };
        config.paths.enabled = true;
        let path_store = PathStore::new(&session_dir, &session_path, &config.paths);
        crate::app_state::AppState {
            http: reqwest::Client::new(),
            backend: backend(config.backend),
            model: "test-model".into(),
            active_effort: None,
            messages: Vec::new(),
            session_dir,
            session_path,
            total_in: 0,
            total_out: 0,
            session_usd: 0.0,
            session_cost_has_unknown: false,
            context_guard_notice: None,
            conversation_summary: None,
            checkpoint_stack: CheckpointStack::new(CheckpointLimits::default()),
            checkpoints_enabled: true,
            play_session: None,
            last_play_scorecard: None,
            approval_cache: ApprovalCache::new(),
            renderer: TuiRenderer::new(config.display.clone()),
            hooks: crate::hooks::HookRegistry::default(),
            session_hook_contexts: Vec::new(),
            pending_hook_contexts: Vec::new(),
            warmed_fingerprint: None,
            tests_ran_this_session: false,
            pending_image_attachments: Vec::new(),
            mcp_tools: Vec::new(),
            config,
            path_store,
            trace: crate::turn_trace::test_trace_for(&trace_path),
            trace_enabled: false,
        }
    }

    #[test]
    fn capture_and_restore_round_trip() {
        let dir = workspace();
        fs::write(dir.path().join("note.txt"), "before\n").unwrap();
        let limits = SnapshotLimits {
            max_bytes: 1024 * 1024,
            max_file_bytes: 1024,
        };
        let snapshot = WorkspaceSnapshot::capture(dir.path(), limits).unwrap();
        fs::write(dir.path().join("note.txt"), "after\n").unwrap();
        let report = snapshot.restore(dir.path());
        assert_eq!(
            fs::read_to_string(dir.path().join("note.txt")).unwrap(),
            "before\n"
        );
        assert_eq!(report.restored, vec!["note.txt"]);
    }

    #[test]
    fn relative_workspace_root_capture_and_restore() {
        let dir = workspace();
        fs::write(dir.path().join("note.txt"), "before\n").unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let limits = SnapshotLimits {
            max_bytes: 1024 * 1024,
            max_file_bytes: 1024,
        };
        let snapshot = WorkspaceSnapshot::capture(Path::new("."), limits).unwrap();
        fs::write(dir.path().join("note.txt"), "after\n").unwrap();
        let report = snapshot.restore(Path::new("."));
        std::env::set_current_dir(prev).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("note.txt")).unwrap(),
            "before\n"
        );
        assert_eq!(report.restored, vec!["note.txt"]);
    }

    #[test]
    fn untracked_file_captured() {
        let dir = workspace();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .ok();
        fs::write(dir.path().join("src/new.rs"), "draft\n").unwrap();
        let limits = SnapshotLimits {
            max_bytes: 1024 * 1024,
            max_file_bytes: 1024,
        };
        let snapshot = WorkspaceSnapshot::capture(dir.path(), limits).unwrap();
        assert!(snapshot.files.contains_key("src/new.rs"));
        fs::write(dir.path().join("src/new.rs"), "broken\n").unwrap();
        snapshot.restore(dir.path());
        assert_eq!(
            fs::read_to_string(dir.path().join("src/new.rs")).unwrap(),
            "draft\n"
        );
    }

    #[test]
    fn oversized_file_skipped() {
        let dir = workspace();
        fs::write(dir.path().join("big.txt"), "1234567890").unwrap();
        let limits = SnapshotLimits {
            max_bytes: 1024 * 1024,
            max_file_bytes: 4,
        };
        let snapshot = WorkspaceSnapshot::capture(dir.path(), limits).unwrap();
        assert!(snapshot.skipped.contains(&"big.txt".to_string()));
    }

    fn fork_path(state: &mut crate::app_state::AppState, name: &str) -> String {
        let root = workspace_root_path(&state.config);
        let session_path = state.session_path.clone();
        let current = PathStore::capture_state(state, &root).unwrap();
        let (new_id, new_state) = state
            .path_store
            .fork(current, &session_path, Some(name), &root)
            .unwrap();
        let transcript = state.path_store.transcript_path(&new_id);
        apply_path_session_state(state, &new_state, &transcript);
        new_id
    }

    fn switch_path(state: &mut crate::app_state::AppState, target: &str) {
        let root = workspace_root_path(&state.config);
        let current = PathStore::capture_state(state, &root).unwrap();
        let (path_state, _report) = state.path_store.switch_to(target, current, &root).unwrap();
        let transcript = state
            .path_store
            .transcript_path(state.path_store.active_id());
        apply_path_session_state(state, &path_state, &transcript);
    }

    #[test]
    fn fork_switch_round_trip() {
        let dir = workspace();
        fs::write(dir.path().join("alpha.txt"), "main-v1\n").unwrap();
        let mut state = test_state(dir.path());
        let root = workspace_root_path(&state.config);

        let plan_a = fork_path(&mut state, "plan-a");
        fs::write(dir.path().join("alpha.txt"), "plan-a-v1\n").unwrap();
        fs::write(dir.path().join("branch-only.txt"), "only-a\n").unwrap();
        state
            .path_store
            .persist_active(PathStore::capture_state(&state, &root).unwrap())
            .unwrap();

        switch_path(&mut state, DEFAULT_PATH_ID);
        assert_eq!(
            fs::read_to_string(dir.path().join("alpha.txt")).unwrap(),
            "main-v1\n"
        );
        assert!(!dir.path().join("branch-only.txt").exists());

        switch_path(&mut state, &plan_a);
        assert_eq!(
            fs::read_to_string(dir.path().join("alpha.txt")).unwrap(),
            "plan-a-v1\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("branch-only.txt")).unwrap(),
            "only-a\n"
        );
    }

    #[test]
    fn pick_applies_structured_result() {
        let dir = workspace();
        fs::write(dir.path().join("alpha.txt"), "main\n").unwrap();
        let mut state = test_state(dir.path());
        let root = workspace_root_path(&state.config);
        fork_path(&mut state, "plan-a");
        fs::write(dir.path().join("alpha.txt"), "picked\n").unwrap();
        state
            .path_store
            .persist_active(PathStore::capture_state(&state, &root).unwrap())
            .unwrap();
        switch_path(&mut state, DEFAULT_PATH_ID);

        let result = state.path_store.pick_from("plan-a", &root, false).unwrap();
        assert!(result.applied);
        assert_eq!(result.files, vec!["alpha.txt"]);
        assert_eq!(
            fs::read_to_string(dir.path().join("alpha.txt")).unwrap(),
            "picked\n"
        );
    }

    #[test]
    fn pick_dry_run_does_not_write() {
        let dir = workspace();
        fs::write(dir.path().join("alpha.txt"), "main\n").unwrap();
        let mut state = test_state(dir.path());
        let root = workspace_root_path(&state.config);
        fork_path(&mut state, "plan-a");
        fs::write(dir.path().join("alpha.txt"), "picked\n").unwrap();
        state
            .path_store
            .persist_active(PathStore::capture_state(&state, &root).unwrap())
            .unwrap();
        switch_path(&mut state, DEFAULT_PATH_ID);

        let result = state.path_store.pick_from("plan-a", &root, true).unwrap();
        assert!(!result.applied);
        assert_eq!(result.files, vec!["alpha.txt"]);
        assert_eq!(
            fs::read_to_string(dir.path().join("alpha.txt")).unwrap(),
            "main\n"
        );
    }
}
