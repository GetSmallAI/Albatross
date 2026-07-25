use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::cancel::CancellationToken;
use crate::config::{AgentConfig, ApprovalPolicy, ToolSelection};
use crate::turn_trace::SharedTurnTrace;

mod apply_patch_tool;
mod batch_edit;
pub mod diff;
mod evaluator;
mod file_edit;
mod file_read;
mod file_write;
mod glob_tool;
mod grep;
mod list_dir;
mod path_policy;
mod repo_search;
mod run_tests;
mod shell;
mod ship_status;
mod subagent;
mod update_plan;
mod verify;
mod web_fetch;

pub use apply_patch_tool::{patch_changed_files, ApplyPatchTool};
pub use batch_edit::BatchEditTool;
pub use diff::unified_diff;
pub use evaluator::{run_evaluation, EvaluatorTool};
pub use file_edit::FileEditTool;
pub use file_read::FileReadTool;
pub use file_write::FileWriteTool;
pub use glob_tool::GlobTool;
pub use grep::GrepTool;
pub use list_dir::ListDirTool;
pub use path_policy::PathPolicy;
pub use repo_search::RepoSearchTool;
pub use run_tests::RunTestsTool;
pub use shell::ShellTool;
pub use ship_status::ShipStatusTool;
pub use subagent::SubagentTool;
pub use update_plan::UpdatePlanTool;
pub use web_fetch::WebFetchTool;

/// Per-turn runtime handles passed into tools that spawn nested agent loops.
#[derive(Clone)]
pub struct ToolRuntimeContext {
    pub trace: SharedTurnTrace,
    pub trace_enabled: bool,
    pub agent_events: Option<tokio::sync::mpsc::UnboundedSender<crate::agent::AgentEvent>>,
    pub hooks: Option<crate::agent::AgentHooks>,
}

/// Base64-encode raw bytes for use in a data URL (e.g. `data:image/png;base64,...`).
/// Re-exported from `file_read` so callers outside the tools module (like
/// `/image`) don't have to duplicate the implementation.
pub fn image_base64_for_data_url(bytes: &[u8]) -> String {
    file_read::b64_encode(bytes)
}

#[derive(Debug, Clone)]
pub struct ToolPreview {
    pub summary: String,
    pub diff: Option<String>,
    pub risk: Option<String>,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> Value;
    fn require_approval(&self, _args: &Value) -> bool {
        false
    }
    async fn preview(&self, _args: &Value) -> Option<ToolPreview> {
        None
    }
    async fn execute(&self, args: Value) -> Value;
    async fn execute_cancelable(&self, args: Value, _cancel: Option<CancellationToken>) -> Value {
        self.execute(args).await
    }
}

/// Choose the tools to expose for a given prompt.
///
/// `Fixed` always sends the whole configured pool. `Auto` (the default) keeps
/// the full working set available for any real request and sends *no* tools
/// only for obvious small talk (greetings, thanks) — the model still decides
/// whether to call them. This mirrors how modern harnesses work: the agent
/// always has read/edit/write/shell on hand, so "build me a site" results in
/// files written to disk instead of code dumped into the chat. The old
/// keyword-bucket heuristic guessed wrong on phrasings like "build a bio site"
/// (no edit tools surfaced), which forced the model to print code it couldn't
/// save.
pub fn select_tool_names(config: &AgentConfig, prompt: &str) -> Vec<String> {
    if config.tool_selection == ToolSelection::Fixed || !is_small_talk(prompt) {
        return config.tools.clone();
    }
    Vec::new()
}

/// Conservatively detect throwaway social messages (greetings, thanks) so we
/// can skip sending tool schemas for them. Deliberately narrow: a false
/// negative just sends tools the model won't use, but a false positive would
/// starve a real request of its tools, so we only match very short, clearly
/// social inputs.
fn is_small_talk(prompt: &str) -> bool {
    let t = prompt.trim().to_lowercase();
    let t = t.trim_end_matches(['.', '!', '?', ' ']);
    if t.is_empty() {
        return true;
    }
    const EXACT: &[&str] = &[
        "hi",
        "hii",
        "hey",
        "hello",
        "yo",
        "sup",
        "thanks",
        "thank you",
        "thx",
        "ty",
        "ok",
        "okay",
        "k",
        "cool",
        "nice",
        "great",
        "awesome",
        "hi there",
        "hey there",
        "hello there",
        "good morning",
        "good afternoon",
        "good evening",
        "how are you",
        "how's it going",
        "who are you",
        "what are you",
        "what can you do",
        "gm",
        "bye",
        "goodbye",
    ];
    EXACT.contains(&t)
}

