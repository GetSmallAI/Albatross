use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::backends::{BackendDescriptor, BackendName, OpenRouterConfig};
use crate::model_system::ModelSystemConfig;

/// Project-local config file read by [`load_config`] and updated by
/// `/backend --default` / `/model --default`.
pub const AGENT_CONFIG_PATH: &str = "agent.config.json";

pub const ALL_TOOL_NAMES: &[&str] = &[
    "apply_patch",
    "batch_edit",
    "critique",
    "file_read",
    "file_write",
    "file_edit",
    "glob",
    "grep",
    "list_dir",
    "repo_search",
    "run_tests",
    "shell",
    "ship_status",
    "web_fetch",
];

pub fn is_tool_name(s: &str) -> bool {
    ALL_TOOL_NAMES.contains(&s)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    Always,
    Never,
    DangerousOnly,
}

impl ApprovalPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApprovalPolicy::Always => "always",
            ApprovalPolicy::Never => "never",
            ApprovalPolicy::DangerousOnly => "dangerous-only",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            "dangerous-only" => Some(Self::DangerousOnly),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolDisplay {
    Emoji,
    Grouped,
    Minimal,
    Hidden,
    /// Debug view: every tool call printed with its full arguments and a large
    /// result preview, so you can see exactly what the agent is doing.
    Verbose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputStyle {
    Block,
    Bordered,
    Plain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoaderStyle {
    Gradient,
    Spinner,
    Minimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolSelection {
    Auto,
    Fixed,
}

impl ToolSelection {
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolSelection::Auto => "auto",
            ToolSelection::Fixed => "fixed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "fixed" => Some(Self::Fixed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperatorMode {
    Explore,
    Edit,
    Ship,
    Review,
    Custom,
}

impl OperatorMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            OperatorMode::Explore => "explore",
            OperatorMode::Edit => "edit",
            OperatorMode::Ship => "ship",
            OperatorMode::Review => "review",
            OperatorMode::Custom => "custom",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "explore" => Some(Self::Explore),
            "edit" => Some(Self::Edit),
            "ship" => Some(Self::Ship),
            "review" => Some(Self::Review),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutsideWorkspace {
    Prompt,
    Deny,
    Allow,
}

impl OutsideWorkspace {
    pub fn as_str(&self) -> &'static str {
        match self {
            OutsideWorkspace::Prompt => "prompt",
            OutsideWorkspace::Deny => "deny",
            OutsideWorkspace::Allow => "allow",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "prompt" => Some(Self::Prompt),
            "deny" => Some(Self::Deny),
            "allow" => Some(Self::Allow),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayConfig {
    #[serde(rename = "toolDisplay", default = "default_tool_display")]
    pub tool_display: ToolDisplay,
    #[serde(rename = "inputStyle", default = "default_input_style")]
    pub input_style: InputStyle,
    #[serde(rename = "loaderText", default = "default_loader_text")]
    pub loader_text: String,
    #[serde(rename = "loaderStyle", default = "default_loader_style")]
    pub loader_style: LoaderStyle,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(rename = "showBanner", default = "default_true")]
    pub show_banner: bool,
    #[serde(rename = "eventLog", default)]
    pub event_log: crate::turn_trace::EventLogConfig,
    #[serde(default = "default_color_mode")]
    pub color: ColorMode,
    #[serde(default)]
    pub ascii: bool,
}

fn default_true() -> bool {
    true
}
fn default_tool_display() -> ToolDisplay {
    ToolDisplay::Grouped
}
fn default_input_style() -> InputStyle {
    InputStyle::Bordered
}
fn default_loader_text() -> String {
    "Thinking".into()
}
fn default_loader_style() -> LoaderStyle {
    LoaderStyle::Spinner
}
fn default_color_mode() -> ColorMode {
    ColorMode::Auto
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            tool_display: default_tool_display(),
            input_style: default_input_style(),
            loader_text: default_loader_text(),
            loader_style: default_loader_style(),
            reasoning: false,
            show_banner: true,
            event_log: crate::turn_trace::EventLogConfig::default(),
            color: default_color_mode(),
            ascii: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScorecardConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(rename = "qualityThreshold", default = "default_quality_threshold")]
    pub quality_threshold: u8,
    #[serde(rename = "nudgeMinTurns", default = "default_nudge_min_turns")]
    pub nudge_min_turns: usize,
}

fn default_quality_threshold() -> u8 {
    80
}

fn default_nudge_min_turns() -> usize {
    3
}

impl Default for ScorecardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            quality_threshold: default_quality_threshold(),
            nudge_min_turns: default_nudge_min_turns(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FableUsageConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(rename = "weeklyTokenBudget", default)]
    pub weekly_token_budget: Option<u64>,
    #[serde(rename = "capShare", default = "default_fable_cap_share")]
    pub cap_share: f64,
    #[serde(rename = "weekStartsOn", default = "default_fable_week_starts_on")]
    pub week_starts_on: String,
    #[serde(rename = "fableModelMatches", default = "default_fable_model_matches")]
    pub fable_model_matches: Vec<String>,
    #[serde(
        rename = "claudeModelMatches",
        default = "default_claude_model_matches"
    )]
    pub claude_model_matches: Vec<String>,
}

fn default_fable_cap_share() -> f64 {
    0.5
}

fn default_fable_week_starts_on() -> String {
    "monday".into()
}

fn default_fable_model_matches() -> Vec<String> {
    vec!["fable".into()]
}

fn default_claude_model_matches() -> Vec<String> {
    vec!["anthropic/".into(), "claude".into(), "fable".into()]
}

impl Default for FableUsageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            weekly_token_budget: None,
            cap_share: default_fable_cap_share(),
            week_starts_on: default_fable_week_starts_on(),
            fable_model_matches: default_fable_model_matches(),
            claude_model_matches: default_claude_model_matches(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(rename = "maxTurns", default = "default_checkpoint_max_turns")]
    pub max_turns: usize,
    #[serde(rename = "maxBytes", default = "default_checkpoint_max_bytes")]
    pub max_bytes: u64,
    #[serde(rename = "maxFileBytes", default = "default_checkpoint_max_file_bytes")]
    pub max_file_bytes: u64,
}

fn default_checkpoint_max_turns() -> usize {
    10
}

fn default_checkpoint_max_bytes() -> u64 {
    10 * 1024 * 1024
}

fn default_checkpoint_max_file_bytes() -> u64 {
    1024 * 1024
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_turns: default_checkpoint_max_turns(),
            max_bytes: default_checkpoint_max_bytes(),
            max_file_bytes: default_checkpoint_max_file_bytes(),
        }
    }
}

