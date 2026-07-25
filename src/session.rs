use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::openai::ChatMessage;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub timestamp: String,
    pub message: ChatMessage,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub path: PathBuf,
    pub modified: SystemTime,
    pub bytes: u64,
    pub messages: usize,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub title: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(rename = "activePathId", default)]
    pub active_path_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionSearchHit {
    pub summary: SessionSummary,
    pub matches: usize,
    pub preview: String,
}

pub fn init_session_dir(dir: &str) -> Result<()> {
    let p = Path::new(dir);
    if !p.exists() {
        std::fs::create_dir_all(p)?;
    }
    Ok(())
}

pub fn new_session_path(dir: &str) -> PathBuf {
    let id = chrono::Utc::now()
        .format("%Y-%m-%dT%H-%M-%S-%3f")
        .to_string()
        + "Z";
    Path::new(dir).join(format!("{id}.jsonl"))
}

pub fn save_message(path: &Path, message: &ChatMessage) -> Result<()> {
    let entry = SessionEntry {
        timestamp: chrono::Utc::now().to_rfc3339(),
        message: message.clone(),
    };
    let line = serde_json::to_string(&entry)?;
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

pub fn metadata_path(path: &Path) -> PathBuf {
    path.with_extension("meta.json")
}

pub fn load_session_metadata(path: &Path) -> Result<SessionMetadata> {
    let meta_path = metadata_path(path);
    if !meta_path.exists() {
        return Ok(SessionMetadata::default());
    }
    let text = fs::read_to_string(meta_path)?;
    Ok(serde_json::from_str(&text).unwrap_or_default())
}

pub fn save_session_metadata(path: &Path, metadata: &SessionMetadata) -> Result<()> {
    let meta_path = metadata_path(path);
    if let Some(parent) = meta_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(meta_path, serde_json::to_string_pretty(metadata)? + "\n")?;
    Ok(())
}

pub fn set_session_title(path: &Path, title: &str) -> Result<()> {
    let mut metadata = load_session_metadata(path)?;
    metadata.title = if title.trim().is_empty() {
        None
    } else {
        Some(title.trim().to_string())
    };
    save_session_metadata(path, &metadata)
}

pub fn load_session(path: &Path) -> Result<Vec<SessionEntry>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut entries = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        entries.push(serde_json::from_str::<SessionEntry>(&line)?);
    }
    Ok(entries)
}

pub fn load_messages(path: &Path) -> Result<Vec<ChatMessage>> {
    Ok(load_session(path)?
        .into_iter()
        .map(|entry| entry.message)
        .collect())
}

pub fn list_sessions(dir: &str) -> Result<Vec<SessionSummary>> {
    let mut out = Vec::new();
    let p = Path::new(dir);
    if !p.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(p)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("history.jsonl") {
            continue;
        }
        let metadata = entry.metadata()?;
        let entries = load_session(&path).unwrap_or_default();
        let messages = entries.len();
        let title = load_session_metadata(&path)
            .ok()
            .and_then(|m| m.title)
            .or_else(|| infer_session_title(&entries));
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        out.push(SessionSummary {
            id,
            path,
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            bytes: metadata.len(),
            messages,
            title,
        });
    }
    out.sort_by_key(|session| Reverse(session.modified));
    Ok(out)
}

pub fn resolve_session_path(dir: &str, id: &str) -> Result<Option<PathBuf>> {
    if id == "latest" {
        return Ok(list_sessions(dir)?.into_iter().next().map(|s| s.path));
    }
    let direct = Path::new(id);
    if direct.exists() {
        return Ok(Some(direct.to_path_buf()));
    }
    let trimmed = id.strip_suffix(".jsonl").unwrap_or(id);
    let candidate = Path::new(dir).join(format!("{trimmed}.jsonl"));
    if candidate.exists() {
        return Ok(Some(candidate));
    }
    Ok(None)
}