/// Returns true when a tool result indicates the workspace was mutated (for ship-loop hooks).
pub fn tool_output_mutated_workspace(tool_name: &str, output: &str) -> bool {
    let Ok(output_json) = serde_json::from_str::<Value>(output) else {
        return false;
    };
    if output_json.get("error").is_some() {
        return false;
    }
    match tool_name {
        "file_write" => output_json
            .get("written")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "file_edit" => output_json
            .get("edited")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "apply_patch" | "batch_edit" => output_json
            .get("applied")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        _ => false,
    }
}

pub fn mutation_tool_names() -> &'static [&'static str] {
    &["file_write", "file_edit", "apply_patch", "batch_edit"]
}

pub fn is_mutation_tool(name: &str) -> bool {
    mutation_tool_names().contains(&name)
}

/// Built-in tools that do not mutate the workspace, so the agent loop can
/// safely run several of them concurrently within a single step.
///
/// Includes network-only `web_fetch` (no workspace writes). Excludes `shell`
/// and `run_tests` (arbitrary commands), mutating file tools, and MCP tools
/// (unknown side effects).
pub fn read_only_tool_names() -> &'static [&'static str] {
    &[
        "file_read",
        "grep",
        "list_dir",
        "glob",
        "repo_search",
        "ship_status",
        "web_fetch",
    ]
}

pub fn is_read_only_tool(name: &str) -> bool {
    read_only_tool_names().contains(&name)
}

pub fn build_tools_for_names(
    config: &AgentConfig,
    names: &[String],
    runtime: Option<&ToolRuntimeContext>,
) -> Vec<Arc<dyn Tool>> {
    let approve_writes = config.approval_policy != ApprovalPolicy::Never;
    let path_policy = PathPolicy::new(&config.workspace_root, config.outside_workspace);
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();
    for name in names {
        let t: Option<Arc<dyn Tool>> = match name.as_str() {
            "apply_patch" => Some(Arc::new(ApplyPatchTool {
                approve: approve_writes,
                path_policy: path_policy.clone(),
            })),
            "file_read" => Some(Arc::new(FileReadTool {
                path_policy: path_policy.clone(),
            })),
            "file_write" => Some(Arc::new(FileWriteTool {
                approve: approve_writes,
                path_policy: path_policy.clone(),
            })),
            "file_edit" => Some(Arc::new(FileEditTool {
                approve: approve_writes,
                path_policy: path_policy.clone(),
            })),
            "glob" => Some(Arc::new(GlobTool {
                path_policy: path_policy.clone(),
            })),
            "grep" => Some(Arc::new(GrepTool {
                path_policy: path_policy.clone(),
            })),
            "list_dir" => Some(Arc::new(ListDirTool {
                path_policy: path_policy.clone(),
            })),
            "repo_search" => Some(Arc::new(RepoSearchTool {
                config: config.clone(),
            })),
            "shell" => Some(Arc::new(ShellTool {
                policy: config.approval_policy,
                path_policy: path_policy.clone(),
            })),
            "run_tests" => Some(Arc::new(RunTestsTool {
                workspace_root: config.workspace_root.clone(),
                policy: config.approval_policy,
            })),
            "batch_edit" => Some(Arc::new(BatchEditTool {
                workspace_root: config.workspace_root.clone(),
            })),
            "ship_status" => Some(Arc::new(ShipStatusTool {
                workspace_root: config.workspace_root.clone(),
            })),
            "update_plan" => Some(Arc::new(UpdatePlanTool)),
            "task" => {
                // The subagent resolves its own backend/model from config so it
                // can run an independent loop. Its toolset is curated read-only
                // (see SubagentTool), so this never recurses into another `task`.
                let backend = config.backend_descriptor();
                let model =
                    crate::backends::default_model(&backend, config.model_override.as_deref());
                Some(Arc::new(SubagentTool {
                    http: reqwest::Client::new(),
                    backend,
                    model,
                    config: config.clone(),
                    runtime: runtime.cloned(),
                }))
            }
            "web_fetch" => Some(Arc::new(WebFetchTool {
                http: reqwest::Client::new(),
            })),
            "critique" => {
                // A separate critic agent, resolved like `task`. Its toolset is
                // curated read-only (see EvaluatorTool), so it never mutates and
                // never recurses into `task`/`critique`.
                let backend = config.backend_descriptor();
                let model =
                    crate::backends::default_model(&backend, config.model_override.as_deref());
                Some(Arc::new(EvaluatorTool {
                    http: reqwest::Client::new(),
                    backend,
                    model,
                    config: config.clone(),
                    runtime: runtime.cloned(),
                }))
            }
            _ => None,
        };
        if let Some(t) = t {
            out.push(t);
        }
    }
    out
}

