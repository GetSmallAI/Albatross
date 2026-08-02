use anyhow::Result;
use chrono::Utc;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

static SECRET_PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();

fn secret_patterns() -> &'static [(Regex, &'static str)] {
    SECRET_PATTERNS.get_or_init(|| {
        vec![
            (
                Regex::new(
                    r#"(?i)\b([A-Z0-9_]*(?:API[_-]?KEY|TOKEN|SECRET|PASSWORD|ACCESS[_-]?KEY)[A-Z0-9_]*=)[^\s'"]+"#,
                )
                .expect("env secret regex"),
                "${1}(redacted)",
            ),
            (
                Regex::new(r"(?i)\b(Bearer\s+)[A-Za-z0-9._~+/=-]{16,}")
                    .expect("bearer token regex"),
                "${1}(redacted)",
            ),
            (
                Regex::new(r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{8,}\b")
                    .expect("jwt regex"),
                "(redacted)",
            ),
            (
                Regex::new(r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b").expect("aws key regex"),
                "(redacted)",
            ),
            (
                Regex::new(r"(?i)\b(?:sk-[a-z0-9_-]{8,}|sk-or-[a-z0-9_-]{8,})")
                    .expect("api key regex"),
                "(redacted)",
            ),
            (
                Regex::new(r"\bgh[pousr]_[A-Za-z0-9_]{20,}\b").expect("github token regex"),
                "(redacted)",
            ),
        ]
    })
}

/// Sidecar path for a chat session transcript: `<stem>.events.jsonl`.
pub fn events_path_for_session(session_path: &Path) -> PathBuf {
    session_path.with_extension("events.jsonl")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventLogConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Default for EventLogConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allowed,
    Denied,
    Always,
    Session,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TurnMetrics {
    pub steps: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    pub model_ms: u64,
    pub tool_ms: u64,
    pub approval_ms: u64,
    pub total_ms: u64,
    pub hit_step_limit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TracePayload {
    ToolCall {
        call_id: String,
        name: String,
        args: Value,
        depth: u32,
    },
    ToolResult {
        call_id: String,
        name: String,
        duration_ms: u64,
        #[serde(default)]
        compacted: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        compact_summary: Option<String>,
        depth: u32,
    },
    Approval {
        tool: String,
        decision: ApprovalDecision,
        cache_key: String,
        duration_ms: u64,
    },
    ContextCompacted {
        method: String,
        before_msgs: usize,
        after_msgs: usize,
    },
    SubagentStart {
        call_id: String,
        task: String,
    },
    SubagentEnd {
        call_id: String,
        input_tokens: u32,
        output_tokens: u32,
        duration_ms: u64,
    },
    Warmup {
        duration_ms: u64,
        reason: String,
    },
    TurnSummary {
        #[serde(flatten)]
        metrics: TurnMetrics,
    },
    HookStart {
        event: String,
        key: String,
        source: String,
        command: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        matcher: Option<String>,
    },
    HookEnd {
        event: String,
        key: String,
        duration_ms: u128,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        timed_out: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        stdout: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        stderr: Option<String>,
    },
    HookDecision {
        event: String,
        key: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        decision: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        feedback: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        warning: Option<String>,
    },
    HookInputRewrite {
        tool: String,
        call_id: String,
        original_args: Value,
        effective_args: Value,
        depth: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceLine {
    timestamp: String,
    turn: u32,
    #[serde(flatten)]
    payload: TracePayload,
}

pub fn redact_value(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(redact_string(s)),
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                let key_lower = k.to_ascii_lowercase();
                if key_lower.contains("api_key")
                    || key_lower.contains("apikey")
                    || key_lower.contains("token")
                    || key_lower.contains("secret")
                    || key_lower.contains("password")
                {
                    out.insert(k.clone(), Value::String("(redacted)".into()));
                } else {
                    out.insert(k.clone(), redact_value(v));
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

pub fn redact_string(s: &str) -> String {
    let mut out = s.to_string();
    for (pattern, replacement) in secret_patterns() {
        out = pattern.replace_all(&out, *replacement).into_owned();
    }
    out
}

pub struct TurnTrace {
    path: PathBuf,
    turn: u32,
    turn_started: Instant,
    enabled: bool,
}

impl TurnTrace {
    pub fn open(session_path: &Path, enabled: bool) -> Result<Self> {
        let path = events_path_for_session(session_path);
        if enabled {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
        }
        Ok(Self {
            path,
            turn: 0,
            turn_started: Instant::now(),
            enabled,
        })
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[allow(dead_code)]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn begin_turn(&mut self) {
        self.turn = self.turn.saturating_add(1);
        self.turn_started = Instant::now();
    }

    #[allow(dead_code)]
    pub fn current_turn(&self) -> u32 {
        self.turn
    }

    pub fn append(&self, payload: TracePayload) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let line = TraceLine {
            timestamp: Utc::now().to_rfc3339(),
            turn: self.turn,
            payload,
        };
        let json = serde_json::to_string(&line)?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(json.as_bytes())?;
        f.write_all(b"\n")?;
        Ok(())
    }

    pub fn log_turn_summary(&self, mut metrics: TurnMetrics) -> Result<()> {
        metrics.total_ms = self.turn_started.elapsed().as_millis() as u64;
        self.append(TracePayload::TurnSummary { metrics })
    }
}

pub type SharedTurnTrace = Arc<Mutex<TurnTrace>>;

pub fn shared_trace(session_path: &Path, enabled: bool) -> Result<SharedTurnTrace> {
    Ok(Arc::new(Mutex::new(TurnTrace::open(
        session_path,
        enabled,
    )?)))
}

pub fn sync_trace_path(session_path: &Path, trace: &SharedTurnTrace) -> Result<()> {
    let mut guard = trace.lock().expect("turn trace mutex");
    guard.path = events_path_for_session(session_path);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTraceSummary {
    pub session_id: String,
    pub events_path: String,
    pub turn_count: usize,
    pub total_steps: usize,
    pub tool_calls: usize,
    pub subagent_runs: usize,
    pub approvals: usize,
    pub model_ms: u64,
    pub tool_ms: u64,
    pub approval_ms: u64,
    pub total_ms: u64,
    pub trace_found: bool,
}

pub fn read_trace_events(path: &Path) -> Result<Vec<TraceLine>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut events = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<TraceLine>(&line) {
            events.push(event);
        }
    }
    Ok(events)
}

pub fn summarize_session_trace(session_path: &Path) -> SessionTraceSummary {
    let session_id = session_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("session")
        .to_string();
    let events_path_buf = events_path_for_session(session_path);
    let events_path = events_path_buf.display().to_string();
    let mut summary = SessionTraceSummary {
        session_id,
        events_path,
        turn_count: 0,
        total_steps: 0,
        tool_calls: 0,
        subagent_runs: 0,
        approvals: 0,
        model_ms: 0,
        tool_ms: 0,
        approval_ms: 0,
        total_ms: 0,
        trace_found: false,
    };
    let Ok(events) = read_trace_events(&events_path_buf) else {
        return summary;
    };
    if events.is_empty() {
        return summary;
    }
    summary.trace_found = true;
    for event in events {
        match event.payload {
            TracePayload::ToolCall { .. } => summary.tool_calls += 1,
            TracePayload::SubagentEnd { .. } => summary.subagent_runs += 1,
            TracePayload::Approval { .. } => summary.approvals += 1,
            TracePayload::TurnSummary { metrics } => {
                summary.turn_count += 1;
                summary.total_steps += metrics.steps;
                summary.model_ms += metrics.model_ms;
                summary.tool_ms += metrics.tool_ms;
                summary.approval_ms += metrics.approval_ms;
                summary.total_ms += metrics.total_ms;
            }
            _ => {}
        }
    }
    summary
}

/// Resolve a stored session path against workspace and session directory.
pub fn resolve_session_path(
    stored_path: &str,
    workspace_root: &str,
    session_dir: &str,
    session_id: &str,
) -> PathBuf {
    let stored = Path::new(stored_path);
    if stored.is_absolute() && stored.exists() {
        return stored.to_path_buf();
    }
    let workspace = Path::new(workspace_root);
    let from_workspace = workspace.join(stored_path);
    if from_workspace.exists() {
        return from_workspace;
    }
    if stored.exists() {
        return stored.to_path_buf();
    }
    let from_session_dir = workspace
        .join(session_dir)
        .join(format!("{session_id}.jsonl"));
    if from_session_dir.exists() {
        return from_session_dir;
    }
    from_workspace
}

/// Test helper: disabled event log on a throwaway session path.
#[cfg(test)]
pub fn test_trace_for(session_path: &Path) -> SharedTurnTrace {
    shared_trace(session_path, false).expect("test trace")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn redacts_api_keys_in_strings() {
        let s = redact_string("use sk-1234567890abcdef for auth");
        assert!(!s.contains("sk-1234567890abcdef"));
        assert!(s.contains("(redacted)"));
    }

    #[test]
    fn redacts_common_secret_shapes_in_strings() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.signaturepart";
        let s = redact_string(&format!(
            "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE Authorization: Bearer {jwt} GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz123456"
        ));

        assert!(!s.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(!s.contains(jwt));
        assert!(!s.contains("ghp_abcdefghijklmnopqrstuvwxyz123456"));
        assert_eq!(s.matches("(redacted)").count(), 3);
    }

    #[test]
    fn redacts_sensitive_object_keys() {
        let v = serde_json::json!({ "api_key": "sk-secret", "path": "src/main.rs" });
        let out = redact_value(&v);
        assert_eq!(out["api_key"], "(redacted)");
        assert_eq!(out["path"], "src/main.rs");
    }

    #[test]
    fn append_writes_jsonl_lines() {
        let dir = TempDir::new().unwrap();
        let session = dir.path().join("2024-01-01.jsonl");
        let mut trace = TurnTrace::open(&session, true).unwrap();
        trace.begin_turn();
        trace
            .append(TracePayload::Warmup {
                duration_ms: 42,
                reason: "test".into(),
            })
            .unwrap();
        let text = std::fs::read_to_string(events_path_for_session(&session)).unwrap();
        assert!(text.contains("\"kind\":\"warmup\""));
        assert!(text.contains("\"turn\":1"));
    }

    #[test]
    fn deserializes_turn_summary_line() {
        let line = r#"{"timestamp":"2026-06-19T02:36:38.670440+00:00","turn":1,"kind":"turnSummary","steps":2,"modelMs":100,"toolMs":200,"approvalMs":0,"totalMs":0,"hitStepLimit":false}"#;
        let parsed: TraceLine = serde_json::from_str(line).expect("trace line");
        match parsed.payload {
            TracePayload::TurnSummary { metrics } => assert_eq!(metrics.steps, 2),
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn read_trace_events_roundtrip() {
        let dir = TempDir::new().unwrap();
        let session = dir.path().join("roundtrip.jsonl");
        let mut trace = TurnTrace::open(&session, true).unwrap();
        trace.begin_turn();
        trace
            .append(TracePayload::ToolCall {
                call_id: "1".into(),
                name: "grep".into(),
                args: serde_json::json!({ "pattern": "foo" }),
                depth: 0,
            })
            .unwrap();
        trace
            .log_turn_summary(TurnMetrics {
                steps: 2,
                ttft_ms: None,
                model_ms: 100,
                tool_ms: 200,
                approval_ms: 0,
                total_ms: 0,
                hit_step_limit: false,
            })
            .unwrap();
        let events = read_trace_events(&events_path_for_session(&session)).unwrap();
        assert_eq!(events.len(), 2, "parsed trace lines");
        assert!(matches!(events[0].payload, TracePayload::ToolCall { .. }));
        assert!(matches!(
            events[1].payload,
            TracePayload::TurnSummary { .. }
        ));
    }

    #[test]
    fn summarize_session_trace_aggregates_turn_metrics() {
        let dir = TempDir::new().unwrap();
        let session = dir.path().join("audit-session.jsonl");
        let mut trace = TurnTrace::open(&session, true).unwrap();
        trace.begin_turn();
        trace
            .append(TracePayload::ToolCall {
                call_id: "1".into(),
                name: "grep".into(),
                args: serde_json::json!({ "pattern": "foo" }),
                depth: 0,
            })
            .unwrap();
        trace
            .append(TracePayload::SubagentEnd {
                call_id: "task-1".into(),
                input_tokens: 100,
                output_tokens: 50,
                duration_ms: 200,
            })
            .unwrap();
        trace
            .append(TracePayload::Approval {
                tool: "file_edit".into(),
                decision: ApprovalDecision::Allowed,
                cache_key: "edit".into(),
                duration_ms: 50,
            })
            .unwrap();
        trace
            .log_turn_summary(TurnMetrics {
                steps: 3,
                ttft_ms: Some(100),
                model_ms: 500,
                tool_ms: 1200,
                approval_ms: 50,
                total_ms: 0,
                hit_step_limit: false,
            })
            .unwrap();

        let summary = summarize_session_trace(&session);
        assert!(summary.trace_found);
        assert_eq!(summary.session_id, "audit-session");
        assert_eq!(summary.turn_count, 1);
        assert_eq!(summary.total_steps, 3);
        assert_eq!(summary.tool_calls, 1);
        assert_eq!(summary.subagent_runs, 1);
        assert_eq!(summary.approvals, 1);
        assert_eq!(summary.model_ms, 500);
        assert_eq!(summary.tool_ms, 1200);
        assert_eq!(summary.approval_ms, 50);
    }

    #[test]
    fn summarize_missing_trace_returns_not_found() {
        let dir = TempDir::new().unwrap();
        let session = dir.path().join("missing.jsonl");
        let summary = summarize_session_trace(&session);
        assert!(!summary.trace_found);
        assert_eq!(summary.turn_count, 0);
    }
}
