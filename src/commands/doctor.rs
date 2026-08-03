//! `/doctor` command group: backend probing + model tuning (recommend,
//! bench, capabilities, autotune). Split out of commands.rs; dispatch
//! lives in mod.rs.

use super::*;

/// `/doctor` is the single entry point for backend probing and model tuning.
/// Subcommands route to the dedicated handlers; bare `/doctor` (and the probe
/// flags `--deep` / `all`) run the connectivity probe.
pub(super) async fn cmd_doctor(args: &str, state: &mut AppState) -> Result<()> {
    let sub = args.split_whitespace().next().unwrap_or("");
    let rest = args.strip_prefix(sub).unwrap_or("").trim();
    match sub {
        "" | "--deep" | "deep" | "all" => cmd_doctor_probe(args, state).await,
        "recommend" => cmd_recommend(rest, state).await,
        "autotune" => cmd_autotune(rest, state).await,
        "bench" => cmd_bench(rest, state).await,
        "models" | "capabilities" | "caps" => cmd_capabilities(rest, state).await,
        other => {
            println!("  {DIM}Unknown /doctor subcommand: {other}{RESET}");
            println!("  {DIM}Try: {CYAN}/doctor{RESET}{DIM} [--deep], {CYAN}/doctor recommend{RESET}{DIM}, {CYAN}/doctor bench{RESET}{DIM}, {CYAN}/doctor models{RESET}{DIM}, {CYAN}/doctor autotune [apply]{RESET}");
            Ok(())
        }
    }
}

