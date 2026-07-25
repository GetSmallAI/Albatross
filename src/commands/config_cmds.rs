//! Config command group: /config, /backend, /model, /tools, /mode, /verbose, /trace,
//! /reasoning, /auth, /login, /logout, /compare, /image.
//! Split out of mod.rs; dispatch lives in mod.rs.

use super::*;

pub(super) fn cmd_config(state: &AppState) {
    println!(
        "  {DIM}mode{RESET}             {CYAN}{}{RESET}",
        state.config.mode.as_str()
    );
    println!(
        "  {DIM}backend{RESET}          {CYAN}{}{RESET}",
        state.config.backend.as_str()
    );
    println!(
        "  {DIM}model{RESET}            {CYAN}{}{RESET}",
        state.model
    );
    if let Some(effort) = state.active_effort {
        println!(
            "  {DIM}effort{RESET}           {CYAN}{}{RESET}",
            effort.as_str()
        );
    }
    println!(
        "  {DIM}workspaceRoot{RESET}    {}",
        state.config.workspace_root
    );
    println!(
        "  {DIM}outsideWorkspace{RESET} {}",
        state.config.outside_workspace.as_str()
    );
    println!(
        "  {DIM}tools{RESET}            {}",
        state.config.tools.join(", ")
    );
    println!(
        "  {DIM}toolSelection{RESET}    {}",
        state.config.tool_selection.as_str()
    );
    println!(
        "  {DIM}slashCommands{RESET}    {}",
        state.config.slash_commands
    );
    println!(
        "  {DIM}showBanner{RESET}       {}",
        state.config.display.show_banner
    );
    println!(
        "  {DIM}context{RESET}          maxMessages={:?} maxBytes={:?}",
        state.config.context.max_messages, state.config.context.max_bytes
    );
    println!(
        "  {DIM}history{RESET}          enabled={} maxEntries={} path={}",
        state.config.history.enabled,
        state.config.history.max_entries,
        state.config.history_path()
    );
    println!(
        "  {DIM}projectMemory{RESET}    enabled={} autoInject={} autoIndex={} maxFileBytes={} maxInjectedBytes={} allowCloudContext={}",
        state.config.project_memory.enabled,
        state.config.project_memory.auto_inject,
        state.config.project_memory.auto_index,
        state.config.project_memory.max_file_bytes,
        state.config.project_memory.max_injected_bytes,
        state.config.project_memory.allow_cloud_context
    );
    println!(
        "  {DIM}checkpoints{RESET}      enabled={} session={} stack={}/{} maxBytes={}",
        state.config.checkpoints.enabled,
        state.checkpoints_enabled,
        state.checkpoint_stack.len(),
        state.checkpoint_stack.limits.max_turns,
        state.config.checkpoints.max_bytes
    );
    if state.paths_enabled() {
        println!(
            "  {DIM}paths{RESET}            enabled={} active={} count={} maxPaths={}",
            state.config.paths.enabled,
            state.path_store.active_id(),
            state.path_store.path_count(),
            state.config.paths.max_paths
        );
    }
}
pub(super) fn cmd_mode(args: &str, state: &mut AppState) {
    let arg = args.trim();
    if arg.is_empty() || arg == "status" {
        println!(
            "  {DIM}mode{RESET}      {CYAN}{}{RESET}",
            state.config.mode.as_str()
        );
        println!("  {DIM}available{RESET} explore, edit, ship, review, custom");
        println!(
            "  {DIM}tools{RESET}     {} · {DIM}toolSelection{RESET} {} · {DIM}approval{RESET} {} · {DIM}maxSteps{RESET} {}",
            state.config.tools.join(", "),
            state.config.tool_selection.as_str(),
            state.config.approval_policy.as_str(),
            state.config.max_steps
        );
        return;
    }
    let Some(mode) = OperatorMode::parse(arg) else {
        println!("  {DIM}Usage: /mode [explore|edit|ship|review|custom]{RESET}");
        return;
    };
    state.config.apply_operator_mode(mode);
    state.checkpoints_enabled = state.config.checkpoints.enabled;
    println!(
        "  {GREEN}✓{RESET} {DIM}mode →{RESET} {CYAN}{}{RESET} {DIM}tools={} approval={} maxSteps={}{RESET}",
        state.config.mode.as_str(),
        state.config.tools.join(","),
        state.config.approval_policy.as_str(),
        state.config.max_steps
    );
}
pub(super) async fn cmd_auth(args: &str) -> Result<()> {
    use crate::auth::{auth_file_path, env_var_for, mask_key, AuthStore, KNOWN_PROVIDERS};

    let (action, rest) = match args.split_once(' ') {
        Some((a, r)) => (a.trim(), r.trim()),
        None => (args.trim(), ""),
    };

    match action {
        "" | "list" | "status" => {
            print_auth_status();
            Ok(())
        }
        "set" => {
            if rest.is_empty() {
                println!(
                    "  {DIM}usage: /auth set <provider>  (known: {}){RESET}",
                    KNOWN_PROVIDERS
                        .iter()
                        .map(|(n, _)| *n)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                return Ok(());
            }
            let provider = rest.to_lowercase();
            if env_var_for(&provider).is_none() {
                println!("  {RED}✗{RESET} {DIM}unknown provider: {provider}{RESET}");
                return Ok(());
            }
            let key = plain_read_line(format!(
                "  {DIM}Paste {} API key (visible while typing): {RESET}",
                provider
            ))
            .await?
            .trim()
            .to_string();
            if key.is_empty() {
                println!("  {DIM}Cancelled (empty key).{RESET}");
                return Ok(());
            }
            let mut store = AuthStore::load();
            store.set(&provider, &key);
            match store.save() {
                Ok(()) => {
                    if let Some(env_name) = env_var_for(&provider) {
                        std::env::set_var(env_name, &key);
                    }
                    let path = auth_file_path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(no path)".into());
                    println!(
                        "  {GREEN}✓{RESET} {DIM}{} →{RESET} {CYAN}{}{RESET} {DIM}(saved to {}){RESET}",
                        provider,
                        mask_key(&key),
                        path
                    );
                }
                Err(e) => println!("  {RED}✗{RESET} {DIM}save failed: {e}{RESET}"),
            }
            Ok(())
        }
        "login" => {
            let mut login_state = AppStateLoginOnly;
            cmd_login(rest, &mut login_state).await
        }
        "clear" => {
            if rest.is_empty() {
                println!("  {DIM}usage: /auth clear <provider>{RESET}");
                return Ok(());
            }
            let provider = rest.to_lowercase();
            let mut store = AuthStore::load();
            if !store.clear(&provider) {
                println!("  {DIM}no stored key for {provider}{RESET}");
                return Ok(());
            }
            match store.save() {
                Ok(()) => {
                    println!("  {GREEN}✓{RESET} {DIM}cleared {provider} from auth file{RESET}")
                }
                Err(e) => println!("  {RED}✗{RESET} {DIM}save failed: {e}{RESET}"),
            }
            // Env var stays set for the current process — re-launch to drop it.
            Ok(())
        }
        other => {
            println!(
                "  {RED}✗{RESET} {DIM}unknown subcommand: {other} (try: list, set, login, clear){RESET}"
            );
            Ok(())
        }
    }
}

struct AppStateLoginOnly;

/// Explicit OAuth provider names accepted by `/login` / `/logout`.
fn normalize_login_provider(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "openai-codex" | "codex" | "chatgpt" => Some("openai-codex"),
        "grok" | "xai" | "xai-oauth" | "x-ai" | "supergrok" | "grok-oauth" => Some("grok"),
        _ => None,
    }
}

