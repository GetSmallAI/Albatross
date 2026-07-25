//! Model-system routing: selector model + role/tier model stack.

use super::*;
use crate::model_system::{
    EffortLevel, ModelRef, ModelSystemConfig, ModelTierSet, ReviewModelSet, ReviewTier,
    RouteDecision, TaskComplexity,
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum RouteApplyTarget {
    Selector,
    Orchestrator(TaskComplexity),
    Coder(TaskComplexity),
    Review(ReviewTier),
    Security,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RouteSelectArgs {
    apply: bool,
    task: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RouteInvocation {
    Status,
    Template,
    Apply(RouteApplyTarget),
    Select(RouteSelectArgs),
}

struct ResolvedRoute<'a> {
    orchestrator: Option<&'a ModelRef>,
    coder: &'a ModelRef,
    coder_effort: Option<EffortLevel>,
    reviewer: Option<(ReviewTier, &'a ModelRef, Option<EffortLevel>)>,
    security: Option<(&'a ModelRef, Option<EffortLevel>)>,
}

pub(super) async fn cmd_route(args: &str, state: &mut AppState) -> Result<()> {
    let Some(invocation) = parse_route_args(args) else {
        route_usage();
        return Ok(());
    };
    match invocation {
        RouteInvocation::Status => {
            print_route_status(&state.config.model_system);
        }
        RouteInvocation::Template => {
            print_route_template();
        }
        RouteInvocation::Apply(target) => {
            apply_route_target(state, target)?;
        }
        RouteInvocation::Select(args) => {
            select_route(state, args).await?;
        }
    }
    Ok(())
}

fn route_usage() {
    println!(
        "  {DIM}Usage: /route status · /route template · /route select [--dry-run] <task> · /route apply coder|orchestrator low|medium|high · /route apply review play|production · /route apply security{RESET}"
    );
}

fn parse_route_args(args: &str) -> Option<RouteInvocation> {
    let trimmed = args.trim();
    if trimmed.is_empty() || trimmed == "status" {
        return Some(RouteInvocation::Status);
    }
    if trimmed == "template" || trimmed == "config" {
        return Some(RouteInvocation::Template);
    }

    if let Some(rest) = trimmed.strip_prefix("apply ") {
        return parse_apply_target(rest).map(RouteInvocation::Apply);
    }
    if trimmed == "apply" {
        return None;
    }

    if let Some(rest) = trimmed
        .strip_prefix("select")
        .or_else(|| trimmed.strip_prefix("choose"))
        .or_else(|| trimmed.strip_prefix("pick"))
    {
        return Some(RouteInvocation::Select(parse_select_args(rest.trim())?));
    }

    Some(RouteInvocation::Select(RouteSelectArgs {
        apply: true,
        task: Some(trimmed.to_string()),
    }))
}

fn parse_apply_target(rest: &str) -> Option<RouteApplyTarget> {
    let mut parts = rest.split_whitespace();
    match parts.next()? {
        "selector" => {
            if parts.next().is_some() {
                None
            } else {
                Some(RouteApplyTarget::Selector)
            }
        }
        "coder" | "coding" => {
            let complexity = TaskComplexity::parse(parts.next()?)?;
            if parts.next().is_some() {
                None
            } else {
                Some(RouteApplyTarget::Coder(complexity))
            }
        }
        "orchestrator" | "plan" | "planner" => {
            let complexity = TaskComplexity::parse(parts.next()?)?;
            if parts.next().is_some() {
                None
            } else {
                Some(RouteApplyTarget::Orchestrator(complexity))
            }
        }
        "review" | "reviewer" => {
            let tier = ReviewTier::parse(parts.next()?)?;
            if parts.next().is_some() {
                None
            } else {
                Some(RouteApplyTarget::Review(tier))
            }
        }
        "security" | "security-review" => {
            if parts.next().is_some() {
                None
            } else {
                Some(RouteApplyTarget::Security)
            }
        }
        _ => None,
    }
}

fn parse_select_args(rest: &str) -> Option<RouteSelectArgs> {
    let mut apply = true;
    let mut task = Vec::new();
    for part in rest.split_whitespace() {
        match part {
            "--dry-run" | "--no-apply" => apply = false,
            "--apply" => apply = true,
            _ => task.push(part),
        }
    }
    Some(RouteSelectArgs {
        apply,
        task: if task.is_empty() {
            None
        } else {
            Some(task.join(" "))
        },
    })
}

fn print_route_status(stack: &ModelSystemConfig) {
    let state = if stack.enabled { "on" } else { "off" };
    println!("  {DIM}modelSystem{RESET}      {CYAN}{state}{RESET}");
    if !stack.any_configured() {
        println!(
            "  {DIM}No model stack configured. Run /route template for the config shape.{RESET}"
        );
        return;
    }
    print_model_ref("planner", stack.planner.as_ref());
    print_model_ref("selector", stack.selector.as_ref());
    print_model_ref("compaction", stack.compaction.as_ref());
    print_tier_set("orchestrator", &stack.orchestrators);
    print_tier_set("coder", &stack.coders);
    print_review_set("review", &stack.reviewers);
    print_model_ref("security", stack.security_reviewer.as_ref());
}

fn print_model_ref(label: &str, model: Option<&ModelRef>) {
    match model {
        Some(model) => println!("  {DIM}{label:<18}{RESET} {}", model.detail()),
        None => println!("  {DIM}{label:<18}{RESET} not configured"),
    }
}

fn print_tier_set(label: &str, set: &ModelTierSet) {
    print_model_ref(&format!("{label}.low"), set.low.as_ref());
    print_model_ref(&format!("{label}.medium"), set.medium.as_ref());
    print_model_ref(&format!("{label}.high"), set.high.as_ref());
}

fn print_review_set(label: &str, set: &ReviewModelSet) {
    print_model_ref(&format!("{label}.play"), set.play.as_ref());
    print_model_ref(&format!("{label}.production"), set.production.as_ref());
}

fn print_route_template() {
    println!(
        r#"  {DIM}Add this to agent.config.json and edit the model ids for your machine/API keys:{RESET}
{{
  "modelSystem": {{
    "enabled": true,
    "planner": {{
      "backend": "openrouter",
      "model": "anthropic/claude-opus-4.8",
      "effort": "high",
      "thinkingDepth": "deep",
      "notes": "Breaks a goal into a routed execution plan."
    }},
    "selector": {{
      "backend": "openrouter",
      "model": "openrouter/fusion",
      "effort": "high",
      "thinkingDepth": "deep",
      "notes": "Chooses the model route for a task."
    }},
    "compaction": {{
      "backend": "openrouter",
      "model": "anthropic/claude-3.5-haiku",
      "notes": "Summarizes the transcript when context is compacted. Omit to inherit the main model."
    }},
    "orchestrators": {{
      "low": {{ "backend": "ollama", "model": "qwen2.5-coder:7b" }},
      "medium": {{ "backend": "openrouter", "model": "qwen/qwen-2.5-coder-32b-instruct" }},
      "high": {{ "backend": "openrouter", "model": "anthropic/claude-sonnet-4.5" }}
    }},
    "coders": {{
      "low": {{ "backend": "ollama", "model": "qwen2.5-coder:7b" }},
      "medium": {{ "backend": "openrouter", "model": "qwen/qwen-2.5-coder-32b-instruct", "effort": "medium" }},
      "high": {{ "backend": "openrouter", "model": "anthropic/claude-sonnet-4.5", "effort": "high" }}
    }},
    "reviewers": {{
      "play": {{ "backend": "ollama", "model": "qwen2.5-coder:7b" }},
      "production": {{ "backend": "openrouter", "model": "openrouter/fusion" }}
    }},
    "securityReviewer": {{ "backend": "openrouter", "model": "openrouter/fusion" }}
  }}
}}"#
    );
}

fn apply_route_target(state: &mut AppState, target: RouteApplyTarget) -> Result<()> {
    let stack = &state.config.model_system;
    let (label, model) = match target {
        RouteApplyTarget::Selector => ("selector".to_string(), stack.selector.as_ref()),
        RouteApplyTarget::Orchestrator(complexity) => (
            format!("orchestrator.{}", complexity.as_str()),
            stack.orchestrator(complexity),
        ),
        RouteApplyTarget::Coder(complexity) => (
            format!("coder.{}", complexity.as_str()),
            stack.coder(complexity),
        ),
        RouteApplyTarget::Review(tier) => {
            (format!("review.{}", tier.as_str()), stack.reviewer(tier))
        }
        RouteApplyTarget::Security => ("security".to_string(), stack.security_reviewer.as_ref()),
    };
    let Some(model) = model.cloned() else {
        println!("  {RED}✗{RESET} {DIM}{label} is not configured in modelSystem.{RESET}");
        return Ok(());
    };
    match apply_model_ref(state, &model, None) {
        Ok(()) => println!(
            "  {GREEN}✓{RESET} {DIM}route applied:{RESET} {label} {DIM}→{RESET} {CYAN}{}{RESET}",
            model.detail_with_effort(None)
        ),
        Err(e) => println!("  {RED}✗{RESET} {DIM}could not apply {label}: {e}{RESET}"),
    }
    Ok(())
}

async fn select_route(state: &mut AppState, args: RouteSelectArgs) -> Result<()> {
    if !state.config.model_system.enabled || !state.config.model_system.any_configured() {
        println!("  {RED}✗{RESET} {DIM}modelSystem is not configured. Run /route template for the config shape.{RESET}");
        return Ok(());
    }
    let Some(selector) = state.config.model_system.selector.clone() else {
        println!("  {RED}✗{RESET} {DIM}modelSystem.selector is required for /route select.{RESET}");
        return Ok(());
    };
    let task = match args.task {
        Some(task) => task,
        None => match state.messages.iter().rev().find_map(|m| m.user_text()) {
            Some(text) => text.to_string(),
            None => {
                println!("  {DIM}No task provided and no prior user message found.{RESET}");
                return Ok(());
            }
        },
    };

    println!("  {DIM}selector{RESET}          {}", selector.detail());
    let decision = match run_selector(state, &selector, &task).await {
        Ok(decision) => decision,
        Err(e) => {
            println!("  {RED}✗{RESET} {DIM}{e}{RESET}");
            return Ok(());
        }
    };
    let route = match resolve_route(&state.config.model_system, &decision) {
        Ok(route) => route,
        Err(e) => {
            println!("  {RED}✗{RESET} {DIM}{e}{RESET}");
            return Ok(());
        }
    };

    println!(
        "  {DIM}complexity{RESET}        {CYAN}{}{RESET}",
        decision.complexity.as_str()
    );
    if let Some(reason) = decision.reason.as_deref().filter(|s| !s.trim().is_empty()) {
        println!("  {DIM}reason{RESET}            {}", reason.trim());
    }
    if let Some(orchestrator) = route.orchestrator {
        println!("  {DIM}orchestrator{RESET}      {}", orchestrator.detail());
    }
    println!(
        "  {DIM}coder{RESET}             {}",
        route.coder.detail_with_effort(route.coder_effort)
    );
    if let Some((tier, reviewer, effort)) = route.reviewer {
        println!(
            "  {DIM}review{RESET}            {} · {}",
            tier.as_str(),
            reviewer.detail_with_effort(effort)
        );
    } else {
        println!("  {DIM}review{RESET}            skipped");
    }
    if let Some((security, effort)) = route.security {
        println!(
            "  {DIM}security{RESET}          {}",
            security.detail_with_effort(effort)
        );
    } else {
        println!("  {DIM}security{RESET}          skipped");
    }

    if args.apply {
        let coder = route.coder.clone();
        match apply_model_ref(state, &coder, route.coder_effort) {
            Ok(()) => println!(
                "  {GREEN}✓{RESET} {DIM}active coding model →{RESET} {CYAN}{}{RESET}{}",
                state.model,
                format_active_effort_suffix(state.active_effort)
            ),
            Err(e) => {
                println!("  {RED}✗{RESET} {DIM}selected coder but could not apply it: {e}{RESET}")
            }
        }
    } else {
        println!(
            "  {DIM}dry run: active model unchanged ({}){RESET}",
            state.model
        );
    }
    Ok(())
}

fn resolve_route<'a>(
    stack: &'a ModelSystemConfig,
    decision: &RouteDecision,
) -> Result<ResolvedRoute<'a>> {
    let coder = stack.coder(decision.complexity).ok_or_else(|| {
        anyhow!(
            "modelSystem.coders.{} is not configured",
            decision.complexity.as_str()
        )
    })?;
    let orchestrator = stack.orchestrator(decision.complexity);
    let coder_effort = decision.coder_effort.or(coder.effort);
    let reviewer = decision.review.and_then(|tier| {
        stack
            .reviewer(tier)
            .map(|model| (tier, model, decision.review_effort.or(model.effort)))
    });
    let security = if decision.security_review {
        stack
            .security_reviewer
            .as_ref()
            .map(|model| (model, decision.security_effort.or(model.effort)))
    } else {
        None
    };
    Ok(ResolvedRoute {
        orchestrator,
        coder,
        coder_effort,
        reviewer,
        security,
    })
}

async fn run_selector(state: &AppState, selector: &ModelRef, task: &str) -> Result<RouteDecision> {
    let backend_desc = state.config.backend_descriptor_for(selector.backend);
    if let Err(e) = validate(&backend_desc) {
        return Err(anyhow!("selector backend is not ready: {e}"));
    }
    let system = selector_system_prompt();
    let user = render_selector_prompt(&state.config.model_system, task);
    let messages = vec![
        ChatMessage::System { content: system },
        ChatMessage::User {
            content: user.into(),
        },
    ];
    let req = ChatRequest {
        model: &selector.model,
        messages: &messages,
        tools: None,
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
        max_tokens: Some(500),
        prompt_cache_key: None,
        effort: selector.effort,
    };
    let mut text = String::new();
    let mut reported_cost = None;
    stream_chat(&state.http, &backend_desc, &req, None, |chunk| {
        if let Some(choice) = chunk.choices.first() {
            if let Some(content) = &choice.delta.content {
                text.push_str(content);
            }
        }
        if let Some(usage) = &chunk.usage {
            if let Some(cost) = usage.cost {
                reported_cost = Some(cost);
            }
        }
    })
    .await?;
    if let Some(cost) = reported_cost {
        println!(
            "  {DIM}selector cost{RESET}    {}",
            catalog::format_usd(cost)
        );
    }
    parse_route_decision(&text)
}

fn selector_system_prompt() -> String {
    "You route coding tasks across a Albatross model system. Return ONLY one JSON object with this exact shape: {\"complexity\":\"low|medium|high\",\"coderEffort\":\"none|minimal|low|medium|high|xhigh|max|null\",\"review\":\"play|production|null\",\"reviewEffort\":\"none|minimal|low|medium|high|xhigh|max|null\",\"securityReview\":true|false,\"securityEffort\":\"none|minimal|low|medium|high|xhigh|max|null\",\"reason\":\"short reason\"}. Choose low complexity for simple edits and small fixes, medium for multi-file feature work, high for ambiguous architecture, long-horizon, reliability-sensitive, or high-risk work. Choose higher coder effort for uncertain implementation, broad refactors, concurrency, migrations, or failing tests. Choose production review for release-quality or production-grade code, play review for prototypes/MVPs/demos, and securityReview=true for auth, secrets, crypto, permissions, dependency, infra, data-safety, or supply-chain risk. Use max or xhigh effort only when deeper reasoning is worth extra latency/cost. Do not include markdown.".into()
}

fn render_selector_prompt(stack: &ModelSystemConfig, task: &str) -> String {
    let mut out = String::new();
    out.push_str("Route this task using only the configured model system.\n\nTask:\n");
    out.push_str(task.trim());
    out.push_str("\n\nConfigured routes:\n");
    append_model_line(&mut out, "planner", stack.planner.as_ref());
    append_model_line(&mut out, "selector", stack.selector.as_ref());
    append_model_line(
        &mut out,
        "orchestrator.low",
        stack.orchestrators.low.as_ref(),
    );
    append_model_line(
        &mut out,
        "orchestrator.medium",
        stack.orchestrators.medium.as_ref(),
    );
    append_model_line(
        &mut out,
        "orchestrator.high",
        stack.orchestrators.high.as_ref(),
    );
    append_model_line(&mut out, "coder.low", stack.coders.low.as_ref());
    append_model_line(&mut out, "coder.medium", stack.coders.medium.as_ref());
    append_model_line(&mut out, "coder.high", stack.coders.high.as_ref());
    append_model_line(&mut out, "review.play", stack.reviewers.play.as_ref());
    append_model_line(
        &mut out,
        "review.production",
        stack.reviewers.production.as_ref(),
    );
    append_model_line(&mut out, "security", stack.security_reviewer.as_ref());
    out
}

fn append_model_line(out: &mut String, label: &str, model: Option<&ModelRef>) {
    out.push_str("- ");
    out.push_str(label);
    out.push_str(": ");
    match model {
        Some(model) => out.push_str(&model.detail()),
        None => out.push_str("not configured"),
    }
    out.push('\n');
}

fn parse_route_decision(text: &str) -> Result<RouteDecision> {
    let value = if let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        value
    } else {
        let Some(json) = extract_first_json_object(text) else {
            return Err(anyhow!("selector did not return a JSON route decision"));
        };
        serde_json::from_str::<serde_json::Value>(json)
            .map_err(|e| anyhow!("selector returned invalid route decision JSON: {e}"))?
    };
    route_decision_from_value(&value)
}

fn route_decision_from_value(value: &serde_json::Value) -> Result<RouteDecision> {
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow!("selector route decision must be a JSON object"))?;
    let complexity = obj
        .get("complexity")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_ascii_lowercase())
        .and_then(|s| TaskComplexity::parse(&s))
        .ok_or_else(|| {
            anyhow!("selector route decision must include complexity low|medium|high")
        })?;
    let review =
        match obj.get("review") {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => {
                let Some(text) = value.as_str() else {
                    return Err(anyhow!("selector review must be play, production, or null"));
                };
                let normalized = text.trim().to_ascii_lowercase();
                match normalized.as_str() {
                    "" | "null" | "none" | "skip" | "skipped" => None,
                    _ => Some(ReviewTier::parse(&normalized).ok_or_else(|| {
                        anyhow!("selector review must be play, production, or null")
                    })?),
                }
            }
        };
    let coder_effort = parse_effort_field(obj, &["coderEffort", "coder_effort"])?;
    let review_effort = parse_effort_field(obj, &["reviewEffort", "review_effort"])?;
    let security_review = obj
        .get("securityReview")
        .or_else(|| obj.get("security_review"))
        .and_then(|value| {
            value
                .as_bool()
                .or_else(|| value.as_str().and_then(parse_boolish))
        })
        .unwrap_or(false);
    let reason = obj
        .get("reason")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let security_effort = parse_effort_field(obj, &["securityEffort", "security_effort"])?;
    Ok(RouteDecision {
        complexity,
        coder_effort,
        review,
        review_effort,
        security_review,
        security_effort,
        reason,
    })
}

