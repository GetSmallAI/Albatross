//! Session command group: /new, /session, /sessions, /resume, /export, /path, /paths, /undo.
//! Split out of mod.rs; dispatch lives in mod.rs.

use super::*;

fn fmt_tokens(n: u32) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f32 / 1000.0)
    } else {
        n.to_string()
    }
}
pub(super) fn cmd_new(state: &mut AppState) {
    if state.play_session.is_some() {
        let _ = restore_play_session(state);
    }
    state.messages.clear();
    state.conversation_summary = None;
    state.context_guard_notice = None;
    state.checkpoint_stack =
        crate::turn_checkpoint::CheckpointStack::new(state.config.checkpoints.limits());
    state.total_in = 0;
    state.total_out = 0;
    state.session_usd = 0.0;
    state.session_cost_has_unknown = false;
    state.trace_enabled = false;
    state.renderer.set_trace(false);
    state.reset_session();
    let _ = state.reset_trace_for_session();
    if let Ok(mut trace) = state.trace.lock() {
        trace.begin_turn();
    }
    println!("  {GREEN}✓{RESET} {DIM}New session started.{RESET}");
}

fn ensure_path_ops_allowed(state: &AppState) -> Result<()> {
    if state.in_play_session() {
        anyhow::bail!("cannot use /path during a /play session — /play exit first");
    }
    Ok(())
}

pub(super) fn cmd_paths(state: &AppState) -> Result<()> {
    if !state.paths_enabled() {
        println!("  {DIM}session paths are disabled in config{RESET}");
        return Ok(());
    }
    let active = state.path_store.active_id();
    if state.path_store.registry.paths.is_empty() {
        println!(
            "  {DIM}path{RESET}           {CYAN}{active}{RESET} {DIM}(only path — /path fork to branch){RESET}"
        );
        return Ok(());
    }
    println!(
        "  {DIM}active{RESET}         {CYAN}{}{RESET} · {} path(s) · {} stored",
        active,
        state.path_store.path_count(),
        format_bytes(state.path_store.total_storage_bytes() as usize)
    );
    for record in &state.path_store.registry.paths {
        let marker = if record.id == active {
            format!("{GREEN}*{RESET} ")
        } else {
            "  ".to_string()
        };
        println!(
            "  {marker}{CYAN}{}{RESET} {DIM}msgs={} files={} updated={}{RESET}",
            record.id, record.message_count, record.file_count, record.updated_at
        );
    }
    Ok(())
}

