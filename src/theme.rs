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
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::config::{ColorMode, ThemePreset};

static COLORS_ENABLED: AtomicBool = AtomicBool::new(true);
static ASCII_SYMBOLS: AtomicBool = AtomicBool::new(false);
static THEME_PRESET: AtomicU8 = AtomicU8::new(0);

pub fn colors_enabled() -> bool {
    COLORS_ENABLED.load(Ordering::Relaxed)
}

pub fn ascii_enabled() -> bool {
    ASCII_SYMBOLS.load(Ordering::Relaxed)
}

/// Resolve and apply the color/glyph switches. Call once per process entry
/// point (interactive, one-shot, eval) right after config load, before any UI
/// output.
pub fn init(color: ColorMode, ascii: bool, theme: ThemePreset) {
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    let term_dumb = std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false);
    let is_tty = std::io::stdout().is_terminal();
    COLORS_ENABLED.store(
        resolve_color_mode(color, no_color, term_dumb, is_tty),
        Ordering::Relaxed,
    );
    ASCII_SYMBOLS.store(ascii, Ordering::Relaxed);
    THEME_PRESET.store(theme_index(theme), Ordering::Relaxed);
}

fn theme_index(theme: ThemePreset) -> u8 {
    match theme {
        ThemePreset::Cyan => 0,
        ThemePreset::Mono => 1,
        ThemePreset::Green => 2,
        ThemePreset::Amber => 3,
    }
}

