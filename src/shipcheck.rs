use anyhow::{anyhow, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

use crate::config::{AgentConfig, OperatorMode};
use crate::project_memory::ProjectIndexFreshness;
use crate::test_integration::{discover_tests, run_tests};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitFileKind {
    Tracked,
    Renamed,
    Untracked,
    Conflict,
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFileState {
    pub path: String,
    pub original_path: Option<String>,
    pub staged: Option<char>,
    pub unstaged: Option<char>,
    pub kind: GitFileKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitBranchState {
    pub oid: Option<String>,
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: i32,
    pub behind: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShipcheckSnapshot {
    pub workspace_root: String,
    pub git_root: String,
    pub branch: GitBranchState,
    pub files: Vec<GitFileState>,
    pub staged_diff_stat: String,
    pub unstaged_diff_stat: String,
    pub test_status: Option<TestStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestStatus {
    pub framework: String,
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub exit_code: i32,
    pub error: Option<String>,
}

impl ShipcheckSnapshot {
    pub fn is_clean(&self) -> bool {
        self.files.is_empty()
    }

    pub fn staged_count(&self) -> usize {
        self.files
            .iter()
            .filter(|file| status_changed(file.staged))
            .count()
    }

    pub fn unstaged_count(&self) -> usize {
        self.files
            .iter()
            .filter(|file| status_changed(file.unstaged))
            .count()
    }

    pub fn untracked_count(&self) -> usize {
        self.files
            .iter()
            .filter(|file| file.kind == GitFileKind::Untracked)
            .count()
    }

    pub fn conflict_count(&self) -> usize {
        self.files
            .iter()
            .filter(|file| file.kind == GitFileKind::Conflict)
            .count()
    }

    pub fn ignored_count(&self) -> usize {
        self.files
            .iter()
            .filter(|file| file.kind == GitFileKind::Ignored)
            .count()
    }

    pub fn branch_label(&self) -> String {
        let head = self.branch.head.as_deref().unwrap_or("(unknown)");
        match self.branch.upstream.as_deref() {
            Some(upstream) => format!(
                "{head} -> {upstream} (+{}/-{})",
                self.branch.ahead, self.branch.behind
            ),
            None => head.to_string(),
        }
    }

    pub fn ready_to_ship(&self) -> bool {
        if self.conflict_count() > 0 {
            return false;
        }
        if let Some(ref tests) = self.test_status {
            if tests.failed > 0 || tests.exit_code != 0 {
                return false;
            }
        }
        true
    }

    pub fn to_agent_json(&self) -> AgentShipStatus {
        AgentShipStatus {
            branch: self.branch_label(),
            staged_files: self.staged_count(),
            unstaged_files: self.unstaged_count(),
            untracked_files: self.untracked_count(),
            conflicts: self.conflict_count(),
            staged_diff_stat: self.staged_diff_stat.clone(),
            unstaged_diff_stat: self.unstaged_diff_stat.clone(),
            test_status: self.test_status.as_ref().map(|t| AgentTestStatusSummary {
                framework: t.framework.clone(),
                total: t.total,
                passed: t.passed,
                failed: t.failed,
                skipped: t.skipped,
                exit_code: t.exit_code,
                error: t.error.clone(),
            }),
            ready_to_ship: self.ready_to_ship(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentShipStatus {
    pub branch: String,
    pub staged_files: usize,
    pub unstaged_files: usize,
    pub untracked_files: usize,
    pub conflicts: usize,
    pub staged_diff_stat: String,
    pub unstaged_diff_stat: String,
    pub test_status: Option<AgentTestStatusSummary>,
    pub ready_to_ship: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTestStatusSummary {
    pub framework: String,
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub exit_code: i32,
    pub error: Option<String>,
}

pub fn ship_status_one_liner(snapshot: &ShipcheckSnapshot, tests_ran_this_session: bool) -> String {
    let tests_note = if tests_ran_this_session {
        "tests ran this session"
    } else {
        "tests not run this session"
    };
    format!(
        "Ship status: branch {}, {} unstaged file(s), {} staged, {tests_note}",
        snapshot.branch_label(),
        snapshot.unstaged_count(),
        snapshot.staged_count(),
    )
}

pub fn append_ship_context(
    base: &str,
    config: &AgentConfig,
    tests_ran_this_session: bool,
) -> String {
    if config.mode != OperatorMode::Ship {
        return base.to_string();
    }
    let Ok(snapshot) = collect_shipcheck(&config.workspace_root) else {
        return base.to_string();
    };
    let line = ship_status_one_liner(&snapshot, tests_ran_this_session);
    format!("{base}\n\n{}", cap_ship_status_line(&line, 512))
}

/// Cap a ship status line to `max_bytes`, rolling back to a char boundary so
/// multi-byte branch names cannot panic on a mid-character slice.
fn cap_ship_status_line(line: &str, max_bytes: usize) -> String {
    if line.len() <= max_bytes {
        return line.to_string();
    }
    let ellipsis = "…";
    let mut end = max_bytes.saturating_sub(ellipsis.len()).min(line.len());
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ellipsis}", &line[..end])
}

pub fn collect_shipcheck(workspace_root: &str) -> Result<ShipcheckSnapshot> {
    collect_shipcheck_with_tests(workspace_root, false)
}

pub fn collect_shipcheck_with_tests(
    workspace_root: &str,
    run_tests_flag: bool,
) -> Result<ShipcheckSnapshot> {
    let git_root = run_git(workspace_root, &["rev-parse", "--show-toplevel"])?
        .trim()
        .to_string();
    let status = run_git(workspace_root, &["status", "--porcelain=v2", "--branch"])?;
    let (branch, files) = parse_status_porcelain_v2(&status)?;
    let staged_diff_stat = run_git(workspace_root, &["diff", "--cached", "--stat", "--"])?;
    let unstaged_diff_stat = run_git(workspace_root, &["diff", "--stat", "--"])?;

    let test_status = if run_tests_flag {
        Some(
            run_shipcheck_tests(workspace_root).unwrap_or_else(|e| TestStatus {
                framework: "unknown".to_string(),
                total: 0,
                passed: 0,
                failed: 0,
                skipped: 0,
                exit_code: 1,
                error: Some(e.to_string()),
            }),
        )
    } else {
        None
    };

    Ok(ShipcheckSnapshot {
        workspace_root: workspace_root.to_string(),
        git_root,
        branch,
        files,
        staged_diff_stat: staged_diff_stat.trim().to_string(),
        unstaged_diff_stat: unstaged_diff_stat.trim().to_string(),
        test_status,
    })
}

fn run_shipcheck_tests(workspace_root: &str) -> Result<TestStatus> {
    let discovery = discover_tests(workspace_root)?;
    if discovery.framework == "unknown" {
        return Ok(TestStatus {
            framework: "none".to_string(),
            total: 0,
            passed: 0,
            failed: 0,
            skipped: 0,
            exit_code: 0,
            error: None,
        });
    }

    let test_result = run_tests(workspace_root, None)?;
    Ok(TestStatus {
        framework: discovery.framework,
        total: test_result.total,
        passed: test_result.passed,
        failed: test_result.failed,
        skipped: test_result.skipped,
        exit_code: test_result.exit_code,
        error: None,
    })
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

fn parse_status_porcelain_v2(input: &str) -> Result<(GitBranchState, Vec<GitFileState>)> {
    let mut branch = GitBranchState::default();
    let mut files = Vec::new();

    for line in input.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("# ") {
            parse_branch_line(rest, &mut branch);
            continue;
        }
        if let Some(file) = parse_file_line(line)? {
            files.push(file);
        }
    }

    Ok((branch, files))
}

fn parse_branch_line(line: &str, branch: &mut GitBranchState) {
    if let Some(value) = line.strip_prefix("branch.oid ") {
        if value != "(initial)" {
            branch.oid = Some(value.to_string());
        }
    } else if let Some(value) = line.strip_prefix("branch.head ") {
        branch.head = Some(value.to_string());
    } else if let Some(value) = line.strip_prefix("branch.upstream ") {
        branch.upstream = Some(value.to_string());
    } else if let Some(value) = line.strip_prefix("branch.ab ") {
        for part in value.split_whitespace() {
            if let Some(n) = part.strip_prefix('+').and_then(|s| s.parse::<i32>().ok()) {
                branch.ahead = n;
            } else if let Some(n) = part.strip_prefix('-').and_then(|s| s.parse::<i32>().ok()) {
                branch.behind = n;
            }
        }
    }
}

fn parse_file_line(line: &str) -> Result<Option<GitFileState>> {
    if let Some(path) = line.strip_prefix("? ") {
        return Ok(Some(GitFileState {
            path: path.to_string(),
            original_path: None,
            staged: None,
            unstaged: None,
            kind: GitFileKind::Untracked,
        }));
    }
    if let Some(path) = line.strip_prefix("! ") {
        return Ok(Some(GitFileState {
            path: path.to_string(),
            original_path: None,
            staged: None,
            unstaged: None,
            kind: GitFileKind::Ignored,
        }));
    }
    if line.starts_with("1 ") {
        let parts: Vec<&str> = line.splitn(9, ' ').collect();
        if parts.len() != 9 {
            return Err(anyhow!("malformed ordinary git status line: {line}"));
        }
        let (staged, unstaged) = parse_xy(parts[1]);
        return Ok(Some(GitFileState {
            path: parts[8].to_string(),
            original_path: None,
            staged,
            unstaged,
            kind: GitFileKind::Tracked,
        }));
    }
    if line.starts_with("2 ") {
        let parts: Vec<&str> = line.splitn(10, ' ').collect();
        if parts.len() != 10 {
            return Err(anyhow!("malformed rename/copy git status line: {line}"));
        }
        let (path, original_path) = parts[9]
            .split_once('\t')
            .map(|(path, original)| (path.to_string(), Some(original.to_string())))
            .unwrap_or_else(|| (parts[9].to_string(), None));
        let (staged, unstaged) = parse_xy(parts[1]);
        return Ok(Some(GitFileState {
            path,
            original_path,
            staged,
            unstaged,
            kind: GitFileKind::Renamed,
        }));
    }
    if line.starts_with("u ") {
        let parts: Vec<&str> = line.splitn(11, ' ').collect();
        if parts.len() != 11 {
            return Err(anyhow!("malformed unmerged git status line: {line}"));
        }
        let (staged, unstaged) = parse_xy(parts[1]);
        return Ok(Some(GitFileState {
            path: parts[10].to_string(),
            original_path: None,
            staged,
            unstaged,
            kind: GitFileKind::Conflict,
        }));
    }
    Ok(None)
}

fn parse_xy(xy: &str) -> (Option<char>, Option<char>) {
    let mut chars = xy.chars();
    (chars.next(), chars.next())
}

fn status_changed(status: Option<char>) -> bool {
    matches!(status, Some(c) if c != '.' && c != ' ')
}

pub fn render_markdown(
    snapshot: &ShipcheckSnapshot,
    freshness: Option<&ProjectIndexFreshness>,
) -> String {
    let mut out = String::new();
    out.push_str("# Albatross Shipcheck\n\n");
    out.push_str(&format!("Generated: {}\n\n", Utc::now().to_rfc3339()));
    out.push_str("## Git\n\n");
    out.push_str(&format!("- Workspace: `{}`\n", snapshot.workspace_root));
    out.push_str(&format!("- Git root: `{}`\n", snapshot.git_root));
    out.push_str(&format!("- Branch: `{}`\n", snapshot.branch_label()));
    out.push_str(&format!(
        "- Status: {}\n\n",
        if snapshot.is_clean() {
            "clean"
        } else {
            "dirty"
        }
    ));
    out.push_str("## Working Tree\n\n");
    out.push_str(&format!("- Staged files: {}\n", snapshot.staged_count()));
    out.push_str(&format!(
        "- Unstaged files: {}\n",
        snapshot.unstaged_count()
    ));
    out.push_str(&format!(
        "- Untracked files: {}\n",
        snapshot.untracked_count()
    ));
    out.push_str(&format!("- Conflicts: {}\n", snapshot.conflict_count()));
    if snapshot.ignored_count() > 0 {
        out.push_str(&format!("- Ignored files: {}\n", snapshot.ignored_count()));
    }
    out.push('\n');

    if !snapshot.files.is_empty() {
        out.push_str("## Files\n\n");
        for file in &snapshot.files {
            out.push_str(&format!("- `{}`", file.path));
            if let Some(original) = &file.original_path {
                out.push_str(&format!(" from `{original}`"));
            }
            out.push_str(&format!(" ({})\n", file_status_label(file)));
        }
        out.push('\n');
    }

    push_diff_stat(&mut out, "Staged Diff", &snapshot.staged_diff_stat);
    push_diff_stat(&mut out, "Unstaged Diff", &snapshot.unstaged_diff_stat);

    out.push_str("## Tests\n\n");
    match &snapshot.test_status {
        Some(status) => {
            out.push_str(&format!("- Framework: `{}`\n", status.framework));
            out.push_str(&format!("- Total: {}\n", status.total));
            out.push_str(&format!("- Passed: {}\n", status.passed));
            out.push_str(&format!("- Failed: {}\n", status.failed));
            out.push_str(&format!("- Skipped: {}\n", status.skipped));
            out.push_str(&format!("- Exit code: {}\n", status.exit_code));
            if let Some(error) = &status.error {
                out.push_str(&format!("- Error: `{error}`\n"));
            }
            if status.failed > 0 || status.exit_code != 0 {
                out.push_str("- Status: **FAILED**\n");
            } else {
                out.push_str("- Status: **PASSED**\n");
            }
        }
        None => out.push_str("- Tests not run (use `/shipcheck --tests` to run tests)\n"),
    }
    out.push('\n');

    out.push_str("## Project Memory\n\n");
    match freshness {
        Some(report) if report.indexed_files > 0 || report.workspace_files > 0 => {
            out.push_str(&format!(
                "- Indexed files: {}\n- Workspace files: {}\n- Fresh: {}\n- Stale: {}\n- Missing: {}\n- Deleted: {}\n- Read errors: {}\n",
                report.indexed_files,
                report.workspace_files,
                report.fresh,
                report.stale,
                report.missing,
                report.deleted,
                report.read_errors
            ));
        }
        Some(_) => out.push_str("- No project-memory index found.\n"),
        None => out.push_str("- Project memory disabled.\n"),
    }

    out
}

fn push_diff_stat(out: &mut String, title: &str, stat: &str) {
    out.push_str(&format!("## {title}\n\n"));
    if stat.trim().is_empty() {
        out.push_str("No changes.\n\n");
    } else {
        out.push_str("```text\n");
        out.push_str(stat);
        out.push_str("\n```\n\n");
    }
}

pub fn file_status_label(file: &GitFileState) -> String {
    match file.kind {
        GitFileKind::Untracked => "untracked".to_string(),
        GitFileKind::Ignored => "ignored".to_string(),
        GitFileKind::Conflict => "conflict".to_string(),
        GitFileKind::Tracked | GitFileKind::Renamed => {
            let mut parts = Vec::new();
            if status_changed(file.staged) {
                parts.push(format!("staged {}", file.staged.unwrap_or('?')));
            }
            if status_changed(file.unstaged) {
                parts.push(format!("unstaged {}", file.unstaged.unwrap_or('?')));
            }
            if parts.is_empty() {
                "tracked".to_string()
            } else {
                parts.join(", ")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShipReadinessStatus {
    Ready,
    NeedsReview,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShipReadiness {
    pub status: ShipReadinessStatus,
    pub blockers: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn evaluate_ship_pr_readiness(
    snapshot: &ShipcheckSnapshot,
    allow_behind: bool,
) -> ShipReadiness {
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();

    if snapshot.branch.head.as_deref() == Some("(detached)") || snapshot.branch.head.is_none() {
        blockers.push("cannot create a PR from a detached or unknown HEAD".into());
    }
    if snapshot.branch.upstream.is_none() {
        blockers.push("branch has no upstream; run /ship push first".into());
    }
    if snapshot.branch.ahead > 0 {
        blockers.push(format!(
            "branch has {} unpushed commit(s); run /ship push first",
            snapshot.branch.ahead
        ));
    }
    if snapshot.branch.behind > 0 && !allow_behind {
        blockers.push(format!(
            "branch is behind upstream by {} commit(s); pull/rebase or pass --allow-behind",
            snapshot.branch.behind
        ));
    }
    if snapshot.conflict_count() > 0 {
        blockers.push(format!(
            "{} conflicted file(s) must be resolved before opening a PR",
            snapshot.conflict_count()
        ));
    }
    if snapshot.staged_count() + snapshot.unstaged_count() > 0 || snapshot.untracked_count() > 0 {
        warnings.push(format!(
            "{} uncommitted file(s) are not part of this PR",
            snapshot.staged_count() + snapshot.unstaged_count() + snapshot.untracked_count()
        ));
    }
    if snapshot.test_status.is_none() {
        warnings.push("tests were not run for this PR; use /ship pr --tests if desired".into());
    }
    if let Some(tests) = &snapshot.test_status {
        if tests.failed > 0 || tests.exit_code != 0 {
            blockers.push(format!(
                "tests failed: {} failed, exit code {}",
                tests.failed, tests.exit_code
            ));
        }
        if let Some(error) = &tests.error {
            blockers.push(format!("test execution error: {error}"));
        }
    }

    let status = if !blockers.is_empty() {
        ShipReadinessStatus::Blocked
    } else if !warnings.is_empty() {
        ShipReadinessStatus::NeedsReview
    } else {
        ShipReadinessStatus::Ready
    };

    ShipReadiness {
        status,
        blockers,
        warnings,
    }
}

pub fn quality_input_from_shipcheck(
    snapshot: &ShipcheckSnapshot,
    allow_behind: bool,
    opened_by_gh: bool,
    has_pr_url: bool,
) -> crate::scorecard::PrQualityInput {
    let readiness = evaluate_ship_pr_readiness(snapshot, allow_behind);
    let score_readiness = match readiness.status {
        ShipReadinessStatus::Ready => crate::scorecard::PrQualityReadiness::Ready,
        ShipReadinessStatus::NeedsReview => crate::scorecard::PrQualityReadiness::NeedsReview,
        ShipReadinessStatus::Blocked => crate::scorecard::PrQualityReadiness::Blocked,
    };
    let tests = match &snapshot.test_status {
        Some(tests) if tests.failed == 0 && tests.exit_code == 0 && tests.error.is_none() => {
            crate::scorecard::PrQualityTestStatus::Passed
        }
        Some(_) => crate::scorecard::PrQualityTestStatus::Failed,
        None => crate::scorecard::PrQualityTestStatus::NotRun,
    };

    crate::scorecard::PrQualityInput {
        readiness: score_readiness,
        blockers: readiness.blockers,
        warnings: readiness.warnings,
        tests,
        opened_by_gh,
        has_pr_url,
    }
}

pub fn default_export_path(session_dir: &str) -> std::path::PathBuf {
    Path::new(session_dir).join("shipcheck").join(format!(
        "{}.md",
        Utc::now().format("%Y-%m-%dT%H-%M-%S-%3fZ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    #[test]
    fn cap_ship_status_line_leaves_short_ascii() {
        assert_eq!(cap_ship_status_line("short", 512), "short");
    }

    #[test]
    fn cap_ship_status_line_does_not_panic_on_emoji_over_cap() {
        // 🚀 is 4 bytes; a hard slice at byte 509 used to panic mid-character.
        let line = format!(
            "Ship status: branch {}, 0 unstaged file(s), 0 staged, tests not run this session",
            "🚀".repeat(130)
        );
        assert!(line.len() > 512);
        assert!(!line.is_char_boundary(509));
        let capped = cap_ship_status_line(&line, 512);
        assert!(capped.len() <= 512);
        assert!(capped.ends_with('…'));
        assert!(capped.is_char_boundary(capped.len() - '…'.len_utf8()));
    }

    #[test]
    fn ship_status_one_liner_with_emoji_branch_caps_safely() {
        let snapshot = ShipcheckSnapshot {
            workspace_root: ".".into(),
            git_root: ".".into(),
            branch: GitBranchState {
                oid: None,
                head: Some("🚀".repeat(120)),
                upstream: None,
                ahead: 0,
                behind: 0,
            },
            files: Vec::new(),
            staged_diff_stat: String::new(),
            unstaged_diff_stat: String::new(),
            test_status: None,
        };
        let line = ship_status_one_liner(&snapshot, false);
        assert!(line.len() > 512);
        let capped = cap_ship_status_line(&line, 512);
        assert!(capped.len() <= 512);
        assert!(capped.contains("Ship status: branch"));
    }

    #[test]
    fn parses_clean_branch_status() {
        let input = "\
# branch.oid abc123
# branch.head main
# branch.upstream origin/main
# branch.ab +0 -0
";
        let (branch, files) = parse_status_porcelain_v2(input).unwrap();

        assert_eq!(branch.oid.as_deref(), Some("abc123"));
        assert_eq!(branch.head.as_deref(), Some("main"));
        assert_eq!(branch.upstream.as_deref(), Some("origin/main"));
        assert_eq!(branch.ahead, 0);
        assert_eq!(branch.behind, 0);
        assert!(files.is_empty());
    }

    #[test]
    fn parses_dirty_counts_and_renames() {
        let input = "\
# branch.oid abc123
# branch.head feature
# branch.upstream origin/feature
# branch.ab +2 -1
1 M. N... 100644 100644 100644 abc def src/staged.rs
1 .M N... 100644 100644 100644 abc def README copy.md
1 MM N... 100644 100644 100644 abc def src/both.rs
2 R. N... 100644 100644 100644 abc def R100 src/new.rs\tsrc/old.rs
u UU N... 100644 100644 100644 100644 a b c src/conflict.rs
? notes/today.md
";
        let (branch, files) = parse_status_porcelain_v2(input).unwrap();
        let snapshot = ShipcheckSnapshot {
            workspace_root: ".".to_string(),
            git_root: ".".to_string(),
            branch,
            files,
            staged_diff_stat: String::new(),
            unstaged_diff_stat: String::new(),
            test_status: None,
        };

        assert_eq!(snapshot.branch.ahead, 2);
        assert_eq!(snapshot.branch.behind, 1);
        assert_eq!(snapshot.staged_count(), 4);
        assert_eq!(snapshot.unstaged_count(), 3);
        assert_eq!(snapshot.untracked_count(), 1);
        assert_eq!(snapshot.conflict_count(), 1);
        let renamed = snapshot
            .files
            .iter()
            .find(|file| file.path == "src/new.rs")
            .unwrap();
        assert_eq!(renamed.original_path.as_deref(), Some("src/old.rs"));
    }

    #[test]
    fn renders_markdown_report() {
        let snapshot = ShipcheckSnapshot {
            workspace_root: "/repo".to_string(),
            git_root: "/repo".to_string(),
            branch: GitBranchState {
                head: Some("main".to_string()),
                upstream: Some("origin/main".to_string()),
                ahead: 1,
                behind: 0,
                oid: Some("abc123".to_string()),
            },
            files: vec![GitFileState {
                path: "src/main.rs".to_string(),
                original_path: None,
                staged: Some('M'),
                unstaged: Some('.'),
                kind: GitFileKind::Tracked,
            }],
            staged_diff_stat: " src/main.rs | 1 +".to_string(),
            unstaged_diff_stat: String::new(),
            test_status: None,
        };
        let freshness = ProjectIndexFreshness {
            indexed_files: 1,
            workspace_files: 1,
            fresh: 1,
            ..Default::default()
        };
        let md = render_markdown(&snapshot, Some(&freshness));

        assert!(md.contains("# Albatross Shipcheck"));
        assert!(md.contains("main -> origin/main (+1/-0)"));
        assert!(md.contains("- Staged files: 1"));
        assert!(md.contains("src/main.rs | 1 +"));
        assert!(md.contains("- Fresh: 1"));
    }

    #[test]
    fn ready_to_ship_respects_conflicts_and_tests() {
        let mut snapshot = ShipcheckSnapshot {
            workspace_root: ".".to_string(),
            git_root: ".".to_string(),
            branch: GitBranchState::default(),
            files: vec![GitFileState {
                path: "src/conflict.rs".to_string(),
                original_path: None,
                staged: None,
                unstaged: None,
                kind: GitFileKind::Conflict,
            }],
            staged_diff_stat: String::new(),
            unstaged_diff_stat: String::new(),
            test_status: None,
        };
        assert!(!snapshot.ready_to_ship());
        snapshot.files.clear();
        snapshot.test_status = Some(TestStatus {
            framework: "cargo".into(),
            total: 1,
            passed: 0,
            failed: 1,
            skipped: 0,
            exit_code: 1,
            error: None,
        });
        assert!(!snapshot.ready_to_ship());
        snapshot.test_status = Some(TestStatus {
            framework: "cargo".into(),
            total: 1,
            passed: 1,
            failed: 0,
            skipped: 0,
            exit_code: 0,
            error: None,
        });
        assert!(snapshot.ready_to_ship());
    }

    #[test]
    fn to_agent_json_serializes_summary() {
        let snapshot = ShipcheckSnapshot {
            workspace_root: "/repo".to_string(),
            git_root: "/repo".to_string(),
            branch: GitBranchState {
                head: Some("main".to_string()),
                upstream: Some("origin/main".to_string()),
                ahead: 1,
                behind: 0,
                oid: Some("abc".to_string()),
            },
            files: vec![],
            staged_diff_stat: " src/main.rs | 1 +".to_string(),
            unstaged_diff_stat: String::new(),
            test_status: None,
        };
        let json = snapshot.to_agent_json();
        assert_eq!(json.branch, "main -> origin/main (+1/-0)");
        assert!(json.ready_to_ship);
        assert_eq!(json.staged_files, 0);
    }

    #[test]
    fn quality_input_from_shipcheck_penalizes_missing_gh() {
        let snapshot = ShipcheckSnapshot {
            workspace_root: "/repo".to_string(),
            git_root: "/repo".to_string(),
            branch: GitBranchState {
                head: Some("feature".to_string()),
                upstream: Some("origin/feature".to_string()),
                ahead: 0,
                behind: 0,
                oid: Some("abc".to_string()),
            },
            files: vec![],
            staged_diff_stat: String::new(),
            unstaged_diff_stat: String::new(),
            test_status: Some(TestStatus {
                framework: "cargo".to_string(),
                total: 1,
                passed: 1,
                failed: 0,
                skipped: 0,
                exit_code: 0,
                error: None,
            }),
        };
        let quality = quality_input_from_shipcheck(&snapshot, false, false, true);
        assert_eq!(quality.tests, crate::scorecard::PrQualityTestStatus::Passed);
        assert!(!quality.opened_by_gh);
        assert!(quality.has_pr_url);
        assert_eq!(
            quality.readiness,
            crate::scorecard::PrQualityReadiness::Ready
        );
    }

    #[test]
    fn collects_real_git_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let init = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .arg("init")
            .output()
            .unwrap();
        assert!(init.status.success());
        fs::write(dir.path().join("notes.md"), "ship it\n").unwrap();

        let snapshot = collect_shipcheck(dir.path().to_str().unwrap()).unwrap();

        assert_eq!(
            std::path::PathBuf::from(&snapshot.git_root),
            fs::canonicalize(dir.path()).unwrap()
        );
        assert_eq!(snapshot.untracked_count(), 1);
        assert!(!snapshot.is_clean());
    }
}