/// Resolve which OAuth provider `/login` / `/logout` should target.
///
/// - Explicit args win (`/logout grok`, `/login codex`, …).
/// - Bare commands use the **active OAuth backend** when the session is on
///   `grok` or `openai-codex`.
/// - Bare commands with a non-OAuth backend (or no backend context, e.g.
///   `/auth login`) require an explicit provider — no silent Codex default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginProviderResolve {
    Provider(&'static str),
    NeedProvider,
    Unknown,
}

fn resolve_login_provider(
    raw: &str,
    active_backend: Option<crate::backends::BackendName>,
) -> LoginProviderResolve {
    let trimmed = raw.trim();
    if !trimmed.is_empty() {
        return match normalize_login_provider(trimmed) {
            Some(p) => LoginProviderResolve::Provider(p),
            None => LoginProviderResolve::Unknown,
        };
    }
    match active_backend {
        Some(b) if b.is_oauth_login() => LoginProviderResolve::Provider(b.as_str()),
        _ => LoginProviderResolve::NeedProvider,
    }
}

fn print_login_provider_error(action: &str, args: &str, resolve: LoginProviderResolve) {
    match resolve {
        LoginProviderResolve::NeedProvider => {
            println!(
                "  {RED}✗{RESET} {DIM}usage: /{action} <openai-codex|grok> \
                 (or switch to an OAuth backend first){RESET}"
            );
        }
        LoginProviderResolve::Unknown => {
            println!(
                "  {RED}✗{RESET} {DIM}unknown {action} provider: {} (try: openai-codex, grok){RESET}",
                args.trim()
            );
        }
        LoginProviderResolve::Provider(_) => {}
    }
}

pub(super) async fn cmd_login(args: &str, state: &mut impl LoginState) -> Result<()> {
    let resolve = resolve_login_provider(args, state.active_backend());
    let LoginProviderResolve::Provider(provider) = resolve else {
        print_login_provider_error("login", args, resolve);
        return Ok(());
    };

    let (title, note) = match provider {
        "grok" => (
            "Grok / SuperGrok login",
            "This uses your SuperGrok or X Premium+ subscription OAuth, not XAI_API_KEY.",
        ),
        _ => (
            "ChatGPT / Codex login",
            "This uses your ChatGPT/Codex subscription OAuth token, not OPENAI_API_KEY.",
        ),
    };
    println!("  {BOLD}{title}{RESET}");
    println!("  {DIM}{note}{RESET}");
    println!("  {DIM}1) Browser login (default){RESET}");
    println!("  {DIM}2) Device-code login (headless/SSH){RESET}");
    let pick = plain_read_line(format!("  {DIM}Select [1]: {RESET}")).await?;
    let device = pick.trim() == "2" || pick.trim().eq_ignore_ascii_case("device");
    let result = match (provider, device) {
        ("grok", true) => crate::xai_oauth::login_and_save_device_code(state.http()).await,
        ("grok", false) => crate::xai_oauth::login_and_save_browser(state.http()).await,
        (_, true) => crate::codex_oauth::login_and_save_device_code(state.http()).await,
        (_, false) => crate::codex_oauth::login_and_save_browser(state.http()).await,
    };
    match result {
        Ok(path) => {
            println!(
                "  {GREEN}✓{RESET} {DIM}logged in to {provider}; saved to {}{RESET}",
                path.display()
            );
            state.after_login()?;
        }
        Err(e) => println!("  {RED}✗{RESET} {DIM}login failed: {e}{RESET}"),
    }
    Ok(())
}

pub(super) trait LoginState {
    fn http(&self) -> &reqwest::Client;
    fn after_login(&mut self) -> Result<()>;
    /// Active backend for bare `/login` defaulting. `None` when there is no
    /// session context (e.g. `/auth login`).
    fn active_backend(&self) -> Option<crate::backends::BackendName>;
}

impl LoginState for AppState {
    fn http(&self) -> &reqwest::Client {
        &self.http
    }
    fn after_login(&mut self) -> Result<()> {
        if self.config.backend.is_oauth_login() {
            self.rebuild_client()?;
            self.resolve_model();
        }
        Ok(())
    }
    fn active_backend(&self) -> Option<crate::backends::BackendName> {
        Some(self.config.backend)
    }
}

impl LoginState for AppStateLoginOnly {
    fn http(&self) -> &reqwest::Client {
        static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
        CLIENT.get_or_init(crate::openai::build_http_client)
    }
    fn after_login(&mut self) -> Result<()> {
        Ok(())
    }
    fn active_backend(&self) -> Option<crate::backends::BackendName> {
        None
    }
}

