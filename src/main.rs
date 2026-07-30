mod agent;
mod agent_eval;
#[cfg(test)]
mod agent_integration_test;
mod app_state;
mod approval;
mod auth;
mod auto_loop;
mod backends;
mod banner;
mod batch_operations;
mod budget;
mod cancel;
mod capabilities;
mod catalog;
mod codex_oauth;
mod codex_responses;
mod commands;
mod config;
mod context_guard;
mod continuation;
mod crash_log;
mod diff_view;
mod dir_migration;
mod fable_usage;
mod fix_loop;
mod handoff;
mod hardware;
mod hooks;
mod input;
mod iterate_loop;
mod loader;
mod markdown;
mod mcp;
mod model_system;
mod openai;
mod path_security;
mod planner;
mod playground;
mod project_memory;
mod prompt_library;
mod recommend;
mod renderer;
mod route_audit;
mod rubric;
mod scorecard;
mod session;
mod session_paths;
mod session_turn;
mod setup;
mod shipcheck;
mod test_integration;
mod theme;
mod tools;
mod turn_checkpoint;
mod turn_trace;
mod update_check;
mod warmup;
mod xai_oauth;

use std::io::{IsTerminal, Read, Write};

use crate::app_state::AppState;
use crate::approval::ApprovalCache;
use crate::backends::{default_model, validate, BackendName};
use crate::banner::{print_banner, BannerInfo};
use crate::commands::dispatch;
use crate::config::load_config;
use crate::hooks::{
    build_hook_registry, dispatch_hook_payload, hook_context_messages, hook_state_file_path,
    load_hook_state_file_from, render_hook_context_block, HookEventName, HookInvocationContext,
    HookNotice, HookNoticeLevel, HookRegistry, HookTrustStatus,
};
use crate::input::{plain_read_line_with_history_outcome, InputHistory, ReadLineOutcome};
use crate::project_memory::{build_project_index, load_project_index, prompt_looks_repo_related};
use crate::renderer::TuiRenderer;
use crate::session::{init_session_dir, load_session_metadata, new_session_path};
use crate::session_paths::{apply_path_session_state, PathStore};
use crate::session_turn::{
    dispatch_app_hook, run_user_turn, system_prompt_with_hook_context,
    updated_prompt_from_hook_input, TurnOptions,
};
use crate::tools::{build_tools_for_names, select_tool_names};
use crate::turn_checkpoint::CheckpointStack;
use crate::warmup::warmup;

const RESET: crate::theme::Style = crate::theme::RESET;
const DIM: crate::theme::Style = crate::theme::MUTED;
const GREEN: crate::theme::Style = crate::theme::SUCCESS;
const YELLOW: crate::theme::Style = crate::theme::WARN;
const RED: crate::theme::Style = crate::theme::ERROR;

struct CliOneShot {
    prompt: String,
    allow_tools: bool,
}

struct CliEval {
    fixture_id: String,
    model: Option<String>,
    json_output: bool,
}

struct NonInteractiveApproval {
    allow: bool,
}

#[async_trait::async_trait]
impl crate::agent::ApprovalProvider for NonInteractiveApproval {
    async fn approve(
        &mut self,
        _name: &str,
        _args: &serde_json::Value,
        _preview: Option<&crate::tools::ToolPreview>,
    ) -> bool {
        self.allow
    }
}

fn hook_context_for_session(
    config: &crate::config::AgentConfig,
    backend: &crate::backends::BackendDescriptor,
    model: &str,
    session_path: &std::path::Path,
    source: &str,
    turn_id: u32,
) -> HookInvocationContext {
    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| crate::session_paths::workspace_root_path(config))
        .display()
        .to_string();
    HookInvocationContext {
        session_id: session_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("session")
            .to_string(),
        turn_id,
        cwd,
        workspace_root: crate::session_paths::workspace_root_path(config)
            .display()
            .to_string(),
        transcript_path: session_path.display().to_string(),
        events_path: crate::turn_trace::events_path_for_session(session_path)
            .display()
            .to_string(),
        backend: backend.name.as_str().into(),
        model: model.into(),
        approval_policy: config.approval_policy.as_str().into(),
        source: source.into(),
    }
}

