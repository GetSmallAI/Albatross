//! The `/auto` autonomous overnight run.
//!
//! `/auto` is the capstone of the plan→iterate→evaluate→reset arc: a single
//! command that runs unattended (overnight) and self-drives the loop those
//! pieces were built for. Structurally it wraps [`crate::iterate_loop`]'s
//! generate→evaluate round in an outer loop that adds three things:
//!
//! - **Auto-reset.** When the live context fills past a ratio, it drives the
//!   shared [`crate::commands::perform_reset`] recipe — drafting a continuation
//!   handoff and starting a fresh session — so a multi-hour run never blows its
//!   context budget. The goal and the latest evaluator feedback are loop-local,
//!   so they survive the reset (which only clears `state.messages`).
//! - **Guardrails.** A max-round ceiling, an optional dollar budget on generator
//!   spend, and an optional wall-clock deadline. There is always a finite bound.
//! - **A done-check.** When a `spec.md` (from `/plan`) supplies Done Criteria,
//!   a round that already clears the rubric threshold checks those criteria
//!   against the working-tree diff — folding in a lightweight spec-validator —
//!   so "done" means more than a single score. Failed-eval rounds skip the
//!   extra LLM call (done-check cannot unblock stop until `passed` is true).
//!   A run that never clears the threshold does one final check after the loop
//!   so the report still shows which criteria the work actually met.
//!
//! Whatever the outcome, it leaves a morning report at
//! `.albatross/auto-report.md`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chrono::Utc;

use crate::app_state::AppState;
use crate::handoff::should_refuse_cloud_handoff;
use crate::iterate_loop::{
    apply_iterate_mode, build_iterate_prompt, collect_diff_context, render_feedback,
    restore_iterate_mode,
};
use crate::openai::{stream_chat, ChatMessage, ChatRequest, StreamOptions};
use crate::planner::default_spec_path;
use crate::session_turn::{run_user_turn, TurnOptions};
use crate::tools::run_evaluation;

const RESET: crate::theme::Style = crate::theme::RESET;
const DIM: crate::theme::Style = crate::theme::MUTED;
const GREEN: crate::theme::Style = crate::theme::SUCCESS;
const RED: crate::theme::Style = crate::theme::ERROR;
const YELLOW: crate::theme::Style = crate::theme::WARN;
const CYAN: crate::theme::Style = crate::theme::ACCENT;

/// Hard upper bound on rounds regardless of config/flags — a runaway guard for
/// an unattended loop. Higher than `/iterate`'s ceiling because overnight runs
/// legitimately span many rounds with resets in between.
const MAX_ROUNDS_CEILING: usize = 40;

/// Consecutive no-progress rounds (no score gain AND no diff change) before the
/// loop gives up rather than burn budget spinning in place.
const STALL_LIMIT: usize = 3;

const SCORE_EPS: f32 = 0.05;

pub struct AutoOptions {
    /// The working goal. Empty is allowed only when `use_spec` is set and the
    /// spec supplies a Goal.
    pub goal: String,
    /// Read the goal and Done Criteria from `.albatross/spec.md`.
    pub use_spec: bool,
    /// Round ceiling for this run (clamped to [`MAX_ROUNDS_CEILING`]).
    pub max_rounds: usize,
    /// Per-round rubric pass bar on the 0–10 scale.
    pub threshold: f32,
    /// Auto-approve mutations for the whole run (overnight runs are unattended).
    pub yolo: bool,
    /// Permit sending workspace context to a cloud evaluator/drafter.
    pub allow_cloud: bool,
    /// Optional dollar cap on generator spend.
    pub budget_usd: Option<f64>,
    /// Optional wall-clock deadline.
    pub deadline: Option<Duration>,
    /// Context-fill ratio at which an auto-reset fires (clamped 0.50..=0.95).
    pub reset_ratio: f64,
}

/// Why the loop stopped — drives the report verdict and the exit summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    GoalMet,
    MaxRounds,
    BudgetExhausted,
    DeadlineReached,
    Stalled,
    TurnError,
    Interrupted,
}

impl StopReason {
    fn label(self) -> &'static str {
        match self {
            StopReason::GoalMet => "Goal met",
            StopReason::MaxRounds => "Max rounds reached",
            StopReason::BudgetExhausted => "Budget exhausted",
            StopReason::DeadlineReached => "Deadline reached",
            StopReason::Stalled => "Stalled (no progress)",
            StopReason::TurnError => "Stopped on a turn error",
            StopReason::Interrupted => "Interrupted",
        }
    }

    fn succeeded(self) -> bool {
        self == StopReason::GoalMet
    }
}

/// One round's outcome, recorded for the morning report.
struct RoundRecord {
    round: usize,
    score: f32,
    passed: bool,
    verdict: String,
    reset_after: bool,
}

/// Parsed Done Criteria checked against the current diff. `met[i]` corresponds
/// to `criteria[i]`.
pub(crate) struct DoneCheck {
    pub(crate) met: Vec<bool>,
}

impl DoneCheck {
    pub(crate) fn met_count(&self) -> usize {
        self.met.iter().filter(|m| **m).count()
    }
    pub(crate) fn total(&self) -> usize {
        self.met.len()
    }
    fn all_met(&self) -> bool {
        !self.met.is_empty() && self.met.iter().all(|m| *m)
    }
}

/// Accumulated run state owned by [`run_auto_loop`] and rendered into the
/// report on every exit path.
struct AutoRun {
    rounds: Vec<RoundRecord>,
    resets: usize,
    started: Instant,
    elapsed: Duration,
    spent_usd: f64,
    budget_usd: Option<f64>,
    threshold: f32,
    stop_reason: StopReason,
    last_done_check: Option<DoneCheck>,
}