pub(super) fn cmd_logout(
    args: &str,
    active_backend: Option<crate::backends::BackendName>,
) -> Result<()> {
    let resolve = resolve_login_provider(args, active_backend);
    let LoginProviderResolve::Provider(provider) = resolve else {
        print_login_provider_error("logout", args, resolve);
        return Ok(());
    };
    let mut store = crate::auth::AuthStore::load();
    if store.clear(provider) {
        store.save()?;
        println!("  {GREEN}✓{RESET} {DIM}cleared {provider} login{RESET}");
    } else {
        println!("  {DIM}no stored {provider} login{RESET}");
    }
    Ok(())
}

fn print_oauth_status(store: &crate::auth::AuthStore, provider: &str, login_hint: &str) {
    if let Some(oauth) = store.get_oauth(provider) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let status = if oauth.expires > now + 60 {
            "oauth logged in"
        } else {
            "oauth refresh needed"
        };
        let source = oauth
            .account_id
            .as_ref()
            .map(|id| format!("auth file · account {id}"))
            .unwrap_or_else(|| "auth file".into());
        println!("  {:<12} {:<22}  {DIM}{}{RESET}", provider, status, source);
    } else {
        println!(
            "  {:<12} {:<22}  {DIM}{login_hint}{RESET}",
            provider, "(not logged in)"
        );
    }
}

fn print_auth_status() {
    use crate::auth::{auth_file_path, mask_key, AuthStore, KNOWN_PROVIDERS};
    let store = AuthStore::load();
    println!("  {DIM}provider     status                  source{RESET}");
    for (provider, env_name) in KNOWN_PROVIDERS {
        let env_val = std::env::var(env_name).unwrap_or_default();
        let (display, source) = if !env_val.is_empty() {
            (mask_key(&env_val), format!("env: {env_name}"))
        } else if let Some(k) = store.get(provider) {
            (mask_key(k), "auth file".into())
        } else {
            ("(not set)".into(), "—".into())
        };
        println!("  {:<12} {:<22}  {DIM}{}{RESET}", provider, display, source);
    }
    print_oauth_status(&store, "openai-codex", "/login openai-codex");
    print_oauth_status(&store, "grok", "/login grok");
    if let Some(path) = auth_file_path() {
        println!("  {DIM}file{RESET}     {}", path.display());
    }
}

pub(super) fn cmd_image(args: &str, state: &mut AppState) {
    let args = args.trim();
    if args.is_empty() || args == "list" {
        if state.pending_image_attachments.is_empty() {
            println!("  {DIM}no images staged. Usage: /image <path>{RESET}");
        } else {
            println!(
                "  {DIM}{} image(s) staged for next turn:{RESET}",
                state.pending_image_attachments.len()
            );
            for (i, url) in state.pending_image_attachments.iter().enumerate() {
                let summary = url.split(';').next().unwrap_or(url);
                println!("  {DIM}{:>2}){RESET} {}", i + 1, summary);
            }
        }
        return;
    }
    if args == "clear" {
        let n = state.pending_image_attachments.len();
        state.pending_image_attachments.clear();
        println!("  {GREEN}✓{RESET} {DIM}cleared {n} staged image(s){RESET}");
        return;
    }
    let path = std::path::Path::new(args);
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            println!(
                "  {RED}✗{RESET} {DIM}cannot read {}: {e}{RESET}",
                path.display()
            );
            return;
        }
    };
    let mime = guess_image_mime(path);
    let data_url = format!(
        "data:{mime};base64,{}",
        crate::tools::image_base64_for_data_url(&bytes)
    );
    let supports_vision = catalog::lookup(state.config.backend, &state.model)
        .map(|m| m.vision)
        .unwrap_or(state.config.backend.is_local());
    if !supports_vision {
        println!(
            "  {YELLOW}!{RESET} {DIM}{} isn't catalog-marked as vision-capable — the model may reject the attachment.{RESET}",
            state.model
        );
    }
    state.pending_image_attachments.push(data_url);
    println!(
        "  {GREEN}✓{RESET} {DIM}image staged ({} bytes, {}). Send a prompt to attach.{RESET}",
        bytes.len(),
        mime
    );
}

pub(super) fn cmd_reasoning(args: &str, state: &mut AppState) {
    let arg = args.trim().to_lowercase();
    let new_state = match arg.as_str() {
        "" | "status" => {
            let cur = state.renderer.reasoning_enabled();
            println!(
                "  {DIM}reasoning panel:{RESET} {} {DIM}(usage: /reasoning on|off){RESET}",
                if cur { "on" } else { "off" }
            );
            return;
        }
        "on" | "true" | "1" => true,
        "off" | "false" | "0" => false,
        other => {
            println!("  {RED}✗{RESET} {DIM}unknown value: {other} (use on, off, status){RESET}");
            return;
        }
    };
    state.renderer.set_reasoning(new_state);
    state.config.display.reasoning = new_state;
    println!(
        "  {GREEN}✓{RESET} {DIM}reasoning panel →{RESET} {CYAN}{}{RESET}",
        if new_state { "on" } else { "off" }
    );
}

/// `/verbose [on|off|status]` — switch the tool view between the normal grouped
/// summary and a detailed debug view that prints every tool call with its full
/// arguments and a large result preview.
pub(super) fn cmd_verbose(args: &str, state: &mut AppState) {
    let arg = args.trim().to_lowercase();
    let new_state = match arg.as_str() {
        "" | "status" => {
            let cur = state.renderer.verbose_enabled();
            println!(
                "  {DIM}verbose tool view:{RESET} {} {DIM}(usage: /verbose on|off){RESET}",
                if cur { "on" } else { "off" }
            );
            return;
        }
        "on" | "true" | "1" => true,
        "off" | "false" | "0" => false,
        other => {
            println!("  {RED}✗{RESET} {DIM}unknown value: {other} (use on, off, status){RESET}");
            return;
        }
    };
    state.renderer.set_verbose(new_state);
    println!(
        "  {GREEN}✓{RESET} {DIM}verbose tool view →{RESET} {CYAN}{}{RESET}{DIM} — every tool call with full args + result{RESET}",
        if new_state { "on" } else { "off" }
    );
}

