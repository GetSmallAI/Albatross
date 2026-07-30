use anyhow::Result;
use std::path::PathBuf;

use crate::agent_eval::AgentEvalCheckResult;
use crate::approval::ApprovalCache;
use crate::backends::{default_model, validate, BackendDescriptor};
use crate::config::AgentConfig;
use crate::hooks::HookRegistry;
use crate::model_system::EffortLevel;
use crate::openai::ChatMessage;
use crate::renderer::TuiRenderer;
use crate::route_audit::ActiveRouteContext;
use crate::session_paths::PathStore;
use crate::tools::Tool;
use crate::turn_checkpoint::CheckpointStack;
use crate::turn_trace::SharedTurnTrace;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct PlayRestoreSnapshot {
    pub config: AgentConfig,
    pub checkpoints_enabled: bool,
}

#[derive(Debug, Clone)]
pub struct PlaySession {
    pub fixture_id: String,
    pub sandbox_root: PathBuf,
    pub restore: PlayRestoreSnapshot,
}

#[derive(Debug, Clone)]
pub struct PlayScorecard {
    pub fixture_id: String,
    pub model: String,
    pub elapsed_ms: u128,
    pub steps: usize,
    pub tool_calls: Vec<String>,
    pub checks: Vec<AgentEvalCheckResult>,
    pub passed: bool,
}

#[derive(Debug, Clone)]
pub struct FixRestoreSnapshot {
    pub config: AgentConfig,
    pub checkpoints_enabled: bool,
}

pub struct AppState {
    pub config: AgentConfig,
    pub http: reqwest::Client,
    pub backend: BackendDescriptor,
    pub model: String,
    pub active_effort: Option<EffortLevel>,
    /// Most recently applied `/route select` decision. Model-call receipts use
    /// this to correlate subsequent coding turns with the decision that chose
    /// the active model.
    pub active_route: Option<ActiveRouteContext>,
    pub messages: Vec<ChatMessage>,
    pub session_dir: String,
    pub session_path: PathBuf,
    pub total_in: u32,
    pub total_out: u32,
    /// Accumulated USD across every turn in this session. Each turn computes
    /// its cost from *its own* model + rate (not the current model × total
    /// tokens), so switching models mid-session reports correctly.
    pub session_usd: f64,
    /// True once any turn used a model that wasn't in the catalog. The status
    /// line uses this to mark the total as a lower bound rather than lie.
    pub session_cost_has_unknown: bool,
    pub context_guard_notice: Option<String>,
    pub conversation_summary: Option<String>,
    pub checkpoint_stack: CheckpointStack,
    pub checkpoints_enabled: bool,
    pub play_session: Option<PlaySession>,
    pub last_play_scorecard: Option<PlayScorecard>,
    pub approval_cache: ApprovalCache,
    pub renderer: TuiRenderer,
    pub hooks: HookRegistry,
    /// Hook context that should be visible for the active session, such as
    /// SessionStart feedback. This is merged into each turn's system prompt
    /// without being persisted as transcript messages.
    pub session_hook_contexts: Vec<String>,
    /// Hook context queued for the next turn only. Stop hooks use this to
    /// hand feedback to the next model call without unbounded transcript
    /// growth across looped runs.
    pub pending_hook_contexts: Vec<String>,
    pub warmed_fingerprint: Option<u64>,
    pub tests_ran_this_session: bool,
    /// Image attachments staged via `/image <path>` that will be folded into
    /// the next user turn. Cleared after they're sent. Stored as
    /// `data:image/...;base64,...` URLs so resume doesn't have to re-read
    /// the file from disk.
    pub pending_image_attachments: Vec<String>,
    /// Tools sourced from configured MCP servers. Spawned once at startup
    /// (in `main`) and appended to every turn's tool list. Kept as Arcs so
    /// each turn shares the same live JSON-RPC connection per server.
    pub mcp_tools: Vec<Arc<dyn Tool>>,
    pub path_store: PathStore,
    pub trace: SharedTurnTrace,
    pub trace_enabled: bool,
}

impl AppState {
    pub fn workspace_root(&self) -> PathBuf {
        crate::session_paths::workspace_root_path(&self.config)
    }

    pub fn paths_enabled(&self) -> bool {
        self.config.paths.enabled
    }

    pub fn save_active_path_metadata(&self) -> Result<()> {
        let mut metadata = crate::session::load_session_metadata(&self.session_path)?;
        metadata.active_path_id = Some(self.path_store.active_id().to_string());
        crate::session::save_session_metadata(&self.session_path, &metadata)
    }
    pub fn rebuild_client(&mut self) -> Result<()> {
        let new_backend = self.config.backend_descriptor();
        validate(&new_backend)?;
        self.backend = new_backend;
        Ok(())
    }

    pub fn resolve_model(&mut self) {
        self.model = default_model(&self.backend, self.config.model_override.as_deref());
    }

    pub fn reset_session(&mut self) {
        self.session_path = crate::session::new_session_path(&self.session_dir);
        self.path_store = PathStore::new(&self.session_dir, &self.session_path, &self.config.paths);
        self.active_route = None;
        if let Ok(mut trace) = self.trace.lock() {
            trace.begin_turn();
        }
    }

    pub fn reset_trace_for_session(&mut self) -> anyhow::Result<()> {
        crate::turn_trace::sync_trace_path(&self.session_path, &self.trace)
    }

    pub fn in_play_session(&self) -> bool {
        self.play_session.is_some()
    }
}