impl CheckpointConfig {
    pub fn limits(&self) -> crate::turn_checkpoint::CheckpointLimits {
        crate::turn_checkpoint::CheckpointLimits {
            max_turns: self.max_turns,
            max_bytes: self.max_bytes,
            max_file_bytes: self.max_file_bytes,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixConfig {
    #[serde(rename = "maxAttempts", default = "default_fix_max_attempts")]
    pub max_attempts: usize,
}

fn default_fix_max_attempts() -> usize {
    5
}

impl Default for FixConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_fix_max_attempts(),
        }
    }
}

/// Configures the `critique` evaluator and the rubric it grades against (#2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RubricConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Override path to the rubric markdown; defaults to
    /// `<workspace>/.albatross/rubric.md`.
    #[serde(rename = "rubricPath", default)]
    pub rubric_path: Option<String>,
    /// Pass threshold on the 0-10 weighted scale.
    #[serde(rename = "passThreshold", default = "default_pass_threshold")]
    pub pass_threshold: f32,
    /// Allow the critic to send workspace context to a cloud backend.
    #[serde(rename = "allowCloud", default)]
    pub allow_cloud: bool,
    /// Let the critic actually run the project's tests (via the fixed-surface
    /// `verify` tool) before scoring functionality, rather than reading only.
    #[serde(rename = "liveVerify", default)]
    pub live_verify: bool,
    /// Timeout (seconds) for a single `verify` run; falls back to a bounded
    /// default so a hung suite can't stall the loop.
    #[serde(rename = "verifyTimeoutSecs", default)]
    pub verify_timeout_secs: Option<u64>,
}

fn default_pass_threshold() -> f32 {
    crate::rubric::DEFAULT_PASS_THRESHOLD
}

impl RubricConfig {
    /// Bounded timeout for a `verify` run (default 10 minutes).
    pub fn verify_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.verify_timeout_secs.unwrap_or(600))
    }
}

impl Default for RubricConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            rubric_path: None,
            pass_threshold: default_pass_threshold(),
            allow_cloud: false,
            live_verify: false,
            verify_timeout_secs: None,
        }
    }
}

/// Configures the `/iterate` generate→evaluate loop (#3). The pass threshold is
/// the rubric's `passThreshold`; only the iteration cap and the (optional)
/// separate evaluator model live here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IterateConfig {
    #[serde(rename = "maxIters", default = "default_iterate_max_iters")]
    pub max_iters: usize,
    /// Run the critic on a different model than the generator (stronger
    /// generator/evaluator separation). `None` reuses the generator's model.
    #[serde(rename = "evaluatorModel", default)]
    pub evaluator_model: Option<String>,
}

fn default_iterate_max_iters() -> usize {
    6
}

impl Default for IterateConfig {
    fn default() -> Self {
        Self {
            max_iters: default_iterate_max_iters(),
            evaluator_model: None,
        }
    }
}

/// Configures the `/auto` autonomous overnight run. `/auto` chains the
/// `/iterate` loop with automatic `/reset` so a multi-hour run never blows its
/// context budget; these are the default guardrails, overridable per-run by CLI
/// flags. The per-round pass bar is the rubric's `passThreshold`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoConfig {
    /// Round ceiling when no `--max`/`--budget`/`--deadline` is given. There is
    /// always a finite bound; the loop additionally hard-clamps to its own
    /// runaway ceiling regardless of this value.
    #[serde(rename = "maxRounds", default = "default_auto_max_rounds")]
    pub max_rounds: usize,
    /// Optional default dollar cap on generator spend (`--budget` overrides).
    #[serde(rename = "budgetUsd", default)]
    pub budget_usd: Option<f64>,
    /// Context-fill ratio (0.50..=0.95) at which an automatic `/reset` fires.
    #[serde(rename = "resetRatio", default = "default_auto_reset_ratio")]
    pub reset_ratio: f64,
    /// Optional default wall-clock deadline (e.g. "6h", "30m"); `--deadline`
    /// overrides. Parsed at run time so an invalid value fails loudly there.
    #[serde(rename = "deadline", default)]
    pub deadline: Option<String>,
}

fn default_auto_max_rounds() -> usize {
    12
}

fn default_auto_reset_ratio() -> f64 {
    0.75
}

impl Default for AutoConfig {
    fn default() -> Self {
        Self {
            max_rounds: default_auto_max_rounds(),
            budget_usd: None,
            reset_ratio: default_auto_reset_ratio(),
            deadline: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(rename = "maxPaths", default = "default_paths_max_paths")]
    pub max_paths: usize,
    #[serde(
        rename = "maxSnapshotBytes",
        default = "default_paths_max_snapshot_bytes"
    )]
    pub max_snapshot_bytes: u64,
    #[serde(rename = "maxFileBytes", default = "default_paths_max_file_bytes")]
    pub max_file_bytes: u64,
}

fn default_paths_max_paths() -> usize {
    5
}

fn default_paths_max_snapshot_bytes() -> u64 {
    50 * 1024 * 1024
}

fn default_paths_max_file_bytes() -> u64 {
    1024 * 1024
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_paths: default_paths_max_paths(),
            max_snapshot_bytes: default_paths_max_snapshot_bytes(),
            max_file_bytes: default_paths_max_file_bytes(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    #[serde(rename = "maxMessages", default)]
    pub max_messages: Option<usize>,
    #[serde(rename = "maxBytes", default)]
    pub max_bytes: Option<usize>,
    #[serde(rename = "modelContextTokens", default)]
    pub model_context_tokens: Option<usize>,
    #[serde(rename = "autoCompact", default)]
    pub auto_compact: Option<bool>,
    #[serde(rename = "compactThreshold", default = "default_compact_threshold")]
    pub compact_threshold: f64,
    #[serde(rename = "reserveRatio", default = "default_reserve_ratio")]
    pub reserve_ratio: f64,
}

fn default_compact_threshold() -> f64 {
    0.85
}

fn default_reserve_ratio() -> f64 {
    0.25
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_messages: Some(40),
            max_bytes: Some(256 * 1024),
            model_context_tokens: None,
            auto_compact: None,
            compact_threshold: default_compact_threshold(),
            reserve_ratio: default_reserve_ratio(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(rename = "maxEntries", default = "default_history_max_entries")]
    pub max_entries: usize,
    #[serde(default)]
    pub path: Option<String>,
}

fn default_history_max_entries() -> usize {
    200
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: default_history_max_entries(),
            path: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectMemoryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(rename = "autoInject", default = "default_true")]
    pub auto_inject: bool,
    #[serde(rename = "autoIndex", default)]
    pub auto_index: bool,
    #[serde(
        rename = "maxFileBytes",
        default = "default_project_memory_max_file_bytes"
    )]
    pub max_file_bytes: usize,
    #[serde(
        rename = "maxInjectedBytes",
        default = "default_project_memory_max_injected_bytes"
    )]
    pub max_injected_bytes: usize,
    #[serde(rename = "allowCloudContext", default)]
    pub allow_cloud_context: bool,
}

