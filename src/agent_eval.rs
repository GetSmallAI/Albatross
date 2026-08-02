use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::agent::{run_agent, AgentEvent, ApprovalProvider, RunResult};
use crate::backends::{validate, BackendDescriptor};
use crate::config::AgentConfig;
use crate::openai::{build_http_client, ChatMessage};
use crate::project_memory::render_system_prompt_with_memory;
use crate::session::{init_session_dir, new_session_path, save_message};
use crate::test_integration::run_tests;
use crate::tools::{build_tools_for_names, select_tool_names, ToolPreview};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEvalFixture {
    pub id: String,
    pub prompt: String,
    pub workspace: Option<String>,
    pub checks: Vec<AgentEvalCheck>,
    #[serde(skip)]
    pub fixture_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AgentEvalCheck {
    TestsPass,
    FileContains { path: String, needle: String },
    GitClean,
    ToolUsed { name: String },
    AssistantMentions { needle: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEvalCheckResult {
    pub check: AgentEvalCheck,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEvalRunResult {
    pub fixture_id: String,
    pub model: String,
    pub backend: String,
    pub passed: bool,
    pub checks: Vec<AgentEvalCheckResult>,
    pub elapsed_ms: u128,
    pub steps: usize,
    #[serde(default)]
    pub hit_step_limit: bool,
    pub tool_calls: Vec<String>,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub transcript_path: String,
    pub error: Option<String>,
}

struct EvalApproval;

#[async_trait]
impl ApprovalProvider for EvalApproval {
    async fn approve(
        &mut self,
        _name: &str,
        _args: &Value,
        _preview: Option<&ToolPreview>,
    ) -> bool {
        true
    }
}

pub fn builtin_fixtures() -> Vec<AgentEvalFixture> {
    vec![
        AgentEvalFixture {
            id: "read-and-explain".into(),
            prompt: "Read src/lib.rs and briefly explain what the `add` function does.".into(),
            workspace: Some("fix-failing-test".into()),
            checks: vec![
                AgentEvalCheck::ToolUsed {
                    name: "file_read".into(),
                },
                AgentEvalCheck::AssistantMentions {
                    needle: "add".into(),
                },
            ],
            fixture_root: None,
        },
        AgentEvalFixture {
            id: "fix-failing-test".into(),
            prompt: "The tests are failing. Fix the bug so `cargo test` passes.".into(),
            workspace: Some("fix-failing-test".into()),
            checks: vec![AgentEvalCheck::TestsPass],
            fixture_root: None,
        },
        AgentEvalFixture {
            id: "small-refactor".into(),
            prompt:
                "Rename the function `greet_user` to `welcome_user` across all files in this crate."
                    .into(),
            workspace: Some("small-refactor".into()),
            checks: vec![
                AgentEvalCheck::FileContains {
                    path: "src/main.rs".into(),
                    needle: "welcome_user".into(),
                },
                AgentEvalCheck::TestsPass,
            ],
            fixture_root: None,
        },
        AgentEvalFixture {
            id: "add-feature".into(),
            prompt: "Add a `mul` function and a passing test for it.".into(),
            workspace: Some("add-feature".into()),
            checks: vec![
                AgentEvalCheck::FileContains {
                    path: "src/lib.rs".into(),
                    needle: "fn mul".into(),
                },
                AgentEvalCheck::TestsPass,
            ],
            fixture_root: None,
        },
    ]
}

pub fn fixture_by_id(id: &str) -> Option<AgentEvalFixture> {
    builtin_fixtures()
        .into_iter()
        .find(|fixture| fixture.id == id)
}

pub fn fixture_by_spec(spec: &str) -> Result<AgentEvalFixture> {
    if let Some(fixture) = fixture_by_id(spec) {
        return Ok(fixture);
    }
    let path = Path::new(spec);
    if path.components().count() == 1 && path.extension().is_none() {
        anyhow::bail!("unknown agent eval fixture: {spec}");
    }
    load_external_fixture(path)
}

fn safe_relative_path(value: &str, label: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        anyhow::bail!("{label} must be relative: {value}");
    }
    for component in path.components() {
        use std::path::Component;
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("{label} escapes the fixture root: {value}");
            }
        }
    }
    Ok(path)
}

fn validate_external_refs(fixture: &AgentEvalFixture) -> Result<()> {
    if let Some(workspace) = &fixture.workspace {
        safe_relative_path(workspace, "fixture workspace")?;
    }
    for check in &fixture.checks {
        if let AgentEvalCheck::FileContains { path, .. } = check {
            safe_relative_path(path, "fileContains path")?;
        }
    }
    Ok(())
}

pub fn load_external_fixture(path: &Path) -> Result<AgentEvalFixture> {
    if !path.exists() {
        anyhow::bail!("external fixture not found: {}", path.display());
    }
    let path = path
        .canonicalize()
        .with_context(|| format!("canonicalize external fixture: {}", path.display()))?;
    if !path.is_file() {
        anyhow::bail!("external fixture is not a file: {}", path.display());
    }
    let root = path
        .parent()
        .ok_or_else(|| anyhow!("external fixture has no parent: {}", path.display()))?
        .to_path_buf();
    let mut fixture: AgentEvalFixture = serde_json::from_str(
        &fs::read_to_string(&path)
            .with_context(|| format!("read external fixture: {}", path.display()))?,
    )
    .with_context(|| format!("parse external fixture JSON: {}", path.display()))?;
    validate_external_refs(&fixture)?;
    if let Some(workspace) = &fixture.workspace {
        let rel = safe_relative_path(workspace, "fixture workspace")?;
        let src = root.join(rel);
        if !src.exists() {
            anyhow::bail!("fixture workspace not found: {}", src.display());
        }
        let src = src
            .canonicalize()
            .with_context(|| format!("canonicalize fixture workspace: {}", src.display()))?;
        if !src.starts_with(&root) {
            anyhow::bail!(
                "fixture workspace escapes the fixture root: {}",
                src.display()
            );
        }
    }
    fixture.fixture_root = Some(root);
    Ok(fixture)
}

pub fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("evals/fixtures")
}