/// `<workspace>/.albatross/auto-report.md`.
pub fn default_auto_report_path(workspace_root: &str) -> PathBuf {
    Path::new(workspace_root)
        .join(".albatross")
        .join("auto-report.md")
}

pub async fn run_auto_loop(state: &mut AppState, opts: AutoOptions) -> Result<()> {
    // --- refusals (mirror run_iterate_loop) ---
    if state.in_play_session() {
        anyhow::bail!("cannot /auto during a /play session — /play exit first");
    }
    if !state.config.rubric.enabled {
        anyhow::bail!("the rubric evaluator is disabled — set rubric.enabled = true to use /auto");
    }
    if should_refuse_cloud_handoff(state.backend.name, state.config.rubric.allow_cloud) {
        anyhow::bail!(
            "/auto sends workspace context to the evaluator — run it on a local backend or set rubric.allowCloud"
        );
    }
    if should_refuse_cloud_handoff(state.backend.name, opts.allow_cloud) {
        anyhow::bail!(
            "/auto auto-resets, which drafts a handoff on the backend — run it on a local backend or pass --cloud"
        );
    }

    // --- resolve goal + Done Criteria ---
    let spec_path = default_spec_path(&state.config.workspace_root);
    let (goal, criteria) = if opts.use_spec {
        let spec = std::fs::read_to_string(&spec_path).map_err(|_| {
            anyhow!(
                "--spec was given but no spec found at {} — run /plan <intent> first",
                spec_path.display()
            )
        })?;
        let spec_goal = extract_spec_goal(&spec);
        let goal = if opts.goal.trim().is_empty() {
            spec_goal
        } else {
            opts.goal.clone()
        };
        (goal, parse_done_criteria(&spec))
    } else {
        // Opportunistically read criteria if a spec happens to exist.
        let criteria = std::fs::read_to_string(&spec_path)
            .map(|s| parse_done_criteria(&s))
            .unwrap_or_default();
        (opts.goal.clone(), criteria)
    };
    if goal.trim().is_empty() {
        anyhow::bail!("no goal — pass `/auto <goal>` or `--spec` with a spec.md that has a Goal");
    }

    let max_rounds = opts.max_rounds.clamp(1, MAX_ROUNDS_CEILING);
    let reset_ratio = opts.reset_ratio.clamp(0.50, 0.95);

    // A distinct evaluator model is the cleanest generator/evaluator split; reuse
    // /iterate's choice and warn when they coincide.
    let evaluator_model = state
        .config
        .iterate
        .evaluator_model
        .clone()
        .unwrap_or_else(|| state.model.clone());
    if evaluator_model == state.model {
        println!(
            "  {DIM}note: generator and evaluator share model {} — set iterate.evaluatorModel for stronger separation{RESET}",
            state.model
        );
    }
    if !state.config.checkpoints.enabled {
        println!(
            "  {YELLOW}warning{RESET} {DIM}checkpoints are off — /undo won't be available after the run{RESET}"
        );
    }

    // Announce the bounds so an unattended run is auditable in the scrollback.
    print_run_header(&goal, max_rounds, &opts, criteria.len());

    // --- outer-loop Ctrl-C flag (caught between rounds; in-turn cancellation is
    // handled by run_user_turn's own handler) ---
    let interrupted = Arc::new(AtomicBool::new(false));
    let signal_flag = interrupted.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_flag.store(true, Ordering::SeqCst);
        }
    });

    let restore = apply_iterate_mode(state);

    let mut run = AutoRun {
        rounds: Vec::new(),
        resets: 0,
        started: Instant::now(),
        elapsed: Duration::ZERO,
        spent_usd: 0.0,
        budget_usd: opts.budget_usd,
        threshold: opts.threshold,
        stop_reason: StopReason::MaxRounds,
        last_done_check: None,
    };

    // Loop-local state — survives auto-resets (cmd_new only clears state.messages).
    let mut feedback: Option<String> = None;
    let mut prev_score = f32::MIN;
    let mut prev_diff_fp: u64 = 0;
    let mut stall_rounds = 0usize;
    let mut loop_result: Result<()> = Ok(());

    for round in 1..=max_rounds {
        // --- stop checks before spending on a fresh round ---
        if let Some(reason) = pre_round_stop(
            interrupted.load(Ordering::SeqCst),
            run.started.elapsed(),
            opts.deadline,
            run.spent_usd,
            opts.budget_usd,
        ) {
            run.stop_reason = reason;
            break;
        }

        println!(
            "  {CYAN}auto{RESET} {DIM}round {round}/{max_rounds} · {}{RESET}",
            elapsed_hms(run.started.elapsed())
        );

        // --- generator turn (budget tracked around it; resets zero session_usd,
        // so we accumulate the per-turn delta rather than read a running total) ---
        let usd_before = state.session_usd;
        let prompt = build_iterate_prompt(round, &goal, feedback.as_deref());
        if let Err(e) = run_user_turn(
            state,
            TurnOptions {
                user_prompt: prompt,
                auto_verify_tests: false,
                yolo_approve: opts.yolo,
                source: "auto",
            },
        )
        .await
        {
            loop_result = Err(e);
            run.stop_reason = StopReason::TurnError;
            break;
        }
        run.spent_usd += (state.session_usd - usd_before).max(0.0);

        // --- evaluator turn ---
        let diff = collect_diff_context(&state.config.workspace_root);
        let target = format!("Goal:\n{goal}");
        let verdict = run_evaluation(
            &state.http,
            &state.backend,
            &evaluator_model,
            &state.config,
            &target,
            Some(&diff),
            None,
            None,
        )
        .await;
        let score = verdict.weighted_total;
        let passed = verdict.parsed && score >= opts.threshold;
        let diff_fp = fnv1a(&diff);

        let mark = if passed { GREEN } else { RED };
        println!(
            "  {mark}eval{RESET} {DIM}{score:.1}/10 (threshold {:.1}){RESET}",
            opts.threshold
        );

        run.rounds.push(RoundRecord {
            round,
            score,
            passed,
            verdict: verdict.verdict.clone(),
            reset_after: false,
        });

        // --- done-check against spec criteria (only when eval already passed) ---
        // Stop requires `passed && done_ok`. Running the LLM done-check on a
        // failing rubric round cannot change the stop decision and wastes a call.
        let mut done_ok = true;
        if should_run_done_check(passed, criteria.len()) {
            let dc = run_done_check(state, &evaluator_model, &criteria, &diff).await;
            println!(
                "  {DIM}done-check {}/{} criteria{RESET}",
                dc.met_count(),
                dc.total()
            );
            done_ok = dc.all_met();
            run.last_done_check = Some(dc);
        }

        if passed && done_ok {
            run.stop_reason = StopReason::GoalMet;
            break;
        }

        feedback = Some(render_feedback(&verdict));

        // --- stall detection: no score gain AND identical diff ---
        if score <= prev_score + SCORE_EPS && diff_fp == prev_diff_fp {
            stall_rounds += 1;
            if stall_rounds >= STALL_LIMIT {
                run.stop_reason = StopReason::Stalled;
                break;
            }
        } else {
            stall_rounds = 0;
        }
        prev_score = score;
        prev_diff_fp = diff_fp;

        // --- auto-reset when the context window is filling ---
        if should_auto_reset(state, reset_ratio) {
            println!("  {YELLOW}reset{RESET} {DIM}context filling — handing off to a fresh session{RESET}");
            if let Err(e) = crate::commands::perform_reset(state, false).await {
                // A failed reset isn't fatal — keep going on the current context.
                println!("  {RED}reset failed:{RESET} {DIM}{e}{RESET}");
            } else {
                run.resets += 1;
                if let Some(last) = run.rounds.last_mut() {
                    last.reset_after = true;
                }
            }
        }
    }

    // The per-round check only runs once the rubric passes, so a run that never
    // cleared the threshold reaches here with no criteria state — and the report
    // would render every criterion unchecked, which reads as "nothing met"
    // rather than "not measured". One check against the final tree fixes that
    // while keeping the per-round saving.
    let reported = run.last_done_check.is_some();
    if should_run_final_done_check(reported, criteria.len(), run.stop_reason) {
        let diff = collect_diff_context(&state.config.workspace_root);
        let dc = run_done_check(state, &evaluator_model, &criteria, &diff).await;
        println!(
            "  {DIM}done-check {}/{} criteria (final){RESET}",
            dc.met_count(),
            dc.total()
        );
        run.last_done_check = Some(dc);
    }

    restore_iterate_mode(state, restore);
    signal_task.abort();
    run.elapsed = run.started.elapsed();

    // Always write the report, whatever the exit path.
    let report_path = write_auto_report(&run, &goal, &criteria, &state.config.workspace_root)?;
    print_run_summary(&run, &report_path);
    loop_result
}

