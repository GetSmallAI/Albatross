//! Context command group: /context, /compact, /reset, /checkpoints.
//! Split out of mod.rs; dispatch lives in mod.rs.

use super::*;

struct ResetArgs {
    dry_run: bool,
    allow_cloud: bool,
}

fn parse_reset_args(args: &str) -> Option<ResetArgs> {
    let mut dry_run = false;
    let mut allow_cloud = false;
    for part in args.split_whitespace() {
        match part {
            "--dry-run" => dry_run = true,
            "--cloud" => allow_cloud = true,
            _ => return None,
        }
    }
    Some(ResetArgs {
        dry_run,
        allow_cloud,
    })
}

/// Reset the context window the article's way: draft a continuation artifact,
/// write it to `.albatross/continue.md`, then start a fresh session seeded
/// with only that artifact. Unlike `/compact` (in-place summary, same session),
/// this is a clean slate carrying an explicit handoff.
pub(super) async fn cmd_reset(args: &str, state: &mut AppState) -> Result<()> {
    let Some(args) = parse_reset_args(args) else {
        println!("  {DIM}Usage: /reset [--dry-run] [--cloud]{RESET}");
        return Ok(());
    };

    // Nothing to hand off if there's no real conversation yet.
    let has_conversation = state
        .messages
        .iter()
        .any(|m| !matches!(m, ChatMessage::System { .. }));
    if !has_conversation {
        println!("  {DIM}Nothing to reset: the conversation is empty.{RESET}");
        return Ok(());
    }

    if should_refuse_cloud_handoff(state.backend.name, args.allow_cloud) {
        println!(
            "  {RED}✗{RESET} {DIM}/reset will not send the conversation to a cloud backend unless you pass --cloud.{RESET}"
        );
        return Ok(());
    }

    perform_reset(state, args.dry_run).await
}

/// The shared `/reset` recipe: draft a continuation artifact from the live
/// conversation, write it to `.albatross/continue.md`, then (unless
/// `dry_run`) clear the session and seed a fresh one with only that artifact.
///
/// Extracted so `/auto` can drive the same reset between rounds. Callers are
/// responsible for the cloud-handoff refusal and the "nothing to reset" guard
/// before invoking this — it assumes there is a conversation worth handing off.
pub(crate) async fn perform_reset(state: &mut AppState, dry_run: bool) -> Result<()> {
    println!(
        "  {DIM}drafting continuation with {} · {}{RESET}",
        state.config.backend.as_str(),
        state.model
    );

    let messages = vec![
        ChatMessage::System {
            content: continuation_system_prompt(),
        },
        ChatMessage::User {
            content: render_continuation_prompt(
                &state.messages,
                state.conversation_summary.as_deref(),
            )
            .into(),
        },
    ];
    let req = ChatRequest {
        model: &state.model,
        messages: &messages,
        tools: None,
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: false,
        }),
        max_tokens: Some(1200),
        prompt_cache_key: None,
        effort: None,
    };
    let mut draft = String::new();
    let result = stream_chat(&state.http, &state.backend, &req, None, |chunk| {
        if let Some(choice) = chunk.choices.first() {
            if let Some(content) = &choice.delta.content {
                draft.push_str(content);
            }
        }
    })
    .await;

    // Build the artifact fully before any teardown — it borrows state.messages,
    // which cmd_new clears.
    let body = match result {
        Ok(_) if !draft.trim().is_empty() => ensure_continuation_sections(&draft),
        Ok(_) => render_fallback_continuation(&state.messages, Some("empty model response")),
        Err(e) => render_fallback_continuation(&state.messages, Some(&e.to_string())),
    };

    // Never tear down the session around an empty artifact.
    if body.trim().is_empty() {
        println!(
            "  {RED}✗{RESET} {DIM}continuation came back empty — leaving the conversation intact.{RESET}"
        );
        return Ok(());
    }

    let out_path = default_continuation_path(&state.config.workspace_root);
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(&out_path, &body)?;
    println!();
    print!("{body}");
    println!(
        "  {GREEN}✓{RESET} {DIM}continuation written →{RESET} {}",
        out_path.display()
    );

    if dry_run {
        println!("  {DIM}dry run — conversation left intact.{RESET}");
        return Ok(());
    }

    // Clean slate (the exact /new recipe), then seed the new session with only
    // the artifact so the next turn picks up from the handoff.
    super::session::cmd_new(state);
    let seed = ChatMessage::User {
        content: format!(
            "Continuing from a previous session. Here is the handoff state to resume from:\n\n{body}"
        )
        .into(),
    };
    state.messages.push(seed.clone());
    // The session dir exists in normal runs (init_session_dir at startup), but
    // ensure it before persisting the seed so a fresh path can't fail to write.
    if let Some(parent) = state.session_path.parent() {
        fs::create_dir_all(parent)?;
    }
    save_message(&state.session_path, &seed)?;
    println!("  {GREEN}✓{RESET} {DIM}new session seeded with continuation context{RESET}");
    Ok(())
}
fn transcript_bytes(messages: &[ChatMessage]) -> usize {
    serde_json::to_vec(messages).map(|v| v.len()).unwrap_or(0)
}

