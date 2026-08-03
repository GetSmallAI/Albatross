//! The `critique` evaluator: a separate, read-only adversarial critic.
//!
//! Cloned from [`crate::tools::subagent`] — a fresh, bounded, read-only
//! `run_agent` loop whose internal tool calls never reach the parent. The
//! difference is the contract: instead of returning prose, the critic scores
//! the work against the configured [`crate::rubric::Rubric`] and we parse a
//! structured [`EvalVerdict`]. This realizes the article's generator/evaluator
//! separation — a different agent (and optionally a different model) judges the
//! work than the one that produced it.
//!
//! [`run_evaluation`] is the callable both this tool and the `/iterate` loop
//! use, so the loop can grade without a parent-context round-trip.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use super::verify::VerifyTool;
use super::{build_tools_for_names, Tool};
use crate::agent::run_agent;
use crate::backends::BackendDescriptor;
use crate::cancel::CancellationToken;
use crate::config::AgentConfig;
use crate::handoff::should_refuse_cloud_handoff;
use crate::openai::ChatMessage;
use crate::rubric::{load_rubric, parse_verdict, EvalVerdict, Rubric};

/// Hard cap on the critic's own loop, like the subagent — keeps an independent
/// grading pass bounded and cheap regardless of the parent's `max_steps`.
const EVALUATOR_MAX_STEPS: usize = 12;

/// A separate read-only critic agent that grades work against the rubric.
pub struct EvaluatorTool {
    pub http: reqwest::Client,
    pub backend: BackendDescriptor,
    pub model: String,
    pub config: AgentConfig,
    pub runtime: Option<super::ToolRuntimeContext>,
}

#[derive(Deserialize)]
struct Args {
    target: String,
    #[serde(default)]
    context: Option<String>,
}

/// Read-only tool names the critic may use. Never includes mutating, shell, the
/// subagent, or the critic itself.
fn evaluator_tool_names(config: &AgentConfig) -> Vec<String> {
    let mut names = vec![
        "file_read".to_string(),
        "grep".to_string(),
        "list_dir".to_string(),
        "glob".to_string(),
    ];
    if config.project_memory.enabled {
        names.push("repo_search".to_string());
    }
    names
}

fn evaluator_system_prompt(rubric: &Rubric, cwd: &str, live_verify: bool) -> String {
    let mut criteria = String::new();
    for c in &rubric.criteria {
        criteria.push_str(&format!(
            "- {} (weight {}): {}\n",
            c.name, c.weight, c.description
        ));
    }
    let mut prompt = String::new();
    prompt.push_str(
        "You are an adversarial quality evaluator. A SEPARATE agent produced the work under review.\n\
         Judge it honestly and skeptically — never praise it by default. Models tend to over-rate\n\
         work; resist that.\n\n\
         You have READ-ONLY tools (read files, grep, list, glob, search). Inspect the ACTUAL work in\n\
         the workspace before scoring. You cannot edit files or run commands — do not try.\n\n\
         Score EACH criterion from 0 to 10 (10 = excellent, 0 = absent or broken):\n",
    );
    prompt.push_str(&criteria);
    prompt.push_str(
        "\nPenalize generic, templated, boilerplate \"AI slop\". Reward specific, deliberate, well-crafted work.\n",
    );
    if let Some(guidance) = &rubric.guidance {
        prompt.push_str("\nProject-specific rubric guidance:\n");
        prompt.push_str(guidance);
        prompt.push('\n');
    }
    if live_verify {
        prompt.push_str(
            "\nBefore scoring Functionality, call the `verify` tool to actually run the project's \
             tests, and base that score on the real pass/fail result — do not assume it works.\n",
        );
    }
    prompt.push_str(
        "\nReturn ONLY a single JSON object — no prose, no markdown fences — in exactly this shape:\n\
         {\"scores\":[{\"name\":\"<criterion>\",\"score\":<0-10>,\"justification\":\"<why>\"}],\
         \"feedback\":[\"<concrete, actionable improvement>\"],\"verdict\":\"<one-sentence overall judgment>\"}\n\
         Include one entry in \"scores\" per criterion above, using the exact criterion names.\n\
         Do NOT include a total or pass/fail — that is computed for you.\n\n",
    );
    prompt.push_str(&format!("Current working directory: {cwd}"));
    prompt
}

fn render_evaluator_prompt(target: &str, context: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("Evaluate the following work against the criteria in your instructions.\n\n");
    out.push_str(target.trim());
    if let Some(ctx) = context.filter(|c| !c.trim().is_empty()) {
        out.push_str("\n\nAdditional context:\n");
        out.push_str(ctx.trim());
    }
    out
}

