//! Centralized TUI theme: a small high-contrast palette plus layout helpers so
//! every surface (banner, input, per-turn renderer, status line) looks
//! consistent.
//!
//! Readability note: we deliberately avoid ANSI "faint" (`\x1b[2m`). Faint
//! renders as washed-out, hard-to-read text on many terminals and themes — it
//! was the main reason the old TUI looked dim. Secondary text uses bright-black
//! (`MUTED`) instead, which stays legible on both light and dark backgrounds.
//!
//! Runtime switches: colors and glyphs honor a process-wide switch set once at
//! startup by [`init`]. `Style` renders its escape code only while colors are
//! enabled (NO_COLOR / `display.color` / non-tty aware), and `Sym` picks an
//! ASCII fallback when `display.ascii` is set. Because both implement
//! `Display`, every existing `format!("{ACCENT}…{RESET}")` site works
//! unchanged.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::ColorMode;

static COLORS_ENABLED: AtomicBool = AtomicBool::new(true);
static ASCII_SYMBOLS: AtomicBool = AtomicBool::new(false);

pub fn colors_enabled() -> bool {
    COLORS_ENABLED.load(Ordering::Relaxed)
}

pub fn ascii_enabled() -> bool {
    ASCII_SYMBOLS.load(Ordering::Relaxed)
}

/// Resolve and apply the color/glyph switches. Call once per process entry
/// point (interactive, one-shot, eval) right after config load, before any UI
/// output.
pub fn init(color: ColorMode, ascii: bool) {
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    let term_dumb = std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false);
    let is_tty = std::io::stdout().is_terminal();
    COLORS_ENABLED.store(
        resolve_color_mode(color, no_color, term_dumb, is_tty),
        Ordering::Relaxed,
    );
    ASCII_SYMBOLS.store(ascii, Ordering::Relaxed);
}

/// Pure color-mode resolution (kept side-effect-free so precedence is unit
/// testable). `always` deliberately overrides NO_COLOR: per no-color.org,
/// user-level configuration that explicitly requests color wins.
pub fn resolve_color_mode(mode: ColorMode, no_color: bool, term_dumb: bool, is_tty: bool) -> bool {
    match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => !no_color && !term_dumb && is_tty,
    }
}

/// An ANSI style that renders its escape code only while colors are enabled.
/// Zero-cost to copy; interpolates directly in `format!` strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style(&'static str);

impl std::fmt::Display for Style {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if colors_enabled() {
            f.write_str(self.0)
        } else {
            Ok(())
        }
    }
}

pub const RESET: Style = Style("\x1b[0m");
pub const BOLD: Style = Style("\x1b[1m");
pub const ITALIC: Style = Style("\x1b[3m");

/// Accent — prompts, headers, the active step. Bright cyan.
pub const ACCENT: Style = Style("\x1b[96m");
/// A slightly deeper accent for large fills (the logo).
pub const ACCENT_DEEP: Style = Style("\x1b[36m");
/// Primary content: the terminal's default foreground (max contrast).
pub const TEXT: Style = Style("\x1b[0m");
/// Secondary text — labels, summaries, hints. Bright-black, NOT faint.
pub const MUTED: Style = Style("\x1b[90m");
pub const SUCCESS: Style = Style("\x1b[92m");
pub const WARN: Style = Style("\x1b[93m");
pub const ERROR: Style = Style("\x1b[91m");
pub const MAGENTA: Style = Style("\x1b[95m");

/// A glyph with an ASCII fallback, selected by the `display.ascii` switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sym(&'static str, &'static str);

impl std::fmt::Display for Sym {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if ascii_enabled() { self.1 } else { self.0 })
    }
}

pub const OK: Sym = Sym("✓", "+");
pub const FAIL: Sym = Sym("✗", "x");
pub const POINT: Sym = Sym("▸", ">");
pub const DOT: Sym = Sym("●", "*");
pub const CHECK: Sym = Sym("✔", "+");
pub const PENDING: Sym = Sym("○", "o");
pub const SUB: Sym = Sym("↳", ">");
pub const WARN_MARK: Sym = Sym("▲", "!");
pub const BULLET: Sym = Sym("•", "*");
pub const PROMPT_CHAR: Sym = Sym("❯", ">");
pub const BRANCH_END: Sym = Sym("└", "`");
pub const BOLT: Sym = Sym("⚡", "*");
pub const BANG: Sym = Sym("!", "!");
pub const HOOK_STOP: Sym = Sym("■", "!");

fn rule_char() -> &'static str {
    if ascii_enabled() {
        "-"
    } else {
        "─"
    }
}

/// Left margin every block shares, so the transcript has a consistent gutter.
pub const PAD: &str = "  ";

/// The real terminal width in columns (clamped to a sane range). Unlike a fixed
/// cap, this lets wrapped content fill the window the way naturally-wrapped
/// terminal text does.
pub fn cols() -> usize {
    crossterm::terminal::size()
        .map(|(c, _)| c as usize)
        .unwrap_or(80)
        .clamp(20, 400)
}