fn write_hook_notices(out: &mut impl Write, notices: &[HookNotice]) {
    for notice in notices {
        let label = match notice.level {
            HookNoticeLevel::Warning => "hook warning",
            HookNoticeLevel::Blocked => "hook blocked",
            HookNoticeLevel::Denied => "hook denied",
            HookNoticeLevel::Stopped => "hook stopped",
            HookNoticeLevel::Feedback => "hook",
        };
        let _ = writeln!(
            out,
            "\n[{label} {}] {}",
            notice.event.as_str(),
            notice.message
        );
    }
}

fn write_hook_review_notice(out: &mut impl Write, hooks: &HookRegistry) {
    let review_needed = hooks
        .entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.trust_status,
                HookTrustStatus::Untrusted | HookTrustStatus::Modified
            )
        })
        .count();
    if review_needed > 0 {
        let _ = writeln!(
            out,
            "  {YELLOW}!{RESET} {DIM}{review_needed} hook(s) need review and will be skipped. Run /hooks to inspect or trust them.{RESET}"
        );
    }
    let invalid = hooks
        .entries
        .iter()
        .filter(|entry| matches!(entry.trust_status, HookTrustStatus::Invalid))
        .count();
    if invalid > 0 {
        let _ = writeln!(
            out,
            "  {YELLOW}!{RESET} {DIM}{invalid} hook(s) have invalid matchers and will be skipped. Run /hooks to inspect them.{RESET}"
        );
    }
}

async fn shutdown_interactive_session(state: &mut AppState) -> anyhow::Result<()> {
    if state.path_store.dirty {
        let current = PathStore::capture_state(state, &state.workspace_root())?;
        let _ = state.path_store.flush_if_dirty(current);
    }
    let _ = state.save_active_path_metadata();
    let _ = dispatch_app_hook(state, HookEventName::SessionEnd, None, None).await;
    Ok(())
}

/// Returns the shell name if the user invoked `albatross completions <shell>`.
/// Recognized shells: bash, zsh, fish.
fn parse_completions_arg() -> Option<String> {
    let mut args = std::env::args().skip(1);
    let first = args.next()?;
    if first != "completions" {
        return None;
    }
    args.next()
}

fn print_usage() {
    println!("albatross {}", env!("CARGO_PKG_VERSION"));
    println!("A small, terminal-first coding harness.");
    println!();
    println!("USAGE:");
    println!("  albatross                      Start an interactive session");
    println!("  albatross --print <text>       Run one prompt and exit (also reads stdin)");
    println!("  albatross --eval <fixture>       Run an agent eval fixture and exit");
    println!("  albatross --continue           Resume the most recent session here");
    println!("  albatross completions <shell>  Print a completion script (bash|zsh|fish)");
    println!();
    println!("FLAGS:");
    println!("  --allow-tools, --yes   Auto-approve tool calls in one-shot mode");
    println!("  -V, --version          Print version and exit");
    println!("  -h, --help             Print this help and exit");
}

fn run_completions(shell: &str) -> anyhow::Result<()> {
    let script = match shell {
        "bash" => COMPLETIONS_BASH,
        "zsh" => COMPLETIONS_ZSH,
        "fish" => COMPLETIONS_FISH,
        other => {
            eprintln!("unsupported shell: {other} (try: bash, zsh, fish)");
            std::process::exit(2);
        }
    };
    print!("{script}");
    Ok(())
}

const COMPLETIONS_BASH: &str = r#"# albatross bash completion
_albatross() {
    local cur prev
    COMPREPLY=()
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"

    if [[ ${COMP_CWORD} -eq 1 ]]; then
        COMPREPLY=( $(compgen -W "--print -p --continue -c --allow-tools --yes completions" -- "$cur") )
        return 0
    fi
    if [[ "$prev" == "completions" ]]; then
        COMPREPLY=( $(compgen -W "bash zsh fish" -- "$cur") )
        return 0
    fi
}
complete -F _albatross albatross
"#;

const COMPLETIONS_ZSH: &str = r#"#compdef albatross
# albatross zsh completion

_albatross() {
    local -a opts
    opts=(
        '--print[Run one-shot with prompt]:prompt'
        '-p[Run one-shot with prompt]:prompt'
        '--continue[Resume the latest session]'
        '-c[Resume the latest session]'
        '--allow-tools[Auto-approve tool calls in one-shot mode]'
        '--yes[Auto-approve tool calls in one-shot mode]'
        'completions[Emit a shell completion script]:shell:(bash zsh fish)'
    )
    _arguments $opts
}