fn fixture_root_for(fixture: &AgentEvalFixture) -> PathBuf {
    fixture.fixture_root.clone().unwrap_or_else(fixtures_root)
}

fn fixture_workspace_src(fixture: &AgentEvalFixture) -> Result<Option<PathBuf>> {
    let Some(rel) = &fixture.workspace else {
        return Ok(None);
    };
    let root = fixture_root_for(fixture);
    let rel = safe_relative_path(rel, "fixture workspace")?;
    let src = root.join(rel);
    if !src.exists() {
        anyhow::bail!("fixture workspace not found: {}", src.display());
    }
    Ok(Some(src))
}

pub fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if file_type.is_symlink() {
            anyhow::bail!(
                "fixture workspace contains unsupported symlink: {}",
                entry.path().display()
            );
        } else if file_type.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), target)?;
        } else {
            anyhow::bail!(
                "fixture workspace contains unsupported entry type: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

pub fn prepare_fixture_workspace(
    fixture: &AgentEvalFixture,
) -> Result<(tempfile::TempDir, PathBuf)> {
    let temp = tempfile::tempdir()?;
    let workspace = temp.path().to_path_buf();
    if let Some(src) = fixture_workspace_src(fixture)? {
        copy_dir_all(&src, &workspace)?;
    }
    Ok((temp, workspace))
}

pub fn prepare_playground_workspace(
    session_dir: &str,
    fixture_id: &str,
    fixture: &AgentEvalFixture,
) -> Result<PathBuf> {
    let stamp = Utc::now().format("%Y-%m-%dT%H-%M-%S-%3fZ");
    let dest = Path::new(session_dir)
        .join("play")
        .join(format!("{fixture_id}-{stamp}"));
    fs::create_dir_all(&dest)?;
    if let Some(src) = fixture_workspace_src(fixture)? {
        copy_dir_all(&src, &dest)?;
    }
    Ok(dest)
}

pub fn init_git_if_needed(workspace: &Path) -> Result<()> {
    if workspace.join(".git").exists() {
        return Ok(());
    }
    let status = std::process::Command::new("git")
        .args(["init"])
        .current_dir(workspace)
        .status()?;
    if !status.success() {
        anyhow::bail!("git init failed");
    }
    let _ = std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(workspace)
        .status();
    let _ = std::process::Command::new("git")
        .args(["commit", "-m", "fixture baseline", "--allow-empty"])
        .current_dir(workspace)
        .status();
    Ok(())
}

pub fn count_assistant_steps(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .filter(|m| matches!(m, ChatMessage::Assistant { .. }))
        .count()
}

pub fn evaluate_checks(
    workspace: &Path,
    checks: &[AgentEvalCheck],
    run: &RunResult,
    tool_calls: &[String],
) -> Vec<AgentEvalCheckResult> {
    checks
        .iter()
        .map(|check| {
            let (passed, detail) = match check {
                AgentEvalCheck::TestsPass => match run_tests(workspace.to_str().unwrap(), None) {
                    Ok(result) => (
                        result.failed == 0 && result.exit_code == 0,
                        format!(
                            "total={} passed={} failed={} exit={}",
                            result.total, result.passed, result.failed, result.exit_code
                        ),
                    ),
                    Err(e) => (false, e.to_string()),
                },
                AgentEvalCheck::FileContains { path, needle } => {
                    let full = workspace.join(path);
                    match fs::read_to_string(&full) {
                        Ok(content) => (
                            content.contains(needle),
                            format!("{} contains '{needle}'", full.display()),
                        ),
                        Err(e) => (false, e.to_string()),
                    }
                }
                AgentEvalCheck::GitClean => match std::process::Command::new("git")
                    .args(["status", "--porcelain"])
                    .current_dir(workspace)
                    .output()
                {
                    Ok(out) if out.status.success() => {
                        let clean = out.stdout.is_empty();
                        (
                            clean,
                            if clean {
                                "working tree clean".into()
                            } else {
                                format!(
                                    "dirty files: {}",
                                    String::from_utf8_lossy(&out.stdout).trim()
                                )
                            },
                        )
                    }
                    Ok(out) => (false, String::from_utf8_lossy(&out.stderr).into()),
                    Err(e) => (false, e.to_string()),
                },
                AgentEvalCheck::ToolUsed { name } => {
                    let used = tool_calls.iter().any(|tool| tool == name);
                    (
                        used,
                        if used {
                            format!("tool {name} was called")
                        } else {
                            format!("tool {name} was not called")
                        },
                    )
                }
                AgentEvalCheck::AssistantMentions { needle } => {
                    let text: String = run
                        .messages
                        .iter()
                        .filter_map(|m| match m {
                            ChatMessage::Assistant { content, .. } => content.as_deref(),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let found = text.to_lowercase().contains(&needle.to_lowercase());
                    (
                        found,
                        if found {
                            format!("assistant mentioned '{needle}'")
                        } else {
                            "assistant did not mention needle".into()
                        },
                    )
                }
            };
            AgentEvalCheckResult {
                check: check.clone(),
                passed,
                detail,
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub async fn run_agent_eval(
    config: &AgentConfig,
    backend_desc: &BackendDescriptor,
    model: &str,
    fixture: &AgentEvalFixture,
) -> Result<AgentEvalRunResult> {
    validate(backend_desc)?;
    let (_temp, workspace) = prepare_fixture_workspace(fixture)?;
    init_git_if_needed(&workspace)?;
    let workspace_root = workspace.to_str().unwrap().to_string();

    let mut eval_config = config.clone();
    eval_config.workspace_root = workspace_root.clone();
    eval_config.approval_policy = crate::config::ApprovalPolicy::Never;
    eval_config.tool_selection = crate::config::ToolSelection::Fixed;
    eval_config.apply_operator_mode(crate::config::OperatorMode::Ship);

    let active_tool_names = select_tool_names(&eval_config, &fixture.prompt);
    let system_prompt = render_system_prompt_with_memory(
        &eval_config,
        backend_desc,
        &active_tool_names,
        &fixture.prompt,
    );
    let messages = vec![
        ChatMessage::System {
            content: system_prompt,
        },
        ChatMessage::User {
            content: fixture.prompt.clone().into(),
        },
    ];
    let tools = build_tools_for_names(&eval_config, &active_tool_names, None);
    let http = build_http_client();
    let mut tool_calls = Vec::new();
    let start = Instant::now();
    let mut approval = EvalApproval;
    let run = run_agent(
        &http,
        backend_desc,
        model,
        None,
        messages,
        tools,
        eval_config.max_steps,
        |event| {
            if let AgentEvent::ToolCall { name, .. } = event {
                tool_calls.push(name);
            }
        },
        Some(&mut approval as &mut dyn ApprovalProvider),
        None,
        None,
        None,
        None,
        0,
        None,
    )
    .await;
    let elapsed_ms = start.elapsed().as_millis();

    init_session_dir(&eval_config.session_dir)?;
    let transcript_path = new_session_path(&eval_config.session_dir);
    if let Ok(result) = &run {
        for message in &result.messages {
            let _ = save_message(&transcript_path, message);
        }
    }

    match run {
        Ok(result) => {
            let checks = evaluate_checks(&workspace, &fixture.checks, &result, &tool_calls);
            let passed = checks.iter().all(|c| c.passed);
            Ok(AgentEvalRunResult {
                fixture_id: fixture.id.clone(),
                model: model.to_string(),
                backend: backend_desc.name.as_str().to_string(),
                passed,
                checks,
                elapsed_ms,
                steps: count_assistant_steps(&result.messages),
                hit_step_limit: result.hit_step_limit,
                tool_calls,
                input_tokens: result.input_tokens,
                output_tokens: result.output_tokens,
                transcript_path: transcript_path.display().to_string(),
                error: None,
            })
        }
        Err(e) => Ok(AgentEvalRunResult {
            fixture_id: fixture.id.clone(),
            model: model.to_string(),
            backend: backend_desc.name.as_str().to_string(),
            passed: false,
            checks: Vec::new(),
            elapsed_ms,
            steps: 0,
            hit_step_limit: false,
            tool_calls,
            input_tokens: 0,
            output_tokens: 0,
            transcript_path: transcript_path.display().to_string(),
            error: Some(e.to_string()),
        }),
    }
}

pub fn render_agent_eval_markdown(results: &[AgentEvalRunResult]) -> String {
    let mut md = String::from("# Albatross Agent Eval\n\n");
    md.push_str("| model | fixture | passed | steps | latency ms | tools_used |\n");
    md.push_str("| --- | --- | --- | --- | --- | --- |\n");
    for result in results {
        md.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            result.model,
            result.fixture_id,
            if result.passed { "yes" } else { "no" },
            result.steps,
            result.elapsed_ms,
            result.tool_calls.join(", ")
        ));
    }
    md.push('\n');
    for result in results {
        md.push_str(&format!("## {} · {}\n\n", result.model, result.fixture_id));
        if let Some(error) = &result.error {
            md.push_str(&format!("**Error:** {error}\n\n"));
        }
        for check in &result.checks {
            md.push_str(&format!(
                "- [{}] {:?}: {}\n",
                if check.passed { "x" } else { " " },
                check.check,
                check.detail
            ));
        }
        md.push('\n');
    }
    md
}

#[allow(dead_code)]
pub async fn run_agent_eval_suite(
    config: &AgentConfig,
    backend_desc: &BackendDescriptor,
    model: &str,
    fixture_ids: &[String],
) -> Result<Vec<AgentEvalRunResult>> {
    let mut out = Vec::new();
    for id in fixture_ids {
        let fixture = fixture_by_spec(id)?;
        out.push(run_agent_eval(config, backend_desc, model, &fixture).await?);
    }
    Ok(out)
}

/// Run a single fixture from the CLI (`--eval <id>`). Returns exit code 0 on pass.
pub async fn run_eval_cli(
    config: &AgentConfig,
    fixture_id: &str,
    model_override: Option<&str>,
    json_output: bool,
) -> anyhow::Result<i32> {
    let fixture = fixture_by_spec(fixture_id)?;
    let backend_desc = config.backend_descriptor();
    crate::backends::validate(&backend_desc)?;
    let model = model_override.map(str::to_string).unwrap_or_else(|| {
        crate::backends::default_model(&backend_desc, config.model_override.as_deref())
    });
    let result = run_agent_eval(config, &backend_desc, &model, &fixture).await?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "{} {} · {} · {}ms · {} steps",
            if result.passed { "PASS" } else { "FAIL" },
            result.fixture_id,
            result.model,
            result.elapsed_ms,
            result.steps
        );
        for check in &result.checks {
            println!(
                "  [{}] {:?} — {}",
                if check.passed { "x" } else { " " },
                check.check,
                check.detail
            );
        }
    }
    Ok(if result.passed { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_fixtures_include_required_ids() {
        let ids: Vec<_> = builtin_fixtures().into_iter().map(|f| f.id).collect();
        assert!(ids.contains(&"read-and-explain".to_string()));
        assert!(ids.contains(&"fix-failing-test".to_string()));
        assert!(ids.contains(&"small-refactor".to_string()));
        assert!(ids.contains(&"add-feature".to_string()));
    }

    #[test]
    fn prepare_playground_workspace_copies_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = fixture_by_id("fix-failing-test").unwrap();
        let dest = prepare_playground_workspace(
            dir.path().to_str().unwrap(),
            "fix-failing-test",
            &fixture,
        )
        .unwrap();
        assert!(dest.join("Cargo.toml").exists());
        assert!(dest.join("src/lib.rs").exists());
    }

    #[test]
    fn file_contains_check_detects_needle() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("note.txt"), "hello world").unwrap();
        let checks = evaluate_checks(
            dir.path(),
            &[AgentEvalCheck::FileContains {
                path: "note.txt".into(),
                needle: "world".into(),
            }],
            &RunResult {
                messages: vec![],
                input_tokens: 0,
                output_tokens: 0,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                reported_cost_usd: None,
                actual_model: None,
                provider: None,
                transcript_rewritten: false,
                conversation_summary: None,
                hit_step_limit: false,
                metrics: crate::turn_trace::TurnMetrics::default(),
            },
            &[],
        );
        assert!(checks[0].passed);
    }

    #[test]
    fn external_fixture_loads_from_json_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("workspace/basic/src")).unwrap();
        fs::write(
            dir.path().join("workspace/basic/Cargo.toml"),
            "[package]\nname=\"x\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("workspace/basic/src/lib.rs"),
            "pub fn add() {}\n",
        )
        .unwrap();
        let fixture_path = dir.path().join("basic.json");
        fs::write(
            &fixture_path,
            r#"{
              "id": "external-basic",
              "prompt": "Read the library.",
              "workspace": "workspace/basic",
              "checks": [
                { "type": "fileContains", "path": "src/lib.rs", "needle": "add" }
              ]
            }"#,
        )
        .unwrap();

        let fixture = fixture_by_spec(fixture_path.to_str().unwrap()).unwrap();

        assert_eq!(fixture.id, "external-basic");
        assert_eq!(
            fixture.fixture_root.as_deref(),
            Some(
                fixture_path
                    .parent()
                    .unwrap()
                    .canonicalize()
                    .unwrap()
                    .as_path()
            )
        );
        let (_temp, workspace) = prepare_fixture_workspace(&fixture).unwrap();
        assert!(workspace.join("src/lib.rs").exists());
    }

    #[cfg(unix)]
    #[test]
    fn fixture_workspace_copy_rejects_symlinked_children() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("workspace");
        let dst = dir.path().join("copied");
        let outside = dir.path().join("outside-secret.txt");
        fs::create_dir_all(&src).unwrap();
        fs::write(&outside, "outside bytes").unwrap();
        symlink(&outside, src.join("leak.txt")).unwrap();

        let err = copy_dir_all(&src, &dst).unwrap_err().to_string();

        assert!(err.contains("unsupported symlink"));
        assert!(!dst.join("leak.txt").exists());
    }

    #[test]
    fn built_in_fixture_lookup_still_works() {
        let fixture = fixture_by_spec("fix-failing-test").unwrap();
        assert_eq!(fixture.id, "fix-failing-test");
        assert!(fixture.fixture_root.is_none());
    }

    #[test]
    fn external_fixture_rejects_workspace_escape() {
        let dir = tempfile::tempdir().unwrap();
        let fixture_path = dir.path().join("escape.json");
        fs::write(
            &fixture_path,
            r#"{
              "id": "escape",
              "prompt": "No escape.",
              "workspace": "../outside",
              "checks": []
            }"#,
        )
        .unwrap();

        let err = load_external_fixture(&fixture_path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("escapes the fixture root"));
    }

    #[test]
    fn external_fixture_rejects_file_contains_escape() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("workspace/basic")).unwrap();
        let fixture_path = dir.path().join("escape-check.json");
        fs::write(
            &fixture_path,
            r#"{
              "id": "escape-check",
              "prompt": "No escape.",
              "workspace": "workspace/basic",
              "checks": [
                { "type": "fileContains", "path": "../secret.txt", "needle": "x" }
              ]
            }"#,
        )
        .unwrap();

        let err = load_external_fixture(&fixture_path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("fileContains path escapes"));
    }

    #[test]
    fn external_fixture_rejects_missing_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let fixture_path = dir.path().join("missing-workspace.json");
        fs::write(
            &fixture_path,
            r#"{
              "id": "missing-workspace",
              "prompt": "Workspace is missing.",
              "workspace": "workspace/missing",
              "checks": []
            }"#,
        )
        .unwrap();

        let err = load_external_fixture(&fixture_path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("fixture workspace not found"));
    }

    #[test]
    fn external_fixture_rejects_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let fixture_path = dir.path().join("bad.json");
        fs::write(&fixture_path, "{not json").unwrap();

        let err = load_external_fixture(&fixture_path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("parse external fixture JSON"));
    }
}
