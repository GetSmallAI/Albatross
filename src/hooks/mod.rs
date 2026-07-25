mod config;
mod discovery;
mod effects;
mod events;
mod registry;
mod runner;
mod runtime;
mod state;

pub use config::{
    load_managed_hooks_from_env, HookCommandConfig, HookConfig, HookGroupConfig, HookState,
    ManagedHookConfig,
};
pub use discovery::{
    discover_hooks, matcher_matches, DiscoveredHook, HookDiscovery, HookSource, HookSourceKind,
    HookStateStore, HookTrustStatus,
};
pub use effects::{parse_hook_effect, HookDecision, HookEffect};
pub use events::{HookEventName, HookInvocationContext};
pub use registry::{build_hook_registry, HookDispatch, HookRegistry};
pub use runner::{run_command_hook, HookRunResult};
pub use runtime::{
    bounded_hook_context_text, dispatch_hook_payload, hook_context_messages,
    plan_updated_payload_from_tool_result, render_hook_context_block, HookNotice, HookNoticeLevel,
    HookOutcome,
};
pub use state::{
    hook_state_file_path, load_hook_state_file_from, save_hook_state_file_to, HookStateFile,
};

#[cfg(test)]
mod tests {
    use super::config::default_hook_timeout_sec;
    use super::events::HookPayload;
    use super::registry::HookDispatchResult;
    use super::runtime::summarize_hook_dispatch;
    use super::state::hook_state_file_path_from_env;
    use super::*;
    use serde_json::{json, Value};
    use std::collections::BTreeMap;

