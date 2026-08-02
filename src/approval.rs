use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::Value;
use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;

use crate::agent::ApprovalProvider;
use crate::tools::ToolPreview;

use crate::theme::{FAIL, OK};

const RESET: crate::theme::Style = crate::theme::RESET;
const DIM: crate::theme::Style = crate::theme::MUTED;
const RED: crate::theme::Style = crate::theme::ERROR;
const GREEN: crate::theme::Style = crate::theme::SUCCESS;

pub struct ApprovalCache {
    pub always_allow: HashSet<String>,
    workspace_root: PathBuf,
}

impl ApprovalCache {
    pub fn new() -> Self {
        Self {
            always_allow: HashSet::new(),
            workspace_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ApprovalOption {
    shortcut: char,
    label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ApprovalPrompt {
    body: Vec<String>,
    options: Vec<ApprovalOption>,
}

fn build_approval_prompt(
    name: &str,
    args: &Value,
    preview: Option<&ToolPreview>,
) -> ApprovalPrompt {
    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
    let mut body = match name {
        "list_dir" => vec![
            "Read directory outside the configured workspace".into(),
            path.to_string(),
        ],
        "file_read" => vec![
            "Read file outside the configured workspace".into(),
            path.to_string(),
        ],
        "grep" => {
            let mut lines = vec!["Search outside the configured workspace".into()];
            if !path.is_empty() {
                lines.push(format!("Path: {path}"));
            }
            if let Some(pattern) = args.get("pattern").and_then(Value::as_str) {
                lines.push(format!("Pattern: {pattern}"));
            }
            lines
        }
        "glob" => {
            let mut lines = vec!["Match files outside the configured workspace".into()];
            if !path.is_empty() {
                lines.push(format!("Path: {path}"));
            }
            if let Some(pattern) = args.get("pattern").and_then(Value::as_str) {
                lines.push(format!("Pattern: {pattern}"));
            }
            lines
        }
        "shell" => vec![
            "Run shell command".into(),
            format!(
                "$ {}",
                args.get("command").and_then(Value::as_str).unwrap_or("")
            ),
        ],
        "web_fetch" => vec![
            "Access the network".into(),
            args.get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ],
        "run_tests" => {
            let mode = args.get("mode").and_then(Value::as_str).unwrap_or("smart");
            let mut lines = vec!["Run project tests".into(), format!("Mode: {mode}")];
            if let Some(pattern) = args.get("pattern").and_then(Value::as_str) {
                lines.push(format!("Pattern: {pattern}"));
            }
            lines
        }
        _ => vec![preview
            .map(|value| value.summary.clone())
            .unwrap_or_else(|| format!("Call {}", name.replace('_', " ")))],
    };
    if let Some(risk) = preview.and_then(|value| value.risk.as_deref()) {
        body.push(format!("Why approval is needed: {risk}"));
    }
    let exact_scope = match name {
        "list_dir" => "this directory",
        "shell" => "this command",
        "file_read" | "file_write" | "file_edit" => "this file",
        "grep" | "glob" => "this search",
        "web_fetch" => "this URL",
        "run_tests" => "this test run",
        _ => "this exact call",
    };

    ApprovalPrompt {
        body,
        options: vec![
            ApprovalOption {
                shortcut: 'y',
                label: "Allow once".into(),
            },
            ApprovalOption {
                shortcut: 's',
                label: format!("Allow {exact_scope} for the session"),
            },
            ApprovalOption {
                shortcut: 'a',
                label: format!("Allow every {name} call this session — broader access"),
            },
            ApprovalOption {
                shortcut: 'n',
                label: "Deny".into(),
            },
        ],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalChoice {
    Once,
    ExactForSession,
    ToolForSession,
    Deny,
}

fn approval_shortcut(value: &str) -> Option<ApprovalChoice> {
    match value.trim().to_ascii_lowercase().as_str() {
        "y" => Some(ApprovalChoice::Once),
        "s" => Some(ApprovalChoice::ExactForSession),
        "a" => Some(ApprovalChoice::ToolForSession),
        "n" | "" => Some(ApprovalChoice::Deny),
        _ => None,
    }
}

async fn prompt_for_approval(prompt: &ApprovalPrompt) -> ApprovalChoice {
    println!("  Permission required");
    for line in &prompt.body {
        println!("    {line}");
    }
    for option in &prompt.options {
        println!("  [{}] {}", option.shortcut, option.label);
    }

    print!("  Allow? [y/s/a/n]: ");
    let _ = std::io::stdout().flush();
    let choice = tokio::task::spawn_blocking(read_approval_shortcut)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or(ApprovalChoice::Deny);
    println!();
    choice
}

fn read_approval_shortcut() -> anyhow::Result<ApprovalChoice> {
    crate::cursor::set_state(crate::cursor::CursorState::Passive)?;
    crossterm::terminal::enable_raw_mode()?;
    let result = loop {
        let Event::Key(KeyEvent {
            code,
            modifiers,
            kind,
            ..
        }) = crossterm::event::read()?
        else {
            continue;
        };
        if kind == KeyEventKind::Release {
            continue;
        }
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
            crate::cursor::restore();
            std::process::exit(0);
        }
        if let KeyCode::Char(key) = code {
            if let Some(choice) = approval_shortcut(&key.to_string()) {
                break choice;
            }
        }
    };
    crossterm::terminal::disable_raw_mode()?;
    Ok(result)
}

fn approval_cache_key(name: &str, args: &Value) -> String {
    let string_arg = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or("");
    let scope = match name {
        "shell" => string_arg("command").to_string(),
        "list_dir" => string_arg("path").to_string(),
        "file_read" | "file_write" | "file_edit" => string_arg("path").to_string(),
        "web_fetch" => string_arg("url").to_string(),
        "grep" | "glob" | "run_tests" => args.to_string(),
        _ => args.to_string(),
    };
    format!("{name}:{scope}")
}

#[async_trait]
impl ApprovalProvider for ApprovalCache {
    async fn approve(&mut self, name: &str, args: &Value, preview: Option<&ToolPreview>) -> bool {
        let cache_key = approval_cache_key(name, args);
        if self.always_allow.contains(name) || self.always_allow.contains(&cache_key) {
            return true;
        }
        if let Some(diff) = preview.and_then(|value| value.diff.as_deref()) {
            println!();
            crate::diff_view::print_compact_preview(diff, &self.workspace_root, 12);
            println!();
        } else {
            println!();
        }
        let prompt = build_approval_prompt(name, args, preview);
        let exact_receipt = prompt.options[1]
            .label
            .strip_prefix("Allow ")
            .and_then(|label| label.strip_suffix(" for the session"))
            .unwrap_or("this exact request")
            .to_string();
        let receipt = prompt
            .body
            .iter()
            .take_while(|line| !line.starts_with("Why approval is needed:"))
            .cloned()
            .collect::<Vec<_>>()
            .join(" · ");
        let choice = prompt_for_approval(&prompt).await;

        match choice {
            ApprovalChoice::Once => {
                println!("  {GREEN}{OK}{RESET} {DIM}Allowed once · {receipt}{RESET}");
                true
            }
            ApprovalChoice::ExactForSession => {
                self.always_allow.insert(cache_key);
                println!(
                    "  {GREEN}{OK}{RESET} {DIM}Allowed for the session · {exact_receipt}{RESET}"
                );
                true
            }
            ApprovalChoice::ToolForSession => {
                self.always_allow.insert(name.to_string());
                println!(
                    "  {GREEN}{OK}{RESET} {DIM}Allowed every {name} call for the session{RESET}"
                );
                true
            }
            ApprovalChoice::Deny => {
                println!("  {RED}{FAIL}{RESET} {DIM}Denied · {receipt}{RESET}");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn external_directory_prompt_explains_action_and_grant_scopes() {
        let prompt = build_approval_prompt("list_dir", &json!({ "path": "/tmp/Albatross" }), None);

        assert_eq!(
            prompt.body,
            vec![
                "Read directory outside the configured workspace",
                "/tmp/Albatross"
            ]
        );
        assert_eq!(
            prompt
                .options
                .iter()
                .map(|option| (option.label.as_str(), option.shortcut))
                .collect::<Vec<_>>(),
            vec![
                ("Allow once", 'y'),
                ("Allow this directory for the session", 's'),
                (
                    "Allow every list_dir call this session — broader access",
                    'a'
                ),
                ("Deny", 'n'),
            ]
        );
    }

    #[test]
    fn shell_prompt_formats_the_command_instead_of_raw_arguments() {
        let prompt = build_approval_prompt(
            "shell",
            &json!({ "command": "cargo test", "timeout": 120 }),
            None,
        );

        assert_eq!(prompt.body, vec!["Run shell command", "$ cargo test"]);
        assert!(!prompt.body.join("\n").contains("timeout"));
        assert_eq!(
            prompt.options[1].label,
            "Allow this command for the session"
        );
    }

    #[test]
    fn approval_prompt_surfaces_the_specific_risk() {
        let preview = ToolPreview {
            summary: "Edit /tmp/notes.md (1 edit)".into(),
            diff: None,
            risk: Some("outside workspace root /workspace".into()),
        };
        let prompt = build_approval_prompt(
            "file_edit",
            &json!({ "path": "/tmp/notes.md" }),
            Some(&preview),
        );

        assert_eq!(
            prompt.body,
            vec![
                "Edit /tmp/notes.md (1 edit)",
                "Why approval is needed: outside workspace root /workspace"
            ]
        );
        assert_eq!(prompt.options[1].label, "Allow this file for the session");
    }

    #[test]
    fn approval_shortcuts_map_to_the_expected_approval_scope() {
        assert_eq!(approval_shortcut("Y"), Some(ApprovalChoice::Once));
        assert_eq!(
            approval_shortcut("s"),
            Some(ApprovalChoice::ExactForSession)
        );
        assert_eq!(approval_shortcut("a"), Some(ApprovalChoice::ToolForSession));
        assert_eq!(approval_shortcut("n"), Some(ApprovalChoice::Deny));
        assert_eq!(approval_shortcut(""), Some(ApprovalChoice::Deny));
        assert_eq!(approval_shortcut("invalid"), None);
    }

    #[test]
    fn common_tools_get_human_readable_prompts_without_json() {
        let cases = [
            (
                "file_read",
                json!({ "path": "/tmp/notes.md", "limit": 20 }),
                "Read file outside the configured workspace",
            ),
            (
                "grep",
                json!({ "pattern": "TODO", "path": "/tmp/project" }),
                "Search outside the configured workspace",
            ),
            (
                "web_fetch",
                json!({ "url": "https://example.com", "max_bytes": 1024 }),
                "Access the network",
            ),
            ("run_tests", json!({ "mode": "all" }), "Run project tests"),
        ];

        for (name, args, heading) in cases {
            let prompt = build_approval_prompt(name, &args, None);
            let rendered = prompt.body.join("\n");
            assert_eq!(prompt.body[0], heading, "tool: {name}");
            assert!(
                !rendered.contains('{'),
                "raw JSON leaked for {name}: {rendered}"
            );
            assert!(
                !rendered.contains('}'),
                "raw JSON leaked for {name}: {rendered}"
            );
        }
    }

    #[test]
    fn session_cache_keys_match_the_scope_promised_by_the_picker() {
        assert_ne!(
            approval_cache_key("web_fetch", &json!({ "url": "https://one.example" })),
            approval_cache_key("web_fetch", &json!({ "url": "https://two.example" }))
        );
        assert_ne!(
            approval_cache_key("grep", &json!({ "path": "src", "pattern": "TODO" })),
            approval_cache_key("grep", &json!({ "path": "src", "pattern": "FIXME" }))
        );
        assert_eq!(
            approval_cache_key(
                "file_write",
                &json!({ "path": "notes.md", "content": "first" })
            ),
            approval_cache_key(
                "file_write",
                &json!({ "path": "notes.md", "content": "second" })
            ),
            "a file-scoped grant should survive content changes"
        );
    }
}
