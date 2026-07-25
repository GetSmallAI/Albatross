use anyhow::{anyhow, Result};
use chrono::Utc;
use ignore::WalkBuilder;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

use crate::backends::BackendDescriptor;
use crate::config::AgentConfig;

const INDEX_VERSION: u32 = 1;
const MAX_TERMS_PER_FILE: usize = 40;
const MAX_IMPORTS_PER_FILE: usize = 20;
const MAX_SYMBOLS_PER_FILE: usize = 80;
const MAX_SNIPPET_CHARS: usize = 360;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectIndex {
    pub version: u32,
    pub generated_at: String,
    pub workspace_root: String,
    pub files: Vec<IndexedFile>,
    pub skipped: IndexSkipStats,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct IndexSkipStats {
    pub ignored: usize,
    pub oversized: usize,
    pub binary: usize,
    pub outside_workspace: usize,
    pub read_errors: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexedFile {
    pub path: String,
    pub language: String,
    pub byte_size: u64,
    pub modified_secs: u64,
    pub sha256: String,
    pub symbols: Vec<IndexedSymbol>,
    pub headings: Vec<IndexedSymbol>,
    pub imports: Vec<String>,
    pub terms: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IndexedSymbol {
    pub kind: String,
    pub name: String,
    pub line: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectNote {
    pub id: String,
    pub timestamp: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoMap {
    pub content: String,
    pub bytes: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoSearchHit {
    pub path: String,
    pub language: String,
    pub score: i32,
    pub reasons: Vec<String>,
    pub symbols: Vec<IndexedSymbol>,
    pub headings: Vec<IndexedSymbol>,
    pub imports: Vec<String>,
    pub snippet: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProjectMemoryStatus {
    pub path: PathBuf,
    pub exists: bool,
    pub files: usize,
    pub bytes: u64,
    pub generated_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectIndexFreshness {
    pub indexed_files: usize,
    pub workspace_files: usize,
    pub fresh: usize,
    pub stale: usize,
    pub missing: usize,
    pub deleted: usize,
    pub read_errors: usize,
}

impl ProjectIndexFreshness {
    pub fn is_fresh(&self) -> bool {
        self.stale == 0 && self.missing == 0 && self.deleted == 0 && self.read_errors == 0
    }
}

pub fn memory_dir(session_dir: &str) -> PathBuf {
    Path::new(session_dir).join("project-memory")
}

pub fn index_path(session_dir: &str) -> PathBuf {
    memory_dir(session_dir).join("index.json")
}

pub fn notes_path(session_dir: &str) -> PathBuf {
    memory_dir(session_dir).join("notes.jsonl")
}

pub fn load_project_index(config: &AgentConfig) -> Result<Option<ProjectIndex>> {
    let path = index_path(&config.session_dir);
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)?;
    let index = serde_json::from_str::<ProjectIndex>(&text)?;
    Ok(Some(index))
}

pub fn project_memory_status(config: &AgentConfig) -> Result<ProjectMemoryStatus> {
    let path = index_path(&config.session_dir);
    if !path.exists() {
        return Ok(ProjectMemoryStatus {
            path,
            exists: false,
            files: 0,
            bytes: 0,
            generated_at: None,
        });
    }
    let bytes = fs::metadata(&path)?.len();
    let generated_at = load_project_index(config)?.map(|idx| idx.generated_at);
    let files = load_project_index(config)?
        .map(|idx| idx.files.len())
        .unwrap_or(0);
    Ok(ProjectMemoryStatus {
        path,
        exists: true,
        files,
        bytes,
        generated_at,
    })
}

pub fn project_index_freshness(config: &AgentConfig) -> Result<ProjectIndexFreshness> {
    let Some(index) = load_project_index(config)? else {
        return Ok(ProjectIndexFreshness::default());
    };
    let root = normalize_path(Path::new(&config.workspace_root));
    let mut indexed_by_path: HashMap<String, IndexedFile> = index
        .files
        .iter()
        .cloned()
        .map(|file| (file.path.clone(), file))
        .collect();
    let mut report = ProjectIndexFreshness {
        indexed_files: indexed_by_path.len(),
        ..Default::default()
    };

    for item in indexable_workspace_files(config, &root) {
        let path = match item {
            Ok(path) => path,
            Err(_) => {
                report.read_errors += 1;
                continue;
            }
        };
        report.workspace_files += 1;
        let rel_path = relative_slash_path(&root, &path)?;
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                report.read_errors += 1;
                continue;
            }
        };
        let modified_secs = file_modified_secs(&metadata);
        let Some(indexed) = indexed_by_path.remove(&rel_path) else {
            report.missing += 1;
            continue;
        };
        if indexed.byte_size == metadata.len() && indexed.modified_secs == modified_secs {
            report.fresh += 1;
            continue;
        }
        match fs::read(&path) {
            Ok(bytes) if sha256_hex(&bytes) == indexed.sha256 => report.fresh += 1,
            Ok(_) => report.stale += 1,
            Err(_) => report.read_errors += 1,
        }
    }
    report.deleted = indexed_by_path.len();
    Ok(report)
}

pub fn refresh_changed_project_index(config: &AgentConfig) -> Result<ProjectIndex> {
    build_project_index(config)
}

pub fn refresh_project_memory_after_write(config: &AgentConfig) -> Result<Option<ProjectIndex>> {
    if !config.project_memory.enabled || !index_path(&config.session_dir).exists() {
        return Ok(None);
    }
    refresh_changed_project_index(config).map(Some)
}

pub fn clear_project_index(config: &AgentConfig) -> Result<bool> {
    let path = index_path(&config.session_dir);
    if path.exists() {
        fs::remove_file(path)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub fn build_project_index(config: &AgentConfig) -> Result<ProjectIndex> {
    if !config.project_memory.enabled {
        return Err(anyhow!("project memory is disabled"));
    }
    let root = normalize_path(Path::new(&config.workspace_root));
    let previous = load_project_index(config)?
        .filter(|idx| normalize_path(Path::new(&idx.workspace_root)) == root);
    let previous_by_path: HashMap<String, IndexedFile> = previous
        .map(|idx| {
            idx.files
                .into_iter()
                .map(|file| (file.path.clone(), file))
                .collect()
        })
        .unwrap_or_default();

    let mut builder = WalkBuilder::new(&root);
    builder
        .standard_filters(true)
        .hidden(false)
        .require_git(false);
    let root_for_filter = root.clone();
    builder.filter_entry(move |entry| !has_skipped_component(&root_for_filter, entry.path()));
    let walker = builder.build();

    let mut files = Vec::new();
    let mut skipped = IndexSkipStats::default();
    for item in walker {
        let entry = match item {
            Ok(entry) => entry,
            Err(_) => {
                skipped.read_errors += 1;
                continue;
            }
        };
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let path = normalize_path(entry.path());
        if !path.starts_with(&root) {
            skipped.outside_workspace += 1;
            continue;
        }
        if is_secret_file(&path) {
            skipped.ignored += 1;
            continue;
        }
        let rel_path = relative_slash_path(&root, &path)?;
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                skipped.read_errors += 1;
                continue;
            }
        };
        if metadata.len() as usize > config.project_memory.max_file_bytes {
            skipped.oversized += 1;
            continue;
        }
        let modified_secs = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        if let Some(previous) = previous_by_path.get(&rel_path) {
            if previous.byte_size == metadata.len() && previous.modified_secs == modified_secs {
                files.push(previous.clone());
                continue;
            }
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(_) => {
                skipped.read_errors += 1;
                continue;
            }
        };
        if looks_binary(&bytes) {
            skipped.binary += 1;
            continue;
        }
        let text = match String::from_utf8(bytes.clone()) {
            Ok(text) => text,
            Err(_) => {
                skipped.binary += 1;
                continue;
            }
        };
        let sha256 = sha256_hex(&bytes);
        files.push(index_text_file(
            rel_path,
            metadata.len(),
            modified_secs,
            sha256,
            &text,
        ));
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let index = ProjectIndex {
        version: INDEX_VERSION,
        generated_at: Utc::now().to_rfc3339(),
        workspace_root: root.display().to_string(),
        files,
        skipped,
    };
    save_project_index(config, &index)?;
    Ok(index)
}

fn indexable_workspace_files(config: &AgentConfig, root: &Path) -> Vec<Result<PathBuf>> {
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(true)
        .hidden(false)
        .require_git(false);
    let root_for_filter = root.to_path_buf();
    builder.filter_entry(move |entry| !has_skipped_component(&root_for_filter, entry.path()));

    let mut files = Vec::new();
    for item in builder.build() {
        let entry = match item {
            Ok(entry) => entry,
            Err(e) => {
                files.push(Err(anyhow!(e.to_string())));
                continue;
            }
        };
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let path = normalize_path(entry.path());
        if !path.starts_with(root) || is_secret_file(&path) {
            continue;
        }
        let Ok(metadata) = fs::metadata(&path) else {
            files.push(Err(anyhow!(
                "failed to read metadata for {}",
                path.display()
            )));
            continue;
        };
        if metadata.len() as usize > config.project_memory.max_file_bytes {
            continue;
        }
        files.push(Ok(path));
    }
    files
}

fn file_modified_secs(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub fn save_project_index(config: &AgentConfig, index: &ProjectIndex) -> Result<()> {
    let dir = memory_dir(&config.session_dir);
    fs::create_dir_all(&dir)?;
    let body = serde_json::to_string_pretty(index)?;
    fs::write(index_path(&config.session_dir), format!("{body}\n"))?;
    Ok(())
}

pub fn load_project_notes(config: &AgentConfig) -> Result<Vec<ProjectNote>> {
    let path = notes_path(&config.session_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut notes = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(note) = serde_json::from_str::<ProjectNote>(&line) {
            notes.push(note);
        }
    }
    Ok(notes)
}

pub fn append_project_note(config: &AgentConfig, text: &str) -> Result<ProjectNote> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("note text cannot be empty"));
    }
    let dir = memory_dir(&config.session_dir);
    fs::create_dir_all(&dir)?;
    let now = Utc::now();
    let note = ProjectNote {
        id: now.format("%Y%m%d%H%M%S%3f").to_string(),
        timestamp: now.to_rfc3339(),
        text: trimmed.to_string(),
    };
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(notes_path(&config.session_dir))?;
    let line = serde_json::to_string(&note)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(note)
}

pub fn forget_project_note(config: &AgentConfig, id: &str) -> Result<usize> {
    let path = notes_path(&config.session_dir);
    if id == "all" {
        let removed = load_project_notes(config)?.len();
        if path.exists() {
            fs::remove_file(path)?;
        }
        return Ok(removed);
    }
    let notes = load_project_notes(config)?;
    let before = notes.len();
    let kept: Vec<ProjectNote> = notes.into_iter().filter(|note| note.id != id).collect();
    let removed = before.saturating_sub(kept.len());
    if removed == 0 {
        return Ok(0);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut body = String::new();
    for note in kept {
        body.push_str(&serde_json::to_string(&note)?);
        body.push('\n');
    }
    fs::write(path, body)?;
    Ok(removed)
}

pub fn search_index(index: &ProjectIndex, query: &str, limit: usize) -> Vec<RepoSearchHit> {
    let tokens = query_tokens(query);
    if tokens.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for file in &index.files {
        let mut score = 0i32;
        let mut reasons = Vec::new();
        let lower_path = file.path.to_lowercase();
        let name = Path::new(&file.path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&file.path)
            .to_lowercase();
        for token in &tokens {
            if lower_path.contains(token) {
                score += 30;
                push_reason(&mut reasons, format!("path matches `{token}`"));
            }
            if name.contains(token) {
                score += 20;
                push_reason(&mut reasons, format!("file name matches `{token}`"));
            }
            for symbol in file.symbols.iter().chain(file.headings.iter()) {
                if symbol.name.to_lowercase().contains(token) {
                    score += 24;
                    push_reason(&mut reasons, format!("symbol `{}` matches", symbol.name));
                }
            }
            for import in &file.imports {
                if import.to_lowercase().contains(token) {
                    score += 8;
                    push_reason(&mut reasons, format!("import matches `{token}`"));
                }
            }
            if file.terms.iter().any(|term| term == token) {
                score += 10;
                push_reason(&mut reasons, format!("keyword `{token}`"));
            }
        }
        if score > 0 {
            hits.push(RepoSearchHit {
                path: file.path.clone(),
                language: file.language.clone(),
                score,
                reasons,
                symbols: file.symbols.iter().take(12).cloned().collect(),
                headings: file.headings.iter().take(8).cloned().collect(),
                imports: file.imports.iter().take(8).cloned().collect(),
                snippet: None,
            });
        }
    }
    hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    hits.truncate(limit.max(1));
    hits
}

pub fn add_snippets(root: &Path, query: &str, hits: &mut [RepoSearchHit]) {
    let tokens = query_tokens(query);
    for hit in hits {
        let path = root.join(&hit.path);
        if let Ok(text) = fs::read_to_string(path) {
            hit.snippet = snippet_for_text(&text, &tokens);
        }
    }
}

pub fn render_repo_map(
    config: &AgentConfig,
    index: &ProjectIndex,
    notes: &[ProjectNote],
    query: Option<&str>,
) -> RepoMap {
    let max_bytes = config.project_memory.max_injected_bytes.max(64);
    let mut out = String::new();
    let mut truncated = false;
    push_capped(
        &mut out,
        &format!(
            "Project memory: {} indexed files under {}\n",
            index.files.len(),
            index.workspace_root
        ),
        max_bytes,
        &mut truncated,
    );
    if !notes.is_empty() {
        push_capped(&mut out, "Notes:\n", max_bytes, &mut truncated);
        for note in notes.iter().rev().take(8) {
            push_capped(
                &mut out,
                &format!("- [{}] {}\n", note.id, one_line(&note.text, 180)),
                max_bytes,
                &mut truncated,
            );
        }
    }
    if let Some(q) = query.filter(|q| !q.trim().is_empty()) {
        let hits = search_index(index, q, 12);
        push_capped(
            &mut out,
            &format!("Focused map for `{}`:\n", q.trim()),
            max_bytes,
            &mut truncated,
        );
        if hits.is_empty() {
            push_capped(
                &mut out,
                "No focused hits; general repo map:\n",
                max_bytes,
                &mut truncated,
            );
            for file in repo_map_files(index).into_iter().take(80) {
                let line = map_line_for_file(file);
                push_capped(&mut out, &line, max_bytes, &mut truncated);
                if truncated {
                    break;
                }
            }
        } else {
            for hit in hits {
                let line = map_line_for_hit(index, &hit);
                push_capped(&mut out, &line, max_bytes, &mut truncated);
                if truncated {
                    break;
                }
            }
        }
    } else {
        push_capped(&mut out, "Repo map:\n", max_bytes, &mut truncated);
        for file in repo_map_files(index).into_iter().take(80) {
            let line = map_line_for_file(file);
            push_capped(&mut out, &line, max_bytes, &mut truncated);
            if truncated {
                break;
            }
        }
    }
    RepoMap {
        bytes: out.len(),
        content: out,
        truncated,
    }
}

pub fn should_inject_project_context(
    config: &AgentConfig,
    backend: &BackendDescriptor,
    prompt: &str,
) -> bool {
    config.project_memory.enabled
        && config.project_memory.auto_inject
        && prompt_looks_repo_related(prompt)
        && (backend.is_local || config.project_memory.allow_cloud_context)
}

pub fn maybe_project_context(
    config: &AgentConfig,
    backend: &BackendDescriptor,
    prompt: &str,
) -> Option<String> {
    if !should_inject_project_context(config, backend, prompt) {
        return None;
    }
    let index = load_project_index(config).ok().flatten()?;
    if index.files.is_empty() {
        return None;
    }
    let notes = load_project_notes(config).unwrap_or_default();
    let map = render_repo_map(config, &index, &notes, Some(prompt));
    Some(map.content)
}

/// Header the prompt-focused repo map is injected under, whether it lives in
/// the system prompt (one-shot/eval callers) or is folded below the cache
/// boundary into the current user turn (the interactive loop).
pub const PROJECT_CONTEXT_HEADER: &str = "Local project memory context:";

fn append_project_prompt(out: &mut String, workspace_root: &str) {
    if let Some(project_prompt) = load_project_prompt(workspace_root) {
        out.push_str("\n\nProject-specific guidance (.albatross/prompt.md):\n");
        out.push_str(&project_prompt);
    }
}

/// System prompt that stays byte-identical across turns within a session:
/// base prompt plus the static `.albatross/prompt.md` guidance.
///
/// It deliberately omits the prompt-focused repo map ([`maybe_project_context`]),
/// which is re-ranked against every user message and so changes each turn.
/// Keeping that volatile block out of the position-0 system message lets the
/// cached prefix (system + tools + prior turns) survive across turns for both
/// hosted prompt caching and local KV/prefix reuse. The interactive loop folds
/// the map into the current user turn instead, below the cache boundary.
pub fn render_stable_system_prompt(config: &AgentConfig, tools: &[String]) -> String {
    let mut out = config.render_system_prompt_for_tools(tools);
    append_project_prompt(&mut out, &config.workspace_root);
    out
}

pub fn render_system_prompt_with_memory(
    config: &AgentConfig,
    backend: &BackendDescriptor,
    tools: &[String],
    prompt: &str,
) -> String {
    let mut out = config.render_system_prompt_for_tools(tools);
    if let Some(context) = maybe_project_context(config, backend, prompt) {
        out.push_str("\n\n");
        out.push_str(PROJECT_CONTEXT_HEADER);
        out.push('\n');
        out.push_str(&context);
    }
    append_project_prompt(&mut out, &config.workspace_root);
    out
}

/// Read `.albatross/prompt.md` from the workspace root, if present.
///
/// Truncated at PROJECT_PROMPT_MAX_BYTES so a stray multi-megabyte file
/// can't blow up every turn's prompt budget. Truncation is byte-accurate
/// but rolled back to a char boundary so we don't slice mid-UTF-8.
fn load_project_prompt(workspace_root: &str) -> Option<String> {
    let path = std::path::Path::new(workspace_root)
        .join(".albatross")
        .join("prompt.md");
    let raw = std::fs::read_to_string(&path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() <= PROJECT_PROMPT_MAX_BYTES {
        return Some(trimmed.to_string());
    }
    let mut cutoff = PROJECT_PROMPT_MAX_BYTES;
    while cutoff > 0 && !trimmed.is_char_boundary(cutoff) {
        cutoff -= 1;
    }
    let mut out = trimmed[..cutoff].to_string();
    out.push_str("\n\n[… project prompt truncated …]");
    Some(out)
}

const PROJECT_PROMPT_MAX_BYTES: usize = 8 * 1024;

pub fn prompt_looks_repo_related(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    [
        "repo",
        "repository",
        "code",
        "codebase",
        "file",
        "files",
        "src",
        "module",
        "function",
        "struct",
        "class",
        "config",
        "command",
        "tool",
        "test",
        "implement",
        "fix",
        "refactor",
        "where is",
        "search",
        "grep",
        "read",
        "edit",
        "build",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn index_text_file(
    path: String,
    byte_size: u64,
    modified_secs: u64,
    sha256: String,
    text: &str,
) -> IndexedFile {
    let language = detect_language(&path);
    let mut symbols = extract_symbols(&language, text);
    if symbols.is_empty() {
        symbols.push(IndexedSymbol {
            kind: "file".into(),
            name: file_stem_name(&path),
            line: 1,
        });
    }
    let headings: Vec<IndexedSymbol> = symbols
        .iter()
        .filter(|symbol| symbol.kind == "heading")
        .cloned()
        .collect();
    let symbols: Vec<IndexedSymbol> = symbols
        .into_iter()
        .filter(|symbol| symbol.kind != "heading")
        .take(MAX_SYMBOLS_PER_FILE)
        .collect();
    IndexedFile {
        path,
        language,
        byte_size,
        modified_secs,
        sha256,
        symbols,
        headings: headings.into_iter().take(MAX_SYMBOLS_PER_FILE).collect(),
        imports: extract_imports(text)
            .into_iter()
            .take(MAX_IMPORTS_PER_FILE)
            .collect(),
        terms: extract_terms(text),
    }
}

fn detect_language(path: &str) -> String {
    let lower = path.to_lowercase();
    let ext = Path::new(&lower)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "rs" => "rust",
        "py" => "python",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "md" | "markdown" => "markdown",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "html" | "css" | "scss" => ext,
        _ => "text",
    }
    .to_string()
}

fn extract_symbols(language: &str, text: &str) -> Vec<IndexedSymbol> {
    match language {
        "rust" => extract_with_regex(
            text,
            rust_symbol_re(),
            &[
                ("fn", "function"),
                ("struct", "struct"),
                ("enum", "enum"),
                ("trait", "trait"),
                ("mod", "module"),
                ("impl", "impl"),
            ],
        ),
        "python" => extract_with_regex(
            text,
            python_symbol_re(),
            &[("def", "function"), ("class", "class")],
        ),
        "typescript" | "javascript" => extract_js_symbols(text),
        "markdown" => extract_markdown_headings(text),
        "json" => extract_json_keys(text),
        "toml" => extract_toml_keys(text),
        _ => Vec::new(),
    }
}

fn extract_with_regex(text: &str, re: &Regex, kinds: &[(&str, &str)]) -> Vec<IndexedSymbol> {
    let kind_map: HashMap<&str, &str> = kinds.iter().copied().collect();
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if let Some(caps) = re.captures(line) {
            let raw_kind = caps.name("kind").map(|m| m.as_str()).unwrap_or("symbol");
            let name = caps.name("name").map(|m| m.as_str()).unwrap_or("").trim();
            if !name.is_empty() {
                out.push(IndexedSymbol {
                    kind: kind_map.get(raw_kind).copied().unwrap_or(raw_kind).into(),
                    name: name.into(),
                    line: idx + 1,
                });
            }
        }
    }
    out
}

fn rust_symbol_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?P<kind>fn|struct|enum|trait|mod|impl)\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)").unwrap()
    })
}

fn python_symbol_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*(?P<kind>def|class)\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)").unwrap()
    })
}