    #[test]
    fn parses_hook_config_with_event_groups() {
        let config: HookConfig = serde_json::from_value(json!({
            "PreToolUse": [
                {
                    "matcher": "shell|file_write",
                    "hooks": [
                        {
                            "type": "command",
                            "command": "$HOME/bin/check",
                            "timeoutSec": 7,
                            "statusMessage": "checking policy"
                        }
                    ]
                }
            ]
        }))
        .unwrap();

        let groups = config.groups_for(HookEventName::PreToolUse);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].matcher.as_deref(), Some("shell|file_write"));
        assert_eq!(groups[0].hooks.len(), 1);
        assert_eq!(groups[0].hooks[0].command, "$HOME/bin/check");
        assert_eq!(groups[0].hooks[0].timeout_sec, 7);
        assert_eq!(
            groups[0].hooks[0].status_message.as_deref(),
            Some("checking policy")
        );
    }

    #[test]
    fn discovery_marks_untrusted_modified_trusted_and_managed_hooks() {
        let mut config = HookConfig::default();
        config.pre_tool_use.push(HookGroupConfig {
            matcher: Some("shell".into()),
            hooks: vec![HookCommandConfig {
                command: "echo pre".into(),
                timeout_sec: default_hook_timeout_sec(),
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });

        let source = HookSource::project("agent.config.json");
        let first = discover_hooks(&config, source.clone(), &HookStateStore::default());
        assert_eq!(first.entries.len(), 1);
        assert_eq!(first.entries[0].trust_status, HookTrustStatus::Untrusted);
        assert!(first.runnable.is_empty());

        let mut states = HookStateStore::default();
        states.user.insert(
            first.entries[0].key.clone(),
            HookState {
                enabled: Some(true),
                trusted_hash: Some(first.entries[0].current_hash.clone()),
            },
        );
        let trusted = discover_hooks(&config, source.clone(), &states);
        assert_eq!(trusted.entries[0].trust_status, HookTrustStatus::Trusted);
        assert_eq!(trusted.runnable.len(), 1);

        states.user.insert(
            first.entries[0].key.clone(),
            HookState {
                enabled: Some(true),
                trusted_hash: Some("sha256:old".into()),
            },
        );
        let modified = discover_hooks(&config, source, &states);
        assert_eq!(modified.entries[0].trust_status, HookTrustStatus::Modified);
        assert!(modified.runnable.is_empty());

        let managed = discover_hooks(
            &config,
            HookSource::managed_launch("terminal-orchestrator"),
            &HookStateStore::default(),
        );
        assert_eq!(managed.entries[0].trust_status, HookTrustStatus::Managed);
        assert_eq!(managed.runnable.len(), 1);
    }

    #[test]
    fn discovery_treats_hook_env_changes_as_modified() {
        let mut config = HookConfig::default();
        config.pre_tool_use.push(HookGroupConfig {
            matcher: Some("shell".into()),
            hooks: vec![HookCommandConfig {
                command: "echo pre".into(),
                timeout_sec: default_hook_timeout_sec(),
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });
        let source = HookSource::project("agent.config.json");
        let first = discover_hooks(&config, source.clone(), &HookStateStore::default());
        let mut states = HookStateStore::default();
        states.user.insert(
            first.entries[0].key.clone(),
            HookState {
                enabled: Some(true),
                trusted_hash: Some(first.entries[0].current_hash.clone()),
            },
        );

        config.pre_tool_use[0].hooks[0]
            .env_vars
            .push("FORWARDED".into());
        let modified = discover_hooks(&config, source, &states);

        assert_eq!(modified.entries[0].trust_status, HookTrustStatus::Modified);
        assert!(modified.runnable.is_empty());
    }

    #[test]
    fn default_hook_timeout_matches_codex_default() {
        let config: HookConfig = serde_json::from_value(json!({
            "PreToolUse": [
                {
                    "hooks": [
                        { "type": "command", "command": "echo pre" }
                    ]
                }
            ]
        }))
        .unwrap();

        assert_eq!(
            config.groups_for(HookEventName::PreToolUse)[0].hooks[0].timeout_sec,
            600
        );
    }

    #[test]
    fn hook_config_rejects_invalid_env_var_names() {
        let err = serde_json::from_value::<HookConfig>(json!({
            "PreToolUse": [
                {
                    "hooks": [
                        {
                            "type": "command",
                            "command": "echo pre",
                            "envVars": ["BAD=NAME"]
                        }
                    ]
                }
            ]
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("must not contain `=` or NUL"));
    }

    #[test]
    fn discovery_skips_async_empty_prompt_and_agent_handlers() {
        let config: HookConfig = serde_json::from_value(json!({
            "PreToolUse": [
                {
                    "hooks": [
                        { "type": "command", "command": "echo sync" },
                        { "type": "command", "command": "echo async", "async": true },
                        { "type": "command", "command": "" },
                        { "type": "prompt" },
                        { "type": "agent" }
                    ]
                }
            ]
        }))
        .unwrap();

        let discovery = discover_hooks(
            &config,
            HookSource::managed_launch("test"),
            &HookStateStore::default(),
        );

        assert_eq!(discovery.entries.len(), 1);
        assert_eq!(discovery.entries[0].handler.command, "echo sync");
    }

    #[test]
    fn discovery_ignores_matchers_for_non_matcher_events() {
        let mut config = HookConfig::default();
        config.stop.push(HookGroupConfig {
            matcher: Some("^never$".into()),
            hooks: vec![HookCommandConfig {
                command: "echo stop".into(),
                timeout_sec: default_hook_timeout_sec(),
                command_windows: None,
                status_message: Some("stopping".into()),
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });

        let discovery = discover_hooks(
            &config,
            HookSource::managed_launch("test"),
            &HookStateStore::default(),
        );

        assert_eq!(discovery.entries.len(), 1);
        assert_eq!(discovery.entries[0].matcher, None);
        assert_eq!(discovery.runnable.len(), 1);
    }

    #[test]
    fn matcher_supports_exact_pipe_star_and_regex() {
        assert!(matcher_matches(None, Some("shell")));
        assert!(matcher_matches(Some("*"), Some("shell")));
        assert!(matcher_matches(Some("shell|file_write"), Some("shell")));
        assert!(matcher_matches(
            Some("shell|file_write"),
            Some("file_write")
        ));
        assert!(!matcher_matches(Some("shell|file_write"), Some("grep")));
        assert!(matcher_matches(Some("^file_.*"), Some("file_write")));
        assert!(!matcher_matches(Some("^file_.*"), Some("shell")));
        assert!(!matcher_matches(Some("sh.*"), Some("xshellx")));
        assert!(!matcher_matches(Some("["), Some("shell")));
    }

    #[test]
    fn dot_matchers_use_regex_semantics() {
        assert!(matcher_matches(Some("a.b"), Some("axb")));
        assert!(!matcher_matches(Some("a.b"), Some("a.b.c")));
    }

    #[test]
    fn invalid_matcher_groups_are_visible_but_not_runnable() {
        let mut config = HookConfig::default();
        config.pre_tool_use.push(HookGroupConfig {
            matcher: Some("[".into()),
            hooks: vec![HookCommandConfig {
                command: "echo bad".into(),
                timeout_sec: default_hook_timeout_sec(),
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });

        let discovery = discover_hooks(
            &config,
            HookSource::managed_launch("test"),
            &HookStateStore::default(),
        );

        assert_eq!(discovery.entries.len(), 1);
        assert!(discovery.runnable.is_empty());
        assert_eq!(discovery.entries[0].trust_status, HookTrustStatus::Invalid);
        assert!(discovery.entries[0]
            .matcher_error
            .as_deref()
            .unwrap()
            .contains("regex"));
    }

    #[test]
    fn parses_hook_effects_from_stdout_and_exit_status() {
        let effect = parse_hook_effect(
            0,
            r#"{"decision":"block","reason":"nope","additionalContext":"ctx","updatedInput":{"command":"cargo test"}}"#,
            "",
        );
        assert_eq!(effect.decision, Some(HookDecision::Block));
        assert_eq!(effect.reason.as_deref(), Some("nope"));
        assert_eq!(effect.additional_context.as_deref(), Some("ctx"));
        assert_eq!(
            effect.updated_input.as_ref().unwrap()["command"],
            "cargo test"
        );

        let exit_two = parse_hook_effect(2, "", "denied by policy");
        assert_eq!(exit_two.decision, Some(HookDecision::Block));
        assert_eq!(exit_two.reason.as_deref(), Some("denied by policy"));

        let invalid = parse_hook_effect(0, "{not-json", "");
        assert!(invalid.decision.is_none());
        assert!(invalid
            .warning
            .as_deref()
            .unwrap()
            .contains("invalid hook JSON"));
    }

    #[test]
    fn event_payload_uses_codex_style_common_fields() {
        let payload = HookPayload::new(HookEventName::PlanUpdated, "session-1")
            .turn_id(2)
            .cwd("/repo")
            .transcript_path(".sessions/session-1.jsonl")
            .insert("progress", json!({"done": 1, "total": 3}))
            .into_value();

        assert_eq!(payload["hook_event_name"], "PlanUpdated");
        assert_eq!(payload["session_id"], "session-1");
        assert_eq!(payload["turn_id"], 2);
        assert_eq!(payload["cwd"], "/repo");
        assert_eq!(payload["progress"]["done"], 1);
    }

    #[test]
    fn invocation_context_builds_common_payload_fields() {
        let ctx = HookInvocationContext {
            session_id: "s1".into(),
            turn_id: 4,
            cwd: "/repo".into(),
            workspace_root: "/repo".into(),
            transcript_path: ".sessions/s1.jsonl".into(),
            events_path: ".sessions/s1.events.jsonl".into(),
            backend: "ollama".into(),
            model: "qwen".into(),
            approval_policy: "dangerous-only".into(),
            source: "interactive".into(),
        };
        let payload = ctx.payload(HookEventName::Stop).into_value();

        assert_eq!(payload["hook_event_name"], "Stop");
        assert_eq!(payload["session_id"], "s1");
        assert_eq!(payload["turn_id"], 4);
        assert_eq!(payload["workspace_root"], "/repo");
        assert_eq!(payload["events_path"], ".sessions/s1.events.jsonl");
        assert_eq!(payload["backend"], "ollama");
        assert_eq!(payload["approval_policy"], "dangerous-only");
    }

    #[test]
    fn managed_launch_config_loads_from_env_json_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hooks.json");
        std::fs::write(
            &file,
            r#"{
              "source": "terminal-orchestrator",
              "hooks": {
                "PreToolUse": [
                  {
                    "matcher": "shell",
                    "hooks": [
                      { "type": "command", "command": "echo pre" }
                    ]
                  }
                ]
              }
            }"#,
        )
        .unwrap();
        let file_path = file.display().to_string();

        let managed = load_managed_hooks_from_env(
            Some(
                r#"{
          "hooks": {
            "Stop": [
              { "hooks": [ { "type": "command", "command": "echo stop" } ] }
            ]
          }
        }"#,
            ),
            Some(file_path.as_str()),
        )
        .unwrap()
        .unwrap();

        assert_eq!(managed.source_label, "terminal-orchestrator");
        assert_eq!(managed.hooks.groups_for(HookEventName::PreToolUse).len(), 1);
        assert_eq!(managed.hooks.groups_for(HookEventName::Stop).len(), 1);
    }

    #[test]
    fn managed_launch_config_reports_missing_env_file() {
        let err = load_managed_hooks_from_env(None, Some("/tmp/does-not-exist/hooks.json"))
            .unwrap_err()
            .to_string();

        assert!(err.contains("reading /tmp/does-not-exist/hooks.json"));
    }

    #[test]
    fn managed_launch_config_rejects_incomplete_wrapped_document() {
        let err = load_managed_hooks_from_env(Some(r#"{"source":"terminal-orchestrator"}"#), None)
            .unwrap_err()
            .to_string();

        assert!(err.contains("parsing ALBATROSS_MANAGED_HOOKS_JSON"));
    }

    #[test]
    fn hook_state_file_path_uses_xdg_then_home() {
        assert_eq!(
            hook_state_file_path_from_env(Some("/tmp/xdg"), Some("/tmp/home"))
                .unwrap()
                .display()
                .to_string(),
            "/tmp/xdg/albatross/hooks-state.json"
        );
        assert_eq!(
            hook_state_file_path_from_env(None, Some("/tmp/home"))
                .unwrap()
                .display()
                .to_string(),
            "/tmp/home/.config/albatross/hooks-state.json"
        );
        assert!(hook_state_file_path_from_env(None, None).is_none());
    }

    #[test]
    fn hook_state_file_roundtrips_user_trust() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks-state.json");
        let mut file = HookStateFile::default();
        file.hooks.insert(
            "project:/repo/agent.config.json:PreToolUse:0:0".into(),
            HookState {
                enabled: Some(true),
                trusted_hash: Some("sha256:abc".into()),
            },
        );
        save_hook_state_file_to(&path, &file).unwrap();
        let loaded = load_hook_state_file_from(&path).unwrap();

        assert_eq!(loaded, file);
    }

    #[test]
    fn hook_state_file_ignores_legacy_trusted_projects() {
        let loaded: HookStateFile =
            serde_json::from_str(r#"{"trustedProjects":{"/repo":true},"hooks":{}}"#).unwrap();
        let serialized = serde_json::to_value(&loaded).unwrap();

        assert!(loaded.hooks.is_empty());
        assert!(serialized.get("trustedProjects").is_none());
    }

    #[tokio::test]
    async fn command_runner_sends_payload_on_stdin_and_parses_effect() {
        let dir = tempfile::tempdir().unwrap();
        let captured = dir.path().join("payload.json");
        let command = format!(
            "cat > {}; printf '%s' '{{\"feedback\":\"ok\"}}'",
            shell_quote(&captured.display().to_string())
        );
        let handler = HookCommandConfig {
            command,
            timeout_sec: default_hook_timeout_sec(),
            command_windows: None,
            status_message: None,
            async_handler: false,
            env: Default::default(),
            env_vars: Vec::new(),
        };
        let payload = HookPayload::new(HookEventName::SessionStart, "s1").into_value();

        let result = run_command_hook(&handler, &payload).await;

        assert_eq!(result.exit_code, Some(0));
        assert!(!result.timed_out);
        assert_eq!(result.effect.feedback.as_deref(), Some("ok"));
        let written = std::fs::read_to_string(captured).unwrap();
        let parsed: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(parsed["hook_event_name"], "SessionStart");
        assert_eq!(parsed["session_id"], "s1");
    }

    #[tokio::test]
    async fn command_runner_clears_parent_credentials_from_child_env() {
        let _guard = EnvVarGuard::set("OPENAI_API_KEY", "sk-test-parent-secret");
        let handler = HookCommandConfig {
            command: r#"if [ -n "$OPENAI_API_KEY" ]; then printf '%s' '{"feedback":"leaked"}'; else printf '%s' '{"feedback":"missing"}'; fi"#.into(),
            timeout_sec: default_hook_timeout_sec(),
            command_windows: None,
            status_message: None,
            async_handler: false,
            env: Default::default(),
            env_vars: Vec::new(),
        };
        let payload = HookPayload::new(HookEventName::SessionStart, "s1").into_value();

        let result = run_command_hook(&handler, &payload).await;

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.effect.feedback.as_deref(), Some("missing"));
    }

    #[tokio::test]
    async fn command_runner_forwards_requested_parent_env_vars_only() {
        let _forwarded = EnvVarGuard::set("ALBATROSS_TEST_FORWARD", "forwarded");
        let _blocked = EnvVarGuard::set("ALBATROSS_TEST_BLOCKED", "blocked");
        let handler = HookCommandConfig {
            command: r#"if [ "$ALBATROSS_TEST_FORWARD" = "forwarded" ] && [ -z "$ALBATROSS_TEST_BLOCKED" ]; then printf '%s' '{"feedback":"ok"}'; else printf '%s' '{"feedback":"bad"}'; fi"#.into(),
            timeout_sec: default_hook_timeout_sec(),
            command_windows: None,
            status_message: None,
            async_handler: false,
            env: BTreeMap::new(),
            env_vars: vec!["ALBATROSS_TEST_FORWARD".into()],
        };
        let payload = HookPayload::new(HookEventName::SessionStart, "s1").into_value();

        let result = run_command_hook(&handler, &payload).await;

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.effect.feedback.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn command_runner_adds_literal_hook_env() {
        let mut env = BTreeMap::new();
        env.insert("ALBATROSS_LITERAL".into(), "literal".into());
        let handler = HookCommandConfig {
            command: r#"if [ "$ALBATROSS_LITERAL" = "literal" ]; then printf '%s' '{"feedback":"ok"}'; else printf '%s' '{"feedback":"bad"}'; fi"#.into(),
            timeout_sec: default_hook_timeout_sec(),
            command_windows: None,
            status_message: None,
            async_handler: false,
            env,
            env_vars: Vec::new(),
        };
        let payload = HookPayload::new(HookEventName::SessionStart, "s1").into_value();

        let result = run_command_hook(&handler, &payload).await;

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.effect.feedback.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn command_runner_reserved_hook_env_overrides_configured_env() {
        let _parent = EnvVarGuard::set("ALBATROSS_SESSION_ID", "parent");
        let mut env = BTreeMap::new();
        env.insert("ALBATROSS_SESSION_ID".into(), "literal".into());
        let handler = HookCommandConfig {
            command: r#"if [ "$ALBATROSS_SESSION_ID" = "s1" ]; then printf '%s' '{"feedback":"ok"}'; else printf '%s' '{"feedback":"bad"}'; fi"#.into(),
            timeout_sec: default_hook_timeout_sec(),
            command_windows: None,
            status_message: None,
            async_handler: false,
            env,
            env_vars: vec!["ALBATROSS_SESSION_ID".into()],
        };
        let payload = HookPayload::new(HookEventName::SessionStart, "s1").into_value();

        let result = run_command_hook(&handler, &payload).await;

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.effect.feedback.as_deref(), Some("ok"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_runner_drains_stdout_while_writing_large_payload() {
        let handler = HookCommandConfig {
            command: r#"perl -e 'alarm 5; print " " x 200000; while (<STDIN>) {}; print "{\"feedback\":\"ok\"}"'"#.into(),
            timeout_sec: 3,
            command_windows: None,
            status_message: None,
            async_handler: false,
            env: Default::default(),
            env_vars: Vec::new(),
        };
        let payload = HookPayload::new(HookEventName::PostToolUse, "s1")
            .insert("tool_response", json!("x".repeat(200_000)))
            .into_value();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run_command_hook(&handler, &payload),
        )
        .await
        .expect("hook I/O should complete without hanging before hook timeout");

        assert_eq!(result.exit_code, Some(0));
        assert!(!result.timed_out);
        assert_eq!(result.effect.feedback.as_deref(), Some("ok"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_runner_timeout_bounds_inherited_stdout_handles() {
        let handler = HookCommandConfig {
            command: "sleep 5 &".into(),
            timeout_sec: 1,
            command_windows: None,
            status_message: None,
            async_handler: false,
            env: Default::default(),
            env_vars: Vec::new(),
        };
        let payload = HookPayload::new(HookEventName::SessionStart, "s1").into_value();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run_command_hook(&handler, &payload),
        )
        .await
        .expect("hook reader handles should be bounded by the hook timeout");

        assert!(result.timed_out);
        assert_eq!(result.exit_code, Some(0));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_runner_timeout_kills_background_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("child.pid");
        let command = format!(
            "sleep 5 & printf '%s' \"$!\" > {}",
            shell_quote(&pid_file.display().to_string())
        );
        let handler = HookCommandConfig {
            command,
            timeout_sec: 1,
            command_windows: None,
            status_message: None,
            async_handler: false,
            env: Default::default(),
            env_vars: Vec::new(),
        };
        let payload = HookPayload::new(HookEventName::SessionStart, "s1").into_value();

        let result = run_command_hook(&handler, &payload).await;
        let pid: i32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
        for _ in 0..20 {
            if !process_exists(pid) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        assert!(result.timed_out);
        assert!(
            !process_exists(pid),
            "background hook process {pid} survived timeout"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_runner_preserves_decision_before_inherited_stdout_timeout() {
        let handler = HookCommandConfig {
            command: r#"printf '%s' '{"decision":"block","reason":"already blocked"}'; sleep 5 &"#
                .into(),
            timeout_sec: 1,
            command_windows: None,
            status_message: None,
            async_handler: false,
            env: Default::default(),
            env_vars: Vec::new(),
        };
        let payload = HookPayload::new(HookEventName::PreToolUse, "s1").into_value();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run_command_hook(&handler, &payload),
        )
        .await
        .expect("hook runner should return at the hook timeout");

        assert!(result.timed_out);
        assert_eq!(result.effect.decision, Some(HookDecision::Block));
        assert_eq!(result.effect.reason.as_deref(), Some("already blocked"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_runner_preserves_exit_two_before_inherited_stdout_timeout() {
        let handler = HookCommandConfig {
            command: "sleep 5 & exit 2".into(),
            timeout_sec: 1,
            command_windows: None,
            status_message: None,
            async_handler: false,
            env: Default::default(),
            env_vars: Vec::new(),
        };
        let payload = HookPayload::new(HookEventName::PreToolUse, "s1").into_value();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run_command_hook(&handler, &payload),
        )
        .await
        .expect("hook runner should return at the hook timeout");

        assert!(result.timed_out);
        assert_eq!(result.exit_code, Some(2));
        assert_eq!(result.effect.decision, Some(HookDecision::Block));
    }

    #[test]
    fn command_runner_uses_fixed_platform_shell() {
        let (program, flag) = runner::hook_shell_program_and_arg();

        #[cfg(windows)]
        {
            assert_eq!(program, "cmd.exe");
            assert_eq!(flag, "/C");
        }
        #[cfg(not(windows))]
        {
            assert_eq!(program, "/bin/sh");
            assert_eq!(flag, "-c");
        }
    }

    #[tokio::test]
    async fn command_runner_reports_timeout() {
        let handler = HookCommandConfig {
            command: "sleep 2".into(),
            timeout_sec: 1,
            command_windows: None,
            status_message: None,
            async_handler: false,
            env: Default::default(),
            env_vars: Vec::new(),
        };
        let payload = HookPayload::new(HookEventName::SessionStart, "s1").into_value();

        let result = run_command_hook(&handler, &payload).await;

        assert_eq!(result.exit_code, None);
        assert!(result.timed_out);
        assert!(result
            .effect
            .warning
            .as_deref()
            .unwrap()
            .contains("timed out"));
    }

    #[tokio::test]
    async fn registry_dispatch_filters_by_event_and_matcher() {
        let mut config = HookConfig::default();
        config.pre_tool_use.push(HookGroupConfig {
            matcher: Some("shell|file_write".into()),
            hooks: vec![HookCommandConfig {
                command: "printf '%s' '{\"feedback\":\"hit\"}'".into(),
                timeout_sec: default_hook_timeout_sec(),
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });
        let discovery = discover_hooks(
            &config,
            HookSource::managed_launch("test"),
            &HookStateStore::default(),
        );
        let registry = HookRegistry::from_discoveries(vec![discovery]);
        let payload = HookPayload::new(HookEventName::PreToolUse, "s1").into_value();

        let miss = registry
            .dispatch(HookEventName::PreToolUse, &payload, Some("grep"))
            .await;
        assert!(miss.results.is_empty());

        let hit = registry
            .dispatch(HookEventName::PreToolUse, &payload, Some("shell"))
            .await;
        assert_eq!(hit.results.len(), 1);
        assert_eq!(hit.results[0].run.effect.feedback.as_deref(), Some("hit"));

        let wrong_event = registry
            .dispatch(HookEventName::PostToolUse, &payload, Some("shell"))
            .await;
        assert!(wrong_event.results.is_empty());
    }

    #[tokio::test]
    async fn registry_dispatch_matches_event_name_when_no_matcher_value_is_supplied() {
        let mut config = HookConfig::default();
        config.session_start.push(HookGroupConfig {
            matcher: Some("SessionStart".into()),
            hooks: vec![HookCommandConfig {
                command: "printf '%s' '{\"feedback\":\"hit\"}'".into(),
                timeout_sec: default_hook_timeout_sec(),
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });
        config.pre_compact.push(HookGroupConfig {
            matcher: Some("^PreCompact$".into()),
            hooks: vec![HookCommandConfig {
                command: "printf '%s' '{\"feedback\":\"compact\"}'".into(),
                timeout_sec: default_hook_timeout_sec(),
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });
        let registry = HookRegistry::from_discoveries(vec![discover_hooks(
            &config,
            HookSource::managed_launch("test"),
            &HookStateStore::default(),
        )]);

        let session_payload = HookPayload::new(HookEventName::SessionStart, "s1").into_value();
        let session_hit = registry
            .dispatch(HookEventName::SessionStart, &session_payload, None)
            .await;
        assert_eq!(session_hit.results.len(), 1);
        assert_eq!(
            session_hit.results[0].run.effect.feedback.as_deref(),
            Some("hit")
        );

        let compact_payload = HookPayload::new(HookEventName::PreCompact, "s1").into_value();
        let compact_hit = registry
            .dispatch(HookEventName::PreCompact, &compact_payload, None)
            .await;
        assert_eq!(compact_hit.results.len(), 1);
        assert_eq!(
            compact_hit.results[0].run.effect.feedback.as_deref(),
            Some("compact")
        );
    }

    #[test]
    fn plan_updated_payload_uses_tool_result_progress() {
        let ctx = HookInvocationContext {
            session_id: "s1".into(),
            turn_id: 5,
            cwd: "/repo".into(),
            workspace_root: "/repo".into(),
            transcript_path: ".sessions/s1.jsonl".into(),
            events_path: ".sessions/s1.events.jsonl".into(),
            backend: "ollama".into(),
            model: "qwen".into(),
            approval_policy: "dangerous-only".into(),
            source: "interactive".into(),
        };
        let payload = plan_updated_payload_from_tool_result(
            &ctx,
            "call-1",
            r#"{
              "plan_updated": true,
              "done": 1,
              "total": 3,
              "steps": [
                { "step": "read", "status": "done" },
                { "step": "edit", "status": "in_progress" },
                { "step": "test", "status": "pending" }
              ]
            }"#,
        )
        .unwrap();

        assert_eq!(payload["hook_event_name"], "PlanUpdated");
        assert_eq!(payload["tool_use_id"], "call-1");
        assert_eq!(payload["progress"]["done"], 1);
        assert_eq!(payload["progress"]["total"], 3);
        assert_eq!(payload["active_step"]["step"], "edit");
        assert_eq!(payload["plan"][2]["step"], "test");
    }

    #[test]
    fn hook_outcome_summarizes_visible_effects() {
        let mut config = HookConfig::default();
        config.user_prompt_submit.push(HookGroupConfig {
            matcher: None,
            hooks: vec![
                HookCommandConfig {
                    command: "echo warning".into(),
                    timeout_sec: default_hook_timeout_sec(),
                    command_windows: None,
                    status_message: None,
                    async_handler: false,
                    env: Default::default(),
                    env_vars: Vec::new(),
                },
                HookCommandConfig {
                    command: "echo block".into(),
                    timeout_sec: default_hook_timeout_sec(),
                    command_windows: None,
                    status_message: None,
                    async_handler: false,
                    env: Default::default(),
                    env_vars: Vec::new(),
                },
            ],
        });
        let discovery = discover_hooks(
            &config,
            HookSource::managed_launch("test"),
            &HookStateStore::default(),
        );
        let registry = HookRegistry::from_discoveries(vec![discovery]);
        let first = registry.runnable[0].clone();
        let second = registry.runnable[1].clone();
        let dispatch = HookDispatch {
            results: vec![
                HookDispatchResult {
                    hook: first,
                    run: HookRunResult {
                        exit_code: Some(1),
                        timed_out: false,
                        failure: None,
                        duration_ms: 10,
                        stdout: String::new(),
                        stderr: String::new(),
                        effect: HookEffect {
                            warning: Some("hook exited with status 1".into()),
                            ..HookEffect::default()
                        },
                    },
                },
                HookDispatchResult {
                    hook: second,
                    run: HookRunResult {
                        exit_code: Some(0),
                        timed_out: false,
                        failure: None,
                        duration_ms: 12,
                        stdout: String::new(),
                        stderr: String::new(),
                        effect: HookEffect {
                            decision: Some(HookDecision::Block),
                            reason: Some("prompt rejected".into()),
                            feedback: Some("use a safer prompt".into()),
                            ..HookEffect::default()
                        },
                    },
                },
            ],
        };

        let outcome = summarize_hook_dispatch(HookEventName::UserPromptSubmit, &dispatch);

        assert_eq!(outcome.blocking_reason.as_deref(), Some("prompt rejected"));
        assert_eq!(outcome.notices.len(), 3);
        assert!(outcome
            .notices
            .iter()
            .any(|notice| notice.message.contains("status 1")));
        assert!(outcome
            .notices
            .iter()
            .any(|notice| notice.message.contains("prompt rejected")));
        assert!(outcome
            .notices
            .iter()
            .any(|notice| notice.message.contains("use a safer prompt")));
    }

    #[test]
    fn pre_tool_use_updated_input_requires_allow_and_block_discards_rewrites() {
        let allow_dispatch = dispatch_with_effects(
            HookEventName::PreToolUse,
            vec![HookEffect {
                decision: Some(HookDecision::Allow),
                updated_input: Some(json!({"command": "cargo test"})),
                ..HookEffect::default()
            }],
        );
        let allow_outcome = summarize_hook_dispatch(HookEventName::PreToolUse, &allow_dispatch);
        assert_eq!(
            allow_outcome.updated_input.as_ref().unwrap()["command"],
            "cargo test"
        );

        let no_allow_dispatch = dispatch_with_effects(
            HookEventName::PreToolUse,
            vec![HookEffect {
                updated_input: Some(json!({"command": "cargo fmt"})),
                ..HookEffect::default()
            }],
        );
        let no_allow_outcome =
            summarize_hook_dispatch(HookEventName::PreToolUse, &no_allow_dispatch);
        assert!(no_allow_outcome.updated_input.is_none());

        let blocked_dispatch = dispatch_with_effects(
            HookEventName::PreToolUse,
            vec![
                HookEffect {
                    decision: Some(HookDecision::Allow),
                    updated_input: Some(json!({"command": "cargo test"})),
                    ..HookEffect::default()
                },
                HookEffect {
                    decision: Some(HookDecision::Block),
                    reason: Some("blocked".into()),
                    updated_input: Some(json!({"command": "cargo fmt"})),
                    ..HookEffect::default()
                },
            ],
        );
        let blocked_outcome = summarize_hook_dispatch(HookEventName::PreToolUse, &blocked_dispatch);
        assert_eq!(blocked_outcome.blocking_reason.as_deref(), Some("blocked"));
        assert!(!blocked_outcome.allowed);
        assert!(blocked_outcome.updated_input.is_none());

        let stopped_dispatch = dispatch_with_effects(
            HookEventName::PreToolUse,
            vec![
                HookEffect {
                    decision: Some(HookDecision::Allow),
                    updated_input: Some(json!({"command": "cargo test"})),
                    ..HookEffect::default()
                },
                HookEffect {
                    decision: Some(HookDecision::Stop),
                    reason: Some("stopped".into()),
                    ..HookEffect::default()
                },
            ],
        );
        let stopped_outcome = summarize_hook_dispatch(HookEventName::PreToolUse, &stopped_dispatch);
        assert_eq!(stopped_outcome.stop_reason.as_deref(), Some("stopped"));
        assert!(!stopped_outcome.allowed);
        assert!(stopped_outcome.updated_input.is_none());
    }

    #[test]
    fn hook_decision_reasons_are_redacted_and_bounded() {
        let dispatch = dispatch_with_effects(
            HookEventName::PreToolUse,
            vec![HookEffect {
                decision: Some(HookDecision::Block),
                reason: Some(format!("sk-secret123\n{}", "x".repeat(20_000))),
                ..HookEffect::default()
            }],
        );

        let outcome = summarize_hook_dispatch(HookEventName::PreToolUse, &dispatch);
        let reason = outcome.blocking_reason.unwrap();

        assert!(!reason.contains("sk-secret123"));
        assert!(reason.contains("(redacted)"));
        assert!(reason.contains("\\n"));
        assert!(!reason.contains('\n'));
        assert!(reason.contains("[truncated]"));
        assert!(reason.len() < 3_000);
    }

    #[test]
    fn synthesized_fail_closed_block_uses_runner_failure_reason() {
        let mut config = HookConfig::default();
        push_hook_group(
            &mut config,
            HookEventName::PreToolUse,
            HookGroupConfig {
                matcher: None,
                hooks: vec![HookCommandConfig {
                    command: "echo hook".into(),
                    timeout_sec: default_hook_timeout_sec(),
                    command_windows: None,
                    status_message: None,
                    async_handler: false,
                    env: Default::default(),
                    env_vars: Vec::new(),
                }],
            },
        );
        let discovery = discover_hooks(
            &config,
            HookSource::managed_launch("test"),
            &HookStateStore::default(),
        );
        let dispatch = HookDispatch {
            results: vec![HookDispatchResult {
                hook: discovery.runnable[0].clone(),
                run: HookRunResult {
                    exit_code: None,
                    timed_out: true,
                    failure: Some(runner::HookRunFailure::Timeout),
                    duration_ms: 1000,
                    stdout: String::new(),
                    stderr: String::new(),
                    effect: HookEffect {
                        decision: Some(HookDecision::Allow),
                        reason: Some("hook said this was safe".into()),
                        warning: Some("hook timed out after 1s".into()),
                        ..HookEffect::default()
                    },
                },
            }],
        };

        let outcome = summarize_hook_dispatch(HookEventName::PreToolUse, &dispatch);

        assert_eq!(
            outcome.blocking_reason.as_deref(),
            Some("hook timed out after 1s")
        );
    }

    #[tokio::test]
    async fn gating_hook_timeout_fails_closed() {
        let mut config = HookConfig::default();
        config.pre_tool_use.push(HookGroupConfig {
            matcher: Some("shell".into()),
            hooks: vec![HookCommandConfig {
                command: "sleep 5".into(),
                timeout_sec: 1,
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });
        let registry = HookRegistry::from_discoveries(vec![discover_hooks(
            &config,
            HookSource::managed_launch("test"),
            &HookStateStore::default(),
        )]);
        let payload = HookPayload::new(HookEventName::PreToolUse, "s1").into_value();

        let outcome = dispatch_hook_payload(
            &registry,
            HookEventName::PreToolUse,
            &payload,
            Some("shell"),
            None,
        )
        .await;

        assert!(outcome
            .blocking_reason
            .as_deref()
            .unwrap()
            .contains("timed out"));
        assert!(!outcome.allowed);
    }

    #[tokio::test]
    async fn gating_hook_shell_infra_failure_fails_closed_but_exit_one_warns() {
        let mut config = HookConfig::default();
        config.permission_request.push(HookGroupConfig {
            matcher: Some("shell".into()),
            hooks: vec![HookCommandConfig {
                command: "albatross-definitely-missing-hook-command".into(),
                timeout_sec: default_hook_timeout_sec(),
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });
        config.permission_request.push(HookGroupConfig {
            matcher: Some("grep".into()),
            hooks: vec![HookCommandConfig {
                command: "exit 1".into(),
                timeout_sec: default_hook_timeout_sec(),
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });
        let registry = HookRegistry::from_discoveries(vec![discover_hooks(
            &config,
            HookSource::managed_launch("test"),
            &HookStateStore::default(),
        )]);
        let payload = HookPayload::new(HookEventName::PermissionRequest, "s1").into_value();

        let missing = dispatch_hook_payload(
            &registry,
            HookEventName::PermissionRequest,
            &payload,
            Some("shell"),
            None,
        )
        .await;
        let exit_one = dispatch_hook_payload(
            &registry,
            HookEventName::PermissionRequest,
            &payload,
            Some("grep"),
            None,
        )
        .await;

        assert!(missing
            .blocking_reason
            .as_deref()
            .unwrap()
            .contains("failed"));
        assert!(exit_one.blocking_reason.is_none());
        assert!(exit_one
            .notices
            .iter()
            .any(|notice| notice.message.contains("status 1")));
    }

    #[test]
    fn only_prompt_hooks_can_rewrite_without_allow() {
        let prompt_dispatch = dispatch_with_effects(
            HookEventName::UserPromptSubmit,
            vec![HookEffect {
                updated_input: Some(json!({"prompt": "rewritten"})),
                ..HookEffect::default()
            }],
        );
        let prompt_outcome =
            summarize_hook_dispatch(HookEventName::UserPromptSubmit, &prompt_dispatch);
        assert_eq!(
            prompt_outcome.updated_input.as_ref().unwrap()["prompt"],
            "rewritten"
        );

        let plan_dispatch = dispatch_with_effects(
            HookEventName::PlanUpdated,
            vec![HookEffect {
                updated_input: Some(json!({"ignored": true})),
                ..HookEffect::default()
            }],
        );
        let plan_outcome = summarize_hook_dispatch(HookEventName::PlanUpdated, &plan_dispatch);
        assert!(plan_outcome.updated_input.is_none());
    }

    #[tokio::test]
    async fn dispatch_hook_payload_writes_trace_without_visible_success() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("s.jsonl");
        let trace = crate::turn_trace::shared_trace(&session, true).unwrap();
        let mut config = HookConfig::default();
        config.session_start.push(HookGroupConfig {
            matcher: None,
            hooks: vec![HookCommandConfig {
                command: "printf '%s' '{\"feedback\":\"started sk-secret123\"}'".into(),
                timeout_sec: default_hook_timeout_sec(),
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });
        let registry = HookRegistry::from_discoveries(vec![discover_hooks(
            &config,
            HookSource::managed_launch("test"),
            &HookStateStore::default(),
        )]);
        let payload = HookPayload::new(HookEventName::SessionStart, "s1").into_value();

        let outcome = dispatch_hook_payload(
            &registry,
            HookEventName::SessionStart,
            &payload,
            None,
            Some(trace.clone()),
        )
        .await;

        assert_eq!(outcome.notices.len(), 1);
        assert_eq!(outcome.notices[0].message, "started (redacted)");
        let text =
            std::fs::read_to_string(crate::turn_trace::events_path_for_session(&session)).unwrap();
        assert!(text.contains("\"kind\":\"hookStart\""));
        assert!(text.contains("\"kind\":\"hookEnd\""));
        assert!(text.contains("\"kind\":\"hookDecision\""));
        assert!(text.contains("(redacted)"));
        assert!(!text.contains("sk-secret123"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dispatch_hook_payload_bounds_trace_decision_text() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("s.jsonl");
        let trace = crate::turn_trace::shared_trace(&session, true).unwrap();
        let mut config = HookConfig::default();
        config.stop.push(HookGroupConfig {
            matcher: None,
            hooks: vec![HookCommandConfig {
                command: r#"printf '{"feedback":"'; printf '%*s' 20000 '' | tr ' ' x; printf '%s' '\n"}'"#.into(),
                timeout_sec: default_hook_timeout_sec(),
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });
        let registry = HookRegistry::from_discoveries(vec![discover_hooks(
            &config,
            HookSource::managed_launch("test"),
            &HookStateStore::default(),
        )]);
        let payload = HookPayload::new(HookEventName::Stop, "s1").into_value();

        dispatch_hook_payload(&registry, HookEventName::Stop, &payload, None, Some(trace)).await;

        let text =
            std::fs::read_to_string(crate::turn_trace::events_path_for_session(&session)).unwrap();
        assert!(text.contains("[truncated]"));
        assert!(text.contains("\\\\n"));
        assert!(text.len() < 8_000);
    }

    #[test]
    fn registry_builder_uses_user_state_and_includes_managed_hooks() {
        let mut project_hooks = HookConfig::default();
        project_hooks.pre_tool_use.push(HookGroupConfig {
            matcher: Some("shell".into()),
            hooks: vec![HookCommandConfig {
                command: "echo project".into(),
                timeout_sec: default_hook_timeout_sec(),
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });
        let first = discover_hooks(
            &project_hooks,
            HookSource::project("/repo/agent.config.json"),
            &HookStateStore::default(),
        );

        let blocked = build_hook_registry(&project_hooks, None, &HookStateFile::default(), "/repo");
        assert!(blocked.runnable.is_empty());

        let mut user_state = HookStateFile::default();
        let still_blocked = build_hook_registry(&project_hooks, None, &user_state, "/repo");
        assert!(still_blocked.runnable.is_empty());
        user_state.hooks.insert(
            first.entries[0].key.clone(),
            HookState {
                enabled: Some(true),
                trusted_hash: Some(first.entries[0].current_hash.clone()),
            },
        );
        let user_trusted = build_hook_registry(&project_hooks, None, &user_state, "/repo");
        assert_eq!(user_trusted.runnable.len(), 1);

        let mut managed_hooks = HookConfig::default();
        managed_hooks.stop.push(HookGroupConfig {
            matcher: None,
            hooks: vec![HookCommandConfig {
                command: "echo managed".into(),
                timeout_sec: default_hook_timeout_sec(),
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });
        let managed = ManagedHookConfig {
            source_label: "terminal-orchestrator".into(),
            hooks: managed_hooks,
        };
        let combined = build_hook_registry(&project_hooks, Some(&managed), &user_state, "/repo");
        assert_eq!(combined.runnable.len(), 2);
    }

    #[test]
    fn project_hook_trust_is_namespaced_by_project_root() {
        let mut project_hooks = HookConfig::default();
        project_hooks.pre_tool_use.push(HookGroupConfig {
            matcher: Some("shell".into()),
            hooks: vec![HookCommandConfig {
                command: "./hooks/check".into(),
                timeout_sec: default_hook_timeout_sec(),
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });

        let repo_a =
            build_hook_registry(&project_hooks, None, &HookStateFile::default(), "/repo-a");
        let repo_a_hook = repo_a.entries[0].clone();
        let mut user_state = HookStateFile::default();
        user_state.hooks.insert(
            repo_a_hook.key.clone(),
            HookState {
                enabled: Some(true),
                trusted_hash: Some(repo_a_hook.current_hash.clone()),
            },
        );

        let trusted_repo_a = build_hook_registry(&project_hooks, None, &user_state, "/repo-a");
        assert_eq!(trusted_repo_a.runnable.len(), 1);

        let repo_b = build_hook_registry(&project_hooks, None, &user_state, "/repo-b");
        assert!(repo_b.runnable.is_empty());
        assert_ne!(repo_a_hook.key, repo_b.entries[0].key);
    }

    #[test]
    fn project_hook_trust_key_uses_lexical_project_root() {
        let mut project_hooks = HookConfig::default();
        project_hooks.pre_tool_use.push(HookGroupConfig {
            matcher: Some("shell".into()),
            hooks: vec![HookCommandConfig {
                command: "./hooks/check".into(),
                timeout_sec: default_hook_timeout_sec(),
                command_windows: None,
                status_message: None,
                async_handler: false,
                env: Default::default(),
                env_vars: Vec::new(),
            }],
        });

        let normalized =
            build_hook_registry(&project_hooks, None, &HookStateFile::default(), "/repo-a");
        let lexical = build_hook_registry(
            &project_hooks,
            None,
            &HookStateFile::default(),
            "/repo-a/../repo-a",
        );

        assert_eq!(normalized.entries[0].key, lexical.entries[0].key);
    }

    fn dispatch_with_effects(event: HookEventName, effects: Vec<HookEffect>) -> HookDispatch {
        let mut config = HookConfig::default();
        push_hook_group(
            &mut config,
            event,
            HookGroupConfig {
                matcher: None,
                hooks: (0..effects.len())
                    .map(|idx| HookCommandConfig {
                        command: format!("echo hook-{idx}"),
                        timeout_sec: default_hook_timeout_sec(),
                        command_windows: None,
                        status_message: None,
                        async_handler: false,
                        env: Default::default(),
                        env_vars: Vec::new(),
                    })
                    .collect(),
            },
        );
        let discovery = discover_hooks(
            &config,
            HookSource::managed_launch("test"),
            &HookStateStore::default(),
        );
        assert_eq!(discovery.runnable.len(), effects.len());
        HookDispatch {
            results: discovery
                .runnable
                .into_iter()
                .zip(effects)
                .map(|(hook, effect)| HookDispatchResult {
                    hook,
                    run: HookRunResult {
                        exit_code: Some(0),
                        timed_out: false,
                        failure: None,
                        duration_ms: 1,
                        stdout: String::new(),
                        stderr: String::new(),
                        effect,
                    },
                })
                .collect(),
        }
    }

    fn push_hook_group(config: &mut HookConfig, event: HookEventName, group: HookGroupConfig) {
        match event {
            HookEventName::SessionStart => config.session_start.push(group),
            HookEventName::UserPromptSubmit => config.user_prompt_submit.push(group),
            HookEventName::PreToolUse => config.pre_tool_use.push(group),
            HookEventName::PermissionRequest => config.permission_request.push(group),
            HookEventName::PostToolUse => config.post_tool_use.push(group),
            HookEventName::PreCompact => config.pre_compact.push(group),
            HookEventName::PostCompact => config.post_compact.push(group),
            HookEventName::PlanUpdated => config.plan_updated.push(group),
            HookEventName::SubagentStart => config.subagent_start.push(group),
            HookEventName::SubagentStop => config.subagent_stop.push(group),
            HookEventName::Stop => config.stop.push(group),
            HookEventName::SessionEnd => config.session_end.push(group),
        }
    }

    fn shell_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(previous) = &self.previous {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[cfg(unix)]
    fn process_exists(pid: i32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}
