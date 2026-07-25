use anyhow::{anyhow, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::backends::BackendName;
use crate::project_memory::ProjectIndexFreshness;
use crate::shipcheck::{GitFileKind, ShipcheckSnapshot};

pub const HANDOFF_CONTEXT_LIMIT_BYTES: usize = 40 * 1024;
const TRUNCATION_MARKER: &str = "\n\n[... handoff context truncated ...]\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffBasis {
    DirtyTree,
    AheadOfUpstream,
}

impl HandoffBasis {
    pub fn label(self) -> &'static str {
        match self {
            HandoffBasis::DirtyTree => "dirty working tree",
            HandoffBasis::AheadOfUpstream => "commits ahead of upstream",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffContext {
    pub basis: HandoffBasis,
    pub content: String,
    pub truncated: bool,
}

pub fn should_refuse_cloud_handoff(backend: BackendName, allow_cloud: bool) -> bool {
    !backend.is_local() && !allow_cloud
}

pub fn default_export_path(session_dir: &str) -> PathBuf {
    Path::new(session_dir).join("handoff").join(format!(
        "{}.md",
        Utc::now().format("%Y-%m-%dT%H-%M-%S-%3fZ")
    ))
}

pub fn collect_handoff_context(snapshot: &ShipcheckSnapshot) -> Result<Option<HandoffContext>> {
    build_handoff_context(snapshot, HANDOFF_CONTEXT_LIMIT_BYTES)
}

pub fn build_handoff_context(
    snapshot: &ShipcheckSnapshot,
    limit_bytes: usize,
) -> Result<Option<HandoffContext>> {
    if !snapshot.is_clean() {
        return build_dirty_context(snapshot, limit_bytes).map(Some);
    }
    if snapshot.branch.ahead > 0 {
        if let Some(upstream) = snapshot.branch.upstream.as_deref() {
            return build_ahead_context(snapshot, upstream, limit_bytes).map(Some);
        }
    }
    Ok(None)
}

fn build_dirty_context(snapshot: &ShipcheckSnapshot, limit_bytes: usize) -> Result<HandoffContext> {
    let staged_diff = run_git(&snapshot.workspace_root, &["diff", "--cached", "--"])?;
    let unstaged_diff = run_git(&snapshot.workspace_root, &["diff", "--"])?;
    let untracked = snapshot
        .files
        .iter()
        .filter(|file| file.kind == GitFileKind::Untracked)
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();

    let mut out = String::new();
    let mut truncated = false;
    push_limited(
        &mut out,
        &shipcheck_summary(snapshot, "Dirty working tree"),
        limit_bytes,
        &mut truncated,
    );
    if !untracked.is_empty() {
        push_limited(
            &mut out,
            "\n## Untracked Files\n\n",
            limit_bytes,
            &mut truncated,
        );
        for path in untracked {
            push_limited(
                &mut out,
                &format!("- `{path}`\n"),
                limit_bytes,
                &mut truncated,
            );
        }
    }
    push_diff_section(
        &mut out,
        "Staged Diff",
        &staged_diff,
        limit_bytes,
        &mut truncated,
    );
    push_diff_section(
        &mut out,
        "Unstaged Diff",
        &unstaged_diff,
        limit_bytes,
        &mut truncated,
    );

    Ok(HandoffContext {
        basis: HandoffBasis::DirtyTree,
        content: out,
        truncated,
    })
}

fn build_ahead_context(
    snapshot: &ShipcheckSnapshot,
    upstream: &str,
    limit_bytes: usize,
) -> Result<HandoffContext> {
    let range = format!("{upstream}..HEAD");
    let commits = run_git(&snapshot.workspace_root, &["log", "--oneline", &range])?;
    let diff = run_git(&snapshot.workspace_root, &["diff", &range, "--"])?;

    let mut out = String::new();
    let mut truncated = false;
    push_limited(
        &mut out,
        &shipcheck_summary(snapshot, "Branch ahead of upstream"),
        limit_bytes,
        &mut truncated,
    );
    push_limited(&mut out, "\n## Commits\n\n", limit_bytes, &mut truncated);
    if commits.trim().is_empty() {
        push_limited(
            &mut out,
            "No commit list detected.\n",
            limit_bytes,
            &mut truncated,
        );
    } else {
        push_limited(&mut out, "```text\n", limit_bytes, &mut truncated);
        push_limited(&mut out, commits.trim(), limit_bytes, &mut truncated);
        push_limited(&mut out, "\n```\n", limit_bytes, &mut truncated);
    }
    push_diff_section(
        &mut out,
        "Upstream Diff",
        &diff,
        limit_bytes,
        &mut truncated,
    );

    Ok(HandoffContext {
        basis: HandoffBasis::AheadOfUpstream,
        content: out,
        truncated,
    })
}

fn shipcheck_summary(snapshot: &ShipcheckSnapshot, basis: &str) -> String {
    format!(
        "# Handoff Source Context\n\n## Basis\n\n{basis}\n\n## Shipcheck\n\n- Workspace: `{}`\n- Git root: `{}`\n- Branch: `{}`\n- Staged files: {}\n- Unstaged files: {}\n- Untracked files: {}\n- Conflicts: {}\n",
        snapshot.workspace_root,
        snapshot.git_root,
        snapshot.branch_label(),
        snapshot.staged_count(),
        snapshot.unstaged_count(),
        snapshot.untracked_count(),
        snapshot.conflict_count()
    )
}

fn push_diff_section(
    out: &mut String,
    title: &str,
    diff: &str,
    limit_bytes: usize,
    truncated: &mut bool,
) {
    push_limited(out, &format!("\n## {title}\n\n"), limit_bytes, truncated);
    if diff.trim().is_empty() {
        push_limited(out, "No changes.\n", limit_bytes, truncated);
    } else {
        push_limited(out, "```diff\n", limit_bytes, truncated);
        push_limited(out, diff.trim(), limit_bytes, truncated);
        push_limited(out, "\n```\n", limit_bytes, truncated);
    }
}

fn push_limited(out: &mut String, text: &str, limit_bytes: usize, truncated: &mut bool) {
    if *truncated || out.len() >= limit_bytes {
        *truncated = true;
        return;
    }
    if out.len() + text.len() <= limit_bytes {
        out.push_str(text);
        return;
    }

    *truncated = true;
    let remaining = limit_bytes.saturating_sub(out.len());
    if remaining == 0 {
        return;
    }
    let marker = if remaining >= TRUNCATION_MARKER.len() {
        TRUNCATION_MARKER
    } else {
        ""
    };
    let prefix_len = remaining.saturating_sub(marker.len());
    out.push_str(prefix_at_char_boundary(text, prefix_len));
    out.push_str(marker);
}

fn prefix_at_char_boundary(text: &str, max_bytes: usize) -> &str {
    if max_bytes >= text.len() {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn run_git(workspace_root: &str, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(args)
        .output()
        .map_err(|e| anyhow!("failed to run git: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        return Err(anyhow!(
            "git {} failed{}",
            args.join(" "),
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn handoff_system_prompt() -> String {
    [
        "You draft concise release handoff copy for Albatross.",
        "Use only the supplied git context. Do not invent tests, issue numbers, or files.",
        "Return Markdown with exactly these top-level sections:",
        "## Commit Message",
        "## Changelog Bullets",
        "## X Post",
        "## Testing",
        "The commit message must be one line.",
        "The X post should be shareable as-is, with one short intro and 3-4 bullets.",
        "If testing evidence is not explicitly present in the context, write `Not detected.` under Testing.",
    ]
    .join("\n")
}

pub fn render_handoff_prompt(
    context: &HandoffContext,
    freshness: Option<&ProjectIndexFreshness>,
) -> String {
    let mut out = String::new();
    out.push_str("Draft release handoff copy for this Albatross change.\n\n");
    out.push_str("Rules:\n");
    out.push_str("- Keep the commit message conventional and one line.\n");
    out.push_str("- Changelog bullets should be user-facing and concise.\n");
    out.push_str("- Do not claim tests ran unless they appear in the supplied context.\n");
    if context.truncated {
        out.push_str("- The git context was truncated; mention that the draft is based on partial diff context only if it affects confidence.\n");
    }
    out.push('\n');
    push_project_memory_summary(&mut out, freshness);
    out.push('\n');
    out.push_str(&context.content);
    out
}

fn push_project_memory_summary(out: &mut String, freshness: Option<&ProjectIndexFreshness>) {
    out.push_str("Project memory freshness:\n");
    match freshness {
        Some(report) if report.indexed_files > 0 || report.workspace_files > 0 => {
            out.push_str(&format!(
                "- indexed={} workspace={} fresh={} stale={} missing={} deleted={} errors={}\n",
                report.indexed_files,
                report.workspace_files,
                report.fresh,
                report.stale,
                report.missing,
                report.deleted,
                report.read_errors
            ));
        }
        Some(_) => out.push_str("- no project-memory index found\n"),
        None => out.push_str("- project memory disabled\n"),
    }
}

pub fn ensure_required_sections(markdown: &str) -> String {
    let mut out = markdown.trim().to_string();
    for section in ["Commit Message", "Changelog Bullets", "X Post", "Testing"] {
        if !has_heading(&out, section) {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&format!("## {section}\n\nNot detected."));
        }
    }
    out.push('\n');
    out
}

fn has_heading(markdown: &str, section: &str) -> bool {
    markdown.lines().any(|line| {
        let trimmed = line.trim();
        let without_hash = trimmed.trim_start_matches('#').trim();
        !without_hash.is_empty() && without_hash.eq_ignore_ascii_case(section)
    })
}

pub fn render_fallback_markdown(
    context: &HandoffContext,
    snapshot: &ShipcheckSnapshot,
    freshness: Option<&ProjectIndexFreshness>,
    error: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str("# Albatross Handoff\n\n");
    out.push_str(&format!("Generated: {}\n\n", Utc::now().to_rfc3339()));
    if let Some(error) = error {
        out.push_str(&format!("Draft status: model draft failed: {error}\n\n"));
    }
    out.push_str("## Commit Message\n\n");
    out.push_str(match context.basis {
        HandoffBasis::DirtyTree => "feat: prepare local working tree handoff\n\n",
        HandoffBasis::AheadOfUpstream => "feat: summarize branch handoff\n\n",
    });
    out.push_str("## Changelog Bullets\n\n");
    out.push_str(&format!("- Handoff basis: {}\n", context.basis.label()));
    out.push_str(&format!("- Branch: `{}`\n", snapshot.branch_label()));
    out.push_str(&format!(
        "- Files: staged={} unstaged={} untracked={} conflicts={}\n",
        snapshot.staged_count(),
        snapshot.unstaged_count(),
        snapshot.untracked_count(),
        snapshot.conflict_count()
    ));
    if context.truncated {
        out.push_str("- Diff context was truncated before drafting.\n");
    }
    out.push_str("\n## X Post\n\n");
    out.push_str("Albatross handoff draft:\n\n");
    out.push_str(&format!("- {}\n", context.basis.label()));
    out.push_str(&format!("- Branch `{}`\n", snapshot.branch_label()));
    out.push_str("- Shipcheck facts included for release review\n");
    out.push_str("- Testing remains to be confirmed\n\n");
    out.push_str("## Testing\n\n");
    out.push_str("Not detected.\n\n");
    out.push_str("## Source Facts\n\n");
    push_project_memory_summary(&mut out, freshness);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shipcheck::collect_shipcheck;
    use std::fs;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test User"]);
        fs::write(dir.join("README.md"), "hello\n").unwrap();
        git(dir, &["add", "README.md"]);
        git(dir, &["commit", "-m", "initial"]);
    }

    #[test]
    fn selects_dirty_tree_basis_and_lists_untracked_without_content() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        fs::write(dir.path().join("README.md"), "hello staged\n").unwrap();
        git(dir.path(), &["add", "README.md"]);
        fs::write(dir.path().join("README.md"), "hello unstaged\n").unwrap();
        fs::write(dir.path().join("secret-notes.md"), "do not include me\n").unwrap();
        let snapshot = collect_shipcheck(dir.path().to_str().unwrap()).unwrap();

        let context = build_handoff_context(&snapshot, 40 * 1024)
            .unwrap()
            .unwrap();

        assert_eq!(context.basis, HandoffBasis::DirtyTree);
        assert!(context.content.contains("## Staged Diff"));
        assert!(context.content.contains("## Unstaged Diff"));
        assert!(context.content.contains("- `secret-notes.md`"));
        assert!(!context.content.contains("do not include me"));
    }

    #[test]
    fn selects_ahead_basis_for_clean_branch_with_upstream() {
        let dir = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        git(remote.path(), &["init", "--bare"]);
        let remote_path = remote.path().to_str().unwrap();
        git(dir.path(), &["remote", "add", "origin", remote_path]);
        let branch = git_out(dir.path(), &["branch", "--show-current"]);
        git(dir.path(), &["push", "-u", "origin", &branch]);
        fs::write(dir.path().join("README.md"), "hello ahead\n").unwrap();
        git(dir.path(), &["add", "README.md"]);
        git(dir.path(), &["commit", "-m", "ahead change"]);
        let snapshot = collect_shipcheck(dir.path().to_str().unwrap()).unwrap();

        let context = build_handoff_context(&snapshot, 40 * 1024)
            .unwrap()
            .unwrap();

        assert_eq!(context.basis, HandoffBasis::AheadOfUpstream);
        assert!(context.content.contains("## Commits"));
        assert!(context.content.contains("ahead change"));
        assert!(context.content.contains("## Upstream Diff"));
    }

    #[test]
    fn clean_branch_without_ahead_has_no_handoff_context() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let snapshot = collect_shipcheck(dir.path().to_str().unwrap()).unwrap();

        assert!(build_handoff_context(&snapshot, 40 * 1024)
            .unwrap()
            .is_none());
    }

    #[test]
    fn truncates_context_with_marker() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        fs::write(dir.path().join("README.md"), "x\n".repeat(500)).unwrap();
        let snapshot = collect_shipcheck(dir.path().to_str().unwrap()).unwrap();

        let context = build_handoff_context(&snapshot, 700).unwrap().unwrap();

        assert!(context.truncated);
        assert!(context.content.len() <= 700);
        assert!(context
            .content
            .contains("[... handoff context truncated ...]"));
    }

    #[test]
    fn cloud_backends_require_explicit_cloud_flag() {
        assert!(should_refuse_cloud_handoff(BackendName::Openrouter, false));
        assert!(!should_refuse_cloud_handoff(BackendName::Openrouter, true));
        assert!(should_refuse_cloud_handoff(BackendName::OpenAi, false));
        assert!(!should_refuse_cloud_handoff(BackendName::OpenAi, true));
        assert!(!should_refuse_cloud_handoff(BackendName::Ollama, false));
    }

    #[test]
    fn fallback_and_section_normalization_include_required_sections() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        fs::write(dir.path().join("README.md"), "changed\n").unwrap();
        let snapshot = collect_shipcheck(dir.path().to_str().unwrap()).unwrap();
        let context = build_handoff_context(&snapshot, 40 * 1024)
            .unwrap()
            .unwrap();
        let fallback = render_fallback_markdown(&context, &snapshot, None, Some("boom"));
        let normalized = ensure_required_sections("## Commit Message\n\nfeat: test");

        for section in [
            "## Commit Message",
            "## Changelog Bullets",
            "## X Post",
            "## Testing",
        ] {
            assert!(fallback.contains(section));
            assert!(normalized.contains(section));
        }
        assert!(fallback.contains("model draft failed: boom"));
    }

    #[test]
    fn default_export_path_uses_handoff_directory() {
        let path = default_export_path(".sessions");
        assert!(path.starts_with(".sessions/handoff"));
        assert_eq!(path.extension().and_then(|s| s.to_str()), Some("md"));
    }
}
