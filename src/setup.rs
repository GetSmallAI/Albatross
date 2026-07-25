use anyhow::Result;
use serde_json::{json, Map, Value};
use std::path::Path;
use std::time::Duration;

use crate::backends::{backend, default_model, validate, BackendName};
use crate::config::{
    dotenv_values, layered_env, AgentConfig, ApprovalPolicy, ToolSelection, AGENT_CONFIG_PATH,
};
use crate::hardware::{detect_hardware_spec, save_hardware_summary};
use crate::input::{plain_read_line, select_from_list};
use crate::openai::{build_http_client, chat_oneshot, list_models, ChatMessage, ChatRequest};

// Routed through the shared theme so the wizard matches the rest of the TUI
// and secondary text is readable bright-black instead of ANSI faint.
const RESET: crate::theme::Style = crate::theme::RESET;
const DIM: crate::theme::Style = crate::theme::MUTED;
const BOLD: crate::theme::Style = crate::theme::BOLD;
const CYAN: crate::theme::Style = crate::theme::ACCENT;
const GREEN: crate::theme::Style = crate::theme::SUCCESS;
const YELLOW: crate::theme::Style = crate::theme::WARN;
const RED: crate::theme::Style = crate::theme::ERROR;

const NO_WIZARD_ENV: &str = "ALBATROSS_NO_WIZARD";

pub fn setup_disabled() -> bool {
    let dotenv = dotenv_values();
    layered_env(&dotenv, NO_WIZARD_ENV)
        .map(|v| is_truthy(&v))
        .unwrap_or(false)
}

pub fn should_run_first_run_setup(config_path: &Path) -> bool {
    !config_path.exists() && !setup_disabled()
}

pub async fn maybe_run_first_run_setup(base: &AgentConfig) -> Result<Option<AgentConfig>> {
    if should_run_first_run_setup(Path::new(AGENT_CONFIG_PATH)) {
        let mut first_run_defaults = base.clone();
        let spec = detect_hardware_spec();
        let _ = save_hardware_summary(&first_run_defaults.session_dir, &spec);
        first_run_defaults.approval_policy = ApprovalPolicy::DangerousOnly;
        run_setup_wizard(&first_run_defaults).await
    } else {
        Ok(None)
    }
}

pub async fn run_setup_wizard(base: &AgentConfig) -> Result<Option<AgentConfig>> {
    let pad = crate::theme::PAD;
    println!();
    println!("{pad}{CYAN}{BOLD}Albatross setup{RESET}");
    println!("{}", crate::theme::rule());
    println!(
        "{pad}{DIM}A few quick questions — I'll write {AGENT_CONFIG_PATH}. Use ↑/↓ and Enter\n{pad}(or a number); defaults are marked {CYAN}*{RESET}{DIM}. Type q to cancel.{RESET}"
    );
    println!();

    let Some(chosen_backend) = prompt_backend(base.backend).await? else {
        println!("  {DIM}Setup cancelled.{RESET}");
        return Ok(None);
    };

    // Cloud backends need an API key. Collect it now so the end-of-wizard
    // probe succeeds instead of failing on a missing key.
    prompt_api_key(chosen_backend).await?;

    let model_default = default_model(&backend(chosen_backend), None);
    let Some(model_override) = prompt_model(
        chosen_backend,
        &model_default,
        base.model_override.as_deref(),
    )
    .await?
    else {
        println!("  {DIM}Setup cancelled.{RESET}");
        return Ok(None);
    };
    let Some(approval_policy) = prompt_approval(base.approval_policy).await? else {
        println!("  {DIM}Setup cancelled.{RESET}");
        return Ok(None);
    };
    let Some(tool_selection) = prompt_tool_selection(base.tool_selection).await? else {
        println!("  {DIM}Setup cancelled.{RESET}");
        return Ok(None);
    };

    let mut config = base.clone();
    config.backend = chosen_backend;
    config.model_override = model_override;
    config.approval_policy = approval_policy;
    config.tool_selection = tool_selection;

    write_agent_config(Path::new(AGENT_CONFIG_PATH), &config)?;
    println!("  {GREEN}✓{RESET} {DIM}wrote {AGENT_CONFIG_PATH}{RESET}");
    probe_setup_backend(&config).await;

    Ok(Some(config))
}