pub(super) async fn cmd_path(args: &str, state: &mut AppState) -> Result<()> {
    let trimmed = args.trim();
    if trimmed.is_empty() || trimmed == "status" {
        cmd_path_status(state);
        return Ok(());
    }
    let mut parts = trimmed.splitn(2, ' ');
    let sub = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();
    match sub {
        "fork" => {
            ensure_path_ops_allowed(state)?;
            let name = if rest.is_empty() { None } else { Some(rest) };
            let root = state.workspace_root();
            let session_path = state.session_path.clone();
            let current = PathStore::capture_state(state, &root)?;
            let (new_id, new_state) = state.path_store.fork(current, &session_path, name, &root)?;
            let transcript = state.path_store.transcript_path(&new_id);
            apply_path_session_state(state, &new_state, &transcript);
            let _ = state.save_active_path_metadata();
            let parent = state
                .path_store
                .registry
                .paths
                .iter()
                .find(|p| p.id == new_id)
                .and_then(|p| p.parent_id.clone())
                .unwrap_or_else(|| DEFAULT_PATH_ID.to_string());
            let notice = format!(
                "Forked to path '{new_id}' from '{parent}' at message {}.",
                state.messages.len()
            );
            state.messages.push(ChatMessage::System {
                content: notice.clone(),
            });
            println!(
                "  {GREEN}✓{RESET} {DIM}forked to path{RESET} {CYAN}{new_id}{RESET} {DIM}— continue here, /path switch to compare{RESET}"
            );
        }
        "switch" => {
            ensure_path_ops_allowed(state)?;
            if rest.is_empty() {
                println!("  {DIM}Usage: /path switch <name>{RESET}");
                return Ok(());
            }
            let root = state.workspace_root();
            let current = PathStore::capture_state(state, &root)?;
            let (path_state, report) = state.path_store.switch_to(rest, current, &root)?;
            let transcript = state
                .path_store
                .transcript_path(state.path_store.active_id());
            apply_path_session_state(state, &path_state, &transcript);
            let _ = state.save_active_path_metadata();
            println!(
                "  {GREEN}✓{RESET} {DIM}switched to path{RESET} {CYAN}{}{RESET}",
                state.path_store.active_id()
            );
            if !report.restored.is_empty() || !report.removed.is_empty() {
                println!(
                    "  {DIM}restored {} · removed {}{RESET}",
                    report.restored.len(),
                    report.removed.len()
                );
            }
            if report.is_partial() {
                println!(
                    "  {YELLOW}!{RESET} {DIM}partial restore — {} skipped, {} errors{RESET}",
                    report.skipped.len(),
                    report.errors.len()
                );
            }
        }
        "diff" => {
            if rest.is_empty() {
                println!("  {DIM}Usage: /path diff <name>{RESET}");
                return Ok(());
            }
            let diff = state.path_store.diff_with(rest, &state.workspace_root())?;
            if diff.is_empty() {
                println!("  {DIM}No file differences vs path `{rest}`.{RESET}");
            } else {
                crate::diff_view::print_diff(&diff, 120);
            }
        }
        "pick" => {
            ensure_path_ops_allowed(state)?;
            let mut name = rest;
            let mut dry_run = false;
            if name.starts_with("--dry-run") {
                dry_run = true;
                name = name.strip_prefix("--dry-run").unwrap_or("").trim();
            }
            if name.is_empty() {
                println!("  {DIM}Usage: /path pick <name> [--dry-run]{RESET}");
                return Ok(());
            }
            let preview = state
                .path_store
                .pick_from(name, &state.workspace_root(), true)?;
            if preview.files.is_empty() {
                println!("  {DIM}Nothing to pick from path `{name}`.{RESET}");
                return Ok(());
            }
            if dry_run {
                println!(
                    "  {DIM}dry-run would apply {} file(s): {}{RESET}",
                    preview.files.len(),
                    preview.files.join(", ")
                );
                return Ok(());
            }
            let diff = state.path_store.diff_with(name, &state.workspace_root())?;
            if !diff.is_empty() {
                println!();
                crate::diff_view::print_diff(&diff, 80);
                println!();
            }
            println!(
                "  {YELLOW}?{RESET} {DIM}Apply {} file(s) from `{name}`? [y/n]{RESET}",
                preview.files.len()
            );
            let answer = plain_read_line(format!("  {YELLOW}? {RESET}")).await?;
            if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
                println!("  {RED}✗{RESET} {DIM}pick cancelled{RESET}");
                return Ok(());
            }
            let result = state
                .path_store
                .pick_from(name, &state.workspace_root(), false)?;
            if result.applied {
                state.path_store.mark_dirty();
                println!(
                    "  {GREEN}✓{RESET} {DIM}picked {} file(s) from `{name}`{RESET}",
                    result.files.len()
                );
            } else if !result.errors.is_empty() {
                println!(
                    "  {RED}✗{RESET} {DIM}pick failed: {}{RESET}",
                    result.errors.join("; ")
                );
            }
        }
        "drop" => {
            ensure_path_ops_allowed(state)?;
            if rest.is_empty() {
                println!("  {DIM}Usage: /path drop <name>{RESET}");
                return Ok(());
            }
            state.path_store.drop_path(rest)?;
            println!("  {GREEN}✓{RESET} {DIM}dropped path `{rest}`{RESET}");
        }
        other => {
            println!(
                "  {DIM}Usage: /path [fork [name] | switch <name> | diff <name> | pick <name> [--dry-run] | drop <name> | status]{RESET} (unknown: {other})"
            );
        }
    }
    Ok(())
}

