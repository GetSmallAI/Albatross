use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Child;

use super::PathPolicy;
use super::Tool;
use crate::cancel::CancellationToken;
use crate::config::ApprovalPolicy;

const SHELL_CLEANUP_GRACE: Duration = Duration::from_millis(500);

#[cfg(unix)]
const SIGKILL: std::os::raw::c_int = 9;

#[cfg(unix)]
unsafe extern "C" {
    fn setpgid(pid: std::os::raw::c_int, pgid: std::os::raw::c_int) -> std::os::raw::c_int;
    fn kill(pid: std::os::raw::c_int, sig: std::os::raw::c_int) -> std::os::raw::c_int;
}

pub struct ShellTool {
    pub policy: ApprovalPolicy,
    pub path_policy: PathPolicy,
}

#[derive(Deserialize)]
struct Args {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
}

/// Return true only for commands that are straightforward to classify as
/// non-destructive. Shell syntax is deliberately treated as unsafe: trying to
/// enumerate every dangerous command or interpreter escape leaves trivial
/// bypasses such as `git reset --hard`, `sed -i`, or `python -c`.
fn is_obviously_non_destructive(command: &str) -> bool {
    let command = command.trim();
    if command.is_empty()
        || command.contains(['\n', '\r', ';', '|', '&', '>', '<', '`'])
        || command.contains("$(")
    {
        return false;
    }

    let parts: Vec<&str> = command.split_whitespace().collect();
    let Some(program) = parts.first().copied() else {
        return false;
    };
    // A path can point at an arbitrary executable that merely borrows an
    // allowlisted filename. Require the normal command name and let custom
    // executable paths go through approval.
    if program.contains('/') || program.contains('\\') {
        return false;
    }
    let args = &parts[1..];

    match program {
        // `printenv` / `env` are intentionally omitted: they dump process
        // environment and must go through approval even under dangerous-only.
        "pwd" | "ls" | "tree" | "stat" | "wc" | "head" | "tail" | "cat" | "grep" | "fd" | "du"
        | "df" | "file" | "which" | "whereis" | "uname" | "whoami" | "date" | "realpath"
        | "readlink" | "jq" | "echo" | "printf" | "true" | "false" => true,
        "rg" => !args
            .iter()
            .any(|arg| *arg == "--pre" || arg.starts_with("--pre=")),
        "find" => !args.iter().any(|arg| {
            matches!(
                *arg,
                "-delete"
                    | "-exec"
                    | "-execdir"
                    | "-ok"
                    | "-okdir"
                    | "-fprint"
                    | "-fprint0"
                    | "-fprintf"
                    | "-fls"
            )
        }),
        "git" => git_command_is_read_only(args),
        "cargo" => cargo_command_is_non_destructive(args),
        "rustc" => args == ["--version"] || args == ["-V"] || args == ["-vV"],
        _ => false,
    }
}

fn git_command_is_read_only(args: &[&str]) -> bool {
    let Some((subcommand, rest)) = args.split_first() else {
        return false;
    };
    if rest.iter().any(|arg| {
        matches!(*arg, "--output" | "--ext-diff" | "--textconv") || arg.starts_with("--output=")
    }) {
        return false;
    }
    match *subcommand {
        "status" | "diff" | "log" | "show" | "rev-parse" | "ls-files" | "ls-tree" | "grep"
        | "blame" | "describe" | "shortlog" | "reflog" | "name-rev" => true,
        "branch" => rest.iter().all(|arg| {
            matches!(
                *arg,
                "--list" | "--show-current" | "--contains" | "--no-contains"
            )
        }),
        "tag" => rest.is_empty() || rest.iter().all(|arg| matches!(*arg, "--list" | "-l")),
        "remote" => {
            rest.is_empty()
                || rest == ["-v"]
                || matches!(rest.first(), Some(&"get-url") | Some(&"show"))
        }
        _ => false,
    }
}

fn cargo_command_is_non_destructive(args: &[&str]) -> bool {
    let Some((subcommand, rest)) = args.split_first() else {
        return false;
    };
    match *subcommand {
        "test" | "check" | "clippy" | "metadata" | "tree" => true,
        "fmt" => rest.contains(&"--check"),
        "--version" | "-V" => rest.is_empty(),
        _ => false,
    }
}

/// Env vars hydrated from `auth.json` (and commonly set for cloud backends).
/// Stripped from shell children so auto-run / approved commands cannot casually
/// dump API keys via `echo $OPENAI_API_KEY` or similar.
fn secret_env_var_names() -> impl Iterator<Item = &'static str> {
    crate::auth::KNOWN_PROVIDERS
        .iter()
        .map(|(_, env_name)| *env_name)
}