async fn cmd_doctor_probe(args: &str, state: &AppState) -> Result<()> {
    let deep = args
        .split_whitespace()
        .any(|arg| arg == "--deep" || arg == "deep");
    let all = args.split_whitespace().any(|arg| arg == "all");
    if deep {
        return cmd_doctor_deep(state, all).await;
    }

    println!("  {BOLD}Albatross doctor{RESET}");
    println!(
        "  {DIM}provider{RESET} {} · {}",
        state.config.backend.as_str(),
        state.backend.base_url
    );
    match list_models(&state.http, &state.backend).await {
        Ok(models) => println!(
            "  {GREEN}✓{RESET} {DIM}models reachable ({}){RESET}",
            models.len()
        ),
        Err(e) => println!("  {RED}✗{RESET} {DIM}models unreachable: {e}{RESET}"),
    }
    let rg = tokio::process::Command::new("rg")
        .arg("--version")
        .output()
        .await;
    match rg {
        Ok(o) if o.status.success() => println!("  {GREEN}✓{RESET} {DIM}ripgrep available{RESET}"),
        _ => println!("  {YELLOW}!{RESET} {DIM}ripgrep unavailable; grep tool will fail{RESET}"),
    }
    fs::create_dir_all(&state.session_dir)?;
    let probe = Path::new(&state.session_dir).join(".doctor-write-test");
    match fs::write(&probe, "ok").and_then(|_| fs::remove_file(&probe)) {
        Ok(_) => println!("  {GREEN}✓{RESET} {DIM}session dir writable{RESET}"),
        Err(e) => println!("  {RED}✗{RESET} {DIM}session dir not writable: {e}{RESET}"),
    }
    if matches!(state.config.backend, BackendName::OpenAiCodex) {
        if crate::auth::AuthStore::load()
            .get_oauth("openai-codex")
            .is_none()
        {
            println!(
                "  {RED}✗{RESET} {DIM}openai-codex login missing; run /login openai-codex{RESET}"
            );
        }
    } else if matches!(state.config.backend, BackendName::Grok) {
        if crate::auth::AuthStore::load()
            .get_oauth(crate::xai_oauth::PROVIDER)
            .is_none()
        {
            println!("  {RED}✗{RESET} {DIM}grok login missing; run /login grok{RESET}");
        }
    } else if !state.config.backend.is_local() && state.backend.api_key.is_empty() {
        let env_name = match state.config.backend {
            BackendName::Openrouter => "OPENROUTER_API_KEY",
            BackendName::OpenAi => "OPENAI_API_KEY",
            BackendName::Anthropic => "ANTHROPIC_API_KEY",
            BackendName::OpenAiCodex => "ChatGPT login",
            BackendName::Grok => "Grok login",
            _ => "API key",
        };
        println!("  {RED}✗{RESET} {DIM}{env_name} missing{RESET}");
    }
    println!(
        "  {DIM}workspace{RESET} {} ({})",
        state.config.workspace_root,
        state.config.outside_workspace.as_str()
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DoctorCapabilityReport {
    generated_at: String,
    active_backend: String,
    rows: Vec<DoctorCapabilityRow>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DoctorCapabilityRow {
    backend: String,
    base_url: String,
    model: String,
    models: ProbeStatus,
    streaming: ProbeStatus,
    usage_chunks: ProbeStatus,
    tool_calls: ProbeStatus,
    inline_tool_json: ProbeStatus,
    warning: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProbeStatus {
    ok: bool,
    detail: String,
}

impl ProbeStatus {
    fn ok(detail: impl Into<String>) -> Self {
        Self {
            ok: true,
            detail: detail.into(),
        }
    }

    fn fail(detail: impl Into<String>) -> Self {
        Self {
            ok: false,
            detail: detail.into(),
        }
    }
}

async fn with_probe_timeout<T>(future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    match tokio::time::timeout(Duration::from_secs(8), future).await {
        Ok(result) => result,
        Err(_) => Err(anyhow!("timed out after 8s")),
    }
}

fn doctor_backend_model(
    backend_desc: &BackendDescriptor,
    listed_models: &[String],
    state: &AppState,
) -> String {
    if backend_desc.name == state.backend.name {
        return state.model.clone();
    }
    listed_models
        .first()
        .cloned()
        .unwrap_or_else(|| default_model(backend_desc, None))
}

async fn probe_streaming(
    state: &AppState,
    backend_desc: &BackendDescriptor,
    model: &str,
) -> (ProbeStatus, ProbeStatus) {
    let messages = vec![ChatMessage::User {
        content: "Reply with exactly: ok".into(),
    }];
    let req = ChatRequest {
        model,
        messages: &messages,
        tools: None,
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
        max_tokens: Some(8),
        prompt_cache_key: None,
        effort: None,
    };
    let mut chunks = 0usize;
    let mut content = String::new();
    let mut saw_usage = false;
    let result = with_probe_timeout(stream_chat(
        &state.http,
        backend_desc,
        &req,
        None,
        |chunk| {
            chunks += 1;
            if chunk.usage.is_some() {
                saw_usage = true;
            }
            if let Some(choice) = chunk.choices.first() {
                if let Some(delta) = &choice.delta.content {
                    content.push_str(delta);
                }
            }
        },
    ))
    .await;
    match result {
        Ok(_) => (
            ProbeStatus::ok(format!("{chunks} chunks, {:?}", content.trim())),
            if saw_usage {
                ProbeStatus::ok("usage chunk observed")
            } else {
                ProbeStatus::fail("no usage chunk observed")
            },
        ),
        Err(e) => (
            ProbeStatus::fail(e.to_string()),
            ProbeStatus::fail("streaming probe failed"),
        ),
    }
}

fn doctor_noop_tool() -> Vec<ToolDef> {
    vec![ToolDef {
        kind: "function",
        function: ToolDefFunction {
            name: "doctor_noop".into(),
            description: "Harmless diagnostic tool. Call this when asked.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "ok": { "type": "boolean" }
                },
                "required": ["ok"]
            }),
        },
    }]
}

fn looks_like_inline_tool_json(content: &str) -> bool {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(content.trim()) else {
        return false;
    };
    parsed
        .get("name")
        .and_then(|v| v.as_str())
        .map(|name| name == "doctor_noop")
        .unwrap_or(false)
}

async fn probe_tool_calls(
    state: &AppState,
    backend_desc: &BackendDescriptor,
    model: &str,
) -> (ProbeStatus, ProbeStatus) {
    let messages = vec![ChatMessage::User {
        content: "Call the doctor_noop tool with ok=true. Do not answer in prose.".into(),
    }];
    let tools = doctor_noop_tool();
    let req = ChatRequest {
        model,
        messages: &messages,
        tools: Some(&tools),
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: false,
        }),
        max_tokens: Some(128),
        prompt_cache_key: None,
        effort: None,
    };
    let mut content = String::new();
    let mut tool_name = String::new();
    let mut tool_args = String::new();
    let result = with_probe_timeout(stream_chat(
        &state.http,
        backend_desc,
        &req,
        None,
        |chunk| {
            if let Some(choice) = chunk.choices.first() {
                if let Some(delta) = &choice.delta.content {
                    content.push_str(delta);
                }
                if let Some(tool_calls) = &choice.delta.tool_calls {
                    for call in tool_calls {
                        if let Some(function) = &call.function {
                            if let Some(name) = &function.name {
                                tool_name.push_str(name);
                            }
                            if let Some(args) = &function.arguments {
                                tool_args.push_str(args);
                            }
                        }
                    }
                }
            }
        },
    ))
    .await;
    match result {
        Ok(_) => {
            let native_ok = tool_name == "doctor_noop";
            let inline_ok = looks_like_inline_tool_json(&content);
            let native_detail = if native_ok {
                let args = if tool_args.is_empty() {
                    "{}"
                } else {
                    tool_args.as_str()
                };
                format!("native tool_calls: {args}")
            } else if inline_ok {
                "no native tool_calls; model emitted inline JSON".into()
            } else if content.trim().is_empty() {
                "no tool call or content observed".into()
            } else {
                format!("no native tool_calls; content {:?}", content.trim())
            };
            (
                ProbeStatus {
                    ok: native_ok,
                    detail: native_detail,
                },
                ProbeStatus {
                    ok: inline_ok,
                    detail: if inline_ok {
                        "inline JSON fallback likely usable".into()
                    } else {
                        "inline JSON fallback not observed".into()
                    },
                },
            )
        }
        Err(e) => (
            ProbeStatus::fail(e.to_string()),
            ProbeStatus::fail("tool probe failed"),
        ),
    }
}

