//! Durable, append-only model-routing receipts.
//!
//! The ledger lives at `<workspace>/.albatross/routes.jsonl`. Each line is a
//! self-contained decision or model-call receipt so routing remains auditable
//! even after the chat session that produced it has been rotated or deleted.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::backends::BackendName;
use crate::config::WORKSPACE_SCRATCH_DIR;
use crate::model_system::{EffortLevel, ModelRef, ModelSystemConfig, RouteDecision};

static ROUTE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveRouteContext {
    pub route_id: String,
    pub role: String,
    pub backend: BackendName,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutedReviewReceipt {
    pub tier: String,
    pub model: ModelRef,
    #[serde(default)]
    pub effort: Option<EffortLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutedModelReceipt {
    pub model: ModelRef,
    #[serde(default)]
    pub effort: Option<EffortLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RouteLedgerEvent {
    RouteDecision {
        timestamp: String,
        route_id: String,
        session_id: String,
        task_hash: String,
        task_preview: String,
        selector: ModelRef,
        #[serde(default)]
        model_system: Box<ModelSystemConfig>,
        decision: RouteDecision,
        #[serde(default)]
        orchestrator: Option<ModelRef>,
        coder: RoutedModelReceipt,
        #[serde(default)]
        reviewer: Box<Option<RoutedReviewReceipt>>,
        #[serde(default)]
        security: Box<Option<RoutedModelReceipt>>,
        applied: bool,
    },
    ModelCall {
        timestamp: String,
        #[serde(default)]
        route_id: Option<String>,
        session_id: String,
        role: String,
        requested_backend: BackendName,
        requested_model: String,
        #[serde(default)]
        actual_model: Option<String>,
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        requested_effort: Option<EffortLevel>,
        #[serde(default)]
        effective_effort: Option<String>,
        effort_status: String,
        input_tokens: u32,
        output_tokens: u32,
        cached_input_tokens: u32,
        #[serde(default)]
        cost_usd: Option<f64>,
        cost_source: String,
        duration_ms: u64,
        status: String,
    },
}

#[derive(Debug, Clone)]
pub struct ModelCallInput<'a> {
    pub route_id: Option<&'a str>,
    pub session_id: &'a str,
    pub role: &'a str,
    pub backend: BackendName,
    pub requested_model: &'a str,
    pub actual_model: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub requested_effort: Option<EffortLevel>,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cached_input_tokens: u32,
    pub cost_usd: Option<f64>,
    pub cost_source: &'a str,
    pub duration_ms: u64,
    pub status: &'a str,
}

pub fn ledger_path(workspace_root: &str) -> PathBuf {
    Path::new(workspace_root)
        .join(WORKSPACE_SCRATCH_DIR)
        .join("routes.jsonl")
}

pub fn new_route_id() -> String {
    let sequence = ROUTE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(
        "route-{}-{sequence}",
        Utc::now().format("%Y%m%dT%H%M%S%.3fZ")
    )
}

pub fn session_id(session_path: &Path) -> String {
    session_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("session")
        .to_string()
}

pub fn task_hash(task: &str) -> String {
    let digest = Sha256::digest(task.trim().as_bytes());
    format!("sha256:{digest:x}")
}

pub fn task_preview(task: &str) -> String {
    const LIMIT: usize = 240;
    let normalized =
        crate::turn_trace::redact_string(&task.split_whitespace().collect::<Vec<_>>().join(" "));
    if normalized.chars().count() <= LIMIT {
        return normalized;
    }
    let mut preview = normalized.chars().take(LIMIT - 1).collect::<String>();
    preview.push('…');
    preview
}

pub fn effective_effort(
    backend: BackendName,
    requested: Option<EffortLevel>,
) -> (Option<String>, &'static str) {
    let Some(requested) = requested else {
        return (None, "not-requested");
    };
    match backend {
        BackendName::Openrouter => (
            Some(requested.openrouter_reasoning_effort().to_string()),
            if requested == EffortLevel::Max {
                "mapped"
            } else {
                "applied"
            },
        ),
        BackendName::OpenAi | BackendName::Grok => match requested.openai_reasoning_effort() {
            Some(effective) => (
                Some(effective.to_string()),
                if effective == requested.as_str() {
                    "applied"
                } else {
                    "mapped"
                },
            ),
            None => (None, "disabled"),
        },
        BackendName::OpenAiCodex
        | BackendName::Ollama
        | BackendName::LmStudio
        | BackendName::Mlx
        | BackendName::LlamaCpp => (None, "unsupported"),
    }
}

pub fn model_call_event(input: ModelCallInput<'_>) -> RouteLedgerEvent {
    let (effective_effort, effort_status) = effective_effort(input.backend, input.requested_effort);
    RouteLedgerEvent::ModelCall {
        timestamp: Utc::now().to_rfc3339(),
        route_id: input.route_id.map(str::to_string),
        session_id: input.session_id.to_string(),
        role: input.role.to_string(),
        requested_backend: input.backend,
        requested_model: input.requested_model.to_string(),
        actual_model: input.actual_model.map(str::to_string),
        provider: input.provider.map(str::to_string),
        requested_effort: input.requested_effort,
        effective_effort,
        effort_status: effort_status.to_string(),
        input_tokens: input.input_tokens,
        output_tokens: input.output_tokens,
        cached_input_tokens: input.cached_input_tokens,
        cost_usd: input.cost_usd,
        cost_source: input.cost_source.to_string(),
        duration_ms: input.duration_ms,
        status: input.status.to_string(),
    }
}

pub fn append_event(workspace_root: &str, event: &RouteLedgerEvent) -> Result<()> {
    let path = ledger_path(workspace_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating route ledger directory {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening route ledger {}", path.display()))?;
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    file.write_all(&line)?;
    Ok(())
}

pub fn read_events(workspace_root: &str) -> Result<Vec<RouteLedgerEvent>> {
    let path = ledger_path(workspace_root);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(&path)
        .with_context(|| format!("opening route ledger {}", path.display()))?;
    let reader = std::io::BufReader::new(file);
    let mut events = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str(&line) {
            events.push(event);
        }
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_receipt_distinguishes_requested_from_effective() {
        assert_eq!(
            effective_effort(BackendName::OpenAi, Some(EffortLevel::Max)),
            (Some("high".into()), "mapped")
        );
        assert_eq!(
            effective_effort(BackendName::Openrouter, Some(EffortLevel::Max)),
            (Some("xhigh".into()), "mapped")
        );
        assert_eq!(
            effective_effort(BackendName::Ollama, Some(EffortLevel::High)),
            (None, "unsupported")
        );
    }

    #[test]
    fn ledger_round_trips_and_skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let event = model_call_event(ModelCallInput {
            route_id: Some("route-1"),
            session_id: "session-1",
            role: "selector",
            backend: BackendName::Openrouter,
            requested_model: "openrouter/auto",
            actual_model: Some("openai/gpt-test"),
            provider: Some("openai"),
            requested_effort: Some(EffortLevel::High),
            input_tokens: 100,
            output_tokens: 20,
            cached_input_tokens: 5,
            cost_usd: Some(0.01),
            cost_source: "provider-reported",
            duration_ms: 123,
            status: "ok",
        });
        append_event(root, &event).unwrap();
        let mut file = OpenOptions::new()
            .append(true)
            .open(ledger_path(root))
            .unwrap();
        writeln!(file, "not json").unwrap();
        assert_eq!(read_events(root).unwrap().len(), 1);
    }

    #[test]
    fn task_preview_is_bounded_and_hash_is_stable() {
        let task = "word ".repeat(100);
        assert!(task_preview(&task).chars().count() <= 240);
        assert_eq!(task_hash(" hello "), task_hash("hello"));
        assert!(!task_preview("OPENAI_API_KEY=sk-secret123456").contains("secret123456"));
    }
}