pub(super) fn cmd_path_status(state: &AppState) {
    if !state.paths_enabled() {
        println!("  {DIM}paths{RESET}           disabled in config");
        return;
    }
    let count = state.path_store.path_count();
    println!(
        "  {DIM}path{RESET}           {CYAN}{}{RESET}",
        state.path_store.active_id()
    );
    if count > 1 {
        println!(
            "  {DIM}paths{RESET}          {count} · {} stored",
            format_bytes(state.path_store.total_storage_bytes() as usize)
        );
    } else {
        println!(
            "  {DIM}paths{RESET}          1 {DIM}(/path fork to try an alternate approach){RESET}"
        );
    }
}
pub(super) fn cmd_session(args: &str, state: &mut AppState) -> Result<()> {
    let trimmed = args.trim();
    if let Some(title) = trimmed.strip_prefix("title ") {
        set_session_title(&state.session_path, title)?;
        println!(
            "  {GREEN}✓{RESET} {DIM}session title →{RESET} {CYAN}{}{RESET}",
            title.trim()
        );
        return Ok(());
    }
    if !trimmed.is_empty() {
        println!("  {DIM}Usage: /session [title <text>]{RESET}");
        return Ok(());
    }
    println!(
        "  {DIM}mode{RESET}      {CYAN}{}{RESET}",
        state.config.mode.as_str()
    );
    println!(
        "  {DIM}provider{RESET}  {CYAN}{}{RESET}",
        state.config.backend.as_str()
    );
    println!("  {DIM}model{RESET}     {CYAN}{}{RESET}", state.model);
    if let Some(effort) = state.active_effort {
        println!("  {DIM}effort{RESET}    {CYAN}{}{RESET}", effort.as_str());
    }
    println!(
        "  {DIM}approval{RESET}  {CYAN}{}{RESET}",
        state.config.approval_policy.as_str()
    );
    println!("  {DIM}session{RESET}   {}", state.session_path.display());
    println!("  {DIM}messages{RESET}  {}", state.messages.len());
    println!(
        "  {DIM}tokens{RESET}    {} in · {} out",
        fmt_tokens(state.total_in),
        fmt_tokens(state.total_out)
    );
    if state.session_usd > 0.0 || state.session_cost_has_unknown {
        let prefix = if state.session_cost_has_unknown {
            "≥"
        } else {
            ""
        };
        println!(
            "  {DIM}cost{RESET}      {prefix}{} (sum of known-price turns)",
            catalog::format_usd(state.session_usd)
        );
    }
    Ok(())
}
pub(super) fn cmd_sessions(args: &str, state: &AppState) -> Result<()> {
    let args = args.trim();
    if let Some(query) = args.strip_prefix("search ") {
        let hits = search_sessions(&state.session_dir, query)?;
        if hits.is_empty() {
            println!("  {DIM}No sessions matched `{}`.{RESET}", query.trim());
            return Ok(());
        }
        for hit in hits.into_iter().take(20) {
            println!(
                "  {CYAN}{}{RESET} {DIM}{} match(es) · {} · {}{RESET}",
                hit.summary.id,
                hit.matches,
                hit.summary.title.as_deref().unwrap_or("untitled"),
                hit.preview
            );
        }
        return Ok(());
    }
    if let Some(id) = args.strip_prefix("delete ") {
        let mut parts: Vec<&str> = id.split_whitespace().collect();
        let confirmed = parts.iter().any(|part| *part == "--yes" || *part == "yes");
        parts.retain(|part| *part != "--yes" && *part != "yes");
        let id = parts.join(" ");
        if id.is_empty() {
            println!("  {DIM}Usage: /sessions delete <id> --yes{RESET}");
            return Ok(());
        }
        if !confirmed {
            println!("  {YELLOW}!{RESET} {DIM}Confirm with /sessions delete {id} --yes{RESET}");
            return Ok(());
        }
        match delete_session(&state.session_dir, &id)? {
            Some(path) => println!(
                "  {GREEN}✓{RESET} {DIM}deleted session {}{RESET}",
                path.display()
            ),
            None => println!("  {YELLOW}!{RESET} {DIM}session not found: {id}{RESET}"),
        }
        return Ok(());
    }
    if args.starts_with("prune") {
        let confirmed = args
            .split_whitespace()
            .any(|part| part == "--yes" || part == "yes");
        if !confirmed {
            println!("  {YELLOW}!{RESET} {DIM}Confirm with /sessions prune --yes (keeps 20 newest sessions).{RESET}");
            return Ok(());
        }
        let sessions = list_sessions(&state.session_dir)?;
        let mut removed = 0usize;
        for session in sessions.into_iter().skip(20) {
            if delete_session(&state.session_dir, &session.id)?.is_some() {
                removed += 1;
            }
        }
        println!("  {GREEN}✓{RESET} {DIM}pruned {removed} old session(s).{RESET}");
        return Ok(());
    }
    if !args.is_empty() {
        println!("  {DIM}Usage: /sessions [search <query>|delete <id> --yes|prune --yes]{RESET}");
        return Ok(());
    }
    let sessions = list_sessions(&state.session_dir)?;
    if sessions.is_empty() {
        println!("  {DIM}No sessions saved yet.{RESET}");
        return Ok(());
    }
    for session in sessions.into_iter().take(20) {
        println!(
            "  {CYAN}{}{RESET} {DIM}{} messages · {} bytes · {} · {}{RESET}",
            session.id,
            session.messages,
            session.bytes,
            format_system_time(session.modified),
            session.title.as_deref().unwrap_or("untitled")
        );
    }
    Ok(())
}