fn doctor_warning(row: &DoctorCapabilityRow) -> Option<String> {
    if row.backend == "llamacpp" && row.streaming.ok && !row.tool_calls.ok {
        return Some(
            "llama.cpp is reachable but native tool_calls were not observed; start llama-server with --jinja for native tool calls".into(),
        );
    }
    if row.streaming.ok && !row.usage_chunks.ok {
        return Some("streaming works, but usage chunks are not reported by this provider".into());
    }
    None
}

fn status_to_cache(status: &ProbeStatus) -> CapabilityStatus {
    CapabilityStatus {
        ok: status.ok,
        detail: status.detail.clone(),
    }
}

fn row_to_capability_record(row: &DoctorCapabilityRow, generated_at: &str) -> CapabilityRecord {
    CapabilityRecord {
        generated_at: generated_at.into(),
        backend: row.backend.clone(),
        base_url: row.base_url.clone(),
        model: row.model.clone(),
        models: status_to_cache(&row.models),
        streaming: status_to_cache(&row.streaming),
        usage_chunks: status_to_cache(&row.usage_chunks),
        tool_calls: status_to_cache(&row.tool_calls),
        inline_tool_json: status_to_cache(&row.inline_tool_json),
        warning: row.warning.clone(),
        benchmark: None,
    }
}

async fn probe_backend_capabilities(
    state: &AppState,
    backend_desc: BackendDescriptor,
) -> DoctorCapabilityRow {
    let mut listed_models = Vec::new();
    let models = match validate(&backend_desc) {
        Ok(()) => match with_probe_timeout(list_models(&state.http, &backend_desc)).await {
            Ok(models) => {
                listed_models = models;
                ProbeStatus::ok(format!("{} model(s)", listed_models.len()))
            }
            Err(e) => ProbeStatus::fail(e.to_string()),
        },
        Err(e) => ProbeStatus::fail(e.to_string()),
    };
    let model = doctor_backend_model(&backend_desc, &listed_models, state);
    let (streaming, usage_chunks, tool_calls, inline_tool_json) = if models.ok {
        let (streaming, usage_chunks) = probe_streaming(state, &backend_desc, &model).await;
        let (tool_calls, inline_tool_json) = if streaming.ok {
            probe_tool_calls(state, &backend_desc, &model).await
        } else {
            (
                ProbeStatus::fail("skipped because streaming failed"),
                ProbeStatus::fail("skipped because streaming failed"),
            )
        };
        (streaming, usage_chunks, tool_calls, inline_tool_json)
    } else {
        (
            ProbeStatus::fail("skipped because /models failed"),
            ProbeStatus::fail("skipped because /models failed"),
            ProbeStatus::fail("skipped because /models failed"),
            ProbeStatus::fail("skipped because /models failed"),
        )
    };
    let mut row = DoctorCapabilityRow {
        backend: backend_desc.name.as_str().into(),
        base_url: backend_desc.base_url,
        model,
        models,
        streaming,
        usage_chunks,
        tool_calls,
        inline_tool_json,
        warning: None,
    };
    row.warning = doctor_warning(&row);
    row
}

fn mark(status: &ProbeStatus) -> &'static str {
    if status.ok {
        "yes"
    } else {
        "no"
    }
}