fn extract_js_symbols(text: &str) -> Vec<IndexedSymbol> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^\s*(?:export\s+)?(?:(?:async\s+)?function\s+(?P<fn>[A-Za-z_$][A-Za-z0-9_$]*)|(?P<kind>class|interface|type|enum|const|let|var)\s+(?P<name>[A-Za-z_$][A-Za-z0-9_$]*))").unwrap()
    });
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if let Some(caps) = re.captures(line) {
            if let Some(name) = caps.name("fn") {
                out.push(IndexedSymbol {
                    kind: "function".into(),
                    name: name.as_str().into(),
                    line: idx + 1,
                });
            } else if let (Some(kind), Some(name)) = (caps.name("kind"), caps.name("name")) {
                out.push(IndexedSymbol {
                    kind: kind.as_str().into(),
                    name: name.as_str().into(),
                    line: idx + 1,
                });
            }
        }
    }
    out
}

fn extract_markdown_headings(text: &str) -> Vec<IndexedSymbol> {
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && trimmed.chars().nth(hashes) == Some(' ') {
            out.push(IndexedSymbol {
                kind: "heading".into(),
                name: trimmed[hashes..].trim().into(),
                line: idx + 1,
            });
        }
    }
    out
}

fn extract_json_keys(text: &str) -> Vec<IndexedSymbol> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let Some(obj) = value.as_object() else {
        return Vec::new();
    };
    obj.keys()
        .take(MAX_SYMBOLS_PER_FILE)
        .map(|key| IndexedSymbol {
            kind: "config-key".into(),
            name: key.clone(),
            line: 1,
        })
        .collect()
}