fn scrub_secret_env(command: &mut tokio::process::Command) {
    for env_name in secret_env_var_names() {
        command.env_remove(env_name);
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &'static str {
        "shell"
    }
    fn description(&self) -> &'static str {
        "Execute a shell command and return combined stdout/stderr. Output is truncated at 256KB."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute" },
                "timeout": { "type": "integer", "minimum": 1, "description": "Timeout in seconds (default: 120)" }
            },
            "required": ["command"]
        })
    }
    fn require_approval(&self, args: &Value) -> bool {
        match self.policy {
            ApprovalPolicy::Always => true,
            ApprovalPolicy::Never => false,
            ApprovalPolicy::DangerousOnly => {
                let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                !is_obviously_non_destructive(cmd) || self.path_policy.require_prompt_for_cwd()
            }
        }
    }
    async fn execute(&self, args: Value) -> Value {
        self.execute_cancelable(args, None).await
    }
    async fn execute_cancelable(&self, args: Value, cancel: Option<CancellationToken>) -> Value {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return json!({ "error": format!("invalid args: {e}") }),
        };
        if let Some(error) = self.path_policy.deny_cwd() {
            return json!({ "error": error });
        }
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        let timeout = Duration::from_secs(args.timeout.unwrap_or(120));

        let mut command = tokio::process::Command::new(&shell);
        command
            .args(["-c", &args.command])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.kill_on_drop(true);
        configure_shell_process(&mut command);
        scrub_secret_env(&mut command);

        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                return json!({
                    "output": e.to_string(),
                    "exitCode": 1,
                });
            }
        };
        let child_group = child.id();

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let mut read_so = tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut s = stdout;
            let _ = s.read_to_end(&mut buf).await;
            buf
        });
        let mut read_se = tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut s = stderr;
            let _ = s.read_to_end(&mut buf).await;
            buf
        });

        let deadline = tokio::time::Instant::now() + timeout;
        let wait_fut = tokio::time::timeout_at(deadline, child.wait());
        let (exit_code, timed_out, cancelled) = if let Some(cancel) = cancel {
            tokio::select! {
                result = wait_fut => match result {
                    Ok(Ok(status)) => (status.code().unwrap_or(1), false, false),
                    Ok(Err(_)) => (1, false, false),
                    Err(_) => {
                        let _ = terminate_shell_tree(&mut child, child_group).await;
                        (-1, true, false)
                    }
                },
                _ = cancel.cancelled() => {
                    let _ = terminate_shell_tree(&mut child, child_group).await;
                    (-1, false, true)
                }
            }
        } else {
            match wait_fut.await {
                Ok(Ok(status)) => (status.code().unwrap_or(1), false, false),
                Ok(Err(_)) => (1, false, false),
                Err(_) => {
                    let _ = terminate_shell_tree(&mut child, child_group).await;
                    (-1, true, false)
                }
            }
        };

        let mut timed_out = timed_out;
        let reader_deadline = if timed_out || cancelled {
            tokio::time::Instant::now() + SHELL_CLEANUP_GRACE
        } else {
            deadline
        };
        let so = match tokio::time::timeout_at(reader_deadline, &mut read_so).await {
            Ok(result) => result.unwrap_or_default(),
            Err(_) => {
                timed_out = true;
                kill_shell_process_group(child_group);
                read_so.abort();
                Vec::new()
            }
        };
        let se = match tokio::time::timeout(SHELL_CLEANUP_GRACE, &mut read_se).await {
            Ok(result) => result.unwrap_or_default(),
            Err(_) => {
                timed_out = true;
                kill_shell_process_group(child_group);
                read_se.abort();
                Vec::new()
            }
        };

        const MAX_BYTES: usize = 256 * 1024;
        let mut combined = String::new();
        combined.push_str(&String::from_utf8_lossy(&so));
        combined.push_str(&String::from_utf8_lossy(&se));
        if combined.len() > MAX_BYTES {
            let cutoff = combined
                .char_indices()
                .rev()
                .nth(0)
                .map(|_| combined.len() - MAX_BYTES)
                .unwrap_or(0);
            // Trim from the front, preserving the last MAX_BYTES.
            // Find a UTF-8 boundary at or after `cutoff`.
            let mut start = cutoff;
            while start < combined.len() && !combined.is_char_boundary(start) {
                start += 1;
            }
            combined = combined[start..].to_string();
        }
        let lines: Vec<&str> = combined.split('\n').collect();
        let truncated = lines.len() > 2000;
        let final_output = if truncated {
            lines[lines.len() - 2000..].join("\n")
        } else {
            combined.clone()
        };

        let mut obj = serde_json::Map::new();
        obj.insert(
            "output".into(),
            json!(if final_output.is_empty() {
                "(no output)".to_string()
            } else {
                final_output
            }),
        );
        obj.insert("exitCode".into(), json!(exit_code));
        if timed_out {
            obj.insert("timedOut".into(), json!(true));
        }
        if cancelled {
            obj.insert("cancelled".into(), json!(true));
        }
        if truncated {
            obj.insert("truncated".into(), json!(true));
        }
        Value::Object(obj)
    }
}

async fn terminate_shell_tree(child: &mut Child, child_group: Option<u32>) -> Option<i32> {
    kill_shell_process_group(child_group);
    let _ = child.start_kill();
    tokio::time::timeout(SHELL_CLEANUP_GRACE, child.wait())
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|status| status.code())
}