fn print_capability_table(rows: &[DoctorCapabilityRow]) {
    println!();
    println!(
        "  {BOLD}{:<11}{RESET} {:<7} {:<9} {:<6} {:<10} warning",
        "provider", "models", "stream", "usage", "toolcalls"
    );
    for row in rows {
        println!(
            "  {CYAN}{:<11}{RESET} {:<7} {:<9} {:<6} {:<10} {}",
            row.backend,
            mark(&row.models),
            mark(&row.streaming),
            mark(&row.usage_chunks),
            mark(&row.tool_calls),
            row.warning.as_deref().unwrap_or("")
        );
    }
    println!();
    for row in rows {
        println!(
            "  {BOLD}{}{RESET} {DIM}{} · {}{RESET}",
            row.backend, row.base_url, row.model
        );
        println!("    {DIM}models:{RESET} {}", row.models.detail);
        println!("    {DIM}streaming:{RESET} {}", row.streaming.detail);
        println!("    {DIM}usage:{RESET} {}", row.usage_chunks.detail);
        println!("    {DIM}tool_calls:{RESET} {}", row.tool_calls.detail);
        println!(
            "    {DIM}inline_json:{RESET} {}",
            row.inline_tool_json.detail
        );
    }
}

fn render_doctor_markdown(report: &DoctorCapabilityReport) -> String {
    let mut out = format!(
        "# Albatross Doctor Report\n\nGenerated: `{}`\n\nActive provider: `{}`\n\n",
        report.generated_at, report.active_backend
    );
    out.push_str("| Provider | Models | Streaming | Usage | Tool Calls | Warning |\n");
    out.push_str("| --- | --- | --- | --- | --- | --- |\n");
    for row in &report.rows {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} |\n",
            row.backend,
            mark(&row.models),
            mark(&row.streaming),
            mark(&row.usage_chunks),
            mark(&row.tool_calls),
            row.warning.as_deref().unwrap_or("")
        ));
    }
    out.push('\n');
    for row in &report.rows {
        out.push_str(&format!("## `{}`\n\n", row.backend));
        out.push_str(&format!("- Base URL: `{}`\n", row.base_url));
        out.push_str(&format!("- Model: `{}`\n", row.model));
        out.push_str(&format!("- Models: {}\n", row.models.detail));
        out.push_str(&format!("- Streaming: {}\n", row.streaming.detail));
        out.push_str(&format!("- Usage chunks: {}\n", row.usage_chunks.detail));
        out.push_str(&format!("- Tool calls: {}\n", row.tool_calls.detail));
        out.push_str(&format!(
            "- Inline JSON fallback: {}\n\n",
            row.inline_tool_json.detail
        ));
    }
    out
}

fn save_doctor_report(
    state: &AppState,
    report: &DoctorCapabilityReport,
) -> Result<(PathBuf, PathBuf)> {
    let dir = Path::new(&state.session_dir).join("doctor");
    fs::create_dir_all(&dir)?;
    let id = Utc::now().format("%Y-%m-%dT%H-%M-%S-%3fZ").to_string();
    let json_path = dir.join(format!("{id}.json"));
    let md_path = dir.join(format!("{id}.md"));
    fs::write(&json_path, serde_json::to_string_pretty(report)?)?;
    fs::write(&md_path, render_doctor_markdown(report))?;
    Ok((json_path, md_path))
}

async fn cmd_doctor_deep(state: &AppState, all: bool) -> Result<()> {
    println!("  {BOLD}Albatross doctor --deep{RESET}");
    println!(
        "  {DIM}probing OpenAI-compatible model, stream, usage, and tool-call behavior{RESET}"
    );
    let backends: Vec<BackendName> = if all {
        BackendName::all().to_vec()
    } else {
        vec![state.config.backend]
    };
    let mut rows = Vec::new();
    for name in backends {
        let backend_desc = if name == state.backend.name {
            state.backend.clone()
        } else {
            backend(name)
        };
        println!(
            "  {DIM}probing {} at {}…{RESET}",
            backend_desc.name.as_str(),
            backend_desc.base_url
        );
        rows.push(probe_backend_capabilities(state, backend_desc).await);
    }
    print_capability_table(&rows);
    let report = DoctorCapabilityReport {
        generated_at: Utc::now().to_rfc3339(),
        active_backend: state.config.backend.as_str().into(),
        rows,
    };
    let mut cached = 0usize;
    for row in &report.rows {
        let record = row_to_capability_record(row, &report.generated_at);
        capabilities::save_record(&state.session_dir, record)?;
        cached += 1;
    }
    let (json_path, md_path) = save_doctor_report(state, &report)?;
    println!(
        "  {GREEN}✓{RESET} {DIM}doctor report saved → {} and {}{RESET}",
        json_path.display(),
        md_path.display()
    );
    println!(
        "  {GREEN}✓{RESET} {DIM}cached {} capability record(s) under {}/capabilities{RESET}",
        cached, state.session_dir
    );
    Ok(())
}