/// `/trace [on|off|status]` — surface nested subagent/critic tool calls in the
/// TUI (indented) and always log them to the session event sidecar.
pub(super) fn cmd_trace(args: &str, state: &mut AppState) {
    let arg = args.trim().to_lowercase();
    let new_state = match arg.as_str() {
        "" | "status" => {
            let cur = state.trace_enabled;
            println!(
                "  {DIM}trace nested agents:{RESET} {} {DIM}(usage: /trace on|off · log: {}){RESET}",
                if cur { "on" } else { "off" },
                crate::turn_trace::events_path_for_session(&state.session_path).display()
            );
            return;
        }
        "on" | "true" | "1" => true,
        "off" | "false" | "0" => false,
        other => {
            println!("  {RED}✗{RESET} {DIM}unknown value: {other} (use on, off, status){RESET}");
            return;
        }
    };
    state.trace_enabled = new_state;
    state.renderer.set_trace(new_state);
    println!(
        "  {GREEN}✓{RESET} {DIM}trace nested agents →{RESET} {CYAN}{}{RESET}{DIM} — subagent/critic tool calls shown indented{RESET}",
        if new_state { "on" } else { "off" }
    );
}

fn guess_image_mime(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "application/octet-stream",
    }
}

fn report_default_persist(result: Result<()>) {
    match result {
        Ok(()) => println!(
            "  {DIM}· saved default in {RESET}{CYAN}{}{RESET}",
            crate::config::AGENT_CONFIG_PATH
        ),
        Err(e) => println!("  {RED}✗{RESET} {DIM}could not save default: {e}{RESET}"),
    }
}

fn persist_model_as_default(state: &AppState) {
    report_default_persist(crate::config::persist_backend_model_defaults(
        Path::new(crate::config::AGENT_CONFIG_PATH),
        state.config.backend,
        Some(state.model.as_str()),
    ));
}

fn persist_backend_as_default(state: &AppState) {
    // Clears modelOverride on disk so the next launch uses the backend default.
    report_default_persist(crate::config::persist_backend_model_defaults(
        Path::new(crate::config::AGENT_CONFIG_PATH),
        state.config.backend,
        None,
    ));
}

/// Build the trailing picker marker for an entry: `(selected)` when it is the
/// live session choice, `(default)` when it is the value persisted in
/// `agent.config.json`. Both may show together when they coincide.
fn picker_marker(is_selected: bool, is_default: bool) -> String {
    let mut out = String::new();
    if is_selected {
        out.push_str(&format!("  {DIM}(selected){RESET}"));
    }
    if is_default {
        out.push_str(&format!("  {DIM}(default){RESET}"));
    }
    out
}

/// Ask whether to pin the just-selected choice as the project default.
/// Used by interactive pickers when the user did not already pass `--default`.
async fn confirm_save_as_default() -> Result<bool> {
    println!(
        "  {DIM}Save as project default in {RESET}{CYAN}{}{RESET}{DIM}? [y/N]{RESET}",
        crate::config::AGENT_CONFIG_PATH
    );
    let answer = plain_read_line(format!("  {DIM}?{RESET} ")).await?;
    let a = answer.trim().to_lowercase();
    Ok(a == "y" || a == "yes")
}

/// After a session switch, persist if requested via flag or interactive confirm.
async fn maybe_persist_model_default(state: &AppState, as_default: bool) -> Result<()> {
    if as_default || confirm_save_as_default().await? {
        persist_model_as_default(state);
    }
    Ok(())
}

async fn maybe_persist_backend_default(state: &AppState, as_default: bool) -> Result<()> {
    if as_default || confirm_save_as_default().await? {
        persist_backend_as_default(state);
    }
    Ok(())
}

pub(super) async fn cmd_backend(args: &str, state: &mut AppState) -> Result<()> {
    let (rest, as_default) = crate::config::strip_default_flag(args);

    // Pin the current backend as project default without switching.
    if rest.is_empty() && as_default {
        println!(
            "  {GREEN}✓{RESET} {DIM}default backend →{RESET} {CYAN}{}{RESET} {DIM}(modelOverride cleared on disk){RESET}",
            state.config.backend.as_str()
        );
        persist_backend_as_default(state);
        return Ok(());
    }

    let chosen: Option<BackendName> = if !rest.is_empty() {
        BackendName::parse(&rest)
    } else {
        let backends = BackendName::all();
        let (persisted_backend, _) = crate::config::persisted_defaults();
        let options: Vec<String> = backends
            .iter()
            .map(|b| {
                format!(
                    "{}{}",
                    b.as_str(),
                    picker_marker(*b == state.config.backend, persisted_backend == Some(*b),)
                )
            })
            .collect();
        let default_idx = backends
            .iter()
            .position(|b| *b == state.config.backend)
            .unwrap_or(0);
        match select_from_list("Backend".into(), options, default_idx).await? {
            Some(idx) => backends.get(idx).copied(),
            None => None,
        }
    };
    let Some(chosen) = chosen else {
        println!("  {DIM}Cancelled.{RESET}");
        return Ok(());
    };
    if matches!(chosen, BackendName::OpenAiCodex)
        && crate::auth::AuthStore::load()
            .get_oauth("openai-codex")
            .is_none()
    {
        println!(
            "  {RED}✗{RESET} {DIM}not logged in for openai-codex. Run /login openai-codex to sign in with ChatGPT.{RESET}"
        );
        return Ok(());
    }
    if matches!(chosen, BackendName::Grok)
        && crate::auth::AuthStore::load()
            .get_oauth(crate::xai_oauth::PROVIDER)
            .is_none()
    {
        println!(
            "  {RED}✗{RESET} {DIM}not logged in for grok. Run /login grok to sign in with SuperGrok / X Premium+.{RESET}"
        );
        return Ok(());
    }
    if !chosen.is_local() && !chosen.is_oauth_login() && backend(chosen).api_key.is_empty() {
        let env_name = match chosen {
            BackendName::Openrouter => "OPENROUTER_API_KEY",
            BackendName::OpenAi => "OPENAI_API_KEY",
            BackendName::OpenAiCodex => "ChatGPT login",
            BackendName::Grok => "Grok login",
            _ => "API key",
        };
        println!("  {RED}✗{RESET} {DIM}{env_name} not set in environment.{RESET}");
        return Ok(());
    }
    state.config.backend = chosen;
    state.config.model_override = None;
    state.active_effort = None;
    state.rebuild_client()?;
    state.resolve_model();
    println!(
        "  {GREEN}✓{RESET} {DIM}backend →{RESET} {CYAN}{}{RESET} {DIM}· model →{RESET} {CYAN}{}{RESET}",
        chosen.as_str(),
        state.model
    );
    // Direct `/backend name --default` skips the confirm; the arrow picker
    // offers to save the selected backend after applying it to this session.
    if rest.is_empty() {
        maybe_persist_backend_default(state, false).await?;
    } else if as_default {
        persist_backend_as_default(state);
    }
    Ok(())
}