fn default_project_memory_max_file_bytes() -> usize {
    512 * 1024
}

fn default_project_memory_max_injected_bytes() -> usize {
    8 * 1024
}

impl Default for ProjectMemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_inject: true,
            auto_index: false,
            max_file_bytes: default_project_memory_max_file_bytes(),
            max_injected_bytes: default_project_memory_max_injected_bytes(),
            allow_cloud_context: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub mode: OperatorMode,
    pub backend: BackendName,
    pub model_override: Option<String>,
    pub system_prompt: String,
    pub max_steps: usize,
    pub session_dir: String,
    pub workspace_root: String,
    pub outside_workspace: OutsideWorkspace,
    pub approval_policy: ApprovalPolicy,
    pub tools: Vec<String>,
    pub tool_selection: ToolSelection,
    pub display: DisplayConfig,
    pub scorecard: ScorecardConfig,
    pub fable: FableUsageConfig,
    pub slash_commands: bool,
    pub context: ContextConfig,
    pub history: HistoryConfig,
    pub project_memory: ProjectMemoryConfig,
    pub checkpoints: CheckpointConfig,
    pub fix: FixConfig,
    pub rubric: RubricConfig,
    pub iterate: IterateConfig,
    pub auto: AutoConfig,
    pub paths: PathsConfig,
    pub openrouter: OpenRouterConfig,
    pub model_system: ModelSystemConfig,
    pub mcp_servers: BTreeMap<String, crate::mcp::McpServerConfig>,
    pub hooks: crate::hooks::HookConfig,
}

const SYSTEM_PROMPT: &str = concat!(
    "You are a coding assistant running on the user's local machine via a small open-weight LLM.\n",
    "\n",
    "Always respond in English unless the user writes to you in another language.\n",
    "\n",
    "Available tools: {tools}.\n",
    "\n",
    "When to use tools vs answer directly:\n",
    "- For greetings, casual chat, or questions you can answer from general knowledge, respond in plain text. Do NOT call a tool.\n",
    "- Only call a tool when the user's request actually needs filesystem access.\n",
    "- When you do call a tool, emit a real tool call — not a JSON description in your text response.\n",
    "\n",
    "When the user asks you to create or change code or files:\n",
    "- DO write the changes to disk with the file tools: file_write to create a\n",
    "  new file, file_edit to modify an existing one. Then briefly say what you\n",
    "  did.\n",
    "- DO NOT paste file contents or large code blocks into your reply. The user\n",
    "  sees the files and diffs directly — repeating them wastes tokens and is\n",
    "  not what they asked for. Only show code inline if they explicitly ask to\n",
    "  see it, or for a tiny (1-3 line) illustration.\n",
    "- After the edits, reply with a short summary: what you created or changed\n",
    "  and the key files — a sentence or two, not the code.\n",
    "- Make minimal targeted edits consistent with existing style.\n",
    "- For a task that takes 3 or more steps, call update_plan first with the\n",
    "  full plan, then call it again to mark each step done as you finish it.\n",
    "  Keep exactly one step in_progress. Skip the plan for trivial one-shot tasks.\n",
    "- When a question needs reading lots of files to answer (\"where is X\n",
    "  handled?\"), delegate it to the read-only `task` subagent so the\n",
    "  exploration stays out of your context; act on the summary it returns.\n",
    "\n",
    "Current working directory: {cwd}",
);

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            mode: OperatorMode::Edit,
            backend: BackendName::Ollama,
            model_override: None,
            system_prompt: SYSTEM_PROMPT.into(),
            max_steps: 20,
            session_dir: ".sessions".into(),
            workspace_root: std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .display()
                .to_string(),
            outside_workspace: OutsideWorkspace::Prompt,
            approval_policy: ApprovalPolicy::Always,
            // Bare-bones core: read, search, edit, run — plus the two
            // lightweight orchestration tools (auto-selection only surfaces them
            // when relevant). repo_search, run_tests, batch_edit, web_fetch, etc.
            // are opt-in via /tools or config.
            tools: vec![
                "file_read".into(),
                "grep".into(),
                "list_dir".into(),
                "file_edit".into(),
                "file_write".into(),
                "shell".into(),
                "update_plan".into(),
                "task".into(),
            ],
            tool_selection: ToolSelection::Auto,
            display: DisplayConfig::default(),
            scorecard: ScorecardConfig::default(),
            fable: FableUsageConfig::default(),
            slash_commands: true,
            context: ContextConfig::default(),
            history: HistoryConfig::default(),
            project_memory: ProjectMemoryConfig::default(),
            checkpoints: CheckpointConfig::default(),
            fix: FixConfig::default(),
            rubric: RubricConfig::default(),
            iterate: IterateConfig::default(),
            auto: AutoConfig::default(),
            paths: PathsConfig::default(),
            openrouter: OpenRouterConfig::default(),
            model_system: ModelSystemConfig::default(),
            mcp_servers: BTreeMap::new(),
            hooks: crate::hooks::HookConfig::default(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    mode: Option<String>,
    backend: Option<String>,
    #[serde(rename = "modelOverride")]
    model_override: Option<String>,
    #[serde(rename = "systemPrompt")]
    system_prompt: Option<String>,
    #[serde(rename = "maxSteps")]
    max_steps: Option<usize>,
    #[serde(rename = "sessionDir")]
    session_dir: Option<String>,
    #[serde(rename = "workspaceRoot")]
    workspace_root: Option<String>,
    #[serde(rename = "outsideWorkspace")]
    outside_workspace: Option<String>,
    #[serde(rename = "approvalPolicy")]
    approval_policy: Option<String>,
    tools: Option<Vec<String>>,
    #[serde(rename = "toolSelection")]
    tool_selection: Option<String>,
    display: Option<DisplayConfig>,
    scorecard: Option<ScorecardConfig>,
    fable: Option<FableUsageConfig>,
    #[serde(rename = "slashCommands")]
    slash_commands: Option<bool>,
    context: Option<ContextConfig>,
    history: Option<HistoryConfig>,
    #[serde(rename = "projectMemory")]
    project_memory: Option<ProjectMemoryConfig>,
    checkpoints: Option<CheckpointConfig>,
    fix: Option<FixConfig>,
    rubric: Option<RubricConfig>,
    iterate: Option<IterateConfig>,
    auto: Option<AutoConfig>,
    paths: Option<PathsConfig>,
    openrouter: Option<OpenRouterConfig>,
    #[serde(rename = "modelSystem")]
    model_system: Option<ModelSystemConfig>,
    #[serde(rename = "mcpServers")]
    mcp_servers: Option<BTreeMap<String, crate::mcp::McpServerConfig>>,
    hooks: Option<Value>,
}

