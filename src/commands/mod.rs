use anyhow::{anyhow, Result};
use chrono::Utc;
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::agent::to_openai_tools;
use crate::agent_eval::{builtin_fixtures, render_agent_eval_markdown, run_agent_eval};
use crate::app_state::AppState;
use crate::auto_loop::{parse_auto_args, parse_done_criteria, run_auto_loop, run_done_check};
use crate::backends::{backend, default_model, validate, BackendDescriptor, BackendName};
use crate::batch_operations::{
    execute_batch_operations, find_cross_file_references, find_related_files,
    preview_batch_operations, BatchEditOperation, EditOperation,
};
use crate::budget::{format_bytes, measure_prompt_budget};
use crate::capabilities::{
    self, best_record, recommended_tool_selection, record_score, sorted_records,
    warmup_recommended, BenchmarkStats, CapabilityRecord, CapabilityStatus,
};
use crate::catalog;
use crate::config::{is_tool_name, OperatorMode, ToolSelection, ALL_TOOL_NAMES};
use crate::context_guard::{
    compact_session, context_status_lines, extract_conversation_summary, merge_system_prompt,
    CompactMethod, CompactSessionContext,
};
use crate::continuation::{
    continuation_system_prompt, default_continuation_path, ensure_continuation_sections,
    render_continuation_prompt, render_fallback_continuation,
};
use crate::fix_loop::{parse_fix_args, run_fix_loop};
use crate::handoff::{
    collect_handoff_context, default_export_path as default_handoff_export_path,
    ensure_required_sections, handoff_system_prompt, render_fallback_markdown,
    render_handoff_prompt, should_refuse_cloud_handoff,
};
use crate::hardware::{detect_hardware_spec, save_hardware_summary, HardwareSpec};
use crate::input::{plain_read_line, select_from_list};
use crate::iterate_loop::{collect_diff_context, parse_iterate_args, run_iterate_loop};
use crate::model_system::{EffortLevel, ModelRef, TaskComplexity};
use crate::openai::{
    list_models, stream_chat, ChatMessage, ChatRequest, StreamOptions, ToolDef, ToolDefFunction,
};
use crate::planner::{
    default_routed_plan_path, default_spec_path, ensure_spec_sections, load_routed_plan,
    parse_routed_plan, planner_system_prompt, render_fallback_spec, render_planner_prompt,
    render_routed_planner_prompt, routed_planner_system_prompt, save_routed_plan, PlanTaskStatus,
    RoutedPlan, RoutedPlanTask,
};
use crate::playground::{
    print_play_list, print_scorecard, restore_play_session, run_play_battle, run_play_fixture,
};
use crate::project_memory::{
    append_project_note, build_project_index, clear_project_index, forget_project_note,
    load_project_index, load_project_notes, project_index_freshness, project_memory_status,
    refresh_changed_project_index, render_repo_map, render_system_prompt_with_memory,
};
use crate::prompt_library::{
    delete_prompt, export_prompts, import_prompts, list_prompts, load_prompt, save_prompt,
    PromptLibrary,
};
use crate::recommend::{
    apply_recommendation_to_config, recommend_models, ModelCandidate, ModelRecommendation,
};
use crate::session::{
    delete_session, list_sessions, load_messages, load_session, load_session_metadata,
    render_markdown, resolve_session_path, save_message, save_session_metadata, search_sessions,
    set_session_title, SessionEntry,
};
use crate::session_paths::{apply_path_session_state, PathStore, DEFAULT_PATH_ID};
use crate::session_turn::{run_user_turn, TurnOptions};
use crate::setup::{build_model_menu, model_override_for, ModelMenuItem, MODEL_FREE_TEXT_LABEL};
use crate::shipcheck::{
    collect_shipcheck, collect_shipcheck_with_tests, default_export_path, file_status_label,
    render_markdown as render_shipcheck_markdown, ShipcheckSnapshot,
};
use crate::test_integration::{
    discover_tests, run_selected_tests, run_tests, smart_test_selection,
};
use crate::tools::{build_tools, build_tools_for_names, select_tool_names};
use crate::warmup::warmup;

// Command groups split out of this file. `dispatch` (below) stays the single
// router; these modules hold the cmd_* handlers and their private helpers.
mod config_cmds;
mod context_cmds;
mod doctor;
mod fable;
mod hooks_cmds;
mod memory;
mod route;
mod scorecard;
mod session;
mod ship;
mod workflow;

pub(crate) use context_cmds::perform_reset;

use doctor::*;
use ship::*;
use workflow::*;

const RESET: crate::theme::Style = crate::theme::RESET;
const DIM: crate::theme::Style = crate::theme::MUTED;
const BOLD: crate::theme::Style = crate::theme::BOLD;
const CYAN: crate::theme::Style = crate::theme::ACCENT;
const GREEN: crate::theme::Style = crate::theme::SUCCESS;
const YELLOW: crate::theme::Style = crate::theme::WARN;
const RED: crate::theme::Style = crate::theme::ERROR;

