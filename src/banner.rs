use std::path::Path;
use std::process::Command;

use crate::theme::{ACCENT, BOLD, MUTED, PAD, RESET, TEXT};

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

fn panel_row(content: &str, inner_width: usize) -> String {
    let content = truncate_to_width(content, inner_width);
    format!("{PAD}{MUTED}│{RESET} {TEXT}{content:<inner_width$}{RESET} {MUTED}│{RESET}")
}

fn render_session_header(info: SessionHeaderInfo<'_>, width: usize) -> String {
    let approval = match info.approval {
        "dangerous-only" => "ask for risky actions",
        "always" => "ask before actions",
        "never" => "no approval prompts",
        other => other,
    };
    let panel_width = width.clamp(32, 78);
    let inner_width = panel_width.saturating_sub(PAD.len() + 6);
    let title = "Albatross";
    let top_fill = "─".repeat(inner_width.saturating_sub(title.len()));
    let bottom_fill = "─".repeat(inner_width + 2);
    let branch = info.branch.unwrap_or("detached");
    let status = if info.dirty { "modified" } else { "clean" };
    let commands = [
        format!("{PAD}{MUTED}/help{RESET}    {TEXT}list commands{RESET}"),
        format!("{PAD}{MUTED}/models{RESET}  {TEXT}change model{RESET}"),
        format!("{PAD}{MUTED}/status{RESET}  {TEXT}inspect this session{RESET}"),
    ];

    [
        format!("{PAD}{MUTED}╭─ {ACCENT}{BOLD}{title}{RESET}{MUTED} {top_fill}{RESET}"),
        panel_row("A terminal coding agent.", inner_width),
        panel_row("", inner_width),
        panel_row(
            &format!("model: {} · backend: {}", info.model, info.backend),
            inner_width,
        ),
        panel_row(
            &format!("project: {} · branch: {}", info.project, branch),
            inner_width,
        ),
        panel_row(
            &format!("mode: {} · approval: {} · {}", info.mode, approval, status),
            inner_width,
        ),
        format!("{PAD}{MUTED}╰{bottom_fill}╯{RESET}"),
        String::new(),
        format!("{PAD}{MUTED}Describe a task or try a command:{RESET}"),
        commands.join("\n"),
    ]
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

        assert_eq!(lines.len(), 12);
        assert!(lines[0].starts_with("  ╭─ Albatross "));
        assert!(rendered.contains("A terminal coding agent."));
        assert!(rendered.contains("model: grok-build-0.1 · backend: grok"));
        assert!(rendered.contains("project: Albatross · branch: main"));
        assert!(rendered.contains("mode: edit · approval: ask for risky actions · modified"));
        assert!(rendered.contains("Describe a task or try a command:"));
        assert!(rendered.contains("/help    list commands"));
        assert!(rendered.contains("/models  change model"));
        assert!(rendered.contains("/status  inspect this session"));
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

        assert_eq!(lines.len(), 12, "startup should stay compact: {rendered:?}");
        for field in [
            "Albatross",
            "A terminal coding agent.",
            "model: gpt-5.2-codex",
            "project: Albatross",
            "mode: review",
            "Describe a task or try a command:",
            "/help    list commands",
        ] {
            assert!(rendered.contains(field), "missing {field:?}: {rendered:?}");
        }
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
        assert!(lines[0].starts_with("  ╭─ Albatross "));
        assert!(rendered.contains("model: grok-build-0.1 · backend: grok"));
        assert!(rendered.contains("Describe a task or try a command:"));
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