impl AgentConfig {
    pub fn apply_operator_mode(&mut self, mode: OperatorMode) {
        self.mode = mode;
        match mode {
            OperatorMode::Explore => {
                self.tools = vec![
                    "file_read".into(),
                    "grep".into(),
                    "list_dir".into(),
                    "glob".into(),
                    "repo_search".into(),
                ];
                self.tool_selection = ToolSelection::Auto;
                self.approval_policy = ApprovalPolicy::DangerousOnly;
                self.max_steps = self.max_steps.clamp(6, 12);
                self.checkpoints.enabled = false;
            }
            OperatorMode::Edit => {
                self.tools = vec![
                    "file_read".into(),
                    "file_edit".into(),
                    "grep".into(),
                    "list_dir".into(),
                    "repo_search".into(),
                    "apply_patch".into(),
                    "run_tests".into(),
                ];
                self.tool_selection = ToolSelection::Auto;
                self.approval_policy = ApprovalPolicy::Always;
                self.max_steps = self.max_steps.max(12);
                self.checkpoints.enabled = true;
            }
            OperatorMode::Ship => {
                self.tools = vec![
                    "file_read".into(),
                    "file_edit".into(),
                    "file_write".into(),
                    "apply_patch".into(),
                    "grep".into(),
                    "list_dir".into(),
                    "glob".into(),
                    "repo_search".into(),
                    "shell".into(),
                    "run_tests".into(),
                    "batch_edit".into(),
                    "ship_status".into(),
                ];
                self.tool_selection = ToolSelection::Auto;
                self.approval_policy = ApprovalPolicy::DangerousOnly;
                self.max_steps = self.max_steps.max(20);
                self.checkpoints.enabled = true;
            }
            OperatorMode::Review => {
                self.tools = vec![
                    "file_read".into(),
                    "grep".into(),
                    "list_dir".into(),
                    "glob".into(),
                    "repo_search".into(),
                    "shell".into(),
                ];
                self.tool_selection = ToolSelection::Auto;
                self.approval_policy = ApprovalPolicy::DangerousOnly;
                self.max_steps = self.max_steps.clamp(8, 16);
                self.checkpoints.enabled = false;
            }
            OperatorMode::Custom => {}
        }
    }

    pub fn render_system_prompt(&self) -> String {
        self.render_system_prompt_for_tools(&self.tools)
    }

    pub fn render_system_prompt_for_tools(&self, tools: &[String]) -> String {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let tool_list = if tools.is_empty() {
            "none".to_string()
        } else {
            tools.join(", ")
        };
        let mut prompt = self
            .system_prompt
            .replace("{cwd}", &cwd)
            .replace("{tools}", &tool_list);
        if self.mode == OperatorMode::Ship {
            prompt.push_str(
                "\n\nShip mode:\n\
                 - Prefer run_tests over raw shell for test execution.\n\
                 - Use batch_edit for multi-file coordinated changes.\n\
                 - Call ship_status before declaring the work done.\n",
            );
        }
        prompt
    }

    pub fn history_path(&self) -> String {
        self.history.path.clone().unwrap_or_else(|| {
            Path::new(&self.session_dir)
                .join("history.jsonl")
                .display()
                .to_string()
        })
    }

    pub fn backend_descriptor(&self) -> BackendDescriptor {
        self.backend_descriptor_for(self.backend)
    }

    pub fn backend_descriptor_for(&self, name: BackendName) -> BackendDescriptor {
        let mut backend = crate::backends::backend(name);
        backend.openrouter = self.openrouter.clone();
        backend
    }
}

fn parse_dotenv_file(path: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let mut value = value.trim().to_string();
        if value.len() >= 2 {
            let quoted = (value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\''));
            if quoted {
                value = value[1..value.len() - 1].to_string();
            }
        }
        out.insert(key.to_string(), value);
    }
    out
}

pub(crate) fn dotenv_values() -> BTreeMap<String, String> {
    let mut out = parse_dotenv_file(Path::new(".env"));
    out.extend(parse_dotenv_file(Path::new(".env.local")));
    out
}

pub(crate) fn layered_env(dotenv: &BTreeMap<String, String>, key: &str) -> Option<String> {
    std::env::var(key).ok().or_else(|| dotenv.get(key).cloned())
}

fn parse_hooks_config<F>(value: Value, mut warn: F) -> crate::hooks::HookConfig
where
    F: FnMut(&str),
{
    let Some(obj) = value.as_object() else {
        warn("agent.config.json hooks ignored: expected object");
        return crate::hooks::HookConfig::default();
    };
    let mut hooks = crate::hooks::HookConfig::default();
    for (key, value) in obj {
        match key.as_str() {
            "SessionStart" => {
                if let Some(groups) = parse_hook_groups(key, value, &mut warn) {
                    hooks.session_start = groups;
                }
            }
            "UserPromptSubmit" => {
                if let Some(groups) = parse_hook_groups(key, value, &mut warn) {
                    hooks.user_prompt_submit = groups;
                }
            }
            "PreToolUse" => {
                if let Some(groups) = parse_hook_groups(key, value, &mut warn) {
                    hooks.pre_tool_use = groups;
                }
            }
            "PermissionRequest" => {
                if let Some(groups) = parse_hook_groups(key, value, &mut warn) {
                    hooks.permission_request = groups;
                }
            }
            "PostToolUse" => {
                if let Some(groups) = parse_hook_groups(key, value, &mut warn) {
                    hooks.post_tool_use = groups;
                }
            }
            "PreCompact" => {
                if let Some(groups) = parse_hook_groups(key, value, &mut warn) {
                    hooks.pre_compact = groups;
                }
            }
            "PostCompact" => {
                if let Some(groups) = parse_hook_groups(key, value, &mut warn) {
                    hooks.post_compact = groups;
                }
            }
            "PlanUpdated" => {
                if let Some(groups) = parse_hook_groups(key, value, &mut warn) {
                    hooks.plan_updated = groups;
                }
            }
            "SubagentStart" => {
                if let Some(groups) = parse_hook_groups(key, value, &mut warn) {
                    hooks.subagent_start = groups;
                }
            }
            "SubagentStop" => {
                if let Some(groups) = parse_hook_groups(key, value, &mut warn) {
                    hooks.subagent_stop = groups;
                }
            }
            "Stop" => {
                if let Some(groups) = parse_hook_groups(key, value, &mut warn) {
                    hooks.stop = groups;
                }
            }
            "SessionEnd" => {
                if let Some(groups) = parse_hook_groups(key, value, &mut warn) {
                    hooks.session_end = groups;
                }
            }
            "state" => warn(
                "agent.config.json hooks.state ignored: project config cannot grant hook trust",
            ),
            _ => warn(&format!(
                "agent.config.json hooks.{key} ignored: unknown hook event"
            )),
        }
    }
    hooks
}

