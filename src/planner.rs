//! Spec expansion for `/plan`.
//!
//! Takes a one- or two-sentence intent and expands it into an ambitious product
//! spec written to `.albatross/spec.md`. Mirrors the drafting shape in
//! [`crate::handoff`]: a system prompt fixes the section contract, the model
//! drafts, [`ensure_spec_sections`] normalizes the result, and
//! [`render_fallback_spec`] provides a deterministic spec when the model draft
//! is empty or fails.
//!
//! The planner deliberately stays at the level of *what* and *why*, not *how*:
//! the system prompt forbids prescribing files, APIs, or code, because premature
//! technical detail in a spec cascades into downstream implementation errors.

use anyhow::{anyhow, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::model_system::{EffortLevel, ModelRef, ModelSystemConfig, TaskComplexity};

/// Top-level sections every spec must contain, in order.
const SPEC_SECTIONS: &[&str] = &[
    "Goal",
    "User Outcomes",
    "Scope",
    "Out of Scope",
    "Done Criteria",
    "Open Questions",
];

/// Where `/plan` writes by default: `<workspace>/.albatross/spec.md`.
/// Mirrors how the project prompt is rooted in `project_memory::load_project_prompt`.
pub fn default_spec_path(workspace_root: &str) -> PathBuf {
    Path::new(workspace_root).join(".albatross").join("spec.md")
}

/// Where routed plans are stored: `<workspace>/.albatross/plan.json`.
pub fn default_routed_plan_path(workspace_root: &str) -> PathBuf {
    Path::new(workspace_root)
        .join(".albatross")
        .join("plan.json")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanTaskStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

impl PlanTaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

fn default_plan_version() -> u8 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutedPlan {
    #[serde(default = "default_plan_version")]
    pub version: u8,
    pub goal: String,
    pub created_at: String,
    /// Correlates planner and executor model-call receipts in routes.jsonl.
    #[serde(default)]
    pub route_id: Option<String>,
    #[serde(default)]
    pub planner: Option<ModelRef>,
    pub tasks: Vec<RoutedPlanTask>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutedPlanTask {
    pub id: String,
    pub title: String,
    pub prompt: String,
    #[serde(default = "default_task_role")]
    pub role: String,
    pub complexity: TaskComplexity,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub acceptance: Vec<String>,
    #[serde(default)]
    pub assigned_model: Option<ModelRef>,
    #[serde(default)]
    pub effort: Option<EffortLevel>,
    #[serde(default = "default_task_status")]
    pub status: PlanTaskStatus,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
}

fn default_task_role() -> String {
    "code".into()
}

fn default_task_status() -> PlanTaskStatus {
    PlanTaskStatus::Pending
}

pub fn save_routed_plan(path: &Path, plan: &RoutedPlan) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let body = serde_json::to_string_pretty(plan)?;
    fs::write(path, format!("{body}\n"))?;
    Ok(())
}

pub fn load_routed_plan(path: &Path) -> Result<RoutedPlan> {
    let body = fs::read_to_string(path).with_context_path("reading routed plan", path)?;
    serde_json::from_str(&body).with_context_path("parsing routed plan JSON", path)
}

trait PathContext<T> {
    fn with_context_path(self, action: &str, path: &Path) -> Result<T>;
}

impl<T, E> PathContext<T> for std::result::Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn with_context_path(self, action: &str, path: &Path) -> Result<T> {
        self.map_err(|e| anyhow!("{action} {}: {e}", path.display()))
    }
}

/// System prompt: fixes the section contract and forbids premature technical
/// detail (the "cascading errors" warning from the harness-design article).
pub fn planner_system_prompt() -> String {
    [
        "You expand a short product intent into a clear, ambitious specification for a software feature.",
        "Be ambitious about scope and user outcomes — but DO NOT over-specify implementation.",
        "Never prescribe file layouts, module names, function signatures, data structures, or code.",
        "Premature technical detail cascades into downstream errors: describe WHAT and WHY, not HOW.",
        "Return Markdown with exactly these top-level sections, in this order:",
        "## Goal",
        "## User Outcomes",
        "## Scope",
        "## Out of Scope",
        "## Done Criteria",
        "## Open Questions",
        "Goal: one or two sentences capturing the intent.",
        "User Outcomes: concrete, user-facing results, as bullets.",
        "Scope: what this feature includes, as bullets.",
        "Out of Scope: explicit non-goals, as bullets.",
        "Done Criteria: observable, testable conditions that mean the work is complete, as bullets.",
        "Open Questions: unknowns or decisions to confirm before building, as bullets. If none, write `None.`",
    ]
    .join("\n")
}

/// The user message: the intent plus the drafting rules.
pub fn render_planner_prompt(intent: &str) -> String {
    let mut out = String::new();
    out.push_str("Expand the following intent into a product spec.\n\n");
    out.push_str("Rules:\n");
    out.push_str("- Be ambitious about outcomes; keep the spec focused on deliverables, not implementation.\n");
    out.push_str("- Do not invent specific files, APIs, commands, or code.\n");
    out.push_str("- Prefer concrete, observable, testable Done Criteria.\n\n");
    out.push_str("Intent:\n");
    out.push_str(intent.trim());
    out
}

pub fn routed_planner_system_prompt() -> String {
    [
        "You are the planning agent for a complexity-aware Albatross execution layer.",
        "Break one software goal into a small, ordered task graph for coding agents.",
        "Return ONLY JSON, with no markdown, prose, or code fence.",
        "Use this exact top-level shape:",
        "{\"goal\":\"...\",\"tasks\":[{\"id\":\"short-kebab-id\",\"title\":\"...\",\"prompt\":\"...\",\"role\":\"investigate|code|review|verify|docs\",\"complexity\":\"low|medium|high\",\"dependsOn\":[\"id\"],\"acceptance\":[\"observable criterion\"],\"effort\":\"none|minimal|low|medium|high|xhigh|max|null\"}]}",
        "Low complexity means isolated edits, simple docs, straightforward tests, or narrow investigation.",
        "Medium complexity means multi-file implementation, moderate ambiguity, or tests plus code changes.",
        "High complexity means architecture, auth/security, broad refactors, migrations, concurrency, reliability-sensitive work, or tasks where mistakes are expensive.",
        "Make dependencies explicit. Do not create circular dependencies. Keep task ids stable and short.",
        "Each prompt must be self-contained enough for an executor, but should not force exact files or APIs unless the user's request already named them.",
        "Prefer 3-8 tasks. Use high complexity sparingly.",
    ]
    .join("\n")
}

pub fn render_routed_planner_prompt(
    intent: &str,
    stack: &ModelSystemConfig,
    planner: Option<&ModelRef>,
) -> String {
    let mut out = String::new();
    out.push_str("Create a routed execution plan for this goal.\n\nGoal:\n");
    out.push_str(intent.trim());
    out.push_str("\n\nConfigured execution layer:\n");
    append_model_line(&mut out, "planner", planner);
    append_model_line(&mut out, "selector", stack.selector.as_ref());
    append_model_line(
        &mut out,
        "orchestrator.low",
        stack.orchestrators.low.as_ref(),
    );
    append_model_line(
        &mut out,
        "orchestrator.medium",
        stack.orchestrators.medium.as_ref(),
    );
    append_model_line(
        &mut out,
        "orchestrator.high",
        stack.orchestrators.high.as_ref(),
    );
    append_model_line(&mut out, "coder.low", stack.coders.low.as_ref());
    append_model_line(&mut out, "coder.medium", stack.coders.medium.as_ref());
    append_model_line(&mut out, "coder.high", stack.coders.high.as_ref());
    append_model_line(&mut out, "review.play", stack.reviewers.play.as_ref());
    append_model_line(
        &mut out,
        "review.production",
        stack.reviewers.production.as_ref(),
    );
    append_model_line(&mut out, "security", stack.security_reviewer.as_ref());
    out.push_str("\nAssign each task a complexity based on the configured coder tiers. Do not include tasks that cannot be executed by the configured stack unless they are unavoidable.\n");
    out
}

fn append_model_line(out: &mut String, label: &str, model: Option<&ModelRef>) {
    out.push_str("- ");
    out.push_str(label);
    out.push_str(": ");
    match model {
        Some(model) => out.push_str(&model.detail()),
        None => out.push_str("not configured"),
    }
    out.push('\n');
}

pub fn parse_routed_plan(
    text: &str,
    fallback_goal: &str,
    planner: Option<ModelRef>,
    stack: &ModelSystemConfig,
) -> Result<RoutedPlan> {
    let value = if let Ok(value) = serde_json::from_str::<Value>(text.trim()) {
        value
    } else {
        let Some(json) = extract_first_json_object(text) else {
            return Err(anyhow!("planner did not return a JSON routed plan"));
        };
        serde_json::from_str::<Value>(json)
            .map_err(|e| anyhow!("planner returned invalid routed-plan JSON: {e}"))?
    };
    routed_plan_from_value(&value, fallback_goal, planner, stack)
}

fn routed_plan_from_value(
    value: &Value,
    fallback_goal: &str,
    planner: Option<ModelRef>,
    stack: &ModelSystemConfig,
) -> Result<RoutedPlan> {
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow!("routed plan must be a JSON object"))?;
    let goal = obj
        .get("goal")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback_goal.trim())
        .to_string();
    if goal.is_empty() {
        return Err(anyhow!("routed plan goal is empty"));
    }
    let raw_tasks = obj
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("routed plan must include a tasks array"))?;
    if raw_tasks.is_empty() {
        return Err(anyhow!("routed plan must include at least one task"));
    }

    let mut tasks = Vec::new();
    let mut ids = HashSet::new();
    for (index, value) in raw_tasks.iter().enumerate() {
        let task = routed_task_from_value(value, index, stack)?;
        if !ids.insert(task.id.clone()) {
            return Err(anyhow!("duplicate routed plan task id: {}", task.id));
        }
        tasks.push(task);
    }

    let known_ids: HashSet<String> = tasks.iter().map(|task| task.id.clone()).collect();
    for task in &tasks {
        for dep in &task.depends_on {
            if !known_ids.contains(dep) {
                return Err(anyhow!(
                    "task {} depends on unknown task id {}",
                    task.id,
                    dep
                ));
            }
            if dep == &task.id {
                return Err(anyhow!("task {} cannot depend on itself", task.id));
            }
        }
    }
    if tasks.iter().all(|task| !task.depends_on.is_empty()) {
        return Err(anyhow!(
            "routed plan must include at least one task without dependencies"
        ));
    }
    if let Some(id) = first_dependency_cycle(&tasks) {
        return Err(anyhow!(
            "routed plan has a circular dependency involving {id}"
        ));
    }

    Ok(RoutedPlan {
        version: 1,
        goal,
        created_at: Utc::now().to_rfc3339(),
        route_id: None,
        planner,
        tasks,
    })
}

