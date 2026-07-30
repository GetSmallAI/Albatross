use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::backends::BackendDescriptor;
use crate::budget::measure_prompt_budget;
use crate::cancel::CancellationToken;
use crate::context_guard::{
    maybe_compact_messages, merge_system_prompt, should_compact, ContextGuardParams,
};
use crate::hooks::{
    bounded_hook_context_text, dispatch_hook_payload, hook_context_messages,
    plan_updated_payload_from_tool_result, render_hook_context_block, HookEventName,
    HookInvocationContext, HookOutcome, HookRegistry,
};
use crate::model_system::EffortLevel;
use crate::openai::{
    stream_chat, ChatMessage, ChatRequest, StreamOptions, ToolCall, ToolDef, ToolDefFunction,
    ToolFunction,
};
use crate::tools::{is_mutation_tool, is_read_only_tool, Tool, ToolPreview};
use crate::turn_checkpoint::TurnCapturer;
use crate::turn_trace::{SharedTurnTrace, TracePayload, TurnMetrics};

#[derive(Clone)]
pub struct AgentHooks {
    pub registry: HookRegistry,
    pub context: HookInvocationContext,
    pub trace: SharedTurnTrace,
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Text {
        delta: String,
    },
    ToolCall {
        name: String,
        call_id: String,
        args: Value,
        depth: u32,
    },
    ToolResult {
        name: String,
        call_id: String,
        output: String,
        depth: u32,
    },
    ToolOutputCompacted {
        name: String,
        call_id: String,
        summary: String,
        depth: u32,
    },
    Reasoning {
        delta: String,
    },
    ContextCompacted {
        notice: String,
        conversation_summary: Option<String>,
    },
    HookNotice(crate::hooks::HookNotice),
    /// The loop ran out its step budget while the model still wanted to call
    /// tools — the task is likely unfinished and can be resumed.
    StepLimitReached {
        max_steps: usize,
    },
}

#[async_trait]
pub trait ApprovalProvider: Send {
    async fn approve(&mut self, name: &str, args: &Value, preview: Option<&ToolPreview>) -> bool;
}

pub struct RunResult {
    pub messages: Vec<ChatMessage>,
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Portion of `input_tokens` the provider served from its prompt cache
    /// across this turn's steps (0 when the provider reports no cache details).
    pub cached_input_tokens: u32,
    pub reported_cost_usd: Option<f64>,
    /// Last provider-resolved model/provider observed across the streamed
    /// steps. These differ from the requested id for dynamic routers.
    pub actual_model: Option<String>,
    pub provider: Option<String>,
    pub transcript_rewritten: bool,
    pub conversation_summary: Option<String>,
    /// True when the loop stopped because it hit `max_steps` while the model
    /// still had pending tool calls (i.e. it was cut off, not finished).
    pub hit_step_limit: bool,
    pub metrics: TurnMetrics,
}

#[derive(Debug, Clone)]
pub struct CompactInfo {
    pub summary: String,
}

pub fn to_openai_tools(tools: &[Arc<dyn Tool>]) -> Vec<ToolDef> {
    tools
        .iter()
        .map(|t| ToolDef {
            kind: "function",
            function: ToolDefFunction {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.input_schema(),
            },
        })
        .collect()
}

fn looks_like_start_of_tool_call(text: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"^\s*(?:```(?:json)?\s*)?\{\s*"?name"?\s*:"#).expect("looks_like regex")
    });
    re.is_match(text)
}

fn try_parse_inline_tool_call(text: &str, tool_names: &HashSet<String>) -> Option<(String, Value)> {
    static OPEN: OnceLock<Regex> = OnceLock::new();
    static CLOSE: OnceLock<Regex> = OnceLock::new();
    let open = OPEN.get_or_init(|| Regex::new(r"^```(?:json)?\s*").unwrap());
    let close = CLOSE.get_or_init(|| Regex::new(r"\s*```$").unwrap());
    let trimmed = text.trim();
    let stripped = open.replace(trimmed, "");
    let stripped = close.replace(&stripped, "");
    let t = stripped.trim();
    if !t.starts_with('{') {
        return None;
    }
    let parsed: Value = serde_json::from_str(t).ok()?;
    let name = parsed.get("name")?.as_str()?.to_string();
    if !tool_names.contains(&name) {
        return None;
    }
    let args = parsed
        .get("arguments")
        .or_else(|| parsed.get("parameters"))
        .or_else(|| parsed.get("args"))
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    if !args.is_object() {
        return None;
    }
    Some((name, args))
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("…[truncated]");
    out
}

