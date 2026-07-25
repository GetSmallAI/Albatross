use serde_json::Value;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;
use tokio::sync::Mutex;

use super::{parse_hook_effect, HookCommandConfig, HookEffect};

const MAX_HOOK_OUTPUT_CAPTURE_BYTES: usize = 1024 * 1024;
const HOOK_CLEANUP_GRACE: Duration = Duration::from_millis(500);

#[cfg(unix)]
const SIGKILL: std::os::raw::c_int = 9;

#[cfg(unix)]
unsafe extern "C" {
    fn setpgid(pid: std::os::raw::c_int, pgid: std::os::raw::c_int) -> std::os::raw::c_int;
    fn kill(pid: std::os::raw::c_int, sig: std::os::raw::c_int) -> std::os::raw::c_int;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookRunFailure {
    Spawn,
    Pipe,
    Timeout,
    Wait,
}

#[derive(Debug, Clone)]
pub struct HookRunResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub failure: Option<HookRunFailure>,
    pub duration_ms: u128,
    pub stdout: String,
    pub stderr: String,
    pub effect: HookEffect,
}

impl HookRunResult {
    pub fn fail_closed_reason(&self) -> Option<String> {
        if self.timed_out || self.failure == Some(HookRunFailure::Timeout) {
            return Some(
                self.effect
                    .warning
                    .clone()
                    .unwrap_or_else(|| "hook timed out".into()),
            );
        }
        match self.failure {
            Some(HookRunFailure::Spawn) => Some(format!(
                "hook command failed to start{}",
                detail_suffix(&self.stderr, self.effect.warning.as_deref())
            )),
            Some(HookRunFailure::Pipe) => Some("hook command failed to open stdio pipes".into()),
            Some(HookRunFailure::Wait) => Some(format!(
                "hook command failed while waiting{}",
                detail_suffix(&self.stderr, self.effect.warning.as_deref())
            )),
            Some(HookRunFailure::Timeout) => unreachable!("handled above"),
            None => match self.exit_code {
                Some(126 | 127) => Some(format!(
                    "hook command failed{}",
                    detail_suffix(&self.stderr, self.effect.warning.as_deref())
                )),
                _ => None,
            },
        }
    }
}

pub async fn run_command_hook(handler: &HookCommandConfig, payload: &Value) -> HookRunResult {
    let start = Instant::now();
    let (shell, flag) = hook_shell_program_and_arg();
    let mut command = tokio::process::Command::new(shell);
    command
        .arg(flag)
        .arg(&handler.command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.kill_on_drop(true);
    configure_hook_process(&mut command);
    apply_hook_env(&mut command, handler, payload);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            let stderr = e.to_string();
            return HookRunResult {
                exit_code: None,
                timed_out: false,
                failure: Some(HookRunFailure::Spawn),
                duration_ms: start.elapsed().as_millis(),
                stdout: String::new(),
                stderr: stderr.clone(),
                effect: HookEffect {
                    warning: Some(format!("hook failed to start: {stderr}")),
                    ..HookEffect::default()
                },
            };
        }
    };
    let child_group = child.id();

    let Some(stdin) = child.stdin.take() else {
        return pipe_failure(start, child, child_group).await;
    };
    let Some(stdout) = child.stdout.take() else {
        return pipe_failure(start, child, child_group).await;
    };
    let Some(stderr) = child.stderr.take() else {
        return pipe_failure(start, child, child_group).await;
    };
    let body = serde_json::to_vec(payload).map(|mut body| {
        body.push(b'\n');
        body
    });
    let mut write_stdin = tokio::spawn(async move {
        let Ok(body) = body else {
            return;
        };
        let mut stdin = stdin;
        let _ = stdin.write_all(&body).await;
    });
    let stdout_buf = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::new()));
    let mut read_stdout = tokio::spawn(read_to_limited_buffer(stdout, stdout_buf.clone()));
    let mut read_stderr = tokio::spawn(read_to_limited_buffer(stderr, stderr_buf.clone()));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(handler.timeout_sec.max(1));
    let mut exit_code = None;
    let mut timed_out = false;
    let mut failure = None;
    if tokio::time::timeout_at(deadline, &mut write_stdin)
        .await
        .is_err()
    {
        timed_out = true;
        failure = Some(HookRunFailure::Timeout);
        write_stdin.abort();
        exit_code = terminate_child_tree(&mut child, child_group).await;
        let _ = wait_task_or_abort(&mut read_stdout, HOOK_CLEANUP_GRACE).await;
        let _ = wait_task_or_abort(&mut read_stderr, HOOK_CLEANUP_GRACE).await;
    }
    if !timed_out {
        match tokio::time::timeout_at(deadline, &mut read_stdout).await {
            Ok(_) => {}
            Err(_) => {
                timed_out = true;
                failure = Some(HookRunFailure::Timeout);
                read_stdout.abort();
                exit_code = terminate_child_tree(&mut child, child_group).await;
                let _ = wait_task_or_abort(&mut read_stderr, HOOK_CLEANUP_GRACE).await;
            }
        }
    }
    if !timed_out {
        match tokio::time::timeout_at(deadline, &mut read_stderr).await {
            Ok(_) => {}
            Err(_) => {
                timed_out = true;
                failure = Some(HookRunFailure::Timeout);
                read_stderr.abort();
                exit_code = terminate_child_tree(&mut child, child_group).await;
            }
        }
    }
    if !timed_out {
        match tokio::time::timeout_at(deadline, child.wait()).await {
            Ok(Ok(status)) => exit_code = Some(status.code().unwrap_or(1)),
            Ok(Err(_)) => failure = Some(HookRunFailure::Wait),
            Err(_) => {
                timed_out = true;
                failure = Some(HookRunFailure::Timeout);
                exit_code = terminate_child_tree(&mut child, child_group).await;
            }
        }
    }

    let stdout = stdout_buf.lock().await.clone();
    let stderr = stderr_buf.lock().await.clone();
    let stdout = String::from_utf8_lossy(&stdout).to_string();
    let stderr = String::from_utf8_lossy(&stderr).to_string();
    let effect = if timed_out || failure.is_some() {
        let mut effect = parse_hook_effect(exit_code.unwrap_or(0), &stdout, &stderr);
        if timed_out {
            push_warning(
                &mut effect,
                format!("hook timed out after {}s", handler.timeout_sec.max(1)),
            );
        } else if failure == Some(HookRunFailure::Wait) {
            push_warning(&mut effect, "hook wait failed".to_string());
        }
        effect
    } else {
        parse_hook_effect(exit_code.unwrap_or(1), &stdout, &stderr)
    };

    HookRunResult {
        exit_code,
        timed_out,
        failure,
        duration_ms: start.elapsed().as_millis(),
        stdout,
        stderr,
        effect,
    }
}