fn extract_toml_keys(text: &str) -> Vec<IndexedSymbol> {
    static KEY_RE: OnceLock<Regex> = OnceLock::new();
    static TABLE_RE: OnceLock<Regex> = OnceLock::new();
    let key_re = KEY_RE.get_or_init(|| Regex::new(r"^\s*([A-Za-z0-9_.-]+)\s*=").unwrap());
    let table_re = TABLE_RE.get_or_init(|| Regex::new(r"^\s*\[+([A-Za-z0-9_.-]+)\]+\s*$").unwrap());
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if let Some(caps) = table_re.captures(line) {
            out.push(IndexedSymbol {
                kind: "config-section".into(),
                name: caps[1].into(),
                line: idx + 1,
            });
        } else if let Some(caps) = key_re.captures(line) {
            out.push(IndexedSymbol {
                kind: "config-key".into(),
                name: caps[1].into(),
                line: idx + 1,
            });
        }
    }
    out
}

fn extract_imports(text: &str) -> Vec<String> {
    let mut imports = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let importish = trimmed.starts_with("use ")
            || trimmed.starts_with("mod ")
            || trimmed.starts_with("import ")
            || trimmed.starts_with("from ")
            || trimmed.starts_with("require(")
            || trimmed.starts_with("const ") && trimmed.contains("require(");
        if importish {
            imports.push(one_line(trimmed, 160));
        }
    }
    imports
}

