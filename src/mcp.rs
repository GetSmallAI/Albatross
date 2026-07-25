use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};

use crate::tools::{Tool, ToolPreview};

/// Map of in-flight JSON-RPC requests indexed by id, with a one-shot
/// channel sender ready to receive each response.
type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value>>>>>;

/// Per-server MCP configuration as parsed from agent.config.json.
///
/// Mirrors the `mcpServers` map shape used by other MCP-aware clients —
/// `{ "name": { "command": "...", "args": ["..."], "env": {...} } }` — so
/// config files are portable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Public summary of a tool exposed by an MCP server.
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Result of a tools/call. The Display impl returns the joined text content,
/// matching the lossy form MCP servers usually intend for LLM consumption.
#[derive(Debug, Clone)]
pub struct McpToolResult {
    pub is_error: bool,
    pub text: String,
}

/// Live JSON-RPC client over a child process's stdin/stdout.
///
/// One reader task drains stdout and dispatches responses to waiting
/// callers by request id. Calls go through the shared mutex on `stdin`.
pub struct McpClient {
    next_id: std::sync::atomic::AtomicI64,
    pending: PendingMap,
    stdin: Mutex<ChildStdin>,
    // Hold the Child so it's killed when the client is dropped.
    _child: Child,
}

impl McpClient {
    /// Spawn an MCP server process and run the initialize handshake.
    pub async fn spawn(name: &str, cfg: &McpServerConfig) -> Result<Self> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args);
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning MCP server `{name}` ({})", cfg.command))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("MCP server `{name}` exposed no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("MCP server `{name}` exposed no stdout"))?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_for_reader = pending.clone();
        let server_name = name.to_string();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                handle_incoming(&server_name, &line, &pending_for_reader).await;
            }
            // Server died — fail every pending request so callers don't hang.
            let mut guard = pending_for_reader.lock().await;
            for (_, tx) in guard.drain() {
                let _ = tx.send(Err(anyhow!(
                    "MCP server `{server_name}` closed before responding"
                )));
            }
        });

        let client = Self {
            next_id: std::sync::atomic::AtomicI64::new(1),
            pending,
            stdin: Mutex::new(stdin),
            _child: child,
        };
        client.initialize(name).await?;
        Ok(client)
    }

    async fn initialize(&self, name: &str) -> Result<()> {
        let _ = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "albatross",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await
            .with_context(|| format!("initialize call to MCP server `{name}` failed"))?;
        // MCP requires a notification (no id) after initialize succeeds.
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>> {
        let resp = self.request("tools/list", json!({})).await?;
        let mut out = Vec::new();
        let Some(tools) = resp.get("tools").and_then(Value::as_array) else {
            return Ok(out);
        };
        for tool in tools {
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if name.is_empty() {
                continue;
            }
            let description = tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let input_schema = tool
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"}));
            out.push(McpToolInfo {
                name,
                description,
                input_schema,
            });
        }
        Ok(out)
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<McpToolResult> {
        let resp = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await?;
        let is_error = resp
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut text = String::new();
        if let Some(parts) = resp.get("content").and_then(Value::as_array) {
            for part in parts {
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(t);
                    }
                }
            }
        }
        Ok(McpToolResult { is_error, text })
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.pending.lock().await;
            guard.insert(id, tx);
        }
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_string(&frame)?;
        line.push('\n');
        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(line.as_bytes())
                .await
                .context("writing JSON-RPC request to MCP server")?;
            stdin.flush().await.context("flushing MCP server stdin")?;
        }
        rx.await.context("MCP response channel dropped")?
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let frame = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_string(&frame)?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .context("writing JSON-RPC notification to MCP server")?;
        stdin.flush().await.context("flushing MCP server stdin")?;
        Ok(())
    }
}

async fn handle_incoming(server_name: &str, line: &str, pending: &PendingMap) {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            // Many MCP servers print informational text on startup. Ignore
            // anything that isn't JSON-RPC instead of choking the channel.
            return;
        }
    };
    let id = value.get("id").and_then(Value::as_i64);
    let Some(id) = id else {
        // Server notification — nothing to dispatch. Future: handle
        // notifications/tools/list_changed by re-listing.
        return;
    };
    let tx = {
        let mut guard = pending.lock().await;
        guard.remove(&id)
    };
    let Some(tx) = tx else { return };
    if let Some(err) = value.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("(no message)")
            .to_string();
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
        let _ = tx.send(Err(anyhow!("{server_name} returned error {code}: {msg}")));
        return;
    }
    let result = value.get("result").cloned().unwrap_or(Value::Null);
    let _ = tx.send(Ok(result));
}