pub fn delete_session(dir: &str, id: &str) -> Result<Option<PathBuf>> {
    let Some(path) = resolve_session_path(dir, id)? else {
        return Ok(None);
    };
    if path.exists() {
        fs::remove_file(&path)?;
    }
    let meta = metadata_path(&path);
    if meta.exists() {
        fs::remove_file(meta)?;
    }
    Ok(Some(path))
}

pub fn search_sessions(dir: &str, query: &str) -> Result<Vec<SessionSearchHit>> {
    let needles: Vec<String> = query
        .to_lowercase()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if needles.is_empty() {
        return Ok(Vec::new());
    }
    let mut hits = Vec::new();
    for summary in list_sessions(dir)? {
        let entries = load_session(&summary.path).unwrap_or_default();
        let mut haystack = summary.title.clone().unwrap_or_default();
        for entry in &entries {
            haystack.push('\n');
            haystack.push_str(&message_text(&entry.message));
        }
        let lower = haystack.to_lowercase();
        let matches = needles
            .iter()
            .filter(|needle| lower.contains(needle.as_str()))
            .count();
        if matches == 0 {
            continue;
        }
        let preview = entries
            .iter()
            .find_map(|entry| {
                let text = message_text(&entry.message);
                let lower_text = text.to_lowercase();
                if needles.iter().any(|needle| lower_text.contains(needle)) {
                    Some(one_line(&text, 120))
                } else {
                    None
                }
            })
            .or_else(|| summary.title.clone())
            .unwrap_or_default();
        hits.push(SessionSearchHit {
            summary,
            matches,
            preview,
        });
    }
    hits.sort_by(|a, b| {
        b.matches
            .cmp(&a.matches)
            .then_with(|| b.summary.modified.cmp(&a.summary.modified))
    });
    Ok(hits)
}

fn infer_session_title(entries: &[SessionEntry]) -> Option<String> {
    entries.iter().find_map(|entry| match &entry.message {
        ChatMessage::User { content } => {
            let text = content.as_text();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(one_line(trimmed, 64))
            }
        }
        _ => None,
    })
}

fn message_text(message: &ChatMessage) -> String {
    match message {
        ChatMessage::System { content } | ChatMessage::Tool { content, .. } => content.clone(),
        ChatMessage::User { content } => content.as_text().into_owned(),
        ChatMessage::Assistant { content, .. } => content.clone().unwrap_or_default(),
    }
}

fn one_line(text: &str, max: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max {
        compact
    } else {
        let mut out: String = compact.chars().take(max).collect();
        out.push('…');
        out
    }
}

pub fn render_markdown(entries: &[SessionEntry]) -> String {
    let mut out = String::from("# Albatross Session\n\n");
    for entry in entries {
        match &entry.message {
            ChatMessage::System { content } => {
                out.push_str("## System\n\n");
                out.push_str(content);
                out.push_str("\n\n");
            }
            ChatMessage::User { content } => {
                out.push_str("## User\n\n");
                out.push_str(&content.as_text());
                out.push_str("\n\n");
            }
            ChatMessage::Assistant {
                content,
                tool_calls,
            } => {
                out.push_str("## Assistant\n\n");
                if let Some(content) = content {
                    out.push_str(content);
                    out.push_str("\n\n");
                }
                if !tool_calls.is_empty() {
                    out.push_str("```json\n");
                    out.push_str(&serde_json::to_string_pretty(tool_calls).unwrap_or_default());
                    out.push_str("\n```\n\n");
                }
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => {
                out.push_str(&format!("## Tool `{tool_call_id}`\n\n"));
                out.push_str("```json\n");
                out.push_str(content);
                out.push_str("\n```\n\n");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_loads_session_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        save_message(
            &path,
            &ChatMessage::User {
                content: "hello".into(),
            },
        )
        .unwrap();
        let entries = load_session(&path).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].message, ChatMessage::User { .. }));
    }

    #[test]
    fn list_sessions_ignores_history() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("2026.jsonl");
        let history = dir.path().join("history.jsonl");
        save_message(
            &session,
            &ChatMessage::User {
                content: "hello".into(),
            },
        )
        .unwrap();
        std::fs::write(history, "{}\n").unwrap();
        let sessions = list_sessions(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "2026");
    }
}