fn first_dependency_cycle(tasks: &[RoutedPlanTask]) -> Option<String> {
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for task in tasks {
        if visit_task_for_cycle(&task.id, tasks, &mut visiting, &mut visited) {
            return Some(task.id.clone());
        }
    }
    None
}

fn visit_task_for_cycle(
    id: &str,
    tasks: &[RoutedPlanTask],
    visiting: &mut HashSet<String>,
    visited: &mut HashSet<String>,
) -> bool {
    if visited.contains(id) {
        return false;
    }
    if !visiting.insert(id.to_string()) {
        return true;
    }
    let has_cycle = tasks
        .iter()
        .find(|task| task.id == id)
        .map(|task| {
            task.depends_on
                .iter()
                .any(|dep| visit_task_for_cycle(dep, tasks, visiting, visited))
        })
        .unwrap_or(false);
    visiting.remove(id);
    visited.insert(id.to_string());
    has_cycle
}

fn routed_task_from_value(
    value: &Value,
    index: usize,
    stack: &ModelSystemConfig,
) -> Result<RoutedPlanTask> {
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow!("task {} must be a JSON object", index + 1))?;
    let fallback_id = format!("task-{}", index + 1);
    let raw_id = string_field(obj, &["id", "key"])
        .or_else(|| string_field(obj, &["title", "name"]).map(|s| sanitize_task_id(&s)))
        .unwrap_or(fallback_id);
    let id = sanitize_task_id(&raw_id);
    if id.is_empty() {
        return Err(anyhow!("task {} has an empty id", index + 1));
    }
    let title = string_field(obj, &["title", "name"])
        .unwrap_or_else(|| id.replace('-', " "))
        .trim()
        .to_string();
    let prompt = string_field(obj, &["prompt", "instructions", "description"])
        .unwrap_or_else(|| title.clone())
        .trim()
        .to_string();
    if prompt.is_empty() {
        return Err(anyhow!("task {id} has an empty prompt"));
    }
    let role = string_field(obj, &["role", "type"])
        .unwrap_or_else(default_task_role)
        .trim()
        .to_ascii_lowercase();
    let complexity = string_field(obj, &["complexity", "difficulty"])
        .and_then(|s| TaskComplexity::parse(&s.trim().to_ascii_lowercase()))
        .unwrap_or(TaskComplexity::Medium);
    let depends_on = array_strings(obj, &["dependsOn", "depends_on", "dependencies"])
        .into_iter()
        .map(|dep| sanitize_task_id(&dep))
        .filter(|dep| !dep.is_empty())
        .collect();
    let acceptance = array_strings(
        obj,
        &[
            "acceptance",
            "acceptanceCriteria",
            "acceptance_criteria",
            "doneCriteria",
            "done_criteria",
        ],
    );
    let effort = string_field(obj, &["effort", "coderEffort", "coder_effort"])
        .and_then(|s| EffortLevel::parse(&s));
    Ok(RoutedPlanTask {
        id,
        title,
        prompt,
        role,
        complexity,
        depends_on,
        acceptance,
        assigned_model: stack.coder(complexity).cloned(),
        effort,
        status: PlanTaskStatus::Pending,
        result: None,
        last_error: None,
    })
}