fn parse_hook_groups<F>(
    key: &str,
    value: &Value,
    warn: &mut F,
) -> Option<Vec<crate::hooks::HookGroupConfig>>
where
    F: FnMut(&str),
{
    let Some(groups) = value.as_array() else {
        warn(&format!(
            "agent.config.json hooks.{key} ignored: expected array"
        ));
        return None;
    };
    let mut parsed = Vec::new();
    for (idx, group) in groups.iter().enumerate() {
        if let Some(group) = parse_hook_group(key, idx, group, warn) {
            parsed.push(group);
        }
    }
    Some(parsed)
}

fn parse_hook_group<F>(
    event_key: &str,
    group_idx: usize,
    value: &Value,
    warn: &mut F,
) -> Option<crate::hooks::HookGroupConfig>
where
    F: FnMut(&str),
{
    let Some(obj) = value.as_object() else {
        warn(&format!(
            "agent.config.json hooks.{event_key}[{group_idx}] ignored: expected object"
        ));
        return None;
    };
    let matcher = match obj.get("matcher") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.clone()),
        Some(_) => {
            warn(&format!(
                "agent.config.json hooks.{event_key}[{group_idx}] ignored: matcher must be a string"
            ));
            return None;
        }
    };
    let mut hooks = Vec::new();
    match obj.get("hooks") {
        None => {}
        Some(Value::Array(items)) => {
            for (hook_idx, hook) in items.iter().enumerate() {
                match serde_json::from_value(hook.clone()) {
                    Ok(hook) => hooks.push(hook),
                    Err(e) => warn(&format!(
                        "agent.config.json hooks.{event_key}[{group_idx}].hooks[{hook_idx}] ignored: {e}"
                    )),
                }
            }
        }
        Some(_) => warn(&format!(
            "agent.config.json hooks.{event_key}[{group_idx}].hooks ignored: expected array"
        )),
    }
    Some(crate::hooks::HookGroupConfig { matcher, hooks })
}

/// Strip a lone `--default` token from slash-command args.
///
/// Returns the remaining argument string (tokens rejoined with spaces) and
/// whether the flag was present. Order is free: `--default` may appear first,
/// last, or between other tokens.
pub fn strip_default_flag(args: &str) -> (String, bool) {
    let mut as_default = false;
    let mut kept = Vec::new();
    for token in args.split_whitespace() {
        if token == "--default" {
            as_default = true;
        } else {
            kept.push(token);
        }
    }
    (kept.join(" "), as_default)
}

/// The backend/model currently persisted as project defaults in
/// `agent.config.json`, used to mark `(default)` in interactive pickers.
///
/// Returns `(backend, model_override)`; either is `None` when the file is
/// missing, unreadable, not a JSON object, or omits that key. This reflects
/// what would load on next launch, independent of the live session.
pub fn persisted_defaults() -> (Option<BackendName>, Option<String>) {
    persisted_defaults_from(Path::new(AGENT_CONFIG_PATH))
}

fn persisted_defaults_from(path: &Path) -> (Option<BackendName>, Option<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (None, None);
    };
    let Ok(file) = serde_json::from_str::<FileConfig>(&text) else {
        return (None, None);
    };
    let backend = file.backend.as_deref().and_then(BackendName::parse);
    (backend, file.model_override)
}

/// Surgically merge `backend` / `modelOverride` into `agent.config.json`.
///
/// - Creates the file when missing.
/// - Preserves every other key when the existing root is a JSON object.
/// - When `model_override` is `None`, removes `modelOverride` from the file.
/// - Refuses to overwrite an existing file whose contents are not a JSON object
///   (invalid JSON or non-object root).
pub fn persist_backend_model_defaults(
    path: &Path,
    backend: BackendName,
    model_override: Option<&str>,
) -> Result<()> {
    let mut root = if path.exists() {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow!("could not read {}: {e}", path.display()))?;
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("could not merge {}: invalid JSON ({e})", path.display()))?;
        match parsed {
            Value::Object(map) => Value::Object(map),
            _ => bail!(
                "could not merge {}: root must be a JSON object",
                path.display()
            ),
        }
    } else {
        Value::Object(serde_json::Map::new())
    };

    {
        let Some(obj) = root.as_object_mut() else {
            bail!(
                "could not merge {}: root must be a JSON object",
                path.display()
            );
        };
        obj.insert("backend".into(), json!(backend.as_str()));
        match model_override {
            Some(model) => {
                obj.insert("modelOverride".into(), json!(model));
            }
            None => {
                obj.remove("modelOverride");
            }
        }
    }

    let body = serde_json::to_string_pretty(&root)?;
    std::fs::write(path, format!("{body}\n"))
        .map_err(|e| anyhow!("could not write {}: {e}", path.display()))?;
    Ok(())
}