/// Render the report and write it to `.albatross/auto-report.md`, creating
/// the parent directory if needed. Returns the path written.
fn write_auto_report(
    run: &AutoRun,
    goal: &str,
    criteria: &[String],
    workspace_root: &str,
) -> Result<PathBuf> {
    let report = render_auto_report(run, goal, criteria);
    let report_path = default_auto_report_path(workspace_root);
    if let Some(parent) = report_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(&report_path, &report)?;
    Ok(report_path)
}

/// Pure pre-round stop decision so the guardrail logic is table-testable.
fn pre_round_stop(
    interrupted: bool,
    elapsed: Duration,
    deadline: Option<Duration>,
    spent_usd: f64,
    budget_usd: Option<f64>,
) -> Option<StopReason> {
    if interrupted {
        return Some(StopReason::Interrupted);
    }
    if let Some(d) = deadline {
        if elapsed >= d {
            return Some(StopReason::DeadlineReached);
        }
    }
    if let Some(cap) = budget_usd {
        if spent_usd >= cap {
            return Some(StopReason::BudgetExhausted);
        }
    }
    None
}

/// True when the live transcript has filled past `reset_ratio` of the model's
/// effective context limit. Deliberately measures the transcript only (omitting
/// the per-turn system prompt and tool schemas, which `run_user_turn` rebuilds
/// internally): it undercounts slightly, so it triggers a touch late rather than
/// spuriously.
fn should_auto_reset(state: &AppState, reset_ratio: f64) -> bool {
    let budget = crate::budget::measure_prompt_budget("", &state.messages, &[]);
    let limit = crate::context_guard::effective_limit_bytes(
        &state.config.context,
        &state.model,
        state.backend.is_local,
    );
    crate::budget::usage_ratio(&budget, limit) >= reset_ratio
}

/// Whether this round should spend an LLM call on Done Criteria.
/// Only useful when the rubric already passed — otherwise stop stays false.
pub(crate) fn should_run_done_check(passed: bool, criteria_len: usize) -> bool {
    passed && criteria_len > 0
}

