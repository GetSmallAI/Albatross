use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc, Weekday};
use std::path::PathBuf;

use crate::config::FableUsageConfig;
use crate::scorecard::{ScorecardStoreDiagnostics, ScorecardTurn};

const RESET: crate::theme::Style = crate::theme::RESET;
const DIM: crate::theme::Style = crate::theme::MUTED;
const GREEN: crate::theme::Style = crate::theme::SUCCESS;
const YELLOW: crate::theme::Style = crate::theme::WARN;
const RED: crate::theme::Style = crate::theme::ERROR;
const CYAN: crate::theme::Style = crate::theme::ACCENT_DEEP;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageBucket {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub turns: usize,
}

impl UsageBucket {
    fn add_turn(&mut self, turn: &ScorecardTurn) {
        self.input_tokens += turn.input_tokens;
        self.output_tokens += turn.output_tokens;
        self.total_tokens += turn.total_tokens;
        self.turns += 1;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FableTurnMarker {
    pub timestamp: DateTime<Utc>,
    pub backend: String,
    pub model: String,
    pub session_id: String,
}

#[derive(Debug, Clone)]
pub struct FableUsageReport {
    pub path: Option<PathBuf>,
    pub diagnostics: Option<ScorecardStoreDiagnostics>,
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,
    pub cap_share: f64,
    pub weekly_token_budget: Option<u64>,
    pub fable_cap_tokens: Option<u64>,
    pub fable: UsageBucket,
    pub claude: UsageBucket,
    pub other_claude: UsageBucket,
    pub all_tracked: UsageBucket,
    pub last_fable: Option<FableTurnMarker>,
}

pub fn load_report(config: &FableUsageConfig) -> Result<FableUsageReport> {
    let read = crate::scorecard::load_turn_records()?;
    Ok(build_report(
        read.path,
        read.diagnostics,
        &read.turns,
        config,
        Utc::now(),
    ))
}

pub fn is_fable_model(config: &FableUsageConfig, model: &str) -> bool {
    model_matches(model, &config.fable_model_matches, &["fable"])
}

pub fn format_footer_suffix(config: &FableUsageConfig, current_model: &str) -> Result<String> {
    if !config.enabled || !is_fable_model(config, current_model) {
        return Ok(String::new());
    }
    let report = load_report(config)?;
    Ok(format_footer_suffix_for_report(&report))
}

pub fn render_report(report: &FableUsageReport, scorecard_enabled: bool) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "  {DIM}fable tracker{RESET}   Claude Fable weekly usage\n"
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
    out.push_str(&format!(
        "  {DIM}window{RESET}          {} -> {}\n",
        format_ts(report.window_start),
        format_ts(report.window_end)
    ));
    out.push('\n');
    out.push_str(&format!(
        "  {CYAN}{:<14}{RESET} {:>10}   {CYAN}{:<14}{RESET} {:>10}   {CYAN}{:<14}{RESET} {:>10}\n",
        "Fable tokens",
        format_tokens(report.fable.total_tokens),
        "Fable turns",
        report.fable.turns,
        "Cap share",
        format_percent(report.cap_share)
    ));
    out.push_str(&format!(
        "  {CYAN}{:<14}{RESET} {:>10}   {CYAN}{:<14}{RESET} {:>10}   {CYAN}{:<14}{RESET} {:>10}\n",
        "Fable in",
        format_tokens(report.fable.input_tokens),
        "Fable out",
        format_tokens(report.fable.output_tokens),
        "All tracked",
        format_tokens(report.all_tracked.total_tokens)
    ));
    out.push_str(&format!(
        "  {CYAN}{:<14}{RESET} {:>10}   {CYAN}{:<14}{RESET} {:>10}   {CYAN}{:<14}{RESET} {:>10}\n",
        "Claude total",
        format_tokens(report.claude.total_tokens),
        "Other Claude",
        format_tokens(report.other_claude.total_tokens),
        "Claude share",
        report
            .fable_share_of_claude()
            .map(format_percent)
            .unwrap_or_else(|| "n/a".to_string())
    ));
    out.push('\n');

    match report.fable_cap_tokens {
        Some(cap) => {
            let ratio = usage_ratio(report.fable.total_tokens, cap);
            let (color, label) = cap_status(ratio);
            out.push_str(&format!(
                "  {DIM}cap{RESET}             {color}{} / {} ({}) · {} left · {label}{RESET}\n",
                format_tokens(report.fable.total_tokens),
                format_tokens(cap),
                format_percent(ratio),
                format_tokens(cap.saturating_sub(report.fable.total_tokens))
            ));
            if report.fable.total_tokens > cap {
                out.push_str(&format!(
                    "  {RED}!{RESET} {DIM}Fable is over the configured weekly allowance.{RESET}\n"
                ));
            }
        }
        None => out.push_str(&format!(
            "  {DIM}cap{RESET}             not configured · set fable.weeklyTokenBudget to show remaining allowance\n"
        )),
    }

    if let Some(share) = report.fable_share_of_claude() {
        let (color, label) = share_status(share, report.cap_share);
        out.push_str(&format!(
            "  {DIM}tracked share{RESET}   {color}{} of tracked Claude usage · {label}{RESET}\n",
            format_percent(share)
        ));
    } else {
        out.push_str(&format!(
            "  {DIM}tracked share{RESET}   n/a · no Claude-family turns in this window\n"
        ));
    }

    match &report.last_fable {
        Some(last) => out.push_str(&format!(
            "  {DIM}last Fable{RESET}      {} · {} · {} · {}\n",
            format_ts(last.timestamp),
            last.backend,
            one_line(&last.model, 48),
            last.session_id
        )),
        None => out.push_str(&format!(
            "  {DIM}last Fable{RESET}      none in this window\n"
        )),
    }

    if report.fable.turns == 0 {
        out.push_str(&format!(
            "  {DIM}hint{RESET}            switch to a model id containing `fable` to start tracking Fable turns\n"
        ));
    }
    if !scorecard_enabled {
        out.push_str(&format!(
            "  {YELLOW}!{RESET} {DIM}scorecard.enabled is false, so new turns are not being recorded for this tracker.{RESET}\n"
        ));
    }
    if report.weekly_token_budget.is_none() {
        out.push_str(&format!(
            "  {DIM}config{RESET}          example: {{ \"fable\": {{ \"weeklyTokenBudget\": 200000, \"capShare\": 0.5 }} }}\n"
        ));
    }
    out.push_str(&format!(
        "  {DIM}scope{RESET}           Albatross tracked turns only; external Claude app usage is not visible here.\n"
    ));
    out
}

impl FableUsageReport {
    pub fn fable_share_of_claude(&self) -> Option<f64> {
        if self.claude.total_tokens == 0 {
            None
        } else {
            Some(self.fable.total_tokens as f64 / self.claude.total_tokens as f64)
        }
    }
}

fn build_report(
    path: Option<PathBuf>,
    diagnostics: Option<ScorecardStoreDiagnostics>,
    turns: &[ScorecardTurn],
    config: &FableUsageConfig,
    now: DateTime<Utc>,
) -> FableUsageReport {
    let cap_share = normalized_cap_share(config.cap_share);
    let week_start = week_start(now, parse_weekday(&config.week_starts_on));
    let week_end = week_start + Duration::days(7);
    let fable_cap_tokens = config.weekly_token_budget.and_then(|budget| {
        if budget == 0 {
            None
        } else {
            Some(((budget as f64 * cap_share).round() as u64).max(1))
        }
    });

    let mut fable = UsageBucket::default();
    let mut claude = UsageBucket::default();
    let mut all_tracked = UsageBucket::default();
    let mut last_fable: Option<FableTurnMarker> = None;

    for turn in turns {
        let Some(timestamp) = parse_timestamp(&turn.timestamp) else {
            continue;
        };
        if timestamp < week_start || timestamp >= week_end {
            continue;
        }
        all_tracked.add_turn(turn);
        let is_fable = is_fable_model(config, &turn.model);
        let is_claude = is_fable || is_claude_model(config, &turn.model);
        if is_claude {
            claude.add_turn(turn);
        }
        if is_fable {
            fable.add_turn(turn);
            if last_fable
                .as_ref()
                .map(|last| timestamp > last.timestamp)
                .unwrap_or(true)
            {
                last_fable = Some(FableTurnMarker {
                    timestamp,
                    backend: turn.backend.clone(),
                    model: turn.model.clone(),
                    session_id: turn.session_id.clone(),
                });
            }
        }
    }

    let other_claude = UsageBucket {
        input_tokens: claude.input_tokens.saturating_sub(fable.input_tokens),
        output_tokens: claude.output_tokens.saturating_sub(fable.output_tokens),
        total_tokens: claude.total_tokens.saturating_sub(fable.total_tokens),
        turns: claude.turns.saturating_sub(fable.turns),
    };

    FableUsageReport {
        path,
        diagnostics,
        window_start: week_start,
        window_end: week_end,
        cap_share,
        weekly_token_budget: config.weekly_token_budget,
        fable_cap_tokens,
        fable,
        claude,
        other_claude,
        all_tracked,
        last_fable,
    }
}

fn format_footer_suffix_for_report(report: &FableUsageReport) -> String {
    if report.fable.total_tokens == 0 {
        return String::new();
    }
    if let Some(cap) = report.fable_cap_tokens {
        return format!(
            "Fable {} / {} wk ({})",
            format_tokens(report.fable.total_tokens),
            format_tokens(cap),
            format_percent(usage_ratio(report.fable.total_tokens, cap))
        );
    }
    if let Some(share) = report.fable_share_of_claude() {
        return format!(
            "Fable {} wk · {} Claude",
            format_tokens(report.fable.total_tokens),
            format_percent(share)
        );
    }
    format!("Fable {} wk", format_tokens(report.fable.total_tokens))
}

fn is_claude_model(config: &FableUsageConfig, model: &str) -> bool {
    model_matches(
        model,
        &config.claude_model_matches,
        &["anthropic/", "claude", "fable"],
    )
}

fn model_matches(model: &str, configured: &[String], defaults: &[&str]) -> bool {
    let model = model.to_ascii_lowercase();
    let mut saw_configured = false;
    for pattern in configured {
        let pattern = pattern.trim().to_ascii_lowercase();
        if pattern.is_empty() {
            continue;
        }
        saw_configured = true;
        if model.contains(&pattern) {
            return true;
        }
    }
    !saw_configured
        && defaults
            .iter()
            .any(|pattern| model.contains(&pattern.to_ascii_lowercase()))
}

fn parse_weekday(value: &str) -> Weekday {
    match value.trim().to_ascii_lowercase().as_str() {
        "sun" | "sunday" => Weekday::Sun,
        "tue" | "tues" | "tuesday" => Weekday::Tue,
        "wed" | "wednesday" => Weekday::Wed,
        "thu" | "thur" | "thurs" | "thursday" => Weekday::Thu,
        "fri" | "friday" => Weekday::Fri,
        "sat" | "saturday" => Weekday::Sat,
        _ => Weekday::Mon,
    }
}

fn week_start(now: DateTime<Utc>, starts_on: Weekday) -> DateTime<Utc> {
    let today = now.date_naive();
    let today_idx = today.weekday().num_days_from_monday() as i64;
    let start_idx = starts_on.num_days_from_monday() as i64;
    let days_since_start = (today_idx - start_idx).rem_euclid(7);
    date_at_midnight(today - Duration::days(days_since_start))
}

fn date_at_midnight(date: NaiveDate) -> DateTime<Utc> {
    DateTime::from_naive_utc_and_offset(date.and_hms_opt(0, 0, 0).unwrap(), Utc)
}

fn parse_timestamp(timestamp: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn normalized_cap_share(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.01, 1.0)
    } else {
        0.5
    }
}

