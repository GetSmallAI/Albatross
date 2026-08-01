use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::config::LoaderStyle;
use crate::theme::{ascii_enabled, colors_enabled, MUTED, RESET};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const ASCII_SPINNER_FRAMES: &[&str] = &["-", "\\", "|", "/"];
const GRADIENT_COLORS: &[&str] = &[
    "\x1b[38;5;240m",
    "\x1b[38;5;245m",
    "\x1b[38;5;250m",
    "\x1b[38;5;255m",
    "\x1b[38;5;250m",
    "\x1b[38;5;245m",
];

static TERMINAL_OUTPUT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn terminal_output_lock() -> &'static Mutex<()> {
    TERMINAL_OUTPUT_LOCK.get_or_init(|| Mutex::new(()))
}

pub struct Loader {
    stop: Arc<AtomicBool>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl Loader {
    pub fn start(text: String, style: LoaderStyle) -> Self {
        Self::start_with_shared(text, style, Arc::new(AtomicBool::new(false)))
    }

    fn start_with_shared(text: String, style: LoaderStyle, stop: Arc<AtomicBool>) -> Self {
        let stop_inner = stop.clone();
        let text_shared = Arc::new(std::sync::Mutex::new(text));
        let text_for_task = text_shared.clone();
        let interval_ms: u64 = match style {
            LoaderStyle::Gradient => 150,
            LoaderStyle::Spinner => 80,
            LoaderStyle::Minimal => 300,
        };
        let handle = tokio::spawn(async move {
            let mut frame: usize = 0;
            draw_if_active(&stop_inner, frame, &text_for_task.lock().unwrap(), style);
            loop {
                tokio::time::sleep(Duration::from_millis(interval_ms)).await;
                if stop_inner.load(Ordering::Relaxed) {
                    break;
                }
                frame = frame.wrapping_add(1);
                draw_if_active(&stop_inner, frame, &text_for_task.lock().unwrap(), style);
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
        let _guard = terminal_output_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut out = std::io::stdout();
        let _ = write!(out, "\r\x1b[K");
        let _ = out.flush();
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

fn draw_if_active(stop: &AtomicBool, frame: usize, text: &str, style: LoaderStyle) {
    let _guard = terminal_output_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut out = std::io::stdout();
    draw_if_active_to(stop, &mut out, frame, text, style);
    let _ = out.flush();
}

fn draw_if_active_to(
    stop: &AtomicBool,
    out: &mut impl Write,
    frame: usize,
    text: &str,
    style: LoaderStyle,
) {
    if stop.load(Ordering::Relaxed) {
        return;
    }
    // Gradient builds raw 256-color codes directly (not via `Style`), so it
    // needs an explicit NO_COLOR fallback: render as the Minimal style instead.
    let style = if style == LoaderStyle::Gradient && !colors_enabled() {
        LoaderStyle::Minimal
    } else {
        style
    };
    match style {
        LoaderStyle::Minimal => {
            let dots = ["·", "··", "···"];
            let _ = write!(out, "\r{MUTED}{text}{}{RESET}", dots[frame % 3]);
        }
        LoaderStyle::Spinner => {
            let frames = if ascii_enabled() {
                ASCII_SPINNER_FRAMES
            } else {
                SPINNER_FRAMES
            };
            let ch = frames[frame % frames.len()];
            let _ = write!(out, "\r{MUTED}{ch} {text}{RESET}");
        }
        LoaderStyle::Gradient => {
            let len = GRADIENT_COLORS.len();
            let mut s = String::from("\r");
            for (i, ch) in text.chars().enumerate() {
                s.push_str(GRADIENT_COLORS[(frame + i) % len]);
                s.push(ch);
            }
            s.push_str(&RESET.to_string());
            let _ = out.write_all(s.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_activity_cannot_repaint_after_the_line_is_cleared() {
        let stopped = AtomicBool::new(true);
        let mut output = Vec::new();

        draw_if_active_to(
            &stopped,
            &mut output,
            0,
            "Running grep…",
            LoaderStyle::Spinner,
        );

        assert!(output.is_empty(), "late frame escaped: {output:?}");
    }
}