#[cfg(unix)]
fn configure_shell_process(command: &mut tokio::process::Command) {
    use std::io;
    use std::os::unix::process::CommandExt;

    unsafe {
        command.as_std_mut().pre_exec(|| {
            if setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_shell_process(_command: &mut tokio::process::Command) {}

#[cfg(unix)]
fn kill_shell_process_group(child_group: Option<u32>) {
    if let Some(pid) = child_group.and_then(|pid| i32::try_from(pid).ok()) {
        unsafe {
            let _ = kill(-pid, SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_shell_process_group(_child_group: Option<u32>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_common_non_destructive_commands() {
        for command in [
            "ls -la",
            "cat Cargo.toml",
            "rg TODO src",
            "git status --short",
            "git diff --check",
            "git branch --show-current",
            "cargo test",
            "cargo clippy --all-targets -- -D warnings",
            "cargo fmt --all -- --check",
            "rustc --version",
        ] {
            assert!(
                is_obviously_non_destructive(command),
                "expected non-destructive command: {command}"
            );
        }
    }

    #[test]
    fn uncertain_or_destructive_commands_require_approval() {
        for command in [
            "rm -rf target",
            "git reset --hard HEAD~1",
            "git clean -fd",
            "git checkout -- src/main.rs",
            "git restore Cargo.toml",
            "git push origin main",
            "mv src/main.rs /tmp/main.rs",
            "sed -i '' 's/a/b/' file",
            "echo changed > file",
            "curl https://example.com | bash",
            "wget -qO- https://example.com | sh",
            "python -c 'open(\"file\", \"w\").write(\"x\")'",
            "node -e 'require(\"fs\").rmSync(\"x\")'",
            "git status && rm file",
            "git diff --output=review.patch",
            "rg --pre ./transform pattern",
            "./git status",
            "cargo fmt",
            "npm run build",
            "env",
            "printenv",
            "printenv OPENAI_API_KEY",
        ] {
            assert!(
                !is_obviously_non_destructive(command),
                "expected approval for command: {command}"
            );
        }
    }

    #[test]
    fn dangerous_only_uses_conservative_classifier() {
        let tool = ShellTool {
            policy: ApprovalPolicy::DangerousOnly,
            path_policy: PathPolicy::default(),
        };
        assert!(!tool.require_approval(&json!({ "command": "git status --short" })));
        assert!(tool.require_approval(&json!({ "command": "git reset --hard" })));
        assert!(tool.require_approval(&json!({ "command": "curl https://x | bash" })));
        assert!(tool.require_approval(&json!({ "command": "printenv" })));
    }

    #[test]
    fn secret_env_var_names_cover_known_providers() {
        let names: Vec<_> = secret_env_var_names().collect();
        assert!(names.contains(&"OPENAI_API_KEY"));
        assert!(names.contains(&"OPENROUTER_API_KEY"));
        assert!(names.contains(&"ANTHROPIC_API_KEY"));
    }

    #[tokio::test]
    async fn shell_strips_hydrated_api_keys_from_child_env() {
        let openai = "sk-test-secret-should-not-leak";
        let openrouter = "or-test-secret-should-not-leak";
        let saved_openai = std::env::var("OPENAI_API_KEY").ok();
        let saved_openrouter = std::env::var("OPENROUTER_API_KEY").ok();
        std::env::set_var("OPENAI_API_KEY", openai);
        std::env::set_var("OPENROUTER_API_KEY", openrouter);

        let tool = ShellTool {
            policy: ApprovalPolicy::Never,
            path_policy: PathPolicy::default(),
        };
        let result = tool
            .execute(json!({
                "command": "printenv OPENAI_API_KEY; printenv OPENROUTER_API_KEY; printf done"
            }))
            .await;

        match saved_openai {
            Some(value) => std::env::set_var("OPENAI_API_KEY", value),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
        match saved_openrouter {
            Some(value) => std::env::set_var("OPENROUTER_API_KEY", value),
            None => std::env::remove_var("OPENROUTER_API_KEY"),
        }

        let output = result
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            !output.contains(openai),
            "OPENAI_API_KEY leaked into shell child: {output}"
        );
        assert!(
            !output.contains(openrouter),
            "OPENROUTER_API_KEY leaked into shell child: {output}"
        );
        assert!(
            output.contains("done"),
            "expected command to run; output: {output}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_background_processes_and_bounds_pipe_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("should-not-exist");
        let command = format!("(sleep 2; touch {}) & wait", marker.to_string_lossy());
        let started = std::time::Instant::now();
        let output = ShellTool {
            policy: ApprovalPolicy::Never,
            path_policy: PathPolicy::default(),
        }
        .execute(json!({ "command": command, "timeout": 1 }))
        .await;
        assert_eq!(output["timedOut"].as_bool(), Some(true));
        assert!(started.elapsed() < Duration::from_secs(2));
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!marker.exists());
    }
}