fn usage_ratio(used: u64, cap: u64) -> f64 {
    if cap == 0 {
        0.0
    } else {
        used as f64 / cap as f64
    }
}

fn cap_status(ratio: f64) -> (crate::theme::Style, &'static str) {
    if ratio >= 1.0 {
        (RED, "over cap")
    } else if ratio >= 0.8 {
        (YELLOW, "near cap")
    } else {
        (GREEN, "on track")
    }
}

fn share_status(share: f64, cap_share: f64) -> (crate::theme::Style, &'static str) {
    if share > cap_share {
        (YELLOW, "above configured share")
    } else {
        (GREEN, "within configured share")
    }
}

fn format_ts(timestamp: DateTime<Utc>) -> String {
    timestamp.format("%Y-%m-%d %H:%M UTC").to_string()
}

fn format_percent(value: f64) -> String {
    if !value.is_finite() {
        return "n/a".into();
    }
    let percent = value * 100.0;
    if percent > 0.0 && percent < 10.0 {
        format!("{percent:.1}%")
    } else {
        format!("{percent:.0}%")
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

fn one_line(text: &str, max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }
    let mut out: String = compact.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(day: u32) -> String {
        format!("2026-07-{day:02}T12:00:00Z")
    }

    fn turn(day: u32, model: &str, tokens: u64) -> ScorecardTurn {
        ScorecardTurn {
            timestamp: ts(day),
            repo: "/repo/demo".into(),
            branch: "feature".into(),
            session_id: format!("s{day}"),
            session_path: format!(".sessions/s{day}.jsonl"),
            backend: "openrouter".into(),
            model: model.into(),
            input_tokens: tokens / 2,
            output_tokens: tokens - tokens / 2,
            total_tokens: tokens,
        }
    }

    #[test]
    fn model_match_defaults_find_fable_and_claude() {
        let config = FableUsageConfig::default();
        assert!(is_fable_model(&config, "anthropic/claude-fable-5-20260701"));
        assert!(!is_fable_model(&config, "anthropic/claude-sonnet-4.5"));
        assert!(is_claude_model(&config, "anthropic/claude-sonnet-4.5"));
    }

    #[test]
    fn weekly_report_filters_to_configured_week_and_cap() {
        let config = FableUsageConfig {
            weekly_token_budget: Some(100_000),
            ..Default::default()
        };
        let turns = vec![
            turn(1, "anthropic/claude-fable-5", 20_000),
            turn(2, "anthropic/claude-sonnet-4.5", 30_000),
            turn(8, "anthropic/claude-fable-5", 99_000),
        ];
        let now = DateTime::parse_from_rfc3339("2026-07-04T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let report = build_report(None, None, &turns, &config, now);

        assert_eq!(report.fable.total_tokens, 20_000);
        assert_eq!(report.claude.total_tokens, 50_000);
        assert_eq!(report.other_claude.total_tokens, 30_000);
        assert_eq!(report.fable_cap_tokens, Some(50_000));
        assert_eq!(report.fable_share_of_claude().unwrap(), 0.4);
    }

    #[test]
    fn sunday_week_start_changes_window() {
        let config = FableUsageConfig {
            week_starts_on: "sunday".into(),
            ..Default::default()
        };
        let now = DateTime::parse_from_rfc3339("2026-07-04T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let report = build_report(None, None, &[], &config, now);

        assert_eq!(
            report.window_start,
            DateTime::parse_from_rfc3339("2026-06-28T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
    }

    #[test]
    fn footer_shows_configured_cap_when_available() {
        let config = FableUsageConfig {
            weekly_token_budget: Some(100_000),
            ..Default::default()
        };
        let now = DateTime::parse_from_rfc3339("2026-07-04T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let report = build_report(
            None,
            None,
            &[turn(1, "anthropic/claude-fable-5", 25_000)],
            &config,
            now,
        );

        assert_eq!(
            format_footer_suffix_for_report(&report),
            "Fable 25.0k / 50.0k wk (50%)"
        );
    }
}