fn extract_terms(text: &str) -> Vec<String> {
    static TOKEN_RE: OnceLock<Regex> = OnceLock::new();
    let re = TOKEN_RE.get_or_init(|| Regex::new(r"[A-Za-z][A-Za-z0-9_]{2,}").unwrap());
    let stop = stop_words();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for mat in re.find_iter(text) {
        let token = mat.as_str().to_lowercase();
        if stop.contains(token.as_str()) {
            continue;
        }
        *counts.entry(token).or_insert(0) += 1;
    }
    let mut terms: Vec<(String, usize)> = counts.into_iter().collect();
    terms.sort_by_key(|(term, count)| (Reverse(*count), term.clone()));
    terms
        .into_iter()
        .take(MAX_TERMS_PER_FILE)
        .map(|(term, _)| term)
        .collect()
}

fn stop_words() -> &'static HashSet<&'static str> {
    static STOP: OnceLock<HashSet<&'static str>> = OnceLock::new();
    STOP.get_or_init(|| {
        [
            "the", "and", "for", "that", "with", "this", "from", "into", "pub", "use", "let",
            "mut", "self", "true", "false", "none", "some", "return", "async", "await", "impl",
            "where", "when", "then", "else", "type", "const", "static", "struct", "class",
            "function", "export", "import", "default", "crate", "super", "mod",
        ]
        .into_iter()
        .collect()
    })
}