pub const COMMANDS: &[(&str, &str)] = &[
    ("/undo", "Revert the last agent turn's file mutations"),
    (
        "/checkpoints",
        "Show or toggle turn checkpoints (on, off, status)",
    ),
    (
        "/path",
        "Fork, switch, diff, pick, or drop parallel session paths",
    ),
    ("/paths", "List saved session paths"),
    ("/help", "List available commands"),
    ("/setup", "Run the setup wizard and write agent.config.json"),
    ("/new", "Start a fresh conversation"),
    ("/clear", "Clear the screen"),
    ("/config", "Show resolved configuration"),
    (
        "/mode",
        "Show or set operator mode (explore, edit, ship, review)",
    ),
    (
        "/shipcheck",
        "Summarize git and project-memory readiness for release",
    ),
    (
        "/ship",
        "Preview last-mile ship readiness and draft a commit message",
    ),
    (
        "/handoff",
        "Draft commit, changelog, testing, and X-ready release copy",
    ),
    (
        "/plan",
        "Expand a spec, create routed task graphs, execute routed plans, or validate Done Criteria",
    ),
    ("/session", "Show session info and token usage"),
    (
        "/scorecard",
        "Show global quality PRs shipped and close PR units",
    ),
    (
        "/fable",
        "Track Claude Fable weekly usage and cap headroom",
    ),
    ("/sessions", "List saved sessions"),
    ("/resume", "Resume latest or named session"),
    ("/export", "Export a session to markdown or json"),
    (
        "/auth",
        "Manage API keys and OAuth credentials (list, set, clear, login)",
    ),
    (
        "/login",
        "Browser/device-code login (defaults to active OAuth backend: openai-codex, grok)",
    ),
    (
        "/logout",
        "Clear an OAuth login (defaults to active OAuth backend: openai-codex, grok)",
    ),
    (
        "/image",
        "Attach an image to the next user prompt (vision-capable models only)",
    ),
    (
        "/reasoning",
        "Toggle the streaming reasoning panel (on, off, status)",
    ),
    (
        "/verbose",
        "Show every tool call with full args + result (on, off, status)",
    ),
    (
        "/trace",
        "Show nested subagent/critic tool activity (on, off, status)",
    ),
    (
        "/hooks",
        "List, trust, enable, or disable configured hooks",
    ),
    (
        "/backend",
        "Switch backend (ollama, lm-studio, mlx, llamacpp, openrouter, openai, openai-codex, grok); --default persists",
    ),
    (
        "/model",
        "List/pick a model; --default pins backend+model in agent.config.json",
    ),
    (
        "/tools",
        "Show or set enabled tools (comma-separated names)",
    ),
    (
        "/compare",
        "Run the last user prompt against the OpenRouter cloud (requires OPENROUTER_API_KEY)",
    ),
    (
        "/fusion",
        "Use OpenRouter Fusion alias or attach Fusion deliberation to an OpenRouter model",
    ),
    (
        "/route",
        "Select or apply a multi-model stack route for the task",
    ),
    (
        "/context",
        "Show or update context limits and auto-guard status",
    ),
    ("/compact", "Summarize or trim older conversation turns"),
    (
        "/reset",
        "Reset context: write a continuation handoff (.small-harness/continue.md) and start fresh",
    ),
    (
        "/doctor",
        "Probe backend/tools/env; subcommands: recommend, bench, models, autotune",
    ),
    ("/index", "Build, refresh, show, or clear project memory"),
    ("/map", "Print the project memory repo map or focused hits"),
    ("/memory", "Turn project memory on/off or show status"),
    ("/remember", "Save a durable project note"),
    ("/forget", "Remove a project note"),
    ("/eval", "Run prompt/model comparison suite"),
    (
        "/batch",
        "Execute multi-file operations with preview and rollback",
    ),
    ("/refactor", "Find cross-file references and related files"),
    (
        "/test",
        "Discover, run, and analyze tests with smart selection",
    ),
    ("/prompt", "Save, list, run, and manage prompt templates"),
    (
        "/play",
        "Try bundled agent demos (fix-failing-test, add-feature, battle, exit)",
    ),
    (
        "/fix",
        "Fix-until-green loop on your repo (smart tests, --attempts, --yolo)",
    ),
    (
        "/iterate",
        "Generate→evaluate→improve loop on a goal (rubric-scored, --max, --threshold, --yolo)",
    ),
    (
        "/auto",
        "Autonomous overnight run: chains /iterate + auto /reset toward a goal/spec with budget/deadline guardrails",
    ),
];

pub async fn dispatch(input: &str, state: &mut AppState) -> Result<()> {
    let mut parts = input.splitn(2, ' ');
    let name = parts.next().unwrap_or("");
    let args = parts.next().unwrap_or("").trim().to_string();

    match name {
        "/help" => help(),
        "/setup" => cmd_setup(state).await?,
        "/new" => session::cmd_new(state),
        "/clear" => clear_screen(),
        "/undo" => session::cmd_undo(&args, state)?,
        "/checkpoints" => context_cmds::cmd_checkpoints(&args, state),
        "/path" => session::cmd_path(&args, state).await?,
        "/paths" => session::cmd_paths(state)?,
        "/config" => config_cmds::cmd_config(state),
        "/mode" => config_cmds::cmd_mode(&args, state),
        "/shipcheck" => cmd_shipcheck(&args, state)?,
        "/ship" => cmd_ship(&args, state).await?,
        "/handoff" => cmd_handoff(&args, state).await?,
        "/plan" => cmd_plan(&args, state).await?,
        "/session" => session::cmd_session(&args, state)?,
        "/scorecard" => scorecard::cmd_scorecard(&args, state)?,
        "/fable" => fable::cmd_fable(&args, state)?,
        "/sessions" => session::cmd_sessions(&args, state)?,
        "/resume" => session::cmd_resume(&args, state)?,
        "/export" => session::cmd_export(&args, state)?,
        "/auth" => config_cmds::cmd_auth(&args).await?,
        "/login" => config_cmds::cmd_login(&args, state).await?,
        "/logout" => config_cmds::cmd_logout(&args, Some(state.config.backend))?,
        "/image" => config_cmds::cmd_image(&args, state),
        "/reasoning" => config_cmds::cmd_reasoning(&args, state),
        "/verbose" => config_cmds::cmd_verbose(&args, state),
        "/trace" => config_cmds::cmd_trace(&args, state),
        "/hooks" => hooks_cmds::cmd_hooks(&args, state)?,
        "/backend" => config_cmds::cmd_backend(&args, state).await?,
        "/model" => config_cmds::cmd_model(&args, state).await?,
        "/tools" => config_cmds::cmd_tools(&args, state),
        "/compare" => config_cmds::cmd_compare(&args, state).await?,
        "/fusion" => config_cmds::cmd_fusion(&args, state)?,
        "/route" => route::cmd_route(&args, state).await?,
        "/context" => context_cmds::cmd_context(&args, state),
        "/compact" => context_cmds::cmd_compact(&args, state).await?,
        "/reset" => context_cmds::cmd_reset(&args, state).await?,
        "/doctor" => cmd_doctor(&args, state).await?,
        "/index" => memory::cmd_index(&args, state)?,
        "/map" => memory::cmd_map(&args, state)?,
        "/memory" => memory::cmd_memory(&args, state),
        "/remember" => memory::cmd_remember(&args, state)?,
        "/forget" => memory::cmd_forget(&args, state)?,
        "/eval" => cmd_eval(&args, state).await?,
        "/batch" => cmd_batch(&args, state)?,
        "/refactor" => cmd_refactor(&args, state)?,
        "/test" => cmd_test(&args, state)?,
        "/prompt" => cmd_prompt(&args, state).await?,
        "/play" => cmd_play(&args, state).await?,
        "/fix" => cmd_fix(&args, state).await?,
        "/iterate" => cmd_iterate(&args, state).await?,
        "/auto" => cmd_auto(&args, state).await?,
        // These model-tuning commands were folded into `/doctor` subcommands.
        "/bench" => redirect_to_doctor("/bench", "bench"),
        "/capabilities" => redirect_to_doctor("/capabilities", "models"),
        "/autotune" => redirect_to_doctor("/autotune", "autotune"),
        "/recommend" => redirect_to_doctor("/recommend", "recommend"),
        other => {
            println!("  {DIM}Unknown command: {other}. Type /help.{RESET}");
        }
    }
    Ok(())
}