compdef _albatross albatross
"#;

const COMPLETIONS_FISH: &str = r#"# albatross fish completion
complete -c albatross -l print -s p -d 'Run one-shot with prompt'
complete -c albatross -l continue -s c -d 'Resume the latest session'
complete -c albatross -l allow-tools -d 'Auto-approve tool calls in one-shot mode'
complete -c albatross -l yes -d 'Auto-approve tool calls in one-shot mode'
complete -c albatross -n '__fish_use_subcommand' -a completions -d 'Emit a shell completion script'
complete -c albatross -n '__fish_seen_subcommand_from completions' -a 'bash zsh fish' -d 'Shell flavor'
"#;

/// Returns true if the user passed `--continue` or `-c`. Only consulted when
/// no one-shot prompt was given — `--continue` is interactive-mode only.
fn should_continue_latest() -> bool {
    std::env::args()
        .skip(1)
        .any(|a| a == "--continue" || a == "-c")
}

fn parse_eval_args() -> Option<anyhow::Result<CliEval>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut fixture_id = None;
    let mut model = None;
    let mut json_output = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--eval" => {
                fixture_id = args.get(i + 1).cloned();
                i += 2;
            }
            "--model" => {
                model = args.get(i + 1).cloned();
                i += 2;
            }
            "--json" => {
                json_output = true;
                i += 1;
            }
            _ => i += 1,
        }
    }
    fixture_id.map(|fixture_id| {
        Ok(CliEval {
            fixture_id,
            model,
            json_output,
        })
    })
}

/// Companion to the config-directory migration, for the per-project scratch
/// directory. Needs a loaded config because `workspace_root` is configurable.
fn migrate_workspace_scratch(workspace_root: &str) {
    if let Some(migration) = crate::config::migrate_legacy_workspace_dir(workspace_root) {
        eprintln!("workspace scratch: {}", migration.describe());
    }
}

async fn run_eval_cli(opts: CliEval) -> anyhow::Result<()> {
    let config = load_config();
    crate::theme::init(config.display.color, config.display.ascii);
    let code = crate::agent_eval::run_eval_cli(
        &config,
        &opts.fixture_id,
        opts.model.as_deref(),
        opts.json_output,
    )
    .await?;
    std::process::exit(code);
}

fn parse_one_shot_args() -> Option<anyhow::Result<CliOneShot>> {
    let mut args = std::env::args().skip(1);
    let mut prompt = None;
    let mut allow_tools = false;
    while let Some(arg) = args.next() {
        if arg == "--allow-tools" || arg == "--yes" {
            allow_tools = true;
        } else if arg == "--print" || arg == "-p" {
            prompt = args.next();
        } else if let Some(rest) = arg.strip_prefix("--print=") {
            prompt = Some(rest.to_string());
        }
    }
    if let Some(prompt) = prompt {
        return Some(Ok(CliOneShot {
            prompt,
            allow_tools,
        }));
    }
    if !std::io::stdin().is_terminal() {
        let mut input = String::new();
        if let Err(e) = std::io::stdin().read_to_string(&mut input) {
            return Some(Err(e.into()));
        }
        return Some(Ok(CliOneShot {
            prompt: input,
            allow_tools,
        }));
    }
    None
}

