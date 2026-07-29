use anyhow::{anyhow, Result};

use crate::app_state::AppState;
use crate::mcp::{configured_trust, spawn_configured, trust_server, McpTrustStatus};

use super::{DIM, GREEN, RESET, YELLOW};

pub(super) async fn cmd_mcp(args: &str, state: &mut AppState) -> Result<()> {
    let mut parts = args.split_whitespace();
    match parts.next() {
        None | Some("") | Some("list") => print_servers(state),
        Some("trust") => {
            let name = parts
                .next()
                .ok_or_else(|| anyhow!("Usage: /mcp trust <server>"))?;
            trust_and_start(state, &[name.to_string()]).await?;
        }
        Some("trust-all") => {
            let names = state.config.mcp_servers.keys().cloned().collect::<Vec<_>>();
            trust_and_start(state, &names).await?;
        }
        Some(_) => println!("  {DIM}Usage: /mcp [list|trust <server>|trust-all]{RESET}"),
    }
    Ok(())
}

fn print_servers(state: &AppState) {
    if state.config.mcp_servers.is_empty() {
        println!("  {DIM}No MCP servers configured.{RESET}");
        return;
    }
    println!("  {GREEN}MCP servers{RESET}");
    for entry in configured_trust(&state.config.mcp_servers, &state.config.workspace_root) {
        let status = match entry.status {
            McpTrustStatus::Trusted => format!("{GREEN}[trusted]{RESET}"),
            McpTrustStatus::Modified => format!("{YELLOW}[modified]{RESET}"),
            McpTrustStatus::Untrusted => format!("{YELLOW}[new]{RESET}"),
        };
        println!("  {status} {} {DIM}{}{RESET}", entry.name, entry.hash);
    }
}

async fn trust_and_start(state: &mut AppState, names: &[String]) -> Result<()> {
    let mut selected = std::collections::BTreeMap::new();
    for name in names {
        let cfg = state
            .config
            .mcp_servers
            .get(name)
            .ok_or_else(|| anyhow!("unknown MCP server: {name}"))?;
        trust_server(&state.config.workspace_root, name, cfg)?;
        selected.insert(name.clone(), cfg.clone());
    }

    let already_loaded = |server: &str, state: &AppState| {
        let prefix = format!("mcp__{server}__");
        state
            .mcp_tools
            .iter()
            .any(|tool| tool.name().starts_with(&prefix))
    };
    selected.retain(|name, _| !already_loaded(name, state));
    let (tools, errors) = spawn_configured(&selected).await;
    let loaded = tools.len();
    state.mcp_tools.extend(tools);
    for error in errors {
        println!("  {YELLOW}!{RESET} {DIM}MCP: {error}{RESET}");
    }
    println!(
        "  {GREEN}✓{RESET} {DIM}trusted {} server(s); loaded {loaded} tool(s){RESET}",
        names.len()
    );
    Ok(())
}
