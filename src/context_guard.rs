use anyhow::{anyhow, Result};
use reqwest::Client;
use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::backends::BackendDescriptor;
use crate::budget::{
    estimate_tokens, format_bytes, headroom_bytes, measure_prompt_budget, usage_ratio, PromptBudget,
};
use crate::config::{AgentConfig, ContextConfig};
use crate::openai::{stream_chat, ChatMessage, ChatRequest, StreamOptions, ToolDef};

const SUMMARIZE_BUDGET_FRACTION: f64 = 0.5;
const TIER2_TOOL_MAX_CHARS: usize = 1500;
const SUMMARY_MARKER: &str = "\n\nConversation summary:\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactMethod {
    None,
    LlmSummary,
    DeterministicTrim,
}

#[derive(Debug, Clone)]
pub struct CompactResult {
    pub compacted: bool,
    pub before_messages: usize,
    pub after_messages: usize,
    pub before_ratio: f64,
    pub after_ratio: f64,
    pub method: CompactMethod,
    pub conversation_summary: Option<String>,
    /// Set when a configured compaction model could not be used as-is, so a
    /// fallback ran: an unavailable backend inherits the main model; a dedicated
    /// model that errors or returns empty is retried with the main model and, if
    /// that also fails, the deterministic trim. Surfaced in the compaction notice.
    /// The wording always matches the final outcome — never claims "summarized
    /// with {main}" when the transcript was only trimmed deterministically.
    pub warning: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ContextGuardConfig {
    pub effective_limit_bytes: usize,
    pub compact_threshold: f64,
    pub auto_compact: bool,
    pub keep_messages: usize,
    pub model_context_tokens: usize,
}

/// Which model summarizes the transcript during compaction. Compaction inherits
/// the main conversation model unless `modelSystem.compaction` selects another.
#[derive(Debug, Clone, Default)]
pub enum CompactionModel {
    /// Use the main conversation model (default, and when nothing is configured).
    #[default]
    Inherit,
    /// A configured, ready model dedicated to compaction.
    Use {
        backend: BackendDescriptor,
        model: String,
        label: String,
    },
    /// A model was configured but its backend is not ready (e.g. missing API
    /// key). Compaction inherits the main model and warns.
    Unavailable { label: String, reason: String },
}

/// Resolve `modelSystem.compaction` into a [`CompactionModel`]. Pure: it does no
/// I/O and never warns; the not-ready warning is surfaced when compaction runs.
pub fn classify_compaction_model(config: &AgentConfig) -> CompactionModel {
    let Some(model_ref) = config.model_system.compaction() else {
        return CompactionModel::Inherit;
    };
    let backend = config.backend_descriptor_for(model_ref.backend);
    match crate::backends::validate(&backend) {
        Ok(()) => CompactionModel::Use {
            backend,
            model: model_ref.model.clone(),
            label: model_ref.label(),
        },
        Err(e) => CompactionModel::Unavailable {
            label: model_ref.label(),
            reason: e.to_string(),
        },
    }
}

#[derive(Debug, Clone)]
pub struct ContextGuardParams {
    pub effective_limit_bytes: usize,
    pub compact_threshold: f64,
    pub auto_compact: bool,
    pub keep_messages: usize,
    pub summarize_budget_bytes: usize,
    pub conversation_summary: Option<String>,
    pub compaction: CompactionModel,
}

#[derive(Debug, Clone)]
pub struct CompactNotice {
    pub line: String,
    pub conversation_summary: Option<String>,
    pub transcript_rewritten: bool,
}

#[derive(Debug)]
pub struct CompactSessionContext<'a> {
    pub messages: &'a mut Vec<ChatMessage>,
    pub system_prompt: &'a str,
    pub tool_defs: &'a [ToolDef],
    pub config: &'a AgentConfig,
    pub model: &'a str,
    pub is_local: bool,
    pub http: &'a Client,
    pub backend: &'a BackendDescriptor,
    pub conversation_summary: Option<&'a str>,
}

struct CompactRequest<'a> {
    messages: &'a mut Vec<ChatMessage>,
    system_prompt: &'a str,
    tool_defs: &'a [ToolDef],
    keep_messages: usize,
    limit_bytes: usize,
    summarize_budget_bytes: usize,
    http: &'a Client,
    backend: &'a BackendDescriptor,
    model: &'a str,
    conversation_summary: Option<&'a str>,
    compaction: &'a CompactionModel,
}

pub fn default_model_context_tokens(model: &str, is_local: bool) -> usize {
    let lower = model.to_ascii_lowercase();
    if !is_local {
        return 128_000;
    }
    if lower.contains("qwen2.5-coder") || lower.contains("qwen2.5_coder") {
        return 32_768;
    }
    if lower.contains("32k") {
        return 32_768;
    }
    if lower.contains("16k") {
        return 16_384;
    }
    if lower.contains("8k") {
        return 8192;
    }
    8192
}

pub fn resolve_model_context_tokens(config: &ContextConfig, model: &str, is_local: bool) -> usize {
    config
        .model_context_tokens
        .unwrap_or_else(|| default_model_context_tokens(model, is_local))
}

pub fn resolve_auto_compact(config: &ContextConfig, is_local: bool) -> bool {
    config.auto_compact.unwrap_or(is_local)
}