fn compact_tool_output(name: &str, output: &str) -> (String, Option<CompactInfo>) {
    const MAX_TOOL_OUTPUT_CHARS: usize = 4000;
    if output.chars().count() <= MAX_TOOL_OUTPUT_CHARS {
        return (output.to_string(), None);
    }
    if let Ok(mut parsed) = serde_json::from_str::<Value>(output) {
        if let Some(obj) = parsed.as_object_mut() {
            for key in ["content", "output", "diff"] {
                if let Some(Value::String(s)) = obj.get_mut(key) {
                    let original = s.chars().count();
                    *s = truncate_chars(s, MAX_TOOL_OUTPUT_CHARS);
                    obj.insert("compacted".into(), Value::Bool(true));
                    let summary =
                        format!("{name} output compacted ({original} chars → context limit)");
                    obj.insert("summary".into(), Value::String(summary.clone()));
                    return (
                        serde_json::to_string(&parsed)
                            .unwrap_or_else(|_| truncate_chars(output, MAX_TOOL_OUTPUT_CHARS)),
                        Some(CompactInfo { summary }),
                    );
                }
            }
            for key in ["matches", "entries"] {
                if let Some(Value::Array(items)) = obj.get_mut(key) {
                    let original = items.len();
                    items.truncate(50);
                    let kept = items.len();
                    obj.insert("compacted".into(), Value::Bool(true));
                    let summary = format!("{name} returned {original} items; kept first {kept}");
                    obj.insert("summary".into(), Value::String(summary.clone()));
                    return (
                        serde_json::to_string(&parsed)
                            .unwrap_or_else(|_| truncate_chars(output, MAX_TOOL_OUTPUT_CHARS)),
                        Some(CompactInfo { summary }),
                    );
                }
            }
        }
    }
    (
        truncate_chars(output, MAX_TOOL_OUTPUT_CHARS),
        Some(CompactInfo {
            summary: format!("{name} output truncated for model context"),
        }),
    )
}

fn updated_tool_input_from_hook(value: &Value) -> Option<Value> {
    if let Some(tool_input) = value.get("tool_input") {
        return tool_input.as_object().map(|_| tool_input.clone());
    }
    value.as_object().map(|_| value.clone())
}

fn append_hook_context_messages(messages: &mut Vec<ChatMessage>, contexts: Vec<String>) {
    if let Some(content) = render_hook_context_block(contexts) {
        messages.push(ChatMessage::User {
            content: content.into(),
        });
    }
}

fn merge_hook_payload(mut payload: Value, fields: Value) -> Value {
    let Some(obj) = payload.as_object_mut() else {
        return payload;
    };
    if let Value::Object(fields) = fields {
        for (key, value) in fields {
            obj.insert(key, value);
        }
    }
    payload
}

fn hook_block_output(reason: &str) -> String {
    serde_json::to_string(&serde_json::json!({ "error": reason }))
        .unwrap_or_else(|_| "{\"error\":\"hook blocked execution\"}".to_string())
}

fn auto_compact_due(
    messages: &[ChatMessage],
    system_prompt: &str,
    tool_defs: &[ToolDef],
    guard: &ContextGuardParams,
) -> bool {
    if !guard.auto_compact {
        return false;
    }
    let active_system_prompt =
        merge_system_prompt(system_prompt, guard.conversation_summary.as_deref());
    let budget = measure_prompt_budget(&active_system_prompt, messages, tool_defs);
    should_compact(
        &budget,
        guard.effective_limit_bytes,
        guard.compact_threshold,
    )
}

async fn dispatch_agent_hook<F>(
    hooks: Option<&AgentHooks>,
    event: HookEventName,
    fields: Value,
    matcher_value: Option<&str>,
    on_event: &mut F,
) -> HookOutcome
where
    F: FnMut(AgentEvent),
{
    let Some(hooks) = hooks else {
        return HookOutcome::default();
    };
    let payload = merge_hook_payload(hooks.context.payload(event).into_value(), fields);
    let outcome = dispatch_hook_payload(
        &hooks.registry,
        event,
        &payload,
        matcher_value,
        Some(hooks.trace.clone()),
    )
    .await;
    for notice in &outcome.notices {
        on_event(AgentEvent::HookNotice(notice.clone()));
    }
    outcome
}

