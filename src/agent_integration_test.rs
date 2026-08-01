//! Integration tests for the agent loop using a mock OpenAI-compatible SSE server.
//! These validate harness behavior without a live LLM.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::{run_agent, AgentEvent, AgentHooks};
use crate::agent_eval::{evaluate_checks, AgentEvalCheck, AgentEvalFixture};
use crate::backends::{BackendDescriptor, BackendName};
use crate::config::AgentConfig;
use crate::openai::{build_http_client, ChatMessage};
use crate::tools::{build_tools_for_names, SubagentTool, Tool};

struct DenyApproval;

#[async_trait::async_trait]
impl crate::agent::ApprovalProvider for DenyApproval {
    async fn approve(
        &mut self,
        _name: &str,
        _args: &serde_json::Value,
        _preview: Option<&crate::tools::ToolPreview>,
    ) -> bool {
        false
    }
}

fn mock_backend(listener: &TcpListener) -> BackendDescriptor {
    BackendDescriptor {
        name: BackendName::Ollama,
        base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
        api_key: "test".into(),
        is_local: true,
        openrouter: crate::backends::OpenRouterConfig::default(),
    }
}

fn spawn_mock_server(
    listener: TcpListener,
    bodies: Vec<&'static str>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for body in bodies {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        }
    })
}

fn fixture_workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("evals/fixtures/fix-failing-test")
}

#[tokio::test]
async fn read_and_explain_mock_loop() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let backend_desc = mock_backend(&listener);
    let tool_call_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":",
        "{\"name\":\"file_read\",\"arguments\":\"{\\\"path\\\":\\\"src/lib.rs\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n"
    );
    let answer_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"The add function sums two integers.\"}}]}\n\n",
        "data: [DONE]\n\n"
    );
    let server = spawn_mock_server(listener, vec![tool_call_body, answer_body]);

    let mut config = AgentConfig::default();
    config.workspace_root = fixture_workspace().display().to_string();
    config.approval_policy = crate::config::ApprovalPolicy::Never;
    config.tools = vec!["file_read".into()];
    config.tool_selection = crate::config::ToolSelection::Fixed;

    let messages = vec![
        ChatMessage::System {
            content: "test".into(),
        },
        ChatMessage::User {
            content: "Read src/lib.rs and explain add.".into(),
        },
    ];
    let tools = build_tools_for_names(&config, &config.tools, None);
    let http = build_http_client();
    let mut tool_calls = Vec::new();
    let mut lifecycle = Vec::new();
    let mut model_requests = 0usize;

    let run = run_agent(
        &http,
        &backend_desc,
        "mock",
        None,
        messages,
        tools,
        6,
        |event| match event {
            AgentEvent::ModelRequestStarted => {
                model_requests += 1;
                lifecycle.push(format!("model-request:{model_requests}"));
            }
            AgentEvent::ToolCall { name, call_id, .. } => {
                tool_calls.push(name);
                lifecycle.push(format!("announced:{call_id}"));
            }
            AgentEvent::ToolExecutionStarted { call_id, .. } => {
                lifecycle.push(format!("running:{call_id}"));
            }
            AgentEvent::ToolExecutionFinished { call_id, .. } => {
                lifecycle.push(format!("execution-finished:{call_id}"));
            }
            AgentEvent::ToolResult { call_id, .. } => {
                lifecycle.push(format!("receipt:{call_id}"));
            }
            _ => {}
        },
        None,
        None,
        None,
        None,
        None,
        0,
        None,
    )
    .await
    .unwrap();

    server.join().unwrap();

    let fixture = AgentEvalFixture {
        id: "mock-read".into(),
        prompt: String::new(),
        workspace: None,
        checks: vec![
            AgentEvalCheck::ToolUsed {
                name: "file_read".into(),
            },
            AgentEvalCheck::AssistantMentions {
                needle: "add".into(),
            },
        ],
        fixture_root: None,
    };
    let checks = evaluate_checks(&fixture_workspace(), &fixture.checks, &run, &tool_calls);
    assert!(checks.iter().all(|c| c.passed), "{checks:?}");
    assert_eq!(
        lifecycle,
        [
            "model-request:1",
            "announced:call_1",
            "running:call_1",
            "execution-finished:call_1",
            "receipt:call_1",
            "model-request:2"
        ]
    );
    assert!(!run.hit_step_limit);
}