async fn cmd_bench(args: &str, state: &AppState) -> Result<()> {
    let model = if args.is_empty() { &state.model } else { args };
    let messages = vec![ChatMessage::User {
        content: "Reply with one short sentence for a latency benchmark.".into(),
    }];
    println!("  {DIM}warming {model}…{RESET}");
    let warm_ms = warmup(
        &state.http,
        &state.backend,
        model,
        state.active_effort,
        "benchmark",
        &[],
    )
    .await
    .ok();
    let req = ChatRequest {
        model,
        messages: &messages,
        tools: None,
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: false,
        }),
        max_tokens: None,
        prompt_cache_key: None,
        effort: state.active_effort,
    };
    let start = Instant::now();
    let mut first = None;
    let mut chars = 0usize;
    stream_chat(&state.http, &state.backend, &req, None, |chunk| {
        if let Some(choice) = chunk.choices.first() {
            if let Some(content) = &choice.delta.content {
                if first.is_none() {
                    first = Some(start.elapsed());
                }
                chars += content.chars().count();
            }
        }
    })
    .await?;
    let total = start.elapsed();
    let stats = BenchmarkStats::new(
        warm_ms,
        first.map(|d| d.as_millis()),
        total.as_millis(),
        chars,
    );
    let cps = if total.as_secs_f64() > 0.0 {
        chars as f64 / total.as_secs_f64()
    } else {
        0.0
    };
    let cache_path =
        capabilities::save_benchmark(&state.session_dir, &state.backend, model, stats)?;
    println!(
        "  {GREEN}✓{RESET} {DIM}warmup={:?} firstToken={:.2}s total={:.2}s charsPerSec={:.1}{RESET}",
        warm_ms.map(|ms| format!("{:.2}s", ms as f64 / 1000.0)),
        first.map(|d| d.as_secs_f64()).unwrap_or(0.0),
        total.as_secs_f64(),
        cps
    );
    println!(
        "  {GREEN}✓{RESET} {DIM}benchmark cached → {}{RESET}",
        cache_path.display()
    );
    Ok(())
}

async fn refresh_capability_cache(state: &AppState, all: bool) -> Result<Vec<CapabilityRecord>> {
    let backends: Vec<BackendName> = if all {
        BackendName::all().to_vec()
    } else {
        vec![state.config.backend]
    };
    let generated_at = Utc::now().to_rfc3339();
    let mut records = Vec::new();
    for name in backends {
        let backend_desc = if name == state.backend.name {
            state.backend.clone()
        } else {
            backend(name)
        };
        println!(
            "  {DIM}probing {} at {}…{RESET}",
            backend_desc.name.as_str(),
            backend_desc.base_url
        );
        let row = probe_backend_capabilities(state, backend_desc).await;
        let record = row_to_capability_record(&row, &generated_at);
        capabilities::save_record(&state.session_dir, record.clone())?;
        records.push(record);
    }
    Ok(records)
}

fn short_model(model: &str, width: usize) -> String {
    if model.chars().count() <= width {
        return model.into();
    }
    let mut out: String = model.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn bench_label(record: &CapabilityRecord) -> String {
    record
        .benchmark
        .as_ref()
        .map(|bench| {
            format!(
                "{:.1} tok/s · {:.2}s first",
                bench.estimated_tokens_per_sec,
                bench.first_token_ms.unwrap_or(0) as f64 / 1000.0
            )
        })
        .unwrap_or_else(|| "not benched".into())
}

fn print_cached_capabilities(records: &[CapabilityRecord]) {
    println!();
    println!(
        "  {BOLD}{:<11}{RESET} {:<28} {:>5} {:<11} {:<10} benchmark",
        "provider", "model", "score", "tools", "usage"
    );
    for record in sorted_records(records) {
        println!(
            "  {CYAN}{:<11}{RESET} {:<28} {:>5} {:<11} {:<10} {}",
            record.backend,
            short_model(&record.model, 28),
            record_score(&record),
            record.tool_path(),
            mark_cache(&record.usage_chunks),
            bench_label(&record)
        );
        if let Some(warning) = &record.warning {
            println!("    {YELLOW}!{RESET} {DIM}{warning}{RESET}");
        }
    }
}

fn mark_cache(status: &CapabilityStatus) -> &'static str {
    if status.ok {
        "yes"
    } else {
        "no"
    }
}