pub(super) fn cmd_resume(args: &str, state: &mut AppState) -> Result<()> {
    let id = if args.is_empty() { "latest" } else { args };
    let Some(path) = resolve_session_path(&state.session_dir, id)? else {
        println!("  {RED}✗{RESET} {DIM}Session not found: {id}{RESET}");
        return Ok(());
    };
    let messages = load_messages(&path)?;
    state.messages = messages;
    state.session_path = path.clone();
    let _ = state.reset_trace_for_session();
    state.path_store =
        PathStore::load(&state.session_dir, &state.session_path, &state.config.paths);
    let metadata = load_session_metadata(&path)?;
    let root = state.workspace_root();
    if let Some((path_state, report)) = state
        .path_store
        .load_resume_state(&root, metadata.active_path_id.as_deref())?
    {
        let transcript = state
            .path_store
            .transcript_path(state.path_store.active_id());
        apply_path_session_state(state, &path_state, &transcript);
        if report.is_partial() {
            println!(
                "  {YELLOW}!{RESET} {DIM}path restore partial — {} skipped, {} errors{RESET}",
                report.skipped.len(),
                report.errors.len()
            );
        }
    }
    let mut updated = metadata.clone();
    updated.active_path_id = Some(state.path_store.active_id().to_string());
    let _ = save_session_metadata(&path, &updated);
    state.conversation_summary = state.messages.first().and_then(|message| match message {
        ChatMessage::System { content } => extract_conversation_summary(content),
        _ => None,
    });
    println!(
        "  {GREEN}✓{RESET} {DIM}resumed{RESET} {CYAN}{}{RESET} {DIM}({} messages){RESET}",
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session"),
        state.messages.len()
    );
    Ok(())
}

fn messages_to_entries(messages: &[ChatMessage]) -> Vec<SessionEntry> {
    messages
        .iter()
        .cloned()
        .map(|message| SessionEntry {
            timestamp: Utc::now().to_rfc3339(),
            message,
        })
        .collect()
}

