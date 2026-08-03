//! Last-mile shipping command group.

use super::*;
use crate::handoff::{
    collect_handoff_context, ensure_required_sections, handoff_system_prompt,
    render_fallback_markdown, render_handoff_prompt, should_refuse_cloud_handoff, HandoffBasis,
    HandoffContext, HANDOFF_CONTEXT_LIMIT_BYTES,
};
use crate::project_memory::project_index_freshness;
use crate::shipcheck::{
    collect_shipcheck, collect_shipcheck_with_tests, evaluate_ship_pr_readiness,
    quality_input_from_shipcheck, ShipReadiness, ShipReadinessStatus, ShipcheckSnapshot,
};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShipAction {
    Preview,
    Commit,
    Push,
    Pr,
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShipStaging {
    All,
    StagedOnly,
}

impl ShipStaging {
    fn label(self) -> &'static str {
        match self {
            ShipStaging::All => "all changes",
            ShipStaging::StagedOnly => "staged changes only",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShipArgs {
    action: ShipAction,
    run_tests: Option<bool>,
    allow_behind: bool,
    allow_cloud: bool,
    staging: Option<ShipStaging>,
    yes: bool,
    message: Option<String>,
    pr_base: Option<String>,
    pr_ready: bool,
    pr_title: Option<String>,
}

fn parse_ship_args(args: &str) -> Option<ShipArgs> {
    let mut parsed = ShipArgs {
        action: ShipAction::Preview,
        run_tests: None,
        allow_behind: false,
        allow_cloud: false,
        staging: None,
        yes: false,
        message: None,
        pr_base: None,
        pr_ready: false,
        pr_title: None,
    };
    let mut action_seen = false;
    let mut parts = args.split_whitespace().peekable();

    while let Some(part) = parts.next() {
        match part {
            "" | "preview" | "--dry-run" => {}
            "commit" => {
                if action_seen {
                    return None;
                }
                parsed.action = ShipAction::Commit;
                action_seen = true;
            }
            "push" => {
                if action_seen {
                    return None;
                }
                parsed.action = ShipAction::Push;
                action_seen = true;
            }
            "pr" => {
                if action_seen {
                    return None;
                }
                parsed.action = ShipAction::Pr;
                action_seen = true;
            }
            "status" | "checks" => {
                if action_seen {
                    return None;
                }
                parsed.action = ShipAction::Status;
                action_seen = true;
            }
            "--tests" => parsed.run_tests = Some(true),
            "--no-tests" => parsed.run_tests = Some(false),
            "--allow-behind" => parsed.allow_behind = true,
            "--cloud" => parsed.allow_cloud = true,
            "--ready" => parsed.pr_ready = true,
            "--draft" => parsed.pr_ready = false,
            "--base" => {
                let value = parts.next()?;
                if value.trim().is_empty() {
                    return None;
                }
                parsed.pr_base = Some(value.to_string());
            }
            p if p.starts_with("--base=") => {
                let value = p.strip_prefix("--base=")?;
                if value.trim().is_empty() {
                    return None;
                }
                parsed.pr_base = Some(value.to_string());
            }
            "--all" => {
                if parsed.staging.replace(ShipStaging::All).is_some() {
                    return None;
                }
            }
            "--staged-only" => {
                if parsed.staging.replace(ShipStaging::StagedOnly).is_some() {
                    return None;
                }
            }
            "--yes" | "-y" => parsed.yes = true,
            "--message" | "-m" => {
                let rest = parts.collect::<Vec<_>>().join(" ");
                if rest.trim().is_empty() {
                    return None;
                }
                parsed.message = Some(rest);
                break;
            }
            "--title" => {
                let rest = parts.collect::<Vec<_>>().join(" ");
                if rest.trim().is_empty() {
                    return None;
                }
                parsed.pr_title = Some(rest);
                break;
            }
            p if p.starts_with("--title=") => {
                let value = p.strip_prefix("--title=")?;
                if value.trim().is_empty() {
                    return None;
                }
                let rest = parts.collect::<Vec<_>>().join(" ");
                parsed.pr_title = Some(if rest.trim().is_empty() {
                    value.to_string()
                } else {
                    format!("{value} {rest}")
                });
                break;
            }
            _ => return None,
        }
    }

    Some(parsed)
}

pub(super) async fn cmd_ship(args: &str, state: &AppState) -> Result<()> {
    let Some(args) = parse_ship_args(args) else {
        println!(
            "  {DIM}Usage: /ship [preview|commit|push|pr|status] [--tests|--no-tests] [--all|--staged-only] [--allow-behind] [--cloud] [--yes] [-m <message>] [--base <branch>] [--ready] [--title <title>]{RESET}"
        );
        return Ok(());
    };

    if args.action != ShipAction::Commit && args.staging.is_some() {
        println!("  {DIM}Usage: staging flags only apply to /ship commit.{RESET}");
        return Ok(());
    }
    if matches!(
        args.action,
        ShipAction::Push | ShipAction::Pr | ShipAction::Status
    ) && args.message.is_some()
    {
        println!(
            "  {DIM}Usage: commit messages only apply to /ship preview or /ship commit.{RESET}"
        );
        return Ok(());
    }
    if args.action != ShipAction::Pr
        && (args.pr_base.is_some() || args.pr_title.is_some() || args.pr_ready)
    {
        println!("  {DIM}Usage: PR flags only apply to /ship pr.{RESET}");
        return Ok(());
    }
    if args.action == ShipAction::Commit && args.staging.is_none() {
        println!(
            "  {DIM}Usage: /ship commit --all | /ship commit --staged-only [--tests|--no-tests] [--yes] [-m <message>]{RESET}"
        );
        return Ok(());
    }

    let run_tests = args
        .run_tests
        .unwrap_or(matches!(args.action, ShipAction::Commit));
    let snapshot = if run_tests {
        collect_shipcheck_with_tests(&state.config.workspace_root, true)?
    } else {
        collect_shipcheck(&state.config.workspace_root)?
    };
    let freshness = if state.config.project_memory.enabled {
        Some(project_index_freshness(&state.config)?)
    } else {
        None
    };
    let readiness = if args.action == ShipAction::Push {
        evaluate_ship_push_readiness(&snapshot, args.allow_behind)
    } else if args.action == ShipAction::Pr {
        evaluate_ship_pr_readiness(&snapshot, args.allow_behind)
    } else if args.action == ShipAction::Status {
        evaluate_ship_status_readiness(&snapshot, args.allow_behind)
    } else {
        evaluate_ship_readiness(&snapshot, args.allow_behind)
    };

    println!(
        "  {DIM}ship{RESET}            {}",
        match args.action {
            ShipAction::Preview => "preview",
            ShipAction::Commit => "commit",
            ShipAction::Push => "push",
            ShipAction::Pr => "pr",
            ShipAction::Status => "status",
        }
    );
    print_shipcheck(&snapshot, freshness.as_ref());
    print_ship_readiness(&readiness);
    if state.config.scorecard.enabled {
        if let Ok(summary) = crate::scorecard::current_summary(&state.config.workspace_root) {
            if summary.turn_count > 0 {
                println!(
                    "  {DIM}scorecard{RESET}       {} turn(s) tracked on this branch",
                    summary.turn_count
                );
            }
        }
    }

    if args.action == ShipAction::Preview {
        match collect_handoff_context(&snapshot)? {
            Some(context) => {
                let draft = draft_ship_commit_message(
                    &snapshot,
                    &context,
                    freshness.as_ref(),
                    state,
                    args.allow_cloud,
                    args.message.as_deref(),
                )
                .await;
                print_ship_commit_draft(&draft);
            }
            None => println!("  {DIM}commitDraft{RESET}     none (no changes or ahead commits)"),
        }
        return Ok(());
    }

    match args.action {
        ShipAction::Commit => {
            run_ship_commit(args, snapshot, freshness.as_ref(), readiness, state).await
        }
        ShipAction::Push => run_ship_push(args, snapshot, readiness, state).await,
        ShipAction::Pr => run_ship_pr(args, snapshot, readiness, state).await,
        ShipAction::Status => run_ship_status(snapshot, readiness, state).await,
        ShipAction::Preview => Ok(()),
    }
}

async fn run_ship_commit(
    args: ShipArgs,
    snapshot: ShipcheckSnapshot,
    freshness: Option<&crate::project_memory::ProjectIndexFreshness>,
    readiness: ShipReadiness,
    state: &AppState,
) -> Result<()> {
    let staging = args.staging.expect("commit action requires staging mode");
    let commit_blockers = commit_specific_blockers(&snapshot, staging);
    for blocker in &commit_blockers {
        println!("  {RED}✗{RESET} {DIM}{blocker}{RESET}");
    }
    if readiness.status == ShipReadinessStatus::Blocked || !commit_blockers.is_empty() {
        println!("  {RED}✗{RESET} {DIM}commit not created{RESET}");
        return Ok(());
    }

    if staging == ShipStaging::All {
        let prompt = format!(
            "  {YELLOW}?{RESET} {DIM}Stage all working-tree changes with git add -A? [y/N]{RESET}"
        );
        if !args.yes && !confirm_ship_action(prompt).await? {
            println!("  {RED}✗{RESET} {DIM}ship commit cancelled before staging{RESET}");
            return Ok(());
        }
        run_git_capture(&state.config.workspace_root, &["add", "-A"])?;
    }

    let staged_snapshot = collect_shipcheck(&state.config.workspace_root)?;
    if staged_snapshot.staged_count() == 0 {
        println!("  {RED}✗{RESET} {DIM}commit not created: no staged changes{RESET}");
        return Ok(());
    }
    println!(
        "  {DIM}staging{RESET}         {} · {} staged file(s)",
        staging.label(),
        staged_snapshot.staged_count()
    );
    print_diff_stat("finalStagedDiff", &staged_snapshot.staged_diff_stat);

    let Some(context) = build_staged_ship_context(&staged_snapshot)? else {
        println!("  {RED}✗{RESET} {DIM}commit not created: staged diff is empty{RESET}");
        return Ok(());
    };
    let draft = draft_ship_commit_message(
        &staged_snapshot,
        &context,
        freshness,
        state,
        args.allow_cloud,
        args.message.as_deref(),
    )
    .await;
    print_ship_commit_draft(&draft);

    let prompt =
        format!("  {YELLOW}?{RESET} {DIM}Create git commit with this message? [y/N]{RESET}");
    if !args.yes && !confirm_ship_action(prompt).await? {
        println!("  {RED}✗{RESET} {DIM}ship commit cancelled before commit{RESET}");
        return Ok(());
    }

    let commit_hash = create_git_commit(&state.config.workspace_root, &draft.message)?;
    let record_path = write_ship_commit_record(
        &state.session_dir,
        &staged_snapshot,
        snapshot.test_status.as_ref(),
        staging,
        &draft.message,
        &commit_hash,
    )?;
    println!("  {GREEN}✓{RESET} {DIM}commit created:{RESET} {CYAN}{commit_hash}{RESET}");
    println!(
        "  {GREEN}✓{RESET} {DIM}ship record saved →{RESET} {}",
        record_path.display()
    );
    Ok(())
}

async fn run_ship_push(
    args: ShipArgs,
    snapshot: ShipcheckSnapshot,
    readiness: ShipReadiness,
    state: &AppState,
) -> Result<()> {
    if readiness.status == ShipReadinessStatus::Blocked {
        println!("  {RED}✗{RESET} {DIM}push not run{RESET}");
        return Ok(());
    }

    let target = match resolve_ship_push_target(&state.config.workspace_root, &snapshot) {
        Ok(target) => target,
        Err(e) => {
            println!("  {RED}✗{RESET} {DIM}push not run: {e}{RESET}");
            return Ok(());
        }
    };
    println!("  {DIM}pushTarget{RESET}      {}", target.description());

    let prompt = format!(
        "  {YELLOW}?{RESET} {DIM}Push {}? [y/N]{RESET}",
        target.description()
    );
    if !args.yes && !confirm_ship_action(prompt).await? {
        println!("  {RED}✗{RESET} {DIM}ship push cancelled{RESET}");
        return Ok(());
    }

    let output = execute_ship_push(&state.config.workspace_root, &target)?;
    let pushed_snapshot = collect_shipcheck(&state.config.workspace_root)?;
    let commit_hash = run_git_capture(
        &state.config.workspace_root,
        &["rev-parse", "--short", "HEAD"],
    )?
    .trim()
    .to_string();
    let record_path = write_ship_push_record(
        &state.session_dir,
        &pushed_snapshot,
        &target,
        &commit_hash,
        &output,
    )?;
    println!(
        "  {GREEN}✓{RESET} {DIM}pushed:{RESET} {CYAN}{}{RESET}",
        target.description()
    );
    println!(
        "  {GREEN}✓{RESET} {DIM}ship record saved →{RESET} {}",
        record_path.display()
    );
    Ok(())
}

async fn run_ship_pr(
    args: ShipArgs,
    snapshot: ShipcheckSnapshot,
    readiness: ShipReadiness,
    state: &AppState,
) -> Result<()> {
    if readiness.status == ShipReadinessStatus::Blocked {
        println!("  {RED}✗{RESET} {DIM}PR not created{RESET}");
        return Ok(());
    }

    let target = match resolve_ship_pr_target(
        &state.config.workspace_root,
        &snapshot,
        args.pr_base.as_deref(),
    ) {
        Ok(target) => target,
        Err(e) => {
            println!("  {RED}✗{RESET} {DIM}PR not created: {e}{RESET}");
            return Ok(());
        }
    };
    let commit_hash = run_git_capture(
        &state.config.workspace_root,
        &["rev-parse", "--short", "HEAD"],
    )?
    .trim()
    .to_string();
    let title = args
        .pr_title
        .as_deref()
        .and_then(normalize_commit_message)
        .unwrap_or_else(|| latest_commit_subject(&state.config.workspace_root));
    let commits = pr_commit_summary(&state.config.workspace_root, &target);
    let body = render_ship_pr_body(&snapshot, &target, &commit_hash, &commits);
    let command = build_gh_pr_command(&target, &title, &body, !args.pr_ready);

    println!("  {DIM}prTarget{RESET}        {}", target.description());
    println!("  {DIM}prTitle{RESET}         {title}");
    println!(
        "  {DIM}prMode{RESET}          {}",
        if args.pr_ready { "ready" } else { "draft" }
    );

    if !gh_cli_ready(&state.config.workspace_root) {
        println!("  {YELLOW}!{RESET} {DIM}GitHub CLI is unavailable or unauthenticated; run this manually:{RESET}");
        println!("    {}", shell_command_display(&command));
        let record_path = write_ship_pr_record(
            &state.session_dir,
            &ShipPrRecord {
                snapshot: &snapshot,
                target: &target,
                title: &title,
                body: &body,
                command: &command,
                url: None,
                status: "manual command printed",
            },
        )?;
        println!(
            "  {GREEN}✓{RESET} {DIM}ship record saved →{RESET} {}",
            record_path.display()
        );
        let quality = quality_input_from_shipcheck(&snapshot, args.allow_behind, false, false);
        super::scorecard::close_scorecard_pr(
            state,
            &title,
            None,
            "manual command printed",
            Some(quality),
            Some(&record_path),
        )?;
        return Ok(());
    }

    let prompt = format!(
        "  {YELLOW}?{RESET} {DIM}Create {} PR {}? [y/N]{RESET}",
        if args.pr_ready { "ready" } else { "draft" },
        target.description()
    );
    if !args.yes && !confirm_ship_action(prompt).await? {
        println!("  {RED}✗{RESET} {DIM}ship PR cancelled{RESET}");
        return Ok(());
    }

    let output = run_command_capture_combined(&state.config.workspace_root, &command)?;
    let url = extract_url(&output);
    let record_path = write_ship_pr_record(
        &state.session_dir,
        &ShipPrRecord {
            snapshot: &snapshot,
            target: &target,
            title: &title,
            body: &body,
            command: &command,
            url: url.as_deref(),
            status: "created",
        },
    )?;
    match &url {
        Some(url) => println!("  {GREEN}✓{RESET} {DIM}PR created:{RESET} {CYAN}{url}{RESET}"),
        None => println!("  {GREEN}✓{RESET} {DIM}PR created{RESET}"),
    }
    println!(
        "  {GREEN}✓{RESET} {DIM}ship record saved →{RESET} {}",
        record_path.display()
    );
    let quality = quality_input_from_shipcheck(&snapshot, args.allow_behind, true, url.is_some());
    super::scorecard::close_scorecard_pr(
        state,
        &title,
        url.as_deref(),
        "created",
        Some(quality),
        Some(&record_path),
    )?;
    Ok(())
}

async fn run_ship_status(
    snapshot: ShipcheckSnapshot,
    readiness: ShipReadiness,
    state: &AppState,
) -> Result<()> {
    if readiness.status == ShipReadinessStatus::Blocked {
        println!("  {RED}✗{RESET} {DIM}PR status not checked{RESET}");
        return Ok(());
    }

    let target = match resolve_ship_pr_target(&state.config.workspace_root, &snapshot, None) {
        Ok(target) => target,
        Err(e) => {
            println!("  {RED}✗{RESET} {DIM}PR status unavailable: {e}{RESET}");
            return Ok(());
        }
    };
    let command = build_gh_pr_status_command(&target);
    println!("  {DIM}prTarget{RESET}        {}", target.description());

    if !gh_cli_ready(&state.config.workspace_root) {
        println!("  {YELLOW}!{RESET} {DIM}GitHub CLI is unavailable or unauthenticated; run this manually:{RESET}");
        println!("    {}", shell_command_display(&command));
        return Ok(());
    }

    let output = run_command_capture_combined(&state.config.workspace_root, &command)?;
    match parse_ship_pr_status_json(&output)? {
        Some(status) => print_ship_pr_status(&status),
        None => {
            println!(
                "  {YELLOW}!{RESET} {DIM}No open PR found for `{}`.{RESET}",
                target.head_branch
            );
            println!("  {DIM}nextAction{RESET}      run /ship pr");
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShipPrTarget {
    remote: String,
    repo: String,
    head_branch: String,
    base_branch: String,
}

impl ShipPrTarget {
    fn description(&self) -> String {
        format!(
            "{}:{} -> {}:{}",
            self.repo, self.head_branch, self.repo, self.base_branch
        )
    }
}

fn resolve_ship_pr_target(
    workspace_root: &str,
    snapshot: &ShipcheckSnapshot,
    explicit_base: Option<&str>,
) -> Result<ShipPrTarget> {
    let upstream = snapshot
        .branch
        .upstream
        .as_deref()
        .ok_or_else(|| anyhow!("branch has no upstream; run /ship push first"))?;
    let (remote, remote_branch) = upstream
        .split_once('/')
        .ok_or_else(|| anyhow!("upstream `{upstream}` does not include a remote"))?;
    let remote_url = run_git_capture(workspace_root, &["remote", "get-url", remote])?;
    let repo = parse_github_repo(remote_url.trim())
        .ok_or_else(|| anyhow!("remote `{remote}` is not a GitHub URL"))?;
    let base_branch = explicit_base
        .and_then(normalize_commit_message)
        .unwrap_or_else(|| {
            discover_default_branch(workspace_root, remote).unwrap_or_else(|| "main".into())
        });

    Ok(ShipPrTarget {
        remote: remote.to_string(),
        repo,
        head_branch: remote_branch.to_string(),
        base_branch,
    })
}

fn parse_github_repo(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches(".git");
    if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        return normalize_repo_slug(rest);
    }
    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        return normalize_repo_slug(rest);
    }
    if let Some(rest) = trimmed.strip_prefix("ssh://git@github.com/") {
        return normalize_repo_slug(rest);
    }
    None
}

fn normalize_repo_slug(value: &str) -> Option<String> {
    let mut parts = value.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

fn discover_default_branch(workspace_root: &str, remote: &str) -> Option<String> {
    let output = run_git_capture(
        workspace_root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            &format!("refs/remotes/{remote}/HEAD"),
        ],
    )
    .ok()?;
    let trimmed = output.trim();
    trimmed
        .strip_prefix(&format!("{remote}/"))
        .filter(|branch| !branch.is_empty())
        .map(str::to_string)
}

fn latest_commit_subject(workspace_root: &str) -> String {
    run_git_capture(workspace_root, &["log", "-1", "--pretty=%s"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Ship Albatross changes".into())
}

fn pr_commit_summary(workspace_root: &str, target: &ShipPrTarget) -> String {
    let base_ref = format!("{}/{}", target.remote, target.base_branch);
    let range = format!("{base_ref}..HEAD");
    run_git_capture(
        workspace_root,
        &["log", "--oneline", "--max-count=20", &range],
    )
    .ok()
    .filter(|s| !s.trim().is_empty())
    .or_else(|| run_git_capture(workspace_root, &["log", "--oneline", "--max-count=5"]).ok())
    .unwrap_or_else(|| "No commit summary available.".into())
}

fn render_ship_pr_body(
    snapshot: &ShipcheckSnapshot,
    target: &ShipPrTarget,
    commit_hash: &str,
    commits: &str,
) -> String {
    let mut out = String::new();
    out.push_str("## Summary\n\n");
    out.push_str("- Prepared by Albatross `/ship pr`.\n");
    out.push_str(&format!("- Head: `{}`\n", target.head_branch));
    out.push_str(&format!("- Base: `{}`\n", target.base_branch));
    out.push_str(&format!("- Commit: `{commit_hash}`\n\n"));
    out.push_str("## Commits\n\n```text\n");
    out.push_str(commits.trim());
    out.push_str("\n```\n\n");
    out.push_str("## Ship Status\n\n");
    out.push_str(&format!("- Branch: `{}`\n", snapshot.branch_label()));
    out.push_str(&format!(
        "- Uncommitted files: {}\n",
        snapshot.staged_count() + snapshot.unstaged_count() + snapshot.untracked_count()
    ));
    out.push_str(&format!("- Conflicts: {}\n", snapshot.conflict_count()));
    out.push_str("\n## Tests\n\n");
    match &snapshot.test_status {
        Some(status) => {
            out.push_str(&format!(
                "- Framework: `{}`\n- Total: {}\n- Passed: {}\n- Failed: {}\n- Skipped: {}\n- Exit code: {}\n",
                status.framework,
                status.total,
                status.passed,
                status.failed,
                status.skipped,
                status.exit_code
            ));
            if let Some(error) = &status.error {
                out.push_str(&format!("- Error: `{error}`\n"));
            }
        }
        None => out.push_str("- Not run by `/ship pr`.\n"),
    }
    out
}

fn build_gh_pr_command(target: &ShipPrTarget, title: &str, body: &str, draft: bool) -> Vec<String> {
    let mut args = vec![
        "gh".to_string(),
        "pr".to_string(),
        "create".to_string(),
        "--repo".to_string(),
        target.repo.clone(),
        "--base".to_string(),
        target.base_branch.clone(),
        "--head".to_string(),
        target.head_branch.clone(),
        "--title".to_string(),
        title.to_string(),
        "--body".to_string(),
        body.to_string(),
    ];
    if draft {
        args.push("--draft".into());
    }
    args
}

fn build_gh_pr_status_command(target: &ShipPrTarget) -> Vec<String> {
    vec![
        "gh".to_string(),
        "pr".to_string(),
        "list".to_string(),
        "--repo".to_string(),
        target.repo.clone(),
        "--head".to_string(),
        target.head_branch.clone(),
        "--state".to_string(),
        "open".to_string(),
        "--json".to_string(),
        "number,url,title,state,headRefName,baseRefName,reviewDecision,mergeable,statusCheckRollup"
            .to_string(),
        "--limit".to_string(),
        "1".to_string(),
    ]
}

fn gh_cli_ready(workspace_root: &str) -> bool {
    command_success(workspace_root, &["gh", "--version"])
        && command_success(workspace_root, &["gh", "auth", "status"])
}

fn command_success(workspace_root: &str, args: &[&str]) -> bool {
    let Some((program, rest)) = args.split_first() else {
        return false;
    };
    Command::new(program)
        .current_dir(workspace_root)
        .args(rest)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn run_command_capture_combined(workspace_root: &str, args: &[String]) -> Result<String> {
    let Some((program, rest)) = args.split_first() else {
        return Err(anyhow!("empty command"));
    };
    let output = Command::new(program)
        .current_dir(workspace_root)
        .args(rest)
        .output()
        .map_err(|e| anyhow!("failed to run {program}: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        let detail = stderr.trim();
        return Err(anyhow!(
            "{} failed{}",
            shell_command_display(args),
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        ));
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

fn shell_command_display(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(arg: &str) -> String {
    if arg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '='))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

fn extract_url(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .find(|part| part.starts_with("https://") || part.starts_with("http://"))
        .map(|part| {
            part.trim_matches(|c: char| c == ')' || c == '(')
                .to_string()
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShipPrStatus {
    number: u64,
    url: String,
    title: String,
    state: String,
    head_branch: String,
    base_branch: String,
    review_decision: Option<String>,
    mergeable: Option<String>,
    checks: Vec<ShipPrCheckStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShipPrCheckStatus {
    name: String,
    workflow: Option<String>,
    status: String,
    conclusion: Option<String>,
    details_url: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ShipPrCheckCounts {
    success: usize,
    failing: usize,
    pending: usize,
    skipped: usize,
}

impl ShipPrCheckCounts {
    fn total(self) -> usize {
        self.success + self.failing + self.pending + self.skipped
    }
}

fn parse_ship_pr_status_json(output: &str) -> Result<Option<ShipPrStatus>> {
    let value: serde_json::Value = serde_json::from_str(output.trim())
        .map_err(|e| anyhow!("failed to parse gh PR status JSON: {e}"))?;
    let Some(pr) = value.as_array().and_then(|items| items.first()) else {
        return Ok(None);
    };

    let checks = pr
        .get("statusCheckRollup")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .map(|item| ShipPrCheckStatus {
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
        .unwrap_or_default();

    Ok(Some(ShipPrStatus {
        number: pr
            .get("number")
            .and_then(|value| value.as_u64())
            .unwrap_or(0),
        url: json_string(pr, "url").unwrap_or_default(),
        title: json_string(pr, "title").unwrap_or_else(|| "Untitled PR".into()),
        state: json_string(pr, "state").unwrap_or_else(|| "UNKNOWN".into()),
        head_branch: json_string(pr, "headRefName").unwrap_or_default(),
        base_branch: json_string(pr, "baseRefName").unwrap_or_default(),
        review_decision: json_string(pr, "reviewDecision"),
        mergeable: json_string(pr, "mergeable"),
        checks,
    }))
}

fn json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn ship_pr_check_counts(checks: &[ShipPrCheckStatus]) -> ShipPrCheckCounts {
    let mut counts = ShipPrCheckCounts::default();
    for check in checks {
        match ship_pr_check_bucket(check) {
            ShipPrCheckBucket::Success => counts.success += 1,
            ShipPrCheckBucket::Failing => counts.failing += 1,
            ShipPrCheckBucket::Pending => counts.pending += 1,
            ShipPrCheckBucket::Skipped => counts.skipped += 1,
        }
    }
    counts
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShipPrCheckBucket {
    Success,
    Failing,
    Pending,
    Skipped,
}

fn ship_pr_check_bucket(check: &ShipPrCheckStatus) -> ShipPrCheckBucket {
    let conclusion = check.conclusion.as_deref().map(str::to_ascii_uppercase);
    let status = check.status.to_ascii_uppercase();

    if matches!(conclusion.as_deref(), Some("SUCCESS")) || status == "SUCCESS" {
        ShipPrCheckBucket::Success
    } else if matches!(
        conclusion.as_deref(),
        Some("FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "ERROR")
    ) || matches!(status.as_str(), "FAILURE" | "ERROR")
    {
        ShipPrCheckBucket::Failing
    } else if matches!(conclusion.as_deref(), Some("SKIPPED" | "NEUTRAL")) {
        ShipPrCheckBucket::Skipped
    } else {
        ShipPrCheckBucket::Pending
    }
}

fn print_ship_pr_status(status: &ShipPrStatus) {
    let counts = ship_pr_check_counts(&status.checks);
    println!(
        "  {DIM}pr{RESET}              #{} {}",
        status.number, status.title
    );
    if !status.url.is_empty() {
        println!("  {DIM}url{RESET}             {CYAN}{}{RESET}", status.url);
    }
    println!(
        "  {DIM}branches{RESET}        {} -> {}",
        status.head_branch, status.base_branch
    );
    println!("  {DIM}state{RESET}           {}", status.state);
    println!(
        "  {DIM}review{RESET}          {}",
        status.review_decision.as_deref().unwrap_or("none")
    );
    println!(
        "  {DIM}mergeable{RESET}       {}",
        status.mergeable.as_deref().unwrap_or("unknown")
    );

    if counts.total() == 0 {
        println!("  {DIM}checks{RESET}          none reported");
        println!("  {DIM}nextAction{RESET}      wait for GitHub to report checks");
        return;
    }

    println!(
        "  {DIM}checks{RESET}          {} success · {} failing · {} pending · {} skipped",
        counts.success, counts.failing, counts.pending, counts.skipped
    );
    for check in status.checks.iter().take(8) {
        println!(
            "    {} {}",
            ship_pr_check_marker(ship_pr_check_bucket(check)),
            ship_pr_check_line(check)
        );
    }
    if status.checks.len() > 8 {
        println!("    ... {} more check(s)", status.checks.len() - 8);
    }
    println!(
        "  {DIM}nextAction{RESET}      {}",
        ship_pr_status_next_action(status, counts)
    );
}

fn ship_pr_check_marker(bucket: ShipPrCheckBucket) -> &'static str {
    match bucket {
        ShipPrCheckBucket::Success => "✓",
        ShipPrCheckBucket::Failing => "✗",
        ShipPrCheckBucket::Pending => "...",
        ShipPrCheckBucket::Skipped => "-",
    }
}

fn ship_pr_check_line(check: &ShipPrCheckStatus) -> String {
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

fn ship_pr_status_next_action(status: &ShipPrStatus, counts: ShipPrCheckCounts) -> &'static str {
    if counts.failing > 0 {
        "inspect failing checks, fix locally, then /ship commit and /ship push"
    } else if counts.pending > 0 {
        "wait for pending checks, then rerun /ship status"
    } else if status.review_decision.as_deref() == Some("CHANGES_REQUESTED") {
        "address requested changes, then /ship commit and /ship push"
    } else if status.review_decision.as_deref() == Some("REVIEW_REQUIRED") {
        "request review or mark ready when the draft is complete"
    } else {
        "merge when ready"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShipPushTarget {
    remote: String,
    local_branch: String,
    remote_branch: String,
    set_upstream: bool,
}

impl ShipPushTarget {
    fn description(&self) -> String {
        if self.set_upstream {
            format!(
                "{} -> {}/{} and set upstream",
                self.local_branch, self.remote, self.remote_branch
            )
        } else {
            format!(
                "{} -> {}/{}",
                self.local_branch, self.remote, self.remote_branch
            )
        }
    }
}

fn resolve_ship_push_target(
    workspace_root: &str,
    snapshot: &ShipcheckSnapshot,
) -> Result<ShipPushTarget> {
    let branch = snapshot
        .branch
        .head
        .as_deref()
        .filter(|head| !head.is_empty() && *head != "(detached)")
        .ok_or_else(|| anyhow!("current HEAD is detached or unknown"))?;

    if let Some(upstream) = snapshot.branch.upstream.as_deref() {
        let (remote, branch_name) = upstream
            .split_once('/')
            .ok_or_else(|| anyhow!("upstream `{upstream}` does not include a remote"))?;
        return Ok(ShipPushTarget {
            remote: remote.to_string(),
            local_branch: branch.to_string(),
            remote_branch: branch_name.to_string(),
            set_upstream: false,
        });
    }

    run_git_capture(workspace_root, &["remote", "get-url", "origin"])?;
    Ok(ShipPushTarget {
        remote: "origin".into(),
        local_branch: branch.to_string(),
        remote_branch: branch.to_string(),
        set_upstream: true,
    })
}

fn execute_ship_push(workspace_root: &str, target: &ShipPushTarget) -> Result<String> {
    if target.set_upstream {
        run_git_capture_combined(
            workspace_root,
            &["push", "-u", &target.remote, &target.local_branch],
        )
    } else {
        run_git_capture_combined(workspace_root, &["push"])
    }
}

fn commit_specific_blockers(snapshot: &ShipcheckSnapshot, staging: ShipStaging) -> Vec<String> {
    match staging {
        ShipStaging::All if snapshot.files.is_empty() => {
            vec!["no working-tree changes to stage for commit".into()]
        }
        ShipStaging::StagedOnly if snapshot.staged_count() == 0 => {
            vec!["no staged changes to commit; stage files first or use --all".into()]
        }
        _ => Vec::new(),
    }
}

async fn confirm_ship_action(prompt: String) -> Result<bool> {
    let answer = plain_read_line(format!("{prompt}\n  {YELLOW}? {RESET}")).await?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShipCommitDraft {
    message: String,
    note: Option<String>,
}

async fn draft_ship_commit_message(
    snapshot: &ShipcheckSnapshot,
    context: &crate::handoff::HandoffContext,
    freshness: Option<&crate::project_memory::ProjectIndexFreshness>,
    state: &AppState,
    allow_cloud: bool,
    explicit_message: Option<&str>,
) -> ShipCommitDraft {
    if let Some(message) = explicit_message.and_then(normalize_commit_message) {
        return ShipCommitDraft {
            message,
            note: Some("using explicit commit message".into()),
        };
    }

    if should_refuse_cloud_handoff(state.backend.name, allow_cloud) {
        let fallback = render_fallback_markdown(context, snapshot, freshness, None);
        return ShipCommitDraft {
            message: extract_markdown_section(&fallback, "Commit Message")
                .unwrap_or_else(|| fallback_commit_message(context)),
            note: Some(
                "cloud provider skipped for diff privacy; pass --cloud to draft with it".into(),
            ),
        };
    }

    let messages = vec![
        ChatMessage::System {
            content: handoff_system_prompt(),
        },
        ChatMessage::User {
            content: render_handoff_prompt(context, freshness).into(),
        },
    ];
    let req = ChatRequest {
        model: &state.model,
        messages: &messages,
        tools: None,
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: false,
        }),
        max_tokens: Some(500),
        prompt_cache_key: None,
        effort: None,
    };
    let mut draft = String::new();
    let result = stream_chat(&state.http, &state.backend, &req, None, |chunk| {
        if let Some(choice) = chunk.choices.first() {
            if let Some(content) = &choice.delta.content {
                draft.push_str(content);
            }
        }
    })
    .await;

    let (body, note) = match result {
        Ok(_) if !draft.trim().is_empty() => (ensure_required_sections(&draft), None),
        Ok(_) => (
            render_fallback_markdown(context, snapshot, freshness, Some("empty model response")),
            Some("model draft was empty; using deterministic fallback".into()),
        ),
        Err(e) => (
            render_fallback_markdown(context, snapshot, freshness, Some(&e.to_string())),
            Some(format!(
                "model draft failed; using deterministic fallback: {e}"
            )),
        ),
    };

    ShipCommitDraft {
        message: extract_markdown_section(&body, "Commit Message")
            .unwrap_or_else(|| fallback_commit_message(context)),
        note,
    }
}

fn fallback_commit_message(context: &crate::handoff::HandoffContext) -> String {
    match context.basis {
        crate::handoff::HandoffBasis::DirtyTree => {
            "feat: prepare local working tree handoff".into()
        }
        crate::handoff::HandoffBasis::AheadOfUpstream => "feat: summarize branch handoff".into(),
    }
}

fn normalize_commit_message(message: &str) -> Option<String> {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn build_staged_ship_context(snapshot: &ShipcheckSnapshot) -> Result<Option<HandoffContext>> {
    if snapshot.staged_count() == 0 {
        return Ok(None);
    }
    let diff = run_git_capture(&snapshot.workspace_root, &["diff", "--cached", "--"])?;
    if diff.trim().is_empty() {
        return Ok(None);
    }

    let mut content = format!(
        "# Handoff Source Context\n\n## Basis\n\nStaged changes for commit\n\n## Shipcheck\n\n- Workspace: `{}`\n- Git root: `{}`\n- Branch: `{}`\n- Staged files: {}\n- Unstaged files: {}\n- Untracked files: {}\n- Conflicts: {}\n\n## Staged Diff\n\n```diff\n",
        snapshot.workspace_root,
        snapshot.git_root,
        snapshot.branch_label(),
        snapshot.staged_count(),
        snapshot.unstaged_count(),
        snapshot.untracked_count(),
        snapshot.conflict_count()
    );
    content.push_str(&truncate_for_ship_context(diff.trim()));
    content.push_str("\n```\n");

    Ok(Some(HandoffContext {
        basis: HandoffBasis::DirtyTree,
        content,
        truncated: diff.len() > HANDOFF_CONTEXT_LIMIT_BYTES,
    }))
}

fn truncate_for_ship_context(text: &str) -> String {
    if text.len() <= HANDOFF_CONTEXT_LIMIT_BYTES {
        return text.to_string();
    }
    let marker = "\n\n[... staged commit context truncated ...]";
    let max_prefix = HANDOFF_CONTEXT_LIMIT_BYTES.saturating_sub(marker.len());
    let mut end = max_prefix.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &text[..end], marker)
}

fn create_git_commit(workspace_root: &str, message: &str) -> Result<String> {
    let message = normalize_commit_message(message)
        .ok_or_else(|| anyhow!("commit message cannot be empty"))?;
    run_git_capture(workspace_root, &["commit", "-m", &message])?;
    Ok(
        run_git_capture(workspace_root, &["rev-parse", "--short", "HEAD"])?
            .trim()
            .to_string(),
    )
}

fn run_git_capture(workspace_root: &str, args: &[&str]) -> Result<String> {
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

fn run_git_capture_combined(workspace_root: &str, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(args)
        .output()
        .map_err(|e| anyhow!("failed to run git: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
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

fn default_ship_record_path(session_dir: &str) -> PathBuf {
    Path::new(session_dir).join("ship").join(format!(
        "{}.md",
        Utc::now().format("%Y-%m-%dT%H-%M-%S-%3fZ")
    ))
}

fn write_ship_commit_record(
    session_dir: &str,
    staged_snapshot: &ShipcheckSnapshot,
    test_status: Option<&crate::shipcheck::TestStatus>,
    staging: ShipStaging,
    message: &str,
    commit_hash: &str,
) -> Result<PathBuf> {
    let path = default_ship_record_path(session_dir);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let mut out = String::new();
    out.push_str("# Albatross Ship Commit\n\n");
    out.push_str(&format!("Generated: {}\n\n", Utc::now().to_rfc3339()));
    out.push_str("## Git\n\n");
    out.push_str(&format!("- Branch: `{}`\n", staged_snapshot.branch_label()));
    out.push_str(&format!("- Commit: `{commit_hash}`\n"));
    out.push_str(&format!("- Staging: `{}`\n", staging.label()));
    out.push_str(&format!(
        "- Staged files: {}\n- Unstaged files left: {}\n- Untracked files left: {}\n\n",
        staged_snapshot.staged_count(),
        staged_snapshot.unstaged_count(),
        staged_snapshot.untracked_count()
    ));
    out.push_str("## Commit Message\n\n```text\n");
    out.push_str(message.trim());
    out.push_str("\n```\n\n");
    out.push_str("## Tests\n\n");
    match test_status {
        Some(status) => {
            out.push_str(&format!(
                "- Framework: `{}`\n- Total: {}\n- Passed: {}\n- Failed: {}\n- Skipped: {}\n- Exit code: {}\n",
                status.framework,
                status.total,
                status.passed,
                status.failed,
                status.skipped,
                status.exit_code
            ));
            if let Some(error) = &status.error {
                out.push_str(&format!("- Error: `{error}`\n"));
            }
        }
        None => out.push_str("- Tests not run for this ship command.\n"),
    }
    out.push_str("\n## Final Staged Diff Stat\n\n");
    if staged_snapshot.staged_diff_stat.trim().is_empty() {
        out.push_str("No staged diff stat captured.\n");
    } else {
        out.push_str("```text\n");
        out.push_str(staged_snapshot.staged_diff_stat.trim());
        out.push_str("\n```\n");
    }

    fs::write(&path, out)?;
    Ok(path)
}

fn write_ship_push_record(
    session_dir: &str,
    snapshot: &ShipcheckSnapshot,
    target: &ShipPushTarget,
    commit_hash: &str,
    push_output: &str,
) -> Result<PathBuf> {
    let path = default_ship_record_path(session_dir);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let mut out = String::new();
    out.push_str("# Albatross Ship Push\n\n");
    out.push_str(&format!("Generated: {}\n\n", Utc::now().to_rfc3339()));
    out.push_str("## Git\n\n");
    out.push_str(&format!("- Branch: `{}`\n", snapshot.branch_label()));
    out.push_str(&format!("- Commit: `{commit_hash}`\n"));
    out.push_str(&format!("- Remote: `{}`\n", target.remote));
    out.push_str(&format!("- Local branch: `{}`\n", target.local_branch));
    out.push_str(&format!("- Remote branch: `{}`\n", target.remote_branch));
    out.push_str(&format!("- Set upstream: `{}`\n", target.set_upstream));
    out.push_str(&format!(
        "- Uncommitted files left: {}\n- Untracked files left: {}\n\n",
        snapshot.staged_count() + snapshot.unstaged_count(),
        snapshot.untracked_count()
    ));
    out.push_str("## Push Output\n\n");
    if push_output.trim().is_empty() {
        out.push_str("No output captured.\n");
    } else {
        out.push_str("```text\n");
        out.push_str(push_output.trim());
        out.push_str("\n```\n");
    }

    fs::write(&path, out)?;
    Ok(path)
}

struct ShipPrRecord<'a> {
    snapshot: &'a ShipcheckSnapshot,
    target: &'a ShipPrTarget,
    title: &'a str,
    body: &'a str,
    command: &'a [String],
    url: Option<&'a str>,
    status: &'a str,
}

fn write_ship_pr_record(session_dir: &str, record: &ShipPrRecord<'_>) -> Result<PathBuf> {
    let path = default_ship_record_path(session_dir);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let mut out = String::new();
    out.push_str("# Albatross Ship PR\n\n");
    out.push_str(&format!("Generated: {}\n\n", Utc::now().to_rfc3339()));
    out.push_str("## GitHub\n\n");
    out.push_str(&format!("- Status: `{}`\n", record.status));
    out.push_str(&format!("- Repository: `{}`\n", record.target.repo));
    out.push_str(&format!("- Head: `{}`\n", record.target.head_branch));
    out.push_str(&format!("- Base: `{}`\n", record.target.base_branch));
    if let Some(url) = record.url {
        out.push_str(&format!("- URL: {url}\n"));
    }
    out.push_str(&format!(
        "- Uncommitted files left: {}\n\n",
        record.snapshot.staged_count()
            + record.snapshot.unstaged_count()
            + record.snapshot.untracked_count()
    ));
    out.push_str("## Command\n\n```bash\n");
    out.push_str(&shell_command_display(record.command));
    out.push_str("\n```\n\n");
    out.push_str("## Title\n\n");
    out.push_str(record.title.trim());
    out.push_str("\n\n## Body\n\n");
    out.push_str(record.body.trim());
    out.push('\n');

    fs::write(&path, out)?;
    Ok(path)
}

fn extract_markdown_section(markdown: &str, section: &str) -> Option<String> {
    let mut in_section = false;
    let mut out = String::new();

    for line in markdown.lines() {
        if markdown_heading_text(line).is_some_and(|heading| heading.eq_ignore_ascii_case(section))
        {
            in_section = true;
            continue;
        }
        if in_section && markdown_heading_text(line).is_some() {
            break;
        }
        if in_section {
            out.push_str(line);
            out.push('\n');
        }
    }

    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn markdown_heading_text(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let text = trimmed.trim_start_matches('#').trim();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn evaluate_ship_readiness(snapshot: &ShipcheckSnapshot, allow_behind: bool) -> ShipReadiness {
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();

    if snapshot.conflict_count() > 0 {
        blockers.push(format!(
            "{} conflicted file(s) must be resolved",
            snapshot.conflict_count()
        ));
    }
    if snapshot.branch.behind > 0 && !allow_behind {
        blockers.push(format!(
            "branch is behind upstream by {} commit(s); pull/rebase or pass --allow-behind",
            snapshot.branch.behind
        ));
    }
    if snapshot.is_clean() && snapshot.branch.ahead == 0 {
        blockers.push("nothing to ship: no working-tree changes and no ahead commits".into());
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
    } else {
        warnings.push("tests were not run; use /ship --tests before committing".into());
    }
    if snapshot.branch.upstream.is_none() {
        warnings.push("no upstream configured; push/PR phases will need a remote target".into());
    }
    if snapshot.untracked_count() > 0 {
        warnings.push(format!(
            "{} untracked file(s) need an explicit staging decision",
            snapshot.untracked_count()
        ));
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

fn evaluate_ship_push_readiness(snapshot: &ShipcheckSnapshot, allow_behind: bool) -> ShipReadiness {
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();

    if snapshot.branch.head.as_deref() == Some("(detached)") || snapshot.branch.head.is_none() {
        blockers.push("cannot push from a detached or unknown HEAD".into());
    }
    if snapshot.conflict_count() > 0 {
        blockers.push(format!(
            "{} conflicted file(s) must be resolved before push",
            snapshot.conflict_count()
        ));
    }
    if snapshot.branch.behind > 0 && !allow_behind {
        blockers.push(format!(
            "branch is behind upstream by {} commit(s); pull/rebase or pass --allow-behind",
            snapshot.branch.behind
        ));
    }
    if snapshot.branch.upstream.is_some() && snapshot.branch.ahead == 0 {
        blockers.push("nothing to push: branch is not ahead of upstream".into());
    }
    if snapshot.branch.upstream.is_none() {
        warnings.push("no upstream configured; /ship push will use origin and set upstream".into());
    }
    if snapshot.staged_count() + snapshot.unstaged_count() > 0 || snapshot.untracked_count() > 0 {
        warnings.push(format!(
            "{} uncommitted file(s) are not part of this push",
            snapshot.staged_count() + snapshot.unstaged_count() + snapshot.untracked_count()
        ));
    }
    if snapshot.test_status.is_none() {
        warnings.push("tests were not run for this push; use /ship push --tests if desired".into());
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

fn evaluate_ship_status_readiness(
    snapshot: &ShipcheckSnapshot,
    allow_behind: bool,
) -> ShipReadiness {
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();

    if snapshot.branch.head.as_deref() == Some("(detached)") || snapshot.branch.head.is_none() {
        blockers.push("cannot resolve PR status from a detached or unknown HEAD".into());
    }
    if snapshot.branch.upstream.is_none() {
        blockers.push("branch has no upstream; run /ship push first".into());
    }
    if snapshot.branch.ahead > 0 {
        warnings.push(format!(
            "local branch has {} unpushed commit(s); PR checks may be stale",
            snapshot.branch.ahead
        ));
    }
    if snapshot.branch.behind > 0 && !allow_behind {
        warnings.push(format!(
            "branch is behind upstream by {} commit(s); status may not reflect local checkout",
            snapshot.branch.behind
        ));
    }
    if snapshot.conflict_count() > 0 {
        warnings.push(format!(
            "{} conflicted file(s) are present locally",
            snapshot.conflict_count()
        ));
    }
    if snapshot.staged_count() + snapshot.unstaged_count() > 0 || snapshot.untracked_count() > 0 {
        warnings.push(format!(
            "{} uncommitted file(s) are not reflected in PR checks",
            snapshot.staged_count() + snapshot.unstaged_count() + snapshot.untracked_count()
        ));
    }
    if let Some(tests) = &snapshot.test_status {
        if tests.failed > 0 || tests.exit_code != 0 {
            warnings.push(format!(
                "local tests failed: {} failed, exit code {}",
                tests.failed, tests.exit_code
            ));
        }
        if let Some(error) = &tests.error {
            warnings.push(format!("local test execution error: {error}"));
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

fn print_ship_readiness(readiness: &ShipReadiness) {
    let (label, color) = match readiness.status {
        ShipReadinessStatus::Ready => ("ready", GREEN),
        ShipReadinessStatus::NeedsReview => ("needs review", YELLOW),
        ShipReadinessStatus::Blocked => ("blocked", RED),
    };
    println!("  {DIM}verdict{RESET}         {color}{label}{RESET}");
    for blocker in &readiness.blockers {
        println!("  {RED}✗{RESET} {DIM}{blocker}{RESET}");
    }
    for warning in &readiness.warnings {
        println!("  {YELLOW}!{RESET} {DIM}{warning}{RESET}");
    }
}

fn print_ship_commit_draft(draft: &ShipCommitDraft) {
    println!("  {DIM}commitDraft{RESET}");
    for line in draft.message.lines() {
        if line.trim().is_empty() {
            println!();
        } else {
            println!("    {line}");
        }
    }
    if let Some(note) = &draft.note {
        println!("  {DIM}{note}{RESET}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shipcheck::{GitBranchState, GitFileKind, GitFileState, TestStatus};
    use std::fs;
    use std::path::Path;
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

    fn init_repo(dir: &Path) {
        git(dir, &["init"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test User"]);
        fs::write(dir.join("README.md"), "hello\n").unwrap();
        git(dir, &["add", "README.md"]);
        git(dir, &["commit", "-m", "initial"]);
    }

    fn sample_snapshot(
        files: Vec<GitFileState>,
        test_status: Option<TestStatus>,
    ) -> ShipcheckSnapshot {
        ShipcheckSnapshot {
            workspace_root: "/tmp/workspace".into(),
            git_root: "/tmp/workspace".into(),
            branch: GitBranchState {
                head: Some("feature".into()),
                upstream: Some("origin/feature".into()),
                ..Default::default()
            },
            files,
            staged_diff_stat: String::new(),
            unstaged_diff_stat: String::new(),
            test_status,
        }
    }

    fn tracked_change(path: &str) -> GitFileState {
        GitFileState {
            path: path.into(),
            original_path: None,
            staged: Some('.'),
            unstaged: Some('M'),
            kind: GitFileKind::Tracked,
        }
    }

    fn passing_tests() -> TestStatus {
        TestStatus {
            framework: "cargo".into(),
            total: 3,
            passed: 3,
            failed: 0,
            skipped: 0,
            exit_code: 0,
            error: None,
        }
    }

    #[test]
    fn parses_ship_preview_args() {
        let args = parse_ship_args("--tests --allow-behind --cloud").unwrap();
        assert_eq!(args.action, ShipAction::Preview);
        assert_eq!(args.run_tests, Some(true));
        assert!(args.allow_behind);
        assert!(args.allow_cloud);

        let args = parse_ship_args("preview --dry-run --no-tests").unwrap();
        assert_eq!(args.action, ShipAction::Preview);
        assert_eq!(args.run_tests, Some(false));

        assert_eq!(
            parse_ship_args("commit").unwrap().action,
            ShipAction::Commit
        );
        assert!(parse_ship_args("commit push").is_none());
        assert!(parse_ship_args("--unknown").is_none());
    }

    #[test]
    fn parses_ship_commit_staging_and_message_args() {
        let args = parse_ship_args("commit --all --yes -m feat: add ship commit").unwrap();
        assert_eq!(args.action, ShipAction::Commit);
        assert_eq!(args.staging, Some(ShipStaging::All));
        assert!(args.yes);
        assert_eq!(args.message.as_deref(), Some("feat: add ship commit"));

        let args = parse_ship_args("commit --staged-only --no-tests").unwrap();
        assert_eq!(args.staging, Some(ShipStaging::StagedOnly));
        assert_eq!(args.run_tests, Some(false));

        assert!(parse_ship_args("commit --all --staged-only").is_none());
        assert!(parse_ship_args("commit --message").is_none());
    }

    #[test]
    fn parses_ship_push_args() {
        let args = parse_ship_args("push --yes --allow-behind --tests").unwrap();
        assert_eq!(args.action, ShipAction::Push);
        assert!(args.yes);
        assert!(args.allow_behind);
        assert_eq!(args.run_tests, Some(true));

        assert!(parse_ship_args("push --all").is_some());
    }

    #[test]
    fn parses_ship_pr_args() {
        let args =
            parse_ship_args("pr --base main --ready --yes --title Add ship PR flow").unwrap();
        assert_eq!(args.action, ShipAction::Pr);
        assert_eq!(args.pr_base.as_deref(), Some("main"));
        assert!(args.pr_ready);
        assert!(args.yes);
        assert_eq!(args.pr_title.as_deref(), Some("Add ship PR flow"));

        let args = parse_ship_args("pr --base=develop --draft --title=Draft title").unwrap();
        assert_eq!(args.pr_base.as_deref(), Some("develop"));
        assert!(!args.pr_ready);
        assert_eq!(args.pr_title.as_deref(), Some("Draft title"));
    }

    #[test]
    fn parses_ship_status_args() {
        let args = parse_ship_args("status --tests --allow-behind").unwrap();
        assert_eq!(args.action, ShipAction::Status);
        assert_eq!(args.run_tests, Some(true));
        assert!(args.allow_behind);

        let alias = parse_ship_args("checks").unwrap();
        assert_eq!(alias.action, ShipAction::Status);
    }

    #[test]
    fn extracts_commit_message_section() {
        let markdown =
            "## Commit Message\n\nfeat: ship preview\n\nBody line\n\n## Testing\n\ncargo test";
        assert_eq!(
            extract_markdown_section(markdown, "commit message").as_deref(),
            Some("feat: ship preview\n\nBody line")
        );
        assert!(extract_markdown_section(markdown, "missing").is_none());
    }

    #[test]
    fn ship_readiness_blocks_clean_tree_without_ahead_commits() {
        let snapshot = sample_snapshot(Vec::new(), Some(passing_tests()));
        let readiness = evaluate_ship_readiness(&snapshot, false);
        assert_eq!(readiness.status, ShipReadinessStatus::Blocked);
        assert!(readiness
            .blockers
            .iter()
            .any(|b| b.contains("nothing to ship")));
    }

    #[test]
    fn ship_readiness_ready_with_changes_and_passing_tests() {
        let snapshot = sample_snapshot(vec![tracked_change("src/main.rs")], Some(passing_tests()));
        let readiness = evaluate_ship_readiness(&snapshot, false);
        assert_eq!(readiness.status, ShipReadinessStatus::Ready);
        assert!(readiness.blockers.is_empty());
        assert!(readiness.warnings.is_empty());
    }

    #[test]
    fn ship_readiness_warns_when_tests_are_not_run() {
        let snapshot = sample_snapshot(vec![tracked_change("src/main.rs")], None);
        let readiness = evaluate_ship_readiness(&snapshot, false);
        assert_eq!(readiness.status, ShipReadinessStatus::NeedsReview);
        assert!(readiness
            .warnings
            .iter()
            .any(|w| w.contains("tests were not run")));
    }

    #[test]
    fn ship_readiness_blocks_behind_branch_unless_allowed() {
        let mut snapshot =
            sample_snapshot(vec![tracked_change("src/main.rs")], Some(passing_tests()));
        snapshot.branch.behind = 2;

        let blocked = evaluate_ship_readiness(&snapshot, false);
        assert_eq!(blocked.status, ShipReadinessStatus::Blocked);

        let allowed = evaluate_ship_readiness(&snapshot, true);
        assert_eq!(allowed.status, ShipReadinessStatus::Ready);
    }

    #[test]
    fn ship_push_readiness_blocks_when_not_ahead_of_upstream() {
        let snapshot = sample_snapshot(Vec::new(), None);
        let readiness = evaluate_ship_push_readiness(&snapshot, false);
        assert_eq!(readiness.status, ShipReadinessStatus::Blocked);
        assert!(readiness
            .blockers
            .iter()
            .any(|b| b.contains("nothing to push")));
    }

    #[test]
    fn ship_push_readiness_allows_new_upstream_with_warning() {
        let mut snapshot = sample_snapshot(Vec::new(), None);
        snapshot.branch.upstream = None;
        let readiness = evaluate_ship_push_readiness(&snapshot, false);
        assert_eq!(readiness.status, ShipReadinessStatus::NeedsReview);
        assert!(readiness
            .warnings
            .iter()
            .any(|w| w.contains("set upstream")));
    }

    #[test]
    fn ship_push_readiness_blocks_behind_branch_unless_allowed() {
        let mut snapshot = sample_snapshot(Vec::new(), Some(passing_tests()));
        snapshot.branch.ahead = 1;
        snapshot.branch.behind = 1;

        let blocked = evaluate_ship_push_readiness(&snapshot, false);
        assert_eq!(blocked.status, ShipReadinessStatus::Blocked);

        let allowed = evaluate_ship_push_readiness(&snapshot, true);
        assert_eq!(allowed.status, ShipReadinessStatus::Ready);
    }

    #[test]
    fn ship_pr_readiness_requires_upstream_and_pushed_branch() {
        let mut snapshot = sample_snapshot(Vec::new(), Some(passing_tests()));
        snapshot.branch.upstream = None;
        let no_upstream = evaluate_ship_pr_readiness(&snapshot, false);
        assert_eq!(no_upstream.status, ShipReadinessStatus::Blocked);
        assert!(no_upstream
            .blockers
            .iter()
            .any(|b| b.contains("no upstream")));

        snapshot.branch.upstream = Some("origin/feature".into());
        snapshot.branch.ahead = 1;
        let unpushed = evaluate_ship_pr_readiness(&snapshot, false);
        assert_eq!(unpushed.status, ShipReadinessStatus::Blocked);
        assert!(unpushed.blockers.iter().any(|b| b.contains("unpushed")));
    }

    #[test]
    fn ship_pr_readiness_warns_without_tests() {
        let snapshot = sample_snapshot(Vec::new(), None);
        let readiness = evaluate_ship_pr_readiness(&snapshot, false);
        assert_eq!(readiness.status, ShipReadinessStatus::NeedsReview);
        assert!(readiness
            .warnings
            .iter()
            .any(|w| w.contains("tests were not run")));
    }

    #[test]
    fn ship_status_readiness_warns_for_unpushed_commits_without_blocking() {
        let mut snapshot = sample_snapshot(Vec::new(), Some(passing_tests()));
        snapshot.branch.ahead = 2;
        let readiness = evaluate_ship_status_readiness(&snapshot, false);

        assert_eq!(readiness.status, ShipReadinessStatus::NeedsReview);
        assert!(readiness.blockers.is_empty());
        assert!(readiness
            .warnings
            .iter()
            .any(|w| w.contains("PR checks may be stale")));
    }

    #[test]
    fn ship_status_readiness_requires_upstream() {
        let mut snapshot = sample_snapshot(Vec::new(), Some(passing_tests()));
        snapshot.branch.upstream = None;
        let readiness = evaluate_ship_status_readiness(&snapshot, false);

        assert_eq!(readiness.status, ShipReadinessStatus::Blocked);
        assert!(readiness.blockers.iter().any(|b| b.contains("no upstream")));
    }

    #[test]
    fn commit_specific_blockers_match_staging_mode() {
        let clean = sample_snapshot(Vec::new(), Some(passing_tests()));
        assert!(commit_specific_blockers(&clean, ShipStaging::All)[0]
            .contains("no working-tree changes"));

        let unstaged = sample_snapshot(vec![tracked_change("src/main.rs")], Some(passing_tests()));
        assert!(
            commit_specific_blockers(&unstaged, ShipStaging::StagedOnly)[0]
                .contains("no staged changes")
        );

        let staged = sample_snapshot(
            vec![GitFileState {
                path: "src/main.rs".into(),
                original_path: None,
                staged: Some('M'),
                unstaged: Some('.'),
                kind: GitFileKind::Tracked,
            }],
            Some(passing_tests()),
        );
        assert!(commit_specific_blockers(&staged, ShipStaging::StagedOnly).is_empty());
    }

    #[test]
    fn staged_ship_context_uses_only_cached_diff() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        fs::write(dir.path().join("README.md"), "staged\n").unwrap();
        git(dir.path(), &["add", "README.md"]);
        fs::write(dir.path().join("later.txt"), "unstaged\n").unwrap();

        let snapshot = collect_shipcheck(dir.path().to_str().unwrap()).unwrap();
        let context = build_staged_ship_context(&snapshot).unwrap().unwrap();

        assert!(context.content.contains("Staged Diff"));
        assert!(context.content.contains("staged"));
        assert!(!context.content.contains("unstaged"));
    }

    #[test]
    fn write_ship_commit_record_creates_markdown() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        fs::write(dir.path().join("README.md"), "changed\n").unwrap();
        git(dir.path(), &["add", "README.md"]);
        let snapshot = collect_shipcheck(dir.path().to_str().unwrap()).unwrap();

        let path = write_ship_commit_record(
            dir.path().join(".sessions").to_str().unwrap(),
            &snapshot,
            Some(&passing_tests()),
            ShipStaging::StagedOnly,
            "feat: test ship record",
            "abc1234",
        )
        .unwrap();
        let body = fs::read_to_string(path).unwrap();

        assert!(body.contains("# Albatross Ship Commit"));
        assert!(body.contains("feat: test ship record"));
        assert!(body.contains("abc1234"));
        assert!(body.contains("Final Staged Diff Stat"));
    }

    #[test]
    fn create_git_commit_commits_staged_changes() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        fs::write(dir.path().join("README.md"), "changed\n").unwrap();
        git(dir.path(), &["add", "README.md"]);

        let hash =
            create_git_commit(dir.path().to_str().unwrap(), "feat: commit from ship").unwrap();
        let subject =
            run_git_capture(dir.path().to_str().unwrap(), &["log", "-1", "--pretty=%s"]).unwrap();

        assert!(!hash.is_empty());
        assert_eq!(subject.trim(), "feat: commit from ship");
    }

    #[test]
    fn resolves_push_target_from_existing_upstream() {
        let mut snapshot = sample_snapshot(Vec::new(), Some(passing_tests()));
        snapshot.branch.ahead = 1;
        snapshot.branch.upstream = Some("origin/release/ship".into());
        let target = resolve_ship_push_target("/tmp", &snapshot).unwrap();

        assert_eq!(target.remote, "origin");
        assert_eq!(target.local_branch, "feature");
        assert_eq!(target.remote_branch, "release/ship");
        assert!(!target.set_upstream);
    }

    #[test]
    fn resolves_push_target_without_upstream_to_origin() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let bare = tempfile::tempdir().unwrap();
        Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg(bare.path())
            .output()
            .unwrap();
        git(
            dir.path(),
            &["remote", "add", "origin", bare.path().to_str().unwrap()],
        );
        let mut snapshot = collect_shipcheck(dir.path().to_str().unwrap()).unwrap();
        snapshot.branch.upstream = None;

        let target = resolve_ship_push_target(dir.path().to_str().unwrap(), &snapshot).unwrap();

        assert_eq!(target.remote, "origin");
        assert_eq!(target.local_branch, snapshot.branch.head.unwrap());
        assert!(target.set_upstream);
    }

    #[test]
    fn execute_ship_push_sets_upstream_on_bare_remote() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let bare = tempfile::tempdir().unwrap();
        Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg(bare.path())
            .output()
            .unwrap();
        git(
            dir.path(),
            &["remote", "add", "origin", bare.path().to_str().unwrap()],
        );
        let snapshot = collect_shipcheck(dir.path().to_str().unwrap()).unwrap();
        let target = resolve_ship_push_target(dir.path().to_str().unwrap(), &snapshot).unwrap();

        let output = execute_ship_push(dir.path().to_str().unwrap(), &target).unwrap();
        let upstream = run_git_capture(
            dir.path().to_str().unwrap(),
            &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        )
        .unwrap();

        assert!(output.contains("branch"));
        assert!(upstream.trim().starts_with("origin/"));
    }

    #[test]
    fn write_ship_push_record_creates_markdown() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let snapshot = collect_shipcheck(dir.path().to_str().unwrap()).unwrap();
        let target = ShipPushTarget {
            remote: "origin".into(),
            local_branch: "feature".into(),
            remote_branch: "feature".into(),
            set_upstream: true,
        };

        let path = write_ship_push_record(
            dir.path().join(".sessions").to_str().unwrap(),
            &snapshot,
            &target,
            "abc1234",
            "pushed",
        )
        .unwrap();
        let body = fs::read_to_string(path).unwrap();

        assert!(body.contains("# Albatross Ship Push"));
        assert!(body.contains("abc1234"));
        assert!(body.contains("pushed"));
        assert!(body.contains("Set upstream"));
    }

    #[test]
    fn parses_github_remote_urls() {
        assert_eq!(
            parse_github_repo("https://github.com/morganlinton/Albatross.git").as_deref(),
            Some("morganlinton/Albatross")
        );
        assert_eq!(
            parse_github_repo("git@github.com:morganlinton/Albatross.git").as_deref(),
            Some("morganlinton/Albatross")
        );
        assert_eq!(
            parse_github_repo("ssh://git@github.com/morganlinton/Albatross.git").as_deref(),
            Some("morganlinton/Albatross")
        );
        assert!(parse_github_repo("https://example.com/x/y.git").is_none());
    }

    #[test]
    fn resolves_pr_target_from_github_upstream() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        git(
            dir.path(),
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:morganlinton/Albatross.git",
            ],
        );
        let mut snapshot = collect_shipcheck(dir.path().to_str().unwrap()).unwrap();
        snapshot.branch.head = Some("feature".into());
        snapshot.branch.upstream = Some("origin/feature".into());

        let target =
            resolve_ship_pr_target(dir.path().to_str().unwrap(), &snapshot, Some("main")).unwrap();

        assert_eq!(target.repo, "morganlinton/Albatross");
        assert_eq!(target.head_branch, "feature");
        assert_eq!(target.base_branch, "main");
    }

    #[test]
    fn build_gh_pr_command_adds_draft_by_default() {
        let target = ShipPrTarget {
            remote: "origin".into(),
            repo: "morganlinton/Albatross".into(),
            head_branch: "feature".into(),
            base_branch: "main".into(),
        };
        let command = build_gh_pr_command(&target, "Title", "Body text", true);

        assert_eq!(command[0], "gh");
        assert!(command.contains(&"--draft".to_string()));
        assert!(shell_command_display(&command).contains("'Body text'"));

        let ready = build_gh_pr_command(&target, "Title", "Body text", false);
        assert!(!ready.contains(&"--draft".to_string()));
    }

    #[test]
    fn build_gh_pr_status_command_uses_open_head_filter() {
        let target = ShipPrTarget {
            remote: "origin".into(),
            repo: "morganlinton/Albatross".into(),
            head_branch: "feature".into(),
            base_branch: "main".into(),
        };
        let command = build_gh_pr_status_command(&target);

        assert_eq!(command[0], "gh");
        assert!(command
            .windows(2)
            .any(|parts| parts[0] == "--state" && parts[1] == "open"));
        assert!(command
            .windows(2)
            .any(|parts| parts[0] == "--head" && parts[1] == "feature"));
        assert!(command.iter().any(|arg| arg.contains("statusCheckRollup")));
    }

    #[test]
    fn render_ship_pr_body_includes_branch_commit_and_tests() {
        let snapshot = sample_snapshot(Vec::new(), Some(passing_tests()));
        let target = ShipPrTarget {
            remote: "origin".into(),
            repo: "morganlinton/Albatross".into(),
            head_branch: "feature".into(),
            base_branch: "main".into(),
        };
        let body = render_ship_pr_body(&snapshot, &target, "abc123", "abc123 subject");

        assert!(body.contains("abc123"));
        assert!(body.contains("feature"));
        assert!(body.contains("main"));
        assert!(body.contains("Passed"));
    }

    #[test]
    fn write_ship_pr_record_creates_markdown() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let snapshot = collect_shipcheck(dir.path().to_str().unwrap()).unwrap();
        let target = ShipPrTarget {
            remote: "origin".into(),
            repo: "morganlinton/Albatross".into(),
            head_branch: "feature".into(),
            base_branch: "main".into(),
        };
        let command = build_gh_pr_command(&target, "Title", "Body", true);

        let path = write_ship_pr_record(
            dir.path().join(".sessions").to_str().unwrap(),
            &ShipPrRecord {
                snapshot: &snapshot,
                target: &target,
                title: "Title",
                body: "Body",
                command: &command,
                url: Some("https://github.com/morganlinton/Albatross/pull/1"),
                status: "created",
            },
        )
        .unwrap();
        let body = fs::read_to_string(path).unwrap();

        assert!(body.contains("# Albatross Ship PR"));
        assert!(body.contains("morganlinton/Albatross"));
        assert!(body.contains("https://github.com/morganlinton/Albatross/pull/1"));
        assert!(body.contains("gh pr create"));
    }

    #[test]
    fn parse_ship_pr_status_json_handles_open_pr_checks() {
        let json = r#"[{
            "number": 7,
            "url": "https://github.com/morganlinton/Albatross/pull/7",
            "title": "Ship status",
            "state": "OPEN",
            "headRefName": "feature",
            "baseRefName": "main",
            "reviewDecision": "REVIEW_REQUIRED",
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
                    "__typename": "CheckRun",
                    "name": "build + test",
                    "workflowName": "CI",
                    "status": "COMPLETED",
                    "conclusion": "FAILURE",
                    "detailsUrl": "https://example.com/failure"
                },
                {
                    "__typename": "CheckRun",
                    "name": "release",
                    "workflowName": "Release",
                    "status": "IN_PROGRESS",
                    "conclusion": null,
                    "detailsUrl": "https://example.com/pending"
                },
                {
                    "__typename": "StatusContext",
                    "context": "optional",
                    "state": "SUCCESS",
                    "targetUrl": "https://example.com/status"
                }
            ]
        }]"#;

        let status = parse_ship_pr_status_json(json).unwrap().unwrap();
        let counts = ship_pr_check_counts(&status.checks);

        assert_eq!(status.number, 7);
        assert_eq!(status.review_decision.as_deref(), Some("REVIEW_REQUIRED"));
        assert_eq!(counts.success, 2);
        assert_eq!(counts.failing, 1);
        assert_eq!(counts.pending, 1);
        assert_eq!(
            ship_pr_status_next_action(&status, counts),
            "inspect failing checks, fix locally, then /ship commit and /ship push"
        );
    }

    #[test]
    fn parse_ship_pr_status_json_handles_empty_list() {
        assert!(parse_ship_pr_status_json("[]").unwrap().is_none());
    }

    #[test]
    fn extract_url_finds_pr_url() {
        assert_eq!(
            extract_url("Created pull request https://github.com/a/b/pull/1").as_deref(),
            Some("https://github.com/a/b/pull/1")
        );
    }
}