async fn cmd_capabilities(args: &str, state: &AppState) -> Result<()> {
    let refresh = args
        .split_whitespace()
        .any(|arg| arg == "refresh" || arg == "--refresh");
    let all = args
        .split_whitespace()
        .any(|arg| arg == "all" || arg == "--all");
    if refresh {
        let records = refresh_capability_cache(state, all).await?;
        println!(
            "  {GREEN}✓{RESET} {DIM}cached {} refreshed capability record(s).{RESET}",
            records.len()
        );
    }

    let records = capabilities::load_records(&state.session_dir)?;
    if records.is_empty() {
        println!(
            "  {DIM}No cached capabilities yet. Run /doctor models refresh or /doctor --deep all.{RESET}"
        );
        return Ok(());
    }

    print_cached_capabilities(&records);
    println!(
        "  {DIM}cache{RESET} {}",
        capabilities::cache_dir(&state.session_dir).display()
    );
    Ok(())
}

async fn cmd_autotune(args: &str, state: &mut AppState) -> Result<()> {
    let parts: Vec<&str> = args.split_whitespace().collect();
    let apply = parts.iter().any(|arg| *arg == "apply" || *arg == "--apply");
    let include_cloud = parts.iter().any(|arg| *arg == "cloud" || *arg == "--cloud");
    let refresh = parts
        .iter()
        .any(|arg| *arg == "refresh" || *arg == "--refresh");
    let all = parts.iter().any(|arg| *arg == "all" || *arg == "--all");

    let mut records = capabilities::load_records(&state.session_dir)?;
    if refresh || records.is_empty() {
        let refreshed = refresh_capability_cache(state, all).await?;
        if !refreshed.is_empty() {
            records = capabilities::load_records(&state.session_dir)?;
        }
    }

    let Some(recommendation) = best_record(&records, include_cloud) else {
        if records.iter().any(CapabilityRecord::is_cloud) && !include_cloud {
            println!(
                "  {DIM}Only cloud-capable cached records were found. Re-run with /doctor autotune --cloud to include them.{RESET}"
            );
        } else {
            println!(
                "  {DIM}No usable cached model yet. Run /doctor autotune refresh all after starting your local providers.{RESET}"
            );
        }
        return Ok(());
    };

    let tool_selection = recommended_tool_selection(&recommendation);
    println!("  {BOLD}Autotune recommendation{RESET}");
    println!(
        "  {DIM}provider{RESET} {} · {DIM}model{RESET} {}",
        recommendation.backend, recommendation.model
    );
    println!(
        "  {DIM}score{RESET} {} · {DIM}tools{RESET} {} · {DIM}toolSelection{RESET} {}",
        record_score(&recommendation),
        recommendation.tool_path(),
        tool_selection.as_str()
    );
    println!(
        "  {DIM}bench{RESET} {} · {DIM}warmup{RESET} {}",
        bench_label(&recommendation),
        if warmup_recommended(&recommendation) {
            "recommended"
        } else {
            "optional"
        }
    );
    if let Some(warning) = &recommendation.warning {
        println!("  {YELLOW}!{RESET} {DIM}{warning}{RESET}");
    }

    if !apply {
        println!("  {DIM}Run /doctor autotune apply to switch this session to the recommendation.{RESET}");
        return Ok(());
    }

    let Some(backend_name) = recommendation.backend_name() else {
        println!(
            "  {RED}✗{RESET} {DIM}Cannot apply unknown provider: {}{RESET}",
            recommendation.backend
        );
        return Ok(());
    };
    let env_backend = backend(backend_name);
    if env_backend.base_url != recommendation.base_url {
        println!(
            "  {YELLOW}!{RESET} {DIM}cached URL was {}, current env resolves to {}{RESET}",
            recommendation.base_url, env_backend.base_url
        );
    }
    state.config.backend = backend_name;
    state.config.model_override = Some(recommendation.model.clone());
    state.active_effort = None;
    state.config.tool_selection = tool_selection;
    state.rebuild_client()?;
    state.resolve_model();
    println!(
        "  {GREEN}✓{RESET} {DIM}active session tuned → {} · {} · tools={}{RESET}",
        state.config.backend.as_str(),
        state.model,
        state.config.tool_selection.as_str()
    );
    Ok(())
}

fn recommend_backend_names(state: &AppState, all: bool, include_cloud: bool) -> Vec<BackendName> {
    if all {
        BackendName::all()
            .iter()
            .copied()
            .filter(|name| include_cloud || name.is_local())
            .collect()
    } else {
        vec![state.config.backend]
    }
}