fn string_field(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| obj.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn array_strings(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Vec<String> {
    let Some(value) = keys.iter().find_map(|key| obj.get(*key)) else {
        return Vec::new();
    };
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        Value::String(text) if !text.trim().is_empty() => vec![text.trim().to_string()],
        _ => Vec::new(),
    }
}

fn sanitize_task_id(value: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in value.trim().chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn extract_first_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = start + offset + ch.len_utf8();
                    return Some(&text[start..end]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Normalize a model draft so every required section is present. Missing
/// sections are appended with a placeholder, mirroring
/// `handoff::ensure_required_sections`.
pub fn ensure_spec_sections(markdown: &str) -> String {
    let mut out = markdown.trim().to_string();
    for section in SPEC_SECTIONS {
        if !has_heading(&out, section) {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            let placeholder = if *section == "Open Questions" {
                "None."
            } else {
                "To be defined."
            };
            out.push_str(&format!("## {section}\n\n{placeholder}"));
        }
    }
    out.push('\n');
    out
}

/// Case-insensitive heading match, ignoring leading `#`s. Local copy of the
/// helper in `handoff.rs` to keep the modules decoupled.
fn has_heading(markdown: &str, section: &str) -> bool {
    markdown.lines().any(|line| {
        let trimmed = line.trim();
        let without_hash = trimmed.trim_start_matches('#').trim();
        !without_hash.is_empty() && without_hash.eq_ignore_ascii_case(section)
    })
}

/// Deterministic spec used when the model draft is empty or the request fails.
/// Always contains every required section so downstream readers (and `/plan
/// show`) get a well-formed file regardless of backend behavior.
pub fn render_fallback_spec(intent: &str, error: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("# Spec\n\n");
    out.push_str(&format!("Generated: {}\n\n", Utc::now().to_rfc3339()));
    if let Some(error) = error {
        out.push_str(&format!("Draft status: model draft failed: {error}\n\n"));
    }
    out.push_str("## Goal\n\n");
    let goal = intent.trim();
    if goal.is_empty() {
        out.push_str("To be defined.\n\n");
    } else {
        out.push_str(goal);
        out.push_str("\n\n");
    }
    out.push_str("## User Outcomes\n\nTo be defined.\n\n");
    out.push_str("## Scope\n\nTo be defined.\n\n");
    out.push_str("## Out of Scope\n\nTo be defined.\n\n");
    out.push_str("## Done Criteria\n\nTo be defined.\n\n");
    out.push_str("## Open Questions\n\nNone.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_spec_path_is_under_albatross() {
        let path = default_spec_path("/tmp/project");
        assert!(path.ends_with(".albatross/spec.md"));
        assert!(path.starts_with("/tmp/project"));
    }

    #[test]
    fn ensure_spec_sections_appends_missing_sections() {
        let normalized = ensure_spec_sections("## Goal\n\nShip a CSV export.");
        for section in SPEC_SECTIONS {
            assert!(
                normalized.contains(&format!("## {section}")),
                "missing section: {section}"
            );
        }
        // The supplied Goal content is preserved.
        assert!(normalized.contains("Ship a CSV export."));
        // Open Questions gets the `None.` placeholder, others `To be defined.`
        assert!(normalized.contains("## Open Questions\n\nNone."));
    }

    #[test]
    fn ensure_spec_sections_is_idempotent_and_case_insensitive() {
        let once = ensure_spec_sections("# goal\n\nx\n\n# OPEN QUESTIONS\n\nNone.");
        // Existing headings (any case / hash count) are not duplicated.
        assert_eq!(
            once.matches("Goal").count() + once.matches("goal").count(),
            1
        );
        assert_eq!(once.to_lowercase().matches("open questions").count(), 1);
    }

    #[test]
    fn fallback_spec_contains_all_sections_and_intent() {
        let spec = render_fallback_spec("add a CSV export command", Some("boom"));
        for section in SPEC_SECTIONS {
            assert!(
                spec.contains(&format!("## {section}")),
                "missing: {section}"
            );
        }
        assert!(spec.contains("add a CSV export command"));
        assert!(spec.contains("model draft failed: boom"));
    }

    #[test]
    fn fallback_spec_handles_empty_intent() {
        let spec = render_fallback_spec("   ", None);
        assert!(spec.contains("## Goal\n\nTo be defined."));
        assert!(!spec.contains("Draft status"));
    }

    #[test]
    fn planner_prompt_includes_intent_and_rules() {
        let prompt = render_planner_prompt("  build a dashboard  ");
        assert!(prompt.contains("build a dashboard"));
        assert!(prompt.contains("not implementation"));
    }

    #[test]
    fn default_routed_plan_path_is_under_albatross() {
        let path = default_routed_plan_path("/tmp/project");
        assert!(path.ends_with(".albatross/plan.json"));
        assert!(path.starts_with("/tmp/project"));
    }

    #[test]
    fn parses_wrapped_routed_plan_and_assigns_coder_tiers() {
        let stack = ModelSystemConfig {
            coders: crate::model_system::ModelTierSet {
                low: ModelRef::parse_spec("ollama:qwen2.5:7b"),
                high: ModelRef::parse_spec("openrouter:anthropic/claude-sonnet-4.5"),
                ..Default::default()
            },
            ..Default::default()
        };
        let plan = parse_routed_plan(
            "```json\n{\"goal\":\"ship auth\",\"tasks\":[{\"id\":\"audit\",\"title\":\"Audit auth\",\"prompt\":\"Inspect auth flow\",\"complexity\":\"low\"},{\"id\":\"implement-refresh\",\"title\":\"Implement refresh\",\"prompt\":\"Add refresh handling\",\"complexity\":\"High\",\"dependsOn\":[\"audit\"],\"acceptance\":[\"expired tokens refresh\"],\"effort\":\"high\"}]}\n```",
            "fallback",
            ModelRef::parse_spec("openrouter:openrouter/fusion"),
            &stack,
        )
        .unwrap();
        assert_eq!(plan.goal, "ship auth");
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(
            plan.tasks[0].assigned_model.as_ref().unwrap().backend,
            crate::backends::BackendName::Ollama
        );
        assert_eq!(
            plan.tasks[1].assigned_model.as_ref().unwrap().model,
            "anthropic/claude-sonnet-4.5"
        );
        assert_eq!(plan.tasks[1].effort, Some(EffortLevel::High));
        assert_eq!(plan.tasks[1].depends_on, vec!["audit"]);
    }

    #[test]
    fn routed_plan_rejects_unknown_dependencies() {
        let err = parse_routed_plan(
            r#"{"goal":"x","tasks":[{"id":"b","prompt":"do b","dependsOn":["missing"]}]}"#,
            "fallback",
            None,
            &ModelSystemConfig::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown task id"));
    }

    #[test]
    fn routed_plan_rejects_cycles_even_with_a_root_task() {
        let err = parse_routed_plan(
            r#"{"goal":"x","tasks":[{"id":"a","prompt":"do a"},{"id":"b","prompt":"do b","dependsOn":["c"]},{"id":"c","prompt":"do c","dependsOn":["b"]}]}"#,
            "fallback",
            None,
            &ModelSystemConfig::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("circular dependency"));
    }
}