pub fn write_agent_config(path: &Path, config: &AgentConfig) -> Result<()> {
    let body = serde_json::to_string_pretty(&setup_config_value(config))?;
    std::fs::write(path, format!("{body}\n"))?;
    Ok(())
}

fn setup_config_value(config: &AgentConfig) -> Value {
    let mut obj = Map::new();
    obj.insert("mode".into(), json!(config.mode.as_str()));
    obj.insert("backend".into(), json!(config.backend.as_str()));
    if let Some(model) = &config.model_override {
        obj.insert("modelOverride".into(), json!(model));
    }
    if config.system_prompt != AgentConfig::default().system_prompt {
        obj.insert("systemPrompt".into(), json!(config.system_prompt));
    }
    obj.insert("maxSteps".into(), json!(config.max_steps));
    obj.insert("sessionDir".into(), json!(config.session_dir));
    obj.insert("workspaceRoot".into(), json!(config.workspace_root));
    obj.insert(
        "outsideWorkspace".into(),
        json!(config.outside_workspace.as_str()),
    );
    obj.insert(
        "approvalPolicy".into(),
        json!(config.approval_policy.as_str()),
    );
    obj.insert("tools".into(), json!(config.tools));
    obj.insert(
        "toolSelection".into(),
        json!(config.tool_selection.as_str()),
    );
    obj.insert("display".into(), json!(&config.display));
    obj.insert("slashCommands".into(), json!(config.slash_commands));
    obj.insert("context".into(), json!(&config.context));
    obj.insert("history".into(), json!(&config.history));
    obj.insert("projectMemory".into(), json!(&config.project_memory));
    Value::Object(obj)
}

async fn prompt_backend(default: BackendName) -> Result<Option<BackendName>> {
    let backends = BackendName::all();
    let options: Vec<String> = backends
        .iter()
        .map(|b| {
            if *b == default {
                format!("{} *", b.as_str())
            } else {
                b.as_str().to_string()
            }
        })
        .collect();
    let default_idx = backends.iter().position(|b| *b == default).unwrap_or(0);
    let Some(idx) = select_from_list("Backend".into(), options, default_idx).await? else {
        return Ok(None);
    };
    Ok(backends.get(idx).copied())
}

/// For a cloud backend, make sure an API key is available. If one is already
/// set (env var or stored auth file, which is hydrated into the env at
/// startup), say so and move on. Otherwise prompt for it and persist it to the
/// `0600` auth file, also setting it in the current process so the end-of-wizard
/// probe works this session. Local backends need no key, so this is a no-op.
async fn prompt_api_key(chosen: BackendName) -> Result<()> {
    use crate::auth::{auth_file_path, env_var_for, mask_key, AuthStore};

    if chosen.is_local() {
        return Ok(());
    }
    let provider = chosen.as_str();
    let Some(env_name) = env_var_for(provider) else {
        return Ok(());
    };

    let existing = std::env::var(env_name).unwrap_or_default();
    if !existing.trim().is_empty() {
        println!(
            "  {GREEN}✓{RESET} {DIM}{provider} key already set ({}){RESET}",
            mask_key(existing.trim())
        );
        return Ok(());
    }

    println!("  {BOLD}API key{RESET}  {DIM}{provider} needs one.{RESET}");
    let key = plain_read_line(format!(
        "  {CYAN}❯{RESET} {DIM}Paste {provider} API key (visible while typing, blank to skip): {RESET}"
    ))
    .await?
    .trim()
    .to_string();

    if key.is_empty() {
        println!(
            "  {YELLOW}!{RESET} {DIM}Skipped — set {env_name} or run /auth set {provider} before your first prompt.{RESET}"
        );
        return Ok(());
    }

    let mut store = AuthStore::load();
    store.set(provider, &key);
    match store.save() {
        Ok(()) => {
            std::env::set_var(env_name, &key);
            let path = auth_file_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(no path)".into());
            println!(
                "  {GREEN}✓{RESET} {DIM}{provider} →{RESET} {CYAN}{}{RESET} {DIM}(saved to {}){RESET}",
                mask_key(&key),
                path
            );
        }
        Err(e) => {
            // Still set it for this session so the probe can succeed.
            std::env::set_var(env_name, &key);
            println!("  {YELLOW}!{RESET} {DIM}saved for this session, but writing the auth file failed: {e}{RESET}");
        }
    }
    Ok(())
}

