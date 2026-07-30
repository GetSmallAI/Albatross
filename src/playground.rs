use anyhow::{anyhow, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::agent_eval::{
    count_assistant_steps, evaluate_checks, fixture_by_id, init_git_if_needed,
    prepare_playground_workspace, render_agent_eval_markdown, run_agent_eval, AgentEvalFixture,
    AgentEvalRunResult,
};
use crate::app_state::{AppState, PlayRestoreSnapshot, PlayScorecard, PlaySession};
use crate::backends::{backend, validate, BackendDescriptor, BackendName};
use crate::config::OperatorMode;
use crate::session_turn::{run_user_turn, TurnOptions};

const RESET: crate::theme::Style = crate::theme::RESET;
const DIM: crate::theme::Style = crate::theme::MUTED;
const BOLD: crate::theme::Style = crate::theme::BOLD;
const CYAN: crate::theme::Style = crate::theme::ACCENT;
const GREEN: crate::theme::Style = crate::theme::SUCCESS;
const RED: crate::theme::Style = crate::theme::ERROR;
const YELLOW: crate::theme::Style = crate::theme::WARN;

pub fn list_play_fixtures() -> Vec<&'static str> {
    vec![
        "fix-failing-test",
        "small-refactor",
        "add-feature",
        "read-and-explain",
    ]
}

pub fn print_play_list() {
    println!("  {BOLD}Playground scenarios{RESET} {DIM}(bundled demos — no repo setup){RESET}");
    println!();
    for id in list_play_fixtures() {
        if let Some(fixture) = fixture_by_id(id) {
            let pitch = fixture.prompt.chars().take(72).collect::<String>();
            println!("  {CYAN}{id:<20}{RESET} {DIM}{pitch}{RESET}");
        }
    }
    println!();
    println!(
        "  {DIM}Usage: /play <fixture> [--yolo] · /play battle <fixture> <model[,model]> · /play exit · /play score{RESET}"
    );
}

pub fn restore_play_session(state: &mut AppState) -> Result<()> {
    let Some(session) = state.play_session.take() else {
        anyhow::bail!("not in a play session — use /play <fixture> first");
    };
    let restore = session.restore;
    state.config = restore.config;
    state.checkpoints_enabled = restore.checkpoints_enabled;
    Ok(())
}

pub fn start_play_session(state: &mut AppState, fixture_id: &str) -> Result<AgentEvalFixture> {
    if state.play_session.is_some() {
        anyhow::bail!("already in a play session — /play exit first");
    }
    let fixture =
        fixture_by_id(fixture_id).ok_or_else(|| anyhow!("unknown play fixture: {fixture_id}"))?;
    let sandbox = prepare_playground_workspace(&state.session_dir, fixture_id, &fixture)?;
    init_git_if_needed(&sandbox)?;
    let restore = PlayRestoreSnapshot {
        config: state.config.clone(),
        checkpoints_enabled: state.checkpoints_enabled,
    };
    state.config.workspace_root = sandbox.to_string_lossy().into_owned();
    state.config.apply_operator_mode(OperatorMode::Ship);
    state.checkpoints_enabled = state.config.checkpoints.enabled;
    state.play_session = Some(PlaySession {
        fixture_id: fixture_id.to_string(),
        sandbox_root: sandbox,
        restore,
    });
    Ok(fixture)
}

pub fn print_scorecard(_state: &AppState, scorecard: &PlayScorecard) {
    println!();
    if scorecard.passed {
        println!(
            "  {GREEN}✓{RESET} {BOLD}PLAY COMPLETE{RESET} {DIM}— {}{RESET}",
            scorecard.fixture_id
        );
    } else {
        println!(
            "  {RED}✗{RESET} {BOLD}PLAY FINISHED{RESET} {DIM}— {}{RESET}",
            scorecard.fixture_id
        );
    }
    for check in &scorecard.checks {
        let label = match &check.check {
            crate::agent_eval::AgentEvalCheck::TestsPass => "TestsPass".to_string(),
            crate::agent_eval::AgentEvalCheck::FileContains { path, .. } => {
                format!("FileContains({path})")
            }
            other => format!("{other:?}"),
        };
        let mark = if check.passed { GREEN } else { RED };
        println!(
            "  {mark}{}{RESET} {label:<22} {DIM}{}{RESET}",
            if check.passed { "✓" } else { "✗" },
            check.detail
        );
    }
    println!(
        "  {DIM}Time{RESET}               {:.1}s · {} steps · {} tool calls",
        scorecard.elapsed_ms as f64 / 1000.0,
        scorecard.steps,
        scorecard.tool_calls.len()
    );
    println!("  {DIM}Try on your repo{RESET}   /fix");
    if scorecard.passed {
        println!(
            "  {DIM}Share line{RESET}         \"{} fixed {} locally in {:.0}s\"",
            scorecard.model,
            scorecard.fixture_id,
            scorecard.elapsed_ms as f64 / 1000.0
        );
    }
    println!();
}

