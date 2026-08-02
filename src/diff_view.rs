//! Colored rendering for unified diffs produced by [`crate::tools::diff::unified_diff`].
//!
//! Classification is order-sensitive: file headers (`---`/`+++`) must be
//! checked before the single-character `-`/`+` line markers, since a header
//! line also starts with those characters.

use crate::theme::{ACCENT, ERROR, MUTED, RESET, SUCCESS};
use std::path::Path;

struct FileDiff<'a> {
    old_path: String,
    new_path: String,
    changed_lines: Vec<&'a str>,
}

fn display_path(raw: &str, workspace_root: &Path) -> String {
    let raw = raw.split('\t').next().unwrap_or(raw);
    if raw == "/dev/null" {
        return String::new();
    }
    let path = Path::new(raw);
    if let Ok(relative) = path.strip_prefix(workspace_root) {
        return relative.display().to_string();
    }
    raw.strip_prefix("a/")
        .or_else(|| raw.strip_prefix("b/"))
        .unwrap_or(raw)
        .to_string()
}

fn parse_file_diffs(diff: &str) -> Vec<FileDiff<'_>> {
    let mut files = Vec::new();
    let mut pending_old_path = None;

    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("--- ") {
            pending_old_path = Some(path.to_string());
        } else if let Some(path) = line.strip_prefix("+++ ") {
            files.push(FileDiff {
                old_path: pending_old_path.take().unwrap_or_default(),
                new_path: path.to_string(),
                changed_lines: Vec::new(),
            });
        } else if line.starts_with('+') || line.starts_with('-') {
            if let Some(file) = files.last_mut() {
                file.changed_lines.push(line);
            }
        }
    }
    files
}

fn change_counts(file: &FileDiff<'_>) -> (usize, usize) {
    let added = file
        .changed_lines
        .iter()
        .filter(|line| line.starts_with('+'))
        .count();
    let removed = file.changed_lines.len().saturating_sub(added);
    (added, removed)
}

