use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::config::AgentConfig;
use crate::project_memory::load_project_index;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchEditOperation {
    pub file_path: String,
    pub operation: EditOperation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum EditOperation {
    Replace {
        old_string: String,
        new_string: String,
    },
    Insert {
        position: InsertPosition,
        content: String,
    },
    Delete {
        pattern: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InsertPosition {
    AfterLine { line: usize },
    BeforeLine { line: usize },
    AtEnd,
    AtStart,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchPreview {
    pub operations: Vec<BatchEditOperation>,
    pub total_files: usize,
    pub estimated_changes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchResult {
    pub successful: Vec<String>,
    pub failed: Vec<FailedOperation>,
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedOperation {
    pub file_path: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrossFileReference {
    pub from_file: String,
    pub to_file: String,
    pub reference_type: String,
    pub line: usize,
}

pub fn find_cross_file_references(
    config: &AgentConfig,
    target_file: &str,
) -> Result<Vec<CrossFileReference>> {
    let Some(index) = load_project_index(config)? else {
        return Ok(Vec::new());
    };

    let target_symbols: Vec<String> = index
        .files
        .iter()
        .find(|f| f.path == target_file)
        .map(|f| {
            f.symbols
                .iter()
                .map(|s| s.name.clone())
                .filter(|name| !name.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let mut references = Vec::new();
    let workspace_root = normalize_workspace_root(Path::new(&config.workspace_root));

    for file in &index.files {
        if file.path == target_file {
            continue;
        }

        for import in &file.imports {
            if target_symbols.iter().any(|symbol| symbol == import) || import.contains(target_file)
            {
                references.push(CrossFileReference {
                    from_file: file.path.clone(),
                    to_file: target_file.to_string(),
                    reference_type: "import".to_string(),
                    line: 0,
                });
            }
        }

        let Ok(full_path) = resolve_workspace_file(&workspace_root, &file.path) else {
            continue;
        };
        let Ok(content) = fs::read_to_string(full_path) else {
            continue;
        };
        for (line_idx, line) in content.lines().enumerate() {
            for symbol in &target_symbols {
                if contains_identifier(line, symbol) {
                    references.push(CrossFileReference {
                        from_file: file.path.clone(),
                        to_file: target_file.to_string(),
                        reference_type: "usage".to_string(),
                        line: line_idx + 1,
                    });
                    break;
                }
            }
            if references.len() >= 100 {
                return Ok(references);
            }
        }
    }

    Ok(references)
}

pub fn find_related_files(config: &AgentConfig, file_path: &str) -> Result<Vec<String>> {
    let references = find_cross_file_references(config, file_path)?;
    let mut related_files = HashSet::new();

    for ref_info in references {
        related_files.insert(ref_info.from_file);
    }

    Ok(related_files.into_iter().collect())
}

fn contains_identifier(line: &str, symbol: &str) -> bool {
    let mut start = 0;
    while let Some(offset) = line[start..].find(symbol) {
        let idx = start + offset;
        let before = line[..idx].chars().next_back();
        let after = line[idx + symbol.len()..].chars().next();
        let before_ok = before.map(|c| !is_ident_char(c)).unwrap_or(true);
        let after_ok = after.map(|c| !is_ident_char(c)).unwrap_or(true);
        if before_ok && after_ok {
            return true;
        }
        start = idx + symbol.len();
    }
    false
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

pub fn preview_batch_operations(operations: &[BatchEditOperation]) -> BatchPreview {
    BatchPreview {
        operations: operations.to_vec(),
        total_files: operations.len(),
        estimated_changes: operations.len(),
    }
}

pub fn execute_batch_operations(
    operations: &[BatchEditOperation],
    workspace_root: &Path,
    dry_run: bool,
) -> Result<BatchResult> {
    let workspace_root = normalize_workspace_root(workspace_root);
    let mut failed = Vec::new();
    let mut grouped: BTreeMap<PathBuf, (String, Vec<EditOperation>)> = BTreeMap::new();

    for op in operations {
        let full_path = match resolve_workspace_file(&workspace_root, &op.file_path) {
            Ok(path) => path,
            Err(e) => {
                failed.push(FailedOperation {
                    file_path: op.file_path.clone(),
                    error: e.to_string(),
                });
                continue;
            }
        };
        if !full_path.exists() {
            failed.push(FailedOperation {
                file_path: op.file_path.clone(),
                error: "File not found".to_string(),
            });
            continue;
        }
        if !full_path.is_file() {
            failed.push(FailedOperation {
                file_path: op.file_path.clone(),
                error: "Path is not a file".to_string(),
            });
            continue;
        }
        grouped
            .entry(full_path)
            .or_insert_with(|| (op.file_path.clone(), Vec::new()))
            .1
            .push(op.operation.clone());
    }

    if !failed.is_empty() {
        return Ok(BatchResult {
            successful: Vec::new(),
            failed,
            skipped: Vec::new(),
        });
    }

    let mut originals = HashMap::new();
    let mut updated = Vec::new();
    for (path, (label, ops)) in &grouped {
        let original = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) => {
                failed.push(FailedOperation {
                    file_path: label.clone(),
                    error: e.to_string(),
                });
                continue;
            }
        };
        let mut working = original.clone();
        for (idx, op) in ops.iter().enumerate() {
            match apply_edit_to_content(&working, op) {
                Ok(content) => working = content,
                Err(e) => {
                    failed.push(FailedOperation {
                        file_path: label.clone(),
                        error: format!("operation {}: {e}", idx + 1),
                    });
                    break;
                }
            }
        }
        originals.insert(path.clone(), original);
        updated.push((path.clone(), label.clone(), working));
    }

    if !failed.is_empty() {
        return Ok(BatchResult {
            successful: Vec::new(),
            failed,
            skipped: Vec::new(),
        });
    }

    if dry_run {
        return Ok(BatchResult {
            successful: Vec::new(),
            failed: Vec::new(),
            skipped: updated.into_iter().map(|(_, label, _)| label).collect(),
        });
    }

    let mut written = Vec::new();
    for (path, label, content) in &updated {
        if let Err(e) = fs::write(path, content) {
            for written_path in &written {
                if let Some(original) = originals.get(written_path) {
                    let _ = fs::write(written_path, original);
                }
            }
            return Ok(BatchResult {
                successful: Vec::new(),
                failed: vec![FailedOperation {
                    file_path: label.clone(),
                    error: format!("write failed and prior files were rolled back: {e}"),
                }],
                skipped: Vec::new(),
            });
        }
        written.push(path.clone());
    }

    Ok(BatchResult {
        successful: updated.into_iter().map(|(_, label, _)| label).collect(),
        failed: Vec::new(),
        skipped: Vec::new(),
    })
}

fn apply_edit_to_content(content: &str, operation: &EditOperation) -> Result<String> {
    match operation {
        EditOperation::Replace {
            old_string,
            new_string,
        } => {
            ensure_unique_match(content, old_string, "old_string")?;
            Ok(content.replacen(old_string, new_string, 1))
        }
        EditOperation::Insert {
            position,
            content: insert_content,
        } => insert_content_at_position(content, position, insert_content),
        EditOperation::Delete { pattern } => {
            ensure_unique_match(content, pattern, "pattern")?;
            Ok(content.replacen(pattern, "", 1))
        }
    }
}

fn ensure_unique_match(content: &str, needle: &str, label: &str) -> Result<()> {
    if needle.is_empty() {
        return Err(anyhow!("{label} is empty"));
    }
    let occurrences = content.matches(needle).count();
    match occurrences {
        0 => Err(anyhow!("{label} not found")),
        1 => Ok(()),
        n => Err(anyhow!("{label} appears {n} times")),
    }
}

fn insert_content_at_position(
    content: &str,
    position: &InsertPosition,
    insert_content: &str,
) -> Result<String> {
    match position {
        InsertPosition::AtStart => Ok(join_inserted(insert_content, content)),
        InsertPosition::AtEnd => Ok(join_inserted(content, insert_content)),
        InsertPosition::BeforeLine { line } | InsertPosition::AfterLine { line } => {
            let line_count = content.lines().count();
            if *line == 0 || *line > line_count {
                return Err(anyhow!("line number out of range"));
            }
            let had_trailing_newline = content.ends_with('\n');
            let mut lines: Vec<String> = content.split('\n').map(str::to_string).collect();
            if had_trailing_newline {
                lines.pop();
            }
            let insert_at = match position {
                InsertPosition::BeforeLine { line } => line - 1,
                InsertPosition::AfterLine { line } => *line,
                _ => unreachable!(),
            };
            lines.insert(insert_at, insert_content.to_string());
            let mut out = lines.join("\n");
            if had_trailing_newline {
                out.push('\n');
            }
            Ok(out)
        }
    }
}

fn join_inserted(left: &str, right: &str) -> String {
    if left.is_empty() {
        right.to_string()
    } else if right.is_empty() || left.ends_with('\n') {
        format!("{left}{right}")
    } else {
        format!("{left}\n{right}")
    }
}

fn resolve_workspace_file(workspace_root: &Path, file_path: &str) -> Result<PathBuf> {
    let input = Path::new(file_path);
    if file_path.trim().is_empty() {
        return Err(anyhow!("file path is empty"));
    }
    if input.is_absolute() {
        return Err(anyhow!("absolute paths are not allowed"));
    }
    let mut clean = PathBuf::new();
    for component in input.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            Component::ParentDir => return Err(anyhow!("parent paths are not allowed")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(anyhow!("absolute paths are not allowed"));
            }
        }
    }
    if clean.as_os_str().is_empty() {
        return Err(anyhow!("file path is empty"));
    }
    let full_path = crate::path_security::resolve_existing_prefix(&workspace_root.join(clean));
    if !full_path.starts_with(workspace_root) {
        return Err(anyhow!("path escapes workspace root"));
    }
    Ok(full_path)
}

fn normalize_workspace_root(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| normalize_path(path))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paths_that_escape_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let operations = vec![BatchEditOperation {
            file_path: "../outside.txt".into(),
            operation: EditOperation::Delete {
                pattern: "x".into(),
            },
        }];

        let result = execute_batch_operations(&operations, dir.path(), false).unwrap();

        assert_eq!(result.failed.len(), 1);
        assert!(result.failed[0].error.contains("parent paths"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_files_that_escape_workspace() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "keep\n").unwrap();
        symlink(outside.path(), workspace.path().join("link")).unwrap();
        let operations = vec![BatchEditOperation {
            file_path: "link/secret.txt".into(),
            operation: EditOperation::Replace {
                old_string: "keep".into(),
                new_string: "changed".into(),
            },
        }];
        let result = execute_batch_operations(&operations, workspace.path(), false).unwrap();
        assert!(result.successful.is_empty());
        assert_eq!(result.failed.len(), 1);
        assert_eq!(
            fs::read_to_string(outside.path().join("secret.txt")).unwrap(),
            "keep\n"
        );
    }

    #[test]
    fn applies_multiple_file_batch_successfully() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        fs::write(dir.path().join("b.txt"), "one\ntwo\n").unwrap();
        let operations = vec![
            BatchEditOperation {
                file_path: "a.txt".into(),
                operation: EditOperation::Replace {
                    old_string: "beta".into(),
                    new_string: "BETA".into(),
                },
            },
            BatchEditOperation {
                file_path: "b.txt".into(),
                operation: EditOperation::Insert {
                    position: InsertPosition::AfterLine { line: 1 },
                    content: "inserted".into(),
                },
            },
        ];

        let result = execute_batch_operations(&operations, dir.path(), false).unwrap();

        assert_eq!(result.successful.len(), 2);
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "alpha\nBETA\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "one\ninserted\ntwo\n"
        );
    }

    #[test]
    fn failed_batch_does_not_write_partial_changes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        fs::write(dir.path().join("b.txt"), "one\ntwo\n").unwrap();
        let operations = vec![
            BatchEditOperation {
                file_path: "a.txt".into(),
                operation: EditOperation::Replace {
                    old_string: "beta".into(),
                    new_string: "BETA".into(),
                },
            },
            BatchEditOperation {
                file_path: "b.txt".into(),
                operation: EditOperation::Replace {
                    old_string: "missing".into(),
                    new_string: "MISS".into(),
                },
            },
        ];

        let result = execute_batch_operations(&operations, dir.path(), false).unwrap();

        assert!(result.successful.is_empty());
        assert_eq!(result.failed.len(), 1);
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "alpha\nbeta\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "one\ntwo\n"
        );
    }
}