fn build_filtered_model_picker(
    ids: Vec<String>,
    backend_default: &str,
    current: &str,
    persisted: Option<&str>,
    filter: &str,
    backend: BackendName,
) -> (Vec<String>, Vec<ModelMenuItem>, usize) {
    let (_, candidates, _) = build_model_menu(ids, backend_default, Some(current));
    let filter = filter.trim().to_lowercase();
    let mut labels = Vec::new();
    let mut items = Vec::new();
    let mut selected_idx = None;

    for item in candidates {
        let ModelMenuItem::Id(id) = item else {
            continue;
        };
        if !filter.is_empty() && !id.to_lowercase().contains(&filter) {
            continue;
        }

        if id == current {
            selected_idx = Some(labels.len());
        }
        let marker = picker_marker(id == current, persisted == Some(id.as_str()));
        let mut label = format!("{id}{marker}");
        if let Some(info) = catalog::lookup(backend, &id) {
            label.push_str(&format!(
                "  {DIM}{}{RESET}",
                catalog::format_cost_label(info)
            ));
        }
        labels.push(label);
        items.push(ModelMenuItem::Id(id));
    }

    labels.push(MODEL_FREE_TEXT_LABEL.into());
    items.push(ModelMenuItem::FreeText);
    (labels, items, selected_idx.unwrap_or(0))
}

pub(super) async fn cmd_model(args: &str, state: &mut AppState) -> Result<()> {
    use std::io::Write;
    let (rest, as_default) = crate::config::strip_default_flag(args);

    if rest.is_empty() && as_default {
        if state.config.model_override.as_deref() != Some(state.model.as_str()) {
            state.config.model_override = Some(state.model.clone());
        }
        println!(
            "  {GREEN}✓{RESET} {DIM}model →{RESET} {CYAN}{}{RESET}",
            state.model
        );
        persist_model_as_default(state);
        return Ok(());
    }

    if !rest.is_empty() {
        let model_override = if matches!(state.config.backend, BackendName::OpenAiCodex) {
            let Some(canonical) = crate::codex_responses::canonical_codex_model(&rest) else {
                println!(
                    "  {RED}✗{RESET} {DIM}{rest} is not supported with ChatGPT/Codex login. Try one of: {}{RESET}",
                    crate::codex_responses::codex_model_list().join(", ")
                );
                return Ok(());
            };
            canonical.to_string()
        } else if matches!(state.config.backend, BackendName::Grok) {
            let Some(canonical) = crate::xai_oauth::canonical_grok_model(&rest) else {
                println!(
                    "  {RED}✗{RESET} {DIM}{rest} is not supported with Grok login. Try one of: {}{RESET}",
                    crate::xai_oauth::grok_model_list().join(", ")
                );
                return Ok(());
            };
            canonical.to_string()
        } else {
            rest
        };
        state.config.model_override = Some(model_override);
        state.active_effort = None;
        state.resolve_model();
        println!(
            "  {GREEN}✓{RESET} {DIM}model →{RESET} {CYAN}{}{RESET}",
            state.model
        );
        if as_default {
            persist_model_as_default(state);
        }
        return Ok(());
    }

    let backend_default = default_model(&state.backend, None);
    let current = state.model.clone();
    let mut out = std::io::stdout();
    let _ = write!(
        out,
        "  {DIM}Fetching models from {}…{RESET}",
        state.config.backend.as_str()
    );
    let _ = out.flush();
    let ids = match list_models(&state.http, &state.backend).await {
        Ok(ids) => {
            let _ = write!(out, "\r\x1b[K");
            let _ = out.flush();
            ids
        }
        Err(e) => {
            let _ = write!(out, "\r\x1b[K");
            let _ = out.flush();
            println!(
                "  {YELLOW}!{RESET} {DIM}Could not list models ({e}); enter a model id.{RESET}"
            );
            return apply_model_free_text(state, &backend_default, Some(current.as_str())).await;
        }
    };
    if ids.is_empty() {
        println!("  {YELLOW}!{RESET} {DIM}No models returned; enter a model id.{RESET}");
        return apply_model_free_text(state, &backend_default, Some(current.as_str())).await;
    }

    let filter = plain_read_line(format!("  {DIM}Filter (blank for all):{RESET} "))
        .await?
        .trim()
        .to_string();
    let (_, persisted_model) = crate::config::persisted_defaults();
    let (labels, items, default_idx) = build_filtered_model_picker(
        ids,
        &backend_default,
        &current,
        persisted_model.as_deref(),
        &filter,
        state.config.backend,
    );

    if !items
        .iter()
        .any(|item| matches!(item, ModelMenuItem::Id(_)))
    {
        println!("  {DIM}No matches; enter a model id.{RESET}");
        return apply_model_free_text(state, &backend_default, Some(current.as_str())).await;
    }

    let Some(idx) = select_from_list("Model".into(), labels, default_idx).await? else {
        println!("  {DIM}Cancelled.{RESET}");
        return Ok(());
    };
    match items.get(idx) {
        Some(ModelMenuItem::Id(id)) => {
            apply_interactive_model_choice(state, model_override_for(id, &backend_default)).await?;
        }
        Some(ModelMenuItem::FreeText) => {
            apply_model_free_text(state, &backend_default, Some(current.as_str())).await?;
        }
        None => println!("  {DIM}Cancelled.{RESET}"),
    }
    Ok(())
}