pub fn effective_limit_bytes(config: &ContextConfig, model: &str, is_local: bool) -> usize {
    let model_tokens = resolve_model_context_tokens(config, model, is_local);
    let fill_ratio = 1.0 - config.reserve_ratio.clamp(0.05, 0.5);
    let from_model = (model_tokens as f64 * 4.0 * fill_ratio) as usize;
    match config.max_bytes {
        Some(max_bytes) => from_model.min(max_bytes),
        None => from_model,
    }
}

pub fn guard_config_from(config: &AgentConfig, model: &str, is_local: bool) -> ContextGuardConfig {
    ContextGuardConfig {
        effective_limit_bytes: effective_limit_bytes(&config.context, model, is_local),
        compact_threshold: config.context.compact_threshold,
        auto_compact: resolve_auto_compact(&config.context, is_local),
        keep_messages: config.context.max_messages.unwrap_or(12).clamp(4, 80),
        model_context_tokens: resolve_model_context_tokens(&config.context, model, is_local),
    }
}

pub fn guard_params_from(
    config: &AgentConfig,
    model: &str,
    is_local: bool,
    conversation_summary: Option<String>,
) -> ContextGuardParams {
    let guard = guard_config_from(config, model, is_local);
    ContextGuardParams {
        effective_limit_bytes: guard.effective_limit_bytes,
        compact_threshold: guard.compact_threshold,
        auto_compact: guard.auto_compact,
        keep_messages: guard.keep_messages,
        summarize_budget_bytes: (guard.effective_limit_bytes as f64 * SUMMARIZE_BUDGET_FRACTION)
            as usize,
        conversation_summary,
        compaction: classify_compaction_model(config),
    }
}

pub fn should_compact(budget: &PromptBudget, limit_bytes: usize, threshold: f64) -> bool {
    if limit_bytes == 0 {
        return false;
    }
    usage_ratio(budget, limit_bytes) >= threshold.clamp(0.5, 0.99)
}

pub fn format_usage_line(budget: &PromptBudget, limit_bytes: usize) -> String {
    format!(
        "{} ({:.0}% of {})",
        format_bytes(budget.effective_total_bytes),
        usage_ratio(budget, limit_bytes) * 100.0,
        format_bytes(limit_bytes)
    )
}

pub fn merge_system_prompt(base: &str, summary: Option<&str>) -> String {
    let base = strip_conversation_summary(base);
    match summary.map(str::trim).filter(|s| !s.is_empty()) {
        Some(summary) => format!("{base}{SUMMARY_MARKER}{summary}"),
        None => base,
    }
}

pub fn strip_conversation_summary(content: &str) -> String {
    content
        .split_once(SUMMARY_MARKER)
        .map(|(base, _)| base)
        .unwrap_or(content)
        .trim_end()
        .to_string()
}

pub fn extract_conversation_summary(content: &str) -> Option<String> {
    content
        .split_once(SUMMARY_MARKER)
        .map(|(_, summary)| summary.trim().to_string())
        .filter(|summary| !summary.is_empty())
}

pub fn validate_transcript(messages: &[ChatMessage]) -> Result<()> {
    let mut pending_tools = HashSet::new();
    for message in messages {
        match message {
            ChatMessage::System { .. } => {}
            ChatMessage::User { .. } => {
                if !pending_tools.is_empty() {
                    return Err(anyhow!(
                        "unfinished tool calls before user message: {pending_tools:?}"
                    ));
                }
            }
            ChatMessage::Assistant { tool_calls, .. } => {
                if !pending_tools.is_empty() {
                    return Err(anyhow!(
                        "unfinished tool calls before assistant message: {pending_tools:?}"
                    ));
                }
                for tool_call in tool_calls {
                    if !tool_call.id.is_empty() {
                        pending_tools.insert(tool_call.id.clone());
                    }
                }
            }
            ChatMessage::Tool { tool_call_id, .. } => {
                if !pending_tools.remove(tool_call_id) {
                    return Err(anyhow!("orphan tool result for id {tool_call_id}"));
                }
            }
        }
    }
    if !pending_tools.is_empty() {
        return Err(anyhow!("missing tool results for ids: {pending_tools:?}"));
    }
    Ok(())
}

pub fn persist_session_transcript(session_path: &Path, messages: &[ChatMessage]) -> Result<()> {
    for message in messages {
        crate::session::save_message(session_path, message)?;
    }
    Ok(())
}

pub fn rewrite_session_transcript(
    session_dir: &str,
    session_path: &mut PathBuf,
    messages: &[ChatMessage],
) -> Result<()> {
    *session_path = crate::session::new_session_path(session_dir);
    persist_session_transcript(session_path, messages)
}

fn non_system_messages(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    messages
        .iter()
        .filter(|m| !matches!(m, ChatMessage::System { .. }))
        .cloned()
        .collect()
}

fn split_into_turns(messages: &[ChatMessage]) -> Vec<Range<usize>> {
    let mut turns = Vec::new();
    let mut start = 0usize;
    for (idx, message) in messages.iter().enumerate() {
        if idx > start && matches!(message, ChatMessage::User { .. }) {
            turns.push(start..idx);
            start = idx;
        }
    }
    if start < messages.len() {
        turns.push(start..messages.len());
    }
    turns
}

