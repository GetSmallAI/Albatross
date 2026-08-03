//! Model-system routing: selector model + role/tier model stack.

use super::*;
use crate::model_system::{
    evaluate_coder_candidates, model_supports_effort, EffortLevel, ModelRef, ModelSystemConfig,
    ModelTierSet, ReviewModelSet, ReviewTier, RouteCandidate, RouteDecision, RoutingObjective,
    RoutingPolicy, TaskComplexity,
};
use crate::route_audit::{
    append_event, ledger_path, model_call_event, new_route_id, policy_hash, read_events,
    route_outcome_event, session_id, task_hash, task_preview, ActiveRouteContext, ModelCallInput,
    RouteLedgerEvent, RouteOutcomeInput, RouteOutcomeStatus, RoutedModelReceipt,
    RoutedReviewReceipt,
};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

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
    Guide,
    Status,
    Template,
    History(usize),
    Explain(Option<String>),
    Spend,
    Report,
    WhyNot(Option<String>),
    Label {
        outcome: RouteOutcomeStatus,
        note: Option<String>,
    },
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
        RouteInvocation::Guide => route_guide(state).await?,
        RouteInvocation::Status => {
            print_route_status(&state.config.model_system);
        }
        RouteInvocation::Template => {
            print_route_template();
        }
        RouteInvocation::History(limit) => print_route_history(state, limit)?,
        RouteInvocation::Explain(route_id) => print_route_explanation(state, route_id.as_deref())?,
        RouteInvocation::Spend => print_route_spend(state)?,
        RouteInvocation::Report => print_route_report(state)?,
        RouteInvocation::WhyNot(query) => print_route_candidates(state, query.as_deref())?,
        RouteInvocation::Label { outcome, note } => {
            label_latest_route(state, outcome, note.as_deref())?
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
        "  {DIM}Usage: /route status · /route history [N] · /route explain [id] · /route spend · /route report · /route why-not [model] · /route label pass|fail [note] · /route simulate <task> · /route template · /route select [--dry-run] <task> · /route apply coder|orchestrator low|medium|high · /route apply review play|production · /route apply security{RESET}"
    );
}

async fn route_guide(state: &mut AppState) -> Result<()> {
    println!(
        "  {DIM}Routing picks a provider, model, and effort for a task. Preview is read-only; select also activates the result.{RESET}"
    );
    let options = vec![
        "Preview a route for a task (no switch)".into(),
        "Select and activate a route for a task".into(),
        "Show the configured routing stack".into(),
        "Explain the latest routing decision".into(),
        "Show a starter routing configuration".into(),
    ];
    let Some(choice) = select_from_list("Route".into(), options, 0).await? else {
        println!("  {DIM}Cancelled.{RESET}");
        return Ok(());
    };
    match choice {
        0 | 1 => {
            let task = plain_read_line(format!(
                "  {CYAN}❯{RESET} {DIM}Describe the task to route:{RESET} "
            ))
            .await?;
            let task = task.trim();
            if task.is_empty() {
                println!("  {DIM}Cancelled — no task entered.{RESET}");
                return Ok(());
            }
            select_route(
                state,
                RouteSelectArgs {
                    apply: choice == 1,
                    task: Some(task.to_string()),
                },
            )
            .await?;
        }
        2 => print_route_status(&state.config.model_system),
        3 => print_route_explanation(state, None)?,
        4 => print_route_template(),
        _ => unreachable!("route guide choice is bounded by its options"),
    }
    Ok(())
}