#[tokio::test]
async fn denied_tools_never_emit_running_activity() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let backend_desc = mock_backend(&listener);
    let tool_call_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":",
        "{\"name\":\"shell\",\"arguments\":\"{\\\"command\\\":\\\"rm -f denied.txt\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n"
    );
    let answer_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Denied.\"}}]}\n\n",
        "data: [DONE]\n\n"
    );
    let server = spawn_mock_server(listener, vec![tool_call_body, answer_body]);

    let mut config = AgentConfig::default();
    config.workspace_root = fixture_workspace().display().to_string();
    config.tools = vec!["shell".into()];
    config.tool_selection = crate::config::ToolSelection::Fixed;

    let tools = build_tools_for_names(&config, &config.tools, None);
    let http = build_http_client();
    let mut approval = DenyApproval;
    let mut lifecycle = Vec::new();

    run_agent(
        &http,
        &backend_desc,
        "mock",
        None,
        vec![ChatMessage::User {
            content: "Delete denied.txt".into(),
        }],
        tools,
        6,
        |event| match event {
            AgentEvent::ToolCall { call_id, .. } => lifecycle.push(format!("announced:{call_id}")),
            AgentEvent::ToolExecutionStarted { call_id, .. } => {
                lifecycle.push(format!("running:{call_id}"))
            }
            AgentEvent::ToolExecutionFinished { call_id, .. } => {
                lifecycle.push(format!("execution-finished:{call_id}"))
            }
            AgentEvent::ToolExecutionSkipped { call_id, .. } => {
                lifecycle.push(format!("execution-skipped:{call_id}"))
            }
            AgentEvent::ToolResult { call_id, .. } => lifecycle.push(format!("receipt:{call_id}")),
            _ => {}
        },
        Some(&mut approval),
        None,
        None,
        None,
        None,
        0,
        None,
    )
    .await
    .unwrap();

    server.join().unwrap();
    assert_eq!(
        lifecycle,
        [
            "announced:call_1",
            "execution-skipped:call_1",
            "receipt:call_1"
        ]
    );
    assert!(!fixture_workspace().join("denied.txt").exists());
}

#[tokio::test]
async fn step_limit_surfaces_hit_step_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let backend_desc = mock_backend(&listener);
    let tool_call_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":",
        "{\"name\":\"file_read\",\"arguments\":\"{\\\"path\\\":\\\"src/lib.rs\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"function\":",
        "{\"name\":\"file_read\",\"arguments\":\"{\\\"path\\\":\\\"Cargo.toml\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n"
    );
    let server = spawn_mock_server(listener, vec![tool_call_body, tool_call_body]);

    let mut config = AgentConfig::default();
    config.workspace_root = fixture_workspace().display().to_string();
    config.approval_policy = crate::config::ApprovalPolicy::Never;
    config.tools = vec!["file_read".into()];
    config.tool_selection = crate::config::ToolSelection::Fixed;

    let messages = vec![ChatMessage::User {
        content: "keep reading".into(),
    }];
    let tools = build_tools_for_names(&config, &config.tools, None);
    let http = build_http_client();
    let mut hit_event = false;

    let run = run_agent(
        &http,
        &backend_desc,
        "mock",
        None,
        messages,
        tools,
        2,
        |event| {
            if matches!(event, AgentEvent::StepLimitReached { .. }) {
                hit_event = true;
            }
        },
        None,
        None,
        None,
        None,
        None,
        0,
        None,
    )
    .await
    .unwrap();

    server.join().unwrap();
    assert!(run.hit_step_limit);
    assert!(hit_event);
}