async fn run_one_shot(opts: CliOneShot) -> anyhow::Result<()> {
    use crate::agent::{run_agent, AgentEvent, AgentHooks};
    use crate::openai::ChatMessage;
    use crate::project_memory::render_system_prompt_with_memory;
    use crate::session::save_message;
    use crate::tools::{build_tools_for_names, ToolRuntimeContext};

    let mut prompt = opts.prompt.trim().to_string();
    if prompt.is_empty() {
        anyhow::bail!("one-shot prompt is empty");
    }
    let config = load_config();
    crate::theme::init(config.display.color, config.display.ascii);
    migrate_workspace_scratch(&config.workspace_root);
    let http = crate::openai::build_http_client();
    let backend_desc = config.backend_descriptor();
    validate(&backend_desc)?;
    let model = default_model(&backend_desc, config.model_override.as_deref());
    init_session_dir(&config.session_dir)?;
    let session_path = new_session_path(&config.session_dir);
    let trace = crate::turn_trace::shared_trace(&session_path, config.display.event_log.enabled)?;
    if let Ok(mut trace_guard) = trace.lock() {
        trace_guard.begin_turn();
    }
    let hooks = load_runtime_hooks(&config)?;
    let hook_context =
        hook_context_for_session(&config, &backend_desc, &model, &session_path, "one-shot", 1);
    let mut out = std::io::stdout();
    write_hook_review_notice(&mut out, &hooks);
    let start_payload = hook_context
        .payload(HookEventName::SessionStart)
        .into_value();
    let start_outcome = dispatch_hook_payload(
        &hooks,
        HookEventName::SessionStart,
        &start_payload,
        None,
        Some(trace.clone()),
    )
    .await;
    write_hook_notices(&mut out, &start_outcome.notices);
    if let Some(reason) = start_outcome.blocking_reason.as_deref() {
        anyhow::bail!("SessionStart hook blocked session: {reason}");
    }
    if let Some(reason) = start_outcome.stop_reason.as_deref() {
        anyhow::bail!("SessionStart hook stopped session: {reason}");
    }

    let prompt_payload = hook_context
        .payload(HookEventName::UserPromptSubmit)
        .insert("prompt", serde_json::json!(prompt.clone()))
        .into_value();
    let prompt_outcome = dispatch_hook_payload(
        &hooks,
        HookEventName::UserPromptSubmit,
        &prompt_payload,
        None,
        Some(trace.clone()),
    )
    .await;
    write_hook_notices(&mut out, &prompt_outcome.notices);
    if let Some(reason) = prompt_outcome.blocking_reason {
        anyhow::bail!("UserPromptSubmit hook blocked prompt: {reason}");
    }
    if let Some(reason) = prompt_outcome.stop_reason {
        anyhow::bail!("UserPromptSubmit hook stopped prompt: {reason}");
    }
    if let Some(updated) = prompt_outcome
        .updated_input
        .as_ref()
        .and_then(updated_prompt_from_hook_input)
    {
        prompt = updated.trim().to_string();
        if prompt.is_empty() {
            anyhow::bail!("UserPromptSubmit hook rewrote prompt to empty");
        }
    }
    if config.project_memory.enabled
        && config.project_memory.auto_index
        && prompt_looks_repo_related(&prompt)
        && load_project_index(&config).ok().flatten().is_none()
    {
        let _ = build_project_index(&config);
    }
    let active_tool_names = select_tool_names(&config, &prompt);
    let mut hook_contexts = hook_context_messages(HookEventName::SessionStart, &start_outcome);
    hook_contexts.extend(hook_context_messages(
        HookEventName::UserPromptSubmit,
        &prompt_outcome,
    ));
    let system_prompt = system_prompt_with_hook_context(
        render_system_prompt_with_memory(&config, &backend_desc, &active_tool_names, &prompt),
        &hook_contexts,
    );
    let messages = vec![
        ChatMessage::System {
            content: system_prompt,
        },
        ChatMessage::User {
            content: prompt.clone().into(),
        },
    ];
    let mut approval = NonInteractiveApproval {
        allow: opts.allow_tools,
    };
    let agent_hooks = AgentHooks {
        registry: hooks.clone(),
        context: hook_context.clone(),
        trace: trace.clone(),
    };
    let tool_runtime = ToolRuntimeContext {
        trace: trace.clone(),
        trace_enabled: false,
        agent_events: None,
        hooks: Some(agent_hooks.clone()),
    };
    let tools = build_tools_for_names(&config, &active_tool_names, Some(&tool_runtime));
    let result = run_agent(
        &http,
        &backend_desc,
        &model,
        None,
        messages,
        tools,
        config.max_steps,
        |event| match event {
            AgentEvent::Text { delta } => {
                let _ = out.write_all(delta.as_bytes());
                let _ = out.flush();
            }
            AgentEvent::Reasoning { delta } if config.display.reasoning => {
                let _ = writeln!(out, "\n[reasoning] {delta}");
            }
            AgentEvent::ToolCall { name, .. } => {
                let _ = writeln!(out, "\n[tool] {name}");
            }
            AgentEvent::ToolResult { name, output, .. } => {
                let _ = writeln!(out, "\n[tool-result] {name}: {output}");
            }
            AgentEvent::ContextCompacted { notice, .. } => {
                let _ = writeln!(out, "\n{notice}");
            }
            AgentEvent::StepLimitReached { max_steps } => {
                let _ = writeln!(
                    out,
                    "\n[stopped after {max_steps} steps — task may be unfinished]"
                );
            }
            AgentEvent::HookNotice(notice) => {
                write_hook_notices(&mut out, &[notice]);
            }
            _ => {}
        },
        Some(&mut approval as &mut dyn crate::agent::ApprovalProvider),
        None,
        None,
        None,
        None,
        0,
        Some(agent_hooks),
    )
    .await?;
    for message in &result.messages {
        let _ = save_message(&session_path, message);
    }
    let stop_payload = hook_context
        .payload(HookEventName::Stop)
        .insert("metrics", serde_json::json!(result.metrics.clone()))
        .insert("input_tokens", serde_json::json!(result.input_tokens))
        .insert("output_tokens", serde_json::json!(result.output_tokens))
        .insert("hit_step_limit", serde_json::json!(result.hit_step_limit))
        .into_value();
    let stop_outcome = dispatch_hook_payload(
        &hooks,
        HookEventName::Stop,
        &stop_payload,
        None,
        Some(trace.clone()),
    )
    .await;
    write_hook_notices(&mut out, &stop_outcome.notices);
    if let Some(content) =
        render_hook_context_block(hook_context_messages(HookEventName::Stop, &stop_outcome))
    {
        let message = ChatMessage::User {
            content: content.into(),
        };
        let _ = save_message(&session_path, &message);
    }
    let end_payload = hook_context.payload(HookEventName::SessionEnd).into_value();
    let end_outcome = dispatch_hook_payload(
        &hooks,
        HookEventName::SessionEnd,
        &end_payload,
        None,
        Some(trace),
    )
    .await;
    write_hook_notices(&mut out, &end_outcome.notices);
    println!();
    Ok(())
}