/// Usable width for wrapped body text: the terminal minus the left gutter and a
/// one-column right breathing margin.
pub fn content_width() -> usize {
    cols().saturating_sub(PAD.len() + 1).max(20)
}

/// Visible width of a string, ignoring ANSI escape sequences (`ESC [ … m`).
/// Used anywhere layout must count columns of already-styled text — the
/// input prompt and the streamed-answer word-wrapper both rely on it.
pub fn visible_len(s: &str) -> usize {
    let mut n = 0;
    let mut in_esc = false;
    for ch in s.chars() {
        if in_esc {
            if ch == 'm' {
                in_esc = false;
            }
        } else if ch == '\x1b' {
            in_esc = true;
        } else {
            n += 1;
        }
    }
    n
}

/// A muted full-width horizontal rule, indented by `PAD`.
pub fn rule() -> String {
    let dashes = cols().saturating_sub(PAD.len());
    format!("{PAD}{MUTED}{}{RESET}", rule_char().repeat(dashes))
}

/// A dim rule that frames a fenced code block. With a language it reads
/// `── rust ─────`; the closing rule (empty language) is just dashes. No
/// leading `PAD` — the renderer positions it directly under the answer gutter.
pub fn code_fence_rule(lang: &str) -> String {
    let target = content_width().min(48);
    let rc = rule_char();
    let lang = lang.trim();
    if lang.is_empty() {
        format!("{MUTED}{}{RESET}", rc.repeat(target))
    } else {
        let label = format!("{rc}{rc} {lang} ");
        let remain = target.saturating_sub(visible_len(&label));
        format!("{MUTED}{label}{}{RESET}", rc.repeat(remain))
    }
}

/// 256-color ramp used by the fading turn headers (and the banner logo):
/// bright cyan → teal → dark gray.
pub(crate) const FADE_RAMP: [u8; 12] = [51, 45, 39, 38, 37, 31, 30, 24, 23, 237, 235, 234];

/// A turn header: an accent label followed by a short rule (~20% of the width)
/// that fades from bright cyan to dark, e.g. `response ──────╴`. The fade is
/// done per-character with 256-color codes so it reads as a soft taper rather
/// than a hard-edged bar. No bottom or side borders — just this top accent.
pub fn fade_header(label: &str) -> String {
    fade_header_at_width(label, cols())
}

/// Render a fading turn header for a known terminal width.
///
/// Most call sites should use [`fade_header`]. Components that already receive
/// a measured width use this variant so their layout and the shared turn-header
/// treatment are calculated from the same value.
pub(crate) fn fade_header_at_width(label: &str, width: usize) -> String {
    let len = (width / 5).clamp(6, 30);
    if !colors_enabled() {
        return format!("{PAD}{label} {}", rule_char().repeat(len));
    }
    let last = FADE_RAMP.len() - 1;
    let denom = len.saturating_sub(1).max(1);
    let mut fade = String::new();
    for i in 0..len {
        let idx = ((i * last) / denom).min(last);
        fade.push_str(&format!("\x1b[38;5;{}m{}", FADE_RAMP[idx], rule_char()));
    }
    format!("{PAD}{ACCENT}{BOLD}{label}{RESET} {fade}{RESET}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_color_mode_precedence() {
        use ColorMode::*;
        // Always wins over everything, including NO_COLOR and non-tty.
        assert!(resolve_color_mode(Always, true, true, false));
        // Never wins over everything.
        assert!(!resolve_color_mode(Never, false, false, true));
        // Auto: on only for a tty without NO_COLOR / TERM=dumb.
        assert!(resolve_color_mode(Auto, false, false, true));
        assert!(!resolve_color_mode(Auto, true, false, true)); // NO_COLOR set
        assert!(!resolve_color_mode(Auto, false, true, true)); // TERM=dumb
        assert!(!resolve_color_mode(Auto, false, false, false)); // piped
    }

    #[test]
    fn style_and_sym_render_by_switch() {
        // Tests share one process; exercise both switch states in a single
        // serialized test and restore the defaults afterwards.
        COLORS_ENABLED.store(true, Ordering::Relaxed);
        ASCII_SYMBOLS.store(false, Ordering::Relaxed);
        assert_eq!(format!("{ACCENT}"), "\x1b[96m");
        assert_eq!(format!("{OK}"), "✓");

        COLORS_ENABLED.store(false, Ordering::Relaxed);
        ASCII_SYMBOLS.store(true, Ordering::Relaxed);
        assert_eq!(format!("{ACCENT}"), "");
        assert_eq!(format!("{OK}"), "+");
        assert!(fade_header("you").contains("you"));
        assert!(!fade_header("you").contains('\x1b'));

        COLORS_ENABLED.store(true, Ordering::Relaxed);
        ASCII_SYMBOLS.store(false, Ordering::Relaxed);
    }
}