async fn refresh_recommendation_capabilities(
    state: &AppState,
    all: bool,
    include_cloud: bool,
) -> Result<usize> {
    let generated_at = Utc::now().to_rfc3339();
    let mut cached = 0usize;
    for name in recommend_backend_names(state, all, include_cloud) {
        let backend_desc = if name == state.backend.name {
            state.backend.clone()
        } else {
            backend(name)
        };
        println!(
            "  {DIM}probing {} at {}…{RESET}",
            backend_desc.name.as_str(),
            backend_desc.base_url
        );
        let row = probe_backend_capabilities(state, backend_desc).await;
        let record = row_to_capability_record(&row, &generated_at);
        capabilities::save_record(&state.session_dir, record)?;
        cached += 1;
    }
    Ok(cached)
}

async fn collect_recommendation_candidates(
    state: &AppState,
    all: bool,
    include_cloud: bool,
) -> Result<Vec<ModelCandidate>> {
    let mut candidates = Vec::new();
    for name in recommend_backend_names(state, all, include_cloud) {
        let backend_desc = if name == state.backend.name {
            state.backend.clone()
        } else {
            backend(name)
        };
        let default = default_model(&backend_desc, None);
        let mut default_candidate =
            ModelCandidate::new(backend_desc.name, backend_desc.base_url.clone(), default);
        default_candidate.is_default = true;
        candidates.push(default_candidate);

        if backend_desc.name == state.backend.name {
            let mut current = ModelCandidate::new(
                backend_desc.name,
                backend_desc.base_url.clone(),
                &state.model,
            );
            current.is_current = true;
            candidates.push(current);
        }

        if validate(&backend_desc).is_err() {
            continue;
        }
        let models = match with_probe_timeout(list_models(&state.http, &backend_desc)).await {
            Ok(models) => models,
            Err(_) => continue,
        };
        for model in models {
            let mut candidate =
                ModelCandidate::new(backend_desc.name, backend_desc.base_url.clone(), model);
            candidate.installed = true;
            candidates.push(candidate);
        }
    }

    for record in capabilities::load_records(&state.session_dir)? {
        let Some(backend_name) = record.backend_name() else {
            continue;
        };
        if !include_cloud && !backend_name.is_local() {
            continue;
        }
        let mut candidate =
            ModelCandidate::new(backend_name, record.base_url.clone(), record.model.clone());
        candidate.capability = Some(record);
        candidates.push(candidate);
    }

    Ok(candidates)
}

fn hardware_summary(spec: &HardwareSpec) -> String {
    let chip = spec.chip_name.as_deref().unwrap_or("unknown chip");
    let machine = spec.machine_name.as_deref().unwrap_or("unknown machine");
    format!(
        "{} {} · {} · {} · {}",
        spec.os,
        spec.arch,
        machine,
        chip,
        spec.memory_label()
    )
}

fn model_size_label(rec: &ModelRecommendation) -> String {
    let size = rec
        .metadata
        .parameters_b
        .map(|params| format!("{params:.0}B"))
        .unwrap_or_else(|| "unknown size".into());
    let quant = rec
        .metadata
        .quant_bits
        .map(|bits| format!("q{bits}"))
        .unwrap_or_else(|| "quant unknown".into());
    let memory = rec
        .metadata
        .estimated_memory_gb
        .map(|gb| format!("~{gb:.1} GB"))
        .unwrap_or_else(|| "memory unknown".into());
    format!("{size} · {quant} · {memory}")
}

fn backend_model_hint(backend_name: BackendName, model: &str) -> String {
    match backend_name {
        BackendName::Ollama => format!("install with `ollama pull {model}`"),
        BackendName::LmStudio => {
            "load a matching model in LM Studio, then start the Local Server".into()
        }
        BackendName::Mlx => format!("start MLX with `mlx_lm.server --model {model}`"),
        BackendName::LlamaCpp => {
            "start llama.cpp with `llama-server -m /path/to/model.gguf --host 127.0.0.1 --port 8080 --jinja`".into()
        }
        BackendName::Openrouter => "set OPENROUTER_API_KEY before using OpenRouter".into(),
        BackendName::OpenAi => "set OPENAI_API_KEY before using OpenAI".into(),
        BackendName::Anthropic => "set ANTHROPIC_API_KEY before using Anthropic".into(),
        BackendName::OpenAiCodex => "run /login openai-codex before using ChatGPT/Codex".into(),
        BackendName::Grok => "run /login grok before using SuperGrok / X Premium+".into(),
    }
}