fn change_action(file: &FileDiff<'_>) -> &'static str {
    match change_counts(file) {
        (0, _) => "delete",
        (_, 0) => "create",
        _ => "edit",
    }
}

fn file_display_path(file: &FileDiff<'_>, workspace_root: &Path) -> String {
    let new_path = display_path(&file.new_path, workspace_root);
    if new_path.is_empty() {
        display_path(&file.old_path, workspace_root)
    } else {
        new_path
    }
}

/// Apply theme colors to a single diff line based on its unified-diff prefix.
/// Pure and side-effect-free so classification is directly unit-testable.
pub fn colorize_diff_line(line: &str) -> String {
    if line.starts_with("+++") || line.starts_with("---") {
        format!("{MUTED}{line}{RESET}")
    } else if line.starts_with("@@") {
        format!("{ACCENT}{line}{RESET}")
    } else if let Some(rest) = line.strip_prefix('+') {
        format!("{SUCCESS}+{rest}{RESET}")
    } else if let Some(rest) = line.strip_prefix('-') {
        format!("{ERROR}-{rest}{RESET}")
    } else {
        format!("{MUTED}{line}{RESET}")
    }
}

fn render_diff(diff: &str, max_lines: usize) -> String {
    let mut out = String::new();
    for line in diff.lines().take(max_lines) {
        out.push_str("  ");
        out.push_str(&colorize_diff_line(line));
        out.push('\n');
    }
    if diff.lines().count() > max_lines {
        out.push_str(&format!("  {MUTED}…diff truncated for display{RESET}\n"));
    }
    out
}

/// Render a summary-first approval preview for a unified diff. File headers and
/// hunk coordinates are implementation details; the user sees one relative
/// path followed by only the lines that will actually change.
pub fn render_compact_preview(diff: &str, workspace_root: &Path, max_lines: usize) -> String {
    let files = parse_file_diffs(diff);
    let mut remaining_lines = max_lines;
    let mut out = if files.len() == 1 {
        format!(
            "  {MUTED}Proposed {}{RESET}  {}\n",
            change_action(&files[0]),
            file_display_path(&files[0], workspace_root)
        )
    } else {
        format!("  {MUTED}Proposed patch · {} files{RESET}\n", files.len())
    };

    for file in &files {
        let indent = if files.len() == 1 { "    " } else { "      " };
        if files.len() > 1 {
            let (added, removed) = change_counts(file);
            out.push_str(&format!(
                "    {}  {SUCCESS}+{added}{RESET} {ERROR}−{removed}{RESET}\n",
                file_display_path(file, workspace_root)
            ));
        }
        let visible = file.changed_lines.len().min(remaining_lines);
        for line in file.changed_lines.iter().take(visible) {
            out.push_str(indent);
            out.push_str(&colorize_diff_line(line));
            out.push('\n');
        }
        remaining_lines = remaining_lines.saturating_sub(visible);
        if file.changed_lines.len() > visible {
            out.push_str(&format!(
                "{indent}{MUTED}… {} more changed lines{RESET}\n",
                file.changed_lines.len() - visible
            ));
        }
    }
    out
}

/// Print a complete diff for explicit inspection commands such as `/undo`.
pub fn print_diff(diff: &str, max_lines: usize) {
    print!("{}", render_diff(diff, max_lines));
}

pub fn print_compact_preview(diff: &str, workspace_root: &Path, max_lines: usize) {
    print!(
        "{}",
        render_compact_preview(diff, workspace_root, max_lines)
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn compact_preview_uses_one_workspace_relative_path_and_only_changed_lines() {
        let diff = "--- /workspace/.albatross/ui-receipt-demo.txt\n+++ /workspace/.albatross/ui-receipt-demo.txt\n@@ -1 +1 @@\n+alpha\n+beta";

        let rendered = render_compact_preview(diff, Path::new("/workspace"), 12);

        assert!(rendered.contains("Proposed create"));
        assert_eq!(
            rendered.matches(".albatross/ui-receipt-demo.txt").count(),
            1
        );
        assert!(!rendered.contains("/workspace/"));
        assert!(!rendered.contains("---"));
        assert!(!rendered.contains("+++"));
        assert!(!rendered.contains("@@"));
        assert!(rendered.contains("+alpha"));
        assert!(rendered.contains("+beta"));
    }

    #[test]
    fn compact_preview_groups_multi_file_patches_without_repeating_diff_headers() {
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old lib\n+new lib\ndiff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -2 +2 @@\n-old main\n+new main";

        let rendered = render_compact_preview(diff, Path::new("/workspace"), 12);

        assert!(rendered.contains("Proposed patch · 2 files"));
        assert_eq!(rendered.matches("src/lib.rs").count(), 1);
        assert_eq!(rendered.matches("src/main.rs").count(), 1);
        assert!(rendered.contains("-old lib"));
        assert!(rendered.contains("+new main"));
        assert!(!rendered.contains("diff --git"));
        assert!(!rendered.contains("@@"));
    }

    #[test]
    fn classifies_added_line() {
        let out = colorize_diff_line("+hello");
        assert!(out.contains('+'));
        assert!(out.contains("hello"));
        assert!(out.starts_with(&SUCCESS.to_string()));
    }

    #[test]
    fn classifies_removed_line() {
        let out = colorize_diff_line("-hello");
        assert!(out.starts_with(&ERROR.to_string()));
    }

    #[test]
    fn classifies_hunk_header() {
        let out = colorize_diff_line("@@ -1 +1 @@");
        assert!(out.starts_with(&ACCENT.to_string()));
    }

    #[test]
    fn file_headers_are_not_treated_as_added_or_removed() {
        let plus_header = colorize_diff_line("+++ path.rs");
        let minus_header = colorize_diff_line("--- path.rs");
        assert!(plus_header.starts_with(&MUTED.to_string()));
        assert!(minus_header.starts_with(&MUTED.to_string()));
        assert!(!plus_header.starts_with(&SUCCESS.to_string()));
        assert!(!minus_header.starts_with(&ERROR.to_string()));
    }

    #[test]
    fn plain_context_line_is_muted() {
        let out = colorize_diff_line("unchanged context");
        assert!(out.starts_with(&MUTED.to_string()));
    }

    #[test]
    fn compact_preview_reports_hidden_changed_lines_past_the_cap() {
        let diff = "--- /workspace/notes.txt\n+++ /workspace/notes.txt\n@@ -1 +1 @@\n+line0\n+line1\n+line2\n+line3\n+line4";

        let rendered = render_compact_preview(diff, Path::new("/workspace"), 3);

        assert!(rendered.contains("… 2 more changed lines"));
        assert!(rendered.contains("line2"));
        assert!(!rendered.contains("line3"));
    }

    #[test]
    fn compact_preview_has_no_truncation_notice_when_under_cap() {
        let diff = "--- /workspace/notes.txt\n+++ /workspace/notes.txt\n@@ -1 +1 @@\n+only line";

        let rendered = render_compact_preview(diff, Path::new("/workspace"), 12);

        assert!(!rendered.contains("more changed lines"));
    }
}