async fn apply_model_free_text(
    state: &mut AppState,
    backend_default: &str,
    current: Option<&str>,
) -> Result<()> {
    let prompt = if let Some(current) = current {
        format!(
            "  {CYAN}❯{RESET} {DIM}Model id [current: {current}; blank: {backend_default}]:{RESET} "
        )
    } else {
        format!("  {CYAN}❯{RESET} {DIM}Model id [blank: {backend_default}]:{RESET} ")
    };
    let input = plain_read_line(prompt).await?;
    let trimmed = input.trim();
    if matches!(trimmed.to_lowercase().as_str(), "q" | "quit" | "cancel") {
        println!("  {DIM}Cancelled.{RESET}");
        return Ok(());
    }
    if trimmed.is_empty() {
        return apply_interactive_model_choice(state, None).await;
    }
    if matches!(state.config.backend, BackendName::OpenAiCodex) {
        let Some(canonical) = crate::codex_responses::canonical_codex_model(trimmed) else {
            println!(
                "  {RED}✗{RESET} {DIM}{trimmed} is not supported with ChatGPT/Codex login. Try one of: {}{RESET}",
                crate::codex_responses::codex_model_list().join(", ")
            );
            return Ok(());
        };
        return apply_interactive_model_choice(
            state,
            model_override_for(canonical, backend_default),
        )
        .await;
    }
    apply_interactive_model_choice(state, model_override_for(trimmed, backend_default)).await
}

async fn apply_interactive_model_choice(
    state: &mut AppState,
    model_override: Option<String>,
) -> Result<()> {
    apply_model_choice(state, model_override);
    maybe_persist_model_default(state, false).await
}

fn apply_model_choice(state: &mut AppState, model_override: Option<String>) {
    state.config.model_override = model_override;
    state.active_effort = None;
    state.resolve_model();
    println!(
        "  {GREEN}✓{RESET} {DIM}model →{RESET} {CYAN}{}{RESET}",
        state.model
    );
}

pub(super) fn cmd_tools(args: &str, state: &mut AppState) {
    if args.is_empty() {
        println!("  {DIM}available{RESET}  {}", ALL_TOOL_NAMES.join(", "));
        println!(
            "  {DIM}enabled{RESET}    {CYAN}{}{RESET}",
            state.config.tools.join(", ")
        );
        println!(
            "  {DIM}mode{RESET}       {CYAN}{}{RESET}",
            state.config.tool_selection.as_str()
        );
        println!(
            "  {DIM}usage{RESET}      /tools auto · /tools fixed · /tools file_read,grep,list_dir"
        );
        return;
    }

    let (mode, list) = if args == "auto" {
        state.config.tool_selection = ToolSelection::Auto;
        state.config.mode = OperatorMode::Custom;
        println!("  {GREEN}✓{RESET} {DIM}tool selection →{RESET} {CYAN}auto{RESET}");
        return;
    } else if args == "fixed" {
        state.config.tool_selection = ToolSelection::Fixed;
        state.config.mode = OperatorMode::Custom;
        println!("  {GREEN}✓{RESET} {DIM}tool selection →{RESET} {CYAN}fixed{RESET}");
        return;
    } else if let Some(rest) = args.strip_prefix("auto ") {
        (ToolSelection::Auto, rest)
    } else if let Some(rest) = args.strip_prefix("fixed ") {
        (ToolSelection::Fixed, rest)
    } else {
        (ToolSelection::Fixed, args)
    };

    let requested: Vec<String> = list
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let invalid: Vec<&String> = requested.iter().filter(|n| !is_tool_name(n)).collect();
    if !invalid.is_empty() {
        let names: Vec<&str> = invalid.iter().map(|s| s.as_str()).collect();
        println!(
            "  {RED}✗{RESET} {DIM}unknown tools: {}{RESET}",
            names.join(", ")
        );
        return;
    }
    state.config.tools = requested;
    state.config.tool_selection = mode;
    state.config.mode = OperatorMode::Custom;
    println!(
        "  {GREEN}✓{RESET} {DIM}tools →{RESET} {CYAN}{}{RESET} {DIM}· mode →{RESET} {CYAN}{}{RESET}",
        state.config.tools.join(", "),
        state.config.tool_selection.as_str()
    );
}