#[tokio::test]
async fn pre_tool_use_hook_block_returns_tool_error() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let backend_desc = mock_backend(&listener);
    let tool_call_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":",
        "{\"name\":\"file_read\",\"arguments\":\"{\\\"path\\\":\\\"src/lib.rs\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n"
    );
    let answer_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Blocked.\"}}]}\n\n",
        "data: [DONE]\n\n"
    );
    let server = spawn_mock_server(listener, vec![tool_call_body, answer_body]);

    let mut config = AgentConfig::default();
    config.workspace_root = fixture_workspace().display().to_string();
    config.approval_policy = crate::config::ApprovalPolicy::Never;
    config.tools = vec!["file_read".into()];
    config.tool_selection = crate::config::ToolSelection::Fixed;

    let mut hook_config = crate::hooks::HookConfig::default();
    hook_config
        .pre_tool_use
        .push(crate::hooks::HookGroupConfig {
            matcher: Some("file_read".into()),
            hooks: vec![crate::hooks::HookCommandConfig {
                command: "printf '%s' '{\"decision\":\"block\",\"reason\":\"blocked by hook\"}'"
                    .into(),
                timeout_sec: 600,
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });
    let registry =
        crate::hooks::HookRegistry::from_discoveries(vec![crate::hooks::discover_hooks(
            &hook_config,
            crate::hooks::HookSource::managed_launch("test"),
            &crate::hooks::HookStateStore::default(),
        )]);
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("s.jsonl");
    let trace = crate::turn_trace::shared_trace(&session, true).unwrap();
    if let Ok(mut trace_guard) = trace.lock() {
        trace_guard.begin_turn();
    }
    let hooks = AgentHooks {
        registry,
        context: crate::hooks::HookInvocationContext {
            session_id: "s".into(),
            turn_id: 1,
            cwd: fixture_workspace().display().to_string(),
            workspace_root: fixture_workspace().display().to_string(),
            transcript_path: session.display().to_string(),
            events_path: crate::turn_trace::events_path_for_session(&session)
                .display()
                .to_string(),
            backend: "ollama".into(),
            model: "mock".into(),
            approval_policy: "never".into(),
            source: "test".into(),
        },
        trace,
    };

    let messages = vec![ChatMessage::User {
        content: "read".into(),
    }];
    let tools = build_tools_for_names(&config, &config.tools, None);
    let http = build_http_client();
    let mut hook_notices = 0usize;

    let run = run_agent(
        &http,
        &backend_desc,
        "mock",
        None,
        messages,
        tools,
        6,
        |event| {
            if matches!(event, AgentEvent::HookNotice(_)) {
                hook_notices += 1;
            }
        },
        None,
        None,
        None,
        None,
        None,
        0,
        Some(hooks),
    )
    .await
    .unwrap();

    server.join().unwrap();
    assert!(hook_notices > 0);
    assert!(run.messages.iter().any(|message| matches!(
        message,
        ChatMessage::Tool { content, .. } if content.contains("blocked by hook")
    )));
}

#[tokio::test]
async fn pre_tool_use_hook_stop_ends_loop_without_tool_execution() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let backend_desc = mock_backend(&listener);
    let tool_call_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":",
        "{\"name\":\"file_read\",\"arguments\":\"{\\\"path\\\":\\\"src/lib.rs\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"function\":",
        "{\"name\":\"file_read\",\"arguments\":\"{\\\"path\\\":\\\"Cargo.toml\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n"
    );
    let server = spawn_mock_server(listener, vec![tool_call_body]);

    let mut config = AgentConfig::default();
    config.workspace_root = fixture_workspace().display().to_string();
    config.tools = vec!["file_read".into()];
    config.tool_selection = crate::config::ToolSelection::Fixed;

    let mut hook_config = crate::hooks::HookConfig::default();
    hook_config
        .pre_tool_use
        .push(crate::hooks::HookGroupConfig {
            matcher: Some("file_read".into()),
            hooks: vec![crate::hooks::HookCommandConfig {
                command: "printf '%s' '{\"decision\":\"stop\",\"reason\":\"stopped by hook\"}'"
                    .into(),
                timeout_sec: 600,
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });
    let registry =
        crate::hooks::HookRegistry::from_discoveries(vec![crate::hooks::discover_hooks(
            &hook_config,
            crate::hooks::HookSource::managed_launch("test"),
            &crate::hooks::HookStateStore::default(),
        )]);
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("s.jsonl");
    let trace = crate::turn_trace::shared_trace(&session, true).unwrap();
    if let Ok(mut trace_guard) = trace.lock() {
        trace_guard.begin_turn();
    }
    let hooks = AgentHooks {
        registry,
        context: crate::hooks::HookInvocationContext {
            session_id: "s".into(),
            turn_id: 1,
            cwd: fixture_workspace().display().to_string(),
            workspace_root: fixture_workspace().display().to_string(),
            transcript_path: session.display().to_string(),
            events_path: crate::turn_trace::events_path_for_session(&session)
                .display()
                .to_string(),
            backend: "ollama".into(),
            model: "mock".into(),
            approval_policy: "never".into(),
            source: "test".into(),
        },
        trace,
    };

    let messages = vec![ChatMessage::User {
        content: "read".into(),
    }];
    let tools = build_tools_for_names(&config, &config.tools, None);
    let http = build_http_client();

    let run = run_agent(
        &http,
        &backend_desc,
        "mock",
        None,
        messages,
        tools,
        6,
        |_| {},
        None,
        None,
        None,
        None,
        None,
        0,
        Some(hooks),
    )
    .await
    .unwrap();

    server.join().unwrap();
    assert!(!run.hit_step_limit);
    let stopped_tool_results = run
        .messages
        .iter()
        .filter(|message| {
            matches!(
                message,
                ChatMessage::Tool { content, .. } if content.contains("stopped by hook")
            )
        })
        .count();
    assert_eq!(stopped_tool_results, 2);
    assert!(!run.messages.iter().any(|message| matches!(
        message,
        ChatMessage::Tool { content, .. } if content.contains("pub fn add")
    )));
}

#[tokio::test]
async fn pre_tool_use_hook_stop_suppresses_pending_tools_in_same_batch() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let backend_desc = mock_backend(&listener);
    let tool_call_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":",
        "{\"name\":\"file_read\",\"arguments\":\"{\\\"path\\\":\\\"src/lib.rs\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"function\":",
        "{\"name\":\"file_read\",\"arguments\":\"{\\\"path\\\":\\\"Cargo.toml\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n"
    );
    let server = spawn_mock_server(listener, vec![tool_call_body]);

    let mut config = AgentConfig::default();
    config.workspace_root = fixture_workspace().display().to_string();
    config.approval_policy = crate::config::ApprovalPolicy::Never;
    config.outside_workspace = crate::config::OutsideWorkspace::Allow;
    config.tools = vec!["file_read".into()];
    config.tool_selection = crate::config::ToolSelection::Fixed;

    let mut hook_config = crate::hooks::HookConfig::default();
    hook_config
        .pre_tool_use
        .push(crate::hooks::HookGroupConfig {
            matcher: Some("file_read".into()),
            hooks: vec![crate::hooks::HookCommandConfig {
                command: concat!(
                    "if grep -q 'Cargo.toml'; then ",
                    "printf '%s' '{\"decision\":\"stop\",\"reason\":\"stopped by second hook\"}'; ",
                    "fi"
                )
                .into(),
                timeout_sec: 600,
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });
    let registry =
        crate::hooks::HookRegistry::from_discoveries(vec![crate::hooks::discover_hooks(
            &hook_config,
            crate::hooks::HookSource::managed_launch("test"),
            &crate::hooks::HookStateStore::default(),
        )]);
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("s.jsonl");
    let trace = crate::turn_trace::shared_trace(&session, true).unwrap();
    if let Ok(mut trace_guard) = trace.lock() {
        trace_guard.begin_turn();
    }
    let hooks = AgentHooks {
        registry,
        context: crate::hooks::HookInvocationContext {
            session_id: "s".into(),
            turn_id: 1,
            cwd: fixture_workspace().display().to_string(),
            workspace_root: fixture_workspace().display().to_string(),
            transcript_path: session.display().to_string(),
            events_path: crate::turn_trace::events_path_for_session(&session)
                .display()
                .to_string(),
            backend: "ollama".into(),
            model: "mock".into(),
            approval_policy: "never".into(),
            source: "test".into(),
        },
        trace,
    };

    let messages = vec![ChatMessage::User {
        content: "read".into(),
    }];
    let tools = build_tools_for_names(&config, &config.tools, None);
    let http = build_http_client();

    let run = run_agent(
        &http,
        &backend_desc,
        "mock",
        None,
        messages,
        tools,
        6,
        |_| {},
        None,
        None,
        None,
        None,
        None,
        0,
        Some(hooks),
    )
    .await
    .unwrap();

    server.join().unwrap();
    let tool_contents: Vec<String> = run
        .messages
        .iter()
        .filter_map(|message| {
            if let ChatMessage::Tool { content, .. } = message {
                Some(content.clone())
            } else {
                None
            }
        })
        .collect();
    let stopped_tool_results = tool_contents
        .iter()
        .filter(|content| content.contains("stopped by second hook"))
        .count();
    assert_eq!(stopped_tool_results, 2, "{tool_contents:#?}");
    assert!(!run.messages.iter().any(|message| matches!(
        message,
        ChatMessage::Tool { content, .. } if content.contains("pub fn add")
    )));
}

#[tokio::test]
async fn pre_tool_use_hook_rewrite_updates_executed_and_stored_tool_input() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let backend_desc = mock_backend(&listener);
    let tool_call_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":",
        "{\"name\":\"file_read\",\"arguments\":\"{\\\"path\\\":\\\"src/missing.rs\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n"
    );
    let answer_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Done.\"}}]}\n\n",
        "data: [DONE]\n\n"
    );
    let server = spawn_mock_server(listener, vec![tool_call_body, answer_body]);

    let mut config = AgentConfig::default();
    config.workspace_root = fixture_workspace().display().to_string();
    config.tools = vec!["file_read".into()];
    config.tool_selection = crate::config::ToolSelection::Fixed;
    let rewritten_path = fixture_workspace().join("src/lib.rs");
    let rewritten_path = rewritten_path.display().to_string();
    let hook_effect = serde_json::json!({
        "decision": "allow",
        "updatedInput": {
            "path": rewritten_path
        }
    })
    .to_string();

    let mut hook_config = crate::hooks::HookConfig::default();
    hook_config
        .pre_tool_use
        .push(crate::hooks::HookGroupConfig {
            matcher: Some("file_read".into()),
            hooks: vec![crate::hooks::HookCommandConfig {
                command: format!("printf '%s' {}", shell_quote(&hook_effect)),
                timeout_sec: 600,
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });
    let registry =
        crate::hooks::HookRegistry::from_discoveries(vec![crate::hooks::discover_hooks(
            &hook_config,
            crate::hooks::HookSource::managed_launch("test"),
            &crate::hooks::HookStateStore::default(),
        )]);
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("s.jsonl");
    let trace = crate::turn_trace::shared_trace(&session, true).unwrap();
    if let Ok(mut trace_guard) = trace.lock() {
        trace_guard.begin_turn();
    }
    let hooks = AgentHooks {
        registry,
        context: crate::hooks::HookInvocationContext {
            session_id: "s".into(),
            turn_id: 1,
            cwd: fixture_workspace().display().to_string(),
            workspace_root: fixture_workspace().display().to_string(),
            transcript_path: session.display().to_string(),
            events_path: crate::turn_trace::events_path_for_session(&session)
                .display()
                .to_string(),
            backend: "ollama".into(),
            model: "mock".into(),
            approval_policy: "never".into(),
            source: "test".into(),
        },
        trace,
    };

    let messages = vec![ChatMessage::User {
        content: "read".into(),
    }];
    let tools = build_tools_for_names(&config, &config.tools, None);
    let http = build_http_client();
    let mut event_args = None;

    let run = run_agent(
        &http,
        &backend_desc,
        "mock",
        None,
        messages,
        tools,
        6,
        |event| {
            if let AgentEvent::ToolCall { args, .. } = event {
                event_args = Some(args);
            }
        },
        None,
        None,
        None,
        None,
        None,
        0,
        Some(hooks),
    )
    .await
    .unwrap();

    server.join().unwrap();

    assert_eq!(
        event_args
            .as_ref()
            .and_then(|args| args.get("path"))
            .and_then(serde_json::Value::as_str),
        Some(rewritten_path.as_str())
    );
    assert!(run.messages.iter().any(|message| matches!(
        message,
        ChatMessage::Tool { content, .. } if content.contains("add")
    )));
    let stored_args = run
        .messages
        .iter()
        .find_map(|message| match message {
            ChatMessage::Assistant { tool_calls, .. } => tool_calls.first(),
            _ => None,
        })
        .map(|call| serde_json::from_str::<serde_json::Value>(&call.function.arguments).unwrap())
        .unwrap();
    assert_eq!(stored_args["path"], rewritten_path);
    assert!(run.messages.iter().any(|message| matches!(
        message,
        ChatMessage::User { content }
            if content.as_text().contains("src/missing.rs")
                && content.as_text().contains(rewritten_path.as_str())
                && content.as_text().contains("rewrote")
    )));
}

#[tokio::test]
async fn task_uses_subagent_stop_without_generic_post_tool_use() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let backend_desc = mock_backend(&listener);
    let parent_tool_call_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_task\",\"function\":",
        "{\"name\":\"task\",\"arguments\":\"{\\\"task\\\":\\\"inspect files\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n"
    );
    let subagent_answer_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"subagent summary\"}}]}\n\n",
        "data: [DONE]\n\n"
    );
    let parent_answer_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Done.\"}}]}\n\n",
        "data: [DONE]\n\n"
    );
    let server = spawn_mock_server(
        listener,
        vec![
            parent_tool_call_body,
            subagent_answer_body,
            parent_answer_body,
        ],
    );

    let mut config = AgentConfig::default();
    config.workspace_root = fixture_workspace().display().to_string();
    config.approval_policy = crate::config::ApprovalPolicy::Never;
    config.tools = vec!["task".into()];
    config.tool_selection = crate::config::ToolSelection::Fixed;
    let mut hook_config = crate::hooks::HookConfig::default();
    hook_config
        .subagent_stop
        .push(crate::hooks::HookGroupConfig {
            matcher: Some("task".into()),
            hooks: vec![crate::hooks::HookCommandConfig {
                command:
                    "printf '%s' '{\"decision\":\"block\",\"reason\":\"subagent stop block\"}'"
                        .into(),
                timeout_sec: 600,
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });
    hook_config
        .post_tool_use
        .push(crate::hooks::HookGroupConfig {
            matcher: Some("task".into()),
            hooks: vec![crate::hooks::HookCommandConfig {
                command: "printf '%s' '{\"decision\":\"block\",\"reason\":\"post hook block\"}'"
                    .into(),
                timeout_sec: 600,
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });
    let registry =
        crate::hooks::HookRegistry::from_discoveries(vec![crate::hooks::discover_hooks(
            &hook_config,
            crate::hooks::HookSource::managed_launch("test"),
            &crate::hooks::HookStateStore::default(),
        )]);
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("s.jsonl");
    let trace = crate::turn_trace::shared_trace(&session, true).unwrap();
    if let Ok(mut trace_guard) = trace.lock() {
        trace_guard.begin_turn();
    }
    let hooks = AgentHooks {
        registry,
        context: crate::hooks::HookInvocationContext {
            session_id: "s".into(),
            turn_id: 1,
            cwd: fixture_workspace().display().to_string(),
            workspace_root: fixture_workspace().display().to_string(),
            transcript_path: session.display().to_string(),
            events_path: crate::turn_trace::events_path_for_session(&session)
                .display()
                .to_string(),
            backend: "ollama".into(),
            model: "mock".into(),
            approval_policy: "never".into(),
            source: "test".into(),
        },
        trace,
    };

    let messages = vec![ChatMessage::User {
        content: "delegate".into(),
    }];
    let http = build_http_client();
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(SubagentTool {
        http: http.clone(),
        backend: backend_desc.clone(),
        model: "mock".into(),
        config: config.clone(),
        runtime: None,
    })];

    let run = run_agent(
        &http,
        &backend_desc,
        "mock",
        None,
        messages,
        tools,
        6,
        |_| {},
        None,
        None,
        None,
        None,
        None,
        0,
        Some(hooks),
    )
    .await
    .unwrap();

    server.join().unwrap();

    let tool_output = run
        .messages
        .iter()
        .find_map(|message| match message {
            ChatMessage::Tool { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .unwrap();
    assert!(tool_output.contains("subagent stop block"));
    assert!(!tool_output.contains("post hook block"));
}

#[tokio::test]
async fn plan_updated_hook_uses_raw_update_plan_output_before_compaction() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let backend_desc = mock_backend(&listener);
    let steps: Vec<serde_json::Value> = (0..150)
        .map(|idx| {
            serde_json::json!({
                "step": format!("large plan step {idx} {}", "x".repeat(80)),
                "status": if idx == 0 { "in_progress" } else { "pending" }
            })
        })
        .collect();
    let args = serde_json::json!({ "steps": steps }).to_string();
    let escaped_args = serde_json::to_string(&args).unwrap();
    let tool_call_body = Box::leak(
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"call_plan\",\"function\":{{\"name\":\"update_plan\",\"arguments\":{escaped_args}}}}}]}}}}]}}\n\n\
             data: [DONE]\n\n"
        )
        .into_boxed_str(),
    );
    let answer_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Done.\"}}]}\n\n",
        "data: [DONE]\n\n"
    );
    let server = spawn_mock_server(listener, vec![tool_call_body, answer_body]);

    let mut config = AgentConfig::default();
    config.workspace_root = fixture_workspace().display().to_string();
    config.tools = vec!["update_plan".into()];
    config.tool_selection = crate::config::ToolSelection::Fixed;

    let mut hook_config = crate::hooks::HookConfig::default();
    hook_config
        .plan_updated
        .push(crate::hooks::HookGroupConfig {
            matcher: None,
            hooks: vec![crate::hooks::HookCommandConfig {
                command: "printf '%s' '{\"feedback\":\"plan hook fired\"}'".into(),
                timeout_sec: 600,
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });
    let registry =
        crate::hooks::HookRegistry::from_discoveries(vec![crate::hooks::discover_hooks(
            &hook_config,
            crate::hooks::HookSource::managed_launch("test"),
            &crate::hooks::HookStateStore::default(),
        )]);
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("s.jsonl");
    let trace = crate::turn_trace::shared_trace(&session, true).unwrap();
    if let Ok(mut trace_guard) = trace.lock() {
        trace_guard.begin_turn();
    }
    let hooks = AgentHooks {
        registry,
        context: crate::hooks::HookInvocationContext {
            session_id: "s".into(),
            turn_id: 1,
            cwd: fixture_workspace().display().to_string(),
            workspace_root: fixture_workspace().display().to_string(),
            transcript_path: session.display().to_string(),
            events_path: crate::turn_trace::events_path_for_session(&session)
                .display()
                .to_string(),
            backend: "ollama".into(),
            model: "mock".into(),
            approval_policy: "never".into(),
            source: "test".into(),
        },
        trace,
    };

    let messages = vec![ChatMessage::User {
        content: "plan".into(),
    }];
    let tools = build_tools_for_names(&config, &config.tools, None);
    let http = build_http_client();
    let mut plan_hook_fired = false;

    let _run = run_agent(
        &http,
        &backend_desc,
        "mock",
        None,
        messages,
        tools,
        6,
        |event| {
            if let AgentEvent::HookNotice(notice) = event {
                if notice.message.contains("plan hook fired") {
                    plan_hook_fired = true;
                }
            }
        },
        None,
        None,
        None,
        None,
        None,
        0,
        Some(hooks),
    )
    .await
    .unwrap();

    server.join().unwrap();
    assert!(plan_hook_fired);
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