pub(super) fn cmd_export(args: &str, state: &AppState) -> Result<()> {
    let mut parts = args.split_whitespace();
    let target = parts.next().unwrap_or("current");
    let format = parts.next().unwrap_or("markdown");
    let explicit_path = parts.next();
    let (entries, id) = if target == "current" {
        let entries = if state.session_path.exists() {
            load_session(&state.session_path)
                .unwrap_or_else(|_| messages_to_entries(&state.messages))
        } else {
            messages_to_entries(&state.messages)
        };
        let id = state
            .session_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("current")
            .to_string();
        (entries, id)
    } else {
        let Some(path) = resolve_session_path(&state.session_dir, target)? else {
            println!("  {RED}✗{RESET} {DIM}Session not found: {target}{RESET}");
            return Ok(());
        };
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session")
            .to_string();
        (load_session(&path)?, id)
    };
    if format == "events" {
        let session_path = if target == "current" {
            state.session_path.clone()
        } else {
            resolve_session_path(&state.session_dir, target)?
                .ok_or_else(|| anyhow::anyhow!("session not found: {target}"))?
        };
        let events_src = crate::turn_trace::events_path_for_session(&session_path);
        let out_path = explicit_path
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(&state.session_dir).join(format!("{id}.events.jsonl")));
        if !events_src.exists() {
            println!(
                "  {RED}✗{RESET} {DIM}no event log at {}{RESET}",
                events_src.display()
            );
            return Ok(());
        }
        fs::copy(&events_src, &out_path)?;
        println!(
            "  {GREEN}✓{RESET} {DIM}exported events →{RESET} {}",
            out_path.display()
        );
        return Ok(());
    }
    let ext = if format == "json" { "json" } else { "md" };
    let out_path = explicit_path
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&state.session_dir).join(format!("{id}.{ext}")));
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let body = if format == "json" {
        serde_json::to_string_pretty(&entries)?
    } else {
        render_markdown(&entries)
    };
    fs::write(&out_path, body)?;
    println!(
        "  {GREEN}✓{RESET} {DIM}exported →{RESET} {}",
        out_path.display()
    );
    Ok(())
}
pub(super) fn cmd_undo(args: &str, state: &mut AppState) -> Result<()> {
    let parts: Vec<&str> = args.split_whitespace().collect();
    if parts.first() == Some(&"list") {
        if state.checkpoint_stack.is_empty() {
            println!("  {DIM}No checkpoints to undo.{RESET}");
            return Ok(());
        }
        println!(
            "  {DIM}Checkpoint stack ({}){RESET}",
            state.checkpoint_stack.len()
        );
        for (idx, cp) in state.checkpoint_stack.checkpoints.iter().rev().enumerate() {
            println!(
                "  {CYAN}{idx}.{RESET} {DIM}{}{RESET} · {} file(s) · skipped {}",
                cp.created_at,
                cp.file_count(),
                cp.skipped.len()
            );
        }
        return Ok(());
    }

    let Some(checkpoint) = state.checkpoint_stack.pop() else {
        println!("  {DIM}Nothing to undo — no checkpoint from a prior mutating turn.{RESET}");
        return Ok(());
    };

    let workspace = Path::new(&state.config.workspace_root);
    let report = crate::turn_checkpoint::restore_checkpoint(&checkpoint, workspace);
    println!("  {GREEN}✓{RESET} {DIM}undo {}{RESET}", checkpoint.id);
    if !report.restored.is_empty() {
        println!(
            "  {DIM}restored {} file(s): {}{RESET}",
            report.restored.len(),
            report.restored.join(", ")
        );
    }
    if !report.removed.is_empty() {
        println!(
            "  {DIM}removed {} created file(s): {}{RESET}",
            report.removed.len(),
            report.removed.join(", ")
        );
    }
    if report.is_partial() {
        println!("  {YELLOW}!{RESET} {DIM}partial undo — some paths were skipped or failed{RESET}");
        if !report.skipped.is_empty() {
            println!("  {DIM}skipped: {}{RESET}", report.skipped.join(", "));
        }
        for err in &report.errors {
            println!("  {RED}✗{RESET} {DIM}{err}{RESET}");
        }
    }
    Ok(())
}
fn format_system_time(t: SystemTime) -> String {
    let dt: chrono::DateTime<Utc> = t.into();
    dt.format("%Y-%m-%d %H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OperatorMode;
    use crate::session_paths::{PathStore, DEFAULT_PATH_ID};
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
            active_route: None,
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
    fn undo_restores_mutated_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("note.txt"), "before\n").unwrap();
        let mut state = test_state(dir.path());
        let mut checkpoint = crate::turn_checkpoint::TurnCheckpoint::new();
        crate::turn_checkpoint::snapshot_file_into(
            &mut checkpoint,
            dir.path(),
            "note.txt",
            state.config.checkpoints.limits(),
        )
        .unwrap();
        fs::write(dir.path().join("note.txt"), "after\n").unwrap();
        state.checkpoint_stack.push(checkpoint);

        cmd_undo("", &mut state).unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join("note.txt")).unwrap(),
            "before\n"
        );
        assert!(state.checkpoint_stack.is_empty());
    }

    #[test]
    fn new_restores_play_session_and_clears_session_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        let original_root = state.config.workspace_root.clone();
        let original_mode = state.config.mode;
        let sandbox = dir.path().join("sandbox");
        state.play_session = Some(crate::app_state::PlaySession {
            fixture_id: "demo".into(),
            sandbox_root: sandbox.clone(),
            restore: crate::app_state::PlayRestoreSnapshot {
                config: state.config.clone(),
                checkpoints_enabled: true,
            },
        });
        state.config.workspace_root = sandbox.display().to_string();
        state.config.apply_operator_mode(OperatorMode::Ship);
        state.messages.push(crate::openai::ChatMessage::User {
            content: "hello".into(),
        });
        state.conversation_summary = Some("summary".into());
        state.context_guard_notice = Some("notice".into());
        state
            .checkpoint_stack
            .push(crate::turn_checkpoint::TurnCheckpoint::new());

        cmd_new(&mut state);

        assert_eq!(state.config.workspace_root, original_root);
        assert_eq!(state.config.mode, original_mode);
        assert!(state.play_session.is_none());
        assert!(state.messages.is_empty());
        assert!(state.conversation_summary.is_none());
        assert!(state.context_guard_notice.is_none());
        assert!(state.checkpoint_stack.is_empty());
    }

    #[test]
    fn path_fork_refuses_during_play_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        state.play_session = Some(crate::app_state::PlaySession {
            fixture_id: "demo".into(),
            sandbox_root: dir.path().join("sandbox"),
            restore: crate::app_state::PlayRestoreSnapshot {
                config: state.config.clone(),
                checkpoints_enabled: true,
            },
        });
        let err = ensure_path_ops_allowed(&state).unwrap_err();
        assert!(err.to_string().contains("/play"));
    }

    #[test]
    fn new_resets_path_store_for_fresh_session() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha.txt"), "main\n").unwrap();
        let mut state = test_state(dir.path());
        let root = state.workspace_root();
        let session_path = state.session_path.clone();
        let current = PathStore::capture_state(&state, &root).unwrap();
        state
            .path_store
            .fork(current, &session_path, Some("plan-a"), &root)
            .unwrap();
        assert!(state.path_store.path_count() >= 2);

        cmd_new(&mut state);

        assert_eq!(state.path_store.active_id(), DEFAULT_PATH_ID);
        assert_eq!(state.path_store.path_count(), 1);
        assert!(state.path_store.registry.paths.is_empty());
    }
}