fn build_scorecard(
    fixture_id: &str,
    model: &str,
    elapsed_ms: u128,
    steps: usize,
    tool_calls: Vec<String>,
    checks: Vec<crate::agent_eval::AgentEvalCheckResult>,
) -> PlayScorecard {
    let passed = checks.iter().all(|c| c.passed);
    PlayScorecard {
        fixture_id: fixture_id.to_string(),
        model: model.to_string(),
        elapsed_ms,
        steps,
        tool_calls,
        checks,
        passed,
    }
}

pub async fn run_play_fixture(state: &mut AppState, fixture_id: &str, yolo: bool) -> Result<()> {
    let fixture = start_play_session(state, fixture_id)?;
    println!(
        "  {CYAN}▶{RESET} {BOLD}Playing:{RESET} {fixture_id} {DIM}· {}{RESET} {DIM}· /play exit to return{RESET}",
        state.model
    );
    let sandbox = state
        .play_session
        .as_ref()
        .map(|s| s.sandbox_root.clone())
        .unwrap();
    let start = Instant::now();
    let outcome = run_user_turn(
        state,
        TurnOptions {
            user_prompt: fixture.prompt.clone(),
            auto_verify_tests: true,
            yolo_approve: yolo,
            source: "play",
        },
    )
    .await?;
    let elapsed_ms = start.elapsed().as_millis();
    let steps = count_assistant_steps(&outcome.run_result.messages);
    let checks = evaluate_checks(
        &sandbox,
        &fixture.checks,
        &outcome.run_result,
        &outcome.tool_calls,
    );
    let scorecard = build_scorecard(
        fixture_id,
        &state.model,
        elapsed_ms,
        steps,
        outcome.tool_calls,
        checks,
    );
    print_scorecard(state, &scorecard);
    state.last_play_scorecard = Some(scorecard);
    Ok(())
}

pub fn parse_play_model(spec: &str, state: &AppState) -> (BackendDescriptor, String) {
    if let Some((prefix, model)) = spec.split_once(':') {
        if let Some(name) = BackendName::parse(prefix) {
            return (backend(name), model.to_string());
        }
    }
    (state.backend.clone(), spec.to_string())
}

fn save_battle_results(
    session_dir: &str,
    results: &[AgentEvalRunResult],
) -> Result<(PathBuf, PathBuf)> {
    let battle_dir = Path::new(session_dir).join("play");
    fs::create_dir_all(&battle_dir)?;
    let id = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S-%3fZ");
    let json_path = battle_dir.join(format!("battle-{id}.json"));
    let md_path = battle_dir.join(format!("battle-{id}.md"));
    fs::write(&json_path, serde_json::to_string_pretty(results)?)?;
    fs::write(&md_path, render_agent_eval_markdown(results))?;
    Ok((json_path, md_path))
}

