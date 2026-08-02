use chrono::Utc;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Once the hook has been installed, this holds the directory we write
/// crash logs into. Kept in a `Mutex<Option<...>>` because `set_hook`
/// requires a `'static` callback and the directory isn't known until
/// session init has run.
static LOG_DIR: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

/// Env vars whose presence we record but whose values we redact, because
/// they hold secrets. Anything else is logged as `name=value`.
const REDACTED_ENV: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
    "LLAMACPP_API_KEY",
    "ANTHROPIC_API_KEY",
];

fn dir_lock() -> &'static Mutex<Option<PathBuf>> {
    LOG_DIR.get_or_init(|| Mutex::new(None))
}

/// Install the panic hook. Calling this twice is harmless — the hook
/// chains into the previous one (which is usually the default
/// stderr-printer), and the second call just refreshes the target dir.
pub fn install_panic_hook() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // The interactive UI normally hides the hardware cursor while the
            // model owns the terminal. Restore it before printing a crash so
            // the user's shell is never left in that passive state.
            crate::cursor::restore();
            // Always run the previous hook first so the user still sees the
            // crash on stderr; the file is the *addition*, not a replacement.
            prev(info);
            let dir = dir_lock().lock().ok().and_then(|g| g.clone());
            if let Some(dir) = dir {
                let _ = write_crash_report(&dir, info);
            }
        }));
    });
}

/// Tell the hook where to write logs. Safe to call any number of times;
/// later calls overwrite the previous directory.
pub fn set_crash_dir(session_dir: &str) {
    let path = Path::new(session_dir).join("crashes");
    if let Ok(mut guard) = dir_lock().lock() {
        *guard = Some(path);
    }
}

fn write_crash_report(
    dir: &Path,
    info: &std::panic::PanicHookInfo<'_>,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let ts = Utc::now().format("%Y-%m-%dT%H-%M-%S-%3fZ");
    let path = dir.join(format!("{ts}.log"));

    let mut body = String::new();
    body.push_str(&format!(
        "albatross {} crash report\n",
        env!("CARGO_PKG_VERSION")
    ));
    body.push_str(&format!("timestamp: {}\n", Utc::now().to_rfc3339()));
    body.push_str(&format!(
        "os: {} {}\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    body.push_str("\n--- panic ---\n");
    if let Some(location) = info.location() {
        body.push_str(&format!(
            "location: {}:{}:{}\n",
            location.file(),
            location.line(),
            location.column()
        ));
    }
    let payload = info.payload();
    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string payload>".into()
    };
    body.push_str(&format!("message: {msg}\n"));

    body.push_str("\n--- backtrace ---\n");
    // Capturing a backtrace requires RUST_BACKTRACE=1 in std; in release
    // builds without that, this just records the request and the user can
    // re-run with RUST_BACKTRACE=1.
    let bt = std::backtrace::Backtrace::capture();
    body.push_str(&bt.to_string());
    if matches!(bt.status(), std::backtrace::BacktraceStatus::Disabled) {
        body.push_str("\n(set RUST_BACKTRACE=1 to capture frames)\n");
    }

    body.push_str("\n--- env (sensitive values redacted) ---\n");
    let mut env_keys: Vec<_> = std::env::vars()
        .filter(|(k, _)| {
            k.starts_with("ALBATROSS_")
                || k.starts_with("AGENT_")
                || k.starts_with("BACKEND")
                || k.starts_with("APPROVAL_")
                || k.starts_with("WARMUP")
                || k.starts_with("WORKSPACE_")
                || k.starts_with("OUTSIDE_")
                || REDACTED_ENV.contains(&k.as_str())
                || k.ends_with("_BASE_URL")
                || k.ends_with("_API_KEY")
        })
        .collect();
    env_keys.sort();
    for (k, v) in env_keys {
        let value = if REDACTED_ENV.contains(&k.as_str()) || k.ends_with("_API_KEY") {
            if v.is_empty() {
                "(empty)".into()
            } else {
                "(redacted)".into()
            }
        } else {
            v
        };
        body.push_str(&format!("{k}={value}\n"));
    }

    std::fs::write(&path, body)?;

    // Best-effort post-print so the user knows where to look. The default
    // hook already printed the panic message; we just add the file path.
    eprintln!(
        "\nalbatross wrote a crash log to {} — attach it when filing an issue.",
        path.display()
    );

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Both tests below mutate the global panic hook and the process env.
    /// Run them serialized — running in parallel makes them race and the
    /// "visible-value" check or the "(empty)" check flakes.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn writes_report_with_panic_message_and_redaction() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("OPENAI_API_KEY", "sk-not-real-zzzzzzzz");
        std::env::set_var("ALBATROSS_TEST_FLAG", "visible-value");

        let dir_path = dir.path().to_path_buf();
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = write_crash_report(&dir_path.join("crashes"), info);
        }));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("intentional test panic");
        }));
        std::panic::set_hook(prev);
        assert!(result.is_err());

        let logs: Vec<_> = std::fs::read_dir(dir.path().join("crashes"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(logs.len(), 1);
        let body = std::fs::read_to_string(&logs[0]).unwrap();
        assert!(body.contains("intentional test panic"));
        assert!(body.contains("OPENAI_API_KEY=(redacted)"));
        assert!(body.contains("ALBATROSS_TEST_FLAG=visible-value"));
        assert!(!body.contains("sk-not-real-zzzzzzzz"));

        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("ALBATROSS_TEST_FLAG");
    }

    #[test]
    fn redacts_empty_api_key_as_empty_not_redacted() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("OPENROUTER_API_KEY", "");
        let dir_path = dir.path().to_path_buf();
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = write_crash_report(&dir_path.join("crashes"), info);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("test");
        }));
        std::panic::set_hook(prev);
        let logs: Vec<_> = std::fs::read_dir(dir.path().join("crashes"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        let body = std::fs::read_to_string(&logs[0]).unwrap();
        assert!(body.contains("OPENROUTER_API_KEY=(empty)"));
        std::env::remove_var("OPENROUTER_API_KEY");
    }
}