pub(super) fn hook_shell_program_and_arg() -> (&'static str, &'static str) {
    if cfg!(windows) {
        ("cmd.exe", "/C")
    } else {
        ("/bin/sh", "-c")
    }
}

fn apply_hook_env(
    command: &mut tokio::process::Command,
    handler: &HookCommandConfig,
    payload: &Value,
) {
    command.env_clear();
    for key in inherited_hook_env_allowlist() {
        if let Ok(value) = std::env::var(key) {
            command.env(key, value);
        }
    }
    for key in &handler.env_vars {
        if let Ok(value) = std::env::var(key) {
            command.env(key, value);
        }
    }
    for (key, value) in &handler.env {
        command.env(key, value);
    }
    if let Some(event) = payload.get("hook_event_name").and_then(Value::as_str) {
        command.env("ALBATROSS_HOOK_EVENT", event);
    }
    if let Some(session_id) = payload.get("session_id").and_then(Value::as_str) {
        command.env("ALBATROSS_SESSION_ID", session_id);
    }
    if let Some(turn_id) = payload.get("turn_id").and_then(Value::as_u64) {
        command.env("ALBATROSS_TURN_ID", turn_id.to_string());
    }
    if let Some(path) = payload.get("transcript_path").and_then(Value::as_str) {
        command.env("ALBATROSS_TRANSCRIPT_PATH", path);
    }
    if let Some(path) = payload.get("events_path").and_then(Value::as_str) {
        command.env("ALBATROSS_EVENTS_PATH", path);
    }
}

#[cfg(not(windows))]
fn inherited_hook_env_allowlist() -> &'static [&'static str] {
    &["PATH", "HOME"]
}

#[cfg(windows)]
fn inherited_hook_env_allowlist() -> &'static [&'static str] {
    &["PATH", "USERPROFILE", "HOMEDRIVE", "HOMEPATH", "SYSTEMROOT"]
}

async fn read_to_limited_buffer<R>(mut reader: R, buf: Arc<Mutex<Vec<u8>>>)
where
    R: AsyncRead + Unpin,
{
    let mut chunk = [0u8; 8192];
    while let Ok(n) = reader.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        let mut guard = buf.lock().await;
        let remaining = MAX_HOOK_OUTPUT_CAPTURE_BYTES.saturating_sub(guard.len());
        if remaining > 0 {
            guard.extend_from_slice(&chunk[..n.min(remaining)]);
        }
    }
}

async fn pipe_failure(start: Instant, mut child: Child, child_group: Option<u32>) -> HookRunResult {
    let _ = terminate_child_tree(&mut child, child_group).await;
    HookRunResult {
        exit_code: None,
        timed_out: false,
        failure: Some(HookRunFailure::Pipe),
        duration_ms: start.elapsed().as_millis(),
        stdout: String::new(),
        stderr: String::new(),
        effect: HookEffect {
            warning: Some("hook failed to open stdio pipes".into()),
            ..HookEffect::default()
        },
    }
}

async fn terminate_child_tree(child: &mut Child, child_group: Option<u32>) -> Option<i32> {
    kill_hook_process_group(child_group);
    let _ = child.start_kill();
    tokio::time::timeout(HOOK_CLEANUP_GRACE, child.wait())
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|status| status.code())
}

async fn wait_task_or_abort<T>(
    task: &mut tokio::task::JoinHandle<T>,
    grace: Duration,
) -> Option<Result<T, tokio::task::JoinError>> {
    match tokio::time::timeout(grace, &mut *task).await {
        Ok(result) => Some(result),
        Err(_) => {
            task.abort();
            None
        }
    }
}

#[cfg(unix)]
fn configure_hook_process(command: &mut tokio::process::Command) {
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
fn configure_hook_process(_command: &mut tokio::process::Command) {}

#[cfg(unix)]
fn kill_hook_process_group(child_group: Option<u32>) {
    if let Some(pid) = child_group.and_then(|pid| i32::try_from(pid).ok()) {
        unsafe {
            let _ = kill(-pid, SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_hook_process_group(_child_group: Option<u32>) {}

fn push_warning(effect: &mut HookEffect, warning: String) {
    match &mut effect.warning {
        Some(existing) => {
            existing.push_str("; ");
            existing.push_str(&warning);
        }
        None => effect.warning = Some(warning),
    }
}

fn detail_suffix(stderr: &str, warning: Option<&str>) -> String {
    let detail = stderr
        .trim()
        .split('\n')
        .next()
        .filter(|line| !line.trim().is_empty())
        .or(warning)
        .unwrap_or("");
    if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    }
}