fn load_runtime_hooks(
    config: &crate::config::AgentConfig,
) -> anyhow::Result<crate::hooks::HookRegistry> {
    let managed_env = std::env::var("ALBATROSS_MANAGED_HOOKS_JSON").ok();
    let managed_file = std::env::var("ALBATROSS_MANAGED_HOOKS_FILE").ok();
    let managed =
        crate::hooks::load_managed_hooks_from_env(managed_env.as_deref(), managed_file.as_deref())?;
    let hook_state = match hook_state_file_path() {
        Some(path) => load_hook_state_file_from(&path).unwrap_or_else(|e| {
            eprintln!("  {YELLOW}!{RESET} {DIM}hook trust state ignored: {e}{RESET}");
            crate::hooks::HookStateFile::default()
        }),
        None => crate::hooks::HookStateFile::default(),
    };
    let project_root = crate::session_paths::workspace_root_path(config)
        .display()
        .to_string();
    Ok(build_hook_registry(
        &config.hooks,
        managed.as_ref(),
        &hook_state,
        &project_root,
    ))
}

fn prompt_fingerprint(
    backend: &crate::backends::BackendDescriptor,
    model: &str,
    effort: Option<crate::model_system::EffortLevel>,
    system_prompt: &str,
    tool_names: &[String],
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    backend.name.hash(&mut hasher);
    backend.base_url.hash(&mut hasher);
    model.hash(&mut hasher);
    effort.hash(&mut hasher);
    system_prompt.hash(&mut hasher);
    tool_names.hash(&mut hasher);
    hasher.finish()
}