fn parse_effort_field(
    obj: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Result<Option<EffortLevel>> {
    for key in keys {
        let Some(value) = obj.get(*key) else {
            continue;
        };
        if value.is_null() {
            return Ok(None);
        }
        let Some(text) = value.as_str() else {
            return Err(anyhow!("{key} must be an effort string or null"));
        };
        let normalized = text.trim().to_ascii_lowercase();
        if normalized.is_empty()
            || matches!(normalized.as_str(), "null" | "none" | "skip" | "skipped")
        {
            return Ok(None);
        }
        return EffortLevel::parse(&normalized)
            .map(Some)
            .ok_or_else(|| anyhow!("{key} must be none|minimal|low|medium|high|xhigh|max"));
    }
    Ok(None)
}

fn parse_boolish(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" => Some(true),
        "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

fn extract_first_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = start + offset + ch.len_utf8();
                    return Some(&text[start..end]);
                }
            }
            _ => {}
        }
    }
    None
}

fn format_active_effort_suffix(effort: Option<EffortLevel>) -> String {
    effort
        .map(|effort| format!(" {DIM}· effort →{RESET} {CYAN}{}{RESET}", effort.as_str()))
        .unwrap_or_default()
}

pub(super) fn apply_model_ref(
    state: &mut AppState,
    model: &ModelRef,
    effort_override: Option<EffortLevel>,
) -> Result<()> {
    let previous_backend = state.config.backend;
    let previous_override = state.config.model_override.clone();
    let previous_backend_desc = state.backend.clone();
    let previous_model = state.model.clone();
    let previous_effort = state.active_effort;

    state.config.backend = model.backend;
    state.config.model_override = Some(model.model.clone());
    state.active_effort = effort_override.or(model.effort);
    match state.rebuild_client() {
        Ok(()) => {
            state.resolve_model();
            state.warmed_fingerprint = None;
            Ok(())
        }
        Err(e) => {
            state.config.backend = previous_backend;
            state.config.model_override = previous_override;
            state.backend = previous_backend_desc;
            state.model = previous_model;
            state.active_effort = previous_effort;
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_route_commands() {
        assert_eq!(parse_route_args(""), Some(RouteInvocation::Status));
        assert_eq!(
            parse_route_args("template"),
            Some(RouteInvocation::Template)
        );
        assert_eq!(
            parse_route_args("apply coder high"),
            Some(RouteInvocation::Apply(RouteApplyTarget::Coder(
                TaskComplexity::High
            )))
        );
        assert_eq!(
            parse_route_args("apply review prod"),
            Some(RouteInvocation::Apply(RouteApplyTarget::Review(
                ReviewTier::Production
            )))
        );
        assert_eq!(
            parse_route_args("select --dry-run add auth"),
            Some(RouteInvocation::Select(RouteSelectArgs {
                apply: false,
                task: Some("add auth".into())
            }))
        );
    }

    #[test]
    fn parses_route_decision_from_wrapped_json() {
        let decision = parse_route_decision(
            "```json\n{\"complexity\":\"high\",\"review\":\"production\",\"securityReview\":true,\"reason\":\"auth\"}\n```",
        )
        .unwrap();
        assert_eq!(decision.complexity, TaskComplexity::High);
        assert_eq!(decision.review, Some(ReviewTier::Production));
        assert!(decision.security_review);
        assert_eq!(decision.coder_effort, None);
    }

    #[test]
    fn parses_route_decision_tolerates_selector_variants() {
        let decision = parse_route_decision(
            r#"{"complexity":"High","coderEffort":"MAX","review":"none","securityReview":"yes","securityEffort":"x-high","reason":"  risky  "}"#,
        )
        .unwrap();
        assert_eq!(decision.complexity, TaskComplexity::High);
        assert_eq!(decision.coder_effort, Some(EffortLevel::Max));
        assert_eq!(decision.review, None);
        assert!(decision.security_review);
        assert_eq!(decision.security_effort, Some(EffortLevel::XHigh));
        assert_eq!(decision.reason.as_deref(), Some("risky"));
    }

    #[test]
    fn resolves_configured_route() {
        let stack = ModelSystemConfig {
            enabled: true,
            coders: ModelTierSet {
                high: ModelRef::parse_spec("openrouter:anthropic/claude-sonnet-4.5"),
                ..Default::default()
            },
            orchestrators: ModelTierSet {
                high: ModelRef::parse_spec("openrouter:openrouter/fusion"),
                ..Default::default()
            },
            reviewers: ReviewModelSet {
                production: ModelRef::parse_spec("openrouter:openrouter/fusion"),
                ..Default::default()
            },
            security_reviewer: ModelRef::parse_spec("openrouter:openrouter/fusion"),
            ..Default::default()
        };
        let decision = RouteDecision {
            complexity: TaskComplexity::High,
            review: Some(ReviewTier::Production),
            coder_effort: Some(EffortLevel::High),
            review_effort: Some(EffortLevel::Max),
            security_review: true,
            security_effort: Some(EffortLevel::Max),
            reason: None,
        };
        let route = resolve_route(&stack, &decision).unwrap();
        assert_eq!(route.coder.model, "anthropic/claude-sonnet-4.5");
        assert_eq!(route.coder_effort, Some(EffortLevel::High));
        assert!(route.orchestrator.is_some());
        assert!(route.reviewer.is_some());
        assert!(route.security.is_some());
    }
}