/// Last menu entry: user types a custom model id after selecting it.
pub(crate) const MODEL_FREE_TEXT_LABEL: &str = "type a model id…";

/// Build the model menu labels + pick values from a `/models` response.
///
/// Ensures `default_model` and any current override appear even if the backend
/// omitted them, sorts for scanability, marks the preferred row with `*`, and
/// always appends a free-text entry.
///
/// Shared by the setup wizard and the interactive `/model` command.
pub(crate) fn build_model_menu(
    fetched: Vec<String>,
    default_model: &str,
    current_override: Option<&str>,
) -> (Vec<String>, Vec<ModelMenuItem>, usize) {
    let mut ids = Vec::new();
    for id in fetched {
        let trimmed = id.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !ids.iter().any(|existing: &String| existing == trimmed) {
            ids.push(trimmed.to_string());
        }
    }
    for extra in [Some(default_model), current_override]
        .into_iter()
        .flatten()
    {
        let trimmed = extra.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !ids.iter().any(|existing| existing == trimmed) {
            ids.push(trimmed.to_string());
        }
    }
    ids.sort();

    let preferred = current_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(default_model);
    let default_idx = ids.iter().position(|m| m == preferred).unwrap_or(0);

    let mut labels = Vec::with_capacity(ids.len() + 1);
    let mut items = Vec::with_capacity(ids.len() + 1);
    for id in &ids {
        let label = if id == preferred {
            format!("{id} *")
        } else {
            id.clone()
        };
        labels.push(label);
        items.push(ModelMenuItem::Id(id.clone()));
    }
    labels.push(MODEL_FREE_TEXT_LABEL.into());
    items.push(ModelMenuItem::FreeText);

    (labels, items, default_idx)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelMenuItem {
    Id(String),
    FreeText,
}

/// Resolve a chosen model id into a config override. Matching the backend
/// default stores no override so `agent.config.json` stays clean.
pub(crate) fn model_override_for(chosen: &str, default_model: &str) -> Option<String> {
    let trimmed = chosen.trim();
    if trimmed.is_empty() || trimmed == default_model {
        None
    } else {
        Some(trimmed.to_string())
    }
}

async fn prompt_model_free_text(
    default_model: &str,
    current_override: Option<&str>,
) -> Result<Option<Option<String>>> {
    let prompt = if let Some(current) = current_override {
        format!(
            "  {CYAN}❯{RESET} {DIM}Model id [current: {current}; blank: {default_model}]:{RESET} "
        )
    } else {
        format!("  {CYAN}❯{RESET} {DIM}Model id [blank: {default_model}]:{RESET} ")
    };
    let input = plain_read_line(prompt).await?;
    let trimmed = input.trim();
    if is_cancel(trimmed) {
        return Ok(None);
    }
    Ok(Some(model_override_for(trimmed, default_model)))
}

async fn prompt_model(
    chosen_backend: BackendName,
    default_model: &str,
    current_override: Option<&str>,
) -> Result<Option<Option<String>>> {
    use std::io::Write;

    let backend_desc = backend(chosen_backend);
    let http = build_http_client();

    let mut out = std::io::stdout();
    let _ = write!(
        out,
        "  {DIM}Fetching models from {}…{RESET}",
        chosen_backend.as_str()
    );
    let _ = out.flush();

    let fetched = match with_probe_timeout(list_models(&http, &backend_desc)).await {
        Ok(models) => {
            let _ = write!(out, "\r\x1b[K");
            let _ = out.flush();
            models
        }
        Err(e) => {
            let _ = write!(out, "\r\x1b[K");
            let _ = out.flush();
            println!(
                "  {YELLOW}!{RESET} {DIM}Could not list models ({e}); enter a model id.{RESET}"
            );
            return prompt_model_free_text(default_model, current_override).await;
        }
    };

    if fetched.is_empty() {
        println!("  {YELLOW}!{RESET} {DIM}No models returned; enter a model id.{RESET}");
        return prompt_model_free_text(default_model, current_override).await;
    }

    let (labels, items, default_idx) = build_model_menu(fetched, default_model, current_override);
    let Some(idx) = select_from_list("Model".into(), labels, default_idx).await? else {
        return Ok(None);
    };
    match items.get(idx) {
        Some(ModelMenuItem::Id(id)) => Ok(Some(model_override_for(id, default_model))),
        Some(ModelMenuItem::FreeText) => {
            prompt_model_free_text(default_model, current_override).await
        }
        None => Ok(None),
    }
}

async fn prompt_approval(default: ApprovalPolicy) -> Result<Option<ApprovalPolicy>> {
    let policies = [
        ApprovalPolicy::Always,
        ApprovalPolicy::DangerousOnly,
        ApprovalPolicy::Never,
    ];
    let options: Vec<String> = policies
        .iter()
        .map(|p| {
            if *p == default {
                format!("{} *", p.as_str())
            } else {
                p.as_str().to_string()
            }
        })
        .collect();
    let default_idx = policies.iter().position(|p| *p == default).unwrap_or(0);
    let Some(idx) = select_from_list("Approval policy".into(), options, default_idx).await? else {
        return Ok(None);
    };
    Ok(policies.get(idx).copied())
}

async fn prompt_tool_selection(default: ToolSelection) -> Result<Option<ToolSelection>> {
    let modes = [ToolSelection::Auto, ToolSelection::Fixed];
    let options: Vec<String> = modes
        .iter()
        .map(|s| {
            if *s == default {
                format!("{} *", s.as_str())
            } else {
                s.as_str().to_string()
            }
        })
        .collect();
    let default_idx = modes.iter().position(|s| *s == default).unwrap_or(0);
    let Some(idx) = select_from_list("Tool mode".into(), options, default_idx).await? else {
        return Ok(None);
    };
    Ok(modes.get(idx).copied())
}

async fn probe_setup_backend(config: &AgentConfig) {
    let http = build_http_client();
    let backend_desc = config.backend_descriptor();
    let model = default_model(&backend_desc, config.model_override.as_deref());
    println!(
        "  {DIM}Probing {} at {} with {CYAN}{}{RESET}{DIM}…{RESET}",
        config.backend.as_str(),
        backend_desc.base_url,
        model
    );

    if let Err(e) = validate(&backend_desc) {
        println!("  {RED}✗{RESET} {DIM}{e}{RESET}");
        println!("  {DIM}Hint: {}{RESET}", backend_hint(config.backend));
        return;
    }

    let models = match with_probe_timeout(list_models(&http, &backend_desc)).await {
        Ok(models) => models,
        Err(e) => {
            println!("  {YELLOW}!{RESET} {DIM}Backend not reachable: {e}{RESET}");
            println!("  {DIM}Hint: {}{RESET}", backend_hint(config.backend));
            return;
        }
    };
    println!(
        "  {GREEN}✓{RESET} {DIM}model list reachable ({} models){RESET}",
        models.len()
    );

    let messages = [ChatMessage::User {
        content: "Reply with ok.".into(),
    }];
    let req = ChatRequest {
        model: &model,
        messages: &messages,
        tools: None,
        stream: false,
        stream_options: None,
        max_tokens: Some(4),
        prompt_cache_key: None,
        effort: None,
    };
    match with_probe_timeout(chat_oneshot(&http, &backend_desc, &req)).await {
        Ok(()) => println!("  {GREEN}✓{RESET} {DIM}chat completion reachable{RESET}"),
        Err(e) => {
            println!("  {YELLOW}!{RESET} {DIM}chat probe failed: {e}{RESET}");
            println!(
                "  {DIM}The config is saved. Check the model id or start the backend, then run /doctor.{RESET}"
            );
        }
    }
}

async fn with_probe_timeout<T>(future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    match tokio::time::timeout(Duration::from_secs(8), future).await {
        Ok(result) => result,
        Err(_) => anyhow::bail!("timed out after 8s"),
    }
}

fn is_cancel(s: &str) -> bool {
    matches!(s.trim().to_lowercase().as_str(), "q" | "quit" | "cancel")
}

fn is_truthy(s: &str) -> bool {
    matches!(
        s.trim().to_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn backend_hint(backend: BackendName) -> &'static str {
    match backend {
        BackendName::Ollama => {
            "run `ollama serve` and pull a model such as `ollama pull qwen2.5-coder:7b`."
        }
        BackendName::LmStudio => {
            "open LM Studio, go to Local Server, load a model, and start the server on port 1234."
        }
        BackendName::Mlx => {
            "start an OpenAI-compatible MLX server, for example `mlx_lm.server --port 8080`."
        }
        BackendName::LlamaCpp => {
            "run `llama-server -m /path/to/model.gguf --host 127.0.0.1 --port 8080 --jinja`."
        }
        BackendName::Openrouter => "set `OPENROUTER_API_KEY` before using the OpenRouter backend.",
        BackendName::OpenAi => {
            "set `OPENAI_API_KEY` before using the OpenAI backend (optionally `OPENAI_BASE_URL` for a compatible proxy)."
        }
        BackendName::OpenAiCodex => {
            "run `/login openai-codex` to sign in with ChatGPT/Codex subscription OAuth."
        }
        BackendName::Grok => {
            "run `/login grok` to sign in with SuperGrok / X Premium+ (browser or device-code)."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_truthy_skip_values() {
        assert!(is_truthy("true"));
        assert!(is_truthy("1"));
        assert!(is_truthy("YES"));
        assert!(is_truthy("on"));
        assert!(!is_truthy("false"));
        assert!(!is_truthy(""));
    }

    #[test]
    fn setup_config_json_uses_public_contract_names() {
        let config = AgentConfig {
            backend: BackendName::LlamaCpp,
            model_override: Some("local-gguf".into()),
            approval_policy: ApprovalPolicy::DangerousOnly,
            tool_selection: ToolSelection::Fixed,
            ..Default::default()
        };

        let value = setup_config_value(&config);

        assert_eq!(value["backend"], "llamacpp");
        assert_eq!(value["modelOverride"], "local-gguf");
        assert_eq!(value["approvalPolicy"], "dangerous-only");
        assert_eq!(value["toolSelection"], "fixed");
        assert_eq!(value["outsideWorkspace"], "prompt");
        assert_eq!(value["slashCommands"], true);
        assert_eq!(value["projectMemory"]["enabled"], true);
        assert!(value.get("systemPrompt").is_none());
    }

    #[test]
    fn first_run_setup_requires_missing_config_and_enabled_wizard() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.config.json");
        let previous = std::env::var(NO_WIZARD_ENV).ok();

        std::env::set_var(NO_WIZARD_ENV, "false");
        assert!(should_run_first_run_setup(&path));

        std::fs::write(&path, "{}").unwrap();
        assert!(!should_run_first_run_setup(&path));

        if let Some(value) = previous {
            std::env::set_var(NO_WIZARD_ENV, value);
        } else {
            std::env::remove_var(NO_WIZARD_ENV);
        }
    }

    #[test]
    fn model_menu_lists_fetched_ids_with_free_text_last() {
        let (labels, items, default_idx) = build_model_menu(
            vec!["zeta".into(), "alpha".into(), "alpha".into()],
            "alpha",
            None,
        );
        assert_eq!(
            labels.last().map(String::as_str),
            Some(MODEL_FREE_TEXT_LABEL)
        );
        assert_eq!(items.last(), Some(&ModelMenuItem::FreeText));
        // Sorted, deduped, default marked, free-text trailing.
        assert_eq!(
            labels,
            vec![
                "alpha *".to_string(),
                "zeta".to_string(),
                MODEL_FREE_TEXT_LABEL.to_string()
            ]
        );
        assert_eq!(default_idx, 0);
        assert_eq!(
            items,
            vec![
                ModelMenuItem::Id("alpha".into()),
                ModelMenuItem::Id("zeta".into()),
                ModelMenuItem::FreeText
            ]
        );
    }

    #[test]
    fn model_menu_injects_default_and_prefers_current_override() {
        let (labels, items, default_idx) =
            build_model_menu(vec!["other".into()], "backend-default", Some("my-custom"));
        assert!(items.contains(&ModelMenuItem::Id("backend-default".into())));
        assert!(items.contains(&ModelMenuItem::Id("my-custom".into())));
        assert!(items.contains(&ModelMenuItem::Id("other".into())));
        assert_eq!(items.last(), Some(&ModelMenuItem::FreeText));
        assert!(labels[default_idx].starts_with("my-custom"));
        assert!(labels[default_idx].contains('*'));
    }

    #[test]
    fn model_override_omits_backend_default() {
        assert_eq!(model_override_for("gpt-4o-mini", "gpt-4o-mini"), None);
        assert_eq!(model_override_for("  ", "gpt-4o-mini"), None);
        assert_eq!(
            model_override_for("gpt-4o", "gpt-4o-mini"),
            Some("gpt-4o".into())
        );
    }
}