fn split_into_context_spans(messages: &[ChatMessage]) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut idx = 0usize;
    while idx < messages.len() {
        let start = idx;
        match &messages[idx] {
            ChatMessage::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                idx += 1;
                let mut pending: HashSet<String> = tool_calls
                    .iter()
                    .filter_map(|tool_call| {
                        if tool_call.id.is_empty() {
                            None
                        } else {
                            Some(tool_call.id.clone())
                        }
                    })
                    .collect();
                while idx < messages.len() {
                    match &messages[idx] {
                        ChatMessage::Tool { tool_call_id, .. } => {
                            pending.remove(tool_call_id);
                            idx += 1;
                            if pending.is_empty() {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
            }
            _ => {
                idx += 1;
            }
        }
        spans.push(start..idx);
    }
    spans
}

fn partition_by_ranges(
    non_system: &[ChatMessage],
    ranges: &[Range<usize>],
    keep_messages: usize,
) -> Option<(Vec<ChatMessage>, Vec<ChatMessage>)> {
    if ranges.len() <= 1 {
        return None;
    }

    let mut recent_start = ranges.len();
    let mut kept_messages = 0usize;
    while recent_start > 0 {
        let range = &ranges[recent_start - 1];
        let range_len = range.end.saturating_sub(range.start);
        if kept_messages > 0 && kept_messages + range_len > keep_messages {
            break;
        }
        recent_start -= 1;
        kept_messages += range_len;
        if kept_messages >= keep_messages {
            break;
        }
    }

    if recent_start == 0 {
        return None;
    }

    while recent_start < ranges.len()
        && matches!(
            non_system.get(ranges[recent_start].start),
            Some(ChatMessage::Tool { .. })
        )
    {
        recent_start += 1;
    }

    let split_at = ranges
        .get(recent_start)
        .map(|range| range.start)
        .unwrap_or(non_system.len());
    let older = non_system[..split_at].to_vec();
    let recent = non_system[split_at..].to_vec();
    if older.is_empty() {
        return None;
    }
    validate_transcript(&recent).ok()?;
    Some((older, recent))
}

fn partition_by_spans(
    non_system: &[ChatMessage],
    keep_messages: usize,
) -> Option<(Vec<ChatMessage>, Vec<ChatMessage>)> {
    let spans = split_into_context_spans(non_system);
    partition_by_ranges(non_system, &spans, keep_messages)
}

fn partition_for_keep(
    non_system: &[ChatMessage],
    keep_messages: usize,
) -> Option<(Vec<ChatMessage>, Vec<ChatMessage>)> {
    let turns = split_into_turns(non_system);
    let Some((mut older, recent)) = partition_by_ranges(non_system, &turns, keep_messages) else {
        return partition_by_spans(non_system, keep_messages);
    };

    if recent.len() <= keep_messages {
        return Some((older, recent));
    }

    if let Some((older_within_recent, recent_suffix)) = partition_by_spans(&recent, keep_messages) {
        older.extend(older_within_recent);
        return Some((older, recent_suffix));
    }

    Some((older, recent))
}

fn transcript_json_bytes(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|m| serde_json::to_vec(m).map(|v| v.len()).unwrap_or(0))
        .sum()
}

fn shrink_tool_messages(messages: &mut [ChatMessage]) {
    for message in messages.iter_mut() {
        if let ChatMessage::Tool { content, .. } = message {
            if content.chars().count() > TIER2_TOOL_MAX_CHARS {
                let trimmed: String = content.chars().take(TIER2_TOOL_MAX_CHARS).collect();
                *content = format!("{trimmed}…[trimmed for context]");
            }
        }
    }
}

fn append_summary(existing: Option<&str>, addition: &str) -> String {
    match existing.map(str::trim).filter(|s| !s.is_empty()) {
        Some(existing) => format!("{existing}\n{addition}"),
        None => addition.to_string(),
    }
}

fn truncate_summary_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str("...");
    out
}

fn deterministic_summary(messages: &[ChatMessage]) -> String {
    let user_count = messages
        .iter()
        .filter(|message| matches!(message, ChatMessage::User { .. }))
        .count();
    let tool_count = messages
        .iter()
        .filter(|message| matches!(message, ChatMessage::Tool { .. }))
        .count();
    let last_user = messages.iter().rev().find_map(|message| match message {
        ChatMessage::User { content } => Some(truncate_summary_text(content.as_text().trim(), 240)),
        _ => None,
    });
    match last_user {
        Some(last_user) if !last_user.is_empty() => format!(
            "[Earlier conversation trimmed to fit context budget: {user_count} user message(s), {tool_count} tool result(s). Last user request before trim: {last_user}]"
        ),
        _ => format!(
            "[Earlier conversation trimmed to fit context budget: {user_count} user message(s), {tool_count} tool result(s).]"
        ),
    }
}

fn build_compacted_messages(
    system_prompt: &str,
    summary: Option<&str>,
    mut recent: Vec<ChatMessage>,
) -> Result<Vec<ChatMessage>> {
    shrink_tool_messages(&mut recent);
    validate_transcript(&recent)?;
    let mut out = vec![ChatMessage::System {
        content: merge_system_prompt(system_prompt, summary),
    }];
    out.extend(recent);
    Ok(out)
}