/// Whether the post-loop pass should spend one call to fill in criteria state
/// for the report. Skipped when a round already reported, when there are no
/// criteria, and on interrupt — a user who pressed Ctrl-C wants the run to end,
/// not to wait on another model call.
pub(crate) fn should_run_final_done_check(
    already_reported: bool,
    criteria_len: usize,
    stop_reason: StopReason,
) -> bool {
    !already_reported && criteria_len > 0 && stop_reason != StopReason::Interrupted
}

/// Ask the evaluator which Done Criteria the current diff satisfies. Kept
/// separate from the 0–10 rubric verdict because coverage and quality are
/// different questions; returns a tolerant default (nothing met) on any failure
/// so the loop never stalls on a flaky check.
pub(crate) async fn run_done_check(
    state: &AppState,
    model: &str,
    criteria: &[String],
    diff: &str,
) -> DoneCheck {
    let none = DoneCheck {
        met: vec![false; criteria.len()],
    };
    if criteria.is_empty() {
        return none;
    }
    let numbered: String = criteria
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{}. {c}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let system = ChatMessage::System {
        content: [
            "You verify which Done Criteria a code change satisfies.",
            "Judge strictly from the supplied diff — a criterion is MET only if the diff clearly satisfies it.",
            "Reply with ONLY a JSON object of 1-based criterion numbers: {\"met\":[...],\"unmet\":[...]}.",
            "Every criterion number must appear in exactly one list. No prose.",
        ]
        .join("\n"),
    };
    let user = ChatMessage::User {
        content: format!(
            "Done Criteria:\n{numbered}\n\nWorking-tree diff:\n{diff}\n\nReturn the JSON object now."
        )
        .into(),
    };
    let messages = vec![system, user];
    let req = ChatRequest {
        model,
        messages: &messages,
        tools: None,
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: false,
        }),
        max_tokens: Some(400),
        prompt_cache_key: None,
        effort: None,
    };
    let mut raw = String::new();
    let result = stream_chat(&state.http, &state.backend, &req, None, |chunk| {
        if let Some(choice) = chunk.choices.first() {
            if let Some(content) = &choice.delta.content {
                raw.push_str(content);
            }
        }
    })
    .await;
    if result.is_err() {
        return none;
    }
    parse_done_check(&raw, criteria.len()).unwrap_or(none)
}

/// Parse the `{"met":[...],"unmet":[...]}` reply into a per-criterion bitmap.
/// `met` wins on conflicts; out-of-range indices are ignored.
fn parse_done_check(raw: &str, total: usize) -> Option<DoneCheck> {
    let json = extract_json_object(raw)?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    let mut met = vec![false; total];
    if let Some(arr) = value.get("met").and_then(|v| v.as_array()) {
        for n in arr.iter().filter_map(|v| v.as_u64()) {
            if n >= 1 && (n as usize) <= total {
                met[n as usize - 1] = true;
            }
        }
    }
    Some(DoneCheck { met })
}

/// Tolerant JSON-object extraction: strips ``` fences and slices the outermost
/// `{...}`. Mirrors the rubric module's approach.
fn extract_json_object(raw: &str) -> Option<String> {
    let mut trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("```json") {
        trimmed = rest.trim();
    } else if let Some(rest) = trimmed.strip_prefix("```") {
        trimmed = rest.trim();
    }
    let trimmed = trimmed.trim_end_matches("```").trim();
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return Some(trimmed.to_string());
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end <= start {
        return None;
    }
    let candidate = &trimmed[start..=end];
    serde_json::from_str::<serde_json::Value>(candidate)
        .ok()
        .map(|_| candidate.to_string())
}

