use std::path::Path;
use std::process::Command;

use crate::theme::{MUTED, PAD, RESET, TEXT};

struct SessionHeaderInfo<'a> {
    project: &'a str,
    branch: Option<&'a str>,
    dirty: bool,
    backend: &'a str,
    model: &'a str,
    mode: &'a str,
    approval: &'a str,
}

fn truncate_to_width(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut truncated = value
        .chars()
        .take(max.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

/// Lay metadata out as compact `label: value` pairs, starting a new row when
/// another pair would overflow. Session context remains recognizably distinct
/// from transcript roles such as `user` and `response`.
fn metadata_rows(fields: &[(&str, String)], width: usize) -> Vec<String> {
    let mut rows: Vec<Vec<(String, String)>> = Vec::new();
    let mut row_width = 0;

    for (label, value) in fields {
        let mut value = value.clone();
        let label_width = label.chars().count() + 2; // `: `
        let available = width.saturating_sub(label_width).max(1);
        value = truncate_to_width(&value, available);
        let field_width = label_width + value.chars().count();
        let separator_width = usize::from(row_width > 0) * 3; // ` · `

        if row_width > 0 && row_width + separator_width + field_width > width {
            rows.push(Vec::new());
            row_width = 0;
        }

        if rows.is_empty() {
            rows.push(Vec::new());
        }
        rows.last_mut()
            .expect("metadata row was initialized")
            .push(((*label).to_string(), value));
        row_width += usize::from(row_width > 0) * 3 + field_width;
    }

    rows.into_iter()
        .map(|row| {
            let fields = row
                .into_iter()
                .map(|(label, value)| format!("{MUTED}{label}:{RESET} {TEXT}{value}{RESET}"))
                .collect::<Vec<_>>()
                .join(&format!("{MUTED} · {RESET}"));
            format!("{PAD}{fields}")
        })
        .collect()
}

fn render_session_header(info: SessionHeaderInfo<'_>, width: usize) -> String {
    let approval = match info.approval {
        "dangerous-only" => "ask for risky actions",
        "always" => "ask before actions",
        "never" => "no approval prompts",
        other => other,
    };
    let content_width = width.clamp(20, 400).saturating_sub(2);
    let inner_width = content_width.saturating_sub(PAD.chars().count());
    let workspace = vec![
        ("session", "albatross".to_string()),
        ("project", info.project.to_string()),
        ("branch", info.branch.unwrap_or("detached").to_string()),
        (
            "status",
            if info.dirty { "modified" } else { "clean" }.to_string(),
        ),
    ];
    let runtime = vec![
        ("backend", info.backend.to_string()),
        ("model", info.model.to_string()),
        ("mode", info.mode.to_string()),
        ("approval", approval.to_string()),
    ];
    [
        metadata_rows(&workspace, inner_width),
        metadata_rows(&runtime, inner_width),
    ]
    .concat()
    .join("\n")
}

fn render_compact_session_context(info: SessionHeaderInfo<'_>) -> String {
    let approval = match info.approval {
        "dangerous-only" => "ask",
        "always" => "ask all",
        "never" => "no prompts",
        other => other,
    };
    let mut workspace = info.branch.unwrap_or(info.project).to_string();
    if info.dirty {
        workspace.push('*');
    }
    format!("{} · {} · {approval} · {workspace}", info.model, info.mode)
}

fn workspace_context(workspace_root: &str) -> (String, Option<String>, bool) {
    let workspace = Path::new(workspace_root);
    let display_workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let project = display_workspace
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(workspace_root)
        .to_string();
    let Ok(output) = Command::new("git")
        .args(["-C", workspace_root, "status", "--porcelain=v2", "--branch"])
        .output()
    else {
        return (project, None, false);
    };
    if !output.status.success() {
        return (project, None, false);
    }

    let status = String::from_utf8_lossy(&output.stdout);
    let branch = status.lines().find_map(|line| {
        line.strip_prefix("# branch.head ")
            .map(|name| name.to_string())
    });
    let dirty = status
        .lines()
        .any(|line| !line.is_empty() && !line.starts_with('#'));
    (project, branch, dirty)
}

pub fn render_session_header_for(
    config: &crate::config::AgentConfig,
    model: &str,
    width: usize,
) -> String {
    let (project, branch, dirty) = workspace_context(&config.workspace_root);
    let mut rendered = render_session_header(
        SessionHeaderInfo {
            project: &project,
            branch: branch.as_deref(),
            dirty,
            backend: config.backend.as_str(),
            model,
            mode: config.mode.as_str(),
            approval: config.approval_policy.as_str(),
        },
        width,
    );
    rendered.push('\n');
    rendered
}

pub fn compact_session_context_for(config: &crate::config::AgentConfig, model: &str) -> String {
    let (project, branch, dirty) = workspace_context(&config.workspace_root);
    render_compact_session_context(SessionHeaderInfo {
        project: &project,
        branch: branch.as_deref(),
        dirty,
        backend: config.backend.as_str(),
        model,
        mode: config.mode.as_str(),
        approval: config.approval_policy.as_str(),
    })
}

pub fn print_session_header(config: &crate::config::AgentConfig, model: &str) {
    println!(
        "{}",
        render_session_header_for(config, model, crate::theme::cols())
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn plain(value: &str) -> String {
        let mut output = String::new();
        let mut in_escape = false;
        for ch in value.chars() {
            if in_escape {
                in_escape = ch != 'm';
            } else if ch == '\x1b' {
                in_escape = true;
            } else {
                output.push(ch);
            }
        }
        output
    }

    #[test]
    fn session_header_shows_the_live_workspace_context() {
        let rendered = plain(&render_session_header(
            SessionHeaderInfo {
                project: "Albatross",
                branch: Some("main"),
                dirty: true,
                backend: "grok",
                model: "grok-build-0.1",
                mode: "edit",
                approval: "dangerous-only",
            },
            120,
        ));
        let lines = rendered.lines().collect::<Vec<_>>();

        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            "  session: albatross · project: Albatross · branch: main · status: modified"
        );
        assert_eq!(
            lines[1],
            "  backend: grok · model: grok-build-0.1 · mode: edit · approval: ask for risky actions"
        );
        assert!(!rendered.contains('─'));
    }

    #[test]
    fn session_header_reflows_on_narrow_terminals() {
        let rendered = plain(&render_session_header(
            SessionHeaderInfo {
                project: "Albatross",
                branch: Some("feature/polished-ui"),
                dirty: false,
                backend: "openai-codex",
                model: "gpt-5.2-codex",
                mode: "review",
                approval: "dangerous-only",
            },
            40,
        ));
        let lines = rendered.lines().collect::<Vec<_>>();

        assert!(lines.len() >= 4, "metadata should reflow: {rendered:?}");
        for field in [
            "session: albatross",
            "project: Albatross",
            "branch: feature/polished-ui",
            "status: clean",
            "backend: openai-codex",
            "model: gpt-5.2-codex",
            "mode: review",
            "approval: ask for risky actions",
        ] {
            assert!(rendered.contains(field), "missing {field:?}: {rendered:?}");
        }
        assert!(!rendered.contains('─'));
        assert!(lines.iter().all(|line| line.chars().count() <= 40));
    }

    #[test]
    fn compact_session_context_keeps_runtime_safety_and_branch_on_one_line() {
        let rendered = render_compact_session_context(SessionHeaderInfo {
            project: "Albatross",
            branch: Some("main"),
            dirty: true,
            backend: "grok",
            model: "grok-4.5",
            mode: "edit",
            approval: "dangerous-only",
        });

        assert_eq!(rendered, "grok-4.5 · edit · ask · main*");
    }

    #[test]
    fn session_header_derives_the_project_and_git_state() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("Albatross");
        std::fs::create_dir(&workspace).unwrap();
        let output = Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&workspace)
            .output()
            .unwrap();
        assert!(output.status.success());
        std::fs::write(workspace.join("new.rs"), "fn main() {}\n").unwrap();

        let mut config = crate::config::AgentConfig::default();
        config.workspace_root = workspace.display().to_string();
        config.backend = crate::backends::BackendName::Grok;
        config.mode = crate::config::OperatorMode::Edit;
        config.approval_policy = crate::config::ApprovalPolicy::DangerousOnly;

        let rendered = plain(&render_session_header_for(&config, "grok-build-0.1", 120));
        let lines = rendered.lines().collect::<Vec<_>>();
        assert_eq!(
            lines[0],
            "  session: albatross · project: Albatross · branch: main · status: modified"
        );
        assert_eq!(
            lines[1],
            "  backend: grok · model: grok-build-0.1 · mode: edit · approval: ask for risky actions"
        );
        assert!(
            rendered.ends_with('\n'),
            "the session header should leave a breathing row before the first prompt"
        );
    }

    #[test]
    fn workspace_context_resolves_a_relative_root_to_its_project_name() {
        let expected = std::env::current_dir()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let (project, _, _) = workspace_context(".");

        assert_eq!(project, expected);
        assert_ne!(project, ".");
    }
}