/// Adapter that exposes an MCP server's tool through the harness's Tool trait.
///
/// The tool name surfaced to the model is `mcp__<server>__<tool>` so MCP
/// tools never collide with built-ins and are visually distinct in logs.
pub struct McpTool {
    pub display_name: String,
    pub description: String,
    pub schema: Value,
    pub client: Arc<McpClient>,
    pub remote_name: String,
}

impl McpTool {
    pub fn full_name(server: &str, tool: &str) -> String {
        format!("mcp__{server}__{tool}")
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &'static str {
        // Tool::name returns &'static str, but MCP tool names are runtime.
        // Leak the String once during construction so the &str lives for
        // the program lifetime. Acceptable since each tool is constructed
        // exactly once at session start.
        Box::leak(self.display_name.clone().into_boxed_str())
    }
    fn description(&self) -> &'static str {
        Box::leak(self.description.clone().into_boxed_str())
    }
    fn input_schema(&self) -> Value {
        self.schema.clone()
    }
    fn require_approval(&self, _args: &Value) -> bool {
        // MCP tools run external code with their own side effects; always
        // gate them through the harness's approval flow.
        true
    }
    async fn preview(&self, _args: &Value) -> Option<ToolPreview> {
        Some(ToolPreview {
            summary: format!("invoke MCP tool {}", self.display_name),
            diff: None,
            risk: Some("external MCP server side effect".into()),
        })
    }
    async fn execute(&self, args: Value) -> Value {
        match self.client.call_tool(&self.remote_name, args).await {
            Ok(result) => {
                if result.is_error {
                    json!({"error": result.text})
                } else {
                    json!({"output": result.text})
                }
            }
            Err(e) => json!({"error": format!("MCP call failed: {e}")}),
        }
    }
}

/// Spawn every configured MCP server and gather their advertised tools as
/// Tool trait objects ready for `build_tools_for_names` to append.
pub async fn spawn_configured(
    servers: &BTreeMap<String, McpServerConfig>,
) -> (Vec<Arc<dyn Tool>>, Vec<String>) {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for (server_name, cfg) in servers {
        match McpClient::spawn(server_name, cfg).await {
            Ok(client) => {
                let client = Arc::new(client);
                match client.list_tools().await {
                    Ok(remote_tools) => {
                        for info in remote_tools {
                            tools.push(Arc::new(McpTool {
                                display_name: McpTool::full_name(server_name, &info.name),
                                description: info.description,
                                schema: info.input_schema,
                                client: client.clone(),
                                remote_name: info.name,
                            }));
                        }
                    }
                    Err(e) => {
                        errors.push(format!("{server_name}: list_tools failed: {e}"));
                    }
                }
            }
            Err(e) => {
                errors.push(format!("{server_name}: spawn failed: {e}"));
            }
        }
    }
    (tools, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_name_uses_double_underscore_separator() {
        assert_eq!(McpTool::full_name("fs", "read_file"), "mcp__fs__read_file");
    }

    #[test]
    fn server_config_parses_typical_shape() {
        let json = r#"{
            "command": "/usr/local/bin/some-mcp",
            "args": ["--root", "/tmp"],
            "env": { "TOKEN": "abc" }
        }"#;
        let cfg: McpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.command, "/usr/local/bin/some-mcp");
        assert_eq!(cfg.args, vec!["--root", "/tmp"]);
        assert_eq!(cfg.env.get("TOKEN").map(String::as_str), Some("abc"));
    }

    #[test]
    fn server_config_defaults_args_and_env_when_absent() {
        let cfg: McpServerConfig = serde_json::from_str(r#"{"command": "x"}"#).unwrap();
        assert!(cfg.args.is_empty());
        assert!(cfg.env.is_empty());
    }

    #[tokio::test]
    async fn spawn_fails_gracefully_for_missing_binary() {
        let cfg = McpServerConfig {
            command: "/definitely-does-not-exist-binary".into(),
            args: vec![],
            env: BTreeMap::new(),
        };
        let result = McpClient::spawn("missing", &cfg).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn spawn_configured_collects_errors_without_panic() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "bad".to_string(),
            McpServerConfig {
                command: "/definitely-does-not-exist-binary".into(),
                args: vec![],
                env: BTreeMap::new(),
            },
        );
        let (tools, errors) = spawn_configured(&servers).await;
        assert!(tools.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("bad"));
    }
}