/// One-liner shown when someone types an old top-level command that is now a
/// `/doctor` subcommand. Keeps muscle memory working without re-listing the
/// commands in `/help`.
fn redirect_to_doctor(old: &str, sub: &str) {
    println!("  {DIM}{old} is now {CYAN}/doctor {sub}{RESET}{DIM}. Run that instead.{RESET}");
}

/// All slash commands (name + description) offered in the completion menu,
/// sorted by name for a stable order. Includes `/exit` and `/quit`, which are
/// handled in the input loop rather than via `COMMANDS`.
pub fn command_list() -> Vec<(String, String)> {
    let mut cmds: Vec<(String, String)> = COMMANDS
        .iter()
        .map(|(n, d)| ((*n).to_string(), (*d).to_string()))
        .collect();
    cmds.push(("/exit".to_string(), "Quit Small Harness".to_string()));
    cmds.push(("/quit".to_string(), "Quit (alias for /exit)".to_string()));
    cmds.sort_by(|a, b| a.0.cmp(&b.0));
    cmds.dedup_by(|a, b| a.0 == b.0);
    cmds
}

fn help() {
    for (n, d) in COMMANDS {
        println!("  {CYAN}{:<12}{RESET} {DIM}{}{RESET}", n, d);
    }
    println!(
        "  {CYAN}{:<12}{RESET} {DIM}Quit Small Harness{RESET}",
        "/exit"
    );
    println!(
        "  {CYAN}{:<12}{RESET} {DIM}Quit (alias for /exit){RESET}",
        "/quit"
    );
}

async fn cmd_setup(state: &mut AppState) -> Result<()> {
    let Some(config) = crate::setup::run_setup_wizard(&state.config).await? else {
        return Ok(());
    };
    let backend_desc = config.backend_descriptor();
    if let Err(e) = validate(&backend_desc) {
        println!(
            "  {YELLOW}!{RESET} {DIM}Config saved, but active session stayed on the previous backend: {e}{RESET}"
        );
        return Ok(());
    }
    let model = default_model(&backend_desc, config.model_override.as_deref());
    let old_session_dir = state.session_dir.clone();
    state.config = config;
    state.backend = backend_desc;
    state.model = model;
    state.active_effort = None;
    state.session_dir = state.config.session_dir.clone();
    if state.session_dir != old_session_dir {
        fs::create_dir_all(&state.session_dir)?;
        state.reset_session();
    }
    println!("  {GREEN}✓{RESET} {DIM}active config updated for this session.{RESET}");
    Ok(())
}