/// Extract the `## Goal` section body of a spec as a one-line goal.
fn extract_spec_goal(spec: &str) -> String {
    section_body(spec, "Goal")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

/// Pull the bullet lines under the `## Done Criteria` heading, skipping
/// placeholder text the planner emits when a section is unfilled.
pub fn parse_done_criteria(spec: &str) -> Vec<String> {
    let body = section_body(spec, "Done Criteria");
    body.lines()
        .map(str::trim)
        .filter_map(|line| {
            let item = line
                .trim_start_matches(['-', '*'])
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .trim_start_matches(['.', ')'])
                .trim();
            if item.is_empty() {
                return None;
            }
            // Only keep lines that were actually bullets/numbered, not prose.
            let is_item = line.starts_with('-')
                || line.starts_with('*')
                || line.chars().next().is_some_and(|c| c.is_ascii_digit());
            if !is_item {
                return None;
            }
            let lower = item.to_ascii_lowercase();
            if lower.starts_with("to be defined") || lower == "none." || lower == "none" {
                return None;
            }
            Some(item.to_string())
        })
        .collect()
}

/// Return the text under a `## <heading>` until the next heading of the same or
/// higher level. Heading match is case-insensitive and ignores leading `#`s.
fn section_body(markdown: &str, heading: &str) -> String {
    let mut out = String::new();
    let mut in_section = false;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix('#') {
            let title = rest.trim_start_matches('#').trim();
            if in_section {
                // Any subsequent heading ends the section.
                break;
            }
            if title.eq_ignore_ascii_case(heading) {
                in_section = true;
            }
            continue;
        }
        if in_section {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn render_auto_report(run: &AutoRun, goal: &str, criteria: &[String]) -> String {
    let mut out = String::new();
    out.push_str("# Auto Run Report\n\n");
    out.push_str(&format!("Generated: {}\n\n", Utc::now().to_rfc3339()));

    out.push_str("## Verdict\n\n");
    out.push_str(run.stop_reason.label());
    out.push_str("\n\n");

    out.push_str("## Goal\n\n");
    out.push_str(goal.trim());
    out.push_str("\n\n");

    let final_score = run.rounds.last().map(|r| r.score).unwrap_or(0.0);
    out.push_str("## Stats\n\n");
    out.push_str(&format!("- Rounds run: {}\n", run.rounds.len()));
    out.push_str(&format!(
        "- Final score: {final_score:.1}/10 (threshold {:.1})\n",
        run.threshold
    ));
    out.push_str(&format!("- Resets: {}\n", run.resets));
    out.push_str(&format!("- Elapsed: {}\n", elapsed_hms(run.elapsed)));
    match run.budget_usd {
        Some(cap) => out.push_str(&format!(
            "- Generator spend: ${:.2} (budget ${:.2})\n",
            run.spent_usd, cap
        )),
        None => out.push_str(&format!("- Generator spend: ${:.2}\n", run.spent_usd)),
    }
    out.push_str(
        "\n_Budget tracks generator turns; evaluator/done-check calls are not counted._\n\n",
    );

    out.push_str("## Per-round scores\n\n");
    out.push_str("| Round | Score | Passed | Reset after | Note |\n");
    out.push_str("|-------|-------|--------|-------------|------|\n");
    for r in &run.rounds {
        let note = r.verdict.replace('|', "\\|").replace('\n', " ");
        let note: String = note.chars().take(80).collect();
        out.push_str(&format!(
            "| {} | {:.1} | {} | {} | {} |\n",
            r.round,
            r.score,
            if r.passed { "yes" } else { "no" },
            if r.reset_after { "yes" } else { "no" },
            note
        ));
    }
    out.push('\n');

    if !criteria.is_empty() {
        out.push_str("## Done Criteria\n\n");
        let met = run.last_done_check.as_ref();
        for (i, c) in criteria.iter().enumerate() {
            let checked = met.and_then(|d| d.met.get(i)).copied().unwrap_or(false);
            out.push_str(&format!("- [{}] {c}\n", if checked { "x" } else { " " }));
        }
        out.push('\n');
    }

    out.push_str("## Next\n\n");
    if run.stop_reason.succeeded() {
        out.push_str("Goal met — review the diff and run `/handoff` to draft the commit.\n");
    } else {
        out.push_str(
            "Not complete — read the per-round notes; `/undo` reverts to the last reset boundary.\n",
        );
    }
    out
}

fn print_run_header(goal: &str, max_rounds: usize, opts: &AutoOptions, criteria: usize) {
    println!("  {CYAN}auto{RESET} {DIM}starting autonomous run{RESET}");
    println!("  {DIM}goal:{RESET} {goal}");
    let mut bounds = vec![format!("max {max_rounds} rounds")];
    if let Some(b) = opts.budget_usd {
        bounds.push(format!("budget ${b:.2}"));
    }
    if let Some(d) = opts.deadline {
        bounds.push(format!("deadline {}", elapsed_hms(d)));
    }
    bounds.push(format!("reset at {:.0}%", opts.reset_ratio * 100.0));
    println!("  {DIM}bounds: {}{RESET}", bounds.join(" · "));
    if criteria > 0 {
        println!("  {DIM}done-check: {criteria} spec criteria{RESET}");
    }
}

fn print_run_summary(run: &AutoRun, report_path: &Path) {
    let mark = if run.stop_reason.succeeded() {
        GREEN
    } else {
        YELLOW
    };
    let final_score = run.rounds.last().map(|r| r.score).unwrap_or(0.0);
    println!(
        "  {mark}{}{RESET} {DIM}· {} round(s) · {} · {:.1}/10 · {} reset(s){RESET}",
        run.stop_reason.label(),
        run.rounds.len(),
        elapsed_hms(run.elapsed),
        final_score,
        run.resets,
    );
    println!("  {DIM}report →{RESET} {}", report_path.display());
}

/// `H:MM:SS` from a duration.
fn elapsed_hms(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// Cheap content fingerprint for stall detection (FNV-1a, 64-bit).
fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// `"6h"`/`"30m"`/`"90s"`/bare-seconds → [`Duration`].
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("empty duration");
    }
    let (num, mult) = match s.chars().last().unwrap() {
        'h' | 'H' => (&s[..s.len() - 1], 3600),
        'm' | 'M' => (&s[..s.len() - 1], 60),
        's' | 'S' => (&s[..s.len() - 1], 1),
        c if c.is_ascii_digit() => (s, 1),
        other => anyhow::bail!("invalid duration unit '{other}' (use h/m/s)"),
    };
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| anyhow!("invalid duration value '{s}'"))?;
    Ok(Duration::from_secs(n * mult))
}

/// `"$2.00"`/`"2.00"`/`"2"` → dollars.
fn parse_budget(s: &str) -> Result<f64> {
    let cleaned = s.trim().trim_start_matches('$').trim();
    let v: f64 = cleaned
        .parse()
        .map_err(|_| anyhow!("invalid budget '{s}' (try --budget 2.00)"))?;
    if v < 0.0 {
        anyhow::bail!("budget must be non-negative");
    }
    Ok(v)
}

pub fn parse_auto_args(
    args: &str,
    default_max: usize,
    default_threshold: f32,
    default_reset_ratio: f64,
) -> Result<AutoOptions> {
    let mut use_spec = false;
    let mut max_rounds = default_max;
    let mut threshold = default_threshold;
    let mut yolo = false;
    let mut allow_cloud = false;
    let mut budget_usd: Option<f64> = None;
    let mut deadline: Option<Duration> = None;
    let mut reset_ratio = default_reset_ratio;
    let mut goal_parts: Vec<&str> = Vec::new();

    // Helper closure-free loop so we can pull the next token for space-separated
    // values, matching parse_iterate_args' shape.
    let mut parts = args.split_whitespace();
    while let Some(part) = parts.next() {
        match part {
            "--spec" => use_spec = true,
            "--yolo" => yolo = true,
            "--cloud" => allow_cloud = true,
            "--max" => {
                let v = parts.next().ok_or_else(|| anyhow!("missing --max value"))?;
                max_rounds = v.parse().map_err(|_| anyhow!("invalid --max value"))?;
            }
            f if f.starts_with("--max=") => {
                max_rounds = f
                    .strip_prefix("--max=")
                    .unwrap()
                    .parse()
                    .map_err(|_| anyhow!("invalid --max value"))?;
            }
            "--threshold" => {
                let v = parts
                    .next()
                    .ok_or_else(|| anyhow!("missing --threshold value"))?;
                threshold = v
                    .parse()
                    .map_err(|_| anyhow!("invalid --threshold value"))?;
            }
            f if f.starts_with("--threshold=") => {
                threshold = f
                    .strip_prefix("--threshold=")
                    .unwrap()
                    .parse()
                    .map_err(|_| anyhow!("invalid --threshold value"))?;
            }
            "--budget" => {
                let v = parts
                    .next()
                    .ok_or_else(|| anyhow!("missing --budget value"))?;
                budget_usd = Some(parse_budget(v)?);
            }
            f if f.starts_with("--budget=") => {
                budget_usd = Some(parse_budget(f.strip_prefix("--budget=").unwrap())?);
            }
            "--deadline" => {
                let v = parts
                    .next()
                    .ok_or_else(|| anyhow!("missing --deadline value"))?;
                deadline = Some(parse_duration(v)?);
            }
            f if f.starts_with("--deadline=") => {
                deadline = Some(parse_duration(f.strip_prefix("--deadline=").unwrap())?);
            }
            "--reset-at" => {
                let v = parts
                    .next()
                    .ok_or_else(|| anyhow!("missing --reset-at value"))?;
                reset_ratio = v.parse().map_err(|_| anyhow!("invalid --reset-at value"))?;
            }
            f if f.starts_with("--reset-at=") => {
                reset_ratio = f
                    .strip_prefix("--reset-at=")
                    .unwrap()
                    .parse()
                    .map_err(|_| anyhow!("invalid --reset-at value"))?;
            }
            other if other.starts_with("--") => {
                anyhow::bail!("unknown /auto argument: {other}")
            }
            other => goal_parts.push(other),
        }
    }

    let goal = goal_parts.join(" ").trim().to_string();
    if goal.is_empty() && !use_spec {
        anyhow::bail!(
            "usage: /auto <goal> [--spec] [--max N] [--threshold X] [--budget $] [--deadline 6h] [--reset-at 0.75] [--yolo] [--cloud]"
        );
    }

    Ok(AutoOptions {
        goal,
        use_spec,
        max_rounds: max_rounds.clamp(1, MAX_ROUNDS_CEILING),
        threshold,
        yolo,
        allow_cloud,
        budget_usd,
        deadline,
        reset_ratio: reset_ratio.clamp(0.50, 0.95),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::ApprovalCache;
    use crate::backends::{backend, BackendName};
    use crate::config::AgentConfig;
    use crate::renderer::TuiRenderer;
    use crate::session_paths::PathStore;
    use crate::turn_checkpoint::CheckpointStack;
    use std::path::Path;

    fn test_state(root: &Path) -> AppState {
        let config = AgentConfig {
            workspace_root: root.display().to_string(),
            session_dir: root.join(".sessions").display().to_string(),
            ..Default::default()
        };
        let session_path = root.join(".sessions/test.jsonl");
        AppState {
            http: reqwest::Client::new(),
            backend: config.backend_descriptor(),
            model: "test-model".into(),
            active_effort: None,
            active_route: None,
            messages: Vec::new(),
            session_dir: config.session_dir.clone(),
            session_path: session_path.clone(),
            total_in: 0,
            total_out: 0,
            session_usd: 0.0,
            session_cost_has_unknown: false,
            context_guard_notice: None,
            conversation_summary: None,
            checkpoint_stack: CheckpointStack::new(config.checkpoints.limits()),
            checkpoints_enabled: config.checkpoints.enabled,
            play_session: None,
            last_play_scorecard: None,
            approval_cache: ApprovalCache::new(),
            renderer: TuiRenderer::new(config.display.clone()),
            hooks: crate::hooks::HookRegistry::default(),
            session_hook_contexts: Vec::new(),
            pending_hook_contexts: Vec::new(),
            warmed_fingerprint: None,
            tests_ran_this_session: false,
            pending_image_attachments: Vec::new(),
            mcp_tools: Vec::new(),
            path_store: PathStore::new(&config.session_dir, &session_path, &config.paths),
            trace: crate::turn_trace::test_trace_for(&session_path),
            trace_enabled: false,
            config,
        }
    }

    #[test]
    fn parse_auto_args_extracts_goal_and_all_flags() {
        let o = parse_auto_args(
            "make X robust --max 5 --threshold 8 --budget $2.50 --deadline 6h --reset-at 0.8 --yolo --cloud",
            12,
            7.0,
            0.75,
        )
        .unwrap();
        assert_eq!(o.goal, "make X robust");
        assert_eq!(o.max_rounds, 5);
        assert!((o.threshold - 8.0).abs() < 1e-6);
        assert_eq!(o.budget_usd, Some(2.5));
        assert_eq!(o.deadline, Some(Duration::from_secs(6 * 3600)));
        assert!((o.reset_ratio - 0.8).abs() < 1e-6);
        assert!(o.yolo);
        assert!(o.allow_cloud);
        assert!(!o.use_spec);
    }

    #[test]
    fn parse_auto_args_equals_forms() {
        let o = parse_auto_args(
            "polish --max=3 --threshold=6.5 --budget=1 --deadline=30m --reset-at=0.6",
            12,
            7.0,
            0.75,
        )
        .unwrap();
        assert_eq!(o.max_rounds, 3);
        assert!((o.threshold - 6.5).abs() < 1e-6);
        assert_eq!(o.budget_usd, Some(1.0));
        assert_eq!(o.deadline, Some(Duration::from_secs(1800)));
        assert!((o.reset_ratio - 0.6).abs() < 1e-6);
    }

    #[test]
    fn parse_auto_args_spec_allows_empty_goal() {
        let o = parse_auto_args("--spec", 12, 7.0, 0.75).unwrap();
        assert!(o.use_spec);
        assert!(o.goal.is_empty());
    }

    #[test]
    fn parse_auto_args_requires_goal_or_spec() {
        assert!(parse_auto_args("", 12, 7.0, 0.75).is_err());
        assert!(parse_auto_args("--max 3", 12, 7.0, 0.75).is_err());
        assert!(parse_auto_args("goal --bogus", 12, 7.0, 0.75).is_err());
        assert!(parse_auto_args("goal --budget nope", 12, 7.0, 0.75).is_err());
    }

    #[test]
    fn parse_auto_args_clamps_max_and_reset_ratio() {
        let big = parse_auto_args("x --max 999", 12, 7.0, 0.75).unwrap();
        assert_eq!(big.max_rounds, MAX_ROUNDS_CEILING);
        let lo = parse_auto_args("x --reset-at 0.1", 12, 7.0, 0.75).unwrap();
        assert!((lo.reset_ratio - 0.50).abs() < 1e-6);
        let hi = parse_auto_args("x --reset-at 0.99", 12, 7.0, 0.75).unwrap();
        assert!((hi.reset_ratio - 0.95).abs() < 1e-6);
    }

    #[test]
    fn parse_duration_units_and_errors() {
        assert_eq!(parse_duration("6h").unwrap(), Duration::from_secs(21600));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("120").unwrap(), Duration::from_secs(120));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("5y").is_err());
    }

    #[test]
    fn parse_budget_forms() {
        assert_eq!(parse_budget("$2.00").unwrap(), 2.0);
        assert_eq!(parse_budget("2.50").unwrap(), 2.5);
        assert_eq!(parse_budget("2").unwrap(), 2.0);
        assert!(parse_budget("abc").is_err());
        assert!(parse_budget("-1").is_err());
    }

    #[test]
    fn parse_done_criteria_extracts_bullets() {
        let spec = "# Spec\n\n## Goal\n\nDo the thing.\n\n## Done Criteria\n\n- retries on 5xx\n- retry count configurable\n2. backoff is exponential\n\n## Open Questions\n\n- none\n";
        let crit = parse_done_criteria(spec);
        assert_eq!(
            crit,
            vec![
                "retries on 5xx".to_string(),
                "retry count configurable".to_string(),
                "backoff is exponential".to_string()
            ]
        );
    }

    #[test]
    fn parse_done_criteria_skips_placeholders() {
        let spec = "## Done Criteria\n\n- To be defined.\n- None.\n";
        assert!(parse_done_criteria(spec).is_empty());
        // No section at all → empty.
        assert!(parse_done_criteria("## Goal\n\nx\n").is_empty());
    }

    #[test]
    fn extract_spec_goal_joins_lines() {
        let spec = "## Goal\n\nMake the parser\nrobust to bad input.\n\n## Scope\n\n- x\n";
        assert_eq!(
            extract_spec_goal(spec),
            "Make the parser robust to bad input."
        );
    }

    #[test]
    fn done_check_only_when_eval_passed_and_criteria_exist() {
        assert!(!should_run_done_check(false, 3));
        assert!(!should_run_done_check(true, 0));
        assert!(!should_run_done_check(false, 0));
        assert!(should_run_done_check(true, 2));
    }

    #[test]
    fn final_done_check_fills_report_when_no_round_ran_one() {
        // A run that never cleared the threshold has no criteria state yet.
        assert!(should_run_final_done_check(false, 3, StopReason::MaxRounds));
        assert!(should_run_final_done_check(false, 3, StopReason::Stalled));
        assert!(should_run_final_done_check(
            false,
            3,
            StopReason::BudgetExhausted
        ));
    }

    #[test]
    fn final_done_check_skipped_when_unnecessary_or_interrupted() {
        // A passing round already reported — don't pay twice.
        assert!(!should_run_final_done_check(true, 3, StopReason::GoalMet));
        // No criteria to check.
        assert!(!should_run_final_done_check(
            false,
            0,
            StopReason::MaxRounds
        ));
        // Ctrl-C means stop now, not "make one more model call".
        assert!(!should_run_final_done_check(
            false,
            3,
            StopReason::Interrupted
        ));
    }

    #[test]
    fn parse_done_check_builds_bitmap() {
        let dc = parse_done_check("{\"met\":[1,3],\"unmet\":[2]}", 3).unwrap();
        assert_eq!(dc.met, vec![true, false, true]);
        assert_eq!(dc.met_count(), 2);
        assert!(!dc.all_met());
        // Fenced + out-of-range indices ignored.
        let dc2 = parse_done_check("```json\n{\"met\":[1,2,9]}\n```", 2).unwrap();
        assert_eq!(dc2.met, vec![true, true]);
        assert!(dc2.all_met());
    }

    #[test]
    fn pre_round_stop_priority() {
        // Interrupt wins over everything.
        assert_eq!(
            pre_round_stop(
                true,
                Duration::from_secs(1),
                Some(Duration::from_secs(10)),
                5.0,
                Some(1.0)
            ),
            Some(StopReason::Interrupted)
        );
        // Deadline before budget.
        assert_eq!(
            pre_round_stop(
                false,
                Duration::from_secs(10),
                Some(Duration::from_secs(10)),
                0.0,
                Some(1.0)
            ),
            Some(StopReason::DeadlineReached)
        );
        // Budget when over cap.
        assert_eq!(
            pre_round_stop(false, Duration::from_secs(1), None, 2.0, Some(1.0)),
            Some(StopReason::BudgetExhausted)
        );
        // Nothing tripped.
        assert_eq!(
            pre_round_stop(
                false,
                Duration::from_secs(1),
                Some(Duration::from_secs(10)),
                0.5,
                Some(1.0)
            ),
            None
        );
    }

    #[test]
    fn render_auto_report_has_all_sections() {
        let run = AutoRun {
            rounds: vec![
                RoundRecord {
                    round: 1,
                    score: 6.0,
                    passed: false,
                    verdict: "needs work".into(),
                    reset_after: false,
                },
                RoundRecord {
                    round: 2,
                    score: 8.0,
                    passed: true,
                    verdict: "good".into(),
                    reset_after: true,
                },
            ],
            resets: 1,
            started: Instant::now(),
            elapsed: Duration::from_secs(3661),
            spent_usd: 0.42,
            budget_usd: Some(2.0),
            threshold: 7.0,
            stop_reason: StopReason::GoalMet,
            last_done_check: Some(DoneCheck {
                met: vec![true, false],
            }),
        };
        let md = render_auto_report(&run, "build a thing", &["a".into(), "b".into()]);
        assert!(md.contains("## Verdict\n\nGoal met"));
        assert!(md.contains("## Goal\n\nbuild a thing"));
        assert!(md.contains("Rounds run: 2"));
        assert!(md.contains("Resets: 1"));
        assert!(md.contains("Elapsed: 1:01:01"));
        assert!(md.contains("$0.42"));
        assert!(md.contains("| 2 | 8.0 | yes | yes |"));
        assert!(md.contains("- [x] a"));
        assert!(md.contains("- [ ] b"));
        assert!(md.contains("/handoff"));
    }

    #[test]
    fn write_auto_report_creates_file_under_albatross() {
        let dir = tempfile::tempdir().unwrap();
        let run = AutoRun {
            rounds: vec![RoundRecord {
                round: 1,
                score: 5.0,
                passed: false,
                verdict: "wip".into(),
                reset_after: false,
            }],
            resets: 0,
            started: Instant::now(),
            elapsed: Duration::from_secs(5),
            spent_usd: 0.0,
            budget_usd: None,
            threshold: 7.0,
            stop_reason: StopReason::TurnError,
            last_done_check: None,
        };
        let path = write_auto_report(&run, "goal", &[], &dir.path().display().to_string()).unwrap();
        assert!(path.ends_with(".albatross/auto-report.md"));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("Stopped on a turn error"));
    }

    #[test]
    fn should_auto_reset_triggers_above_ratio() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        // Tiny context budget so a little transcript trips the ratio.
        state.config.context.model_context_tokens = Some(64);
        state.config.context.max_bytes = Some(400);
        assert!(!should_auto_reset(&state, 0.75));
        state.messages.push(ChatMessage::User {
            content: "x".repeat(2000).into(),
        });
        assert!(should_auto_reset(&state, 0.75));
    }

    #[tokio::test]
    async fn auto_refuses_during_play_session() {
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
        let err = run_auto_loop(&mut state, opts("x")).await.unwrap_err();
        assert!(err.to_string().contains("/play exit"));
    }

    #[tokio::test]
    async fn auto_refuses_cloud_without_allow_flag() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        state.backend = backend(BackendName::Openrouter);
        let err = run_auto_loop(&mut state, opts("x")).await.unwrap_err();
        assert!(err.to_string().contains("allowCloud") || err.to_string().contains("cloud"));
    }

    #[tokio::test]
    async fn auto_refuses_when_rubric_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        state.config.rubric.enabled = false;
        let err = run_auto_loop(&mut state, opts("x")).await.unwrap_err();
        assert!(err.to_string().contains("disabled"));
    }

    #[tokio::test]
    async fn auto_refuses_spec_flag_without_spec_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        let mut o = opts("");
        o.use_spec = true;
        let err = run_auto_loop(&mut state, o).await.unwrap_err();
        assert!(err.to_string().contains("no spec"));
    }

    fn opts(goal: &str) -> AutoOptions {
        AutoOptions {
            goal: goal.into(),
            use_spec: false,
            max_rounds: 1,
            threshold: 7.0,
            yolo: true,
            allow_cloud: false,
            budget_usd: None,
            deadline: None,
            reset_ratio: 0.75,
        }
    }
}