pub fn load_config() -> AgentConfig {
    let mut config = AgentConfig::default();
    let dotenv = dotenv_values();

    let path = Path::new(AGENT_CONFIG_PATH);
    if path.exists() {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<FileConfig>(&text) {
                Ok(file) => {
                    if let Some(m) = file.mode.as_deref().and_then(OperatorMode::parse) {
                        config.apply_operator_mode(m);
                    }
                    if let Some(b) = file.backend.as_deref().and_then(BackendName::parse) {
                        config.backend = b;
                    }
                    if let Some(m) = file.model_override {
                        config.model_override = Some(m);
                    }
                    if let Some(sp) = file.system_prompt {
                        config.system_prompt = sp;
                    }
                    if let Some(s) = file.max_steps {
                        config.max_steps = s;
                    }
                    if let Some(d) = file.session_dir {
                        config.session_dir = d;
                    }
                    if let Some(w) = file.workspace_root {
                        config.workspace_root = w;
                    }
                    if let Some(o) = file
                        .outside_workspace
                        .as_deref()
                        .and_then(OutsideWorkspace::parse)
                    {
                        config.outside_workspace = o;
                    }
                    if let Some(a) = file
                        .approval_policy
                        .as_deref()
                        .and_then(ApprovalPolicy::parse)
                    {
                        config.approval_policy = a;
                    }
                    if let Some(t) = file.tools {
                        let valid: Vec<String> =
                            t.into_iter().filter(|n| is_tool_name(n)).collect();
                        if !valid.is_empty() {
                            config.tools = valid;
                        }
                    }
                    if let Some(s) = file
                        .tool_selection
                        .as_deref()
                        .and_then(ToolSelection::parse)
                    {
                        config.tool_selection = s;
                    }
                    if let Some(d) = file.display {
                        config.display = d;
                    }
                    if let Some(s) = file.scorecard {
                        config.scorecard = s;
                    }
                    if let Some(f) = file.fable {
                        config.fable = f;
                    }
                    if let Some(sc) = file.slash_commands {
                        config.slash_commands = sc;
                    }
                    if let Some(c) = file.context {
                        config.context = c;
                    }
                    if let Some(h) = file.history {
                        config.history = h;
                    }
                    if let Some(p) = file.project_memory {
                        config.project_memory = p;
                    }
                    if let Some(c) = file.checkpoints {
                        config.checkpoints = c;
                    }
                    if let Some(f) = file.fix {
                        config.fix = f;
                    }
                    if let Some(r) = file.rubric {
                        config.rubric = r;
                    }
                    if let Some(i) = file.iterate {
                        config.iterate = i;
                    }
                    if let Some(a) = file.auto {
                        config.auto = a;
                    }
                    if let Some(p) = file.paths {
                        config.paths = p;
                    }
                    if let Some(o) = file.openrouter {
                        config.openrouter = o;
                    }
                    if let Some(m) = file.model_system {
                        config.model_system = m;
                    }
                    if let Some(s) = file.mcp_servers {
                        config.mcp_servers = s;
                    }
                    if let Some(h) = file.hooks {
                        config.hooks = parse_hooks_config(h, |warning| eprintln!("{warning}"));
                    }
                    if let Some(m) = file.mode.as_deref().and_then(OperatorMode::parse) {
                        config.mode = m;
                    }
                }
                Err(e) => eprintln!("{} ignored: failed to parse config: {e}", path.display()),
            },
            Err(e) => eprintln!("{} ignored: failed to read config: {e}", path.display()),
        }
    }

    if let Some(s) = layered_env(&dotenv, "BACKEND") {
        if let Some(b) = BackendName::parse(&s) {
            config.backend = b;
        }
    }
    if let Some(m) = layered_env(&dotenv, "AGENT_MODEL") {
        config.model_override = Some(m);
    }
    if let Some(s) = layered_env(&dotenv, "AGENT_MAX_STEPS") {
        if let Ok(n) = s.parse::<usize>() {
            config.max_steps = n;
        }
    }
    if let Some(s) = layered_env(&dotenv, "APPROVAL_POLICY") {
        if let Some(p) = ApprovalPolicy::parse(&s) {
            config.approval_policy = p;
        }
    }
    if let Some(s) = layered_env(&dotenv, "AGENT_TOOLS") {
        let requested: Vec<String> = s
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && is_tool_name(s))
            .collect();
        if !requested.is_empty() {
            config.tools = requested;
        }
    }
    if let Some(s) = layered_env(&dotenv, "AGENT_TOOL_SELECTION") {
        if let Some(selection) = ToolSelection::parse(&s) {
            config.tool_selection = selection;
        }
    }
    if let Some(s) = layered_env(&dotenv, "ALBATROSS_MODE") {
        if let Some(mode) = OperatorMode::parse(&s) {
            config.apply_operator_mode(mode);
        }
    }
    if let Some(s) = layered_env(&dotenv, "WORKSPACE_ROOT") {
        config.workspace_root = s;
    }
    if let Some(s) = layered_env(&dotenv, "OUTSIDE_WORKSPACE") {
        if let Some(p) = OutsideWorkspace::parse(&s) {
            config.outside_workspace = p;
        }
    }
    if let Some(s) = layered_env(&dotenv, "AGENT_CONTEXT_MAX_MESSAGES") {
        if let Ok(n) = s.parse::<usize>() {
            config.context.max_messages = Some(n);
        }
    }
    if let Some(s) = layered_env(&dotenv, "AGENT_CONTEXT_MAX_BYTES") {
        if let Ok(n) = s.parse::<usize>() {
            config.context.max_bytes = Some(n);
        }
    }
    if let Some(s) = layered_env(&dotenv, "AGENT_CONTEXT_MODEL_TOKENS") {
        if let Ok(n) = s.parse::<usize>() {
            config.context.model_context_tokens = Some(n);
        }
    }
    if let Some(s) = layered_env(&dotenv, "AGENT_CONTEXT_AUTO_COMPACT") {
        config.context.auto_compact = Some(s != "false" && s != "0");
    }
    if let Some(s) = layered_env(&dotenv, "AGENT_CONTEXT_COMPACT_THRESHOLD") {
        if let Ok(n) = s.parse::<f64>() {
            config.context.compact_threshold = n.clamp(0.5, 0.99);
        }
    }
    if let Some(s) = layered_env(&dotenv, "AGENT_CONTEXT_RESERVE_RATIO") {
        if let Ok(n) = s.parse::<f64>() {
            config.context.reserve_ratio = n.clamp(0.05, 0.5);
        }
    }
    if let Some(s) = layered_env(&dotenv, "AGENT_HISTORY") {
        config.history.enabled = s != "false" && s != "0";
    }
    if let Some(s) = layered_env(&dotenv, "AGENT_HISTORY_MAX_ENTRIES") {
        if let Ok(n) = s.parse::<usize>() {
            config.history.max_entries = n.max(1);
        }
    }

    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dotenv_quotes_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "A=one\n# nope\nB=\"two words\"\nC='three words'\nBAD\n",
        )
        .unwrap();
        let parsed = parse_dotenv_file(&path);
        assert_eq!(parsed.get("A").map(String::as_str), Some("one"));
        assert_eq!(parsed.get("B").map(String::as_str), Some("two words"));
        assert_eq!(parsed.get("C").map(String::as_str), Some("three words"));
        assert!(!parsed.contains_key("BAD"));
    }

    #[test]
    fn process_env_wins_over_dotenv_values() {
        let mut dotenv = BTreeMap::new();
        dotenv.insert("ALBATROSS_TEST_LAYER".into(), "dotenv".into());
        std::env::set_var("ALBATROSS_TEST_LAYER", "process");
        assert_eq!(
            layered_env(&dotenv, "ALBATROSS_TEST_LAYER").as_deref(),
            Some("process")
        );
        std::env::remove_var("ALBATROSS_TEST_LAYER");
    }

    #[test]
    fn strip_default_flag_positions_and_absence() {
        assert_eq!(
            strip_default_flag("grok-4.5 --default"),
            ("grok-4.5".into(), true)
        );
        assert_eq!(
            strip_default_flag("--default grok-4.5"),
            ("grok-4.5".into(), true)
        );
        assert_eq!(strip_default_flag("--default"), ("".into(), true));
        assert_eq!(strip_default_flag("ollama"), ("ollama".into(), false));
        assert_eq!(
            strip_default_flag("provider/model-name --default"),
            ("provider/model-name".into(), true)
        );
        // Multi-token ids rejoin with a single space.
        assert_eq!(
            strip_default_flag("--default foo bar"),
            ("foo bar".into(), true)
        );
    }

    #[test]
    fn persisted_defaults_reads_backend_and_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.config.json");

        // Missing file → no defaults.
        assert_eq!(persisted_defaults_from(&path), (None, None));

        std::fs::write(
            &path,
            r#"{ "backend": "openrouter", "modelOverride": "x-ai/grok-4.5" }"#,
        )
        .unwrap();
        assert_eq!(
            persisted_defaults_from(&path),
            (Some(BackendName::Openrouter), Some("x-ai/grok-4.5".into()))
        );

        // Backend without a pinned model → only the backend is a default.
        std::fs::write(&path, r#"{ "backend": "ollama" }"#).unwrap();
        assert_eq!(
            persisted_defaults_from(&path),
            (Some(BackendName::Ollama), None)
        );

        // Non-object / invalid JSON is treated as no defaults, never trusted.
        std::fs::write(&path, "not-json {").unwrap();
        assert_eq!(persisted_defaults_from(&path), (None, None));
    }

    #[test]
    fn persist_backend_model_defaults_creates_and_merges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.config.json");

        persist_backend_model_defaults(&path, BackendName::OpenAi, Some("gpt-4o-mini")).unwrap();
        let created: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(created["backend"], "openai");
        assert_eq!(created["modelOverride"], "gpt-4o-mini");

        std::fs::write(
            &path,
            r#"{
  "backend": "ollama",
  "modelOverride": "qwen2.5:7b",
  "approvalPolicy": "dangerous-only",
  "display": { "showBanner": false }
}
"#,
        )
        .unwrap();

        persist_backend_model_defaults(&path, BackendName::Openrouter, Some("x-ai/grok-3"))
            .unwrap();
        let merged: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(merged["backend"], "openrouter");
        assert_eq!(merged["modelOverride"], "x-ai/grok-3");
        assert_eq!(merged["approvalPolicy"], "dangerous-only");
        assert_eq!(merged["display"]["showBanner"], false);
    }

    #[test]
    fn persist_backend_model_defaults_clears_model_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.config.json");
        std::fs::write(
            &path,
            r#"{"backend":"openai","modelOverride":"gpt-4o","tools":["shell"]}"#,
        )
        .unwrap();

        persist_backend_model_defaults(&path, BackendName::Ollama, None).unwrap();
        let merged: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(merged["backend"], "ollama");
        assert!(merged.get("modelOverride").is_none());
        assert_eq!(merged["tools"], json!(["shell"]));
    }

    #[test]
    fn persist_backend_model_defaults_refuses_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.config.json");
        std::fs::write(&path, "not-json {").unwrap();
        let err = persist_backend_model_defaults(&path, BackendName::Ollama, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid JSON"), "unexpected error: {msg}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not-json {");
    }

    #[test]
    fn persist_backend_model_defaults_refuses_non_object_root() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.config.json");
        std::fs::write(&path, r#"[1, 2, 3]"#).unwrap();
        let err = persist_backend_model_defaults(&path, BackendName::Ollama, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("root must be a JSON object"),
            "unexpected error: {msg}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"[1, 2, 3]"#);
    }

    #[test]
    fn parses_openrouter_fusion_config() {
        let file: FileConfig = serde_json::from_str(
            r#"{
              "openrouter": {
                "fusion": {
                  "enabled": true,
                  "analysisModels": ["~openai/gpt-latest", "deepseek/deepseek-v3.2"],
                  "judgeModel": "~anthropic/claude-opus-latest",
                  "maxToolCalls": 4
                }
              }
            }"#,
        )
        .unwrap();
        let fusion = file.openrouter.unwrap().fusion;
        assert!(fusion.enabled);
        assert_eq!(
            fusion.analysis_models,
            vec!["~openai/gpt-latest", "deepseek/deepseek-v3.2"]
        );
        assert_eq!(
            fusion.judge_model.as_deref(),
            Some("~anthropic/claude-opus-latest")
        );
        assert_eq!(fusion.max_tool_calls, Some(4));
    }

    #[test]
    fn parses_model_system_config() {
        let file: FileConfig = serde_json::from_str(
            r#"{
              "modelSystem": {
                "enabled": true,
                "selector": {
                  "backend": "openrouter",
                  "model": "openrouter/fusion",
                  "effort": "high",
                  "thinkingDepth": "deep"
                },
                "orchestrators": {
                  "low": { "backend": "ollama", "model": "qwen2.5-coder:7b" },
                  "medium": { "backend": "openrouter", "model": "qwen/qwen-2.5-coder-32b-instruct" },
                  "high": { "backend": "openrouter", "model": "anthropic/claude-sonnet-4.5" }
                },
                "coders": {
                  "low": { "backend": "ollama", "model": "qwen2.5-coder:7b" },
                  "medium": { "backend": "openrouter", "model": "qwen/qwen-2.5-coder-32b-instruct" },
                  "high": { "backend": "openrouter", "model": "anthropic/claude-sonnet-4.5" }
                },
                "reviewers": {
                  "play": { "backend": "ollama", "model": "qwen2.5-coder:7b" },
                  "production": { "backend": "openrouter", "model": "openrouter/fusion" }
                },
                "securityReviewer": {
                  "backend": "openrouter",
                  "model": "openrouter/fusion"
                }
              }
            }"#,
        )
        .unwrap();
        let stack = file.model_system.unwrap();
        assert!(stack.enabled);
        assert_eq!(
            stack
                .coders
                .high
                .as_ref()
                .map(|m| (m.backend, m.model.as_str())),
            Some((BackendName::Openrouter, "anthropic/claude-sonnet-4.5"))
        );
        assert_eq!(
            stack.security_reviewer.as_ref().map(|m| m.model.as_str()),
            Some("openrouter/fusion")
        );
    }

    #[test]
    fn parses_fable_usage_config() {
        let file: FileConfig = serde_json::from_str(
            r#"{
              "fable": {
                "enabled": true,
                "weeklyTokenBudget": 200000,
                "capShare": 0.5,
                "weekStartsOn": "sunday",
                "fableModelMatches": ["claude-fable", "fable"],
                "claudeModelMatches": ["anthropic/", "claude"]
              }
            }"#,
        )
        .unwrap();
        let fable = file.fable.unwrap();

        assert!(fable.enabled);
        assert_eq!(fable.weekly_token_budget, Some(200_000));
        assert_eq!(fable.cap_share, 0.5);
        assert_eq!(fable.week_starts_on, "sunday");
        assert_eq!(
            fable.fable_model_matches,
            vec!["claude-fable".to_string(), "fable".to_string()]
        );
        assert_eq!(
            fable.claude_model_matches,
            vec!["anthropic/".to_string(), "claude".to_string()]
        );
    }

    #[test]
    fn parses_hooks_config() {
        let file: FileConfig = serde_json::from_str(
            r#"{
              "hooks": {
                "PreToolUse": [
                  {
                    "matcher": "shell",
                    "hooks": [
                      {
                        "type": "command",
                        "command": "$HOME/bin/check",
                        "timeoutSec": 5,
                        "env": { "STATIC_FLAG": "1" },
                        "envVars": ["ZENTTY_PANE_TOKEN"]
                      }
                    ]
                  }
                ]
              }
            }"#,
        )
        .unwrap();
        let hooks: crate::hooks::HookConfig = serde_json::from_value(file.hooks.unwrap()).unwrap();
        let groups = hooks.groups_for(crate::hooks::HookEventName::PreToolUse);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].matcher.as_deref(), Some("shell"));
        assert_eq!(groups[0].hooks[0].command, "$HOME/bin/check");
        assert_eq!(groups[0].hooks[0].timeout_sec, 5);
        assert_eq!(
            groups[0].hooks[0]
                .env
                .get("STATIC_FLAG")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(groups[0].hooks[0].env_vars, vec!["ZENTTY_PANE_TOKEN"]);
    }

    #[test]
    fn malformed_hooks_do_not_break_other_file_config() {
        let file: FileConfig = serde_json::from_str(
            r#"{
              "maxSteps": 7,
              "hooks": {
                "PreToolUse": [
                  {
                    "hooks": [
                      {
                        "type": "unknown",
                        "command": "echo nope"
                      }
                    ]
                  }
                ]
              }
            }"#,
        )
        .unwrap();

        assert_eq!(file.max_steps, Some(7));
        assert!(file.hooks.is_some());
        assert!(serde_json::from_value::<crate::hooks::HookConfig>(file.hooks.unwrap()).is_err());
    }

    #[test]
    fn malformed_hook_section_warns_and_keeps_valid_sections() {
        let mut warnings = Vec::new();
        let hooks = parse_hooks_config(
            serde_json::json!({
                "PreToolUse": [
                    {
                        "hooks": [
                            {
                                "type": "unknown",
                                "command": "echo nope"
                            }
                        ]
                    }
                ],
                "Stop": [
                    {
                        "hooks": [
                            {
                                "type": "command",
                                "command": "echo stop"
                            }
                        ]
                    }
                ]
            }),
            |warning| warnings.push(warning.to_string()),
        );

        assert!(hooks.groups_for(crate::hooks::HookEventName::PreToolUse)[0]
            .hooks
            .is_empty());
        assert_eq!(
            hooks.groups_for(crate::hooks::HookEventName::Stop)[0].hooks[0].command,
            "echo stop"
        );
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("hooks.PreToolUse[0].hooks[0] ignored")));
    }

    #[test]
    fn invalid_top_level_hooks_shape_warns_and_loads_empty_hooks() {
        let mut warnings = Vec::new();
        let hooks = parse_hooks_config(serde_json::json!("nope"), |warning| {
            warnings.push(warning.to_string())
        });

        assert!(hooks
            .groups_for(crate::hooks::HookEventName::Stop)
            .is_empty());
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("hooks ignored")));
    }

    #[test]
    fn unknown_hook_event_warns_instead_of_silent_ignore() {
        let mut warnings = Vec::new();
        let hooks = parse_hooks_config(
            serde_json::json!({
                "PreToolUes": [
                    {
                        "hooks": [
                            {
                                "type": "command",
                                "command": "echo typo"
                            }
                        ]
                    }
                ]
            }),
            |warning| warnings.push(warning.to_string()),
        );

        assert!(hooks
            .groups_for(crate::hooks::HookEventName::PreToolUse)
            .is_empty());
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("unknown hook event")));
    }

    #[test]
    fn project_hook_state_warns_and_is_ignored() {
        let mut warnings = Vec::new();
        let hooks = parse_hooks_config(
            serde_json::json!({
                "state": {
                    "project:/repo/agent.config.json:PreToolUse:0:0": {
                        "enabled": true,
                        "trustedHash": "sha256:abc"
                    }
                }
            }),
            |warning| warnings.push(warning.to_string()),
        );

        assert!(hooks
            .groups_for(crate::hooks::HookEventName::PreToolUse)
            .is_empty());
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("hooks.state ignored")));
    }

    #[test]
    fn malformed_hook_in_event_does_not_drop_valid_sibling_hook() {
        let mut warnings = Vec::new();
        let hooks = parse_hooks_config(
            serde_json::json!({
                "PreToolUse": [
                    {
                        "matcher": "shell",
                        "hooks": [
                            {
                                "type": "unknown",
                                "command": "echo typo"
                            },
                            {
                                "type": "command",
                                "command": "echo valid"
                            }
                        ]
                    }
                ]
            }),
            |warning| warnings.push(warning.to_string()),
        );

        let groups = hooks.groups_for(crate::hooks::HookEventName::PreToolUse);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].hooks.len(), 1);
        assert_eq!(groups[0].hooks[0].command, "echo valid");
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("hooks.PreToolUse[0].hooks[0] ignored")));
    }
}
