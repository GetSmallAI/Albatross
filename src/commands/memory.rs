//! Project memory command group: /index, /map, /memory, /remember, /forget.
//! Split out of mod.rs; dispatch lives in mod.rs.

use super::*;

pub(super) fn cmd_index(args: &str, state: &AppState) -> Result<()> {
    let arg = args.trim();
    match arg {
        "" => {
            let status = project_memory_status(&state.config)?;
            if status.exists {
                print_project_memory_status(&status);
            } else {
                let index = build_project_index(&state.config)?;
                print_index_built(&index);
            }
        }
        "status" => {
            let status = project_memory_status(&state.config)?;
            print_project_memory_status(&status);
            if status.exists {
                let freshness = project_index_freshness(&state.config)?;
                print_project_index_freshness(&freshness);
            }
        }
        "refresh" | "refresh changed" | "changed" => {
            let index = refresh_changed_project_index(&state.config)?;
            print_index_built(&index);
        }
        "clear" => {
            if clear_project_index(&state.config)? {
                println!("  {GREEN}✓{RESET} {DIM}project memory index cleared.{RESET}");
            } else {
                println!("  {DIM}No project memory index to clear.{RESET}");
            }
        }
        other => println!(
            "  {DIM}Usage: /index [refresh|refresh changed|status|clear] (got {other}){RESET}"
        ),
    }
    Ok(())
}

fn print_index_built(index: &crate::project_memory::ProjectIndex) {
    println!(
        "  {GREEN}✓{RESET} {DIM}indexed {} files under {}{RESET}",
        index.files.len(),
        index.workspace_root
    );
    println!(
        "  {DIM}skipped{RESET} ignored={} oversized={} binary={} outside={} errors={}",
        index.skipped.ignored,
        index.skipped.oversized,
        index.skipped.binary,
        index.skipped.outside_workspace,
        index.skipped.read_errors
    );
}

fn print_project_memory_status(status: &crate::project_memory::ProjectMemoryStatus) {
    if status.exists {
        println!(
            "  {GREEN}✓{RESET} {DIM}project memory index:{RESET} {}",
            status.path.display()
        );
        println!(
            "  {DIM}files{RESET} {}  {DIM}bytes{RESET} {}  {DIM}generated{RESET} {}",
            status.files,
            status.bytes,
            status.generated_at.as_deref().unwrap_or("unknown")
        );
    } else {
        println!(
            "  {YELLOW}!{RESET} {DIM}project memory index missing:{RESET} {}",
            status.path.display()
        );
        println!("  {DIM}Run /index to build it.{RESET}");
    }
}

fn print_project_index_freshness(freshness: &crate::project_memory::ProjectIndexFreshness) {
    let marker = if freshness.is_fresh() { GREEN } else { YELLOW };
    let label = if freshness.is_fresh() {
        "fresh"
    } else {
        "stale"
    };
    println!(
        "  {marker}{label}{RESET} {DIM}workspaceFiles={} indexed={} fresh={} stale={} missing={} deleted={} errors={}{RESET}",
        freshness.workspace_files,
        freshness.indexed_files,
        freshness.fresh,
        freshness.stale,
        freshness.missing,
        freshness.deleted,
        freshness.read_errors
    );
}

pub(super) fn cmd_map(args: &str, state: &AppState) -> Result<()> {
    let Some(index) = load_project_index(&state.config)? else {
        println!("  {YELLOW}!{RESET} {DIM}project memory index missing. Run /index first.{RESET}");
        return Ok(());
    };
    let notes = load_project_notes(&state.config)?;
    let query = if args.trim().is_empty() {
        None
    } else {
        Some(args.trim())
    };
    let map = render_repo_map(&state.config, &index, &notes, query);
    print!("{}", map.content);
    if map.truncated {
        println!("  {DIM}map truncated at {} bytes{RESET}", map.bytes);
    }
    Ok(())
}

pub(super) fn cmd_memory(args: &str, state: &mut AppState) {
    match args.trim() {
        "" | "status" => {
            println!(
                "  {DIM}projectMemory{RESET} enabled={} autoInject={} autoIndex={} allowCloudContext={}",
                state.config.project_memory.enabled,
                state.config.project_memory.auto_inject,
                state.config.project_memory.auto_index,
                state.config.project_memory.allow_cloud_context
            );
            if let Ok(status) = project_memory_status(&state.config) {
                print_project_memory_status(&status);
            }
        }
        "on" => {
            state.config.project_memory.enabled = true;
            println!("  {GREEN}✓{RESET} {DIM}project memory enabled for this session.{RESET}");
        }
        "off" => {
            state.config.project_memory.enabled = false;
            println!("  {GREEN}✓{RESET} {DIM}project memory disabled for this session.{RESET}");
        }
        other => println!("  {DIM}Usage: /memory [on|off|status] (got {other}){RESET}"),
    }
}

pub(super) fn cmd_remember(args: &str, state: &AppState) -> Result<()> {
    if args.trim().is_empty() {
        println!("  {DIM}Usage: /remember <project note>{RESET}");
        return Ok(());
    }
    let note = append_project_note(&state.config, args)?;
    println!(
        "  {GREEN}✓{RESET} {DIM}remembered{RESET} {CYAN}{}{RESET}",
        note.id
    );
    Ok(())
}

pub(super) fn cmd_forget(args: &str, state: &AppState) -> Result<()> {
    let id = args.trim();
    if id.is_empty() {
        println!("  {DIM}Usage: /forget <id|all>{RESET}");
        return Ok(());
    }
    let removed = forget_project_note(&state.config, id)?;
    if id == "all" {
        println!("  {GREEN}✓{RESET} {DIM}forgot all project notes.{RESET}");
    } else if removed == 0 {
        println!("  {YELLOW}!{RESET} {DIM}project note not found: {id}{RESET}");
    } else {
        println!("  {GREEN}✓{RESET} {DIM}forgot project note {id}.{RESET}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_memory::{load_project_index, load_project_notes};
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
    fn memory_commands_toggle_and_persist_notes() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());

        cmd_memory("off", &mut state);
        assert!(!state.config.project_memory.enabled);
        cmd_memory("on", &mut state);
        assert!(state.config.project_memory.enabled);

        cmd_remember("Entry point is src/main.rs", &state).unwrap();
        let notes = load_project_notes(&state.config).unwrap();
        assert_eq!(notes.len(), 1);
        cmd_forget(&notes[0].id, &state).unwrap();
        assert!(load_project_notes(&state.config).unwrap().is_empty());
    }

    #[test]
    fn index_command_builds_maps_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        let state = test_state(dir.path());

        cmd_index("", &state).unwrap();
        assert!(load_project_index(&state.config).unwrap().is_some());
        cmd_map("main", &state).unwrap();
        cmd_index("clear", &state).unwrap();
        assert!(load_project_index(&state.config).unwrap().is_none());
    }
}
