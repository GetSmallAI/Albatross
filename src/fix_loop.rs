use anyhow::{anyhow, Result};

use crate::app_state::{AppState, FixRestoreSnapshot};
use crate::config::{ApprovalPolicy, OperatorMode};
use crate::session_turn::{run_user_turn, TurnOptions};
use crate::test_integration::{
    format_test_failure_feedback, run_selected_tests, run_tests, smart_test_selection, TestResult,
};

const RESET: crate::theme::Style = crate::theme::RESET;
const DIM: crate::theme::Style = crate::theme::MUTED;
const GREEN: crate::theme::Style = crate::theme::SUCCESS;
const RED: crate::theme::Style = crate::theme::ERROR;
const YELLOW: crate::theme::Style = crate::theme::WARN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixTestScope {
    Smart,
    All,
}

pub struct FixOptions {
    pub scope: FixTestScope,
    pub max_attempts: usize,
    pub yolo: bool,
}

fn tests_passing(result: &TestResult) -> bool {
    result.failed == 0 && result.exit_code == 0
}

fn run_fix_tests(state: &AppState, scope: FixTestScope) -> Result<TestResult> {
    match scope {
        FixTestScope::All => run_tests(&state.config.workspace_root, None),
        FixTestScope::Smart => {
            let selected = smart_test_selection(&state.config.workspace_root)?;
            if selected.is_empty() {
                run_tests(&state.config.workspace_root, None)
            } else {
                run_selected_tests(&state.config.workspace_root, &selected)
            }
        }
    }
}

