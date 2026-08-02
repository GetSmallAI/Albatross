use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{diff::unified_diff, PathPolicy, Tool, ToolPreview};

pub struct FileEditTool {
    pub approve: bool,
    pub path_policy: PathPolicy,
}

#[derive(Deserialize)]
struct Args {
    path: String,
    edits: Vec<Edit>,
}

#[derive(Deserialize)]
struct Edit {
    old_text: String,
    new_text: String,
}

/// Locate the line span that changed between `original` and `updated`, as a
/// half-open `[start, end)` range of 0-based line indices into `updated`.
/// Returns `None` when nothing changed. Uses a common prefix/suffix scan, so a
/// small edit in a large file yields a small span.
fn changed_line_span(original: &str, updated: &str) -> Option<(usize, usize)> {
    let o: Vec<&str> = original.lines().collect();
    let u: Vec<&str> = updated.lines().collect();
    if o == u {
        return None;
    }
    let max_pre = o.len().min(u.len());
    let mut pre = 0;
    while pre < max_pre && o[pre] == u[pre] {
        pre += 1;
    }
    let max_suf = (o.len() - pre).min(u.len() - pre);
    let mut suf = 0;
    while suf < max_suf && o[o.len() - 1 - suf] == u[u.len() - 1 - suf] {
        suf += 1;
    }
    let start = pre;
    // Show at least one line so a pure deletion still anchors somewhere.
    let end = u.len().saturating_sub(suf).max(start + 1).min(u.len());
    Some((start, end))
}

/// Render a line-numbered snippet of `content` covering `[start, end)` plus a
/// few lines of context on each side. Capped so a huge edit can't flood the
/// model's context.
fn numbered_snippet(content: &str, start: usize, end: usize, context: usize) -> String {
    const MAX_LINES: usize = 30;
    let lines: Vec<&str> = content.lines().collect();
    let from = start.saturating_sub(context);
    let full_to = (end + context).min(lines.len());
    let truncated = full_to.saturating_sub(from) > MAX_LINES;
    let to = if truncated { from + MAX_LINES } else { full_to };
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate().take(to).skip(from) {
        out.push_str(&format!("{:>5}  {}\n", i + 1, line));
    }
    if truncated {
        out.push_str("…[snippet truncated]\n");
    }
    out.truncate(out.trim_end().len());
    out
}