fn print_recommendations(spec: &HardwareSpec, recommendations: &[ModelRecommendation]) {
    println!("  {BOLD}Hardware-aware recommendation{RESET}");
    println!("  {DIM}hardware{RESET} {}", hardware_summary(spec));
    println!(
        "  {DIM}tier{RESET} {} · {}",
        spec.tier().as_str(),
        spec.tier().guidance()
    );
    for (idx, rec) in recommendations.iter().take(3).enumerate() {
        println!();
        println!(
            "  {CYAN}{}){RESET} {BOLD}{}{RESET} {DIM}· {}{RESET}",
            idx + 1,
            rec.model,
            rec.backend.as_str()
        );
        println!(
            "     {DIM}score{RESET} {} · {DIM}confidence{RESET} {} · {DIM}fit{RESET} {} · {DIM}installed{RESET} {}",
            rec.score,
            rec.confidence.as_str(),
            rec.memory_fit.as_str(),
            rec.installed
        );
        println!(
            "     {DIM}size{RESET} {} · {DIM}tools{RESET} {} · {DIM}bench{RESET} {}",
            model_size_label(rec),
            rec.tool_path,
            rec.benchmark_label.as_deref().unwrap_or("not benched")
        );
        let why = rec
            .rationale
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        if !why.is_empty() {
            println!("     {DIM}why{RESET} {why}");
        }
    }
}

async fn cmd_recommend(args: &str, state: &mut AppState) -> Result<()> {
    let parts: Vec<&str> = args.split_whitespace().collect();
    let apply = parts.iter().any(|arg| *arg == "apply" || *arg == "--apply");
    let include_cloud = parts.iter().any(|arg| *arg == "cloud" || *arg == "--cloud");
    let refresh = parts
        .iter()
        .any(|arg| *arg == "refresh" || *arg == "--refresh");
    let all = parts.iter().any(|arg| *arg == "all" || *arg == "--all");

    let spec = detect_hardware_spec();
    let hardware_path = save_hardware_summary(&state.session_dir, &spec)?;
    if refresh || all {
        let cached = refresh_recommendation_capabilities(state, all, include_cloud).await?;
        println!(
            "  {GREEN}✓{RESET} {DIM}refreshed {} capability record(s).{RESET}",
            cached
        );
    }

    let candidates = collect_recommendation_candidates(state, all, include_cloud).await?;
    let recommendations = recommend_models(&spec, candidates, include_cloud);
    if recommendations.is_empty() {
        println!("  {DIM}No model candidates found. Start a local provider, then rerun /doctor recommend refresh.{RESET}");
        return Ok(());
    }

    print_recommendations(&spec, &recommendations);
    println!(
        "  {DIM}hardware summary cached → {}{RESET}",
        hardware_path.display()
    );

    let best = recommendations
        .first()
        .expect("checked recommendations is not empty");
    if !best.installed {
        println!(
            "  {YELLOW}!{RESET} {DIM}top recommendation is not installed: {}{RESET}",
            backend_model_hint(best.backend, &best.model)
        );
    }

    if !apply {
        println!(
            "  {DIM}Run /doctor recommend apply to switch this session to the top recommendation.{RESET}"
        );
        return Ok(());
    }

    let env_backend = backend(best.backend);
    if env_backend.base_url != best.base_url {
        println!(
            "  {YELLOW}!{RESET} {DIM}recommended URL was {}, current env resolves to {}{RESET}",
            best.base_url, env_backend.base_url
        );
    }
    apply_recommendation_to_config(&mut state.config, best);
    state.active_effort = None;
    state.rebuild_client()?;
    state.resolve_model();
    println!(
        "  {GREEN}✓{RESET} {DIM}active session recommendation applied → {} · {}{RESET}",
        state.config.backend.as_str(),
        state.model
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_inline_tool_json_for_doctor_noop() {
        assert!(looks_like_inline_tool_json(
            r#"{"name":"doctor_noop","arguments":{"ok":true}}"#
        ));
        assert!(!looks_like_inline_tool_json(
            r#"{"name":"other_tool","arguments":{"ok":true}}"#
        ));
        assert!(!looks_like_inline_tool_json("plain text"));
    }

    #[test]
    fn llama_cpp_warning_mentions_jinja_when_tool_calls_missing() {
        let row = DoctorCapabilityRow {
            backend: "llamacpp".into(),
            base_url: "http://localhost:8080/v1".into(),
            model: "gpt-3.5-turbo".into(),
            models: ProbeStatus::ok("1 model"),
            streaming: ProbeStatus::ok("stream ok"),
            usage_chunks: ProbeStatus::ok("usage ok"),
            tool_calls: ProbeStatus::fail("missing"),
            inline_tool_json: ProbeStatus::fail("missing"),
            warning: None,
        };
        assert!(doctor_warning(&row).unwrap().contains("--jinja"));
    }
}