pub async fn summarize_transcript(
    http: &Client,
    backend: &BackendDescriptor,
    model: &str,
    existing_summary: Option<&str>,
    older: &[ChatMessage],
) -> Result<String> {
    let transcript = serde_json::to_string(&serde_json::json!({
        "existingSummary": existing_summary.unwrap_or(""),
        "newMessages": older,
    }))?;
    let messages = vec![
        ChatMessage::System {
            content: "Update this Albatross conversation summary for continuing context. Preserve durable goals, decisions, files touched, errors, and pending work. Fold the existing summary and new messages into one concise summary without duplicating details.".into(),
        },
        ChatMessage::User {
            content: transcript.into(),
        },
    ];
    let req = ChatRequest {
        model,
        messages: &messages,
        tools: None,
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: false,
        }),
        max_tokens: None,
        prompt_cache_key: None,
        effort: None,
    };
    let mut out = String::new();
    stream_chat(http, backend, &req, None, |chunk| {
        if let Some(choice) = chunk.choices.first() {
            if let Some(content) = &choice.delta.content {
                out.push_str(content);
            }
        }
    })
    .await?;
    Ok(out)
}

fn compact_notice(
    before_messages: usize,
    after_messages: usize,
    before_ratio: f64,
    after_ratio: f64,
    method: CompactMethod,
) -> String {
    let method_label = match method {
        CompactMethod::LlmSummary => "summarized",
        CompactMethod::DeterministicTrim => "trimmed",
        CompactMethod::None => "compacted",
    };
    format!(
        "Compacted {before_messages} messages → {after_messages} ({method_label}), budget {:.0}% → {:.0}%",
        before_ratio * 100.0,
        after_ratio * 100.0
    )
}

async fn compact_messages_core(req: CompactRequest<'_>) -> Result<CompactResult> {
    let CompactRequest {
        messages,
        system_prompt,
        tool_defs,
        keep_messages,
        limit_bytes,
        summarize_budget_bytes,
        http,
        backend,
        model,
        conversation_summary,
        compaction,
    } = req;

    // Compaction summarizes with a dedicated model when one is configured and
    // ready; otherwise it inherits the main model. Prompt-budget sizing above is
    // always based on the main model, since that is the context we are fitting.
    let (summary_backend, summary_model): (&BackendDescriptor, &str) = match compaction {
        CompactionModel::Use { backend, model, .. } => (backend, model.as_str()),
        CompactionModel::Inherit | CompactionModel::Unavailable { .. } => (backend, model),
    };

    let active_system_prompt = merge_system_prompt(system_prompt, conversation_summary);
    let budget_before = measure_prompt_budget(&active_system_prompt, messages, tool_defs);
    let before_messages = messages.len();
    let before_ratio = usage_ratio(&budget_before, limit_bytes);

    if before_messages <= keep_messages + 1 {
        return Ok(CompactResult {
            compacted: false,
            before_messages,
            after_messages: before_messages,
            before_ratio,
            after_ratio: before_ratio,
            method: CompactMethod::None,
            conversation_summary: conversation_summary.map(str::to_string),
            warning: None,
        });
    }

    let non_system = non_system_messages(messages);
    let Some((older, recent)) = partition_for_keep(&non_system, keep_messages) else {
        return Ok(CompactResult {
            compacted: false,
            before_messages,
            after_messages: before_messages,
            before_ratio,
            after_ratio: before_ratio,
            method: CompactMethod::None,
            conversation_summary: conversation_summary.map(str::to_string),
            warning: None,
        });
    };

    let use_tier2 = transcript_json_bytes(&older) > summarize_budget_bytes;
    // Only warn about an unavailable / failed compaction model when we would
    // actually have called it (the LLM-summary path); the deterministic trim
    // uses no model. Warnings are set only after the final outcome is known so
    // a failed main-model retry never claims "summarized with {main}".
    let mut warning = None;
    let (method, new_summary) = if use_tier2 {
        let addition = deterministic_summary(&older);
        (
            CompactMethod::DeterministicTrim,
            append_summary(conversation_summary, &addition),
        )
    } else {
        let unavailable = match compaction {
            CompactionModel::Unavailable { label, reason } => {
                Some((label.as_str(), reason.as_str()))
            }
            _ => None,
        };

        let mut result = summarize_transcript(
            http,
            summary_backend,
            summary_model,
            conversation_summary,
            &older,
        )
        .await;

        // A dedicated compaction model that errors or returns empty retries with
        // the main conversation model before falling back to the deterministic
        // trim, so a broken compaction backend never silently loses the summary.
        // Capture the failure detail here; only claim "summarized with main"
        // after the retry actually produces a summary.
        let mut dedicated_fail: Option<(&str, String)> = None;
        if let CompactionModel::Use { label, .. } = compaction {
            if !matches!(&result, Ok(summary) if !summary.trim().is_empty()) {
                let detail = match &result {
                    Err(e) => e.to_string(),
                    _ => "returned an empty summary".to_string(),
                };
                dedicated_fail = Some((label.as_str(), detail));
                result =
                    summarize_transcript(http, backend, model, conversation_summary, &older).await;
            }
        }

        match result {
            Ok(summary) if !summary.trim().is_empty() => {
                if let Some((label, detail)) = dedicated_fail {
                    warning = Some(format!(
                        "compaction model {label} failed ({detail}); summarized with {model} instead"
                    ));
                } else if let Some((label, reason)) = unavailable {
                    warning = Some(format!(
                        "compaction model {label} unavailable ({reason}); summarized with {model}"
                    ));
                }
                (CompactMethod::LlmSummary, summary.trim().to_string())
            }
            _ => {
                // The summary model — and the main-model retry, if one ran —
                // failed, so fall back to the deterministic trim. Never leave a
                // "summarized with {main}" notice on this path: the transcript
                // was cut, not summarized.
                if let Some((label, detail)) = dedicated_fail {
                    warning = Some(format!(
                        "compaction model {label} failed ({detail}); main model {model} also failed; trimmed deterministically"
                    ));
                } else if let Some((label, reason)) = unavailable {
                    warning = Some(format!(
                        "compaction model {label} unavailable ({reason}); main model {model} also failed; trimmed deterministically"
                    ));
                }
                let addition = deterministic_summary(&older);
                (
                    CompactMethod::DeterministicTrim,
                    append_summary(conversation_summary, &addition),
                )
            }
        }
    };

    let compacted_messages = build_compacted_messages(system_prompt, Some(&new_summary), recent)?;
    *messages = compacted_messages;

    let active_system_prompt = merge_system_prompt(system_prompt, Some(&new_summary));
    let budget_after = measure_prompt_budget(&active_system_prompt, messages, tool_defs);
    let after_ratio = usage_ratio(&budget_after, limit_bytes);

    Ok(CompactResult {
        compacted: true,
        before_messages,
        after_messages: messages.len(),
        before_ratio,
        after_ratio,
        method,
        conversation_summary: Some(new_summary),
        warning,
    })
}