pub fn build_tools(config: &AgentConfig) -> Vec<Arc<dyn Tool>> {
    build_tools_for_names(config, &config.tools, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolSelection;

    #[test]
    fn small_talk_sends_no_tools() {
        let config = AgentConfig {
            tool_selection: ToolSelection::Auto,
            ..Default::default()
        };
        for greeting in [
            "hi",
            "hello there",
            "thanks",
            "ok",
            "  hey!  ",
            "Good morning",
        ] {
            assert!(
                select_tool_names(&config, greeting).is_empty(),
                "{greeting:?} should be treated as small talk"
            );
        }
    }

    #[test]
    fn real_request_sends_the_full_pool() {
        let config = AgentConfig {
            tool_selection: ToolSelection::Auto,
            ..Default::default()
        };
        assert_eq!(
            select_tool_names(&config, "what files are in src?"),
            config.tools
        );
    }

    #[test]
    fn build_request_includes_write_tools() {
        // Regression: "build a bio site" used to surface no edit tools, forcing
        // the model to dump code into the chat. It must now get file_write +
        // file_edit + shell so it can actually create files.
        let config = AgentConfig {
            tool_selection: ToolSelection::Auto,
            ..Default::default()
        };
        let names = select_tool_names(&config, "build a bio site");
        assert!(names.contains(&"file_write".to_string()));
        assert!(names.contains(&"file_edit".to_string()));
        assert!(names.contains(&"shell".to_string()));
    }

    #[test]
    fn auto_only_sends_configured_tools() {
        let config = AgentConfig {
            tool_selection: ToolSelection::Auto,
            tools: vec!["file_read".into(), "shell".into()],
            ..Default::default()
        };
        assert_eq!(
            select_tool_names(&config, "do some real work"),
            vec!["file_read".to_string(), "shell".to_string()]
        );
    }

    #[test]
    fn fixed_mode_sends_full_pool_even_for_greetings() {
        let config = AgentConfig {
            tool_selection: ToolSelection::Fixed,
            ..Default::default()
        };
        assert_eq!(select_tool_names(&config, "hi"), config.tools);
    }

    #[test]
    fn batch_edit_dry_run_does_not_count_as_mutation() {
        let output = r#"{"preview":{},"applied":false,"dryRun":true,"successful":[],"failed":[]}"#;
        assert!(!tool_output_mutated_workspace("batch_edit", output));
    }

    #[test]
    fn batch_edit_validation_failure_does_not_count_as_mutation() {
        let output = r#"{"preview":{},"applied":false,"successful":[],"failed":[{"filePath":"a.rs","error":"not found"}]}"#;
        assert!(!tool_output_mutated_workspace("batch_edit", output));
    }

    #[test]
    fn batch_edit_apply_counts_as_mutation() {
        let output = r#"{"preview":{},"applied":true,"successful":["a.rs"],"failed":[]}"#;
        assert!(tool_output_mutated_workspace("batch_edit", output));
    }

    #[test]
    fn read_only_classifier_excludes_side_effecting_tools() {
        assert!(is_read_only_tool("file_read"));
        assert!(is_read_only_tool("grep"));
        assert!(is_read_only_tool("repo_search"));
        // Side-effecting or arbitrary-command tools are never parallelized.
        assert!(!is_read_only_tool("shell"));
        assert!(!is_read_only_tool("run_tests"));
        assert!(!is_read_only_tool("file_edit"));
        // MCP and unknown tools are not in the safe set.
        assert!(!is_read_only_tool("mcp__fs__read_file"));
        // Network fetch does not touch the workspace — safe to parallelize.
        assert!(is_read_only_tool("web_fetch"));
    }

    #[test]
    fn mutation_and_read_only_sets_are_disjoint() {
        for m in mutation_tool_names() {
            assert!(!is_read_only_tool(m), "{m} must not be read-only");
        }
    }

    #[test]
    fn file_edit_success_counts_as_mutation() {
        let output = r#"{"edited":true,"path":"src/main.rs","diff":"..."}"#;
        assert!(tool_output_mutated_workspace("file_edit", output));
    }

    #[test]
    fn mutation_detection_uses_structured_fields_not_error_substrings() {
        let output = r#"{"edited":true,"path":"src/main.rs","diff":"+ let label = \"error\";"}"#;
        assert!(tool_output_mutated_workspace("file_edit", output));

        let output = r#"{"path":"src/main.rs","diff":"..."}"#;
        assert!(!tool_output_mutated_workspace("file_edit", output));
    }
}
