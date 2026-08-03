use crate::theme::{colors_enabled, fade_ramp, rule, ACCENT, ACCENT_DEEP, BOLD, MUTED, PAD, RESET};

#[cfg(test)]
const LOGO_NAME: &str = "ALBATROSS";
const LOGO: &str = r"   █████╗ ██╗     ██████╗  █████╗ ████████╗██████╗  ██████╗ ███████╗███████╗
  ██╔══██╗██║     ██╔══██╗██╔══██╗╚══██╔══╝██╔══██╗██╔═══██╗██╔════╝██╔════╝
  ███████║██║     ██████╔╝███████║   ██║   ██████╔╝██║   ██║███████╗███████╗
  ██╔══██║██║     ██╔══██╗██╔══██║   ██║   ██╔══██╗██║   ██║╚════██║╚════██║
  ██║  ██║███████╗██████╔╝██║  ██║   ██║   ██║  ██║╚██████╔╝███████║███████║
  ╚═╝  ╚═╝╚══════╝╚═════╝ ╚═╝  ╚═╝   ╚═╝   ╚═╝  ╚═╝ ╚═════╝ ╚══════╝╚══════╝";

pub struct BannerInfo<'a> {
    pub model: &'a str,
    pub backend: &'a str,
    pub approval: &'a str,
}

/// One `label  value` row with an aligned, readable label.
fn row(label: &str, value: &str) -> String {
    format!("{PAD}{MUTED}{label:<9}{RESET}{ACCENT}{value}{RESET}")
}

/// The logo, colored per-line across the cyan segment of the shared fade
/// ramp (the ramp's gray tail is reserved for the trailing-off `fade_header`
/// rule and would make the bottom logo rows illegible here). Falls back to
/// the previous flat `ACCENT_DEEP` when colors are disabled.
fn gradient_logo() -> String {
    if !colors_enabled() {
        return format!("{ACCENT_DEEP}{BOLD}{LOGO}{RESET}");
    }
    // Use the colorful half of the active theme's ramp; the gray tail (indices
    // 9..=11) is deliberately excluded here.
    const COLOR_SEGMENT_END: usize = 8;
    let lines: Vec<&str> = LOGO.lines().collect();
    let last = lines.len().saturating_sub(1).max(1);
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        let idx = (i * COLOR_SEGMENT_END) / last;
        out.push_str(&format!(
            "{BOLD}\x1b[38;5;{}m{line}{RESET}\n",
            fade_ramp()[idx]
        ));
    }
    out
}

pub fn print_banner(info: BannerInfo<'_>) {
    println!();
    print!("{}", gradient_logo());
    println!();
    println!(
        "{PAD}{BOLD}Albatross v{}{RESET}  {MUTED}— a small, terminal-first coding harness{RESET}",
        env!("CARGO_PKG_VERSION")
    );
    println!("{}", row("provider", info.backend));
    println!("{}", row("model", info.model));
    println!("{}", row("approval", info.approval));
    println!("{}", rule());
    println!(
        "{PAD}{MUTED}/help{RESET} commands  {MUTED}·{RESET}  {MUTED}/provider /model{RESET} switch  {MUTED}·{RESET}  {MUTED}/exit{RESET} quit"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // gradient_logo() reads a process-global color switch; serialize the two
    // color-mode tests so they don't race with each other or with theme.rs's
    // own switch-flipping test.
    static COLOR_SWITCH_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn with_colors(enabled: bool, f: impl FnOnce()) {
        let _guard = COLOR_SWITCH_TEST_LOCK.lock().unwrap();
        crate::theme::init(
            if enabled {
                crate::config::ColorMode::Always
            } else {
                crate::config::ColorMode::Never
            },
            false,
            crate::config::ThemePreset::Cyan,
        );
        f();
        crate::theme::init(
            crate::config::ColorMode::Always,
            false,
            crate::config::ThemePreset::Cyan,
        );
    }

    #[test]
    fn gradient_logo_has_no_escapes_when_colors_disabled() {
        with_colors(false, || {
            let logo = gradient_logo();
            assert!(!logo.contains('\x1b'));
            assert!(logo.contains("███████╗")); // still the same art, just uncolored
            assert_eq!(LOGO_NAME, "ALBATROSS");
        });
    }

    #[test]
    fn albatross_wordmark_has_uniform_rows() {
        let widths = LOGO
            .lines()
            .map(|line| line.chars().count())
            .collect::<Vec<_>>();
        assert_eq!(widths.len(), 6);
        assert!(widths.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn gradient_logo_colors_every_line_when_enabled() {
        with_colors(true, || {
            let logo = gradient_logo();
            let line_count = LOGO.lines().count();
            assert_eq!(logo.matches("\x1b[38;5;").count(), line_count);
        });
    }
}