pub async fn compact_messages(
    ctx: &mut CompactSessionContext<'_>,
    keep: Option<usize>,
) -> Result<CompactResult> {
    let guard = guard_config_from(ctx.config, ctx.model, ctx.is_local);
    let keep = keep.unwrap_or(guard.keep_messages);
    let summarize_budget =
        (guard.effective_limit_bytes as f64 * SUMMARIZE_BUDGET_FRACTION) as usize;
    let compaction = classify_compaction_model(ctx.config);

    compact_messages_core(CompactRequest {
        messages: ctx.messages,
        system_prompt: ctx.system_prompt,
        tool_defs: ctx.tool_defs,
        keep_messages: keep,
        limit_bytes: guard.effective_limit_bytes,
        summarize_budget_bytes: summarize_budget,
        http: ctx.http,
        backend: ctx.backend,
        model: ctx.model,
        conversation_summary: ctx.conversation_summary,
        compaction: &compaction,
    })
    .await
}

pub async fn compact_session(
    ctx: &mut CompactSessionContext<'_>,
    session_dir: &str,
    session_path: &mut PathBuf,
    keep: Option<usize>,
) -> Result<CompactResult> {
    let result = compact_messages(ctx, keep).await?;

    if result.compacted {
        rewrite_session_transcript(session_dir, session_path, ctx.messages)?;
    }

    Ok(result)
}

pub async fn maybe_auto_compact(
    ctx: &mut CompactSessionContext<'_>,
    session_dir: &str,
    session_path: &mut PathBuf,
    request_only_context_bytes: usize,
) -> Result<Option<CompactNotice>> {
    let guard = guard_config_from(ctx.config, ctx.model, ctx.is_local);
    let active_system_prompt = merge_system_prompt(ctx.system_prompt, ctx.conversation_summary);
    let budget = add_request_only_context_bytes(
        measure_prompt_budget(&active_system_prompt, ctx.messages, ctx.tool_defs),
        request_only_context_bytes,
    );

    if !guard.auto_compact {
        if should_compact(
            &budget,
            guard.effective_limit_bytes,
            guard.compact_threshold,
        ) {
            return Ok(Some(CompactNotice {
                line: format!(
                    "  \x1b[33m!\x1b[0m \x1b[2mprompt budget is {} — run /compact or enable autoCompact\x1b[0m",
                    format_usage_line(&budget, guard.effective_limit_bytes)
                ),
                conversation_summary: ctx.conversation_summary.map(str::to_string),
                transcript_rewritten: false,
            }));
        }
        return Ok(None);
    }

    if !should_compact(
        &budget,
        guard.effective_limit_bytes,
        guard.compact_threshold,
    ) {
        return Ok(None);
    }

    let mut result = compact_messages(ctx, None).await?;

    if !result.compacted {
        return Ok(None);
    }

    // `compact_messages` operates on the durable raw transcript. Correct its
    // reported ratios for request-only context that is injected on the wire
    // but intentionally never written into session history.
    result.before_ratio = usage_ratio(&budget, guard.effective_limit_bytes);
    let active_system_prompt =
        merge_system_prompt(ctx.system_prompt, result.conversation_summary.as_deref());
    let after_budget = add_request_only_context_bytes(
        measure_prompt_budget(&active_system_prompt, ctx.messages, ctx.tool_defs),
        request_only_context_bytes,
    );
    result.after_ratio = usage_ratio(&after_budget, guard.effective_limit_bytes);

    rewrite_session_transcript(session_dir, session_path, ctx.messages)?;

    Ok(Some(CompactNotice {
        line: compaction_notice_line(&result),
        conversation_summary: result.conversation_summary.clone(),
        transcript_rewritten: true,
    }))
}