fn parse_route_args(args: &str) -> Option<RouteInvocation> {
    let trimmed = args.trim();
    if trimmed.is_empty() || trimmed == "guide" || trimmed == "help" {
        return Some(RouteInvocation::Guide);
    }
    if trimmed == "status" {
        return Some(RouteInvocation::Status);
    }
    if trimmed == "template" || trimmed == "config" {
        return Some(RouteInvocation::Template);
    }
    if trimmed == "spend" || trimmed == "cost" || trimmed == "costs" {
        return Some(RouteInvocation::Spend);
    }
    if trimmed == "report" || trimmed == "scorecard" {
        return Some(RouteInvocation::Report);
    }
    if trimmed == "why-not" || trimmed == "candidates" {
        return Some(RouteInvocation::WhyNot(None));
    }
    if let Some(rest) = trimmed.strip_prefix("why-not ") {
        let query = rest.trim();
        return (!query.is_empty()).then(|| RouteInvocation::WhyNot(Some(query.to_string())));
    }
    if let Some(rest) = trimmed.strip_prefix("label ") {
        let mut parts = rest.trim().splitn(2, char::is_whitespace);
        let outcome = match parts.next()? {
            "pass" | "passed" | "success" => RouteOutcomeStatus::Pass,
            "fail" | "failed" | "failure" => RouteOutcomeStatus::Fail,
            _ => return None,
        };
        let note = parts
            .next()
            .map(str::trim)
            .filter(|note| !note.is_empty())
            .map(str::to_string);
        return Some(RouteInvocation::Label { outcome, note });
    }
    if let Some(rest) = trimmed.strip_prefix("simulate") {
        let mut args = parse_select_args(rest.trim())?;
        args.apply = false;
        return Some(RouteInvocation::Select(args));
    }
    if trimmed == "history" {
        return Some(RouteInvocation::History(10));
    }
    if let Some(rest) = trimmed.strip_prefix("history ") {
        return rest.trim().parse().ok().map(RouteInvocation::History);
    }
    if trimmed == "explain" {
        return Some(RouteInvocation::Explain(None));
    }
    if let Some(rest) = trimmed.strip_prefix("explain ") {
        let id = rest.trim();
        return (!id.is_empty()).then(|| RouteInvocation::Explain(Some(id.to_string())));
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
    println!(
        "  {DIM}policy{RESET}            {} · unknown-cost={}{}{}{}",
        stack.policy.objective.as_str(),
        stack.policy.unknown_cost.as_str(),
        stack
            .policy
            .max_turn_usd
            .map(|cap| format!(" · max-turn={}", catalog::format_usd(cap)))
            .unwrap_or_default(),
        stack
            .policy
            .min_confidence
            .map(|confidence| format!(" · min-confidence={confidence}%"))
            .unwrap_or_default(),
        if stack.policy.local_only {
            " · local-only"
        } else {
            ""
        }
    );
}

fn print_route_history(state: &AppState, limit: usize) -> Result<()> {
    let events = read_events(&state.config.workspace_root)?;
    let decisions = events
        .iter()
        .filter_map(|event| match event {
            RouteLedgerEvent::RouteDecision {
                timestamp,
                route_id,
                decision,
                coder,
                task_preview,
                applied,
                ..
            } => Some((timestamp, route_id, decision, coder, task_preview, applied)),
            _ => None,
        })
        .rev()
        .take(limit.max(1))
        .collect::<Vec<_>>();
    if decisions.is_empty() {
        println!(
            "  {DIM}No route decisions recorded yet. Ledger: {}{RESET}",
            ledger_path(&state.config.workspace_root).display()
        );
        return Ok(());
    }
    println!(
        "  {DIM}route history{RESET}      latest {}",
        decisions.len()
    );
    for (timestamp, route_id, decision, coder, preview, applied) in decisions {
        let mode = if *applied { "applied" } else { "dry-run" };
        println!(
            "  {DIM}{timestamp}{RESET} {CYAN}{route_id}{RESET} · {} · {} · {mode}",
            decision.complexity.as_str(),
            coder.model.detail_with_effort(coder.effort)
        );
        println!("    {DIM}{}{RESET}", preview);
    }
    Ok(())
}

fn print_route_explanation(state: &AppState, requested_id: Option<&str>) -> Result<()> {
    let events = read_events(&state.config.workspace_root)?;
    let decision = events.iter().rev().find(|event| match event {
        RouteLedgerEvent::RouteDecision { route_id, .. } => {
            requested_id.map(|id| id == route_id).unwrap_or(true)
        }
        _ => false,
    });
    let Some(RouteLedgerEvent::RouteDecision {
        timestamp,
        route_id,
        session_id,
        task_hash,
        task_preview,
        selector,
        policy_hash,
        candidates,
        model_system,
        decision,
        orchestrator,
        coder,
        reviewer,
        security,
        applied,
    }) = decision
    else {
        println!("  {DIM}No matching route decision found.{RESET}");
        return Ok(());
    };
    println!("  {DIM}route{RESET}             {CYAN}{route_id}{RESET}");
    println!("  {DIM}timestamp{RESET}         {timestamp}");
    println!("  {DIM}session{RESET}           {session_id}");
    println!("  {DIM}task{RESET}              {task_preview}");
    println!("  {DIM}task hash{RESET}         {task_hash}");
    println!("  {DIM}selector{RESET}          {}", selector.detail());
    println!("  {DIM}policy hash{RESET}       {policy_hash}");
    println!("  {DIM}candidate snapshot{RESET}");
    if candidates.is_empty() {
        print_route_status(model_system);
    } else {
        print_candidate_scoreboard(candidates, Some(decision.complexity));
    }
    println!(
        "  {DIM}decision{RESET}          {} · {}",
        decision.complexity.as_str(),
        if *applied { "applied" } else { "dry-run" }
    );
    if let Some(reason) = decision.reason.as_deref() {
        println!("  {DIM}reason{RESET}            {reason}");
    }
    if let Some(confidence) = decision.confidence {
        println!("  {DIM}confidence{RESET}        {confidence}%");
    }
    for adjustment in &decision.policy_adjustments {
        println!("  {DIM}policy adjustment{RESET} {adjustment}");
    }
    if let Some(model) = orchestrator {
        println!("  {DIM}orchestrator{RESET}      {}", model.detail());
    }
    println!(
        "  {DIM}coder{RESET}             {}",
        coder.model.detail_with_effort(coder.effort)
    );
    match reviewer.as_ref() {
        Some(review) => println!(
            "  {DIM}review{RESET}            {} · {}",
            review.tier,
            review.model.detail_with_effort(review.effort)
        ),
        None => println!("  {DIM}review{RESET}            skipped"),
    }
    match security.as_ref() {
        Some(security) => println!(
            "  {DIM}security{RESET}          {}",
            security.model.detail_with_effort(security.effort)
        ),
        None => println!("  {DIM}security{RESET}          skipped"),
    }

    let calls = events
        .iter()
        .filter(|event| {
            matches!(event, RouteLedgerEvent::ModelCall { route_id: Some(id), .. } if id == route_id)
        })
        .collect::<Vec<_>>();
    let mut total = 0.0;
    let mut unknown = 0usize;
    println!("  {DIM}model calls{RESET}       {}", calls.len());
    for call in calls {
        if let RouteLedgerEvent::ModelCall {
            role,
            requested_backend,
            requested_model,
            actual_model,
            provider,
            requested_effort,
            effective_effort,
            effort_status,
            input_tokens,
            output_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
            cost_usd,
            cost_source,
            duration_ms,
            status,
            ..
        } = call
        {
            if let Some(cost) = cost_usd {
                total += cost;
            } else {
                unknown += 1;
            }
            println!(
                "    {role}: {}:{} → {} · {} in/{} out/{} cached/{} cache write · {} · {:.1}s · {status}",
                requested_backend.as_str(),
                requested_model,
                actual_model.as_deref().unwrap_or("not reported"),
                input_tokens,
                output_tokens,
                cached_input_tokens,
                cache_creation_input_tokens,
                cost_usd
                    .map(catalog::format_usd)
                    .unwrap_or_else(|| "$?".into()),
                *duration_ms as f64 / 1000.0,
            );
            if requested_effort.is_some() || effective_effort.is_some() {
                println!(
                    "      effort requested={} effective={} ({effort_status}) · provider={} · cost source={cost_source}",
                    requested_effort.map(|e| e.as_str()).unwrap_or("none"),
                    effective_effort.as_deref().unwrap_or("none"),
                    provider.as_deref().unwrap_or("not reported")
                );
            }
        }
    }
    let prefix = if unknown > 0 { "≥" } else { "" };
    println!(
        "  {DIM}total cost{RESET}        {prefix}{}{}",
        catalog::format_usd(total),
        if unknown > 0 {
            format!(" · {unknown} unknown call(s)")
        } else {
            String::new()
        }
    );
    if let Some(RouteLedgerEvent::RouteOutcome {
        outcome,
        source,
        tests_passed,
        ready_to_ship,
        note,
        ..
    }) = events.iter().rev().find(|event| {
        matches!(event, RouteLedgerEvent::RouteOutcome { route_id: id, .. } if id == route_id)
    }) {
        println!(
            "  {DIM}latest outcome{RESET}    {} · {source}",
            outcome.as_str()
        );
        if tests_passed.is_some() || ready_to_ship.is_some() {
            println!(
                "    {DIM}tests={} · ready-to-ship={}{RESET}",
                tests_passed
                    .map(|passed| if passed { "pass" } else { "fail" })
                    .unwrap_or("unknown"),
                ready_to_ship
                    .map(|ready| if ready { "yes" } else { "no" })
                    .unwrap_or("unknown")
            );
        }
        if let Some(note) = note {
            println!("    {DIM}{note}{RESET}");
        }
    } else {
        println!("  {DIM}latest outcome{RESET}    unlabeled");
    }
    println!(
        "  {DIM}ledger{RESET}            {}",
        ledger_path(&state.config.workspace_root).display()
    );
    Ok(())
}

fn print_route_spend(state: &AppState) -> Result<()> {
    let events = read_events(&state.config.workspace_root)?;
    let mut by_role = std::collections::BTreeMap::<String, (f64, usize, usize)>::new();
    let mut by_model = std::collections::BTreeMap::<String, (f64, usize, usize)>::new();
    for event in events {
        if let RouteLedgerEvent::ModelCall {
            role,
            requested_backend,
            requested_model,
            actual_model,
            cost_usd,
            ..
        } = event
        {
            let entry = by_role.entry(role).or_default();
            entry.1 += 1;
            let model_label = actual_model
                .unwrap_or_else(|| format!("{}:{}", requested_backend.as_str(), requested_model));
            let model_entry = by_model.entry(model_label).or_default();
            model_entry.1 += 1;
            match cost_usd {
                Some(cost) => {
                    entry.0 += cost;
                    model_entry.0 += cost;
                }
                None => {
                    entry.2 += 1;
                    model_entry.2 += 1;
                }
            }
        }
    }
    if by_role.is_empty() {
        println!("  {DIM}No model-call receipts recorded yet.{RESET}");
        return Ok(());
    }
    println!("  {DIM}route spend by role{RESET}");
    let mut total = 0.0;
    let mut unknown = 0usize;
    for (role, (cost, calls, missing)) in by_role {
        total += cost;
        unknown += missing;
        println!(
            "  {DIM}{role:<18}{RESET} {:>8} · {calls} call(s){}",
            catalog::format_usd(cost),
            if missing > 0 {
                format!(" · {missing} unknown")
            } else {
                String::new()
            }
        );
    }
    println!("  {DIM}route spend by model{RESET}");
    for (model, (cost, calls, missing)) in by_model {
        println!(
            "  {DIM}{model:<32}{RESET} {:>8} · {calls} call(s){}",
            catalog::format_usd(cost),
            if missing > 0 {
                format!(" · {missing} unknown")
            } else {
                String::new()
            }
        );
    }
    println!(
        "  {DIM}total{RESET}             {}{}",
        if unknown > 0 { "≥" } else { "" },
        catalog::format_usd(total)
    );
    Ok(())
}

#[derive(Debug, Default, PartialEq)]
struct RouteReportSummary {
    decisions: usize,
    applied: usize,
    passed: usize,
    failed: usize,
    unlabeled: usize,
    total_cost_usd: f64,
    unknown_cost_calls: usize,
    confidence_total: u64,
    confidence_count: usize,
    by_complexity: BTreeMap<String, usize>,
    by_model: BTreeMap<String, (usize, f64, usize)>,
}

fn summarize_route_events(events: &[RouteLedgerEvent]) -> RouteReportSummary {
    let mut summary = RouteReportSummary::default();
    let mut route_ids = BTreeSet::new();
    let mut outcomes = BTreeMap::new();
    for event in events {
        match event {
            RouteLedgerEvent::RouteDecision {
                route_id,
                decision,
                applied,
                ..
            } => {
                summary.decisions += 1;
                summary.applied += usize::from(*applied);
                route_ids.insert(route_id.clone());
                *summary
                    .by_complexity
                    .entry(decision.complexity.as_str().into())
                    .or_default() += 1;
                if let Some(confidence) = decision.confidence {
                    summary.confidence_total += u64::from(confidence);
                    summary.confidence_count += 1;
                }
            }
            RouteLedgerEvent::ModelCall {
                route_id: Some(_),
                requested_backend,
                requested_model,
                actual_model,
                cost_usd,
                ..
            } => {
                let label = actual_model.clone().unwrap_or_else(|| {
                    format!("{}:{}", requested_backend.as_str(), requested_model)
                });
                let entry = summary.by_model.entry(label).or_default();
                entry.0 += 1;
                match cost_usd {
                    Some(cost) => {
                        entry.1 += cost;
                        summary.total_cost_usd += cost;
                    }
                    None => {
                        entry.2 += 1;
                        summary.unknown_cost_calls += 1;
                    }
                }
            }
            RouteLedgerEvent::RouteOutcome {
                route_id, outcome, ..
            } => {
                outcomes.insert(route_id.clone(), *outcome);
            }
            _ => {}
        }
    }
    for route_id in route_ids {
        match outcomes.get(&route_id) {
            Some(RouteOutcomeStatus::Pass) => summary.passed += 1,
            Some(RouteOutcomeStatus::Fail) => summary.failed += 1,
            None => summary.unlabeled += 1,
        }
    }
    summary
}

fn print_route_report(state: &AppState) -> Result<()> {
    let events = read_events(&state.config.workspace_root)?;
    let summary = summarize_route_events(&events);
    if summary.decisions == 0 {
        println!("  {DIM}No route decisions recorded yet.{RESET}");
        return Ok(());
    }
    println!("  {DIM}routing report{RESET}");
    println!(
        "  {DIM}decisions{RESET}         {} · {} applied",
        summary.decisions, summary.applied
    );
    println!(
        "  {DIM}outcomes{RESET}          {} pass · {} fail · {} unlabeled",
        summary.passed, summary.failed, summary.unlabeled
    );
    if summary.confidence_count > 0 {
        println!(
            "  {DIM}avg confidence{RESET}    {}%",
            summary.confidence_total / summary.confidence_count as u64
        );
    }
    println!("  {DIM}complexity mix{RESET}");
    for (complexity, count) in summary.by_complexity {
        println!("    {complexity:<8} {count}");
    }
    println!("  {DIM}resolved models{RESET}");
    for (model, (calls, cost, unknown)) in summary.by_model {
        println!(
            "    {model:<32} {calls} call(s) · {}{}",
            if unknown > 0 { "≥" } else { "" },
            catalog::format_usd(cost)
        );
    }
    println!(
        "  {DIM}routed cost{RESET}       {}{}",
        if summary.unknown_cost_calls > 0 {
            "≥"
        } else {
            ""
        },
        catalog::format_usd(summary.total_cost_usd)
    );
    println!(
        "  {DIM}ledger{RESET}            {}",
        ledger_path(&state.config.workspace_root).display()
    );
    Ok(())
}

fn label_latest_route(
    state: &AppState,
    outcome: RouteOutcomeStatus,
    note: Option<&str>,
) -> Result<()> {
    let events = read_events(&state.config.workspace_root)?;
    let Some(route_id) = events.iter().rev().find_map(|event| match event {
        RouteLedgerEvent::RouteDecision { route_id, .. } => Some(route_id.as_str()),
        _ => None,
    }) else {
        println!("  {DIM}No route decision is available to label.{RESET}");
        return Ok(());
    };
    let event = route_outcome_event(RouteOutcomeInput {
        route_id,
        session_id: &session_id(&state.session_path),
        outcome,
        source: "manual",
        tests_passed: None,
        ready_to_ship: None,
        note,
    });
    append_event(&state.config.workspace_root, &event)?;
    println!(
        "  {GREEN}✓{RESET} {DIM}route outcome recorded:{RESET} {CYAN}{}{RESET} · {}",
        route_id,
        outcome.as_str()
    );
    Ok(())
}

fn estimated_route_input_tokens(state: &AppState, task: &str) -> u32 {
    const ROUTING_OVERHEAD_TOKENS: usize = 1_500;
    let transcript_bytes = state
        .messages
        .iter()
        .map(|message| {
            serde_json::to_vec(message)
                .map(|value| value.len())
                .unwrap_or(0)
        })
        .sum::<usize>();
    let tokens = crate::budget::estimate_tokens(transcript_bytes.saturating_add(task.len()))
        .saturating_add(ROUTING_OVERHEAD_TOKENS);
    u32::try_from(tokens).unwrap_or(u32::MAX)
}

fn current_route_task(state: &AppState) -> String {
    state
        .messages
        .iter()
        .rev()
        .find_map(|message| message.user_text())
        .map(|text| text.into_owned())
        .unwrap_or_else(|| "routing preview".into())
}

fn current_candidates(state: &AppState, task: &str) -> Vec<RouteCandidate> {
    evaluate_coder_candidates(
        &state.config.model_system.coders,
        &state.config.model_system.policy,
        estimated_route_input_tokens(state, task),
    )
}

fn print_candidate_scoreboard(candidates: &[RouteCandidate], selected: Option<TaskComplexity>) {
    for candidate in candidates {
        let marker = if selected == Some(candidate.complexity) {
            "→"
        } else {
            " "
        };
        let score = candidate
            .selector_score
            .map(|score| format!(" · score={score}"))
            .unwrap_or_default();
        println!(
            "    {marker} {:<6} {}{score}",
            candidate.complexity.as_str(),
            candidate.detail()
        );
        for reason in &candidate.exclusions {
            println!("        {RED}excluded:{RESET} {reason}");
        }
        for warning in &candidate.warnings {
            println!("        {YELLOW}warning:{RESET} {warning}");
        }
    }
}

fn print_route_candidates(state: &AppState, query: Option<&str>) -> Result<()> {
    let task = current_route_task(state);
    let candidates = current_candidates(state, &task);
    let filtered = candidates
        .iter()
        .filter(|candidate| {
            query.is_none_or(|query| {
                let query = query.to_ascii_lowercase();
                candidate
                    .model
                    .label()
                    .to_ascii_lowercase()
                    .contains(&query)
                    || candidate.complexity.as_str().contains(&query)
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    if filtered.is_empty() {
        println!("  {DIM}No configured coder candidate matches that query.{RESET}");
        return Ok(());
    }
    println!(
        "  {DIM}candidate policy{RESET}  {} · hash={}",
        state.config.model_system.policy.objective.as_str(),
        policy_hash(&state.config.model_system.policy)
    );
    print_candidate_scoreboard(&filtered, None);
    Ok(())
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
    "policy": {{
      "objective": "balanced",
      "maxTurnUsd": null,
      "unknownCost": "warn",
      "localOnly": false,
      "minConfidence": 70,
      "requireEffortSupport": false,
      "estimatedOutputTokens": 2000
    }},
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
    if let Err(error) = validate_routing_policy(&state.config.model_system.policy) {
        println!("  {RED}✗{RESET} {DIM}{error}{RESET}");
        return Ok(());
    }
    if let Err(error) = validate_selector_policy(&selector, &state.config.model_system.policy) {
        println!("  {RED}✗{RESET} {DIM}{error}{RESET}");
        return Ok(());
    }
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

    let route_id = new_route_id();
    let mut candidates = current_candidates(state, &task);
    if !candidates.iter().any(|candidate| candidate.eligible) {
        println!(
            "  {RED}✗{RESET} {DIM}routing policy excluded every configured coder. Run /route why-not for details.{RESET}"
        );
        print_candidate_scoreboard(&candidates, None);
        return Ok(());
    }
    println!("  {DIM}route id{RESET}          {CYAN}{route_id}{RESET}");
    println!("  {DIM}selector{RESET}          {}", selector.detail());
    println!(
        "  {DIM}policy{RESET}            {} · {}",
        state.config.model_system.policy.objective.as_str(),
        policy_hash(&state.config.model_system.policy)
    );
    let mut decision = match run_selector(state, &selector, &task, &route_id, &candidates).await {
        Ok(decision) => decision,
        Err(e) => {
            println!("  {RED}✗{RESET} {DIM}{e}{RESET}");
            return Ok(());
        }
    };
    for candidate in &mut candidates {
        candidate.selector_score = decision
            .candidate_scores
            .get(candidate.complexity.as_str())
            .copied();
    }
    if let Err(error) = enforce_routing_policy(
        &mut decision,
        &mut candidates,
        &state.config.model_system.policy,
    ) {
        println!("  {RED}✗{RESET} {DIM}{error}{RESET}");
        print_candidate_scoreboard(&candidates, Some(decision.complexity));
        return Ok(());
    }
    let route = match resolve_route(&state.config.model_system, &decision) {
        Ok(route) => route,
        Err(e) => {
            println!("  {RED}✗{RESET} {DIM}{e}{RESET}");
            return Ok(());
        }
    };

    let selected_orchestrator = route.orchestrator.cloned();
    let selected_coder = RoutedModelReceipt {
        model: route.coder.clone(),
        effort: route.coder_effort,
    };
    let selected_reviewer = route
        .reviewer
        .map(|(tier, model, effort)| RoutedReviewReceipt {
            tier: tier.as_str().to_string(),
            model: model.clone(),
            effort,
        });
    let selected_security = route.security.map(|(model, effort)| RoutedModelReceipt {
        model: model.clone(),
        effort,
    });

    println!(
        "  {DIM}complexity{RESET}        {CYAN}{}{RESET}",
        decision.complexity.as_str()
    );
    if let Some(confidence) = decision.confidence {
        println!("  {DIM}confidence{RESET}        {confidence}%");
    }
    if let Some(reason) = decision.reason.as_deref().filter(|s| !s.trim().is_empty()) {
        println!("  {DIM}reason{RESET}            {}", reason.trim());
    }
    for adjustment in &decision.policy_adjustments {
        println!("  {DIM}policy adjustment{RESET} {adjustment}");
    }
    println!("  {DIM}candidates{RESET}");
    print_candidate_scoreboard(&candidates, Some(decision.complexity));
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

    let mut applied = false;
    if args.apply {
        let coder = selected_coder.model.clone();
        match apply_model_ref(state, &coder, selected_coder.effort) {
            Ok(()) => {
                applied = true;
                println!(
                    "  {GREEN}✓{RESET} {DIM}active coding model →{RESET} {CYAN}{}{RESET}{}",
                    state.model,
                    format_active_effort_suffix(state.active_effort)
                );
                state.active_route = Some(ActiveRouteContext {
                    route_id: route_id.clone(),
                    role: "coder".into(),
                    backend: coder.backend,
                    model: coder.model,
                });
            }
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
    let decision_receipt = RouteLedgerEvent::RouteDecision {
        timestamp: chrono::Utc::now().to_rfc3339(),
        route_id,
        session_id: session_id(&state.session_path),
        task_hash: task_hash(&task),
        task_preview: task_preview(&task),
        selector,
        policy_hash: policy_hash(&state.config.model_system.policy),
        candidates,
        model_system: Box::new(state.config.model_system.clone()),
        decision,
        orchestrator: selected_orchestrator,
        coder: Box::new(selected_coder),
        reviewer: Box::new(selected_reviewer),
        security: Box::new(selected_security),
        applied,
    };
    if let Err(error) = append_event(&state.config.workspace_root, &decision_receipt) {
        println!("  {YELLOW}!{RESET} {DIM}route decision not recorded: {error}{RESET}");
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

fn complexity_rank(complexity: TaskComplexity) -> u8 {
    match complexity {
        TaskComplexity::Low => 0,
        TaskComplexity::Medium => 1,
        TaskComplexity::High => 2,
    }
}

fn validate_routing_policy(policy: &RoutingPolicy) -> Result<()> {
    if policy.min_confidence.is_some_and(|minimum| minimum > 100) {
        return Err(anyhow!(
            "routing policy minConfidence must be between 0 and 100"
        ));
    }
    if policy
        .max_turn_usd
        .is_some_and(|cap| !cap.is_finite() || cap < 0.0)
    {
        return Err(anyhow!(
            "routing policy maxTurnUsd must be a finite, non-negative number"
        ));
    }
    Ok(())
}

fn validate_selector_policy(selector: &ModelRef, policy: &RoutingPolicy) -> Result<()> {
    if policy.local_only && !selector.backend.is_local() {
        return Err(anyhow!(
            "routing policy localOnly requires a local selector; {} is hosted",
            selector.label()
        ));
    }
    if policy.require_effort_support
        && selector
            .effort
            .is_some_and(|effort| effort != EffortLevel::None)
        && !model_supports_effort(selector.backend, &selector.model)
    {
        return Err(anyhow!(
            "routing policy requires effort support, but selector {} cannot apply effort",
            selector.label()
        ));
    }
    Ok(())
}

fn policy_fallback(
    candidates: &[RouteCandidate],
    objective: RoutingObjective,
) -> Option<&RouteCandidate> {
    let eligible = candidates
        .iter()
        .filter(|candidate| candidate.eligible)
        .collect::<Vec<_>>();
    match objective {
        RoutingObjective::Quality => eligible
            .into_iter()
            .max_by_key(|candidate| complexity_rank(candidate.complexity)),
        RoutingObjective::Cost => eligible.into_iter().min_by(|left, right| {
            match (left.estimated_cost_usd, right.estimated_cost_usd) {
                (Some(left_cost), Some(right_cost)) => left_cost
                    .partial_cmp(&right_cost)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| {
                        complexity_rank(left.complexity).cmp(&complexity_rank(right.complexity))
                    }),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => {
                    complexity_rank(left.complexity).cmp(&complexity_rank(right.complexity))
                }
            }
        }),
        RoutingObjective::Balanced => [
            TaskComplexity::Medium,
            TaskComplexity::High,
            TaskComplexity::Low,
        ]
        .into_iter()
        .find_map(|complexity| {
            eligible
                .iter()
                .copied()
                .find(|candidate| candidate.complexity == complexity)
        }),
    }
}

fn enforce_routing_policy(
    decision: &mut RouteDecision,
    candidates: &mut [RouteCandidate],
    policy: &RoutingPolicy,
) -> Result<()> {
    validate_routing_policy(policy)?;
    if let Some(confidence) = decision.confidence {
        if confidence > 100 {
            return Err(anyhow!("selector confidence must be between 0 and 100"));
        }
    }

    let requested_effort = decision
        .coder_effort
        .filter(|effort| *effort != EffortLevel::None);
    if policy.require_effort_support && requested_effort.is_some() {
        for candidate in candidates.iter_mut() {
            if !model_supports_effort(candidate.model.backend, &candidate.model.model) {
                let reason = format!(
                    "{} cannot apply selector-requested effort {}",
                    candidate.model.backend.as_str(),
                    requested_effort
                        .map(|effort| effort.as_str())
                        .unwrap_or("none")
                );
                if !candidate.exclusions.contains(&reason) {
                    candidate.exclusions.push(reason);
                }
                candidate.eligible = false;
            }
        }
    }

    let original = decision.complexity;
    let selected_eligible = candidates
        .iter()
        .find(|candidate| candidate.complexity == original)
        .is_some_and(|candidate| candidate.eligible);
    let below_confidence = policy
        .min_confidence
        .is_some_and(|minimum| decision.confidence.unwrap_or_default() < minimum);
    if selected_eligible && !below_confidence {
        return Ok(());
    }

    let fallback = policy_fallback(candidates, policy.objective)
        .ok_or_else(|| anyhow!("routing policy excluded every candidate after selector output"))?;
    let cause = if !selected_eligible {
        "selected candidate was excluded by policy".to_string()
    } else {
        format!(
            "selector confidence {}% was below the {}% policy minimum",
            decision.confidence.unwrap_or_default(),
            policy.min_confidence.unwrap_or_default()
        )
    };
    decision.policy_adjustments.push(format!(
        "{cause}; {} → {} ({})",
        original.as_str(),
        fallback.complexity.as_str(),
        policy.objective.as_str()
    ));
    decision.complexity = fallback.complexity;
    Ok(())
}

async fn run_selector(
    state: &mut AppState,
    selector: &ModelRef,
    task: &str,
    route_id: &str,
    candidates: &[RouteCandidate],
) -> Result<RouteDecision> {
    let backend_desc = state.config.backend_descriptor_for(selector.backend);
    if let Err(e) = validate(&backend_desc) {
        return Err(anyhow!("selector provider is not ready: {e}"));
    }
    let system = selector_system_prompt();
    let user = render_selector_prompt(&state.config.model_system, task, candidates);
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
    let mut input_tokens = 0;
    let mut output_tokens = 0;
    let mut cached_input_tokens = 0;
    let mut cache_creation_input_tokens = 0;
    let mut actual_model = None;
    let mut provider = None;
    let started = Instant::now();
    let result = stream_chat(&state.http, &backend_desc, &req, None, |chunk| {
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
            if let Some(content) = &choice.delta.content {
                text.push_str(content);
            }
        }
        if let Some(usage) = &chunk.usage {
            input_tokens += usage.prompt_tokens;
            output_tokens += usage.completion_tokens;
            cached_input_tokens += usage.cached_tokens();
            cache_creation_input_tokens += usage.cache_creation_tokens();
            if let Some(cost) = usage.cost {
                reported_cost = Some(cost);
            }
        }
    })
    .await;
    let catalog_cost = catalog::turn_cost_with_cache_usd(
        selector.backend,
        actual_model.as_deref().unwrap_or(&selector.model),
        input_tokens,
        cached_input_tokens,
        cache_creation_input_tokens,
        output_tokens,
    );
    let (cost, cost_source) = if let Some(cost) = reported_cost {
        (Some(cost), "provider-reported")
    } else if let Some(cost) = catalog_cost {
        (Some(cost), "catalog-estimate")
    } else if selector.backend.is_local() {
        (Some(0.0), "local")
    } else {
        (None, "unknown")
    };
    let receipt = model_call_event(ModelCallInput {
        route_id: Some(route_id),
        session_id: &session_id(&state.session_path),
        role: "selector",
        backend: selector.backend,
        requested_model: &selector.model,
        actual_model: actual_model.as_deref(),
        provider: provider.as_deref(),
        requested_effort: selector.effort,
        input_tokens,
        output_tokens,
        cached_input_tokens,
        cache_creation_input_tokens,
        cost_usd: cost,
        cost_source,
        duration_ms: started.elapsed().as_millis() as u64,
        status: if result.is_ok() { "ok" } else { "error" },
    });
    if let Err(error) = append_event(&state.config.workspace_root, &receipt) {
        println!("  {YELLOW}!{RESET} {DIM}selector receipt not recorded: {error}{RESET}");
    }
    match cost {
        Some(cost) => state.session_usd += cost,
        None if !selector.backend.is_local() && (input_tokens > 0 || output_tokens > 0) => {
            state.session_cost_has_unknown = true;
        }
        None => {}
    }
    result?;
    if let Some(cost) = cost {
        println!(
            "  {DIM}selector cost{RESET}    {}",
            catalog::format_usd(cost)
        );
    }
    parse_route_decision(&text)
}

fn selector_system_prompt() -> String {
    "You route coding tasks across an Albatross model system. Return ONLY one JSON object with this exact shape: {\"complexity\":\"low|medium|high\",\"coderEffort\":\"none|minimal|low|medium|high|xhigh|max|null\",\"review\":\"play|production|null\",\"reviewEffort\":\"none|minimal|low|medium|high|xhigh|max|null\",\"securityReview\":true|false,\"securityEffort\":\"none|minimal|low|medium|high|xhigh|max|null\",\"confidence\":0-100,\"candidateScores\":{\"low\":0-100,\"medium\":0-100,\"high\":0-100},\"reason\":\"short reason\"}. Never choose a candidate marked excluded. Score each configured candidate for this task even when excluded. Choose low complexity for simple edits and small fixes, medium for multi-file feature work, high for ambiguous architecture, long-horizon, reliability-sensitive, or high-risk work. Choose higher coder effort for uncertain implementation, broad refactors, concurrency, migrations, or failing tests. Choose production review for release-quality or production-grade code, play review for prototypes/MVPs/demos, and securityReview=true for auth, secrets, crypto, permissions, dependency, infra, data-safety, or supply-chain risk. Use max or xhigh effort only when deeper reasoning is worth extra latency/cost. Do not include markdown.".into()
}

fn render_selector_prompt(
    stack: &ModelSystemConfig,
    task: &str,
    candidates: &[RouteCandidate],
) -> String {
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
    out.push_str("\nPolicy-evaluated coder candidates:\n");
    for candidate in candidates {
        let cost = candidate
            .estimated_cost_usd
            .map(catalog::format_usd)
            .unwrap_or_else(|| "$?".into());
        let state = if candidate.eligible {
            "eligible"
        } else {
            "excluded"
        };
        out.push_str(&format!(
            "- {}: {} · {state} · estimated cost {cost}",
            candidate.complexity.as_str(),
            candidate.model.detail_with_effort(None)
        ));
        if !candidate.exclusions.is_empty() {
            out.push_str(&format!(
                " · exclusions: {}",
                candidate.exclusions.join("; ")
            ));
        }
        if !candidate.warnings.is_empty() {
            out.push_str(&format!(" · warnings: {}", candidate.warnings.join("; ")));
        }
        out.push('\n');
    }
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
    let confidence = obj
        .get("confidence")
        .filter(|value| !value.is_null())
        .map(|value| parse_score_value(value, "confidence"))
        .transpose()?;
    let mut candidate_scores = BTreeMap::new();
    if let Some(value) = obj
        .get("candidateScores")
        .or_else(|| obj.get("candidate_scores"))
        .filter(|value| !value.is_null())
    {
        let scores = value
            .as_object()
            .ok_or_else(|| anyhow!("selector candidateScores must be a JSON object"))?;
        for (tier, value) in scores {
            let normalized = tier.trim().to_ascii_lowercase();
            if TaskComplexity::parse(&normalized).is_none() {
                return Err(anyhow!(
                    "selector candidateScores keys must be low, medium, or high"
                ));
            }
            candidate_scores.insert(normalized, parse_score_value(value, "candidate score")?);
        }
    }
    Ok(RouteDecision {
        complexity,
        coder_effort,
        review,
        review_effort,
        security_review,
        security_effort,
        reason,
        confidence,
        candidate_scores,
        policy_adjustments: Vec::new(),
    })
}

fn parse_score_value(value: &serde_json::Value, label: &str) -> Result<u8> {
    if let Some(value) = value.as_u64() {
        return u8::try_from(value)
            .ok()
            .filter(|value| *value <= 100)
            .ok_or_else(|| anyhow!("selector {label} must be between 0 and 100"));
    }
    if let Some(value) = value.as_f64() {
        let percentage = if (0.0..=1.0).contains(&value) {
            (value * 100.0).round()
        } else {
            value.round()
        };
        if (0.0..=100.0).contains(&percentage) {
            return Ok(percentage as u8);
        }
    }
    Err(anyhow!(
        "selector {label} must be a number between 0 and 100"
    ))
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
    let previous_active_route = state.active_route.clone();

    state.config.backend = model.backend;
    state.config.model_override = Some(model.model.clone());
    state.active_effort = effort_override.or(model.effort);
    state.active_route = None;
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
            state.active_route = previous_active_route;
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_route_commands() {
        assert_eq!(parse_route_args(""), Some(RouteInvocation::Guide));
        assert_eq!(parse_route_args("guide"), Some(RouteInvocation::Guide));
        assert_eq!(parse_route_args("status"), Some(RouteInvocation::Status));
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
        assert_eq!(
            parse_route_args("history"),
            Some(RouteInvocation::History(10))
        );
        assert_eq!(
            parse_route_args("history 25"),
            Some(RouteInvocation::History(25))
        );
        assert_eq!(parse_route_args("spend"), Some(RouteInvocation::Spend));
        assert_eq!(parse_route_args("report"), Some(RouteInvocation::Report));
        assert_eq!(
            parse_route_args("why-not gpt-4o"),
            Some(RouteInvocation::WhyNot(Some("gpt-4o".into())))
        );
        assert_eq!(
            parse_route_args("simulate risky refactor"),
            Some(RouteInvocation::Select(RouteSelectArgs {
                apply: false,
                task: Some("risky refactor".into())
            }))
        );
        assert_eq!(
            parse_route_args("label fail regression in auth"),
            Some(RouteInvocation::Label {
                outcome: RouteOutcomeStatus::Fail,
                note: Some("regression in auth".into())
            })
        );
        assert_eq!(
            parse_route_args("explain route-123"),
            Some(RouteInvocation::Explain(Some("route-123".into())))
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
            r#"{"complexity":"High","coderEffort":"MAX","review":"none","securityReview":"yes","securityEffort":"x-high","reason":"  risky  ","confidence":0.82,"candidateScores":{"low":42,"medium":0.71,"high":91}}"#,
        )
        .unwrap();
        assert_eq!(decision.complexity, TaskComplexity::High);
        assert_eq!(decision.coder_effort, Some(EffortLevel::Max));
        assert_eq!(decision.review, None);
        assert!(decision.security_review);
        assert_eq!(decision.security_effort, Some(EffortLevel::XHigh));
        assert_eq!(decision.reason.as_deref(), Some("risky"));
        assert_eq!(decision.confidence, Some(82));
        assert_eq!(decision.candidate_scores.get("medium"), Some(&71));
        assert_eq!(decision.candidate_scores.get("high"), Some(&91));
    }

    fn test_candidate(
        complexity: TaskComplexity,
        cost: Option<f64>,
        eligible: bool,
    ) -> RouteCandidate {
        RouteCandidate {
            complexity,
            model: ModelRef::parse_spec(match complexity {
                TaskComplexity::Low => "ollama:low",
                TaskComplexity::Medium => "openai:gpt-4o-mini",
                TaskComplexity::High => "openai:gpt-4o",
            })
            .unwrap(),
            eligible,
            estimated_input_tokens: 1_000,
            estimated_output_tokens: 2_000,
            estimated_cost_usd: cost,
            selector_score: None,
            exclusions: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn test_decision(complexity: TaskComplexity, confidence: Option<u8>) -> RouteDecision {
        RouteDecision {
            complexity,
            coder_effort: None,
            review: None,
            review_effort: None,
            security_review: false,
            security_effort: None,
            reason: None,
            confidence,
            candidate_scores: BTreeMap::new(),
            policy_adjustments: Vec::new(),
        }
    }

    #[test]
    fn policy_replaces_an_excluded_selection_by_objective() {
        let mut candidates = vec![
            test_candidate(TaskComplexity::Low, Some(0.0), false),
            test_candidate(TaskComplexity::Medium, Some(0.01), true),
            test_candidate(TaskComplexity::High, Some(0.05), true),
        ];
        let mut decision = test_decision(TaskComplexity::Low, Some(95));
        let policy = RoutingPolicy {
            objective: RoutingObjective::Quality,
            ..Default::default()
        };
        enforce_routing_policy(&mut decision, &mut candidates, &policy).unwrap();
        assert_eq!(decision.complexity, TaskComplexity::High);
        assert!(decision.policy_adjustments[0].contains("excluded"));
    }

    #[test]
    fn policy_replaces_low_confidence_selection() {
        let mut candidates = vec![
            test_candidate(TaskComplexity::Low, Some(0.0), true),
            test_candidate(TaskComplexity::Medium, Some(0.01), true),
            test_candidate(TaskComplexity::High, Some(0.05), true),
        ];
        let mut decision = test_decision(TaskComplexity::High, Some(55));
        let policy = RoutingPolicy {
            min_confidence: Some(70),
            ..Default::default()
        };
        enforce_routing_policy(&mut decision, &mut candidates, &policy).unwrap();
        assert_eq!(decision.complexity, TaskComplexity::Medium);
        assert!(decision.policy_adjustments[0].contains("55%"));
    }

    #[test]
    fn local_only_policy_rejects_a_hosted_selector() {
        let selector = ModelRef::parse_spec("openrouter:openrouter/fusion").unwrap();
        let policy = RoutingPolicy {
            local_only: true,
            ..Default::default()
        };
        let error = validate_selector_policy(&selector, &policy).unwrap_err();
        assert!(error.to_string().contains("requires a local selector"));
    }

    #[test]
    fn invalid_policy_is_rejected_before_selection() {
        let policy = RoutingPolicy {
            min_confidence: Some(101),
            ..Default::default()
        };
        assert!(validate_routing_policy(&policy).is_err());
        let policy = RoutingPolicy {
            max_turn_usd: Some(-0.01),
            ..Default::default()
        };
        assert!(validate_routing_policy(&policy).is_err());
    }

    #[test]
    fn report_uses_latest_outcome_and_routed_costs() {
        let decision: RouteLedgerEvent = serde_json::from_value(serde_json::json!({
            "kind": "routeDecision",
            "timestamp": "2026-01-01T00:00:00Z",
            "route_id": "route-1",
            "session_id": "session-1",
            "task_hash": "sha256:test",
            "task_preview": "test",
            "selector": { "backend": "openrouter", "model": "openrouter/auto" },
            "decision": { "complexity": "high", "confidence": 80 },
            "coder": {
                "model": { "backend": "open-ai", "model": "gpt-4o" }
            },
            "applied": true
        }))
        .unwrap();
        let call = model_call_event(ModelCallInput {
            route_id: Some("route-1"),
            session_id: "session-1",
            role: "coder",
            backend: BackendName::OpenAi,
            requested_model: "gpt-4o",
            actual_model: Some("gpt-4o-2026-01-01"),
            provider: Some("openai"),
            requested_effort: Some(EffortLevel::High),
            input_tokens: 100,
            output_tokens: 20,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cost_usd: Some(0.02),
            cost_source: "provider-reported",
            duration_ms: 123,
            status: "ok",
        });
        let failed = route_outcome_event(RouteOutcomeInput {
            route_id: "route-1",
            session_id: "session-1",
            outcome: RouteOutcomeStatus::Fail,
            source: "auto-test",
            tests_passed: Some(false),
            ready_to_ship: Some(false),
            note: None,
        });
        let passed = route_outcome_event(RouteOutcomeInput {
            route_id: "route-1",
            session_id: "session-1",
            outcome: RouteOutcomeStatus::Pass,
            source: "manual",
            tests_passed: None,
            ready_to_ship: None,
            note: Some("fixed"),
        });
        let summary = summarize_route_events(&[decision, call, failed, passed]);
        assert_eq!(summary.decisions, 1);
        assert_eq!(summary.applied, 1);
        assert_eq!(summary.passed, 1);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.unlabeled, 0);
        assert_eq!(summary.confidence_total, 80);
        assert_eq!(summary.total_cost_usd, 0.02);
        assert_eq!(summary.by_model["gpt-4o-2026-01-01"].0, 1);
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
            confidence: None,
            candidate_scores: BTreeMap::new(),
            policy_adjustments: Vec::new(),
        };
        let route = resolve_route(&stack, &decision).unwrap();
        assert_eq!(route.coder.model, "anthropic/claude-sonnet-4.5");
        assert_eq!(route.coder_effort, Some(EffortLevel::High));
        assert!(route.orchestrator.is_some());
        assert!(route.reviewer.is_some());
        assert!(route.security.is_some());
    }
}