#[async_trait]
impl Tool for FileEditTool {
    fn name(&self) -> &'static str {
        "file_edit"
    }
    fn description(&self) -> &'static str {
        "Apply search-and-replace edits to a file. Each old_text must appear exactly once. Returns a unified diff plus the re-read applied state (verified + a line-numbered snippet) so you can confirm the change landed."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute or relative path to the file" },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_text": { "type": "string", "description": "Exact text to find (must appear once)" },
                            "new_text": { "type": "string", "description": "Text to replace it with" }
                        },
                        "required": ["old_text", "new_text"]
                    }
                }
            },
            "required": ["path", "edits"]
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
        let original = tokio::fs::read_to_string(&resolved.normalized).await.ok()?;
        let mut working = original.clone();
        for edit in &args.edits {
            if edit.old_text.is_empty() || working.matches(&edit.old_text).count() != 1 {
                return Some(ToolPreview {
                    summary: format!("Edit {}", resolved.normalized.display()),
                    diff: None,
                    risk: Some(
                        "preview unavailable until each old_text matches exactly once".into(),
                    ),
                });
            }
            working = working.replacen(&edit.old_text, &edit.new_text, 1);
        }
        let mut risk = None;
        if resolved.outside_workspace {
            risk = Some(format!(
                "outside workspace root {}",
                self.path_policy.root().display()
            ));
        }
        Some(ToolPreview {
            summary: format!(
                "Edit {} ({} edits)",
                resolved.normalized.display(),
                args.edits.len()
            ),
            diff: Some(unified_diff(
                &original,
                &working,
                &resolved.normalized.display().to_string(),
            )),
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
        let original = match tokio::fs::read_to_string(&resolved.normalized).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Honour the Claude Code convention: a single edit with empty old_text on a
                // missing file means "create this file with new_text as its entire content".
                // This avoids the retry loop where models trained on Claude Code's Edit tool
                // send file_edit({old_text: "", new_text: <content>}) to create new files.
                if args.edits.len() == 1 && args.edits[0].old_text.is_empty() {
                    let content = &args.edits[0].new_text;
                    if let Some(parent) = resolved.normalized.parent() {
                        if !parent.as_os_str().is_empty() {
                            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                                return json!({ "error": e.to_string() });
                            }
                        }
                    }
                    return match tokio::fs::write(&resolved.normalized, content.as_bytes()).await {
                        Ok(_) => json!({
                            "edited": true,
                            "created": true,
                            "path": path,
                            "diff": unified_diff("", content, &path),
                            "addedLines": content.lines().count(),
                            "removedLines": 0,
                            "hunks": usize::from(!content.is_empty()),
                            "verified": true,
                            "applied_snippet": content,
                        }),
                        Err(e) => json!({ "error": e.to_string() }),
                    };
                }
                return json!({ "error": format!("File not found: {path}. Use file_write to create new files.") });
            }
            Err(e) => return json!({ "error": e.to_string() }),
        };
        let mut working = original.clone();
        for (idx, edit) in args.edits.iter().enumerate() {
            if edit.old_text.is_empty() {
                return json!({ "error": format!("Edit {}: old_text is empty. Use file_write to create new files.", idx + 1) });
            }
            let occurrences = working.matches(&edit.old_text).count();
            if occurrences == 0 {
                return json!({ "error": format!("Edit {}: old_text not found", idx + 1) });
            }
            if occurrences > 1 {
                return json!({
                    "error": format!(
                        "Edit {}: old_text appears {} times — make it unique",
                        idx + 1,
                        occurrences
                    )
                });
            }
            working = working.replacen(&edit.old_text, &edit.new_text, 1);
        }
        if let Err(e) = tokio::fs::write(&resolved.normalized, working.as_bytes()).await {
            return json!({ "error": e.to_string() });
        }
        // Re-read from disk so the model sees the actually-applied state, not an
        // assumption. `verified` is false if what landed on disk differs from
        // what we intended to write (e.g. a concurrent writer or odd encoding).
        let on_disk = tokio::fs::read_to_string(&resolved.normalized)
            .await
            .unwrap_or_else(|_| working.clone());
        let verified = on_disk == working;
        let applied_snippet = changed_line_span(&original, &on_disk)
            .map(|(start, end)| numbered_snippet(&on_disk, start, end, 3))
            .unwrap_or_default();
        json!({
            "edited": true,
            "path": path,
            "diff": unified_diff(&original, &working, &path),
            "verified": verified,
            "applied_snippet": applied_snippet,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_identical_has_no_hunks() {
        let d = unified_diff("a\nb\nc", "a\nb\nc", "f.txt");
        assert_eq!(d, "--- f.txt\n+++ f.txt");
    }

    #[test]
    fn diff_single_replacement() {
        let d = unified_diff("a\nb\nc", "a\nB\nc", "f.txt");
        assert!(d.contains("-b"));
        assert!(d.contains("+B"));
        assert!(d.contains("@@ -2 +2 @@"));
    }

    #[test]
    fn diff_includes_added_line() {
        let d = unified_diff("a\nc", "a\nb\nc", "f.txt");
        assert!(d.contains("+b"));
        assert!(d.contains("@@ -2 +2 @@"));
    }

    #[test]
    fn diff_includes_removed_line() {
        let d = unified_diff("a\nb\nc", "a\nc", "f.txt");
        assert!(d.contains("-b"));
        assert!(d.contains("@@ -2 +2 @@"));
    }

    #[test]
    fn diff_header_uses_path() {
        let d = unified_diff("x", "y", "/abs/path/foo.rs");
        assert!(d.starts_with("--- /abs/path/foo.rs\n+++ /abs/path/foo.rs"));
    }

    #[test]
    fn changed_span_none_when_identical() {
        assert_eq!(changed_line_span("a\nb\nc", "a\nb\nc"), None);
    }

    #[test]
    fn changed_span_is_tight_around_single_edit() {
        // Only line index 2 changes in a 5-line file.
        let original = "a\nb\nc\nd\ne";
        let updated = "a\nb\nC\nd\ne";
        assert_eq!(changed_line_span(original, updated), Some((2, 3)));
    }

    #[test]
    fn numbered_snippet_uses_one_based_line_numbers_and_context() {
        let content = "a\nb\nc\nd\ne";
        // changed line index 2 (=line 3), context 1 → lines 2..=4
        let snip = numbered_snippet(content, 2, 3, 1);
        assert!(snip.contains("    2  b"));
        assert!(snip.contains("    3  c"));
        assert!(snip.contains("    4  d"));
        assert!(!snip.contains("    1  a"));
        assert!(!snip.contains("    5  e"));
    }

    #[tokio::test]
    async fn creates_new_file_when_old_text_empty_and_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/new.html");

        let result = FileEditTool {
            approve: false,
            path_policy: PathPolicy::default(),
        }
        .execute(json!({
            "path": path.to_str().unwrap(),
            "edits": [{ "old_text": "", "new_text": "<h1>hello</h1>" }]
        }))
        .await;

        assert!(result["edited"].as_bool().unwrap(), "{result}");
        assert_eq!(result["created"].as_bool(), Some(true));
        assert_eq!(result["addedLines"].as_u64(), Some(1));
        assert_eq!(result["removedLines"].as_u64(), Some(0));
        assert_eq!(result["hunks"].as_u64(), Some(1));
        assert_eq!(result["verified"].as_bool(), Some(true));
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "<h1>hello</h1>");
    }

    #[tokio::test]
    async fn file_not_found_with_nonempty_old_text_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.txt");

        let result = FileEditTool {
            approve: false,
            path_policy: PathPolicy::default(),
        }
        .execute(json!({
            "path": path.to_str().unwrap(),
            "edits": [{ "old_text": "something", "new_text": "else" }]
        }))
        .await;

        let err = result["error"].as_str().unwrap();
        assert!(err.contains("File not found"), "{err}");
        assert!(err.contains("file_write"), "{err}");
    }

    #[tokio::test]
    async fn applies_unique_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        tokio::fs::write(&path, "alpha\nbeta\ngamma").await.unwrap();

        let result = FileEditTool {
            approve: false,
            path_policy: PathPolicy::default(),
        }
        .execute(json!({
            "path": path.to_str().unwrap(),
            "edits": [{ "old_text": "beta", "new_text": "BETA" }]
        }))
        .await;

        assert!(result["edited"].as_bool().unwrap());
        // Re-read verification confirms the write landed and surfaces the
        // applied region back to the model.
        assert_eq!(result["verified"].as_bool(), Some(true));
        assert!(result["applied_snippet"].as_str().unwrap().contains("BETA"));
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "alpha\nBETA\ngamma");
    }

    #[tokio::test]
    async fn rejects_non_unique_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        tokio::fs::write(&path, "x\nx\nx").await.unwrap();

        let result = FileEditTool {
            approve: false,
            path_policy: PathPolicy::default(),
        }
        .execute(json!({
            "path": path.to_str().unwrap(),
            "edits": [{ "old_text": "x", "new_text": "y" }]
        }))
        .await;
        assert!(result["error"].as_str().unwrap().contains("3 times"));
    }

    #[tokio::test]
    async fn rejects_missing_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        tokio::fs::write(&path, "alpha").await.unwrap();

        let result = FileEditTool {
            approve: false,
            path_policy: PathPolicy::default(),
        }
        .execute(json!({
            "path": path.to_str().unwrap(),
            "edits": [{ "old_text": "missing", "new_text": "x" }]
        }))
        .await;
        assert!(result["error"].as_str().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn applies_sequential_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        tokio::fs::write(&path, "one two three").await.unwrap();

        let result = FileEditTool {
            approve: false,
            path_policy: PathPolicy::default(),
        }
        .execute(json!({
            "path": path.to_str().unwrap(),
            "edits": [
                { "old_text": "one", "new_text": "ONE" },
                { "old_text": "three", "new_text": "THREE" }
            ]
        }))
        .await;

        assert!(result["edited"].as_bool().unwrap());
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "ONE two THREE");
    }
}