fn add_request_only_context_bytes(
    mut budget: PromptBudget,
    request_only_context_bytes: usize,
) -> PromptBudget {
    budget.transcript_bytes = budget
        .transcript_bytes
        .saturating_add(request_only_context_bytes);
    budget.total_bytes = budget
        .total_bytes
        .saturating_add(request_only_context_bytes);
    budget.effective_total_bytes = budget
        .effective_total_bytes
        .saturating_add(request_only_context_bytes);
    budget.estimated_tokens = estimate_tokens(budget.effective_total_bytes);
    budget
}

pub async fn maybe_compact_messages(
    messages: &mut Vec<ChatMessage>,
    system_prompt: &str,
    tool_defs: &[ToolDef],
    guard: &ContextGuardParams,
    http: &Client,
    backend: &BackendDescriptor,
    model: &str,
) -> Result<Option<CompactNotice>> {
    if !guard.auto_compact {
        return Ok(None);
    }

    let active_system_prompt =
        merge_system_prompt(system_prompt, guard.conversation_summary.as_deref());
    let budget = measure_prompt_budget(&active_system_prompt, messages, tool_defs);
    if !should_compact(
        &budget,
        guard.effective_limit_bytes,
        guard.compact_threshold,
    ) {
        return Ok(None);
    }

    let result = compact_messages_core(CompactRequest {
        messages,
        system_prompt,
        tool_defs,
        keep_messages: guard.keep_messages,
        limit_bytes: guard.effective_limit_bytes,
        summarize_budget_bytes: guard.summarize_budget_bytes,
        http,
        backend,
        model,
        conversation_summary: guard.conversation_summary.as_deref(),
        compaction: &guard.compaction,
    })
    .await?;

    if !result.compacted {
        return Ok(None);
    }

    Ok(Some(CompactNotice {
        line: compaction_notice_line(&result),
        conversation_summary: result.conversation_summary.clone(),
        transcript_rewritten: true,
    }))
}

/// The one-line compaction notice, with an optional warning line appended when a
/// configured compaction model could not be used.
fn compaction_notice_line(result: &CompactResult) -> String {
    let mut line = format!(
        "  \x1b[32m✓\x1b[0m \x1b[2m{}\x1b[0m",
        compact_notice(
            result.before_messages,
            result.after_messages,
            result.before_ratio,
            result.after_ratio,
            result.method
        )
    );
    if let Some(warning) = &result.warning {
        line.push_str(&format!("\n  \x1b[33m!\x1b[0m \x1b[2m{warning}\x1b[0m"));
    }
    line
}

