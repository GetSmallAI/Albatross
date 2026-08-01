use std::io::Write;
use std::sync::{Mutex, OnceLock};

use crate::config::LoaderStyle;
use crate::theme::{ACCENT, BOLD, DOT, PAD, RESET};

#[derive(Default)]
struct ActivityRegion {
    current: Option<String>,
}

impl ActivityRegion {
    fn replace(&mut self, text: &str) -> String {
        if self.current.as_deref() == Some(text) {
            return String::new();
        }
        let mut output = self.clear();
        output.push_str(&format!(
            "\r\n\r{PAD}{ACCENT}{DOT}{RESET} {BOLD}{text}{RESET}"
        ));
        self.current = Some(text.to_string());
        output
    }

    fn clear(&mut self) -> String {
        if self.current.take().is_none() {
            return String::new();
        }
        "\x1b[1A\r\x1b[0J".to_string()
    }
}

static TERMINAL_OUTPUT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn terminal_output_lock() -> &'static Mutex<()> {
    TERMINAL_OUTPUT_LOCK.get_or_init(|| Mutex::new(()))
}

pub struct Loader {
    region: ActivityRegion,
}

impl Loader {
    pub fn start(text: String, _style: LoaderStyle) -> Self {
        let mut loader = Self {
            region: ActivityRegion::default(),
        };
        loader.update(text);
        loader
    }

    pub fn update(&mut self, text: String) {
        let _guard = terminal_output_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut out = std::io::stdout();
        let _ = write!(out, "{}", self.region.replace(&text));
        let _ = out.flush();
    }

    pub fn stop(mut self) {
        self.clear();
    }

    fn clear(&mut self) {
        let _guard = terminal_output_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut out = std::io::stdout();
        let _ = write!(out, "{}", self.region.clear());
        let _ = out.flush();
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_phase_owns_a_leading_gap_for_stable_handoffs() {
        let mut region = ActivityRegion::default();

        let thinking = region.replace("Thinking");
        let tools = region.replace("Using tools");
        let cleared = region.clear();

        assert!(thinking.contains("Thinking"));
        assert!(
            thinking.starts_with("\r\n\r"),
            "activity should reserve the same leading gap as tools and responses: {thinking:?}"
        );
        assert!(tools.starts_with("\x1b[1A\r\x1b[0J\r\n\r"));
        assert!(tools.contains("Using tools"));
        assert_eq!(cleared, "\x1b[1A\r\x1b[0J");
    }

    #[test]
    fn unchanged_activity_phase_does_not_repaint_the_terminal() {
        let mut region = ActivityRegion::default();
        let _ = region.replace("Thinking");

        assert_eq!(region.replace("Thinking"), "");
    }

    #[test]
    fn cleared_activity_has_no_background_frame_that_can_repaint() {
        let mut region = ActivityRegion::default();
        let _ = region.replace("Thinking");
        let _ = region.clear();

        assert_eq!(region.clear(), "");
    }
}