pub(super) fn cmd_context(args: &str, state: &mut AppState) {
    if !args.is_empty() {
        for part in args.split_whitespace() {
            if let Some(value) = part.strip_prefix("maxMessages=") {
                if let Ok(n) = value.parse::<usize>() {
                    state.config.context.max_messages = Some(n);
                }
            } else if let Some(value) = part.strip_prefix("maxBytes=") {
                if let Ok(n) = value.parse::<usize>() {
                    state.config.context.max_bytes = Some(n);
                }
            } else if let Some(value) = part.strip_prefix("modelTokens=") {
                if let Ok(n) = value.parse::<usize>() {
                    state.config.context.model_context_tokens = Some(n);
                }
            } else if let Some(value) = part.strip_prefix("autoCompact=") {
                state.config.context.auto_compact = Some(matches!(value, "on" | "true" | "1"));
            } else if let Some(value) = part.strip_prefix("compactThreshold=") {
                if let Ok(n) = value.parse::<f64>() {
                    state.config.context.compact_threshold = n.clamp(0.5, 0.99);
                }
            } else if let Some(value) = part.strip_prefix("reserveRatio=") {
                if let Ok(n) = value.parse::<f64>() {
                    state.config.context.reserve_ratio = n.clamp(0.05, 0.5);
                }
            }
        }
    }
    let last_prompt = last_user_prompt(state).unwrap_or_default();
    let active_tool_names = select_tool_names(&state.config, &last_prompt);
    let base_system_prompt = render_system_prompt_with_memory(
        &state.config,
        &state.backend,
        &active_tool_names,
        &last_prompt,
    );
    let system_prompt =
        merge_system_prompt(&base_system_prompt, state.conversation_summary.as_deref());
    let tools = build_tools_for_names(&state.config, &active_tool_names, None);
    let tool_defs = to_openai_tools(&tools);
    let budget = measure_prompt_budget(&system_prompt, &state.messages, &tool_defs);
    println!("  {DIM}messages{RESET}  {}", state.messages.len());
    println!(
        "  {DIM}mode{RESET}      {}",
        state.config.tool_selection.as_str()
    );
    println!(
        "  {DIM}tools{RESET}     {}",
        if active_tool_names.is_empty() {
            "none".to_string()
        } else {
            active_tool_names.join(", ")
        }
    );
    println!(
        "  {DIM}bytes{RESET}     {}",
        transcript_bytes(&state.messages)
    );
    println!(
        "  {DIM}budget{RESET}    total={} (~{} tokens)",
        format_bytes(budget.effective_total_bytes),
        budget.estimated_tokens
    );
    println!(
        "  {DIM}breakdown{RESET} system={} transcript={} toolSchemas={} toolResults={}",
        format_bytes(budget.system_bytes),
        format_bytes(budget.transcript_bytes),
        format_bytes(budget.tool_schema_bytes),
        format_bytes(budget.tool_result_bytes)
    );
    println!(
        "  {DIM}limits{RESET}    maxMessages={:?} maxBytes={:?} modelTokens={:?}",
        state.config.context.max_messages,
        state.config.context.max_bytes,
        state.config.context.model_context_tokens
    );
    for line in context_status_lines(
        &state.config,
        &state.model,
        state.backend.is_local,
        &budget,
        state.context_guard_notice.as_deref(),
        state.conversation_summary.as_deref(),
    ) {
        println!("{line}");
    }
}
pub(super) async fn cmd_compact(args: &str, state: &mut AppState) -> Result<()> {
    let keep = if args.is_empty() {
        None
    } else {
        Some(args.parse::<usize>().unwrap_or(12).clamp(4, 80))
    };
    let last_prompt = last_user_prompt(state).unwrap_or_default();
    let active_tool_names = select_tool_names(&state.config, &last_prompt);
    let base_system_prompt = render_system_prompt_with_memory(
        &state.config,
        &state.backend,
        &active_tool_names,
        &last_prompt,
    );
    let tools = build_tools_for_names(&state.config, &active_tool_names, None);
    let tool_defs = to_openai_tools(&tools);

    println!("  {DIM}Compacting older messages…{RESET}");
    let mut compact_ctx = CompactSessionContext {
        messages: &mut state.messages,
        system_prompt: &base_system_prompt,
        tool_defs: &tool_defs,
        config: &state.config,
        model: &state.model,
        is_local: state.backend.is_local,
        http: &state.http,
        backend: &state.backend,
        conversation_summary: state.conversation_summary.as_deref(),
    };
    let result = compact_session(
        &mut compact_ctx,
        &state.session_dir,
        &mut state.session_path,
        keep,
    )
    .await?;

    if !result.compacted {
        println!("  {DIM}Nothing to compact yet.{RESET}");
        return Ok(());
    }

    if let Some(summary) = result.conversation_summary {
        state.conversation_summary = Some(summary);
    }

    let method = match result.method {
        CompactMethod::LlmSummary => "summarized",
        CompactMethod::DeterministicTrim => "trimmed",
        CompactMethod::None => "compacted",
    };
    state.context_guard_notice = Some(format!(
        "Compacted {} messages → {} ({method}), budget {:.0}% → {:.0}%",
        result.before_messages,
        result.after_messages,
        result.before_ratio * 100.0,
        result.after_ratio * 100.0
    ));
    println!(
        "  {GREEN}✓{RESET} {DIM}{}{RESET}",
        state.context_guard_notice.as_deref().unwrap_or("")
    );
    if let Some(warning) = result.warning {
        println!("  {YELLOW}!{RESET} {DIM}{warning}{RESET}");
    }
    println!("  {DIM}session → {}{RESET}", state.session_path.display());
    Ok(())
}

