use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::tools::patch_changed_files;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointLimits {
    pub max_turns: usize,
    pub max_bytes: u64,
    pub max_file_bytes: u64,
}

impl Default for CheckpointLimits {
    fn default() -> Self {
        Self {
            max_turns: 10,
            max_bytes: 10 * 1024 * 1024,
            max_file_bytes: 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileBaseline {
    pub rel_path: String,
    pub existed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnCheckpoint {
    pub id: String,
    pub created_at: String,
    pub files: BTreeMap<String, FileBaseline>,
    pub skipped: Vec<String>,
    pub snapshot_bytes: u64,
}

impl TurnCheckpoint {
    pub fn new() -> Self {
        Self {
            id: Utc::now().format("%Y-%m-%dT%H-%M-%S-%3fZ").to_string(),
            created_at: Utc::now().to_rfc3339(),
            files: BTreeMap::new(),
            skipped: Vec::new(),
            snapshot_bytes: 0,
        }
    }

    pub fn has_restorable_files(&self) -> bool {
        !self.files.is_empty()
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}

impl Default for TurnCheckpoint {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreReport {
    pub restored: Vec<String>,
    pub removed: Vec<String>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

impl RestoreReport {
    pub fn is_partial(&self) -> bool {
        !self.skipped.is_empty() || !self.errors.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointStack {
    pub checkpoints: Vec<TurnCheckpoint>,
    pub limits: CheckpointLimits,
}

impl CheckpointStack {
    pub fn new(limits: CheckpointLimits) -> Self {
        Self {
            checkpoints: Vec::new(),
            limits,
        }
    }

    pub fn push(&mut self, checkpoint: TurnCheckpoint) {
        self.checkpoints.push(checkpoint);
        while self.checkpoints.len() > self.limits.max_turns {
            self.checkpoints.remove(0);
        }
    }

    pub fn pop(&mut self) -> Option<TurnCheckpoint> {
        self.checkpoints.pop()
    }

    pub fn len(&self) -> usize {
        self.checkpoints.len()
    }

    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty()
    }
}

pub struct TurnCapturer {
    pub checkpoint: TurnCheckpoint,
    pub workspace_root: PathBuf,
    pub limits: CheckpointLimits,
}

impl TurnCapturer {
    pub fn new(workspace_root: impl Into<PathBuf>, limits: CheckpointLimits) -> Self {
        Self {
            checkpoint: TurnCheckpoint::new(),
            workspace_root: workspace_root.into(),
            limits,
        }
    }

    pub async fn snapshot_before_tool(&mut self, tool_name: &str, args: &Value) {
        let paths = paths_from_tool_call(tool_name, args, &self.workspace_root);
        for path in paths {
            let _ = snapshot_file_into_async(
                &mut self.checkpoint,
                &self.workspace_root,
                &path,
                self.limits,
            )
            .await;
        }
    }

    pub fn into_checkpoint(self) -> TurnCheckpoint {
        self.checkpoint
    }
}

pub fn should_push_checkpoint(memory_changed: bool, checkpoint: &TurnCheckpoint) -> bool {
    memory_changed && checkpoint.has_restorable_files()
}

pub fn paths_from_tool_call(tool_name: &str, args: &Value, workspace_root: &Path) -> Vec<String> {
    match tool_name {
        "file_write" | "file_edit" => args
            .get("path")
            .and_then(Value::as_str)
            .and_then(|p| relative_workspace_path(workspace_root, p))
            .into_iter()
            .collect(),
        "apply_patch" => {
            let patch = args.get("patch").and_then(Value::as_str).unwrap_or("");
            let base = args.get("path").and_then(Value::as_str).unwrap_or(".");
            let base_resolved = resolve_under_workspace(workspace_root, base);
            let Some(base_path) = base_resolved else {
                return Vec::new();
            };
            patch_changed_files(patch)
                .into_iter()
                .filter_map(|file| {
                    relative_workspace_path(
                        workspace_root,
                        base_path.join(&file).to_string_lossy().as_ref(),
                    )
                })
                .collect()
        }
        "batch_edit" => {
            let dry_run = args
                .get("dryRun")
                .or_else(|| args.get("dry_run"))
                .and_then(Value::as_bool)
                .unwrap_or(true);
            if dry_run {
                return Vec::new();
            }
            args.get("operations")
                .and_then(Value::as_array)
                .map(|ops| {
                    ops.iter()
                        .filter_map(|op| {
                            op.get("filePath")
                                .or_else(|| op.get("file_path"))
                                .and_then(Value::as_str)
                                .and_then(|p| relative_workspace_path(workspace_root, p))
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

pub fn snapshot_file_into(
    checkpoint: &mut TurnCheckpoint,
    workspace_root: &Path,
    rel_path: &str,
    limits: CheckpointLimits,
) -> std::io::Result<()> {
    let Some(full_path) = prepare_snapshot_path(checkpoint, workspace_root, rel_path)? else {
        return Ok(());
    };
    let bytes = fs::read(&full_path)?;
    ingest_snapshot_bytes(checkpoint, rel_path, bytes, limits);
    Ok(())
}

/// Async twin of [`snapshot_file_into`] for the agent loop: large file reads
/// stay off the async runtime's worker threads.
pub async fn snapshot_file_into_async(
    checkpoint: &mut TurnCheckpoint,
    workspace_root: &Path,
    rel_path: &str,
    limits: CheckpointLimits,
) -> std::io::Result<()> {
    let Some(full_path) = prepare_snapshot_path(checkpoint, workspace_root, rel_path)? else {
        return Ok(());
    };
    let bytes = tokio::fs::read(&full_path).await?;
    ingest_snapshot_bytes(checkpoint, rel_path, bytes, limits);
    Ok(())
}

/// Resolve path and record missing/non-file baselines. `Ok(None)` means the
/// caller should stop (already snapshotted, missing, or not a file).
fn prepare_snapshot_path(
    checkpoint: &mut TurnCheckpoint,
    workspace_root: &Path,
    rel_path: &str,
) -> std::io::Result<Option<PathBuf>> {
    if checkpoint.files.contains_key(rel_path) {
        return Ok(None);
    }
    let Some(full_path) = resolve_under_workspace(workspace_root, rel_path) else {
        return Ok(None);
    };

    let existed = full_path.exists();
    if !existed {
        checkpoint.files.insert(
            rel_path.to_string(),
            FileBaseline {
                rel_path: rel_path.to_string(),
                existed: false,
                content: None,
            },
        );
        return Ok(None);
    }

    if !full_path.is_file() {
        checkpoint.skipped.push(rel_path.to_string());
        return Ok(None);
    }

    Ok(Some(full_path))
}

fn ingest_snapshot_bytes(
    checkpoint: &mut TurnCheckpoint,
    rel_path: &str,
    bytes: Vec<u8>,
    limits: CheckpointLimits,
) {
    if looks_binary(&bytes) {
        checkpoint.skipped.push(rel_path.to_string());
        checkpoint.files.insert(
            rel_path.to_string(),
            FileBaseline {
                rel_path: rel_path.to_string(),
                existed: true,
                content: None,
            },
        );
        return;
    }

    let file_len = bytes.len() as u64;
    if file_len > limits.max_file_bytes {
        checkpoint.skipped.push(rel_path.to_string());
        checkpoint.files.insert(
            rel_path.to_string(),
            FileBaseline {
                rel_path: rel_path.to_string(),
                existed: true,
                content: None,
            },
        );
        return;
    }

    if checkpoint.snapshot_bytes + file_len > limits.max_bytes {
        checkpoint.skipped.push(rel_path.to_string());
        return;
    }

    checkpoint.snapshot_bytes += file_len;
    checkpoint.files.insert(
        rel_path.to_string(),
        FileBaseline {
            rel_path: rel_path.to_string(),
            existed: true,
            content: Some(bytes),
        },
    );
}

pub fn restore_file_baselines(
    files: &BTreeMap<String, FileBaseline>,
    skipped: &[String],
    workspace_root: &Path,
) -> RestoreReport {
    let checkpoint = TurnCheckpoint {
        id: String::new(),
        created_at: String::new(),
        files: files.clone(),
        skipped: skipped.to_vec(),
        snapshot_bytes: 0,
    };
    restore_checkpoint(&checkpoint, workspace_root)
}

pub fn restore_checkpoint(checkpoint: &TurnCheckpoint, workspace_root: &Path) -> RestoreReport {
    let mut report = RestoreReport::default();
    for baseline in checkpoint.files.values() {
        let Some(full_path) = resolve_under_workspace(workspace_root, &baseline.rel_path) else {
            report
                .errors
                .push(format!("{}: outside workspace", baseline.rel_path));
            continue;
        };

        if !baseline.existed {
            if full_path.exists() {
                match fs::remove_file(&full_path) {
                    Ok(()) => report.removed.push(baseline.rel_path.clone()),
                    Err(e) => report
                        .errors
                        .push(format!("{}: failed to remove: {e}", baseline.rel_path)),
                }
            }
            continue;
        }

        match &baseline.content {
            Some(content) => {
                if let Some(parent) = full_path.parent() {
                    if let Err(e) = fs::create_dir_all(parent) {
                        report.errors.push(format!(
                            "{}: failed to create parent dir: {e}",
                            baseline.rel_path
                        ));
                        continue;
                    }
                }
                match fs::write(&full_path, content) {
                    Ok(()) => report.restored.push(baseline.rel_path.clone()),
                    Err(e) => report
                        .errors
                        .push(format!("{}: failed to restore: {e}", baseline.rel_path)),
                }
            }
            None => report.skipped.push(baseline.rel_path.clone()),
        }
    }
    for skipped in &checkpoint.skipped {
        if !report.skipped.iter().any(|p| p == skipped) {
            report.skipped.push(skipped.clone());
        }
    }
    report
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|byte| *byte == 0)
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

fn resolve_under_workspace(workspace_root: &Path, path: &str) -> Option<PathBuf> {
    let workspace_root = normalize_path(workspace_root);
    let joined = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        workspace_root.join(path)
    };
    let normalized = normalize_path(&joined);
    if normalized.starts_with(&workspace_root) {
        Some(normalized)
    } else {
        None
    }
}

fn relative_workspace_path(workspace_root: &Path, path: &str) -> Option<String> {
    let full = resolve_under_workspace(workspace_root, path)?;
    let workspace_root = normalize_path(workspace_root);
    full.strip_prefix(&workspace_root)
        .ok()
        .map(|rel| {
            rel.components()
                .filter_map(|c| match c {
                    Component::Normal(part) => part.to_str().map(str::to_string),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("/")
        })
        .filter(|s| !s.is_empty())
}

pub fn active_tools_need_checkpoints(tool_names: &[String]) -> bool {
    tool_names
        .iter()
        .any(|name| crate::tools::is_mutation_tool(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::process::Command;

    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn paths_from_file_edit() {
        let dir = workspace();
        let paths =
            paths_from_tool_call("file_edit", &json!({ "path": "src/main.rs" }), dir.path());
        assert_eq!(paths, vec!["src/main.rs"]);
    }

    #[tokio::test]
    async fn async_snapshot_matches_sync_api() {
        let dir = workspace();
        fs::write(dir.path().join("note.txt"), "hello\n").unwrap();
        let limits = CheckpointLimits::default();

        let mut sync_cp = TurnCheckpoint::new();
        snapshot_file_into(&mut sync_cp, dir.path(), "note.txt", limits).unwrap();

        let mut async_cp = TurnCheckpoint::new();
        snapshot_file_into_async(&mut async_cp, dir.path(), "note.txt", limits)
            .await
            .unwrap();

        assert_eq!(
            sync_cp
                .files
                .get("note.txt")
                .and_then(|f| f.content.clone()),
            async_cp
                .files
                .get("note.txt")
                .and_then(|f| f.content.clone())
        );
        assert_eq!(sync_cp.snapshot_bytes, async_cp.snapshot_bytes);
    }

    #[test]
    fn paths_from_apply_patch() {
        let dir = workspace();
        let patch = "--- a/foo.txt\n+++ b/foo.txt\n@@ -1 +1 @@\n-a\n+b\n";
        let paths = paths_from_tool_call(
            "apply_patch",
            &json!({ "path": ".", "patch": patch }),
            dir.path(),
        );
        assert_eq!(paths, vec!["foo.txt"]);
    }

    #[test]
    fn paths_from_batch_edit_respects_dry_run() {
        let dir = workspace();
        let dry = paths_from_tool_call(
            "batch_edit",
            &json!({
                "operations": [{ "filePath": "a.txt", "operation": { "type": "replace", "old_string": "a", "new_string": "b" } }],
                "dryRun": true
            }),
            dir.path(),
        );
        assert!(dry.is_empty());
        let apply = paths_from_tool_call(
            "batch_edit",
            &json!({
                "operations": [{ "filePath": "a.txt", "operation": { "type": "replace", "old_string": "a", "new_string": "b" } }],
                "dryRun": false
            }),
            dir.path(),
        );
        assert_eq!(apply, vec!["a.txt"]);
    }

    #[test]
    fn snapshot_and_restore_file_edit() {
        let dir = workspace();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "before\n").unwrap();
        let limits = CheckpointLimits {
            max_file_bytes: 1024,
            ..Default::default()
        };
        let mut checkpoint = TurnCheckpoint::new();
        snapshot_file_into(&mut checkpoint, dir.path(), "src/main.rs", limits).unwrap();
        fs::write(dir.path().join("src/main.rs"), "after\n").unwrap();

        let report = restore_checkpoint(&checkpoint, dir.path());
        assert_eq!(
            fs::read_to_string(dir.path().join("src/main.rs")).unwrap(),
            "before\n"
        );
        assert_eq!(report.restored, vec!["src/main.rs"]);
    }

    #[test]
    fn restore_removes_created_file() {
        let dir = workspace();
        let limits = CheckpointLimits::default();
        let mut checkpoint = TurnCheckpoint::new();
        snapshot_file_into(&mut checkpoint, dir.path(), "new.txt", limits).unwrap();
        fs::write(dir.path().join("new.txt"), "created\n").unwrap();

        let report = restore_checkpoint(&checkpoint, dir.path());
        assert!(!dir.path().join("new.txt").exists());
        assert_eq!(report.removed, vec!["new.txt"]);
    }

    #[test]
    fn restore_overwritten_file() {
        let dir = workspace();
        fs::write(dir.path().join("old.txt"), "original\n").unwrap();
        let limits = CheckpointLimits::default();
        let mut checkpoint = TurnCheckpoint::new();
        snapshot_file_into(&mut checkpoint, dir.path(), "old.txt", limits).unwrap();
        fs::write(dir.path().join("old.txt"), "changed\n").unwrap();

        restore_checkpoint(&checkpoint, dir.path());
        assert_eq!(
            fs::read_to_string(dir.path().join("old.txt")).unwrap(),
            "original\n"
        );
    }

    #[test]
    fn oversized_file_is_skipped_with_warning() {
        let dir = workspace();
        let limits = CheckpointLimits {
            max_file_bytes: 4,
            ..Default::default()
        };
        fs::write(dir.path().join("big.txt"), "1234567890").unwrap();
        let mut checkpoint = TurnCheckpoint::new();
        snapshot_file_into(&mut checkpoint, dir.path(), "big.txt", limits).unwrap();
        fs::write(dir.path().join("big.txt"), "mutated").unwrap();

        let report = restore_checkpoint(&checkpoint, dir.path());
        assert!(report.skipped.contains(&"big.txt".to_string()));
        assert_eq!(
            fs::read_to_string(dir.path().join("big.txt")).unwrap(),
            "mutated"
        );
        assert!(report.is_partial());
    }

    #[test]
    fn outside_workspace_not_snapshotted() {
        let dir = workspace();
        let mut checkpoint = TurnCheckpoint::new();
        snapshot_file_into(
            &mut checkpoint,
            dir.path(),
            "../outside.txt",
            CheckpointLimits::default(),
        )
        .unwrap();
        assert!(checkpoint.files.is_empty());
    }

    #[test]
    fn should_push_requires_memory_changed_and_files() {
        let mut cp = TurnCheckpoint::new();
        assert!(!should_push_checkpoint(true, &cp));
        cp.files.insert(
            "a.txt".into(),
            FileBaseline {
                rel_path: "a.txt".into(),
                existed: false,
                content: None,
            },
        );
        assert!(should_push_checkpoint(true, &cp));
        assert!(!should_push_checkpoint(false, &cp));
    }

    #[test]
    fn stack_trims_oldest() {
        let mut stack = CheckpointStack::new(CheckpointLimits {
            max_turns: 2,
            ..Default::default()
        });
        stack.push(TurnCheckpoint {
            id: "1".into(),
            ..Default::default()
        });
        stack.push(TurnCheckpoint {
            id: "2".into(),
            ..Default::default()
        });
        stack.push(TurnCheckpoint {
            id: "3".into(),
            ..Default::default()
        });
        assert_eq!(stack.len(), 2);
        assert_eq!(stack.checkpoints[0].id, "2");
    }

    #[test]
    fn integration_two_turns_undo_reverts_last_only() {
        let dir = workspace();
        fs::write(dir.path().join("a.txt"), "v0\n").unwrap();

        let limits = CheckpointLimits::default();
        let mut stack = CheckpointStack::new(limits);

        let mut turn1 = TurnCheckpoint::new();
        snapshot_file_into(&mut turn1, dir.path(), "a.txt", limits).unwrap();
        fs::write(dir.path().join("a.txt"), "v1\n").unwrap();
        stack.push(turn1);

        let mut turn2 = TurnCheckpoint::new();
        snapshot_file_into(&mut turn2, dir.path(), "a.txt", limits).unwrap();
        fs::write(dir.path().join("a.txt"), "v2\n").unwrap();
        stack.push(turn2);

        let last = stack.pop().unwrap();
        restore_checkpoint(&last, dir.path());
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "v1\n"
        );
    }

    #[test]
    fn lazy_snapshot_before_untracked_edit() {
        let dir = workspace();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .ok();
        fs::write(dir.path().join("src/new.rs"), "draft\n").unwrap();

        let limits = CheckpointLimits::default();
        let mut checkpoint = TurnCheckpoint::new();
        snapshot_file_into(&mut checkpoint, dir.path(), "src/new.rs", limits).unwrap();
        fs::write(dir.path().join("src/new.rs"), "broken\n").unwrap();

        restore_checkpoint(&checkpoint, dir.path());
        assert_eq!(
            fs::read_to_string(dir.path().join("src/new.rs")).unwrap(),
            "draft\n"
        );
    }

    #[test]
    fn snapshot_works_with_relative_workspace_root() {
        let dir = workspace();
        fs::write(dir.path().join("note.txt"), "before\n").unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let mut checkpoint = TurnCheckpoint::new();
        snapshot_file_into(
            &mut checkpoint,
            Path::new("."),
            "note.txt",
            CheckpointLimits::default(),
        )
        .unwrap();
        std::env::set_current_dir(prev).unwrap();
        assert_eq!(checkpoint.file_count(), 1);
        assert!(checkpoint.files.contains_key("note.txt"));
    }

    #[test]
    fn relative_workspace_root_restore_round_trip() {
        let dir = workspace();
        fs::write(dir.path().join("note.txt"), "before\n").unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let mut checkpoint = TurnCheckpoint::new();
        snapshot_file_into(
            &mut checkpoint,
            Path::new("."),
            "note.txt",
            CheckpointLimits::default(),
        )
        .unwrap();
        fs::write(dir.path().join("note.txt"), "after\n").unwrap();
        let report = restore_checkpoint(&checkpoint, Path::new("."));
        std::env::set_current_dir(prev).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("note.txt")).unwrap(),
            "before\n"
        );
        assert_eq!(report.restored, vec!["note.txt"]);
    }

    #[test]
    fn batch_edit_dry_run_should_not_push() {
        assert!(paths_from_tool_call(
            "batch_edit",
            &json!({ "operations": [{ "filePath": "x.txt" }], "dryRun": true }),
            Path::new("/tmp"),
        )
        .is_empty());
    }
}