async fn probe_backend(
    http: &reqwest::Client,
    b: &crate::backends::BackendDescriptor,
) -> Result<(), String> {
    use crate::openai::list_models;
    match list_models(http, b).await {
        Ok(_) => Ok(()),
        Err(e) => {
            let hint = match b.name {
                crate::backends::BackendName::Ollama => {
                    "Is `ollama serve` running? Default port is 11434."
                }
                crate::backends::BackendName::LmStudio => {
                    "Open LM Studio → \"Local Server\" tab → Start Server. Default port is 1234."
                }
                crate::backends::BackendName::Mlx => {
                    "Start an MLX OpenAI-compatible server (e.g. `mlx_lm.server`). Default port is 8080."
                }
                crate::backends::BackendName::LlamaCpp => {
                    "Start `llama-server -m /path/to/model.gguf --host 127.0.0.1 --port 8080`. Use `--jinja` for native tool calls."
                }
                crate::backends::BackendName::Openrouter => "Check OPENROUTER_API_KEY.",
                crate::backends::BackendName::OpenAi => {
                    "Check OPENAI_API_KEY (or OPENAI_BASE_URL if you're targeting a compatible proxy)."
                }
                crate::backends::BackendName::OpenAiCodex => {
                    "Run `/login openai-codex` to sign in with ChatGPT/Codex."
                }
                crate::backends::BackendName::Grok => {
                    "Run `/login grok` to sign in with SuperGrok / X Premium+."
                }
            };
            Err(format!("{e}. {hint}"))
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Handle informational flags first — these must never touch config or
    // validate a backend (otherwise `--version` fails when no API key is set).
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("albatross {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return Ok(());
    }

    // Runs before anything reads the config directory, so a pre-rename install
    // carries its credentials over instead of looking logged out.
    if let Some(migration) = crate::auth::migrate_legacy_config_dir() {
        eprintln!("config: {}", migration.describe());
    }
    crate::auth::hydrate_env_from_file();
    crate::crash_log::install_panic_hook();
    if let Some(shell) = parse_completions_arg() {
        return run_completions(&shell);
    }
    if let Some(opts) = parse_eval_args() {
        return run_eval_cli(opts?).await;
    }
    if let Some(opts) = parse_one_shot_args() {
        return run_one_shot(opts?).await;
    }
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "albatross requires an interactive TTY (run it directly in a terminal, not piped)."
        );
        std::process::exit(1);
    }
    let setup_base = load_config();
    crate::theme::init(setup_base.display.color, setup_base.display.ascii);
    let _ = setup::maybe_run_first_run_setup(&setup_base).await?;
    let config = load_config();
    crate::theme::init(config.display.color, config.display.ascii);
    migrate_workspace_scratch(&config.workspace_root);
    let http = crate::openai::build_http_client();
    let backend_desc = config.backend_descriptor();
    let missing_oauth_login = config.backend.is_oauth_login()
        && match config.backend {
            BackendName::OpenAiCodex => crate::auth::AuthStore::load()
                .get_oauth("openai-codex")
                .is_none(),
            BackendName::Grok => crate::auth::AuthStore::load()
                .get_oauth(crate::xai_oauth::PROVIDER)
                .is_none(),
            _ => false,
        };
    if let Err(e) = validate(&backend_desc) {
        if missing_oauth_login {
            let login_cmd = match config.backend {
                BackendName::Grok => "/login grok",
                _ => "/login openai-codex",
            };
            println!("  {YELLOW}!{RESET} {DIM}{e}{RESET}");
            println!(
                "  {DIM}Starting anyway so you can run {login_cmd}, or /backend to switch.{RESET}"
            );
        } else {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
    let model = default_model(&backend_desc, config.model_override.as_deref());

    if config.display.show_banner {
        print_banner(BannerInfo {
            backend: config.backend.as_str(),
            model: &model,
            approval: config.approval_policy.as_str(),
        });
        if let Some(notice) = crate::update_check::pending_notice(env!("CARGO_PKG_VERSION")) {
            println!("  {YELLOW}↑{RESET} {DIM}{notice}{RESET}");
        }
    }

    // Refresh the update-check cache in the background. Failures are silent
    // and the result lands in time for the next launch; we never block the
    // user's first prompt on a GitHub API call.
    {
        let http_bg = http.clone();
        tokio::spawn(async move {
            crate::update_check::refresh_cache_if_stale(&http_bg).await;
        });
    }

    let mut warmed_fingerprint = None;
    let probe = if missing_oauth_login {
        Err(match config.backend {
            BackendName::Grok => "Run /login grok to sign in with SuperGrok / X Premium+.".into(),
            _ => "Run /login openai-codex to sign in with ChatGPT/Codex.".into(),
        })
    } else {
        probe_backend(&http, &backend_desc).await
    };
    if let Err(hint) = probe {
        println!("  {YELLOW}!{RESET} {DIM}Backend not reachable: {hint}{RESET}");
        println!("  {DIM}You can still type /backend to switch, or fix and retry.{RESET}");
    } else if std::env::var("WARMUP").as_deref() != Ok("false") {
        let warmup_tool_names = select_tool_names(&config, "");
        let warmup_tools_vec = build_tools_for_names(&config, &warmup_tool_names, None);
        let warmup_tool_defs = crate::agent::to_openai_tools(&warmup_tools_vec);
        let warmup_prompt = config.render_system_prompt_for_tools(&warmup_tool_names);
        let loader = crate::loader::Loader::start("Warming up".into(), config.display.loader_style);
        match warmup(
            &http,
            &backend_desc,
            &model,
            None,
            &warmup_prompt,
            &warmup_tool_defs,
        )
        .await
        {
            Ok(ms) => {
                warmed_fingerprint = Some(prompt_fingerprint(
                    &backend_desc,
                    &model,
                    None,
                    &warmup_prompt,
                    &warmup_tool_names,
                ));
                loader.stop();
                println!(
                    "  {DIM}warmed up in {:.1}s — first prompt should be fast{RESET}",
                    ms as f64 / 1000.0
                );
            }
            Err(e) => {
                loader.stop();
                println!("  {DIM}warmup skipped: {e}{RESET}");
            }
        }
    }

    init_session_dir(&config.session_dir)?;
    crate::crash_log::set_crash_dir(&config.session_dir);
    let mut input_history = InputHistory::load(
        config.history_path(),
        config.history.max_entries,
        config.history.enabled,
    );
    // Slash commands (name + description) for the completion menu.
    let command_names = crate::commands::command_list();
    let session_path = new_session_path(&config.session_dir);
    let session_dir = config.session_dir.clone();
    let paths_config = config.paths.clone();
    let checkpoint_limits = config.checkpoints.limits();
    let checkpoints_enabled = config.checkpoints.enabled;
    let display = config.display.clone();
    let hooks = load_runtime_hooks(&config)?;

    let trace = crate::turn_trace::shared_trace(&session_path, config.display.event_log.enabled)?;
    if let Ok(mut t) = trace.lock() {
        t.begin_turn();
    }

    let mut state = AppState {
        config,
        http,
        backend: backend_desc,
        model,
        active_effort: None,
        active_route: None,
        messages: Vec::new(),
        session_dir: session_dir.clone(),
        session_path: session_path.clone(),
        total_in: 0,
        total_out: 0,
        session_usd: 0.0,
        session_cost_has_unknown: false,
        context_guard_notice: None,
        conversation_summary: None,
        checkpoint_stack: CheckpointStack::new(checkpoint_limits),
        checkpoints_enabled,
        play_session: None,
        last_play_scorecard: None,
        approval_cache: ApprovalCache::new(),
        renderer: TuiRenderer::new(display),
        hooks,
        session_hook_contexts: Vec::new(),
        pending_hook_contexts: Vec::new(),
        warmed_fingerprint,
        tests_ran_this_session: false,
        pending_image_attachments: Vec::new(),
        mcp_tools: Vec::new(),
        path_store: PathStore::new(&session_dir, &session_path, &paths_config),
        trace,
        trace_enabled: false,
    };

    if !state.config.mcp_servers.is_empty() {
        let (trusted, discovery) =
            crate::mcp::trusted_configured(&state.config.mcp_servers, &state.config.workspace_root);
        let pending = discovery
            .iter()
            .filter(|entry| entry.status != crate::mcp::McpTrustStatus::Trusted)
            .count();
        if pending > 0 {
            println!(
                "  {YELLOW}!{RESET} {DIM}MCP: {pending} new or modified server(s) skipped; review with /mcp{RESET}"
            );
        }
        let (tools, errors) = crate::mcp::spawn_configured(&trusted).await;
        if !tools.is_empty() {
            println!(
                "  {DIM}MCP: {} tool(s) loaded from {} server(s){RESET}",
                tools.len(),
                trusted.len() - errors.len()
            );
        }
        for err in &errors {
            println!("  {YELLOW}!{RESET} {DIM}MCP: {err}{RESET}");
        }
        state.mcp_tools = tools;
    }

    if should_continue_latest() {
        match crate::session::resolve_session_path(&state.session_dir, "latest") {
            Ok(Some(path)) => match crate::session::load_messages(&path) {
                Ok(messages) => {
                    state.messages = messages;
                    state.session_path = path.clone();
                    let _ = state.reset_trace_for_session();
                    state.path_store = PathStore::load(
                        &state.session_dir,
                        &state.session_path,
                        &state.config.paths,
                    );
                    let metadata = load_session_metadata(&path).unwrap_or_default();
                    let root = state.workspace_root();
                    if let Some((path_state, report)) = state
                        .path_store
                        .load_resume_state(&root, metadata.active_path_id.as_deref())?
                    {
                        let transcript = state
                            .path_store
                            .transcript_path(state.path_store.active_id());
                        apply_path_session_state(&mut state, &path_state, &transcript);
                        if report.is_partial() {
                            println!(
                                "{YELLOW}!{RESET} {DIM}--continue path restore partial{RESET}"
                            );
                        }
                    }
                    let id = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("session");
                    let path_note = if state.path_store.path_count() > 1 {
                        format!(" · path {}", state.path_store.active_id())
                    } else {
                        String::new()
                    };
                    println!(
                        "{GREEN}{}{RESET} {DIM}continuing{RESET} {} {DIM}({} messages{path_note}){RESET}",
                        crate::theme::OK,
                        id,
                        state.messages.len()
                    );
                }
                Err(e) => println!("{YELLOW}!{RESET} {DIM}--continue: {e}{RESET}"),
            },
            Ok(None) => println!(
                "{YELLOW}!{RESET} {DIM}--continue: no prior session found in {}{RESET}",
                state.session_dir
            ),
            Err(e) => println!("{YELLOW}!{RESET} {DIM}--continue: {e}{RESET}"),
        }
    }

    state.renderer.set_trace(state.trace_enabled);
    let mut stdout = std::io::stdout();
    write_hook_review_notice(&mut stdout, &state.hooks);
    let start_outcome =
        dispatch_app_hook(&mut state, HookEventName::SessionStart, None, None).await;
    if let Some(reason) = start_outcome.blocking_reason.as_deref() {
        anyhow::bail!("SessionStart hook blocked session: {reason}");
    }
    if let Some(reason) = start_outcome.stop_reason.as_deref() {
        anyhow::bail!("SessionStart hook stopped session: {reason}");
    }
    state.session_hook_contexts =
        hook_context_messages(HookEventName::SessionStart, &start_outcome);

    loop {
        // Header-only turn marker (a short fading rule), then a clean accent
        // prompt. The same robust line reader is used regardless of the
        // configured input style.
        println!();
        println!("{}", crate::theme::fade_header("you"));
        let input = match plain_read_line_with_history_outcome(
            format!(
                "{}{}{}{} ",
                crate::theme::PAD,
                crate::theme::ACCENT,
                crate::theme::PROMPT_CHAR,
                RESET
            ),
            input_history.entries().to_vec(),
            command_names.clone(),
        )
        .await?
        {
            ReadLineOutcome::Line(input) => input,
            ReadLineOutcome::Eof | ReadLineOutcome::Interrupted => {
                shutdown_interactive_session(&mut state).await?;
                println!("{}{}bye.{RESET}", crate::theme::PAD, crate::theme::MUTED);
                return Ok(());
            }
        };
        let trimmed = input.trim();
        if trimmed.is_empty() {
            continue;
        }
        let _ = input_history.push(&input);

        if matches!(trimmed, "/exit" | "/quit" | "exit" | "quit" | ".exit") {
            shutdown_interactive_session(&mut state).await?;
            println!("{}{}bye.{RESET}", crate::theme::PAD, crate::theme::MUTED);
            return Ok(());
        }

        if state.config.slash_commands && trimmed.starts_with('/') {
            if let Err(e) = dispatch(trimmed, &mut state).await {
                println!("  {RED}{}{RESET} {DIM}{e}{RESET}", crate::theme::FAIL);
            }
            continue;
        }

        if state.config.project_memory.enabled
            && state.config.project_memory.auto_index
            && prompt_looks_repo_related(trimmed)
            && load_project_index(&state.config).ok().flatten().is_none()
        {
            if let Err(e) = build_project_index(&state.config) {
                println!("  {YELLOW}!{RESET} {DIM}project memory auto-index skipped: {e}{RESET}");
            }
        }

        let auto_verify = state.config.mode == crate::config::OperatorMode::Ship;
        if let Err(e) = run_user_turn(
            &mut state,
            TurnOptions {
                user_prompt: trimmed.to_string(),
                auto_verify_tests: auto_verify,
                yolo_approve: false,
                source: "interactive",
            },
        )
        .await
        {
            println!("  {RED}{}{RESET} {DIM}{e}{RESET}", crate::theme::FAIL);
        }
    }
}