/// Run a fresh, read-only critic agent and return its parsed verdict. Refuses to
/// send workspace context to a cloud backend unless `rubric.allowCloud` is set.
#[allow(clippy::too_many_arguments)]
pub async fn run_evaluation(
    http: &reqwest::Client,
    backend: &BackendDescriptor,
    model: &str,
    config: &AgentConfig,
    target: &str,
    context: Option<&str>,
    cancel: Option<CancellationToken>,
    runtime: Option<super::ToolRuntimeContext>,
) -> EvalVerdict {
    if should_refuse_cloud_handoff(backend.name, config.rubric.allow_cloud) {
        return EvalVerdict::failed(
            "evaluator will not send workspace context to a cloud provider without rubric.allowCloud",
        );
    }
    let rubric = load_rubric(
        &config.workspace_root,
        config.rubric.pass_threshold,
        config.rubric.rubric_path.as_deref(),
    );
    let system =
        evaluator_system_prompt(&rubric, &config.workspace_root, config.rubric.live_verify);
    let user = render_evaluator_prompt(target, context);
    let initial = vec![
        ChatMessage::System { content: system },
        ChatMessage::User {
            content: user.into(),
        },
    ];
    let mut tools = build_tools_for_names(config, &evaluator_tool_names(config), None);
    if config.rubric.live_verify {
        // A fixed-surface test runner the read-only critic may call without an
        // approval gate (see VerifyTool); bounded by a timeout.
        tools.push(Arc::new(VerifyTool {
            workspace_root: config.workspace_root.clone(),
            timeout: config.rubric.verify_timeout(),
        }));
    }

    let trace = runtime.as_ref().map(|r| r.trace.clone());
    let trace_enabled = runtime.as_ref().map(|r| r.trace_enabled).unwrap_or(false);
    let event_tx = runtime.as_ref().and_then(|r| r.agent_events.clone());
    let hooks = runtime.as_ref().and_then(|r| r.hooks.clone());

    let result = run_agent(
        http,
        backend,
        model,
        None,
        initial,
        tools,
        EVALUATOR_MAX_STEPS,
        |event| {
            if trace_enabled {
                if let Some(tx) = &event_tx {
                    let _ = tx.send(crate::tools::subagent::forward_subagent_event(event));
                }
            }
        },
        None, // no approval provider => any mutating tool would be denied
        cancel,
        None,
        None,
        trace,
        1,
        hooks,
    )
    .await;

    match result {
        Ok(run) => {
            let text = run
                .messages
                .iter()
                .rev()
                .find_map(|m| match m {
                    ChatMessage::Assistant {
                        content: Some(c), ..
                    } if !c.trim().is_empty() => Some(c.trim().to_string()),
                    _ => None,
                })
                .unwrap_or_default();
            parse_verdict(&text, &rubric)
        }
        Err(e) => EvalVerdict::failed(&format!("evaluator failed: {e}")),
    }
}

#[async_trait]
impl Tool for EvaluatorTool {
    fn name(&self) -> &'static str {
        "critique"
    }
    fn description(&self) -> &'static str {
        "Delegate an adversarial quality evaluation to a separate read-only critic agent. It inspects the work in the workspace, scores it 0-10 against the configured rubric, and returns a structured verdict (per-criterion scores, weighted total, pass/fail, actionable feedback). The critic cannot edit files or run commands. Use it for an independent grade of completed work."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "What work to evaluate and where to find it (e.g. \"the CSV export feature added to src/export.rs\")."
                },
                "context": {
                    "type": "string",
                    "description": "Optional extra context: the goal, a diff, or constraints."
                }
            },
            "required": ["target"]
        })
    }
    async fn execute(&self, args: Value) -> Value {
        self.execute_cancelable(args, None).await
    }
    async fn execute_cancelable(&self, args: Value, cancel: Option<CancellationToken>) -> Value {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return json!({ "error": format!("invalid args: {e}") }),
        };
        if args.target.trim().is_empty() {
            return json!({ "error": "target must not be empty" });
        }
        let verdict = run_evaluation(
            &self.http,
            &self.backend,
            &self.model,
            &self.config,
            &args.target,
            args.context.as_deref(),
            cancel,
            self.runtime.clone(),
        )
        .await;
        serde_json::to_value(verdict)
            .unwrap_or_else(|e| json!({ "error": format!("failed to serialize verdict: {e}") }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{backend, BackendName};

    fn tool() -> EvaluatorTool {
        EvaluatorTool {
            http: reqwest::Client::new(),
            backend: backend(BackendName::Ollama),
            model: "test".into(),
            config: AgentConfig::default(),
            runtime: None,
        }
    }

    #[test]
    fn evaluator_tools_are_read_only_and_exclude_self() {
        let names = evaluator_tool_names(&AgentConfig::default());
        assert!(names.contains(&"file_read".to_string()));
        assert!(!names.contains(&"critique".to_string()));
        assert!(!names.contains(&"task".to_string()));
        assert!(!names.contains(&"shell".to_string()));
        assert!(!names.contains(&"file_edit".to_string()));
    }

    #[test]
    fn system_prompt_lists_criteria_and_demands_json() {
        let rubric = crate::rubric::default_rubric(7.0);
        let prompt = evaluator_system_prompt(&rubric, "/tmp", false);
        assert!(prompt.contains("Originality"));
        assert!(prompt.contains("AI slop"));
        assert!(prompt.contains("single JSON object"));
        assert!(prompt.contains("/tmp"));
        // Without live_verify the critic is not told to run anything.
        assert!(!prompt.contains("`verify`"));
    }

    #[test]
    fn system_prompt_requests_verify_when_live_verify_on() {
        let rubric = crate::rubric::default_rubric(7.0);
        let prompt = evaluator_system_prompt(&rubric, "/tmp", true);
        assert!(prompt.contains("`verify`"));
        assert!(prompt.contains("Functionality"));
    }

    #[tokio::test]
    async fn empty_target_is_an_error() {
        let out = tool().execute(json!({ "target": "  " })).await;
        assert!(out.get("error").is_some());
    }

    #[tokio::test]
    async fn refuses_cloud_without_allow_flag() {
        let config = AgentConfig::default(); // rubric.allow_cloud = false
        let cloud = backend(BackendName::Openrouter);
        let verdict = run_evaluation(
            &reqwest::Client::new(),
            &cloud,
            "model",
            &config,
            "some work",
            None,
            None,
            None,
        )
        .await;
        assert!(!verdict.pass);
        assert!(!verdict.parsed);
        assert!(verdict.feedback.iter().any(|f| f.contains("allowCloud")));
    }
}