fn query_tokens(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '.')
        .map(|token| token.trim().to_lowercase())
        .filter(|token| token.len() >= 2)
        .take(12)
        .collect()
}

fn repo_map_files(index: &ProjectIndex) -> Vec<&IndexedFile> {
    let mut files: Vec<&IndexedFile> = index.files.iter().collect();
    files.sort_by_key(|file| {
        let priority = if file.path == "Cargo.toml"
            || file.path == "package.json"
            || file.path.ends_with("/main.rs")
            || file.path.ends_with("/lib.rs")
            || file.path.ends_with("/mod.rs")
            || file.path.ends_with("README.md")
        {
            0
        } else if !file.symbols.is_empty() || !file.headings.is_empty() {
            1
        } else {
            2
        };
        (priority, file.path.clone())
    });
    files
}

fn map_line_for_hit(index: &ProjectIndex, hit: &RepoSearchHit) -> String {
    let file = index.files.iter().find(|file| file.path == hit.path);
    if let Some(file) = file {
        map_line_for_file(file)
    } else {
        format!("- {} ({}) score={}\n", hit.path, hit.language, hit.score)
    }
}

fn map_line_for_file(file: &IndexedFile) -> String {
    let mut parts = Vec::new();
    if !file.symbols.is_empty() {
        parts.push(format!(
            "symbols: {}",
            file.symbols
                .iter()
                .take(8)
                .map(|symbol| symbol.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !file.headings.is_empty() {
        parts.push(format!(
            "headings: {}",
            file.headings
                .iter()
                .take(5)
                .map(|heading| heading.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if parts.is_empty() && !file.terms.is_empty() {
        parts.push(format!(
            "terms: {}",
            file.terms
                .iter()
                .take(6)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if parts.is_empty() {
        format!("- {} ({})\n", file.path, file.language)
    } else {
        format!(
            "- {} ({}) - {}\n",
            file.path,
            file.language,
            parts.join("; ")
        )
    }
}

fn snippet_for_text(text: &str, tokens: &[String]) -> Option<String> {
    // Match per line on that line's own lowercase copy. Never apply offsets from
    // a lowercased whole-file string onto the original: to_lowercase can change
    // byte lengths (e.g. Turkish İ), which used to panic on out-of-bounds slices.
    let mut chosen = text.lines().next().unwrap_or("");
    for line in text.lines() {
        let lower = line.to_lowercase();
        if tokens.iter().any(|token| lower.contains(token)) {
            chosen = line;
            break;
        }
    }
    let snippet = one_line(chosen, MAX_SNIPPET_CHARS);
    if snippet.trim().is_empty() {
        None
    } else {
        Some(snippet)
    }
}

fn push_reason(reasons: &mut Vec<String>, reason: String) {
    if reasons.len() < 5 && !reasons.contains(&reason) {
        reasons.push(reason);
    }
}

fn push_capped(out: &mut String, line: &str, max_bytes: usize, truncated: &mut bool) {
    if *truncated {
        return;
    }
    if out.len() + line.len() <= max_bytes {
        out.push_str(line);
        return;
    }
    const MARKER: &str = "\n[truncated]\n";
    let remaining = max_bytes.saturating_sub(out.len());
    if remaining > MARKER.len() {
        let keep = remaining.saturating_sub(MARKER.len());
        out.push_str(&line.chars().take(keep).collect::<String>());
        out.push_str(MARKER);
    } else if remaining > 0 {
        out.push_str(&MARKER.chars().take(remaining).collect::<String>());
    }
    *truncated = true;
}

fn one_line(value: &str, max_chars: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        compact
    } else {
        let mut out = compact
            .chars()
            .take(max_chars.saturating_sub(12))
            .collect::<String>();
        out.push_str("[truncated]");
        out
    }
}

fn has_skipped_component(root: &Path, path: &Path) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components().any(|component| match component {
        Component::Normal(name) => matches!(
            name.to_str(),
            Some(".git" | ".sessions" | "target" | "node_modules")
        ),
        _ => false,
    })
}

fn is_secret_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let lower = name.to_lowercase();
    lower == ".env"
        || lower.starts_with(".env.")
        || lower == ".npmrc"
        || lower == ".pypirc"
        || lower == "credentials"
        || lower == "credentials.json"
        || lower == "secrets.json"
        || lower == "secret.json"
        || lower == "id_rsa"
        || lower == "id_ed25519"
        || lower.ends_with(".pem")
        || lower.ends_with(".key")
        || lower.ends_with(".p12")
        || lower.ends_with(".pfx")
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|byte| *byte == 0)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn file_stem_name(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(path)
        .to_string()
}

fn relative_slash_path(root: &Path, path: &Path) -> Result<String> {
    let rel = path.strip_prefix(root)?;
    Ok(rel
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => part.to_str().map(ToString::to_string),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/"))
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
    use crate::backends::{BackendDescriptor, BackendName};
    use crate::config::ProjectMemoryConfig;

    fn config_for(root: &Path) -> AgentConfig {
        AgentConfig {
            workspace_root: root.display().to_string(),
            session_dir: root.join(".sessions").display().to_string(),
            ..Default::default()
        }
    }

    fn local_backend() -> BackendDescriptor {
        BackendDescriptor {
            name: BackendName::Ollama,
            base_url: String::new(),
            api_key: String::new(),
            is_local: true,
            openrouter: crate::backends::OpenRouterConfig::default(),
        }
    }

    fn cloud_backend() -> BackendDescriptor {
        BackendDescriptor {
            name: BackendName::Openrouter,
            base_url: String::new(),
            api_key: String::new(),
            is_local: false,
            openrouter: crate::backends::OpenRouterConfig::default(),
        }
    }

    #[test]
    fn indexes_respect_ignore_large_binary_secret_and_workspace() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(dir.path().join("main.rs"), "pub fn run() {}\n").unwrap();
        fs::write(dir.path().join("ignored.txt"), "ignore me").unwrap();
        fs::write(dir.path().join(".env"), "TOKEN=secret").unwrap();
        fs::write(dir.path().join("bin.dat"), b"a\0b").unwrap();
        fs::write(dir.path().join("large.txt"), "x".repeat(32)).unwrap();
        let mut config = config_for(dir.path());
        config.project_memory.max_file_bytes = 16;

        let index = build_project_index(&config).unwrap();

        assert!(index.files.iter().any(|file| file.path == "main.rs"));
        assert!(!index.files.iter().any(|file| file.path == "ignored.txt"));
        assert!(!index.files.iter().any(|file| file.path == ".env"));
        assert!(!index.files.iter().any(|file| file.path == "bin.dat"));
        assert!(!index.files.iter().any(|file| file.path == "large.txt"));
        assert!(index.skipped.binary >= 1);
        assert!(index.skipped.oversized >= 1);
    }

    #[test]
    fn extracts_symbols_for_supported_languages() {
        let rust = extract_symbols("rust", "pub struct App {}\nfn run() {}\n");
        assert!(rust.iter().any(|s| s.name == "App" && s.kind == "struct"));
        assert!(rust.iter().any(|s| s.name == "run" && s.kind == "function"));

        let py = extract_symbols("python", "class Thing:\n  pass\ndef go():\n  pass\n");
        assert!(py.iter().any(|s| s.name == "Thing" && s.kind == "class"));
        assert!(py.iter().any(|s| s.name == "go" && s.kind == "function"));

        let js = extract_symbols("typescript", "export interface Props {}\nconst value = 1\n");
        assert!(js
            .iter()
            .any(|s| s.name == "Props" && s.kind == "interface"));
        assert!(js.iter().any(|s| s.name == "value" && s.kind == "const"));

        let md = extract_symbols("markdown", "# Title\n## Next\n");
        assert!(md.iter().any(|s| s.name == "Title" && s.kind == "heading"));

        let json = extract_symbols("json", r#"{"scripts":{},"dependencies":{}}"#);
        assert!(json.iter().any(|s| s.name == "scripts"));

        let toml = extract_symbols("toml", "[package]\nname = \"x\"\n");
        assert!(toml.iter().any(|s| s.name == "package"));
        assert!(toml.iter().any(|s| s.name == "name"));
    }

    #[test]
    fn incremental_index_reuses_unchanged_files_and_drops_deleted_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        fs::write(&path, "pub fn first() {}\n").unwrap();
        let config = config_for(dir.path());

        let first = build_project_index(&config).unwrap();
        let old_hash = first.files[0].sha256.clone();
        let second = build_project_index(&config).unwrap();
        assert_eq!(second.files[0].sha256, old_hash);

        fs::remove_file(&path).unwrap();
        let third = build_project_index(&config).unwrap();
        assert!(third.files.is_empty());
    }

    #[test]
    fn search_ranks_symbol_and_path_matches() {
        let index = ProjectIndex {
            version: INDEX_VERSION,
            generated_at: "now".into(),
            workspace_root: "/tmp/x".into(),
            skipped: IndexSkipStats::default(),
            files: vec![
                IndexedFile {
                    path: "src/commands.rs".into(),
                    language: "rust".into(),
                    byte_size: 1,
                    modified_secs: 1,
                    sha256: "a".into(),
                    symbols: vec![IndexedSymbol {
                        kind: "function".into(),
                        name: "dispatch".into(),
                        line: 1,
                    }],
                    headings: Vec::new(),
                    imports: Vec::new(),
                    terms: vec!["misc".into()],
                },
                IndexedFile {
                    path: "src/other.rs".into(),
                    language: "rust".into(),
                    byte_size: 1,
                    modified_secs: 1,
                    sha256: "b".into(),
                    symbols: Vec::new(),
                    headings: Vec::new(),
                    imports: Vec::new(),
                    terms: vec!["dispatch".into()],
                },
            ],
        };
        let hits = search_index(&index, "dispatch", 2);
        assert_eq!(hits[0].path, "src/commands.rs");
    }

    #[test]
    fn snippet_for_text_survives_turkish_i_length_change() {
        // U+0130 LATIN CAPITAL LETTER I WITH DOT ABOVE expands under to_lowercase
        // (2 bytes → 3). Offsets from the lowercased file used to panic.
        let text = format!("{}\nneedle_token appears here", "İ".repeat(200));
        let tokens = vec!["needle_token".to_string()];
        let snippet = snippet_for_text(&text, &tokens).expect("snippet");
        assert!(snippet.contains("needle_token"));
        assert!(!snippet.contains('İ'));
    }

    #[test]
    fn snippet_for_text_returns_first_line_when_no_token_match() {
        let snippet = snippet_for_text("alpha\nbeta", &["zzz".into()]).unwrap();
        assert_eq!(snippet, "alpha");
    }

    #[test]
    fn repo_map_obeys_byte_cap() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_for(dir.path());
        config.project_memory.max_injected_bytes = 128;
        let mut files = Vec::new();
        for i in 0..20 {
            files.push(index_text_file(
                format!("src/file{i}.rs"),
                1,
                1,
                "x".into(),
                "pub fn example_symbol_name() {}\n",
            ));
        }
        let index = ProjectIndex {
            version: INDEX_VERSION,
            generated_at: "now".into(),
            workspace_root: dir.path().display().to_string(),
            files,
            skipped: IndexSkipStats::default(),
        };
        let map = render_repo_map(&config, &index, &[], None);
        assert!(map.bytes <= 160);
        assert!(map.truncated);
    }

    #[test]
    fn cloud_auto_injection_requires_explicit_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_for(dir.path());
        assert!(should_inject_project_context(
            &config,
            &local_backend(),
            "map this repo"
        ));
        assert!(!should_inject_project_context(
            &config,
            &cloud_backend(),
            "map this repo"
        ));
        let config = AgentConfig {
            project_memory: ProjectMemoryConfig {
                allow_cloud_context: true,
                ..Default::default()
            },
            ..config
        };
        assert!(should_inject_project_context(
            &config,
            &cloud_backend(),
            "map this repo"
        ));
    }

    #[test]
    fn stable_system_prompt_excludes_prompt_focused_map() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("parser.rs"), "pub fn parse_tokens() {}\n").unwrap();
        fs::write(dir.path().join("render.rs"), "pub fn render_frame() {}\n").unwrap();
        let config = config_for(dir.path());
        build_project_index(&config).unwrap();
        let backend = local_backend();
        let tools = vec!["file_read".to_string()];

        // The focused map exists and is a function of the current user prompt.
        let ctx =
            maybe_project_context(&config, &backend, "how does parse_tokens read code").unwrap();
        assert!(ctx.contains("Focused map for"));

        // One-shot callers still embed the map, so their prompt varies per turn
        // — exactly what makes it unfit for the cache prefix.
        let full_a = render_system_prompt_with_memory(
            &config,
            &backend,
            &tools,
            "how does parse_tokens read code",
        );
        let full_b = render_system_prompt_with_memory(
            &config,
            &backend,
            &tools,
            "explain render_frame in this code",
        );
        assert!(full_a.contains(PROJECT_CONTEXT_HEADER));
        assert_ne!(
            full_a, full_b,
            "system prompt with the map embedded changes with the user prompt"
        );

        // The stable prefix carries neither the header nor any focused map, and
        // is identical regardless of the turn's prompt.
        let stable = render_stable_system_prompt(&config, &tools);
        assert!(!stable.contains(PROJECT_CONTEXT_HEADER));
        assert!(!stable.contains("Focused map for"));
        assert_eq!(stable, render_stable_system_prompt(&config, &tools));
    }

    #[test]
    fn notes_append_and_forget() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_for(dir.path());
        let note = append_project_note(&config, "Entry point is src/main.rs").unwrap();
        assert_eq!(load_project_notes(&config).unwrap().len(), 1);
        assert_eq!(forget_project_note(&config, &note.id).unwrap(), 1);
        assert!(load_project_notes(&config).unwrap().is_empty());
    }

    #[test]
    fn project_prompt_is_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(super::load_project_prompt(dir.path().to_str().unwrap()).is_none());
    }

    #[test]
    fn project_prompt_loads_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join(".albatross");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("prompt.md"), "Always use snake_case.\n").unwrap();
        let loaded = super::load_project_prompt(dir.path().to_str().unwrap()).unwrap();
        assert!(loaded.contains("snake_case"));
        assert!(!loaded.ends_with('\n'));
    }

    #[test]
    fn project_prompt_truncates_when_over_limit() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join(".albatross");
        std::fs::create_dir_all(&nested).unwrap();
        let huge = "x".repeat(super::PROJECT_PROMPT_MAX_BYTES * 2);
        std::fs::write(nested.join("prompt.md"), &huge).unwrap();
        let loaded = super::load_project_prompt(dir.path().to_str().unwrap()).unwrap();
        assert!(loaded.contains("truncated"));
        assert!(loaded.len() <= super::PROJECT_PROMPT_MAX_BYTES + 64);
    }

    #[test]
    fn empty_project_prompt_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join(".albatross");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("prompt.md"), "   \n\n  ").unwrap();
        assert!(super::load_project_prompt(dir.path().to_str().unwrap()).is_none());
    }
}