fn clear_screen() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[2J\x1b[H");
    let _ = out.flush();
}

fn cmd_shipcheck(args: &str, state: &AppState) -> Result<()> {
    let parts: Vec<&str> = args.split_whitespace().collect();
    let action = parts.first().copied();
    let export = matches!(action, Some("export") | Some("save"));
    let run_tests = args.contains("--tests");

    if matches!(action, Some(other) if other != "export" && other != "save" && other != "--tests") {
        println!("  {DIM}Usage: /shipcheck [export [path]] [--tests]{RESET}");
        return Ok(());
    }

    // Filter out --tests from path parsing
    let path_parts: Vec<&str> = parts.iter().filter(|p| **p != "--tests").cloned().collect();
    let explicit_path = if path_parts.len() > 1 {
        Some(PathBuf::from(path_parts[1]))
    } else {
        None
    };

    if path_parts.len() > 2 {
        println!("  {DIM}Usage: /shipcheck [export [path]] [--tests]{RESET}");
        return Ok(());
    }

    let snapshot = if run_tests {
        collect_shipcheck_with_tests(&state.config.workspace_root, true)?
    } else {
        collect_shipcheck(&state.config.workspace_root)?
    };

    let freshness = if state.config.project_memory.enabled {
        Some(project_index_freshness(&state.config)?)
    } else {
        None
    };
    print_shipcheck(&snapshot, freshness.as_ref());

    if export {
        let out_path = explicit_path.unwrap_or_else(|| default_export_path(&state.session_dir));
        if let Some(parent) = out_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        fs::write(
            &out_path,
            render_shipcheck_markdown(&snapshot, freshness.as_ref()),
        )?;
        println!(
            "  {GREEN}✓{RESET} {DIM}shipcheck saved →{RESET} {}",
            out_path.display()
        );
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HandoffArgs {
    export: bool,
    explicit_path: Option<PathBuf>,
    allow_cloud: bool,
}

fn parse_handoff_args(args: &str) -> Option<HandoffArgs> {
    let mut export = false;
    let mut explicit_path = None;
    let mut allow_cloud = false;

    for part in args.split_whitespace() {
        match part {
            "--cloud" => allow_cloud = true,
            "export" | "save" if !export => export = true,
            other if export && explicit_path.is_none() => {
                explicit_path = Some(PathBuf::from(other));
            }
            _ => return None,
        }
    }

    Some(HandoffArgs {
        export,
        explicit_path,
        allow_cloud,
    })
}

async fn cmd_handoff(args: &str, state: &AppState) -> Result<()> {
    let Some(args) = parse_handoff_args(args) else {
        println!("  {DIM}Usage: /handoff [export|save] [path] [--cloud]{RESET}");
        return Ok(());
    };

    if should_refuse_cloud_handoff(state.backend.name, args.allow_cloud) {
        println!(
            "  {RED}✗{RESET} {DIM}/handoff will not send diff context to OpenRouter unless you pass --cloud.{RESET}"
        );
        return Ok(());
    }

    let snapshot = collect_shipcheck(&state.config.workspace_root)?;
    let freshness = if state.config.project_memory.enabled {
        Some(project_index_freshness(&state.config)?)
    } else {
        None
    };
    let Some(context) = collect_handoff_context(&snapshot)? else {
        println!(
            "  {DIM}Nothing to hand off: working tree is clean and branch is not ahead of upstream.{RESET}"
        );
        return Ok(());
    };

    println!(
        "  {DIM}drafting handoff from {} with {} · {}{RESET}",
        context.basis.label(),
        state.config.backend.as_str(),
        state.model
    );

    let messages = vec![
        ChatMessage::System {
            content: handoff_system_prompt(),
        },
        ChatMessage::User {
            content: render_handoff_prompt(&context, freshness.as_ref()).into(),
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
        max_tokens: Some(900),
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

    let body = match result {
        Ok(_) if !draft.trim().is_empty() => ensure_required_sections(&draft),
        Ok(_) => render_fallback_markdown(
            &context,
            &snapshot,
            freshness.as_ref(),
            Some("empty model response"),
        ),
        Err(e) => render_fallback_markdown(
            &context,
            &snapshot,
            freshness.as_ref(),
            Some(&e.to_string()),
        ),
    };

    println!();
    print!("{body}");

    if args.export {
        let out_path = args
            .explicit_path
            .unwrap_or_else(|| default_handoff_export_path(&state.session_dir));
        if let Some(parent) = out_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        fs::write(&out_path, body)?;
        println!(
            "  {GREEN}✓{RESET} {DIM}handoff saved →{RESET} {}",
            out_path.display()
        );
    }

    Ok(())
}

enum PlanInvocation {
    Show,
    Validate,
    Status,
    Route {
        intent: String,
        export_path: Option<PathBuf>,
        planner: Option<PlannerChoice>,
    },
    Execute(PlanExecuteArgs),
    Draft {
        intent: String,
        export_path: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlannerChoice {
    Configured,
    Selector,
    Tier(TaskComplexity),
    Active,
    Explicit(ModelRef),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlanExecuteArgs {
    yolo: bool,
    max_tasks: Option<usize>,
}

/// Parse `/plan` arguments. Returns `None` to print usage.
///   `/plan <intent>`                 → draft to `.small-harness/spec.md`
///   `/plan <intent> --export <path>` → draft to `<path>` instead
///   `/plan show`                     → print the saved spec
///   `/plan validate`                 → check the spec's Done Criteria vs the diff
///   `/plan route <intent>`           → draft `.small-harness/plan.json`
///   `/plan status`                   → show routed plan status
///   `/plan execute`                  → execute ready routed-plan tasks
fn parse_plan_args(args: &str) -> Option<PlanInvocation> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "show" {
        return Some(PlanInvocation::Show);
    }
    if trimmed == "validate" {
        return Some(PlanInvocation::Validate);
    }
    if matches!(trimmed, "status" | "route status" | "routed status") {
        return Some(PlanInvocation::Status);
    }
    if let Some(rest) = trimmed
        .strip_prefix("route ")
        .or_else(|| trimmed.strip_prefix("routed "))
    {
        return parse_plan_route_args(rest.trim());
    }
    if matches!(trimmed, "route" | "routed") {
        return None;
    }
    if let Some(rest) = trimmed
        .strip_prefix("execute")
        .or_else(|| trimmed.strip_prefix("run"))
    {
        return parse_plan_execute_args(rest.trim()).map(PlanInvocation::Execute);
    }

    let mut export_path: Option<PathBuf> = None;
    let mut intent_parts: Vec<&str> = Vec::new();
    let mut parts = trimmed.split_whitespace();
    while let Some(part) = parts.next() {
        if part == "--export" {
            export_path = Some(PathBuf::from(parts.next()?));
        } else if let Some(value) = part.strip_prefix("--export=") {
            if value.is_empty() {
                return None;
            }
            export_path = Some(PathBuf::from(value));
        } else {
            intent_parts.push(part);
        }
    }

    let intent = intent_parts.join(" ");
    if intent.trim().is_empty() {
        return None;
    }
    Some(PlanInvocation::Draft {
        intent,
        export_path,
    })
}

fn parse_plan_route_args(rest: &str) -> Option<PlanInvocation> {
    if rest.is_empty() {
        return None;
    }
    let mut export_path: Option<PathBuf> = None;
    let mut planner: Option<PlannerChoice> = None;
    let mut intent_parts: Vec<&str> = Vec::new();
    let mut parts = rest.split_whitespace();
    while let Some(part) = parts.next() {
        if part == "--export" {
            export_path = Some(PathBuf::from(parts.next()?));
        } else if let Some(value) = part.strip_prefix("--export=") {
            if value.is_empty() {
                return None;
            }
            export_path = Some(PathBuf::from(value));
        } else if part == "--planner" {
            planner = Some(parse_planner_choice(parts.next()?)?);
        } else if let Some(value) = part.strip_prefix("--planner=") {
            if value.is_empty() {
                return None;
            }
            planner = Some(parse_planner_choice(value)?);
        } else if part.starts_with("--") {
            return None;
        } else {
            intent_parts.push(part);
        }
    }
    let intent = intent_parts.join(" ");
    if intent.trim().is_empty() {
        return None;
    }
    Some(PlanInvocation::Route {
        intent,
        export_path,
        planner,
    })
}

fn parse_planner_choice(value: &str) -> Option<PlannerChoice> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "configured" | "default" | "planner" => Some(PlannerChoice::Configured),
        "selector" => Some(PlannerChoice::Selector),
        "active" | "current" => Some(PlannerChoice::Active),
        "low" | "medium" | "med" | "high" => {
            TaskComplexity::parse(&normalized).map(PlannerChoice::Tier)
        }
        _ => ModelRef::parse_spec(value).map(PlannerChoice::Explicit),
    }
}

fn parse_plan_execute_args(rest: &str) -> Option<PlanExecuteArgs> {
    let mut yolo = false;
    let mut max_tasks = None;
    let mut parts = rest.split_whitespace();
    while let Some(part) = parts.next() {
        match part {
            "--yolo" => yolo = true,
            "--max" => {
                max_tasks = Some(parts.next()?.parse().ok()?);
            }
            _ if part.starts_with("--max=") => {
                max_tasks = Some(part.trim_start_matches("--max=").parse().ok()?);
            }
            "" => {}
            _ => return None,
        }
    }
    Some(PlanExecuteArgs { yolo, max_tasks })
}

async fn cmd_plan(args: &str, state: &mut AppState) -> Result<()> {
    let Some(invocation) = parse_plan_args(args) else {
        println!(
            "  {DIM}Usage: /plan <intent> · /plan route [--planner high|selector|backend:model] <intent> · /plan status · /plan execute [--yolo] [--max N] · /plan show · /plan validate{RESET}"
        );
        return Ok(());
    };

    let default_path = default_spec_path(&state.config.workspace_root);
    let routed_path = default_routed_plan_path(&state.config.workspace_root);

    let (intent, export_path) = match invocation {
        PlanInvocation::Show => {
            match fs::read_to_string(&default_path) {
                Ok(content) => {
                    println!();
                    print!("{content}");
                }
                Err(_) => println!(
                    "  {DIM}No spec yet at {} — run /plan <intent> to create one.{RESET}",
                    default_path.display()
                ),
            }
            return Ok(());
        }
        PlanInvocation::Validate => return cmd_plan_validate(state, &default_path).await,
        PlanInvocation::Status => {
            print_routed_plan_status(&routed_path)?;
            return Ok(());
        }
        PlanInvocation::Route {
            intent,
            export_path,
            planner,
        } => {
            return cmd_plan_route(state, &intent, export_path, planner, &routed_path).await;
        }
        PlanInvocation::Execute(args) => {
            return cmd_plan_execute(state, &routed_path, args).await;
        }
        PlanInvocation::Draft {
            intent,
            export_path,
        } => (intent, export_path),
    };

    println!(
        "  {DIM}expanding spec with {} · {}{RESET}",
        state.config.backend.as_str(),
        state.model
    );

    let messages = vec![
        ChatMessage::System {
            content: planner_system_prompt(),
        },
        ChatMessage::User {
            content: render_planner_prompt(&intent).into(),
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
        max_tokens: Some(1500),
        prompt_cache_key: None,
        effort: state.active_effort,
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

    let body = match result {
        Ok(_) if !draft.trim().is_empty() => ensure_spec_sections(&draft),
        Ok(_) => render_fallback_spec(&intent, Some("empty model response")),
        Err(e) => render_fallback_spec(&intent, Some(&e.to_string())),
    };

    println!();
    print!("{body}");

    let out_path = export_path.unwrap_or(default_path);
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(&out_path, body)?;
    println!(
        "  {GREEN}✓{RESET} {DIM}spec saved →{RESET} {}",
        out_path.display()
    );

    Ok(())
}

async fn cmd_plan_route(
    state: &AppState,
    intent: &str,
    export_path: Option<PathBuf>,
    planner_choice: Option<PlannerChoice>,
    default_path: &Path,
) -> Result<()> {
    let planner = match resolve_planner_model(state, planner_choice) {
        Ok(planner) => planner,
        Err(e) => {
            println!("  {RED}✗{RESET} {DIM}{e}{RESET}");
            return Ok(());
        }
    };
    let backend_desc = state.config.backend_descriptor_for(planner.backend);
    if let Err(e) = validate(&backend_desc) {
        println!("  {RED}✗{RESET} {DIM}planner backend is not ready: {e}{RESET}");
        return Ok(());
    }

    println!(
        "  {DIM}routing plan with{RESET} {}",
        planner.detail_with_effort(planner.effort)
    );
    let messages = vec![
        ChatMessage::System {
            content: routed_planner_system_prompt(),
        },
        ChatMessage::User {
            content: render_routed_planner_prompt(
                intent,
                &state.config.model_system,
                Some(&planner),
            )
            .into(),
        },
    ];
    let req = ChatRequest {
        model: &planner.model,
        messages: &messages,
        tools: None,
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
        max_tokens: Some(2400),
        prompt_cache_key: None,
        effort: planner.effort,
    };
    let mut draft = String::new();
    let mut reported_cost = None;
    let result = stream_chat(&state.http, &backend_desc, &req, None, |chunk| {
        if let Some(choice) = chunk.choices.first() {
            if let Some(content) = &choice.delta.content {
                draft.push_str(content);
            }
        }
        if let Some(usage) = &chunk.usage {
            if let Some(cost) = usage.cost {
                reported_cost = Some(cost);
            }
        }
    })
    .await;

    if let Some(cost) = reported_cost {
        println!(
            "  {DIM}planner cost{RESET}     {}",
            catalog::format_usd(cost)
        );
    }

    let plan = match result {
        Ok(_) if !draft.trim().is_empty() => parse_routed_plan(
            &draft,
            intent,
            Some(planner.clone()),
            &state.config.model_system,
        ),
        Ok(_) => Err(anyhow!("planner returned an empty routed plan")),
        Err(e) => Err(anyhow!("planner request failed: {e}")),
    };
    let plan = match plan {
        Ok(plan) => plan,
        Err(e) => {
            println!("  {RED}✗{RESET} {DIM}{e}{RESET}");
            return Ok(());
        }
    };

    print_routed_plan_summary(&plan);
    let out_path = export_path.unwrap_or_else(|| default_path.to_path_buf());
    save_routed_plan(&out_path, &plan)?;
    println!(
        "  {GREEN}✓{RESET} {DIM}routed plan saved →{RESET} {}",
        out_path.display()
    );
    Ok(())
}

fn resolve_planner_model(state: &AppState, choice: Option<PlannerChoice>) -> Result<ModelRef> {
    let stack = &state.config.model_system;
    let active = || ModelRef {
        backend: state.config.backend,
        model: state.model.clone(),
        effort: state.active_effort,
        thinking_depth: None,
        notes: Some("Current active model.".into()),
    };
    match choice.unwrap_or(PlannerChoice::Configured) {
        PlannerChoice::Configured => Ok(stack
            .planner
            .clone()
            .or_else(|| stack.selector.clone())
            .or_else(|| stack.orchestrator(TaskComplexity::High).cloned())
            .or_else(|| stack.orchestrator(TaskComplexity::Medium).cloned())
            .or_else(|| stack.orchestrator(TaskComplexity::Low).cloned())
            .unwrap_or_else(active)),
        PlannerChoice::Selector => stack
            .selector
            .clone()
            .ok_or_else(|| anyhow!("modelSystem.selector is not configured")),
        PlannerChoice::Tier(complexity) => stack
            .orchestrator(complexity)
            .or_else(|| stack.coder(complexity))
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "no planner/orchestrator/coder is configured for {}",
                    complexity.as_str()
                )
            }),
        PlannerChoice::Active => Ok(active()),
        PlannerChoice::Explicit(model) => Ok(model),
    }
}

fn print_routed_plan_status(path: &Path) -> Result<()> {
    let plan = match load_routed_plan(path) {
        Ok(plan) => plan,
        Err(_) if !path.exists() => {
            println!(
                "  {DIM}No routed plan yet at {} — run /plan route <intent>.{RESET}",
                path.display()
            );
            return Ok(());
        }
        Err(e) => {
            println!("  {RED}✗{RESET} {DIM}{e}{RESET}");
            return Ok(());
        }
    };
    print_routed_plan_summary(&plan);
    println!("  {DIM}plan path{RESET}        {}", path.display());
    Ok(())
}

fn print_routed_plan_summary(plan: &RoutedPlan) {
    let pending = plan
        .tasks
        .iter()
        .filter(|task| task.status == PlanTaskStatus::Pending)
        .count();
    let running = plan
        .tasks
        .iter()
        .filter(|task| task.status == PlanTaskStatus::Running)
        .count();
    let done = plan
        .tasks
        .iter()
        .filter(|task| task.status == PlanTaskStatus::Done)
        .count();
    let failed = plan
        .tasks
        .iter()
        .filter(|task| task.status == PlanTaskStatus::Failed)
        .count();
    println!("  {DIM}routed plan{RESET}      {}", plan.goal);
    println!(
        "  {DIM}tasks{RESET}            {} total · {} done · {} running · {} pending · {} failed",
        plan.tasks.len(),
        done,
        running,
        pending,
        failed
    );
    for task in &plan.tasks {
        println!(
            "  {DIM}- {:<10}{RESET} {:<7} {:<7} {}{}",
            task.id,
            task.status.as_str(),
            task.complexity.as_str(),
            task.title,
            format_task_model_suffix(task)
        );
    }
}

fn format_task_model_suffix(task: &RoutedPlanTask) -> String {
    match &task.assigned_model {
        Some(model) => {
            let effort = task.effort.or(model.effort);
            format!(" {DIM}→{RESET} {}", model.detail_with_effort(effort))
        }
        None => " {DIM}→ no configured coder{RESET}".into(),
    }
}

async fn cmd_plan_execute(state: &mut AppState, path: &Path, args: PlanExecuteArgs) -> Result<()> {
    let mut plan = match load_routed_plan(path) {
        Ok(plan) => plan,
        Err(_) if !path.exists() => {
            println!(
                "  {DIM}No routed plan yet at {} — run /plan route <intent>.{RESET}",
                path.display()
            );
            return Ok(());
        }
        Err(e) => {
            println!("  {RED}✗{RESET} {DIM}{e}{RESET}");
            return Ok(());
        }
    };
    let original = ActiveModelSnapshot::capture(state);
    let mut ran = 0usize;
    let mut stopped_on_error = false;

    while args.max_tasks.map(|max| ran < max).unwrap_or(true) {
        let Some(index) = next_ready_task_index(&plan) else {
            break;
        };
        let task = plan.tasks[index].clone();
        let Some(model) = task
            .assigned_model
            .clone()
            .or_else(|| state.config.model_system.coder(task.complexity).cloned())
        else {
            plan.tasks[index].status = PlanTaskStatus::Failed;
            plan.tasks[index].last_error = Some(format!(
                "modelSystem.coders.{} is not configured",
                task.complexity.as_str()
            ));
            save_routed_plan(path, &plan)?;
            stopped_on_error = true;
            break;
        };

        plan.tasks[index].status = PlanTaskStatus::Running;
        plan.tasks[index].assigned_model = Some(model.clone());
        plan.tasks[index].last_error = None;
        save_routed_plan(path, &plan)?;

        println!(
            "  {DIM}plan task{RESET}        {} · {} · {}",
            task.id,
            task.complexity.as_str(),
            model.detail_with_effort(task.effort.or(model.effort))
        );
        if let Err(e) = route::apply_model_ref(state, &model, task.effort) {
            plan.tasks[index].status = PlanTaskStatus::Failed;
            plan.tasks[index].last_error = Some(format!("could not apply routed model: {e}"));
            save_routed_plan(path, &plan)?;
            stopped_on_error = true;
            break;
        }

        let prompt = render_routed_task_prompt(&plan, &task);
        match run_user_turn(
            state,
            TurnOptions {
                user_prompt: prompt,
                auto_verify_tests: false,
                yolo_approve: args.yolo,
                source: "plan-execute",
            },
        )
        .await
        {
            Ok(outcome) => {
                plan.tasks[index].status = PlanTaskStatus::Done;
                plan.tasks[index].result = last_assistant_text(&outcome.run_result.messages)
                    .map(|s| truncate_summary(&s, 2000));
                plan.tasks[index].last_error = None;
                save_routed_plan(path, &plan)?;
                ran += 1;
            }
            Err(e) => {
                plan.tasks[index].status = PlanTaskStatus::Failed;
                plan.tasks[index].last_error = Some(e.to_string());
                save_routed_plan(path, &plan)?;
                stopped_on_error = true;
                break;
            }
        }
    }

    if let Err(e) = original.restore(state) {
        println!("  {YELLOW}!{RESET} {DIM}could not restore original active model: {e}{RESET}");
    }

    if ran == 0 && !stopped_on_error {
        if plan
            .tasks
            .iter()
            .all(|task| task.status == PlanTaskStatus::Done)
        {
            println!("  {GREEN}✓{RESET} {DIM}routed plan already complete{RESET}");
        } else {
            println!(
                "  {DIM}No ready tasks. Check failed or unmet dependencies with /plan status.{RESET}"
            );
        }
    } else if stopped_on_error {
        println!(
            "  {RED}✗{RESET} {DIM}routed execution stopped after {} completed task(s); inspect /plan status.{RESET}",
            ran
        );
    } else {
        println!(
            "  {GREEN}✓{RESET} {DIM}routed execution ran {} task(s); status saved →{RESET} {}",
            ran,
            path.display()
        );
    }
    print_routed_plan_summary(&plan);
    Ok(())
}

#[derive(Debug, Clone)]
struct ActiveModelSnapshot {
    backend: BackendName,
    model_override: Option<String>,
    backend_desc: BackendDescriptor,
    model: String,
    active_effort: Option<EffortLevel>,
    warmed_fingerprint: Option<u64>,
}

impl ActiveModelSnapshot {
    fn capture(state: &AppState) -> Self {
        Self {
            backend: state.config.backend,
            model_override: state.config.model_override.clone(),
            backend_desc: state.backend.clone(),
            model: state.model.clone(),
            active_effort: state.active_effort,
            warmed_fingerprint: state.warmed_fingerprint,
        }
    }

    fn restore(self, state: &mut AppState) -> Result<()> {
        state.config.backend = self.backend;
        state.config.model_override = self.model_override;
        state.backend = self.backend_desc;
        state.model = self.model;
        state.active_effort = self.active_effort;
        state.warmed_fingerprint = self.warmed_fingerprint;
        state.rebuild_client()
    }
}

fn next_ready_task_index(plan: &RoutedPlan) -> Option<usize> {
    plan.tasks.iter().position(|task| {
        matches!(
            task.status,
            PlanTaskStatus::Pending | PlanTaskStatus::Running
        ) && task.depends_on.iter().all(|dep| {
            plan.tasks
                .iter()
                .any(|candidate| candidate.id == *dep && candidate.status == PlanTaskStatus::Done)
        })
    })
}

fn render_routed_task_prompt(plan: &RoutedPlan, task: &RoutedPlanTask) -> String {
    let mut out = String::new();
    out.push_str("Execute one task from a Small Harness routed plan.\n\n");
    out.push_str("Overall goal:\n");
    out.push_str(&plan.goal);
    out.push_str("\n\nTask:\n");
    out.push_str(&format!("- id: {}\n", task.id));
    out.push_str(&format!("- title: {}\n", task.title));
    out.push_str(&format!("- role: {}\n", task.role));
    out.push_str(&format!("- complexity: {}\n", task.complexity.as_str()));
    if !task.depends_on.is_empty() {
        out.push_str("- completed dependencies:\n");
        for dep in &task.depends_on {
            if let Some(prev) = plan.tasks.iter().find(|candidate| candidate.id == *dep) {
                out.push_str(&format!("  - {}: {}\n", prev.id, prev.title));
                if let Some(result) = prev.result.as_deref().filter(|s| !s.trim().is_empty()) {
                    out.push_str("    result: ");
                    out.push_str(&truncate_summary(result, 400));
                    out.push('\n');
                }
            }
        }
    }
    out.push_str("\nInstructions:\n");
    out.push_str(&task.prompt);
    out.push_str("\n\nAcceptance criteria:\n");
    if task.acceptance.is_empty() {
        out.push_str("- Complete the task without expanding into unrelated plan tasks.\n");
    } else {
        for item in &task.acceptance {
            out.push_str("- ");
            out.push_str(item);
            out.push('\n');
        }
    }
    out.push_str("\nWork only on this task. Do not perform later tasks unless they are strictly required to make this task coherent. When finished, summarize what changed and any verification you ran.");
    out
}

fn last_assistant_text(messages: &[ChatMessage]) -> Option<String> {
    messages.iter().rev().find_map(|message| match message {
        ChatMessage::Assistant {
            content: Some(content),
            ..
        } if !content.trim().is_empty() => Some(content.trim().to_string()),
        _ => None,
    })
}

fn truncate_summary(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in text.trim().chars().take(max_chars) {
        out.push(ch);
    }
    if text.trim().chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

/// `/plan validate`: read the saved spec's Done Criteria and check each one
/// against the current working-tree diff, printing a met/unmet checklist. The
/// same done-check `/auto` runs each round, exposed as a one-shot command so you
/// can ask "am I done?" by hand. Sends the diff to the model, so it honors the
/// same cloud-handoff refusal as `/iterate`.
async fn cmd_plan_validate(state: &AppState, spec_path: &Path) -> Result<()> {
    let spec = match fs::read_to_string(spec_path) {
        Ok(s) => s,
        Err(_) => {
            println!(
                "  {DIM}No spec yet at {} — run /plan <intent> to create one.{RESET}",
                spec_path.display()
            );
            return Ok(());
        }
    };
    let criteria = parse_done_criteria(&spec);
    if criteria.is_empty() {
        println!(
            "  {DIM}No Done Criteria found in {} — nothing to validate.{RESET}",
            spec_path.display()
        );
        return Ok(());
    }
    if should_refuse_cloud_handoff(state.backend.name, state.config.rubric.allow_cloud) {
        println!(
            "  {RED}✗{RESET} {DIM}/plan validate sends the working diff to the model — run on a local backend or set rubric.allowCloud.{RESET}"
        );
        return Ok(());
    }

    let model = state
        .config
        .iterate
        .evaluator_model
        .clone()
        .unwrap_or_else(|| state.model.clone());
    println!(
        "  {DIM}checking {} Done Criteria against the working tree with {}{RESET}",
        criteria.len(),
        model
    );
    let diff = collect_diff_context(&state.config.workspace_root);
    let check = run_done_check(state, &model, &criteria, &diff).await;
    println!();
    print!("{}", render_validate_report(&criteria, &check.met));
    Ok(())
}

/// Render the Done-Criteria checklist for `/plan validate`. Pure for testing.
fn render_validate_report(criteria: &[String], met: &[bool]) -> String {
    let mut out = String::new();
    for (i, c) in criteria.iter().enumerate() {
        let ok = met.get(i).copied().unwrap_or(false);
        let (mark, color) = if ok { ("✓", GREEN) } else { ("✗", RED) };
        out.push_str(&format!("  {color}{mark}{RESET} {c}\n"));
    }
    let met_count = met.iter().filter(|m| **m).count();
    out.push_str(&format!(
        "  {DIM}{}/{} criteria met{RESET}\n",
        met_count,
        criteria.len()
    ));
    out
}

fn print_shipcheck(
    snapshot: &ShipcheckSnapshot,
    freshness: Option<&crate::project_memory::ProjectIndexFreshness>,
) {
    let status_color = if snapshot.is_clean() { GREEN } else { YELLOW };
    let status = if snapshot.is_clean() {
        "clean"
    } else {
        "dirty"
    };
    println!("  {DIM}shipcheck{RESET}       {status_color}{status}{RESET}");
    println!(
        "  {DIM}branch{RESET}          {CYAN}{}{RESET}",
        snapshot.branch_label()
    );
    if snapshot.branch.behind > 0 {
        println!(
            "  {YELLOW}!{RESET} {DIM}branch is behind upstream by {} commit(s).{RESET}",
            snapshot.branch.behind
        );
    }
    if snapshot.conflict_count() > 0 {
        println!(
            "  {RED}✗{RESET} {DIM}{} conflicted file(s) need attention before release.{RESET}",
            snapshot.conflict_count()
        );
    }
    println!(
        "  {DIM}files{RESET}           staged={} unstaged={} untracked={} conflicts={}",
        snapshot.staged_count(),
        snapshot.unstaged_count(),
        snapshot.untracked_count(),
        snapshot.conflict_count()
    );
    print_shipcheck_file_preview(snapshot);
    print_diff_stat("stagedDiff", &snapshot.staged_diff_stat);
    print_diff_stat("unstagedDiff", &snapshot.unstaged_diff_stat);

    if let Some(test_status) = &snapshot.test_status {
        let test_color = if test_status.failed > 0 || test_status.exit_code != 0 {
            RED
        } else {
            GREEN
        };
        println!(
            "  {DIM}tests{RESET}           framework={CYAN}{}{RESET} total={} passed={} failed={} skipped={} {test_color}exit_code={}{}{RESET}",
            test_status.framework,
            test_status.total,
            test_status.passed,
            test_status.failed,
            test_status.skipped,
            test_status.exit_code,
            if test_status.failed > 0 || test_status.exit_code != 0 { " [FAILED]" } else { " [PASSED]" }
        );
        if let Some(error) = &test_status.error {
            println!("  {RED}✗{RESET} {DIM}test execution error: {error}{RESET}");
        }
    } else {
        println!("  {DIM}tests{RESET}           not run (use --tests flag)");
    }

    print_project_memory_freshness(freshness);
}

fn print_shipcheck_file_preview(snapshot: &ShipcheckSnapshot) {
    if snapshot.files.is_empty() {
        return;
    }
    for file in snapshot.files.iter().take(8) {
        let origin = file
            .original_path
            .as_ref()
            .map(|path| format!(" from {path}"))
            .unwrap_or_default();
        println!(
            "  {DIM}change{RESET}          {}{origin} {DIM}({}){RESET}",
            file.path,
            file_status_label(file)
        );
    }
    if snapshot.files.len() > 8 {
        println!(
            "  {DIM}change{RESET}          …and {} more{RESET}",
            snapshot.files.len() - 8
        );
    }
}

fn print_diff_stat(label: &str, stat: &str) {
    if stat.trim().is_empty() {
        println!("  {DIM}{label}{RESET}      none");
    } else {
        println!("  {DIM}{label}{RESET}");
        for line in stat.lines() {
            println!("    {DIM}{line}{RESET}");
        }
    }
}

fn print_project_memory_freshness(
    freshness: Option<&crate::project_memory::ProjectIndexFreshness>,
) {
    match freshness {
        Some(report) if report.indexed_files > 0 || report.workspace_files > 0 => {
            let color = if report.is_fresh() { GREEN } else { YELLOW };
            println!(
                "  {DIM}projectMemory{RESET}   {color}{} fresh{RESET} · stale={} missing={} deleted={} errors={}",
                report.fresh,
                report.stale,
                report.missing,
                report.deleted,
                report.read_errors
            );
        }
        Some(_) => println!("  {DIM}projectMemory{RESET}   not indexed"),
        None => println!("  {DIM}projectMemory{RESET}   disabled"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::BackendDescriptor;
    use crate::config::AgentConfig;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    fn test_state(root: &Path) -> AppState {
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

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test User"]);
        fs::write(dir.join("README.md"), "hello\n").unwrap();
        git(dir, &["add", "README.md"]);
        git(dir, &["commit", "-m", "initial"]);
    }

    #[test]
    fn parses_handoff_export_and_cloud_args() {
        let args = parse_handoff_args("export .sessions/handoff/manual.md --cloud").unwrap();
        assert!(args.export);
        assert!(args.allow_cloud);
        assert_eq!(
            args.explicit_path.as_deref(),
            Some(Path::new(".sessions/handoff/manual.md"))
        );
        assert!(parse_handoff_args("unexpected").is_none());
    }

    #[tokio::test]
    async fn handoff_noops_without_changes_or_ahead_commits() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let state = test_state(dir.path());

        cmd_handoff("export", &state).await.unwrap();

        assert!(!dir.path().join(".sessions/handoff").exists());
    }

    #[tokio::test]
    async fn handoff_export_writes_fallback_when_model_fails() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        fs::write(dir.path().join("README.md"), "changed\n").unwrap();
        let out_path = dir.path().join("handoff.md");
        let mut state = test_state(dir.path());
        state.backend = BackendDescriptor {
            name: BackendName::Ollama,
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: "test".into(),
            is_local: true,
            openrouter: crate::backends::OpenRouterConfig::default(),
        };

        cmd_handoff(&format!("export {}", out_path.display()), &state)
            .await
            .unwrap();

        let body = fs::read_to_string(out_path).unwrap();
        assert!(body.contains("## Commit Message"));
        assert!(body.contains("## Changelog Bullets"));
        assert!(body.contains("## X Post"));
        assert!(body.contains("## Testing"));
        assert!(body.contains("model draft failed"));
    }

    #[test]
    fn parses_plan_args_variants() {
        assert!(matches!(
            parse_plan_args("show"),
            Some(PlanInvocation::Show)
        ));
        assert!(matches!(
            parse_plan_args("validate"),
            Some(PlanInvocation::Validate)
        ));
        assert!(matches!(
            parse_plan_args("validate the csv export"),
            Some(PlanInvocation::Draft { .. })
        ));
        assert!(matches!(
            parse_plan_args("status"),
            Some(PlanInvocation::Status)
        ));
        assert!(matches!(
            parse_plan_args("route --planner high build oauth"),
            Some(PlanInvocation::Route {
                planner: Some(PlannerChoice::Tier(TaskComplexity::High)),
                ..
            })
        ));
        assert!(matches!(
            parse_plan_args("execute --yolo --max 2"),
            Some(PlanInvocation::Execute(PlanExecuteArgs {
                yolo: true,
                max_tasks: Some(2)
            }))
        ));

        let Some(PlanInvocation::Draft {
            intent,
            export_path,
        }) = parse_plan_args("add a CSV export command")
        else {
            panic!("expected draft");
        };
        assert_eq!(intent, "add a CSV export command");
        assert!(export_path.is_none());

        let Some(PlanInvocation::Draft {
            intent,
            export_path,
        }) = parse_plan_args("build a dashboard --export /tmp/out.md")
        else {
            panic!("expected draft");
        };
        assert_eq!(intent, "build a dashboard");
        assert_eq!(export_path.as_deref(), Some(Path::new("/tmp/out.md")));

        let Some(PlanInvocation::Draft { export_path, .. }) =
            parse_plan_args("thing --export=/tmp/x.md")
        else {
            panic!("expected draft");
        };
        assert_eq!(export_path.as_deref(), Some(Path::new("/tmp/x.md")));

        assert!(parse_plan_args("").is_none());
        assert!(parse_plan_args("   ").is_none());
        assert!(parse_plan_args("intent --export").is_none());
        assert!(parse_plan_args("--export=/tmp/x.md").is_none());
    }

    #[test]
    fn render_validate_report_marks_met_and_unmet() {
        let criteria = vec![
            "retries on 5xx".to_string(),
            "retries are logged".to_string(),
        ];
        let out = render_validate_report(&criteria, &[true, false]);
        assert!(out.contains("✓"));
        assert!(out.contains("✗"));
        assert!(out.contains("retries on 5xx"));
        assert!(out.contains("retries are logged"));
        assert!(out.contains("1/2 criteria met"));
    }

    #[tokio::test]
    async fn plan_writes_fallback_spec_when_model_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        state.backend = BackendDescriptor {
            name: BackendName::Ollama,
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: "test".into(),
            is_local: true,
            openrouter: crate::backends::OpenRouterConfig::default(),
        };

        cmd_plan("add a CSV export command", &mut state)
            .await
            .unwrap();

        let body = fs::read_to_string(dir.path().join(".small-harness/spec.md")).unwrap();
        for section in [
            "## Goal",
            "## User Outcomes",
            "## Scope",
            "## Out of Scope",
            "## Done Criteria",
            "## Open Questions",
        ] {
            assert!(body.contains(section), "missing {section}");
        }
        assert!(body.contains("add a CSV export command"));
        assert!(body.contains("model draft failed"));
    }

    #[tokio::test]
    async fn plan_export_overrides_default_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        state.backend = BackendDescriptor {
            name: BackendName::Ollama,
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: "test".into(),
            is_local: true,
            openrouter: crate::backends::OpenRouterConfig::default(),
        };
        let out_path = dir.path().join("nested/custom-spec.md");

        cmd_plan(
            &format!("build a dashboard --export {}", out_path.display()),
            &mut state,
        )
        .await
        .unwrap();

        assert!(out_path.exists());
        assert!(!dir.path().join(".small-harness/spec.md").exists());
        assert!(fs::read_to_string(out_path)
            .unwrap()
            .contains("## Done Criteria"));
    }

    #[tokio::test]
    async fn plan_show_without_spec_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        cmd_plan("show", &mut state).await.unwrap();
        assert!(!dir.path().join(".small-harness/spec.md").exists());
    }
}
