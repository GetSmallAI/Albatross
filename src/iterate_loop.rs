//! The `/iterate` generate→evaluate→improve loop.
//!
//! Structurally a clone of [`crate::fix_loop`], with one swap: where `/fix`
//! stops on `tests_passing`, `/iterate` stops when a *separate* evaluator
//! ([`crate::tools::run_evaluation`]) scores the work at or above a threshold on
//! the rubric. Each round the generator works toward the goal, the critic grades
//! the resulting diff, and — if short of the bar — the critic's feedback is fed
//! back with explicit permission to refine *or pivot*. This is the article's
//! iterative feedback loop with the generator/evaluator separation that makes it
//! trustworthy.

use anyhow::{anyhow, Result};

use crate::app_state::{AppState, FixRestoreSnapshot};
use crate::config::{ApprovalPolicy, OperatorMode};
use crate::handoff::{collect_handoff_context, should_refuse_cloud_handoff};
use crate::rubric::EvalVerdict;
use crate::session_turn::{run_user_turn, TurnOptions};
use crate::shipcheck::collect_shipcheck;
use crate::tools::run_evaluation;

const RESET: crate::theme::Style = crate::theme::RESET;
const DIM: crate::theme::Style = crate::theme::MUTED;
const GREEN: crate::theme::Style = crate::theme::SUCCESS;
const RED: crate::theme::Style = crate::theme::ERROR;
const YELLOW: crate::theme::Style = crate::theme::WARN;

/// Upper bound on iterations regardless of config/flags — a runaway guard, since
/// each round costs a generator turn plus a critic run.
const MAX_ITERS_CEILING: usize = 15;

pub struct IterateOptions {
    pub goal: String,
    pub max_iters: usize,
    pub threshold: f32,
    pub yolo: bool,
}

/// Swap into a full-toolkit working mode for the duration of the loop (like
/// `/fix`), returning a snapshot to restore afterward.
pub(crate) fn apply_iterate_mode(state: &mut AppState) -> FixRestoreSnapshot {
    let restore = FixRestoreSnapshot {
        config: state.config.clone(),
        checkpoints_enabled: state.checkpoints_enabled,
    };
    state.config.apply_operator_mode(OperatorMode::Ship);
    state.checkpoints_enabled = state.config.checkpoints.enabled;
    if state.config.approval_policy == ApprovalPolicy::Always {
        state.config.approval_policy = ApprovalPolicy::DangerousOnly;
    }
    restore
}

pub(crate) fn restore_iterate_mode(state: &mut AppState, restore: FixRestoreSnapshot) {
    state.config = restore.config;
    state.checkpoints_enabled = restore.checkpoints_enabled;
}

pub(crate) fn build_iterate_prompt(iter: usize, goal: &str, feedback: Option<&str>) -> String {
    match feedback {
        None => format!("Work toward this goal, writing the changes to disk:\n\n{goal}"),
        Some(fb) => format!(
            "[Iteration {iter}] A separate evaluator reviewed your last attempt and it did not meet \
             the quality bar. Address this feedback — refine the current direction, or pivot to a \
             different approach if it isn't working. Make the changes on disk.\n\n\
             Goal:\n{goal}\n\nEvaluator feedback:\n{fb}"
        ),
    }
}

pub(crate) fn render_feedback(verdict: &EvalVerdict) -> String {
    let mut out = String::new();
    if !verdict.verdict.is_empty() {
        out.push_str(&verdict.verdict);
        out.push('\n');
    }
    for f in &verdict.feedback {
        out.push_str("- ");
        out.push_str(f);
        out.push('\n');
    }
    if out.trim().is_empty() {
        out.push_str("No specific feedback was returned; raise quality toward the rubric.");
    }
    out
}

/// Collect the current working-tree diff as the critic's evaluation context.
pub(crate) fn collect_diff_context(workspace_root: &str) -> String {
    match collect_shipcheck(workspace_root).and_then(|snap| collect_handoff_context(&snap)) {
        Ok(Some(ctx)) => ctx.content,
        Ok(None) => "No changes detected in the working tree.".to_string(),
        Err(e) => format!("Could not collect diff context: {e}"),
    }
}