pub fn current_preset() -> ThemePreset {
    match THEME_PRESET.load(Ordering::Relaxed) {
        1 => ThemePreset::Mono,
        2 => ThemePreset::Green,
        3 => ThemePreset::Amber,
        _ => ThemePreset::Cyan,
    }
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
enum StyleValue {
    Fixed(&'static str),
    Role(StyleRole),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StyleRole {
    Accent,
    AccentDeep,
    Muted,
    Success,
    Warn,
    Error,
    Magenta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style(StyleValue);

fn role_code(theme: ThemePreset, role: StyleRole) -> &'static str {
    match (theme, role) {
        (ThemePreset::Cyan, StyleRole::Accent) => "\x1b[96m",
        (ThemePreset::Cyan, StyleRole::AccentDeep) => "\x1b[36m",
        (ThemePreset::Cyan, StyleRole::Muted) => "\x1b[90m",
        (ThemePreset::Cyan, StyleRole::Success) => "\x1b[92m",
        (ThemePreset::Cyan, StyleRole::Warn) => "\x1b[93m",
        (ThemePreset::Cyan, StyleRole::Error) => "\x1b[91m",
        (ThemePreset::Cyan, StyleRole::Magenta) => "\x1b[95m",
        (ThemePreset::Mono, StyleRole::Muted) => "\x1b[90m",
        (ThemePreset::Mono, _) => "\x1b[39m",
        (ThemePreset::Green, StyleRole::Accent) => "\x1b[92m",
        (ThemePreset::Green, StyleRole::AccentDeep) => "\x1b[32m",
        (ThemePreset::Green, StyleRole::Muted) => "\x1b[90m",
        (ThemePreset::Green, StyleRole::Warn) => "\x1b[93m",
        (ThemePreset::Green, StyleRole::Error) => "\x1b[91m",
        (ThemePreset::Green, _) => "\x1b[92m",
        (ThemePreset::Amber, StyleRole::Accent) => "\x1b[93m",
        (ThemePreset::Amber, StyleRole::AccentDeep) => "\x1b[33m",
        (ThemePreset::Amber, StyleRole::Muted) => "\x1b[90m",
        (ThemePreset::Amber, StyleRole::Success) => "\x1b[92m",
        (ThemePreset::Amber, StyleRole::Error) => "\x1b[91m",
        (ThemePreset::Amber, _) => "\x1b[93m",
    }
}

impl std::fmt::Display for Style {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if colors_enabled() {
            match self.0 {
                StyleValue::Fixed(code) => f.write_str(code),
                StyleValue::Role(role) => f.write_str(role_code(current_preset(), role)),
            }
        } else {
            Ok(())
        }
    }
}

pub const RESET: Style = Style(StyleValue::Fixed("\x1b[0m"));
pub const BOLD: Style = Style(StyleValue::Fixed("\x1b[1m"));
pub const ITALIC: Style = Style(StyleValue::Fixed("\x1b[3m"));

/// Accent — prompts, headers, the active step. Bright cyan.
pub const ACCENT: Style = Style(StyleValue::Role(StyleRole::Accent));
/// A slightly deeper accent for large fills (the logo).
pub const ACCENT_DEEP: Style = Style(StyleValue::Role(StyleRole::AccentDeep));
/// Primary content: the terminal's default foreground (max contrast).
pub const TEXT: Style = Style(StyleValue::Fixed("\x1b[0m"));
/// Secondary text — labels, summaries, hints. Bright-black, NOT faint.
pub const MUTED: Style = Style(StyleValue::Role(StyleRole::Muted));
pub const SUCCESS: Style = Style(StyleValue::Role(StyleRole::Success));
pub const WARN: Style = Style(StyleValue::Role(StyleRole::Warn));
pub const ERROR: Style = Style(StyleValue::Role(StyleRole::Error));
pub const MAGENTA: Style = Style(StyleValue::Role(StyleRole::Magenta));

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
pub const BRANCH: Sym = Sym("├", "|");
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
const CYAN_FADE_RAMP: [u8; 12] = [51, 45, 39, 38, 37, 31, 30, 24, 23, 237, 235, 234];
const MONO_FADE_RAMP: [u8; 12] = [255, 252, 249, 246, 243, 240, 238, 237, 236, 235, 234, 232];
const GREEN_FADE_RAMP: [u8; 12] = [120, 84, 48, 42, 36, 35, 34, 28, 22, 237, 235, 234];
const AMBER_FADE_RAMP: [u8; 12] = [229, 220, 214, 208, 202, 166, 130, 94, 58, 237, 235, 234];

pub(crate) fn fade_ramp() -> &'static [u8; 12] {
    match current_preset() {
        ThemePreset::Cyan => &CYAN_FADE_RAMP,
        ThemePreset::Mono => &MONO_FADE_RAMP,
        ThemePreset::Green => &GREEN_FADE_RAMP,
        ThemePreset::Amber => &AMBER_FADE_RAMP,
    }
}

/// A turn header: an accent label followed by a short rule (~20% of the width)
/// that fades from bright cyan to dark, e.g. `response ──────╴`. The fade is
/// done per-character with 256-color codes so it reads as a soft taper rather
/// than a hard-edged bar. No bottom or side borders — just this top accent.
pub fn fade_header(label: &str) -> String {
    let len = (cols() / 5).clamp(6, 30);
    if !colors_enabled() {
        return format!("{PAD}{label} {}", rule_char().repeat(len));
    }
    let ramp = fade_ramp();
    let last = ramp.len() - 1;
    let denom = len.saturating_sub(1).max(1);
    let mut fade = String::new();
    for i in 0..len {
        let idx = ((i * last) / denom).min(last);
        fade.push_str(&format!("\x1b[38;5;{}m{}", ramp[idx], rule_char()));
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
        init(ColorMode::Always, false, ThemePreset::Cyan);
        assert_eq!(format!("{ACCENT}"), "\x1b[96m");
        assert_eq!(format!("{OK}"), "✓");

        init(ColorMode::Always, false, ThemePreset::Green);
        assert_eq!(format!("{ACCENT}"), "\x1b[92m");
        init(ColorMode::Always, false, ThemePreset::Amber);
        assert_eq!(format!("{ACCENT_DEEP}"), "\x1b[33m");
        init(ColorMode::Always, false, ThemePreset::Mono);
        assert_eq!(format!("{ERROR}"), "\x1b[39m");

        COLORS_ENABLED.store(false, Ordering::Relaxed);
        ASCII_SYMBOLS.store(true, Ordering::Relaxed);
        assert_eq!(format!("{ACCENT}"), "");
        assert_eq!(format!("{OK}"), "+");
        assert!(fade_header("you").contains("you"));
        assert!(!fade_header("you").contains('\x1b'));
        init(ColorMode::Always, false, ThemePreset::Cyan);

        COLORS_ENABLED.store(true, Ordering::Relaxed);
        ASCII_SYMBOLS.store(false, Ordering::Relaxed);
    }
}