pub(crate) fn last_user_prompt(state: &AppState) -> Option<String> {
    state
        .messages
        .iter()
        .rev()
        .find_map(|m| m.user_text().map(|s| s.into_owned()))
}
pub(super) fn cmd_checkpoints(args: &str, state: &mut AppState) {
    let arg = args.trim();
    match arg {
        "on" => {
            state.checkpoints_enabled = true;
            println!("  {GREEN}✓{RESET} {DIM}turn checkpoints enabled for this session{RESET}");
        }
        "off" => {
            state.checkpoints_enabled = false;
            println!("  {GREEN}✓{RESET} {DIM}turn checkpoints disabled for this session{RESET}");
        }
        "status" | "" => {
            println!(
                "  {DIM}checkpoints{RESET}  config={} session={} stack={}/{} maxFileBytes={}",
                state.config.checkpoints.enabled,
                state.checkpoints_enabled,
                state.checkpoint_stack.len(),
                state.checkpoint_stack.limits.max_turns,
                state.config.checkpoints.max_file_bytes
            );
        }
        other => {
            println!("  {DIM}Usage: /checkpoints [on|off|status]{RESET} (unknown: {other})");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{backend, BackendDescriptor, BackendName};
    use crate::openai::ChatMessage;
    use std::fs;
    use std::path::Path;

    fn test_state(root: &Path) -> AppState {
        use crate::backends::backend;
        use crate::config::AgentConfig;
        use crate::session_paths::PathStore;
        let mut config = AgentConfig {
            workspace_root: root.display().to_string(),
            session_dir: root.join(".sessions").display().to_string(),
            ..Default::default()
        };
        config.project_memory.max_injected_bytes = 1024;
        config.paths.enabled = true;
        let session_path = root.join(".sessions/test.jsonl");
        AppState {
            http: reqwest::Client::new(),
            backend: backend(config.backend),
            model: "test-model".into(),
            active_effort: None,
            messages: Vec::new(),
            session_dir: config.session_dir.clone(),
            session_path,
            total_in: 0,
            total_out: 0,
            session_usd: 0.0,
            session_cost_has_unknown: false,
            context_guard_notice: None,
            conversation_summary: None,
            checkpoint_stack: crate::turn_checkpoint::CheckpointStack::new(
                config.checkpoints.limits(),
            ),
            checkpoints_enabled: config.checkpoints.enabled,
            play_session: None,
            last_play_scorecard: None,
            approval_cache: crate::approval::ApprovalCache::new(),
            renderer: crate::renderer::TuiRenderer::new(config.display.clone()),
            hooks: crate::hooks::HookRegistry::default(),
            session_hook_contexts: Vec::new(),
            pending_hook_contexts: Vec::new(),
            warmed_fingerprint: None,
            tests_ran_this_session: false,
            pending_image_attachments: Vec::new(),
            mcp_tools: Vec::new(),
            path_store: PathStore::new(
                &config.session_dir,
                &root.join(".sessions/test.jsonl"),
                &config.paths,
            ),
            trace: crate::turn_trace::test_trace_for(&root.join(".sessions/test.jsonl")),
            trace_enabled: false,
            config,
        }
    }

    #[test]
    fn parse_reset_args_variants() {
        assert!(parse_reset_args("").is_some());
        let a = parse_reset_args("--dry-run --cloud").unwrap();
        assert!(a.dry_run && a.allow_cloud);
        assert!(parse_reset_args("--bogus").is_none());
    }

    fn dead_local_backend() -> BackendDescriptor {
        BackendDescriptor {
            name: BackendName::Ollama,
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: "test".into(),
            is_local: true,
            openrouter: crate::backends::OpenRouterConfig::default(),
        }
    }

    #[tokio::test]
    async fn reset_writes_artifact_and_seeds_new_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        state.backend = dead_local_backend();
        state.messages.push(ChatMessage::User {
            content: "make the parser robust".to_string().into(),
        });
        let old_path = state.session_path.clone();

        cmd_reset("", &mut state).await.unwrap();

        let body = fs::read_to_string(dir.path().join(".albatross/continue.md")).unwrap();
        for section in [
            "## Done",
            "## In Progress",
            "## Key Decisions",
            "## Next Steps",
            "## Key Files",
        ] {
            assert!(body.contains(section), "missing {section}");
        }
        assert!(body.contains("make the parser robust"));
        assert_eq!(state.messages.len(), 1);
        if let ChatMessage::User { content } = &state.messages[0] {
            assert!(content.as_text().contains("handoff state"));
        } else {
            panic!("seed should be a user message");
        }
        assert_ne!(state.session_path, old_path);
    }

    #[tokio::test]
    async fn reset_dry_run_keeps_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        state.backend = dead_local_backend();
        state.messages.push(ChatMessage::User {
            content: "x".to_string().into(),
        });
        let before = state.messages.len();
        let old_path = state.session_path.clone();

        cmd_reset("--dry-run", &mut state).await.unwrap();

        assert!(dir.path().join(".albatross/continue.md").exists());
        assert_eq!(state.messages.len(), before);
        assert_eq!(state.session_path, old_path);
    }

    #[tokio::test]
    async fn reset_noops_on_empty_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        cmd_reset("", &mut state).await.unwrap();
        assert!(!dir.path().join(".albatross/continue.md").exists());
        assert!(state.messages.is_empty());
    }

    #[tokio::test]
    async fn reset_refuses_cloud_without_flag() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        state.backend = backend(BackendName::Openrouter);
        state.messages.push(ChatMessage::User {
            content: "x".to_string().into(),
        });
        cmd_reset("", &mut state).await.unwrap();
        assert!(!dir.path().join(".albatross/continue.md").exists());
        assert_eq!(state.messages.len(), 1);
    }
}