pub(super) async fn cmd_compare(args: &str, state: &AppState) -> Result<()> {
    use std::io::Write;
    let cloud_backend = backend(BackendName::Openrouter);
    if cloud_backend.api_key.is_empty() {
        println!("  {RED}✗{RESET} {DIM}OPENROUTER_API_KEY not set.{RESET}");
        return Ok(());
    }
    let last_user = state.messages.iter().rev().find_map(|m| m.user_text());
    let Some(user_text) = last_user else {
        println!("  {DIM}No user message yet.{RESET}");
        return Ok(());
    };
    let cloud_model = if !args.is_empty() {
        args.to_string()
    } else {
        default_model(&cloud_backend, None)
    };
    println!("  {YELLOW}⇆{RESET} {BOLD}cloud{RESET} {DIM}{cloud_model}{RESET}");
    println!();

    let messages = vec![
        ChatMessage::System {
            content: state.config.render_system_prompt(),
        },
        ChatMessage::User {
            content: user_text.to_string().into(),
        },
    ];
    let req = ChatRequest {
        model: &cloud_model,
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
    let mut out = std::io::stdout();
    let result = stream_chat(&state.http, &cloud_backend, &req, None, |chunk| {
        if let Some(c) = chunk.choices.first() {
            if let Some(content) = &c.delta.content {
                let _ = out.write_all(content.as_bytes());
                let _ = out.flush();
            }
        }
    })
    .await;
    println!();
    if let Err(e) = result {
        println!("  {RED}✗{RESET} {DIM}{e}{RESET}");
    }
    Ok(())
}

const FUSION_MODEL: &str = "openrouter/fusion";

#[derive(Debug, Clone, PartialEq, Eq)]
struct FusionToolArgs {
    model: Option<String>,
    analysis_models: Option<Vec<String>>,
    judge_model: Option<String>,
    max_tool_calls: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FusionInvocation {
    Status,
    Alias,
    Tool(FusionToolArgs),
    Off,
}

fn parse_fusion_args(args: &str) -> Option<FusionInvocation> {
    let trimmed = args.trim();
    if trimmed.is_empty() || trimmed == "status" {
        return Some(FusionInvocation::Status);
    }
    if matches!(trimmed, "on" | "alias") {
        return Some(FusionInvocation::Alias);
    }
    if trimmed == "off" {
        return Some(FusionInvocation::Off);
    }

    let mut parts = trimmed.split_whitespace();
    if parts.next()? != "tool" {
        return None;
    }
    let mut parsed = FusionToolArgs {
        model: None,
        analysis_models: None,
        judge_model: None,
        max_tool_calls: None,
    };
    for part in parts {
        if let Some(value) = part
            .strip_prefix("panel=")
            .or_else(|| part.strip_prefix("--panel="))
        {
            let models = parse_csv(value);
            if models.is_empty() || models.len() > 8 {
                return None;
            }
            parsed.analysis_models = Some(models);
        } else if let Some(value) = part
            .strip_prefix("judge=")
            .or_else(|| part.strip_prefix("--judge="))
        {
            if value.trim().is_empty() {
                return None;
            }
            parsed.judge_model = Some(value.trim().to_string());
        } else if let Some(value) = part
            .strip_prefix("max-tools=")
            .or_else(|| part.strip_prefix("--max-tools="))
            .or_else(|| part.strip_prefix("maxToolCalls="))
        {
            let n = value.parse::<u8>().ok()?;
            if !(1..=16).contains(&n) {
                return None;
            }
            parsed.max_tool_calls = Some(n);
        } else if parsed.model.is_none() {
            parsed.model = Some(part.to_string());
        } else {
            return None;
        }
    }
    Some(FusionInvocation::Tool(parsed))
}

fn parse_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn fusion_usage() {
    println!(
        "  {DIM}Usage: /fusion status · /fusion on · /fusion tool [model] [panel=a,b,c] [judge=model] [max-tools=1..16] · /fusion off{RESET}"
    );
}

fn print_fusion_status(state: &AppState) {
    let fusion = &state.config.openrouter.fusion;
    let alias_active = matches!(state.config.backend, BackendName::Openrouter)
        && state.model == FUSION_MODEL
        && !fusion.enabled;
    let mode = if alias_active {
        "alias"
    } else if matches!(state.config.backend, BackendName::Openrouter) && fusion.enabled {
        "tool"
    } else {
        "off"
    };
    println!("  {DIM}fusion{RESET}           {CYAN}{mode}{RESET}");
    println!(
        "  {DIM}backend/model{RESET}    {} · {}",
        state.config.backend.as_str(),
        state.model
    );
    if fusion.enabled {
        let panel = if fusion.analysis_models.is_empty() {
            "Quality preset".into()
        } else {
            fusion.analysis_models.join(", ")
        };
        let judge = fusion
            .judge_model
            .as_deref()
            .unwrap_or("OpenRouter default / outer model");
        let max_tools = fusion
            .max_tool_calls
            .map(|n| n.to_string())
            .unwrap_or_else(|| "default".into());
        println!("  {DIM}panel{RESET}            {panel}");
        println!("  {DIM}judge{RESET}            {judge}");
        println!("  {DIM}maxToolCalls{RESET}     {max_tools}");
    }
    println!(
        "  {DIM}cost{RESET}             OpenRouter usage.cost is used when returned; otherwise cost is marked unknown"
    );
    fusion_usage();
}

fn ensure_openrouter_key() -> bool {
    if backend(BackendName::Openrouter).api_key.is_empty() {
        println!("  {RED}✗{RESET} {DIM}OPENROUTER_API_KEY not set. Run /auth set openrouter first.{RESET}");
        false
    } else {
        true
    }
}

pub(super) fn cmd_fusion(args: &str, state: &mut AppState) -> Result<()> {
    let Some(invocation) = parse_fusion_args(args) else {
        fusion_usage();
        return Ok(());
    };

    match invocation {
        FusionInvocation::Status => {
            print_fusion_status(state);
        }
        FusionInvocation::Alias => {
            if !ensure_openrouter_key() {
                return Ok(());
            }
            state.config.backend = BackendName::Openrouter;
            state.config.model_override = Some(FUSION_MODEL.into());
            state.active_effort = None;
            state.config.openrouter.fusion.enabled = false;
            state.rebuild_client()?;
            state.resolve_model();
            state.warmed_fingerprint = None;
            println!(
                "  {GREEN}✓{RESET} {DIM}Fusion alias enabled →{RESET} {CYAN}{}{RESET}",
                state.model
            );
            println!(
                "  {DIM}Use this for research, architecture tradeoffs, reviews, and high-stakes debugging; use /fusion off for normal coding turns.{RESET}"
            );
        }
        FusionInvocation::Tool(args) => {
            if !ensure_openrouter_key() {
                return Ok(());
            }
            let was_openrouter = matches!(state.config.backend, BackendName::Openrouter);
            let fallback_model = default_model(&backend(BackendName::Openrouter), None);
            let model = args.model.unwrap_or_else(|| {
                if was_openrouter && state.model != FUSION_MODEL {
                    state.model.clone()
                } else {
                    fallback_model
                }
            });

            state.config.backend = BackendName::Openrouter;
            state.config.model_override = Some(model);
            state.active_effort = None;
            {
                let fusion = &mut state.config.openrouter.fusion;
                fusion.enabled = true;
                if let Some(models) = args.analysis_models {
                    fusion.analysis_models = models;
                }
                if let Some(judge) = args.judge_model {
                    fusion.judge_model = Some(judge);
                }
                if let Some(max_tool_calls) = args.max_tool_calls {
                    fusion.max_tool_calls = Some(max_tool_calls);
                }
            }
            state.rebuild_client()?;
            state.resolve_model();
            state.warmed_fingerprint = None;
            println!(
                "  {GREEN}✓{RESET} {DIM}Fusion tool enabled on{RESET} {CYAN}{}{RESET}",
                state.model
            );
            println!(
                "  {DIM}The model can invoke OpenRouter Fusion when a turn benefits from multi-model deliberation.{RESET}"
            );
        }
        FusionInvocation::Off => {
            state.config.openrouter.fusion.enabled = false;
            if matches!(state.config.backend, BackendName::Openrouter)
                && state.config.model_override.as_deref() == Some(FUSION_MODEL)
            {
                state.config.model_override = None;
            }
            state.active_effort = None;
            state.backend.openrouter = state.config.openrouter.clone();
            state.resolve_model();
            state.warmed_fingerprint = None;
            println!(
                "  {GREEN}✓{RESET} {DIM}Fusion off · model →{RESET} {CYAN}{}{RESET}",
                state.model
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OperatorMode;
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
    fn mode_ship_syncs_session_checkpoints_flag() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        state.config.apply_operator_mode(OperatorMode::Explore);
        state.checkpoints_enabled = state.config.checkpoints.enabled;
        assert!(!state.checkpoints_enabled);

        cmd_mode("ship", &mut state);

        assert_eq!(state.config.mode, OperatorMode::Ship);
        assert!(state.config.checkpoints.enabled);
        assert!(state.checkpoints_enabled);
    }

    #[test]
    fn resolve_login_provider_explicit_aliases() {
        use crate::backends::BackendName;
        assert_eq!(
            resolve_login_provider("codex", Some(BackendName::Grok)),
            LoginProviderResolve::Provider("openai-codex")
        );
        assert_eq!(
            resolve_login_provider("xai", Some(BackendName::OpenAiCodex)),
            LoginProviderResolve::Provider("grok")
        );
        assert_eq!(
            resolve_login_provider("nope", None),
            LoginProviderResolve::Unknown
        );
    }

    #[test]
    fn resolve_login_provider_bare_uses_active_oauth_backend() {
        use crate::backends::BackendName;
        assert_eq!(
            resolve_login_provider("", Some(BackendName::Grok)),
            LoginProviderResolve::Provider("grok")
        );
        assert_eq!(
            resolve_login_provider("  ", Some(BackendName::OpenAiCodex)),
            LoginProviderResolve::Provider("openai-codex")
        );
    }

    #[test]
    fn resolve_login_provider_bare_requires_provider_without_oauth_backend() {
        use crate::backends::BackendName;
        assert_eq!(
            resolve_login_provider("", Some(BackendName::Ollama)),
            LoginProviderResolve::NeedProvider
        );
        assert_eq!(
            resolve_login_provider("", Some(BackendName::OpenAi)),
            LoginProviderResolve::NeedProvider
        );
        assert_eq!(
            resolve_login_provider("", None),
            LoginProviderResolve::NeedProvider
        );
    }

    #[test]
    fn parses_fusion_alias_and_off() {
        assert_eq!(parse_fusion_args(""), Some(FusionInvocation::Status));
        assert_eq!(parse_fusion_args("status"), Some(FusionInvocation::Status));
        assert_eq!(parse_fusion_args("on"), Some(FusionInvocation::Alias));
        assert_eq!(parse_fusion_args("alias"), Some(FusionInvocation::Alias));
        assert_eq!(parse_fusion_args("off"), Some(FusionInvocation::Off));
        assert!(parse_fusion_args("unknown").is_none());
    }

    #[test]
    fn parses_fusion_tool_options() {
        let parsed = parse_fusion_args(
            "tool anthropic/claude-sonnet-4.5 panel=~openai/gpt-latest,deepseek/deepseek-v3.2 judge=~anthropic/claude-opus-latest max-tools=4",
        );
        let Some(FusionInvocation::Tool(args)) = parsed else {
            panic!("expected tool invocation");
        };
        assert_eq!(args.model.as_deref(), Some("anthropic/claude-sonnet-4.5"));
        assert_eq!(
            args.analysis_models,
            Some(vec![
                "~openai/gpt-latest".into(),
                "deepseek/deepseek-v3.2".into()
            ])
        );
        assert_eq!(
            args.judge_model.as_deref(),
            Some("~anthropic/claude-opus-latest")
        );
        assert_eq!(args.max_tool_calls, Some(4));
    }

    #[test]
    fn rejects_invalid_fusion_tool_options() {
        assert!(parse_fusion_args("tool model-a model-b").is_none());
        assert!(parse_fusion_args("tool panel=").is_none());
        assert!(parse_fusion_args("tool panel=m1,m2,m3,m4,m5,m6,m7,m8,m9").is_none());
        assert!(parse_fusion_args("tool max-tools=0").is_none());
        assert!(parse_fusion_args("tool max-tools=17").is_none());
        assert!(parse_fusion_args("tool judge=").is_none());
    }

    #[test]
    fn filtered_model_picker_preserves_filter_and_default_markers() {
        let (labels, items, selected_idx) = build_filtered_model_picker(
            vec!["gpt-4o-mini".into(), "gpt-4o".into(), "o1-mini".into()],
            "gpt-4o-mini",
            "gpt-4o",
            Some("gpt-4o-mini"),
            "gpt",
            BackendName::OpenAi,
        );

        assert_eq!(items.len(), 3, "two filtered ids plus free text");
        assert!(labels.iter().all(|label| !label.contains("o1-mini")));
        assert!(labels[selected_idx].contains("gpt-4o"));
        assert!(labels[selected_idx].contains("(selected)"));
        assert!(labels
            .iter()
            .any(|label| label.contains("gpt-4o-mini") && label.contains("(default)")));
        assert_eq!(items.last(), Some(&ModelMenuItem::FreeText));
    }

    #[test]
    fn filtered_model_picker_keeps_free_text_when_nothing_matches() {
        let (labels, items, selected_idx) = build_filtered_model_picker(
            vec!["gpt-4o-mini".into()],
            "gpt-4o-mini",
            "gpt-4o-mini",
            None,
            "claude",
            BackendName::OpenAi,
        );

        assert_eq!(labels, vec![MODEL_FREE_TEXT_LABEL.to_string()]);
        assert_eq!(items, vec![ModelMenuItem::FreeText]);
        assert_eq!(selected_idx, 0);
    }
}