pub async fn run_iterate_loop(state: &mut AppState, opts: IterateOptions) -> Result<()> {
    if state.in_play_session() {
        anyhow::bail!("cannot /iterate during a /play session — /play exit first");
    }
    if !state.config.rubric.enabled {
        anyhow::bail!(
            "the rubric evaluator is disabled — set rubric.enabled = true to use /iterate"
        );
    }
    if should_refuse_cloud_handoff(state.backend.name, state.config.rubric.allow_cloud) {
        anyhow::bail!(
            "/iterate sends workspace context to the evaluator — run it on a local provider or set rubric.allowCloud"
        );
    }
    let max_iters = opts.max_iters.clamp(1, MAX_ITERS_CEILING);

    // A different evaluator model is the cleanest realization of the
    // generator/evaluator split; warn when they coincide.
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

    let restore = apply_iterate_mode(state);
    let mut succeeded = false;
    let mut iters_used = 0usize;
    let mut last_total = 0.0f32;
    let mut feedback: Option<String> = None;
    let mut loop_result = Ok(());

    for iter in 1..=max_iters {
        iters_used = iter;
        println!(
            "  {YELLOW}iterate{RESET} {DIM}round {iter}/{max_iters} toward {:.1}/10{RESET}",
            opts.threshold
        );
        let prompt = build_iterate_prompt(iter, &opts.goal, feedback.as_deref());
        if let Err(e) = run_user_turn(
            state,
            TurnOptions {
                user_prompt: prompt,
                auto_verify_tests: false,
                yolo_approve: opts.yolo,
                source: "iterate",
            },
        )
        .await
        {
            loop_result = Err(e);
            break;
        }

        let diff = collect_diff_context(&state.config.workspace_root);
        let target = format!("Goal:\n{}", opts.goal);
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
        last_total = verdict.weighted_total;
        // The per-run threshold (possibly set by --threshold) wins over the
        // rubric default baked into verdict.pass.
        let passed = verdict.parsed && verdict.weighted_total >= opts.threshold;

        let mark = if passed { GREEN } else { RED };
        let summary = if verdict.verdict.is_empty() {
            String::new()
        } else {
            format!(" {DIM}· {}{RESET}", verdict.verdict)
        };
        println!(
            "  {mark}eval{RESET} {DIM}{:.1}/10 (threshold {:.1}){RESET}{summary}",
            verdict.weighted_total, opts.threshold
        );

        if passed {
            succeeded = true;
            break;
        }
        feedback = Some(render_feedback(&verdict));
    }

    restore_iterate_mode(state, restore);
    loop_result?;

    if succeeded {
        println!(
            "  {GREEN}✓{RESET} {DIM}reached {last_total:.1}/10 in {iters_used} round(s) · ready for /handoff{RESET}"
        );
    } else {
        println!(
            "  {RED}✗{RESET} {DIM}stalled at {last_total:.1}/10 after {iters_used} round(s) — see feedback · /undo available{RESET}"
        );
    }
    Ok(())
}

