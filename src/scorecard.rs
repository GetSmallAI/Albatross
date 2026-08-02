use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const RESET: crate::theme::Style = crate::theme::RESET;
const DIM: crate::theme::Style = crate::theme::MUTED;
const GREEN: crate::theme::Style = crate::theme::SUCCESS;
const YELLOW: crate::theme::Style = crate::theme::WARN;
const CYAN: crate::theme::Style = crate::theme::ACCENT_DEEP;
const BRIGHT_GREEN: crate::theme::Style = crate::theme::SUCCESS;
const STORE_DIR_ENV: &str = "ALBATROSS_SCORECARD_DIR";
#[cfg(test)]
const DEFAULT_QUALITY_PR_THRESHOLD: u8 = 80;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkUnit {
    pub repo: String,
    pub branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ScorecardEvent {
    Turn(ScorecardTurn),
    Pr(ScorecardPr),
    Verification(ScorecardVerification),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScorecardTurn {
    pub timestamp: String,
    pub repo: String,
    pub branch: String,
    pub session_id: String,
    pub session_path: String,
    pub backend: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ScorecardPr {
    pub timestamp: String,
    pub repo: String,
    pub branch: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub status: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub turn_count: usize,
    pub session_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ship_record_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<ScorecardQuality>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_traces: Vec<crate::turn_trace::SessionTraceSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<ScorecardVerification>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ScorecardVerification {
    pub timestamp: String,
    pub pr_timestamp: String,
    pub source: String,
    pub url: String,
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub state: String,
    pub merged: bool,
    pub draft: bool,
    pub head_branch: String,
    pub base_branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mergeable: Option<String>,
    pub checks: ScorecardRemoteChecks,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub check_runs: Vec<ScorecardRemoteCheck>,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ScorecardRemoteChecks {
    pub total: usize,
    pub success: usize,
    pub failing: usize,
    pub pending: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ScorecardRemoteCheck {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ScorecardQuality {
    pub score: u8,
    pub grade: String,
    pub counts: bool,
    pub status: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrQualityReadiness {
    Ready,
    NeedsReview,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrQualityTestStatus {
    Passed,
    Failed,
    NotRun,
}

#[derive(Debug, Clone)]
pub struct PrQualityInput {
    pub readiness: PrQualityReadiness,
    pub blockers: Vec<String>,
    pub warnings: Vec<String>,
    pub tests: PrQualityTestStatus,
    pub opened_by_gh: bool,
    pub has_pr_url: bool,
}

pub struct TurnRecordInput<'a> {
    pub workspace_root: &'a str,
    pub session_path: &'a Path,
    pub backend: &'a str,
    pub model: &'a str,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub enabled: bool,
}

pub struct PrCloseInput<'a> {
    pub workspace_root: &'a str,
    pub session_dir: &'a str,
    pub title: &'a str,
    pub url: Option<&'a str>,
    pub status: &'a str,
    pub quality: Option<PrQualityInput>,
    pub ship_record_path: Option<&'a str>,
    pub quality_threshold: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrCloseSummary {
    pub path: PathBuf,
    pub pr: ScorecardPr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenUnitSummary {
    pub repo: String,
    pub branch: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub turn_count: usize,
    pub session_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyScore {
    pub date: NaiveDate,
    pub pr_count: usize,
    pub scored_count: usize,
    pub quality_count: usize,
    pub total_quality_score: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct ScorecardReport {
    pub path: Option<PathBuf>,
    pub lifetime_tokens: u64,
    pub turn_count: usize,
    pub closed_prs: Vec<ScorecardPr>,
    pub current_open: OpenUnitSummary,
    pub daily_scores: BTreeMap<NaiveDate, DailyScore>,
    pub current_streak: usize,
    pub longest_streak: usize,
    pub today: NaiveDate,
    pub diagnostics: Option<ScorecardStoreDiagnostics>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedScorecardLine {
    pub line_number: usize,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScorecardStoreDiagnostics {
    pub path: PathBuf,
    pub exists: bool,
    pub bytes: u64,
    pub total_lines: usize,
    pub blank_lines: usize,
    pub event_count: usize,
    pub turn_count: usize,
    pub pr_count: usize,
    pub verification_count: usize,
    pub malformed_count: usize,
    pub malformed_lines: Vec<MalformedScorecardLine>,
}

#[derive(Debug, Clone)]
struct ScorecardRead {
    events: Vec<ScorecardEvent>,
    diagnostics: ScorecardStoreDiagnostics,
}

#[derive(Debug, Clone)]
pub struct ScorecardTurnRead {
    pub path: Option<PathBuf>,
    pub turns: Vec<ScorecardTurn>,
    pub diagnostics: Option<ScorecardStoreDiagnostics>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScorecardResetSummary {
    pub path: PathBuf,
    pub backup_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScorecardVerifySummary {
    pub path: PathBuf,
    pub index: usize,
    pub verification: ScorecardVerification,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScorecardVerifySkip {
    pub index: usize,
    pub title: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScorecardVerifyError {
    pub index: usize,
    pub title: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScorecardVerifyAllSummary {
    pub path: PathBuf,
    pub verified: Vec<ScorecardVerifySummary>,
    pub skipped: Vec<ScorecardVerifySkip>,
    pub failed: Vec<ScorecardVerifyError>,
}

pub fn scorecard_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(STORE_DIR_ENV) {
        return Some(PathBuf::from(dir).join("events.jsonl"));
    }
    let base = if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local").join("share")
    } else {
        return None;
    };
    Some(
        base.join("albatross")
            .join("scorecard")
            .join("events.jsonl"),
    )
}

pub fn record_turn(input: TurnRecordInput<'_>) -> Result<Option<PathBuf>> {
    if !input.enabled {
        return Ok(None);
    }
    if input.input_tokens == 0 && input.output_tokens == 0 {
        return Ok(None);
    }
    let Some(path) = scorecard_path() else {
        return Ok(None);
    };
    let unit = current_work_unit(input.workspace_root);
    let session_id = input
        .session_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("session")
        .to_string();
    let session_path = session_path_for_storage(input.workspace_root, input.session_path);
    let event = ScorecardEvent::Turn(ScorecardTurn {
        timestamp: Utc::now().to_rfc3339(),
        repo: unit.repo,
        branch: unit.branch,
        session_id,
        session_path,
        backend: input.backend.to_string(),
        model: input.model.to_string(),
        input_tokens: input.input_tokens as u64,
        output_tokens: input.output_tokens as u64,
        total_tokens: input.input_tokens as u64 + input.output_tokens as u64,
    });
    append_event(&path, &event)?;
    Ok(Some(path))
}

pub fn close_pr_for_workspace(input: PrCloseInput<'_>) -> Result<Option<PrCloseSummary>> {
    let Some(path) = scorecard_path() else {
        return Ok(None);
    };
    close_pr_at_path(&path, input, Utc::now())
}

pub fn load_report(workspace_root: &str) -> Result<ScorecardReport> {
    let path = scorecard_path();
    let (events, diagnostics) = match &path {
        Some(path) => {
            let read = read_events_with_diagnostics(path)?;
            (read.events, Some(read.diagnostics))
        }
        None => (Vec::new(), None),
    };
    Ok(build_report(
        path,
        &events,
        current_work_unit(workspace_root),
        Utc::now().date_naive(),
        diagnostics,
    ))
}

pub fn load_turn_records() -> Result<ScorecardTurnRead> {
    let path = scorecard_path();
    let (events, diagnostics) = match &path {
        Some(path) => {
            let read = read_events_with_diagnostics(path)?;
            (read.events, Some(read.diagnostics))
        }
        None => (Vec::new(), None),
    };
    let turns = events
        .into_iter()
        .filter_map(|event| match event {
            ScorecardEvent::Turn(turn) => Some(turn),
            _ => None,
        })
        .collect();
    Ok(ScorecardTurnRead {
        path,
        turns,
        diagnostics,
    })
}

pub fn recent_prs(limit: usize) -> Result<Vec<ScorecardPr>> {
    let Some(path) = scorecard_path() else {
        return Ok(Vec::new());
    };
    let mut prs = closed_prs_with_verifications(&read_events(&path)?);
    prs.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    prs.truncate(limit);
    Ok(prs)
}

pub fn recent_pr_by_index(index: usize) -> Result<Option<ScorecardPr>> {
    if index == 0 {
        return Ok(None);
    }
    let prs = recent_prs(index)?;
    Ok(prs.get(index - 1).cloned())
}

pub fn scorecard_diagnostics() -> Result<Option<ScorecardStoreDiagnostics>> {
    let Some(path) = scorecard_path() else {
        return Ok(None);
    };
    Ok(Some(read_events_with_diagnostics(&path)?.diagnostics))
}

pub fn export_store(target: Option<&Path>) -> Result<Option<PathBuf>> {
    let Some(path) = scorecard_path() else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    let out_path = target
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_export_path_for(&path));
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    fs::copy(&path, &out_path)
        .with_context(|| format!("copying {} to {}", path.display(), out_path.display()))?;
    Ok(Some(out_path))
}

pub fn reset_store() -> Result<Option<ScorecardResetSummary>> {
    let Some(path) = scorecard_path() else {
        return Ok(None);
    };
    Ok(Some(reset_store_at_path(&path)?))
}

pub fn verify_recent_pr(
    index: usize,
    workspace_root: &str,
) -> Result<Option<ScorecardVerifySummary>> {
    let Some(path) = scorecard_path() else {
        return Ok(None);
    };
    verify_recent_pr_at_path(&path, index, |pr| {
        fetch_github_pr_verification(workspace_root, pr, Utc::now())
    })
}

pub fn verify_all_prs(workspace_root: &str) -> Result<Option<ScorecardVerifyAllSummary>> {
    let Some(path) = scorecard_path() else {
        return Ok(None);
    };
    Ok(Some(verify_all_prs_at_path(&path, |pr| {
        fetch_github_pr_verification(workspace_root, pr, Utc::now())
    })?))
}

pub fn render_report(report: &ScorecardReport) -> String {
    let mut out = String::new();
    let total_prs = report.closed_prs.len();
    let quality_prs = quality_pr_count(&report.closed_prs);
    let scored_prs = scored_pr_count(&report.closed_prs);
    let needs_followup = scored_prs.saturating_sub(quality_prs);
    let clean_ships = clean_ship_count(&report.closed_prs);
    let avg_quality = average_quality_score(&report.closed_prs);
    let tokens_per_quality_pr = average_tokens_per_quality_pr(&report.closed_prs);
    let top_quality = top_quality_pr(&report.closed_prs);
    let (remote_ok, remote_checked) = remote_verified_count(&report.closed_prs);

    out.push_str(&format!(
        "  {DIM}scorecard{RESET}       global quality PRs shipped\n"
    ));
    match &report.path {
        Some(path) => out.push_str(&format!(
            "  {DIM}store{RESET}           {}\n",
            path.display()
        )),
        None => out.push_str(&format!("  {DIM}store{RESET}           unavailable\n")),
    }
    if let Some(diagnostics) = report.diagnostics.as_ref() {
        if diagnostics.malformed_count > 0 {
            out.push_str(&format!(
                "  {YELLOW}!{RESET} {DIM}store warning{RESET}  skipped {} malformed line(s); run /scorecard doctor\n",
                diagnostics.malformed_count
            ));
        }
    }
    out.push('\n');
    out.push_str(&format!(
        "  {CYAN}{:<13}{RESET} {:>10}   {CYAN}{:<13}{RESET} {:>10}   {CYAN}{:<13}{RESET} {:>10}\n",
        "Quality PRs",
        format!("{quality_prs}/{total_prs}"),
        "Quality rate",
        format_rate(quality_prs, total_prs),
        "Avg quality",
        avg_quality
            .map(format_quality_score)
            .unwrap_or_else(|| "n/a".to_string())
    ));
    out.push_str(&format!(
        "  {CYAN}{:<13}{RESET} {:>10}   {CYAN}{:<13}{RESET} {:>10}   {CYAN}{:<13}{RESET} {:>10}\n",
        "Clean ships",
        clean_ships,
        "Needs follow",
        needs_followup,
        "Tokens / QPR",
        tokens_per_quality_pr
            .map(format_tokens)
            .unwrap_or_else(|| "n/a".to_string())
    ));
    out.push_str(&format!(
        "  {CYAN}{:<13}{RESET} {:>10}   {CYAN}{:<13}{RESET} {:>10}   {CYAN}{:<13}{RESET} {:>10}\n",
        "Tracked",
        format_tokens(report.lifetime_tokens),
        "Streak",
        format!("{}d / {}d", report.current_streak, report.longest_streak),
        "Scored PRs",
        scored_prs
    ));
    out.push_str(&format!(
        "  {CYAN}{:<13}{RESET} {:>10}   {CYAN}{:<13}{RESET} {:>10}   {CYAN}{:<13}{RESET} {:>10}\n",
        "Open branch",
        format_tokens(report.current_open.total_tokens),
        "Open turns",
        report.current_open.turn_count,
        "All turns",
        report.turn_count
    ));
    out.push_str(&format!(
        "  {CYAN}{:<13}{RESET} {:>10}   {CYAN}{:<13}{RESET} {:>10}   {CYAN}{:<13}{RESET} {:>10}\n",
        "Remote OK",
        if remote_checked == 0 {
            "n/a".to_string()
        } else {
            format!("{remote_ok}/{remote_checked}")
        },
        "Remote rate",
        format_rate(remote_ok, remote_checked),
        "Verified",
        remote_checked
    ));
    out.push('\n');
    out.push_str(&format!(
        "  {DIM}current{RESET}         {} · {}\n",
        repo_label(&report.current_open.repo),
        report.current_open.branch
    ));
    if let Some((pr, quality)) = top_quality {
        out.push_str(&format!(
            "  {DIM}top quality{RESET}     {} {} · {} · {}\n",
            quality.grade,
            format_quality_score(quality.score),
            pr_date_label(&pr.timestamp),
            one_line(&pr.title, 64)
        ));
    }
    out.push('\n');
    out.push_str(&render_daily_grid(report));
    if total_prs == 0 {
        out.push_str(&format!(
            "\n  {DIM}No closed PR units yet. Run /scorecard close <label> or /ship pr after doing tracked work.{RESET}\n"
        ));
    } else {
        out.push_str(&format!(
            "\n  {DIM}Daily grid shows quality PRs shipped. Tokens are context, not the score.{RESET}\n"
        ));
        out.push_str(&format!("  {DIM}recent detail → /scorecard pr 1{RESET}\n"));
        out.push_str(&format!(
            "  {DIM}remote check → /scorecard verify 1 or /scorecard verify --all{RESET}\n"
        ));
    }
    out
}

pub fn render_diagnostics(diagnostics: &ScorecardStoreDiagnostics) -> String {
    let mut out = String::new();
    out.push_str(&format!("  {DIM}scorecard doctor{RESET}\n"));
    out.push_str(&format!(
        "  {DIM}store{RESET}           {}\n",
        diagnostics.path.display()
    ));
    out.push_str(&format!(
        "  {DIM}exists{RESET}          {}\n",
        if diagnostics.exists { "yes" } else { "no" }
    ));
    out.push_str(&format!(
        "  {DIM}size{RESET}            {} byte(s)\n",
        diagnostics.bytes
    ));
    out.push_str(&format!(
        "  {DIM}lines{RESET}           {} total · {} blank · {} malformed\n",
        diagnostics.total_lines, diagnostics.blank_lines, diagnostics.malformed_count
    ));
    out.push_str(&format!(
        "  {DIM}events{RESET}          {} valid · {} turn(s) · {} PR close(s) · {} verification(s)\n",
        diagnostics.event_count,
        diagnostics.turn_count,
        diagnostics.pr_count,
        diagnostics.verification_count
    ));
    if diagnostics.malformed_count == 0 {
        out.push_str(&format!(
            "  {GREEN}✓{RESET} {DIM}scorecard store is readable{RESET}\n"
        ));
    } else {
        out.push_str(&format!(
            "  {YELLOW}!{RESET} {DIM}malformed lines were skipped while reading the scorecard store{RESET}\n"
        ));
        for line in &diagnostics.malformed_lines {
            out.push_str(&format!(
                "    {YELLOW}!{RESET} line {}: {}\n",
                line.line_number,
                one_line(&line.error, 96)
            ));
        }
        if diagnostics.malformed_count > diagnostics.malformed_lines.len() {
            out.push_str(&format!(
                "    {DIM}... {} more malformed line(s){RESET}\n",
                diagnostics.malformed_count - diagnostics.malformed_lines.len()
            ));
        }
        out.push_str(&format!(
            "  {DIM}export a copy before manual repair → /scorecard export{RESET}\n"
        ));
    }
    out
}

pub fn render_recent_prs(prs: &[ScorecardPr]) -> String {
    let mut out = String::new();
    out.push_str(&format!("  {DIM}scorecard PRs{RESET}\n"));
    if prs.is_empty() {
        out.push_str(&format!(
            "  {DIM}No closed PR units yet. Run /scorecard close <label> or /ship pr.{RESET}\n"
        ));
        return out;
    }
    for (index, pr) in prs.iter().enumerate() {
        let url = pr
            .url
            .as_deref()
            .map(|url| format!(" · {url}"))
            .unwrap_or_default();
        let quality = format_quality_cell(pr.quality.as_ref());
        out.push_str(&format!(
            "  #{:<3} {}  {:<18} {:>8}  {} · {} · {}{}\n",
            index + 1,
            pr_date_label(&pr.timestamp),
            quality,
            format_tokens(pr.total_tokens),
            repo_label(&pr.repo),
            pr.branch,
            one_line(&pr.title, 64),
            url
        ));
        let evidence = render_pr_evidence_line(pr);
        if !evidence.is_empty() {
            out.push_str(&format!("        {DIM}{evidence}{RESET}\n"));
        }
    }
    out.push_str(&format!("\n  {DIM}detail → /scorecard pr <n>{RESET}\n"));
    out
}

pub fn render_pr_detail(pr: &ScorecardPr, index: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!("  {DIM}scorecard PR{RESET}     #{index}\n"));
    out.push_str(&format!(
        "  {DIM}title{RESET}           {}\n",
        one_line(&pr.title, 120)
    ));
    out.push_str(&format!(
        "  {DIM}closed{RESET}          {} · {} · {}\n",
        pr_date_label(&pr.timestamp),
        repo_label(&pr.repo),
        pr.branch
    ));
    out.push_str(&format!(
        "  {DIM}status{RESET}          {} · {} tokens · {} turn(s) · {} session(s)\n",
        pr.status,
        format_tokens(pr.total_tokens),
        pr.turn_count,
        pr.session_count
    ));
    if let Some(url) = pr.url.as_deref() {
        out.push_str(&format!("  {DIM}url{RESET}             {url}\n"));
    }
    if let Some(quality) = pr.quality.as_ref() {
        out.push_str(&format!(
            "  {DIM}quality{RESET}         {} {} · {}\n",
            quality.grade,
            format_quality_score(quality.score),
            quality.status
        ));
        if !quality.evidence.is_empty() {
            out.push_str(&format!("  {DIM}evidence{RESET}\n"));
            for item in &quality.evidence {
                out.push_str(&format!("    {GREEN}·{RESET} {item}\n"));
            }
        }
        if !quality.reasons.is_empty() {
            out.push_str(&format!("  {DIM}why not counted{RESET}\n"));
            for item in &quality.reasons {
                out.push_str(&format!("    {YELLOW}!{RESET} {item}\n"));
            }
        }
        if !quality.blockers.is_empty() {
            out.push_str(&format!("  {DIM}blockers{RESET}\n"));
            for item in &quality.blockers {
                out.push_str(&format!("    {YELLOW}!{RESET} {item}\n"));
            }
        }
        if !quality.warnings.is_empty() {
            out.push_str(&format!("  {DIM}warnings{RESET}\n"));
            for item in &quality.warnings {
                out.push_str(&format!("    {YELLOW}!{RESET} {item}\n"));
            }
        }
    } else {
        out.push_str(&format!("  {DIM}quality{RESET}         unrated\n"));
    }
    if let Some(remote) = pr.remote.as_ref() {
        out.push_str(&render_remote_verification(remote));
    } else if pr.url.as_deref().and_then(parse_github_pr_url).is_some() {
        out.push_str(&format!(
            "  {DIM}remote{RESET}          not verified yet · run /scorecard verify {index}\n"
        ));
    }
    if let Some(path) = pr.ship_record_path.as_deref() {
        out.push_str(&format!("  {DIM}ship record{RESET}    {path}\n"));
    }
    out.push('\n');
    if pr.session_traces.is_empty() && pr.session_ids.is_empty() {
        out.push_str(&format!(
            "  {DIM}sessions{RESET}        no session audit captured at close\n"
        ));
    } else if !pr.session_traces.is_empty() {
        out.push_str(&format!("  {DIM}sessions{RESET}\n"));
        for trace in &pr.session_traces {
            out.push_str(&format!("    {CYAN}{}{RESET}\n", trace.session_id));
            if trace.trace_found {
                out.push_str(&format!(
                    "      {} turn(s) · {} step(s) · {} tool(s) · {} subagent(s) · {} approval(s)\n",
                    trace.turn_count,
                    trace.total_steps,
                    trace.tool_calls,
                    trace.subagent_runs,
                    trace.approvals
                ));
                out.push_str(&format!(
                    "      model {:.1}s · tools {:.1}s · approval {:.1}s · total {:.1}s\n",
                    trace.model_ms as f64 / 1000.0,
                    trace.tool_ms as f64 / 1000.0,
                    trace.approval_ms as f64 / 1000.0,
                    trace.total_ms as f64 / 1000.0
                ));
                out.push_str(&format!(
                    "      {DIM}events → {}{RESET}\n",
                    trace.events_path
                ));
            } else {
                out.push_str(&format!(
                    "      {DIM}no event log at close (display.eventLog.enabled may have been off){RESET}\n"
                ));
            }
        }
    } else {
        out.push_str(&format!(
            "  {DIM}sessions{RESET}        {}\n",
            pr.session_ids.join(", ")
        ));
        for path in &pr.session_paths {
            out.push_str(&format!("    {DIM}session → {path}{RESET}\n"));
        }
    }
    out.push_str(&format!(
        "\n  {DIM}raw trace export → /export <session> events{RESET}\n"
    ));
    out
}

fn render_pr_evidence_line(pr: &ScorecardPr) -> String {
    let mut parts = Vec::new();
    if !pr.session_ids.is_empty() {
        parts.push(format!("{} session(s)", pr.session_ids.len()));
    }
    if pr.ship_record_path.is_some() {
        parts.push("ship record".into());
    }
    if let Some(quality) = pr.quality.as_ref() {
        if !quality.evidence.is_empty() {
            parts.push(quality.evidence.join(" · "));
        }
        if !quality.counts && !quality.reasons.is_empty() {
            parts.push(format!("not counted: {}", quality.reasons.join(" · ")));
        }
    }
    if let Some(remote) = pr.remote.as_ref() {
        parts.push(format!(
            "remote {} · {} success · {} failing · {} pending",
            remote_outcome_label(&remote.outcome),
            remote.checks.success,
            remote.checks.failing,
            remote.checks.pending
        ));
    }
    parts.join(" · ")
}

pub fn render_verification_summary(summary: &ScorecardVerifySummary) -> String {
    let remote = &summary.verification;
    format!(
        "  {GREEN}✓{RESET} {DIM}scorecard remote verified:{RESET} #{} {} · {} · {} success · {} failing · {} pending\n  {GREEN}✓{RESET} {DIM}scorecard saved →{RESET} {}\n  {DIM}detail → /scorecard pr {}{RESET}\n",
        remote.number,
        one_line(&remote.title, 72),
        remote_outcome_label(&remote.outcome),
        remote.checks.success,
        remote.checks.failing,
        remote.checks.pending,
        summary.path.display(),
        summary.index
    )
}

pub fn render_verify_all_summary(summary: &ScorecardVerifyAllSummary) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "  {GREEN}✓{RESET} {DIM}scorecard remote verify:{RESET} {} verified · {} skipped · {} failed\n",
        summary.verified.len(),
        summary.skipped.len(),
        summary.failed.len()
    ));
    for item in &summary.verified {
        out.push_str(&format!(
            "    #{:<3} {} · {} success · {} failing · {} pending · {}\n",
            item.index,
            remote_outcome_label(&item.verification.outcome),
            item.verification.checks.success,
            item.verification.checks.failing,
            item.verification.checks.pending,
            one_line(&item.verification.title, 64)
        ));
    }
    for item in &summary.skipped {
        out.push_str(&format!(
            "    {YELLOW}!{RESET} #{:<3} skipped · {} · {}\n",
            item.index,
            item.reason,
            one_line(&item.title, 64)
        ));
    }
    for item in &summary.failed {
        out.push_str(&format!(
            "    {YELLOW}!{RESET} #{:<3} failed · {} · {}\n",
            item.index,
            one_line(&item.error, 80),
            one_line(&item.title, 64)
        ));
    }
    out.push_str(&format!(
        "  {GREEN}✓{RESET} {DIM}scorecard saved →{RESET} {}\n",
        summary.path.display()
    ));
    out
}

fn render_remote_verification(remote: &ScorecardVerification) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "  {DIM}remote{RESET}          {} · checked {} · GitHub #{}\n",
        remote_outcome_label(&remote.outcome),
        pr_date_label(&remote.timestamp),
        remote.number
    ));
    out.push_str(&format!(
        "  {DIM}remote state{RESET}    {} · review {} · mergeable {}\n",
        remote.state,
        remote.review_decision.as_deref().unwrap_or("none"),
        remote.mergeable.as_deref().unwrap_or("unknown")
    ));
    out.push_str(&format!(
        "  {DIM}remote checks{RESET}   {} success · {} failing · {} pending · {} skipped\n",
        remote.checks.success, remote.checks.failing, remote.checks.pending, remote.checks.skipped
    ));
    for reason in &remote.reasons {
        out.push_str(&format!("    {YELLOW}!{RESET} {reason}\n"));
    }
    for check in remote.check_runs.iter().take(6) {
        out.push_str(&format!(
            "    {} {}\n",
            remote_check_marker(remote_check_bucket(check)),
            remote_check_line(check)
        ));
    }
    if remote.check_runs.len() > 6 {
        out.push_str(&format!(
            "    {DIM}... {} more check(s){RESET}\n",
            remote.check_runs.len() - 6
        ));
    }
    out
}

fn remote_check_marker(bucket: RemoteCheckBucket) -> &'static str {
    match bucket {
        RemoteCheckBucket::Success => "✓",
        RemoteCheckBucket::Failing => "✗",
        RemoteCheckBucket::Pending => "...",
        RemoteCheckBucket::Skipped => "-",
    }
}

fn remote_check_line(check: &ScorecardRemoteCheck) -> String {
    let mut label = String::new();
    if let Some(workflow) = &check.workflow {
        label.push_str(workflow);
        label.push_str(" / ");
    }
    label.push_str(&check.name);
    let state = check
        .conclusion
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or(&check.status);
    label.push_str(&format!(" ({state})"));
    if let Some(url) = &check.details_url {
        label.push_str(&format!(" {url}"));
    }
    label
}

pub fn render_current(summary: &OpenUnitSummary) -> String {
    format!(
        "  {DIM}current PR unit{RESET} {}\n  {DIM}branch{RESET}          {}\n  {DIM}tokens{RESET}          {} total · {} in · {} out\n  {DIM}turns{RESET}           {} turn(s) · {} session(s)\n  {DIM}quality{RESET}         scored when /ship pr closes with readiness/test evidence\n",
        repo_label(&summary.repo),
        summary.branch,
        format_tokens(summary.total_tokens),
        format_tokens(summary.input_tokens),
        format_tokens(summary.output_tokens),
        summary.turn_count,
        summary.session_count
    )
}

pub fn current_summary(workspace_root: &str) -> Result<OpenUnitSummary> {
    let events = match scorecard_path() {
        Some(path) => read_events(&path)?,
        None => Vec::new(),
    };
    Ok(open_summary_for_unit(
        &events,
        &current_work_unit(workspace_root),
    ))
}

fn verify_recent_pr_at_path<F>(
    path: &Path,
    index: usize,
    mut fetch: F,
) -> Result<Option<ScorecardVerifySummary>>
where
    F: FnMut(&ScorecardPr) -> Result<ScorecardVerification>,
{
    if index == 0 {
        return Ok(None);
    }
    let mut prs = closed_prs_with_verifications(&read_events(path)?);
    prs.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    let Some(pr) = prs.get(index - 1) else {
        return Ok(None);
    };
    ensure_verifiable_github_pr(pr)?;
    let verification = fetch(pr)?;
    append_event(path, &ScorecardEvent::Verification(verification.clone()))?;
    Ok(Some(ScorecardVerifySummary {
        path: path.to_path_buf(),
        index,
        verification,
    }))
}

fn verify_all_prs_at_path<F>(path: &Path, mut fetch: F) -> Result<ScorecardVerifyAllSummary>
where
    F: FnMut(&ScorecardPr) -> Result<ScorecardVerification>,
{
    let mut prs = closed_prs_with_verifications(&read_events(path)?);
    prs.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    let mut summary = ScorecardVerifyAllSummary {
        path: path.to_path_buf(),
        verified: Vec::new(),
        skipped: Vec::new(),
        failed: Vec::new(),
    };

    for (offset, pr) in prs.iter().enumerate() {
        let index = offset + 1;
        if let Err(error) = ensure_verifiable_github_pr(pr) {
            summary.skipped.push(ScorecardVerifySkip {
                index,
                title: pr.title.clone(),
                reason: error.to_string(),
            });
            continue;
        }
        match fetch(pr) {
            Ok(verification) => {
                append_event(path, &ScorecardEvent::Verification(verification.clone()))?;
                summary.verified.push(ScorecardVerifySummary {
                    path: path.to_path_buf(),
                    index,
                    verification,
                });
            }
            Err(error) => summary.failed.push(ScorecardVerifyError {
                index,
                title: pr.title.clone(),
                error: error.to_string(),
            }),
        }
    }

    Ok(summary)
}

fn close_pr_at_path(
    path: &Path,
    input: PrCloseInput<'_>,
    now: DateTime<Utc>,
) -> Result<Option<PrCloseSummary>> {
    let events = read_events(path)?;
    let unit = current_work_unit(input.workspace_root);
    let open = open_summary_for_unit(&events, &unit);
    if open.turn_count == 0 {
        return Ok(None);
    }
    let open_sessions = open_sessions_for_unit(&events, &unit);
    let session_traces =
        collect_session_traces(&open_sessions, input.workspace_root, input.session_dir);
    let pr = ScorecardPr {
        timestamp: now.to_rfc3339(),
        repo: unit.repo,
        branch: unit.branch,
        title: normalize_label(input.title),
        url: input
            .url
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string),
        status: input.status.trim().to_string(),
        input_tokens: open.input_tokens,
        output_tokens: open.output_tokens,
        total_tokens: open.total_tokens,
        turn_count: open.turn_count,
        session_count: open.session_count,
        quality: input
            .quality
            .map(|quality| assess_pr_quality(quality, input.quality_threshold)),
        session_ids: open_sessions.session_ids,
        session_paths: open_sessions.session_paths,
        ship_record_path: input
            .ship_record_path
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(str::to_string),
        session_traces,
        remote: None,
    };
    append_event(path, &ScorecardEvent::Pr(pr.clone()))?;
    Ok(Some(PrCloseSummary {
        path: path.to_path_buf(),
        pr,
    }))
}

fn build_report(
    path: Option<PathBuf>,
    events: &[ScorecardEvent],
    current_unit: WorkUnit,
    today: NaiveDate,
    diagnostics: Option<ScorecardStoreDiagnostics>,
) -> ScorecardReport {
    let mut lifetime_tokens = 0_u64;
    let mut turn_count = 0_usize;
    let mut closed_prs = closed_prs_with_verifications(events);
    let mut daily_scores: BTreeMap<NaiveDate, DailyScore> = BTreeMap::new();

    for event in events {
        match event {
            ScorecardEvent::Turn(turn) => {
                lifetime_tokens += turn.total_tokens;
                turn_count += 1;
            }
            ScorecardEvent::Pr(pr) => {
                if let Some(date) = date_from_timestamp(&pr.timestamp) {
                    let score = daily_scores.entry(date).or_insert(DailyScore {
                        date,
                        pr_count: 0,
                        scored_count: 0,
                        quality_count: 0,
                        total_quality_score: 0,
                        total_tokens: 0,
                    });
                    score.pr_count += 1;
                    score.total_tokens += pr.total_tokens;
                    if let Some(quality) = &pr.quality {
                        score.scored_count += 1;
                        score.total_quality_score += quality.score as u64;
                        if quality.counts {
                            score.quality_count += 1;
                        }
                    }
                }
            }
            ScorecardEvent::Verification(_) => {}
        }
    }

    closed_prs.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    let (current_streak, longest_streak) = streaks(&daily_scores, today);
    let current_open = open_summary_for_unit(events, &current_unit);

    ScorecardReport {
        path,
        lifetime_tokens,
        turn_count,
        closed_prs,
        current_open,
        daily_scores,
        current_streak,
        longest_streak,
        today,
        diagnostics,
    }
}

fn closed_prs_with_verifications(events: &[ScorecardEvent]) -> Vec<ScorecardPr> {
    let mut latest: BTreeMap<(String, String), ScorecardVerification> = BTreeMap::new();
    for event in events {
        let ScorecardEvent::Verification(verification) = event else {
            continue;
        };
        latest.insert(
            (
                verification.pr_timestamp.clone(),
                normalize_url_key(&verification.url),
            ),
            verification.clone(),
        );
    }

    events
        .iter()
        .filter_map(|event| {
            let ScorecardEvent::Pr(pr) = event else {
                return None;
            };
            let mut pr = pr.clone();
            if let Some(url) = pr.url.as_deref() {
                pr.remote = latest
                    .get(&(pr.timestamp.clone(), normalize_url_key(url)))
                    .cloned();
            }
            Some(pr)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GithubPrRef {
    repo: String,
    number: u64,
    url: String,
}

fn ensure_verifiable_github_pr(pr: &ScorecardPr) -> Result<()> {
    let url = pr
        .url
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("PR has no URL"))?;
    parse_github_pr_url(url)
        .map(|_| ())
        .ok_or_else(|| anyhow::anyhow!("PR URL is not a GitHub pull request URL"))
}

fn fetch_github_pr_verification(
    workspace_root: &str,
    pr: &ScorecardPr,
    now: DateTime<Utc>,
) -> Result<ScorecardVerification> {
    let command = build_github_pr_view_command(pr)?;
    let output = run_command_capture_combined(workspace_root, &command)?;
    parse_github_pr_view_json(pr, &output, now)
}

fn build_github_pr_view_command(pr: &ScorecardPr) -> Result<Vec<String>> {
    let url = pr
        .url
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("PR has no URL"))?;
    let target = parse_github_pr_url(url)
        .ok_or_else(|| anyhow::anyhow!("PR URL is not a GitHub pull request URL"))?;
    Ok(vec![
        "gh".to_string(),
        "pr".to_string(),
        "view".to_string(),
        target.url,
        "--repo".to_string(),
        target.repo,
        "--json".to_string(),
        "number,url,title,state,isDraft,mergedAt,headRefName,baseRefName,reviewDecision,mergeable,statusCheckRollup"
            .to_string(),
    ])
}

fn parse_github_pr_url(url: &str) -> Option<GithubPrRef> {
    let mut trimmed = url.trim();
    let query_pos = trimmed.find('?').into_iter().chain(trimmed.find('#')).min();
    if let Some(pos) = query_pos {
        trimmed = &trimmed[..pos];
    }
    trimmed = trimmed.trim_end_matches('/');
    let rest = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .or_else(|| trimmed.strip_prefix("https://www.github.com/"))
        .or_else(|| trimmed.strip_prefix("http://www.github.com/"))?;
    let mut parts = rest.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    let marker = parts.next()?.trim();
    let number = parts.next()?.trim().parse::<u64>().ok()?;
    if owner.is_empty() || repo.is_empty() || marker != "pull" {
        return None;
    }
    Some(GithubPrRef {
        repo: format!("{owner}/{repo}"),
        number,
        url: format!("https://github.com/{owner}/{repo}/pull/{number}"),
    })
}

fn parse_github_pr_view_json(
    local_pr: &ScorecardPr,
    output: &str,
    now: DateTime<Utc>,
) -> Result<ScorecardVerification> {
    let value: serde_json::Value = serde_json::from_str(output.trim())
        .map_err(|e| anyhow::anyhow!("failed to parse gh PR JSON: {e}"))?;
    let url = json_string(&value, "url")
        .or_else(|| local_pr.url.clone())
        .ok_or_else(|| anyhow::anyhow!("gh PR JSON did not include a URL"))?;
    let target = parse_github_pr_url(&url)
        .ok_or_else(|| anyhow::anyhow!("gh PR JSON URL is not a GitHub pull request URL"))?;
    let check_runs = parse_remote_checks(&value);
    let checks = remote_check_counts(&check_runs);
    let state = json_string(&value, "state").unwrap_or_else(|| "UNKNOWN".into());
    let merged = state.eq_ignore_ascii_case("MERGED") || json_string(&value, "mergedAt").is_some();
    let draft = value
        .get("isDraft")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let review_decision = json_string(&value, "reviewDecision");
    let mergeable = json_string(&value, "mergeable");
    let (outcome, reasons) =
        classify_remote_outcome(&state, merged, draft, review_decision.as_deref(), checks);

    Ok(ScorecardVerification {
        timestamp: now.to_rfc3339(),
        pr_timestamp: local_pr.timestamp.clone(),
        source: "github".into(),
        url: target.url,
        repo: target.repo,
        number: value
            .get("number")
            .and_then(|value| value.as_u64())
            .unwrap_or(target.number),
        title: json_string(&value, "title").unwrap_or_else(|| local_pr.title.clone()),
        state,
        merged,
        draft,
        head_branch: json_string(&value, "headRefName").unwrap_or_default(),
        base_branch: json_string(&value, "baseRefName").unwrap_or_default(),
        review_decision,
        mergeable,
        checks,
        check_runs,
        outcome,
        reasons,
    })
}

fn parse_remote_checks(value: &serde_json::Value) -> Vec<ScorecardRemoteCheck> {
    value
        .get("statusCheckRollup")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .map(|item| ScorecardRemoteCheck {
                    name: json_string(item, "name")
                        .or_else(|| json_string(item, "context"))
                        .unwrap_or_else(|| "unnamed check".into()),
                    workflow: json_string(item, "workflowName"),
                    status: json_string(item, "status")
                        .or_else(|| json_string(item, "state"))
                        .unwrap_or_else(|| "UNKNOWN".into()),
                    conclusion: json_string(item, "conclusion"),
                    details_url: json_string(item, "detailsUrl")
                        .or_else(|| json_string(item, "targetUrl")),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn remote_check_counts(checks: &[ScorecardRemoteCheck]) -> ScorecardRemoteChecks {
    let mut counts = ScorecardRemoteChecks {
        total: checks.len(),
        ..ScorecardRemoteChecks::default()
    };
    for check in checks {
        match remote_check_bucket(check) {
            RemoteCheckBucket::Success => counts.success += 1,
            RemoteCheckBucket::Failing => counts.failing += 1,
            RemoteCheckBucket::Pending => counts.pending += 1,
            RemoteCheckBucket::Skipped => counts.skipped += 1,
        }
    }
    counts
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteCheckBucket {
    Success,
    Failing,
    Pending,
    Skipped,
}

fn remote_check_bucket(check: &ScorecardRemoteCheck) -> RemoteCheckBucket {
    let conclusion = check.conclusion.as_deref().map(str::to_ascii_uppercase);
    let status = check.status.to_ascii_uppercase();

    if matches!(conclusion.as_deref(), Some("SUCCESS")) || status == "SUCCESS" {
        RemoteCheckBucket::Success
    } else if matches!(
        conclusion.as_deref(),
        Some("FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "ERROR")
    ) || matches!(status.as_str(), "FAILURE" | "ERROR")
    {
        RemoteCheckBucket::Failing
    } else if matches!(conclusion.as_deref(), Some("SKIPPED" | "NEUTRAL")) {
        RemoteCheckBucket::Skipped
    } else {
        RemoteCheckBucket::Pending
    }
}

fn classify_remote_outcome(
    state: &str,
    merged: bool,
    draft: bool,
    review_decision: Option<&str>,
    checks: ScorecardRemoteChecks,
) -> (String, Vec<String>) {
    let mut reasons = Vec::new();
    let state_upper = state.to_ascii_uppercase();

    if checks.failing > 0 {
        reasons.push(format!("{} remote check(s) failing", checks.failing));
        return ("failing".into(), reasons);
    }
    if checks.pending > 0 {
        reasons.push(format!("{} remote check(s) pending", checks.pending));
        return ("pending".into(), reasons);
    }
    if state_upper == "CLOSED" && !merged {
        reasons.push("PR is closed without merge".into());
        return ("closed".into(), reasons);
    }
    if draft {
        reasons.push("PR is still draft".into());
        return ("draft".into(), reasons);
    }
    if review_decision == Some("CHANGES_REQUESTED") {
        reasons.push("review changes requested".into());
        return ("changesRequested".into(), reasons);
    }
    if checks.total == 0 {
        reasons.push("no remote checks reported".into());
        return ("unchecked".into(), reasons);
    }
    if review_decision == Some("REVIEW_REQUIRED") && !merged {
        reasons.push("review still required".into());
        return ("reviewRequired".into(), reasons);
    }
    if merged {
        return ("merged".into(), reasons);
    }
    ("verified".into(), reasons)
}

fn run_command_capture_combined(workspace_root: &str, args: &[String]) -> Result<String> {
    let Some((program, rest)) = args.split_first() else {
        anyhow::bail!("empty command");
    };
    let output = Command::new(program)
        .current_dir(workspace_root)
        .args(rest)
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run {program}: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        let detail = stderr.trim();
        anyhow::bail!(
            "gh PR verification failed{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        );
    }
    let mut combined = String::new();
    combined.push_str(&stdout);
    if !stderr.trim().is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }
    Ok(combined)
}

fn open_summary_for_unit(events: &[ScorecardEvent], unit: &WorkUnit) -> OpenUnitSummary {
    let latest_close = latest_pr_timestamp(events, unit);
    let mut input_tokens = 0_u64;
    let mut output_tokens = 0_u64;
    let mut total_tokens = 0_u64;
    let mut turn_count = 0_usize;
    let mut sessions = BTreeSet::new();

    for event in events {
        let ScorecardEvent::Turn(turn) = event else {
            continue;
        };
        if turn.repo != unit.repo || turn.branch != unit.branch {
            continue;
        }
        if !is_after(&turn.timestamp, latest_close.as_ref()) {
            continue;
        }
        input_tokens += turn.input_tokens;
        output_tokens += turn.output_tokens;
        total_tokens += turn.total_tokens;
        turn_count += 1;
        sessions.insert(turn.session_id.clone());
    }

    OpenUnitSummary {
        repo: unit.repo.clone(),
        branch: unit.branch.clone(),
        input_tokens,
        output_tokens,
        total_tokens,
        turn_count,
        session_count: sessions.len(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionRef {
    session_id: String,
    session_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenUnitSessions {
    sessions: Vec<SessionRef>,
    session_ids: Vec<String>,
    session_paths: Vec<String>,
}

fn open_sessions_for_unit(events: &[ScorecardEvent], unit: &WorkUnit) -> OpenUnitSessions {
    let latest_close = latest_pr_timestamp(events, unit);
    let mut sessions = BTreeMap::new();

    for event in events {
        let ScorecardEvent::Turn(turn) = event else {
            continue;
        };
        if turn.repo != unit.repo || turn.branch != unit.branch {
            continue;
        }
        if !is_after(&turn.timestamp, latest_close.as_ref()) {
            continue;
        }
        sessions
            .entry(turn.session_id.clone())
            .or_insert_with(|| turn.session_path.clone());
    }

    let sessions: Vec<SessionRef> = sessions
        .into_iter()
        .map(|(session_id, session_path)| SessionRef {
            session_id,
            session_path,
        })
        .collect();
    let session_ids: Vec<String> = sessions.iter().map(|s| s.session_id.clone()).collect();
    let session_paths: Vec<String> = sessions.iter().map(|s| s.session_path.clone()).collect();

    OpenUnitSessions {
        sessions,
        session_ids,
        session_paths,
    }
}

fn collect_session_traces(
    open_sessions: &OpenUnitSessions,
    workspace_root: &str,
    session_dir: &str,
) -> Vec<crate::turn_trace::SessionTraceSummary> {
    open_sessions
        .sessions
        .iter()
        .map(|session| {
            let resolved = crate::turn_trace::resolve_session_path(
                &session.session_path,
                workspace_root,
                session_dir,
                &session.session_id,
            );
            let mut summary = crate::turn_trace::summarize_session_trace(&resolved);
            summary.session_id = session.session_id.clone();
            summary.events_path = crate::turn_trace::events_path_for_session(&resolved)
                .display()
                .to_string();
            summary
        })
        .collect()
}

fn session_path_for_storage(workspace_root: &str, session_path: &Path) -> String {
    let workspace = Path::new(workspace_root);
    session_path
        .strip_prefix(workspace)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| session_path.display().to_string())
}

fn latest_pr_timestamp(events: &[ScorecardEvent], unit: &WorkUnit) -> Option<DateTime<Utc>> {
    events
        .iter()
        .filter_map(|event| match event {
            ScorecardEvent::Pr(pr) if pr.repo == unit.repo && pr.branch == unit.branch => {
                parse_timestamp(&pr.timestamp)
            }
            _ => None,
        })
        .max()
}

fn is_after(timestamp: &str, after: Option<&DateTime<Utc>>) -> bool {
    match after {
        Some(after) => parse_timestamp(timestamp)
            .map(|timestamp| timestamp > *after)
            .unwrap_or(false),
        None => true,
    }
}

fn assess_pr_quality(input: PrQualityInput, quality_threshold: u8) -> ScorecardQuality {
    let mut score = 100_i16;
    let mut evidence = Vec::new();

    match input.readiness {
        PrQualityReadiness::Ready => evidence.push("ship readiness ready".to_string()),
        PrQualityReadiness::NeedsReview => {
            score -= 10;
            evidence.push("ship readiness needs review".to_string());
        }
        PrQualityReadiness::Blocked => {
            score -= 45;
            evidence.push("ship readiness blocked".to_string());
        }
    }

    match input.tests {
        PrQualityTestStatus::Passed => evidence.push("tests passed".to_string()),
        PrQualityTestStatus::Failed => {
            score -= 40;
            evidence.push("tests failed".to_string());
        }
        PrQualityTestStatus::NotRun => {
            score -= 20;
            evidence.push("tests not run".to_string());
        }
    }

    if input.opened_by_gh {
        evidence.push("PR command succeeded".to_string());
    } else {
        score -= 15;
        evidence.push("PR command not verified".to_string());
    }
    if input.has_pr_url {
        evidence.push("PR URL captured".to_string());
    } else {
        score -= 5;
        evidence.push("PR URL not captured".to_string());
    }

    score -= (input.blockers.len() as i16 * 15).min(45);
    score -= (input.warnings.len() as i16 * 5).min(20);

    let score = score.clamp(0, 100) as u8;
    let has_pr_evidence = input.opened_by_gh || input.has_pr_url;
    let counts = score >= quality_threshold
        && input.readiness != PrQualityReadiness::Blocked
        && input.tests == PrQualityTestStatus::Passed
        && has_pr_evidence;
    let mut reasons = Vec::new();
    if score < quality_threshold {
        reasons.push(format!(
            "score {score}/100 is below quality threshold {quality_threshold}/100"
        ));
    }
    if input.readiness == PrQualityReadiness::Blocked {
        reasons.push("ship readiness is blocked".to_string());
    }
    match input.tests {
        PrQualityTestStatus::Passed => {}
        PrQualityTestStatus::Failed => reasons.push("tests failed".to_string()),
        PrQualityTestStatus::NotRun => reasons.push("tests were not run".to_string()),
    }
    if !has_pr_evidence {
        reasons.push("no PR URL or successful PR command was captured".to_string());
    }
    let status = if counts {
        "quality"
    } else if input.readiness == PrQualityReadiness::Blocked {
        "blocked"
    } else {
        "needsReview"
    };

    ScorecardQuality {
        score,
        grade: grade_for_score(score).to_string(),
        counts,
        status: status.to_string(),
        reasons,
        evidence,
        blockers: input.blockers,
        warnings: input.warnings,
    }
}

fn append_event(path: &Path, event: &ScorecardEvent) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let json = serde_json::to_string(event)?;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    f.write_all(json.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

fn read_events(path: &Path) -> Result<Vec<ScorecardEvent>> {
    Ok(read_events_with_diagnostics(path)?.events)
}

fn read_events_with_diagnostics(path: &Path) -> Result<ScorecardRead> {
    const MAX_MALFORMED_LINES: usize = 5;
    let metadata = fs::metadata(path).ok();
    let mut diagnostics = ScorecardStoreDiagnostics {
        path: path.to_path_buf(),
        exists: metadata.is_some(),
        bytes: metadata.map(|m| m.len()).unwrap_or(0),
        total_lines: 0,
        blank_lines: 0,
        event_count: 0,
        turn_count: 0,
        pr_count: 0,
        verification_count: 0,
        malformed_count: 0,
        malformed_lines: Vec::new(),
    };
    if !path.exists() {
        return Ok(ScorecardRead {
            events: Vec::new(),
            diagnostics,
        });
    }
    let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line_number = index + 1;
        diagnostics.total_lines += 1;
        let line = line?;
        if line.trim().is_empty() {
            diagnostics.blank_lines += 1;
            continue;
        }
        match serde_json::from_str::<ScorecardEvent>(&line) {
            Ok(event) => {
                diagnostics.event_count += 1;
                match &event {
                    ScorecardEvent::Turn(_) => diagnostics.turn_count += 1,
                    ScorecardEvent::Pr(_) => diagnostics.pr_count += 1,
                    ScorecardEvent::Verification(_) => diagnostics.verification_count += 1,
                }
                events.push(event);
            }
            Err(error) => {
                diagnostics.malformed_count += 1;
                if diagnostics.malformed_lines.len() < MAX_MALFORMED_LINES {
                    diagnostics.malformed_lines.push(MalformedScorecardLine {
                        line_number,
                        error: error.to_string(),
                    });
                }
            }
        }
    }
    Ok(ScorecardRead {
        events,
        diagnostics,
    })
}

fn reset_store_at_path(path: &Path) -> Result<ScorecardResetSummary> {
    let backup_path = if path.exists() {
        let backup = backup_path_for(path, "reset");
        if let Some(parent) = backup.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::copy(path, &backup)
            .with_context(|| format!("backing up {} to {}", path.display(), backup.display()))?;
        fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
        Some(backup)
    } else {
        None
    };
    Ok(ScorecardResetSummary {
        path: path.to_path_buf(),
        backup_path,
    })
}

fn default_export_path_for(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(
        "events-{}-export.jsonl",
        Utc::now().format("%Y%m%dT%H%M%S%3fZ")
    ))
}

fn backup_path_for(path: &Path, reason: &str) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("events");
    parent.join(format!(
        "{stem}-{}-{reason}.backup.jsonl",
        Utc::now().format("%Y%m%dT%H%M%S%3fZ")
    ))
}

fn current_work_unit(workspace_root: &str) -> WorkUnit {
    let repo = git_capture(workspace_root, &["rev-parse", "--show-toplevel"])
        .map(|value| canonical_or_display(Path::new(value.trim())))
        .unwrap_or_else(|_| canonical_or_display(Path::new(workspace_root)));
    let branch = git_capture(workspace_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| {
            if value == "HEAD" {
                git_capture(workspace_root, &["rev-parse", "--short", "HEAD"])
                    .map(|hash| format!("detached:{}", hash.trim()))
                    .unwrap_or_else(|_| "detached".into())
            } else {
                value
            }
        })
        .unwrap_or_else(|| "(no-git)".into());
    WorkUnit { repo, branch }
}

fn git_capture(workspace_root: &str, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !output.status.success() {
        anyhow::bail!("git {} failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn canonical_or_display(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn render_daily_grid(report: &ScorecardReport) -> String {
    let weeks = 53_i64;
    let raw_start = report.today - Duration::days(weeks * 7 - 1);
    let start = raw_start - Duration::days(raw_start.weekday().num_days_from_sunday() as i64);
    let mut out = String::new();
    out.push_str(&format!("  {DIM}Quality PR activity{RESET}\n"));
    out.push_str(&render_month_labels(start, weeks as usize));
    let labels = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    for day in 0..7_i64 {
        out.push_str(&format!("  {:<3}", labels[day as usize]));
        for week in 0..weeks {
            let date = start + Duration::days(week * 7 + day);
            if date > report.today {
                out.push_str("  ");
                continue;
            }
            out.push_str(&quality_square(report.daily_scores.get(&date)));
            out.push(' ');
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "      {DIM}empty{RESET} {}  {BRIGHT_GREEN}all quality{RESET} {GREEN}some quality{RESET} {YELLOW}needs review / unrated{RESET}\n",
        empty_square()
    ));
    out
}

fn render_month_labels(start: NaiveDate, weeks: usize) -> String {
    let mut cells = vec![' '; weeks * 2 + 4];
    let mut last_month = None;
    for week in 0..weeks {
        let date = start + Duration::days(week as i64 * 7);
        let month = date.month();
        if last_month != Some(month) {
            let label = month_label(month);
            let pos = 4 + week * 2;
            for (idx, ch) in label.chars().enumerate() {
                if pos + idx < cells.len() {
                    cells[pos + idx] = ch;
                }
            }
            last_month = Some(month);
        }
    }
    let mut out = String::from("  ");
    out.extend(cells);
    out.push('\n');
    out
}

fn quality_square(score: Option<&DailyScore>) -> String {
    let Some(score) = score else {
        return empty_square();
    };
    if score.pr_count == 0 {
        return empty_square();
    }
    let color = if score.quality_count > 0 && score.quality_count == score.pr_count {
        BRIGHT_GREEN
    } else if score.quality_count > 0 {
        GREEN
    } else {
        YELLOW
    };
    format!("{color}■{RESET}")
}

fn empty_square() -> String {
    format!("{DIM}□{RESET}")
}

fn streaks(scores: &BTreeMap<NaiveDate, DailyScore>, today: NaiveDate) -> (usize, usize) {
    let active: BTreeSet<NaiveDate> = scores
        .iter()
        .filter_map(|(date, score)| (score.quality_count > 0).then_some(*date))
        .collect();
    if active.is_empty() {
        return (0, 0);
    }

    let mut current = 0_usize;
    let mut cursor = today;
    while active.contains(&cursor) {
        current += 1;
        cursor -= Duration::days(1);
    }

    let mut longest = 0_usize;
    let mut run = 0_usize;
    let first = *active.iter().next().unwrap();
    let last = *active.iter().next_back().unwrap();
    let mut day = first;
    while day <= last {
        if active.contains(&day) {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
        day += Duration::days(1);
    }
    (current, longest)
}

fn scored_pr_count(prs: &[ScorecardPr]) -> usize {
    prs.iter().filter(|pr| pr.quality.is_some()).count()
}

fn quality_pr_count(prs: &[ScorecardPr]) -> usize {
    prs.iter()
        .filter(|pr| pr.quality.as_ref().is_some_and(|quality| quality.counts))
        .count()
}

fn clean_ship_count(prs: &[ScorecardPr]) -> usize {
    prs.iter()
        .filter(|pr| {
            pr.quality
                .as_ref()
                .is_some_and(|quality| quality.counts && quality.score >= 90)
        })
        .count()
}

fn remote_verified_count(prs: &[ScorecardPr]) -> (usize, usize) {
    let mut checked = 0_usize;
    let mut ok = 0_usize;
    for verification in prs.iter().filter_map(|pr| pr.remote.as_ref()) {
        checked += 1;
        if remote_outcome_is_ok(&verification.outcome) {
            ok += 1;
        }
    }
    (ok, checked)
}

fn average_quality_score(prs: &[ScorecardPr]) -> Option<u8> {
    let mut count = 0_u64;
    let mut total = 0_u64;
    for quality in prs.iter().filter_map(|pr| pr.quality.as_ref()) {
        count += 1;
        total += quality.score as u64;
    }
    total.checked_div(count).map(|score| score as u8)
}

fn average_tokens_per_quality_pr(prs: &[ScorecardPr]) -> Option<u64> {
    let mut count = 0_u64;
    let mut total = 0_u64;
    for pr in prs
        .iter()
        .filter(|pr| pr.quality.as_ref().is_some_and(|quality| quality.counts))
    {
        count += 1;
        total += pr.total_tokens;
    }
    total.checked_div(count)
}

fn top_quality_pr(prs: &[ScorecardPr]) -> Option<(&ScorecardPr, &ScorecardQuality)> {
    prs.iter()
        .filter_map(|pr| pr.quality.as_ref().map(|quality| (pr, quality)))
        .max_by_key(|(_, quality)| quality.score)
}

fn format_quality_cell(quality: Option<&ScorecardQuality>) -> String {
    match quality {
        Some(quality) => format!(
            "{} {:>3} {}",
            quality.grade,
            format_quality_score(quality.score),
            quality.status
        ),
        None => "--  n/a unrated".to_string(),
    }
}

fn format_quality_score(score: u8) -> String {
    format!("{score}/100")
}

fn remote_outcome_label(outcome: &str) -> &'static str {
    match outcome {
        "merged" => "merged",
        "verified" => "verified",
        "pending" => "pending",
        "failing" => "failing",
        "changesRequested" => "changes requested",
        "reviewRequired" => "review required",
        "draft" => "draft",
        "closed" => "closed",
        "unchecked" => "unchecked",
        _ => "unknown",
    }
}

fn remote_outcome_is_ok(outcome: &str) -> bool {
    matches!(outcome, "verified" | "merged")
}

fn format_rate(numerator: usize, denominator: usize) -> String {
    if denominator == 0 {
        "n/a".to_string()
    } else {
        format!("{:.0}%", numerator as f64 * 100.0 / denominator as f64)
    }
}

fn grade_for_score(score: u8) -> &'static str {
    match score {
        90..=100 => "A",
        80..=89 => "B",
        70..=79 => "C",
        60..=69 => "D",
        _ => "F",
    }
}

fn parse_timestamp(timestamp: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn date_from_timestamp(timestamp: &str) -> Option<NaiveDate> {
    parse_timestamp(timestamp).map(|dt| dt.date_naive())
}

fn pr_date_label(timestamp: &str) -> String {
    date_from_timestamp(timestamp)
        .map(|date| date.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown-date".into())
}

fn normalize_label(label: &str) -> String {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        "Untitled PR".into()
    } else {
        trimmed.to_string()
    }
}

fn repo_label(repo: &str) -> String {
    Path::new(repo)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(repo)
        .to_string()
}

fn one_line(text: &str, max_chars: usize) -> String {
    let mut out = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.chars().count() <= max_chars {
        return out;
    }
    let mut truncated = String::new();
    for ch in out.chars().take(max_chars.saturating_sub(1)) {
        truncated.push(ch);
    }
    truncated.push('…');
    out = truncated;
    out
}

fn normalize_url_key(url: &str) -> String {
    if let Some(target) = parse_github_pr_url(url) {
        return target.url.to_ascii_lowercase();
    }
    let mut key = url.trim().to_ascii_lowercase();
    while key.ends_with('/') {
        key.pop();
    }
    key
}

fn json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn month_label(month: u32) -> &'static str {
    match month {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "",
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn ts(day: u32) -> String {
        format!("2026-06-{day:02}T12:00:00Z")
    }

    fn turn(day: u32, tokens: u64, session: &str) -> ScorecardEvent {
        ScorecardEvent::Turn(ScorecardTurn {
            timestamp: ts(day),
            repo: "/repo/demo".into(),
            branch: "feature".into(),
            session_id: session.into(),
            session_path: format!(".sessions/{session}.jsonl"),
            backend: "openrouter".into(),
            model: "model".into(),
            input_tokens: tokens / 2,
            output_tokens: tokens - tokens / 2,
            total_tokens: tokens,
        })
    }

    fn pr(day: u32, tokens: u64, title: &str) -> ScorecardEvent {
        ScorecardEvent::Pr(ScorecardPr {
            timestamp: ts(day),
            repo: "/repo/demo".into(),
            branch: "feature".into(),
            title: title.into(),
            url: None,
            status: "created".into(),
            input_tokens: tokens,
            output_tokens: 0,
            total_tokens: tokens,
            turn_count: 1,
            session_count: 1,
            session_ids: Vec::new(),
            session_paths: Vec::new(),
            ship_record_path: None,
            quality: None,
            session_traces: Vec::new(),
            remote: None,
        })
    }

    fn quality_pr(day: u32, tokens: u64, title: &str, score: u8, counts: bool) -> ScorecardEvent {
        ScorecardEvent::Pr(ScorecardPr {
            timestamp: ts(day),
            repo: "/repo/demo".into(),
            branch: "feature".into(),
            title: title.into(),
            url: None,
            status: "created".into(),
            input_tokens: tokens,
            output_tokens: 0,
            total_tokens: tokens,
            turn_count: 1,
            session_count: 1,
            session_ids: Vec::new(),
            session_paths: Vec::new(),
            ship_record_path: None,
            quality: Some(ScorecardQuality {
                score,
                grade: grade_for_score(score).into(),
                counts,
                status: if counts { "quality" } else { "needsReview" }.into(),
                reasons: Vec::new(),
                evidence: vec!["tests passed".into()],
                blockers: Vec::new(),
                warnings: Vec::new(),
            }),
            session_traces: Vec::new(),
            remote: None,
        })
    }

    fn github_pr_event(day: u32, title: &str) -> ScorecardEvent {
        let ScorecardEvent::Pr(mut pr) = quality_pr(day, 1000, title, 92, true) else {
            unreachable!();
        };
        pr.url =
            Some("https://github.com/morganlinton/Albatross/pull/42/files?check_suite=1".into());
        ScorecardEvent::Pr(pr)
    }

    fn verification_for(pr: &ScorecardPr, outcome: &str) -> ScorecardVerification {
        ScorecardVerification {
            timestamp: "2026-06-20T12:00:00Z".into(),
            pr_timestamp: pr.timestamp.clone(),
            source: "github".into(),
            url: pr.url.clone().unwrap(),
            repo: "morganlinton/Albatross".into(),
            number: 42,
            title: pr.title.clone(),
            state: "OPEN".into(),
            merged: false,
            draft: false,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            review_decision: Some("APPROVED".into()),
            mergeable: Some("MERGEABLE".into()),
            checks: ScorecardRemoteChecks {
                total: 2,
                success: 2,
                failing: 0,
                pending: 0,
                skipped: 0,
            },
            check_runs: vec![ScorecardRemoteCheck {
                name: "cargo test".into(),
                workflow: Some("CI".into()),
                status: "COMPLETED".into(),
                conclusion: Some("SUCCESS".into()),
                details_url: Some("https://example.com/check".into()),
            }],
            outcome: outcome.into(),
            reasons: Vec::new(),
        }
    }

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

    #[test]
    fn report_uses_quality_score_instead_of_lowest_tokens() {
        let events = vec![
            turn(10, 1000, "a"),
            quality_pr(10, 1000, "well tested", 94, true),
            turn(11, 200, "b"),
            quality_pr(11, 200, "thin evidence", 72, false),
        ];
        let report = build_report(
            None,
            &events,
            WorkUnit {
                repo: "/repo/demo".into(),
                branch: "feature".into(),
            },
            NaiveDate::from_ymd_opt(2026, 6, 11).unwrap(),
            None,
        );
        let (top, quality) = top_quality_pr(&report.closed_prs).unwrap();
        assert_eq!(top.title, "well tested");
        assert_eq!(quality.score, 94);
        assert_eq!(quality_pr_count(&report.closed_prs), 1);
        assert_eq!(
            average_tokens_per_quality_pr(&report.closed_prs),
            Some(1000)
        );
        assert_eq!(report.longest_streak, 1);
    }

    #[test]
    fn open_summary_counts_only_turns_after_latest_pr_boundary() {
        let events = vec![
            turn(10, 1000, "a"),
            pr(10, 1000, "first"),
            turn(11, 300, "b"),
            turn(12, 700, "c"),
        ];
        let summary = open_summary_for_unit(
            &events,
            &WorkUnit {
                repo: "/repo/demo".into(),
                branch: "feature".into(),
            },
        );
        assert_eq!(summary.total_tokens, 1000);
        assert_eq!(summary.turn_count, 2);
        assert_eq!(summary.session_count, 2);
    }

    #[test]
    fn render_recent_prs_lists_raw_token_totals() {
        let prs = vec![match quality_pr(12, 42000, "tiny fix", 91, true) {
            ScorecardEvent::Pr(pr) => pr,
            _ => unreachable!(),
        }];
        let rendered = render_recent_prs(&prs);
        assert!(rendered.contains("#1  "));
        assert!(rendered.contains("A"));
        assert!(rendered.contains("91/100"));
        assert!(rendered.contains("42.0k"));
        assert!(rendered.contains("tiny fix"));
    }

    #[test]
    fn parses_github_pr_urls_for_remote_verification() {
        let parsed = parse_github_pr_url(
            "https://github.com/morganlinton/Albatross/pull/42/files?check_suite=1#summary",
        )
        .unwrap();

        assert_eq!(parsed.repo, "morganlinton/Albatross");
        assert_eq!(parsed.number, 42);
        assert_eq!(
            parsed.url,
            "https://github.com/morganlinton/Albatross/pull/42"
        );
        assert!(parse_github_pr_url("https://example.com/a/b/pull/1").is_none());
    }

    #[test]
    fn parses_github_pr_view_json_into_remote_verification() {
        let local = match github_pr_event(12, "remote status") {
            ScorecardEvent::Pr(pr) => pr,
            _ => unreachable!(),
        };
        let json = r#"{
            "number": 42,
            "url": "https://github.com/morganlinton/Albatross/pull/42",
            "title": "remote status",
            "state": "OPEN",
            "isDraft": false,
            "mergedAt": null,
            "headRefName": "feature",
            "baseRefName": "main",
            "reviewDecision": "APPROVED",
            "mergeable": "MERGEABLE",
            "statusCheckRollup": [
                {
                    "__typename": "CheckRun",
                    "name": "clippy + fmt",
                    "workflowName": "CI",
                    "status": "COMPLETED",
                    "conclusion": "SUCCESS",
                    "detailsUrl": "https://example.com/success"
                },
                {
                    "__typename": "StatusContext",
                    "context": "required",
                    "state": "SUCCESS",
                    "targetUrl": "https://example.com/status"
                },
                {
                    "__typename": "CheckRun",
                    "name": "optional",
                    "workflowName": "CI",
                    "status": "COMPLETED",
                    "conclusion": "SKIPPED",
                    "detailsUrl": "https://example.com/skipped"
                }
            ]
        }"#;

        let verification = parse_github_pr_view_json(
            &local,
            json,
            DateTime::parse_from_rfc3339("2026-06-20T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        )
        .unwrap();

        assert_eq!(verification.outcome, "verified");
        assert_eq!(verification.checks.success, 2);
        assert_eq!(verification.checks.skipped, 1);
        assert_eq!(verification.review_decision.as_deref(), Some("APPROVED"));
    }

    #[test]
    fn verify_recent_pr_appends_verification_without_mutating_pr_close() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let ScorecardEvent::Pr(pr) = github_pr_event(12, "append remote") else {
            unreachable!();
        };
        append_event(&path, &ScorecardEvent::Pr(pr.clone())).unwrap();

        let summary = verify_recent_pr_at_path(&path, 1, |pr| Ok(verification_for(pr, "verified")))
            .unwrap()
            .unwrap();
        let events = read_events(&path).unwrap();
        let prs = closed_prs_with_verifications(&events);
        let stored_pr_events = events
            .iter()
            .filter(|event| matches!(event, ScorecardEvent::Pr(_)))
            .count();
        let verification_events = events
            .iter()
            .filter(|event| matches!(event, ScorecardEvent::Verification(_)))
            .count();

        assert_eq!(summary.index, 1);
        assert_eq!(stored_pr_events, 1);
        assert_eq!(verification_events, 1);
        assert_eq!(
            prs[0].remote.as_ref().map(|remote| remote.outcome.as_str()),
            Some("verified")
        );
        assert!(render_pr_detail(&prs[0], 1).contains("remote"));
        assert!(render_pr_detail(&prs[0], 1).contains("cargo test"));
    }

    #[test]
    fn quality_assessment_requires_tests_and_pr_creation() {
        let warnings = vec!["tests were not run for this PR".to_string()];
        let quality = assess_pr_quality(
            PrQualityInput {
                readiness: PrQualityReadiness::NeedsReview,
                blockers: vec![],
                warnings,
                tests: PrQualityTestStatus::NotRun,
                opened_by_gh: true,
                has_pr_url: true,
            },
            DEFAULT_QUALITY_PR_THRESHOLD,
        );

        assert_eq!(quality.score, 65);
        assert_eq!(quality.grade, "D");
        assert!(!quality.counts);
        assert_eq!(quality.status, "needsReview");
        assert!(quality
            .reasons
            .contains(&"score 65/100 is below quality threshold 80/100".to_string()));
        assert!(quality.reasons.contains(&"tests were not run".to_string()));
    }

    #[test]
    fn read_events_reports_malformed_jsonl_without_dropping_valid_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let valid = serde_json::to_string(&turn(10, 1000, "a")).unwrap();
        std::fs::write(
            &path,
            format!("{valid}\nnot json\n\n{{\"kind\":\"unknown\"}}\n"),
        )
        .unwrap();

        let read = read_events_with_diagnostics(&path).unwrap();

        assert_eq!(read.events.len(), 1);
        assert_eq!(read.diagnostics.total_lines, 4);
        assert_eq!(read.diagnostics.blank_lines, 1);
        assert_eq!(read.diagnostics.event_count, 1);
        assert_eq!(read.diagnostics.turn_count, 1);
        assert_eq!(read.diagnostics.malformed_count, 2);
        assert_eq!(read.diagnostics.malformed_lines[0].line_number, 2);
        assert_eq!(read.diagnostics.malformed_lines[1].line_number, 4);
    }

    #[test]
    fn render_report_warns_when_store_has_malformed_lines() {
        let diagnostics = ScorecardStoreDiagnostics {
            path: PathBuf::from("/tmp/events.jsonl"),
            exists: true,
            bytes: 10,
            total_lines: 2,
            blank_lines: 0,
            event_count: 1,
            turn_count: 1,
            pr_count: 0,
            verification_count: 0,
            malformed_count: 1,
            malformed_lines: vec![MalformedScorecardLine {
                line_number: 2,
                error: "expected value".into(),
            }],
        };
        let report = build_report(
            Some(PathBuf::from("/tmp/events.jsonl")),
            &[turn(10, 1000, "a")],
            WorkUnit {
                repo: "/repo/demo".into(),
                branch: "feature".into(),
            },
            NaiveDate::from_ymd_opt(2026, 6, 11).unwrap(),
            Some(diagnostics),
        );

        let rendered = render_report(&report);

        assert!(rendered.contains("store warning"));
        assert!(rendered.contains("/scorecard doctor"));
    }

    #[test]
    fn render_diagnostics_lists_malformed_lines() {
        let diagnostics = ScorecardStoreDiagnostics {
            path: PathBuf::from("/tmp/events.jsonl"),
            exists: true,
            bytes: 10,
            total_lines: 2,
            blank_lines: 0,
            event_count: 1,
            turn_count: 1,
            pr_count: 0,
            verification_count: 0,
            malformed_count: 1,
            malformed_lines: vec![MalformedScorecardLine {
                line_number: 2,
                error: "expected value".into(),
            }],
        };

        let rendered = render_diagnostics(&diagnostics);

        assert!(rendered.contains("scorecard doctor"));
        assert!(rendered.contains("line 2"));
        assert!(rendered.contains("/scorecard export"));
    }

    #[test]
    fn reset_store_at_path_copies_backup_before_removing_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, "scorecard data\n").unwrap();

        let reset = reset_store_at_path(&path).unwrap();

        assert_eq!(reset.path, path);
        assert!(!reset.path.exists());
        let backup = reset.backup_path.expect("backup path");
        assert!(backup.exists());
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "scorecard data\n");
    }

    #[test]
    fn close_pr_at_path_writes_boundary_for_current_branch() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init"]);
        git(repo.path(), &["checkout", "-b", "feature"]);
        let path = repo.path().join("events.jsonl");
        let unit = current_work_unit(repo.path().to_str().unwrap());

        append_event(
            &path,
            &ScorecardEvent::Turn(ScorecardTurn {
                timestamp: ts(13),
                repo: unit.repo.clone(),
                branch: unit.branch.clone(),
                session_id: "a".into(),
                session_path: ".sessions/a.jsonl".into(),
                backend: "openrouter".into(),
                model: "model".into(),
                input_tokens: 700,
                output_tokens: 300,
                total_tokens: 1000,
            }),
        )
        .unwrap();

        let closed = close_pr_at_path(
            &path,
            PrCloseInput {
                workspace_root: repo.path().to_str().unwrap(),
                session_dir: ".sessions",
                title: "feature PR",
                url: Some("https://example.com/pr/1"),
                status: "created",
                quality: Some(PrQualityInput {
                    readiness: PrQualityReadiness::Ready,
                    blockers: vec![],
                    warnings: vec![],
                    tests: PrQualityTestStatus::Passed,
                    opened_by_gh: true,
                    has_pr_url: true,
                }),
                ship_record_path: Some(".sessions/ship-pr.md"),
                quality_threshold: DEFAULT_QUALITY_PR_THRESHOLD,
            },
            DateTime::parse_from_rfc3339("2026-06-14T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        )
        .unwrap()
        .unwrap();

        assert_eq!(closed.pr.total_tokens, 1000);
        assert_eq!(closed.pr.turn_count, 1);
        assert_eq!(closed.pr.title, "feature PR");
        assert_eq!(closed.pr.url.as_deref(), Some("https://example.com/pr/1"));
        assert_eq!(closed.pr.quality.as_ref().unwrap().score, 100);
        assert!(closed.pr.quality.as_ref().unwrap().counts);
        assert_eq!(closed.pr.session_ids, vec!["a".to_string()]);
        assert_eq!(
            closed.pr.ship_record_path.as_deref(),
            Some(".sessions/ship-pr.md")
        );
        assert!(close_pr_at_path(
            &path,
            PrCloseInput {
                workspace_root: repo.path().to_str().unwrap(),
                session_dir: ".sessions",
                title: "duplicate",
                url: None,
                status: "manual",
                quality: None,
                ship_record_path: None,
                quality_threshold: DEFAULT_QUALITY_PR_THRESHOLD,
            },
            DateTime::parse_from_rfc3339("2026-06-14T13:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn manual_quality_input_counts_with_url_and_tests() {
        let quality = assess_pr_quality(
            PrQualityInput {
                readiness: PrQualityReadiness::Ready,
                blockers: vec![],
                warnings: vec![],
                tests: PrQualityTestStatus::Passed,
                opened_by_gh: false,
                has_pr_url: true,
            },
            DEFAULT_QUALITY_PR_THRESHOLD,
        );
        assert_eq!(quality.score, 85);
        assert!(quality.counts);
    }

    #[test]
    fn render_pr_detail_includes_quality_and_sessions() {
        let pr = ScorecardPr {
            timestamp: ts(12),
            repo: "/repo/demo".into(),
            branch: "feature".into(),
            title: "audit me".into(),
            url: Some("https://example.com/pr/2".into()),
            status: "created".into(),
            input_tokens: 1000,
            output_tokens: 500,
            total_tokens: 1500,
            turn_count: 3,
            session_count: 1,
            session_ids: vec!["sess-a".into()],
            session_paths: vec![".sessions/sess-a.jsonl".into()],
            ship_record_path: Some(".sessions/ship-pr.md".into()),
            quality: Some(ScorecardQuality {
                score: 91,
                grade: "A".into(),
                counts: true,
                status: "quality".into(),
                reasons: Vec::new(),
                evidence: vec!["tests passed".into()],
                blockers: Vec::new(),
                warnings: Vec::new(),
            }),
            session_traces: vec![crate::turn_trace::SessionTraceSummary {
                session_id: "sess-a".into(),
                events_path: ".sessions/sess-a.events.jsonl".into(),
                turn_count: 3,
                total_steps: 8,
                tool_calls: 4,
                subagent_runs: 1,
                approvals: 2,
                model_ms: 1000,
                tool_ms: 2000,
                approval_ms: 100,
                total_ms: 3200,
                trace_found: true,
            }],
            remote: None,
        };
        let rendered = render_pr_detail(&pr, 1);
        assert!(rendered.contains("#1"));
        assert!(rendered.contains("audit me"));
        assert!(rendered.contains("A 91/100"));
        assert!(rendered.contains("sess-a"));
        assert!(rendered.contains("ship record"));
        assert!(rendered.contains("4 tool(s)"));
    }

    #[test]
    fn render_pr_detail_explains_quality_failures() {
        let pr = match quality_pr(12, 42000, "thin evidence", 65, false) {
            ScorecardEvent::Pr(mut pr) => {
                pr.quality.as_mut().unwrap().reasons = vec![
                    "score 65/100 is below quality threshold 80/100".into(),
                    "tests were not run".into(),
                ];
                pr
            }
            _ => unreachable!(),
        };

        let rendered = render_pr_detail(&pr, 1);

        assert!(rendered.contains("why not counted"));
        assert!(rendered.contains("tests were not run"));
    }

    #[test]
    fn close_pr_attaches_session_traces_when_events_exist() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init"]);
        git(repo.path(), &["checkout", "-b", "feature"]);
        let sessions_dir = repo.path().join(".sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let session_path = sessions_dir.join("audit-a.jsonl");
        let mut trace = crate::turn_trace::TurnTrace::open(&session_path, true).unwrap();
        trace.begin_turn();
        trace
            .log_turn_summary(crate::turn_trace::TurnMetrics {
                steps: 2,
                ttft_ms: None,
                model_ms: 100,
                tool_ms: 200,
                approval_ms: 0,
                total_ms: 0,
                hit_step_limit: false,
            })
            .unwrap();

        let path = repo.path().join("events.jsonl");
        let unit = current_work_unit(repo.path().to_str().unwrap());
        append_event(
            &path,
            &ScorecardEvent::Turn(ScorecardTurn {
                timestamp: ts(13),
                repo: unit.repo.clone(),
                branch: unit.branch.clone(),
                session_id: "audit-a".into(),
                session_path: ".sessions/audit-a.jsonl".into(),
                backend: "openrouter".into(),
                model: "model".into(),
                input_tokens: 500,
                output_tokens: 500,
                total_tokens: 1000,
            }),
        )
        .unwrap();

        let closed = close_pr_at_path(
            &path,
            PrCloseInput {
                workspace_root: repo.path().to_str().unwrap(),
                session_dir: ".sessions",
                title: "traced PR",
                url: None,
                status: "manual",
                quality: None,
                ship_record_path: None,
                quality_threshold: DEFAULT_QUALITY_PR_THRESHOLD,
            },
            DateTime::parse_from_rfc3339("2026-06-14T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        )
        .unwrap()
        .unwrap();

        assert_eq!(closed.pr.session_traces.len(), 1);
        assert!(closed.pr.session_traces[0].trace_found);
        assert_eq!(closed.pr.session_traces[0].turn_count, 1);
        assert_eq!(closed.pr.session_traces[0].total_steps, 2);
    }
}
