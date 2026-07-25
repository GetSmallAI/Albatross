//! Context reset with a structured handoff artifact (`/reset`).
//!
//! `/reset` drafts a continuation artifact from the current conversation, writes
//! it to `.albatross/continue.md`, then clears the live transcript and seeds
//! a fresh session whose first message *is* that artifact. This is the article's
//! "reset over compaction": a clean context window plus an explicit handoff that
//! carries enough state to continue — rather than an in-place summary that
//! leaves the old context (and its accumulated drift) behind.
//!
//! Drafting mirrors [`crate::handoff`]: a system prompt fixes the section
//! contract, the model drafts, [`ensure_continuation_sections`] normalizes the
//! result, and [`render_fallback_continuation`] gives a deterministic artifact
//! when the model is empty or unreachable.

use chrono::Utc;
use std::path::{Path, PathBuf};

use crate::openai::ChatMessage;

/// Top-level sections every continuation artifact must contain, in order.
const CONTINUATION_SECTIONS: &[&str] = &[
    "Done",
    "In Progress",
    "Key Decisions",
    "Next Steps",
    "Key Files",
];

/// Where `/reset` writes by default: `<workspace>/.albatross/continue.md`.
pub fn default_continuation_path(workspace_root: &str) -> PathBuf {
    Path::new(workspace_root)
        .join(".albatross")
        .join("continue.md")
}

pub fn continuation_system_prompt() -> String {
    [
        "You write a concise handoff note so a fresh agent session can continue this work cleanly.",
        "Capture only what carries forward — not the blow-by-blow. Be specific and concrete.",
        "Return Markdown with exactly these top-level sections, in this order:",
        "## Done",
        "## In Progress",
        "## Key Decisions",
        "## Next Steps",
        "## Key Files",
        "Done: what is finished, as bullets.",
        "In Progress: what is mid-flight and its current state, as bullets.",
        "Key Decisions: choices already made and why, so they aren't relitigated, as bullets.",
        "Next Steps: the concrete actions to take next, as an ordered list.",
        "Key Files: paths that matter, each with a few words on why, as bullets.",
        "If a section has nothing, write `None.`",
    ]
    .join("\n")
}

/// Build the drafting prompt from the conversation (and any prior summary).
pub fn render_continuation_prompt(messages: &[ChatMessage], summary: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str(
        "Write a continuation handoff for the conversation below so a new session can resume the work.\n\n",
    );
    if let Some(s) = summary.filter(|s| !s.trim().is_empty()) {
        out.push_str("Summary of earlier (already-trimmed) context:\n");
        out.push_str(s.trim());
        out.push_str("\n\n");
    }
    out.push_str("Conversation transcript:\n");
    out.push_str(&serialize_transcript(messages));
    out
}

/// Render the conversation into a compact text transcript for the drafting
/// prompt. The system prompt and tool results are omitted — the former is noise
/// for a handoff, the latter is large and low-signal.
fn serialize_transcript(messages: &[ChatMessage]) -> String {
    let mut out = String::new();
    for m in messages {
        match m {
            ChatMessage::User { content } => {
                let text = content.as_text();
                let text = text.trim();
                if !text.is_empty() {
                    out.push_str("\n[user]\n");
                    out.push_str(text);
                    out.push('\n');
                }
            }
            ChatMessage::Assistant {
                content: Some(c), ..
            } if !c.trim().is_empty() => {
                out.push_str("\n[assistant]\n");
                out.push_str(c.trim());
                out.push('\n');
            }
            _ => {} // System and Tool messages are intentionally skipped.
        }
    }
    out
}

/// Normalize a model draft so every required section is present.
pub fn ensure_continuation_sections(markdown: &str) -> String {
    let mut out = markdown.trim().to_string();
    for section in CONTINUATION_SECTIONS {
        if !has_heading(&out, section) {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&format!("## {section}\n\nNone."));
        }
    }
    out.push('\n');
    out
}

/// Case-insensitive heading match, ignoring leading `#`s.
fn has_heading(markdown: &str, section: &str) -> bool {
    markdown.lines().any(|line| {
        let without_hash = line.trim().trim_start_matches('#').trim();
        !without_hash.is_empty() && without_hash.eq_ignore_ascii_case(section)
    })
}

/// Deterministic artifact used when the model draft is empty or fails. Always
/// contains every section so the reset always has a well-formed handoff to seed.
pub fn render_fallback_continuation(messages: &[ChatMessage], error: Option<&str>) -> String {
    let user_count = messages
        .iter()
        .filter(|m| matches!(m, ChatMessage::User { .. }))
        .count();
    let last_user = messages.iter().rev().find_map(|m| match m {
        ChatMessage::User { content } => {
            let t = content.as_text().trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t.chars().take(280).collect::<String>())
            }
        }
        _ => None,
    });

    let mut out = String::new();
    out.push_str("# Continuation\n\n");
    out.push_str(&format!("Generated: {}\n\n", Utc::now().to_rfc3339()));
    if let Some(e) = error {
        out.push_str(&format!("Draft status: model draft failed: {e}\n\n"));
    }
    out.push_str("## Done\n\nNone recorded.\n\n");
    out.push_str("## In Progress\n\n");
    out.push_str(&format!(
        "- {user_count} user message(s) exchanged before the reset.\n\n"
    ));
    out.push_str("## Key Decisions\n\nNone recorded.\n\n");
    out.push_str("## Next Steps\n\n");
    match last_user {
        Some(u) => out.push_str(&format!("1. Resume from the last request: {u}\n\n")),
        None => out.push_str("1. Resume the previous work.\n\n"),
    }
    out.push_str("## Key Files\n\nNone recorded.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> ChatMessage {
        ChatMessage::User {
            content: text.to_string().into(),
        }
    }

    #[test]
    fn default_continuation_path_is_under_albatross() {
        let path = default_continuation_path("/tmp/project");
        assert!(path.ends_with(".albatross/continue.md"));
        assert!(path.starts_with("/tmp/project"));
    }

    #[test]
    fn ensure_continuation_sections_appends_missing() {
        let normalized = ensure_continuation_sections("## Done\n\nShipped the parser.");
        for section in CONTINUATION_SECTIONS {
            assert!(
                normalized.contains(&format!("## {section}")),
                "missing {section}"
            );
        }
        assert!(normalized.contains("Shipped the parser."));
    }

    #[test]
    fn serialize_transcript_keeps_user_and_assistant_only() {
        let messages = vec![
            ChatMessage::System {
                content: "secret system prompt".into(),
            },
            user("add a CSV export"),
        ];
        let text = serialize_transcript(&messages);
        assert!(text.contains("[user]"));
        assert!(text.contains("add a CSV export"));
        assert!(!text.contains("secret system prompt"));
    }

    #[test]
    fn fallback_contains_all_sections_and_last_request() {
        let messages = vec![user("first ask"), user("make the parser robust")];
        let body = render_fallback_continuation(&messages, Some("boom"));
        for section in CONTINUATION_SECTIONS {
            assert!(body.contains(&format!("## {section}")), "missing {section}");
        }
        assert!(body.contains("make the parser robust"));
        assert!(body.contains("model draft failed: boom"));
        assert!(body.contains("2 user message(s)"));
    }
}