pub async fn run_play_battle(
    state: &AppState,
    fixture_id: &str,
    model_specs: &[String],
) -> Result<Vec<AgentEvalRunResult>> {
    let fixture =
        fixture_by_id(fixture_id).ok_or_else(|| anyhow!("unknown play fixture: {fixture_id}"))?;
    let mut results = Vec::new();
    for spec in model_specs {
        let (backend_desc, model) = parse_play_model(spec, state);
        validate(&backend_desc)?;
        println!(
            "  {YELLOW}⚔{RESET} {DIM}battle · {} · {}{RESET}",
            backend_desc.name.as_str(),
            model
        );
        let result = run_agent_eval(&state.config, &backend_desc, &model, &fixture).await?;
        println!(
            "  {} {}{RESET}",
            if result.passed {
                format!("{GREEN}✓{RESET}")
            } else {
                format!("{RED}✗{RESET}")
            },
            if result.passed { "passed" } else { "failed" }
        );
        results.push(result);
    }

    println!();
    println!(
        "  {BOLD}{:<24} {:<8} {:<8} {:<10} tools{RESET}",
        "model", "passed", "steps", "latency"
    );
    for result in &results {
        println!(
            "  {:<24} {:<8} {:<8} {:<10} {}",
            result.model,
            if result.passed { "yes" } else { "no" },
            result.steps,
            result.elapsed_ms,
            result.tool_calls.len()
        );
    }
    println!();

    let (json_path, md_path) = save_battle_results(&state.session_dir, &results)?;
    println!(
        "  {GREEN}✓{RESET} {DIM}battle saved → {} and {}{RESET}",
        json_path.display(),
        md_path.display()
    );
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::ApprovalCache;
    use crate::config::AgentConfig;
    use crate::renderer::TuiRenderer;
    use crate::session_paths::PathStore;
    use crate::turn_checkpoint::CheckpointStack;

    fn test_state(root: &Path) -> AppState {
        let mut config = AgentConfig {
            workspace_root: root.display().to_string(),
            session_dir: root.join(".sessions").display().to_string(),
            ..Default::default()
        };
        config.apply_operator_mode(OperatorMode::Explore);
        let session_path = root.join(".sessions/test.jsonl");
        AppState {
            http: reqwest::Client::new(),
            backend: backend(config.backend),
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
    fn play_session_restore_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        let original_root = state.config.workspace_root.clone();
        let original_mode = state.config.mode;
        let original_tools = state.config.tools.clone();
        let original_tool_selection = state.config.tool_selection;
        let original_approval = state.config.approval_policy;
        let original_max_steps = state.config.max_steps;
        let original_checkpoint_config = state.config.checkpoints.enabled;
        state.play_session = Some(PlaySession {
            fixture_id: "demo".into(),
            sandbox_root: dir.path().join("sandbox"),
            restore: PlayRestoreSnapshot {
                config: state.config.clone(),
                checkpoints_enabled: true,
            },
        });
        state.config.workspace_root = dir.path().join("sandbox").display().to_string();
        state.config.apply_operator_mode(OperatorMode::Ship);
        state.checkpoints_enabled = false;

        restore_play_session(&mut state).unwrap();
        assert_eq!(state.config.workspace_root, original_root);
        assert_eq!(state.config.mode, original_mode);
        assert_eq!(state.config.tools, original_tools);
        assert_eq!(state.config.tool_selection, original_tool_selection);
        assert_eq!(state.config.approval_policy, original_approval);
        assert_eq!(state.config.max_steps, original_max_steps);
        assert_eq!(state.config.checkpoints.enabled, original_checkpoint_config);
        assert!(state.checkpoints_enabled);
        assert!(state.play_session.is_none());
    }

    #[test]
    fn play_refuses_while_active() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        state.play_session = Some(PlaySession {
            fixture_id: "demo".into(),
            sandbox_root: PathBuf::from("/tmp/demo"),
            restore: PlayRestoreSnapshot {
                config: state.config.clone(),
                checkpoints_enabled: true,
            },
        });
        let err = start_play_session(&mut state, "fix-failing-test").unwrap_err();
        assert!(err.to_string().contains("/play exit"));
    }

    #[test]
    fn play_battle_saves_json_and_markdown() {
        let dir = tempfile::tempdir().unwrap();
        let results = vec![AgentEvalRunResult {
            fixture_id: "fix-failing-test".into(),
            model: "model-a".into(),
            backend: "ollama".into(),
            passed: true,
            checks: Vec::new(),
            elapsed_ms: 42,
            steps: 2,
            hit_step_limit: false,
            tool_calls: vec!["file_edit".into()],
            input_tokens: 10,
            output_tokens: 20,
            transcript_path: "session.jsonl".into(),
            error: None,
        }];

        let (json_path, md_path) = save_battle_results(dir.path().to_str().unwrap(), &results)
            .expect("save battle results");

        let json = fs::read_to_string(json_path).unwrap();
        let md = fs::read_to_string(md_path).unwrap();
        assert!(json.contains("\"model\": \"model-a\""));
        assert!(md.contains("| model-a | fix-failing-test | yes | 2 | 42 | file_edit |"));
    }
}