fn apply_fix_mode(state: &mut AppState) -> FixRestoreSnapshot {
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

pub fn restore_fix_mode(state: &mut AppState, restore: FixRestoreSnapshot) {
    state.config = restore.config;
    state.checkpoints_enabled = restore.checkpoints_enabled;
}

fn build_fix_prompt(attempt: usize, result: &TestResult) -> String {
    let feedback = format_test_failure_feedback(result);
    if attempt == 1 {
        format!("The tests are failing. Fix the code so all selected tests pass.\n\n{feedback}")
    } else {
        format!("[Fix attempt {attempt}] Tests still failing. Continue fixing.\n\n{feedback}")
    }
}

pub async fn run_fix_loop(state: &mut AppState, opts: FixOptions) -> Result<()> {
    if state.in_play_session() {
        anyhow::bail!("cannot /fix during a /play session — /play exit first");
    }
    if opts.max_attempts == 0 {
        anyhow::bail!("max attempts must be at least 1");
    }

    let initial = run_fix_tests(state, opts.scope)?;
    if tests_passing(&initial) {
        println!("  {GREEN}✓{RESET} {DIM}Nothing to fix — all tests pass.{RESET}");
        return Ok(());
    }

    let restore = apply_fix_mode(state);
    let mut last_result = initial;
    let mut succeeded = false;
    let mut attempts_used = 0usize;
    let mut loop_result = Ok(());

    for attempt in 1..=opts.max_attempts {
        attempts_used = attempt;
        println!(
            "  {YELLOW}fix{RESET} {DIM}attempt {attempt}/{} — {} failed{RESET}",
            opts.max_attempts, last_result.failed
        );
        let prompt = build_fix_prompt(attempt, &last_result);
        if let Err(e) = run_user_turn(
            state,
            TurnOptions {
                user_prompt: prompt,
                auto_verify_tests: false,
                yolo_approve: opts.yolo,
                source: "fix",
            },
        )
        .await
        {
            loop_result = Err(e);
            break;
        }

        match run_fix_tests(state, opts.scope) {
            Ok(result) => last_result = result,
            Err(e) => {
                loop_result = Err(e);
                break;
            }
        }
        if tests_passing(&last_result) {
            succeeded = true;
            break;
        }
    }

    restore_fix_mode(state, restore);
    loop_result?;

    if succeeded {
        println!(
            "  {GREEN}✓{RESET} {DIM}fixed in {attempts_used} attempt(s) · ready for /handoff{RESET}"
        );
    } else {
        println!(
            "  {RED}✗{RESET} {DIM}still failing after {attempts_used} attempt(s) — see context · /undo available{RESET}"
        );
    }
    Ok(())
}

pub fn parse_fix_args(args: &str, default_attempts: usize) -> Result<FixOptions> {
    let mut scope = FixTestScope::Smart;
    let mut max_attempts = default_attempts;
    let mut yolo = false;
    let mut parts = args.split_whitespace();
    while let Some(part) = parts.next() {
        match part {
            "all" => scope = FixTestScope::All,
            "smart" => scope = FixTestScope::Smart,
            "--yolo" => yolo = true,
            "--attempts" => {
                let n = parts
                    .next()
                    .ok_or_else(|| anyhow!("missing --attempts value"))?;
                max_attempts = n.parse().map_err(|_| anyhow!("invalid --attempts value"))?;
            }
            flag if flag.starts_with("--attempts=") => {
                let n = flag
                    .strip_prefix("--attempts=")
                    .ok_or_else(|| anyhow!("invalid --attempts flag"))?;
                max_attempts = n.parse().map_err(|_| anyhow!("invalid --attempts value"))?;
            }
            other if other.starts_with("--attempts") => {
                anyhow::bail!("use --attempts N or --attempts=N");
            }
            "" => {}
            other => anyhow::bail!("unknown /fix argument: {other}"),
        }
    }
    Ok(FixOptions {
        scope,
        max_attempts,
        yolo,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::ApprovalCache;
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
    fn fix_loop_respects_max_attempts_config() {
        let opts = parse_fix_args("--attempts=3", 5).unwrap();
        assert_eq!(opts.max_attempts, 3);
        let opts = parse_fix_args("--attempts 4", 5).unwrap();
        assert_eq!(opts.max_attempts, 4);
        let opts = parse_fix_args("all --yolo", 5).unwrap();
        assert_eq!(opts.scope, FixTestScope::All);
        assert!(opts.yolo);
    }

    #[test]
    fn fix_refuses_during_play_session() {
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
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(async {
                run_fix_loop(
                    &mut state,
                    FixOptions {
                        scope: FixTestScope::Smart,
                        max_attempts: 1,
                        yolo: true,
                    },
                )
                .await
            })
            .unwrap_err();
        assert!(err.to_string().contains("/play exit"));
    }

    #[test]
    fn fix_mode_restore_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path());
        state.config.apply_operator_mode(OperatorMode::Explore);
        let original_tools = state.config.tools.clone();
        let original_tool_selection = state.config.tool_selection;
        let original_approval = state.config.approval_policy;
        let original_max_steps = state.config.max_steps;
        let original_checkpoint_config = state.config.checkpoints.enabled;
        state.checkpoints_enabled = false;
        let restore = apply_fix_mode(&mut state);
        assert_eq!(state.config.mode, OperatorMode::Ship);
        assert!(state.checkpoints_enabled);
        restore_fix_mode(&mut state, restore);
        assert_eq!(state.config.mode, OperatorMode::Explore);
        assert_eq!(state.config.tools, original_tools);
        assert_eq!(state.config.tool_selection, original_tool_selection);
        assert_eq!(state.config.approval_policy, original_approval);
        assert_eq!(state.config.max_steps, original_max_steps);
        assert_eq!(state.config.checkpoints.enabled, original_checkpoint_config);
        assert!(!state.checkpoints_enabled);
    }

    #[test]
    fn fix_loop_stops_when_tests_pass() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname=\"green\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\n#[cfg(test)]\nmod tests { use super::*; #[test] fn add_works() { assert_eq!(add(2, 3), 5); } }\n",
        )
        .unwrap();
        let mut state = test_state(dir.path());
        let original_mode = state.config.mode;
        let rt = tokio::runtime::Runtime::new().unwrap();

        rt.block_on(async {
            run_fix_loop(
                &mut state,
                FixOptions {
                    scope: FixTestScope::All,
                    max_attempts: 3,
                    yolo: true,
                },
            )
            .await
            .unwrap();
        });

        assert_eq!(state.config.mode, original_mode);
        assert!(state.messages.is_empty());
    }

    #[test]
    fn tests_passing_helper() {
        assert!(tests_passing(&TestResult {
            failed: 0,
            exit_code: 0,
            ..Default::default()
        }));
        assert!(!tests_passing(&TestResult {
            failed: 1,
            exit_code: 1,
            ..Default::default()
        }));
    }
}