#[allow(clippy::too_many_arguments)]
pub async fn run_agent<F>(
    http: &reqwest::Client,
    backend: &BackendDescriptor,
    model: &str,
    effort: Option<EffortLevel>,
    initial_messages: Vec<ChatMessage>,
    tools: Vec<Arc<dyn Tool>>,
    max_steps: usize,
    mut on_event: F,
    mut approve: Option<&mut dyn ApprovalProvider>,
    cancel: Option<CancellationToken>,
    guard: Option<(ContextGuardParams, String)>,
    mut capturer: Option<&mut TurnCapturer>,
    trace: Option<SharedTurnTrace>,
    depth: u32,
    hooks: Option<AgentHooks>,
) -> Result<RunResult>
where
    F: FnMut(AgentEvent),
{
    let mut messages = initial_messages;
    let tool_defs = to_openai_tools(&tools);
    let tool_map: HashMap<String, Arc<dyn Tool>> = tools
        .iter()
        .map(|t| (t.name().to_string(), t.clone()))
        .collect();
    let tool_names: HashSet<String> = tools.iter().map(|t| t.name().to_string()).collect();

    let mut total_in: u32 = 0;
    let mut total_out: u32 = 0;
    let mut total_cached: u32 = 0;
    let mut reported_cost_usd: Option<f64> = None;
    let mut actual_model: Option<String> = None;
    let mut provider: Option<String> = None;
    // Prefix identity is fixed for the turn (system message + model are stable
    // across steps), so derive OpenAI's cache-routing key once and reuse it.
    let cache_key = crate::openai::session_cache_key(backend, model, &messages);
    let mut transcript_rewritten = false;
    let mut natural_stop = false;
    let mut conversation_summary = guard
        .as_ref()
        .map(|(params, _)| params.conversation_summary.clone())
        .unwrap_or_default();

    let turn_started = Instant::now();
    let mut metrics = TurnMetrics::default();
    let mut ttft_recorded = false;
    let mut steps_taken = 0usize;

    let log_trace = |payload: TracePayload| {
        if let Some(trace) = &trace {
            if let Ok(guard) = trace.lock() {
                let _ = guard.append(payload);
            }
        }
    };

    for step in 0..max_steps {
        if cancel.as_ref().map(|c| c.is_cancelled()).unwrap_or(false) {
            break;
        }
        steps_taken += 1;
        let step_start = Instant::now();
        let req = ChatRequest {
            model,
            messages: &messages,
            tools: if tool_defs.is_empty() {
                None
            } else {
                Some(&tool_defs)
            },
            stream: true,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            max_tokens: None,
            prompt_cache_key: cache_key.as_deref(),
            effort,
        };

        let mut assistant_text = String::new();
        let mut buffering_inline = false;
        let mut tool_calls: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
        let mut saw_first_token = false;
        let mut step_reported_cost_usd: Option<f64> = None;

        stream_chat(http, backend, &req, cancel.clone(), |chunk| {
            if chunk
                .model
                .as_deref()
                .is_some_and(|value| !value.is_empty())
            {
                actual_model = chunk.model.clone();
            }
            if chunk
                .provider
                .as_deref()
                .is_some_and(|value| !value.is_empty())
            {
                provider = chunk.provider.clone();
            }
            if let Some(choice) = chunk.choices.first() {
                if let Some(reasoning) = &choice.delta.reasoning {
                    if !saw_first_token {
                        saw_first_token = true;
                        if !ttft_recorded {
                            metrics.ttft_ms = Some(turn_started.elapsed().as_millis() as u64);
                            ttft_recorded = true;
                        }
                    }
                    on_event(AgentEvent::Reasoning {
                        delta: reasoning.clone(),
                    });
                }
                if let Some(content) = &choice.delta.content {
                    if !saw_first_token && !content.is_empty() {
                        saw_first_token = true;
                        if !ttft_recorded {
                            metrics.ttft_ms = Some(turn_started.elapsed().as_millis() as u64);
                            ttft_recorded = true;
                        }
                    }
                    let was_empty = assistant_text.is_empty();
                    assistant_text.push_str(content);
                    if was_empty && looks_like_start_of_tool_call(&assistant_text) {
                        buffering_inline = true;
                    }
                    if !buffering_inline && !content.is_empty() {
                        on_event(AgentEvent::Text {
                            delta: content.clone(),
                        });
                    }
                }
                if let Some(tcs) = &choice.delta.tool_calls {
                    for tc in tcs {
                        let idx = tc.index.unwrap_or(0);
                        let entry = tool_calls
                            .entry(idx)
                            .or_insert_with(|| (String::new(), String::new(), String::new()));
                        if let Some(id) = &tc.id {
                            if !id.is_empty() {
                                entry.0 = id.clone();
                            }
                        }
                        if let Some(f) = &tc.function {
                            if let Some(n) = &f.name {
                                entry.1.push_str(n);
                            }
                            if let Some(a) = &f.arguments {
                                entry.2.push_str(a);
                            }
                        }
                    }
                }
            }
            if let Some(usage) = &chunk.usage {
                total_in += usage.prompt_tokens;
                total_out += usage.completion_tokens;
                total_cached += usage.cached_tokens();
                if let Some(cost) = usage.cost {
                    step_reported_cost_usd = Some(cost);
                }
            }
        })
        .await?;
        if let Some(cost) = step_reported_cost_usd {
            reported_cost_usd = Some(reported_cost_usd.unwrap_or(0.0) + cost);
        }
        metrics.model_ms += step_start.elapsed().as_millis() as u64;

        let mut final_calls: Vec<ToolCall> = tool_calls
            .into_values()
            .filter(|(id, name, _)| !id.is_empty() && !name.is_empty())
            .map(|(id, name, args)| ToolCall {
                id,
                kind: "function".into(),
                function: ToolFunction {
                    name,
                    arguments: if args.is_empty() { "{}".into() } else { args },
                },
            })
            .collect();

        if final_calls.is_empty() && buffering_inline {
            if let Some((name, args)) = try_parse_inline_tool_call(&assistant_text, &tool_names) {
                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                final_calls.push(ToolCall {
                    id: format!("inline-{step}-{ts}"),
                    kind: "function".into(),
                    function: ToolFunction {
                        name,
                        arguments: serde_json::to_string(&args).unwrap_or_else(|_| "{}".into()),
                    },
                });
                assistant_text.clear();
            } else {
                on_event(AgentEvent::Text {
                    delta: assistant_text.clone(),
                });
            }
        }

        let assistant_content = if assistant_text.is_empty() {
            None
        } else {
            Some(assistant_text.clone())
        };
        if final_calls.is_empty() {
            messages.push(ChatMessage::Assistant {
                content: assistant_content,
                tool_calls: final_calls,
            });
            natural_stop = true;
            break;
        }

        // Tool execution proceeds in three phases so that read-only calls in a
        // single step can run concurrently while mutations stay strictly serial:
        //   1. Resolve approvals and capture mutation snapshots, in order. This
        //      phase is interactive and borrows `approve`/`capturer`, so it must
        //      stay sequential.
        //   2. Execute. Read-only tools are polled concurrently; mutations and
        //      other side-effecting tools (shell, run_tests, MCP) run serially in
        //      call order.
        //   3. Emit ToolResult events and push tool messages in call order.
        enum Pending {
            /// Output already determined (denial or unknown tool); skip execution.
            Done(String),
            Run {
                tool: Arc<dyn Tool>,
                args: Value,
                read_only: bool,
            },
        }

        let mut tcs: Vec<ToolCall> = Vec::with_capacity(final_calls.len());
        let mut tool_inputs: Vec<Value> = Vec::with_capacity(final_calls.len());
        let mut original_tool_inputs: Vec<Option<Value>> = Vec::with_capacity(final_calls.len());
        let mut run_post_hooks: Vec<bool> = Vec::with_capacity(final_calls.len());
        let mut pending: Vec<Pending> = Vec::with_capacity(final_calls.len());
        let mut hook_contexts: Vec<String> = Vec::new();
        let mut stop_after_step = false;
        let mut stop_remaining_reason: Option<String> = None;

        for mut tc in final_calls {
            let mut parsed_args: Value = serde_json::from_str(&tc.function.arguments)
                .unwrap_or_else(|_| Value::Object(serde_json::Map::new()));
            if let Some(reason) = stop_remaining_reason.clone() {
                on_event(AgentEvent::ToolCall {
                    name: tc.function.name.clone(),
                    call_id: tc.id.clone(),
                    args: parsed_args.clone(),
                    depth,
                });
                log_trace(TracePayload::ToolCall {
                    call_id: tc.id.clone(),
                    name: tc.function.name.clone(),
                    args: crate::turn_trace::redact_value(&parsed_args),
                    depth,
                });
                tool_inputs.push(parsed_args);
                original_tool_inputs.push(None);
                run_post_hooks.push(false);
                tcs.push(tc);
                pending.push(Pending::Done(hook_block_output(&reason)));
                continue;
            }
            let original_args = parsed_args.clone();
            let mut rewritten_original_args = None;
            let pre_outcome = dispatch_agent_hook(
                hooks.as_ref(),
                HookEventName::PreToolUse,
                serde_json::json!({
                    "tool_name": tc.function.name,
                    "tool_use_id": tc.id,
                    "tool_input": parsed_args,
                    "depth": depth,
                }),
                Some(&tc.function.name),
                &mut on_event,
            )
            .await;
            hook_contexts.extend(hook_context_messages(
                HookEventName::PreToolUse,
                &pre_outcome,
            ));
            let pre_stop_reason = pre_outcome.stop_reason.clone();
            if let Some(updated) = pre_outcome
                .updated_input
                .as_ref()
                .and_then(updated_tool_input_from_hook)
            {
                parsed_args = updated;
                tc.function.arguments =
                    serde_json::to_string(&parsed_args).unwrap_or_else(|_| "{}".into());
                if parsed_args != original_args {
                    rewritten_original_args = Some(original_args.clone());
                    let original_text = serde_json::to_string(&original_args)
                        .unwrap_or_else(|_| "<unserializable>".into());
                    let rewritten_text = serde_json::to_string(&parsed_args)
                        .unwrap_or_else(|_| "<unserializable>".into());
                    hook_contexts.push(format!(
                        "PreToolUse hook rewrote {} input. original={} effective={}",
                        tc.function.name,
                        bounded_hook_context_text(&original_text),
                        bounded_hook_context_text(&rewritten_text)
                    ));
                    log_trace(TracePayload::HookInputRewrite {
                        tool: tc.function.name.clone(),
                        call_id: tc.id.clone(),
                        original_args: crate::turn_trace::redact_value(&original_args),
                        effective_args: crate::turn_trace::redact_value(&parsed_args),
                        depth,
                    });
                }
            }
            on_event(AgentEvent::ToolCall {
                name: tc.function.name.clone(),
                call_id: tc.id.clone(),
                args: parsed_args.clone(),
                depth,
            });
            log_trace(TracePayload::ToolCall {
                call_id: tc.id.clone(),
                name: tc.function.name.clone(),
                args: crate::turn_trace::redact_value(&parsed_args),
                depth,
            });

            let mut run_post_hook = false;
            let entry = if let Some(reason) = pre_outcome.blocking_reason {
                Pending::Done(hook_block_output(&reason))
            } else if let Some(reason) = pre_stop_reason {
                stop_after_step = true;
                stop_remaining_reason = Some(reason.clone());
                Pending::Done(hook_block_output(&reason))
            } else if let Some(tool) = tool_map.get(&tc.function.name) {
                let needs_approval = tool.require_approval(&parsed_args);
                let mut denied = false;
                let mut denial_reason = "User denied execution.".to_string();
                if needs_approval {
                    let preview = tool.preview(&parsed_args).await;
                    let preview_payload = preview.as_ref().map(|preview| {
                        serde_json::json!({
                            "summary": preview.summary,
                            "diff": preview.diff,
                            "risk": preview.risk,
                        })
                    });
                    let permission_outcome = dispatch_agent_hook(
                        hooks.as_ref(),
                        HookEventName::PermissionRequest,
                        serde_json::json!({
                            "tool_name": tc.function.name,
                            "tool_use_id": tc.id,
                            "tool_input": parsed_args,
                            "preview": preview_payload,
                            "depth": depth,
                        }),
                        Some(&tc.function.name),
                        &mut on_event,
                    )
                    .await;
                    hook_contexts.extend(hook_context_messages(
                        HookEventName::PermissionRequest,
                        &permission_outcome,
                    ));
                    if let Some(reason) = permission_outcome.blocking_reason {
                        denied = true;
                        denial_reason = reason;
                    }
                    if let Some(reason) = permission_outcome.stop_reason {
                        denied = true;
                        denial_reason = reason.clone();
                        stop_after_step = true;
                        stop_remaining_reason = Some(reason);
                    }
                    if !permission_outcome.allowed && !denied {
                        if let Some(provider) = approve.as_deref_mut() {
                            if !provider
                                .approve(&tc.function.name, &parsed_args, preview.as_ref())
                                .await
                            {
                                denied = true;
                            }
                        } else {
                            denied = true;
                        }
                    }
                }
                if denied {
                    Pending::Done(
                        serde_json::to_string(&serde_json::json!({
                            "error": denial_reason
                        }))
                        .unwrap(),
                    )
                } else {
                    let subagent_start = if tc.function.name == "task" {
                        let outcome = dispatch_agent_hook(
                            hooks.as_ref(),
                            HookEventName::SubagentStart,
                            serde_json::json!({
                                "tool_name": tc.function.name,
                                "tool_use_id": tc.id,
                                "tool_input": parsed_args,
                                "depth": depth,
                            }),
                            Some(&tc.function.name),
                            &mut on_event,
                        )
                        .await;
                        hook_contexts.extend(hook_context_messages(
                            HookEventName::SubagentStart,
                            &outcome,
                        ));
                        let stop_reason = outcome.stop_reason.clone();
                        if let Some(reason) = stop_reason.clone() {
                            stop_after_step = true;
                            stop_remaining_reason = Some(reason);
                        }
                        outcome.blocking_reason.or(stop_reason)
                    } else {
                        None
                    };
                    if let Some(reason) = subagent_start {
                        Pending::Done(hook_block_output(&reason))
                    } else {
                        if is_mutation_tool(&tc.function.name) {
                            if let Some(c) = capturer.as_deref_mut() {
                                c.snapshot_before_tool(&tc.function.name, &parsed_args)
                                    .await;
                            }
                        }
                        let pending_entry = Pending::Run {
                            tool: tool.clone(),
                            args: parsed_args.clone(),
                            read_only: is_read_only_tool(&tc.function.name),
                        };
                        run_post_hook = true;
                        pending_entry
                    }
                }
            } else {
                Pending::Done(
                    serde_json::to_string(&serde_json::json!({
                        "error": format!("Unknown tool: {}", tc.function.name)
                    }))
                    .unwrap(),
                )
            };
            tool_inputs.push(parsed_args);
            original_tool_inputs.push(rewritten_original_args);
            run_post_hooks.push(run_post_hook);
            tcs.push(tc);
            pending.push(entry);
        }

        if let Some(reason) = stop_remaining_reason.as_deref() {
            for (entry, run_post_hook) in pending.iter_mut().zip(run_post_hooks.iter_mut()) {
                if matches!(entry, Pending::Run { .. }) {
                    *entry = Pending::Done(hook_block_output(reason));
                    *run_post_hook = false;
                }
            }
        }

        messages.push(ChatMessage::Assistant {
            content: assistant_content,
            tool_calls: tcs.clone(),
        });

        fn value_to_string(result: &Value) -> String {
            if let Some(s) = result.as_str() {
                s.to_string()
            } else {
                serde_json::to_string(result).unwrap_or_else(|_| "null".into())
            }
        }

        let pending_len = pending.len();
        let mut outputs: Vec<Option<String>> = (0..pending_len).map(|_| None).collect();
        let mut tool_durations: Vec<u64> = vec![0; pending_len];
        let mut read_idx: Vec<usize> = Vec::new();
        let mut read_futs = Vec::new();
        let mut serial: Vec<(usize, Arc<dyn Tool>, Value)> = Vec::new();

        for (i, entry) in pending.into_iter().enumerate() {
            match entry {
                Pending::Done(out) => outputs[i] = Some(out),
                Pending::Run {
                    tool,
                    args,
                    read_only: true,
                } => {
                    let c = cancel.clone();
                    read_idx.push(i);
                    read_futs.push(async move {
                        let start = Instant::now();
                        let out = value_to_string(&tool.execute_cancelable(args, c).await);
                        (out, start.elapsed().as_millis() as u64)
                    });
                }
                Pending::Run {
                    tool,
                    args,
                    read_only: false,
                } => serial.push((i, tool, args)),
            }
        }

        if !read_futs.is_empty() {
            let results = futures_util::future::join_all(read_futs).await;
            for (i, (out, ms)) in read_idx.into_iter().zip(results) {
                outputs[i] = Some(out);
                tool_durations[i] = ms;
                metrics.tool_ms += ms;
            }
        }

        for (i, tool, args) in serial {
            let start = Instant::now();
            outputs[i] = Some(value_to_string(
                &tool.execute_cancelable(args, cancel.clone()).await,
            ));
            let ms = start.elapsed().as_millis() as u64;
            tool_durations[i] = ms;
            metrics.tool_ms += ms;
        }

        for (((((tc, output), duration_ms), tool_input), original_tool_input), run_post_hook) in tcs
            .into_iter()
            .zip(outputs)
            .zip(tool_durations)
            .zip(tool_inputs)
            .zip(original_tool_inputs)
            .zip(run_post_hooks)
        {
            let output_str = output.unwrap_or_else(|| "null".into());
            let (mut trimmed, compact_info) = compact_tool_output(&tc.function.name, &output_str);
            if tc.function.name == "update_plan" {
                if let Some(hooks_ref) = hooks.as_ref() {
                    if let Some(payload) = plan_updated_payload_from_tool_result(
                        &hooks_ref.context,
                        &tc.id,
                        &output_str,
                    ) {
                        let outcome = dispatch_hook_payload(
                            &hooks_ref.registry,
                            HookEventName::PlanUpdated,
                            &payload,
                            None,
                            Some(hooks_ref.trace.clone()),
                        )
                        .await;
                        for notice in &outcome.notices {
                            on_event(AgentEvent::HookNotice(notice.clone()));
                        }
                        hook_contexts
                            .extend(hook_context_messages(HookEventName::PlanUpdated, &outcome));
                        if outcome.stop_reason.is_some() {
                            stop_after_step = true;
                        }
                    }
                }
            }
            let input_rewritten = original_tool_input.is_some();
            let hook_tool_payload = |tool_response: &str| {
                let mut payload = serde_json::json!({
                    "tool_name": tc.function.name,
                    "tool_use_id": tc.id,
                    "tool_input": tool_input,
                    "tool_response": tool_response,
                    "input_rewritten": input_rewritten,
                    "depth": depth,
                });
                if let (Some(obj), Some(original)) =
                    (payload.as_object_mut(), original_tool_input.as_ref())
                {
                    obj.insert("original_tool_input".into(), original.clone());
                }
                payload
            };
            if tc.function.name == "task" && run_post_hook {
                let subagent_stop = dispatch_agent_hook(
                    hooks.as_ref(),
                    HookEventName::SubagentStop,
                    hook_tool_payload(&trimmed),
                    Some(&tc.function.name),
                    &mut on_event,
                )
                .await;
                hook_contexts.extend(hook_context_messages(
                    HookEventName::SubagentStop,
                    &subagent_stop,
                ));
                if let Some(reason) = subagent_stop.blocking_reason {
                    trimmed = hook_block_output(&reason);
                }
                if subagent_stop.stop_reason.is_some() {
                    stop_after_step = true;
                }
            }
            if run_post_hook && tc.function.name != "task" {
                let post_outcome = dispatch_agent_hook(
                    hooks.as_ref(),
                    HookEventName::PostToolUse,
                    hook_tool_payload(&trimmed),
                    Some(&tc.function.name),
                    &mut on_event,
                )
                .await;
                hook_contexts.extend(hook_context_messages(
                    HookEventName::PostToolUse,
                    &post_outcome,
                ));
                if let Some(reason) = post_outcome.blocking_reason {
                    trimmed = hook_block_output(&reason);
                }
                if post_outcome.stop_reason.is_some() {
                    stop_after_step = true;
                }
            }
            if let Some(info) = &compact_info {
                on_event(AgentEvent::ToolOutputCompacted {
                    name: tc.function.name.clone(),
                    call_id: tc.id.clone(),
                    summary: info.summary.clone(),
                    depth,
                });
            }
            on_event(AgentEvent::ToolResult {
                name: tc.function.name.clone(),
                call_id: tc.id.clone(),
                output: trimmed.clone(),
                depth,
            });
            log_trace(TracePayload::ToolResult {
                call_id: tc.id.clone(),
                name: tc.function.name.clone(),
                duration_ms,
                compacted: compact_info.is_some(),
                compact_summary: compact_info.as_ref().map(|i| i.summary.clone()),
                depth,
            });
            messages.push(ChatMessage::Tool {
                tool_call_id: tc.id,
                content: trimmed,
            });
        }

        append_hook_context_messages(&mut messages, hook_contexts);

        if stop_after_step {
            natural_stop = true;
            break;
        }

        if let Some((guard_params, system_prompt)) = &guard {
            if !auto_compact_due(&messages, system_prompt, &tool_defs, guard_params) {
                continue;
            }
            let pre_compact = dispatch_agent_hook(
                hooks.as_ref(),
                HookEventName::PreCompact,
                serde_json::json!({
                    "phase": "agent",
                    "message_count": messages.len(),
                    "depth": depth,
                }),
                None,
                &mut on_event,
            )
            .await;
            append_hook_context_messages(
                &mut messages,
                hook_context_messages(HookEventName::PreCompact, &pre_compact),
            );
            let mut stop_after_compact_hook = pre_compact.stop_reason.is_some();
            if pre_compact.blocking_reason.is_none() {
                if let Some(notice) = maybe_compact_messages(
                    &mut messages,
                    system_prompt,
                    &tool_defs,
                    guard_params,
                    http,
                    backend,
                    model,
                )
                .await?
                {
                    if notice.transcript_rewritten {
                        transcript_rewritten = true;
                    }
                    if let Some(summary) = notice.conversation_summary {
                        conversation_summary = Some(summary);
                    }
                    log_trace(TracePayload::ContextCompacted {
                        method: "auto".into(),
                        before_msgs: messages.len(),
                        after_msgs: messages.len(),
                    });
                    on_event(AgentEvent::ContextCompacted {
                        notice: notice.line.clone(),
                        conversation_summary: conversation_summary.clone(),
                    });
                    let post_compact = dispatch_agent_hook(
                        hooks.as_ref(),
                        HookEventName::PostCompact,
                        serde_json::json!({
                            "phase": "agent",
                            "notice": notice.line,
                            "conversation_summary": conversation_summary,
                            "message_count": messages.len(),
                            "depth": depth,
                        }),
                        None,
                        &mut on_event,
                    )
                    .await;
                    append_hook_context_messages(
                        &mut messages,
                        hook_context_messages(HookEventName::PostCompact, &post_compact),
                    );
                    if post_compact.stop_reason.is_some() {
                        stop_after_compact_hook = true;
                    }
                }
            }
            if stop_after_compact_hook {
                natural_stop = true;
                break;
            }
        }
    }

    let cancelled = cancel.as_ref().map(|c| c.is_cancelled()).unwrap_or(false);
    let hit_step_limit = !natural_stop && !cancelled && max_steps > 0;
    if hit_step_limit {
        on_event(AgentEvent::StepLimitReached { max_steps });
    }

    metrics.steps = steps_taken;
    metrics.hit_step_limit = hit_step_limit;
    metrics.total_ms = turn_started.elapsed().as_millis() as u64;

    Ok(RunResult {
        messages,
        input_tokens: total_in,
        output_tokens: total_out,
        cached_input_tokens: total_cached,
        reported_cost_usd,
        actual_model,
        provider,
        transcript_rewritten,
        conversation_summary,
        hit_step_limit,
        metrics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> HashSet<String> {
        ["shell", "file_read", "grep"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn looks_like_tool_call_positives() {
        assert!(looks_like_start_of_tool_call("{\"name\":"));
        assert!(looks_like_start_of_tool_call("  {\"name\": \"shell\""));
        assert!(looks_like_start_of_tool_call(
            "```json\n{\"name\":\"shell\""
        ));
        assert!(looks_like_start_of_tool_call("```\n{\"name\":"));
        assert!(looks_like_start_of_tool_call("{name: \"shell\""));
    }

    #[test]
    fn looks_like_tool_call_negatives() {
        assert!(!looks_like_start_of_tool_call("Hello, how can I help?"));
        assert!(!looks_like_start_of_tool_call(""));
        assert!(!looks_like_start_of_tool_call("Here's the answer:"));
        assert!(!looks_like_start_of_tool_call("{not_name: 1}"));
    }

    #[test]
    fn parse_inline_arguments_field() {
        let n = names();
        let (name, args) =
            try_parse_inline_tool_call(r#"{"name":"shell","arguments":{"command":"ls"}}"#, &n)
                .unwrap();
        assert_eq!(name, "shell");
        assert_eq!(args.get("command").unwrap().as_str().unwrap(), "ls");
    }

    #[test]
    fn parse_inline_parameters_alias() {
        let n = names();
        let (_, args) =
            try_parse_inline_tool_call(r#"{"name":"shell","parameters":{"command":"pwd"}}"#, &n)
                .unwrap();
        assert_eq!(args.get("command").unwrap().as_str().unwrap(), "pwd");
    }

    #[test]
    fn parse_inline_args_alias() {
        let n = names();
        let (_, args) =
            try_parse_inline_tool_call(r#"{"name":"grep","args":{"pattern":"foo"}}"#, &n).unwrap();
        assert_eq!(args.get("pattern").unwrap().as_str().unwrap(), "foo");
    }

    #[test]
    fn parse_inline_with_json_fence() {
        let n = names();
        let r = try_parse_inline_tool_call(
            "```json\n{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}\n```",
            &n,
        );
        assert!(r.is_some());
    }

    #[test]
    fn parse_inline_with_bare_fence() {
        let n = names();
        let r = try_parse_inline_tool_call(
            "```\n{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}\n```",
            &n,
        );
        assert!(r.is_some());
    }

    #[test]
    fn parse_inline_unknown_tool_returns_none() {
        let n = names();
        assert!(
            try_parse_inline_tool_call(r#"{"name":"unknown_tool","arguments":{}}"#, &n).is_none()
        );
    }

    #[test]
    fn parse_inline_invalid_json_returns_none() {
        let n = names();
        assert!(try_parse_inline_tool_call("{name", &n).is_none());
        assert!(try_parse_inline_tool_call("not json at all", &n).is_none());
        assert!(try_parse_inline_tool_call("", &n).is_none());
    }

    #[test]
    fn parse_inline_missing_name_returns_none() {
        let n = names();
        assert!(try_parse_inline_tool_call(r#"{"arguments":{}}"#, &n).is_none());
    }

    #[test]
    fn parse_inline_no_args_defaults_to_empty_object() {
        let n = names();
        let (_, args) = try_parse_inline_tool_call(r#"{"name":"shell"}"#, &n).unwrap();
        assert!(args.is_object());
    }

    #[test]
    fn compacts_large_json_content_field() {
        let output = serde_json::json!({
            "content": "x".repeat(5000),
            "totalLines": 1
        })
        .to_string();
        let compacted = compact_tool_output("file_read", &output);
        let parsed: Value = serde_json::from_str(&compacted.0).unwrap();
        assert_eq!(parsed["compacted"].as_bool(), Some(true));
        assert!(parsed["content"].as_str().unwrap().contains("[truncated]"));
        assert!(compacted.1.is_some());
    }

    #[test]
    fn compacts_large_match_arrays() {
        let matches: Vec<Value> = (0..1000)
            .map(|i| serde_json::json!({ "line": i }))
            .collect();
        let output = serde_json::json!({ "matches": matches, "count": 1000 }).to_string();
        let compacted = compact_tool_output("grep", &output);
        let parsed: Value = serde_json::from_str(&compacted.0).unwrap();
        assert_eq!(parsed["compacted"].as_bool(), Some(true));
        assert_eq!(parsed["matches"].as_array().unwrap().len(), 50);
    }

    #[test]
    fn updated_tool_input_accepts_direct_object_or_tool_input_field() {
        let direct = serde_json::json!({ "command": "cargo test" });
        assert_eq!(updated_tool_input_from_hook(&direct), Some(direct));
        assert_eq!(
            updated_tool_input_from_hook(&serde_json::json!({
                "tool_input": { "command": "cargo check" }
            })),
            Some(serde_json::json!({ "command": "cargo check" }))
        );
        assert_eq!(
            updated_tool_input_from_hook(&serde_json::json!("nope")),
            None
        );
    }

    #[test]
    fn hook_outcome_context_includes_additional_context_and_feedback() {
        let mut outcome = crate::hooks::HookOutcome::default();
        outcome.additional_context.push("extra context".into());
        outcome.notices.push(crate::hooks::HookNotice {
            event: crate::hooks::HookEventName::PostToolUse,
            hook_key: Some("managed:test:PostToolUse:0:0".into()),
            level: crate::hooks::HookNoticeLevel::Feedback,
            message: "try again".into(),
        });

        let messages = hook_context_messages(HookEventName::PostToolUse, &outcome);

        assert_eq!(messages.len(), 2);
        assert!(messages[0].contains("extra context"));
        assert!(messages[1].contains("try again"));
    }

    #[test]
    fn hook_context_appends_single_deduped_user_message() {
        let mut messages = vec![ChatMessage::Assistant {
            content: Some("done".into()),
            tool_calls: Vec::new(),
        }];

        append_hook_context_messages(
            &mut messages,
            vec![
                "extra context".into(),
                "extra context".into(),
                "PostToolUse hook feedback: try again".into(),
            ],
        );

        assert_eq!(messages.len(), 2);
        match messages.last().unwrap() {
            ChatMessage::User { content } => {
                let text = content.as_text();
                assert!(text.contains("Additional context from hooks:"));
                assert_eq!(text.matches("extra context").count(), 1);
                assert!(text.contains("PostToolUse hook feedback: try again"));
            }
            other => panic!("expected user hook context message, got {other:?}"),
        }
    }

    #[test]
    fn hook_context_appending_caps_aggregate_context() {
        let mut messages = Vec::new();
        let contexts = (0..20)
            .map(|idx| format!("context-{idx} {}", "x".repeat(2_000)))
            .collect();

        append_hook_context_messages(&mut messages, contexts);

        match messages.last().unwrap() {
            ChatMessage::User { content } => {
                let text = content.as_text();
                assert!(text.contains("[truncated]"));
                assert!(text.len() < 9_000);
            }
            other => panic!("expected user hook context message, got {other:?}"),
        }
    }
}
