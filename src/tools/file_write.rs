use super::{diff::unified_diff, PathPolicy, Tool, ToolPreview};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

pub struct FileWriteTool {
    pub approve: bool,
    pub path_policy: PathPolicy,
}

#[derive(Deserialize)]
struct Args {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &'static str {
        "file_write"
    }
    fn description(&self) -> &'static str {
        "Write content to a file. Creates parent directories if needed. Overwrites if the file exists."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute or relative path to the file" },
                "content": { "type": "string", "description": "Full content to write" }
            },
            "required": ["path", "content"]
        })
    }
    fn require_approval(&self, _args: &Value) -> bool {
        self.approve
            || _args
                .get("path")
                .and_then(Value::as_str)
                .map(|p| self.path_policy.require_prompt_for_path(p))
                .unwrap_or(false)
    }
    async fn preview(&self, args: &Value) -> Option<ToolPreview> {
        let args: Args = serde_json::from_value(args.clone()).ok()?;
        let resolved = self.path_policy.resolve(&args.path);
        let old = tokio::fs::read_to_string(&resolved.normalized)
            .await
            .unwrap_or_default();
        let mut risk = None;
        if resolved.outside_workspace {
            risk = Some(format!(
                "outside workspace root {}",
                self.path_policy.root().display()
            ));
        }
        let path = resolved.normalized.display().to_string();
        Some(ToolPreview {
            summary: format!("Write {path} ({} bytes)", args.content.len()),
            diff: Some(unified_diff(&old, &args.content, &path)),
            risk,
        })
    }
    async fn execute(&self, args: Value) -> Value {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return json!({ "error": format!("invalid args: {e}") }),
        };
        if let Some(error) = self.path_policy.deny_path(&args.path) {
            return json!({ "error": error });
        }
        let resolved = self.path_policy.resolve(&args.path);
        let path = resolved.normalized.display().to_string();
        if let Some(parent) = resolved.normalized.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return json!({ "error": e.to_string() });
                }
            }
        }
        match tokio::fs::write(&resolved.normalized, args.content.as_bytes()).await {
            Ok(_) => json!({
                "written": true,
                "path": path,
                "bytes": args.content.len(),
            }),
            Err(e) => json!({ "error": e.to_string() }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_file_to_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let result = FileWriteTool {
            approve: false,
            path_policy: PathPolicy::default(),
        }
        .execute(json!({
            "path": path.to_str().unwrap(),
            "content": "hello"
        }))
        .await;
        assert!(result["written"].as_bool().unwrap());
        assert_eq!(result["bytes"].as_u64().unwrap(), 5);
        let read = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(read, "hello");
    }

    #[tokio::test]
    async fn writes_file_creating_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c/file.txt");
        let result = FileWriteTool {
            approve: false,
            path_policy: PathPolicy::default(),
        }
        .execute(json!({
            "path": path.to_str().unwrap(),
            "content": "deep"
        }))
        .await;
        assert!(result["written"].as_bool().unwrap());
        let read = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(read, "deep");
    }

    #[tokio::test]
    async fn write_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("o.txt");
        tokio::fs::write(&path, "old contents").await.unwrap();
        let _ = FileWriteTool {
            approve: false,
            path_policy: PathPolicy::default(),
        }
        .execute(json!({
            "path": path.to_str().unwrap(),
            "content": "new"
        }))
        .await;
        let read = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(read, "new");
    }

    #[test]
    fn approval_field_drives_require_approval() {
        let t1 = FileWriteTool {
            approve: true,
            path_policy: PathPolicy::default(),
        };
        let t2 = FileWriteTool {
            approve: false,
            path_policy: PathPolicy::default(),
        };
        let v = json!({});
        assert!(t1.require_approval(&v));
        assert!(!t2.require_approval(&v));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deny_policy_rejects_write_through_workspace_symlink() {
        use crate::config::OutsideWorkspace;
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), workspace.path().join("link")).unwrap();
        let result = FileWriteTool {
            approve: false,
            path_policy: PathPolicy::new(
                workspace.path().to_str().unwrap(),
                OutsideWorkspace::Deny,
            ),
        }
        .execute(json!({ "path": "link/secret.txt", "content": "leak" }))
        .await;
        assert!(result.get("error").is_some());
        assert!(!outside.path().join("secret.txt").exists());
    }
}