pub fn context_status_lines(
    config: &AgentConfig,
    model: &str,
    is_local: bool,
    budget: &PromptBudget,
    last_notice: Option<&str>,
    conversation_summary: Option<&str>,
) -> Vec<String> {
    let guard = guard_config_from(config, model, is_local);
    let ratio = usage_ratio(budget, guard.effective_limit_bytes);
    let mut lines = vec![
        format!(
            "  \x1b[2meffectiveLimit\x1b[0m  {} (~{} model tokens, {:.0}% used, {} headroom)",
            format_bytes(guard.effective_limit_bytes),
            guard.model_context_tokens,
            ratio * 100.0,
            format_bytes(headroom_bytes(budget, guard.effective_limit_bytes))
        ),
        format!(
            "  \x1b[2mautoGuard\x1b[0m     autoCompact={} threshold={:.0}% reserve={:.0}%",
            guard.auto_compact,
            guard.compact_threshold * 100.0,
            config.context.reserve_ratio * 100.0
        ),
    ];
    if conversation_summary
        .map(str::trim)
        .is_some_and(|s| !s.is_empty())
    {
        lines.push("  \x1b[2msummary\x1b[0m       stored (persists across turns)".into());
    }
    if let Some(notice) = last_notice {
        lines.push(format!("  \x1b[2mlastGuard\x1b[0m     {notice}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{BackendDescriptor, BackendName};
    use crate::config::ContextConfig;
    use crate::openai::{ToolCall, ToolFunction};

    fn sample_budget(total: usize) -> PromptBudget {
        PromptBudget {
            system_bytes: 0,
            transcript_bytes: 0,
            tool_schema_bytes: 0,
            tool_result_bytes: 0,
            total_bytes: total,
            effective_total_bytes: total,
            estimated_tokens: total / 4,
        }
    }

    #[test]
    fn effective_limit_uses_model_tokens_and_max_bytes_min() {
        let config = ContextConfig {
            model_context_tokens: Some(8192),
            max_bytes: Some(256 * 1024),
            reserve_ratio: 0.25,
            ..Default::default()
        };
        let limit = effective_limit_bytes(&config, "qwen2.5-coder:7b", true);
        assert_eq!(limit, 8192 * 4 * 3 / 4);
    }

    #[test]
    fn effective_limit_respects_max_bytes_cap() {
        let config = ContextConfig {
            model_context_tokens: Some(32768),
            max_bytes: Some(32 * 1024),
            ..Default::default()
        };
        let limit = effective_limit_bytes(&config, "big-model", true);
        assert_eq!(limit, 32 * 1024);
    }

    #[test]
    fn should_compact_at_threshold_not_before() {
        let budget = sample_budget(840);
        assert!(!should_compact(&budget, 1000, 0.85));
        assert!(should_compact(&sample_budget(850), 1000, 0.85));
    }

    #[test]
    fn auto_compact_defaults_local_on_cloud_off() {
        let config = ContextConfig::default();
        assert!(resolve_auto_compact(&config, true));
        assert!(!resolve_auto_compact(&config, false));
    }

    #[test]
    fn partition_keeps_tool_rounds_intact() {
        let messages = vec![
            ChatMessage::User {
                content: "one".into(),
            },
            ChatMessage::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    kind: "function".into(),
                    function: ToolFunction {
                        name: "grep".into(),
                        arguments: "{}".into(),
                    },
                }],
            },
            ChatMessage::Tool {
                tool_call_id: "call-1".into(),
                content: "match".into(),
            },
            ChatMessage::User {
                content: "two".into(),
            },
            ChatMessage::Assistant {
                content: Some("done".into()),
                tool_calls: vec![],
            },
        ];
        let (older, recent) = partition_for_keep(&messages, 2).expect("partition");
        assert_eq!(older.len(), 3);
        assert_eq!(recent.len(), 2);
        validate_transcript(&recent).expect("valid recent transcript");
    }

    #[test]
    fn partition_can_split_large_single_user_turn_on_tool_boundaries() {
        let messages = vec![
            ChatMessage::User {
                content: "use multiple tools".into(),
            },
            ChatMessage::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    kind: "function".into(),
                    function: ToolFunction {
                        name: "grep".into(),
                        arguments: "{}".into(),
                    },
                }],
            },
            ChatMessage::Tool {
                tool_call_id: "call-1".into(),
                content: "first".into(),
            },
            ChatMessage::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call-2".into(),
                    kind: "function".into(),
                    function: ToolFunction {
                        name: "file_read".into(),
                        arguments: "{}".into(),
                    },
                }],
            },
            ChatMessage::Tool {
                tool_call_id: "call-2".into(),
                content: "second".into(),
            },
        ];
        let (older, recent) = partition_for_keep(&messages, 3).expect("partition");
        assert_eq!(older.len(), 3);
        assert_eq!(recent.len(), 2);
        validate_transcript(&recent).expect("valid recent transcript");
        assert!(matches!(
            &recent[0],
            ChatMessage::Assistant { tool_calls, .. } if tool_calls[0].id == "call-2"
        ));
    }

    #[tokio::test]
    async fn compact_budget_counts_existing_summary_bytes() {
        let mut messages = vec![
            ChatMessage::System {
                content: "sys".into(),
            },
            ChatMessage::User {
                content: "old request".into(),
            },
            ChatMessage::Assistant {
                content: Some("old answer".into()),
                tool_calls: vec![],
            },
            ChatMessage::User {
                content: "current request".into(),
            },
        ];
        let existing_summary = "x".repeat(500);
        let backend = BackendDescriptor {
            name: BackendName::Ollama,
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: "test".into(),
            is_local: true,
            openrouter: crate::backends::OpenRouterConfig::default(),
        };
        let http = Client::new();
        let result = compact_messages_core(CompactRequest {
            messages: &mut messages,
            system_prompt: "sys",
            tool_defs: &[],
            keep_messages: 1,
            limit_bytes: 1000,
            summarize_budget_bytes: 0,
            http: &http,
            backend: &backend,
            model: "mock",
            conversation_summary: Some(&existing_summary),
            compaction: &CompactionModel::Inherit,
        })
        .await
        .expect("deterministic compaction");

        assert!(result.before_ratio > 0.5);
        assert!(result
            .conversation_summary
            .as_deref()
            .is_some_and(|summary| summary.contains(&existing_summary)));
    }

    #[test]
    fn validate_rejects_orphan_tool_message() {
        let messages = vec![ChatMessage::Tool {
            tool_call_id: "call-1".into(),
            content: "orphan".into(),
        }];
        assert!(validate_transcript(&messages).is_err());
    }

    #[test]
    fn merge_and_extract_summary_round_trip() {
        let merged = merge_system_prompt("base prompt", Some("kept goals"));
        assert!(merged.contains("kept goals"));
        assert_eq!(
            extract_conversation_summary(&merged).as_deref(),
            Some("kept goals")
        );
        assert_eq!(strip_conversation_summary(&merged), "base prompt");
    }

    #[test]
    fn default_model_tokens_conservative_for_unknown_local() {
        assert_eq!(default_model_context_tokens("my-model", true), 8192);
        assert_eq!(
            default_model_context_tokens("qwen2.5-coder:7b", true),
            32768
        );
        assert_eq!(default_model_context_tokens("gpt-4", false), 128_000);
    }

    #[test]
    fn compaction_inherits_main_model_by_default() {
        let config = AgentConfig::default();
        assert!(matches!(
            classify_compaction_model(&config),
            CompactionModel::Inherit
        ));
    }

    #[test]
    fn compaction_uses_configured_ready_backend() {
        // Ollama needs no API key, so its backend is always ready.
        let mut config = AgentConfig::default();
        config.model_system.compaction =
            crate::model_system::ModelRef::parse_spec("ollama:qwen2.5-coder:7b");
        match classify_compaction_model(&config) {
            CompactionModel::Use { model, .. } => assert_eq!(model, "qwen2.5-coder:7b"),
            other => panic!("expected Use, got {other:?}"),
        }
    }

    #[test]
    fn compaction_marks_not_ready_backend_unavailable() {
        // openrouter is not ready without an API key. Force the key empty for
        // this test regardless of the ambient environment, then restore it.
        let saved = std::env::var("OPENROUTER_API_KEY").ok();
        std::env::set_var("OPENROUTER_API_KEY", "");

        let mut config = AgentConfig::default();
        config.model_system.compaction =
            crate::model_system::ModelRef::parse_spec("openrouter:anthropic/claude-3.5-haiku");
        let classified = classify_compaction_model(&config);

        match saved {
            Some(value) => std::env::set_var("OPENROUTER_API_KEY", value),
            None => std::env::remove_var("OPENROUTER_API_KEY"),
        }

        match classified {
            CompactionModel::Unavailable { label, reason } => {
                assert_eq!(label, "openrouter:anthropic/claude-3.5-haiku");
                assert!(reason.contains("OPENROUTER_API_KEY"), "reason: {reason}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    fn sample_compact_messages() -> Vec<ChatMessage> {
        vec![
            ChatMessage::System {
                content: "sys".into(),
            },
            ChatMessage::User {
                content: "old request".into(),
            },
            ChatMessage::Assistant {
                content: Some("old answer".into()),
                tool_calls: vec![],
            },
            ChatMessage::User {
                content: "current request".into(),
            },
        ]
    }

    fn dead_backend() -> BackendDescriptor {
        // Port 9 (discard) refuses the connection, so summarize_transcript errors.
        BackendDescriptor {
            name: BackendName::Ollama,
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: "test".into(),
            is_local: true,
            openrouter: crate::backends::OpenRouterConfig::default(),
        }
    }

    #[tokio::test]
    async fn compaction_use_model_failure_surfaces_warning() {
        // A dedicated compaction model whose summarize call fails must not
        // silently degrade to the deterministic trim; the failure is surfaced.
        let mut messages = sample_compact_messages();
        let backend = dead_backend();
        let compaction = CompactionModel::Use {
            backend: backend.clone(),
            model: "compactor".into(),
            label: "ollama:compactor".into(),
        };
        let http = Client::new();
        let result = compact_messages_core(CompactRequest {
            messages: &mut messages,
            system_prompt: "sys",
            tool_defs: &[],
            keep_messages: 1,
            limit_bytes: 1000,
            // Large budget keeps this on the LLM-summary path (not tier-2 trim).
            summarize_budget_bytes: 1_000_000,
            http: &http,
            backend: &backend,
            model: "mock",
            conversation_summary: None,
            compaction: &compaction,
        })
        .await
        .expect("compaction runs");

        assert_eq!(result.method, CompactMethod::DeterministicTrim);
        let warning = result.warning.expect("failure surfaced as warning");
        assert!(warning.contains("ollama:compactor"), "warning: {warning}");
        assert!(warning.contains("failed"), "warning: {warning}");
        // When the main-model retry also fails, the notice must say we trimmed —
        // never claim the main model summarized the transcript.
        assert!(
            warning.contains("trimmed deterministically"),
            "warning: {warning}"
        );
        assert!(
            warning.contains("main model mock also failed"),
            "warning: {warning}"
        );
        assert!(
            !warning.contains("summarized with"),
            "must not claim main model summarized after both failed: {warning}"
        );
    }

    #[tokio::test]
    async fn compaction_unavailable_then_main_fails_does_not_claim_summarized() {
        // Compaction model is configured but its backend is not ready, so we
        // inherit the main model. If that also fails, the notice must not say
        // "summarized with {main}" while we actually deterministic-trim.
        let mut messages = sample_compact_messages();
        let backend = dead_backend();
        let compaction = CompactionModel::Unavailable {
            label: "openrouter:anthropic/claude-3.5-haiku".into(),
            reason: "OPENROUTER_API_KEY is not set".into(),
        };
        let http = Client::new();
        let result = compact_messages_core(CompactRequest {
            messages: &mut messages,
            system_prompt: "sys",
            tool_defs: &[],
            keep_messages: 1,
            limit_bytes: 1000,
            summarize_budget_bytes: 1_000_000,
            http: &http,
            backend: &backend,
            model: "main-model",
            conversation_summary: None,
            compaction: &compaction,
        })
        .await
        .expect("compaction runs");

        assert_eq!(result.method, CompactMethod::DeterministicTrim);
        let warning = result.warning.expect("failure surfaced as warning");
        assert!(
            warning.contains("openrouter:anthropic/claude-3.5-haiku"),
            "warning: {warning}"
        );
        assert!(warning.contains("unavailable"), "warning: {warning}");
        assert!(
            warning.contains("main model main-model also failed"),
            "warning: {warning}"
        );
        assert!(
            warning.contains("trimmed deterministically"),
            "warning: {warning}"
        );
        assert!(
            !warning.contains("summarized with"),
            "must not claim main model summarized after it failed: {warning}"
        );
    }

    #[test]
    fn compaction_notice_appends_warning_when_present() {
        let result = CompactResult {
            compacted: true,
            before_messages: 10,
            after_messages: 4,
            before_ratio: 0.9,
            after_ratio: 0.3,
            method: CompactMethod::LlmSummary,
            conversation_summary: Some("summary".into()),
            warning: Some("compaction model openrouter:x unavailable".into()),
        };
        let line = compaction_notice_line(&result);
        assert!(line.contains("compaction model openrouter:x unavailable"));

        let no_warning = CompactResult {
            warning: None,
            ..result
        };
        assert!(!compaction_notice_line(&no_warning).contains("unavailable"));
    }
}