pub fn parse_iterate_args(
    args: &str,
    default_max: usize,
    default_threshold: f32,
) -> Result<IterateOptions> {
    let mut max_iters = default_max;
    let mut threshold = default_threshold;
    let mut yolo = false;
    let mut goal_parts: Vec<&str> = Vec::new();
    let mut parts = args.split_whitespace();
    while let Some(part) = parts.next() {
        match part {
            "--yolo" => yolo = true,
            "--max" => {
                let n = parts.next().ok_or_else(|| anyhow!("missing --max value"))?;
                max_iters = n.parse().map_err(|_| anyhow!("invalid --max value"))?;
            }
            flag if flag.starts_with("--max=") => {
                let n = flag.strip_prefix("--max=").unwrap();
                max_iters = n.parse().map_err(|_| anyhow!("invalid --max value"))?;
            }
            "--threshold" => {
                let t = parts
                    .next()
                    .ok_or_else(|| anyhow!("missing --threshold value"))?;
                threshold = t
                    .parse()
                    .map_err(|_| anyhow!("invalid --threshold value"))?;
            }
            flag if flag.starts_with("--threshold=") => {
                let t = flag.strip_prefix("--threshold=").unwrap();
                threshold = t
                    .parse()
                    .map_err(|_| anyhow!("invalid --threshold value"))?;
            }
            other if other.starts_with("--") => {
                anyhow::bail!("unknown /iterate argument: {other}")
            }
            other => goal_parts.push(other),
        }
    }
    let goal = goal_parts.join(" ").trim().to_string();
    if goal.is_empty() {
        anyhow::bail!("usage: /iterate <goal> [--max N] [--threshold X] [--yolo]");
    }
    Ok(IterateOptions {
        goal,
        max_iters: max_iters.clamp(1, MAX_ITERS_CEILING),
        threshold,
        yolo,
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
    fn parse_iterate_args_extracts_goal_and_flags() {
        let opts = parse_iterate_args(
            "make the parser robust --max 4 --threshold 8.5 --yolo",
            6,
            7.0,
        )
        .unwrap();
        assert_eq!(opts.goal, "make the parser robust");
        assert_eq!(opts.max_iters, 4);
        assert!((opts.threshold - 8.5).abs() < 1e-6);
        assert!(opts.yolo);

        let eq = parse_iterate_args("polish the UI --max=3 --threshold=6", 6, 7.0).unwrap();
        assert_eq!(eq.goal, "polish the UI");
        assert_eq!(eq.max_iters, 3);
        assert!((eq.threshold - 6.0).abs() < 1e-6);
    }

    #[test]
    fn parse_iterate_args_defaults_clamp_and_errors() {
        let d = parse_iterate_args("do a thing", 6, 7.0).unwrap();
        assert_eq!(d.max_iters, 6);
        assert!((d.threshold - 7.0).abs() < 1e-6);
        // Clamp to the ceiling.
        let big = parse_iterate_args("x --max 99", 6, 7.0).unwrap();
        assert_eq!(big.max_iters, MAX_ITERS_CEILING);
        // Empty goal and unknown flag are errors.
        assert!(parse_iterate_args("--max 3", 6, 7.0).is_err());
        assert!(parse_iterate_args("goal --bogus", 6, 7.0).is_err());
        assert!(parse_iterate_args("goal --max nope", 6, 7.0).is_err());
    }

    #[test]
    fn iterate_mode_restore_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        state.config.apply_operator_mode(OperatorMode::Explore);
        let original_mode = state.config.mode;
        let original_tools = state.config.tools.clone();
        let restore = apply_iterate_mode(&mut state);
        assert_eq!(state.config.mode, OperatorMode::Ship);
        restore_iterate_mode(&mut state, restore);
        assert_eq!(state.config.mode, original_mode);
        assert_eq!(state.config.tools, original_tools);
    }

    #[tokio::test]
    async fn iterate_refuses_during_play_session() {
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
        let err = run_iterate_loop(
            &mut state,
            IterateOptions {
                goal: "x".into(),
                max_iters: 1,
                threshold: 7.0,
                yolo: true,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("/play exit"));
    }

    #[tokio::test]
    async fn iterate_refuses_cloud_without_allow_flag() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        state.backend = backend(BackendName::Openrouter);
        // rubric.allow_cloud defaults to false.
        let err = run_iterate_loop(
            &mut state,
            IterateOptions {
                goal: "x".into(),
                max_iters: 1,
                threshold: 7.0,
                yolo: true,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("allowCloud"));
    }

    #[tokio::test]
    async fn iterate_refuses_when_rubric_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        state.config.rubric.enabled = false;
        let err = run_iterate_loop(
            &mut state,
            IterateOptions {
                goal: "x".into(),
                max_iters: 1,
                threshold: 7.0,
                yolo: true,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("disabled"));
    }
}
