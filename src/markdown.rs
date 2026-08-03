//! Incremental inline-markdown styling for streamed assistant text.
//!
//! [`MarkdownInline`] sits between the raw model deltas and the answer word
//! wrapper: it recognizes a small, high-value subset of markdown — bold,
//! italic, inline code, ATX headings, list bullets, and fenced code blocks —
//! and emits [`MdEvent`]s. Text events carry ANSI-styled text (the wrapper is
//! ANSI-blind, so the injected codes never skew wrapping); `CodeStart`/`CodeEnd`
//! are out-of-band signals the renderer turns into framing rules and a
//! verbatim (no-wrap) region.
//!
//! Design notes:
//! - **Chunk-boundary safe.** Markers can be split across deltas (`**` arriving
//!   as `*` then `*`). Undecidable trailing runs are held in `pending` and
//!   re-examined on the next `feed`; `finish` flushes whatever remains as
//!   literal text.
//! - **Conservative inline.** Emphasis uses simple flanking rules so `2 * 3`
//!   and `snake_case` are left alone. Unclosed markers reset at the next hard
//!   newline, so a stray `*` never bleeds styling across the whole answer.
//! - **NO_COLOR aware for free.** Style codes come from [`crate::theme`] and
//!   render empty when color is disabled, so markers are still *stripped* and
//!   bullets substituted — standard rich-CLI behavior — with one code path.

use crate::theme::{ACCENT, BOLD, BULLET, ITALIC, RESET};

/// An event produced while parsing streamed markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MdEvent {
    /// Styled, ready-to-wrap text (ANSI codes already injected).
    Text(String),
    /// Enter a fenced code block; the fence line has been consumed.
    CodeStart { lang: String },
    /// Leave a fenced code block.
    CodeEnd,
}

pub struct MarkdownInline {
    bold: bool,
    italic: bool,
    code: bool,
    heading: bool,
    in_fence: bool,
    at_line_start: bool,
    /// Last visible char emitted, for emphasis flanking. `'\n'` at line start.
    prev: char,
    /// Undecided trailing chars carried to the next `feed`.
    pending: String,
}

impl Default for MarkdownInline {
    fn default() -> Self {
        Self::new()
    }
}

impl MarkdownInline {
    pub fn new() -> Self {
        Self {
            bold: false,
            italic: false,
            code: false,
            heading: false,
            in_fence: false,
            at_line_start: true,
            prev: '\n',
            pending: String::new(),
        }
    }

    /// ANSI codes for whichever inline styles are currently active — emitted
    /// after a `RESET` so closing one marker doesn't drop the others.
    fn active_codes(&self) -> String {
        let mut s = String::new();
        if self.heading {
            s.push_str(&format!("{BOLD}{ACCENT}"));
        }
        if self.bold {
            s.push_str(&format!("{BOLD}"));
        }
        if self.italic {
            s.push_str(&format!("{ITALIC}"));
        }
        if self.code {
            s.push_str(&format!("{ACCENT}"));
        }
        s
    }

    fn open(&mut self, text: &mut String, code: &str) {
        text.push_str(code);
    }

    fn close(&mut self, text: &mut String) {
        text.push_str(&format!("{RESET}"));
        text.push_str(&self.active_codes());
    }

    pub fn feed(&mut self, delta: &str) -> Vec<MdEvent> {
        let mut events = Vec::new();
        let mut text = String::new();
        let buf: Vec<char> = self.pending.chars().chain(delta.chars()).collect();
        self.pending.clear();
        let mut i = 0;

        'outer: while i < buf.len() {
            let ch = buf[i];

            // ---- Inside a fenced code block: raw passthrough + close watch ----
            if self.in_fence {
                if self.at_line_start && ch == '`' {
                    let run = backtick_run(&buf, i);
                    if run.end == buf.len() && run.len < 3 {
                        self.pending = buf[i..].iter().collect();
                        break 'outer;
                    }
                    if run.len >= 3 {
                        // Need the whole fence line consumed before closing.
                        let Some(nl) = find_newline(&buf, run.end) else {
                            self.pending = buf[i..].iter().collect();
                            break 'outer;
                        };
                        if !text.is_empty() {
                            events.push(MdEvent::Text(std::mem::take(&mut text)));
                        }
                        events.push(MdEvent::CodeEnd);
                        self.in_fence = false;
                        self.at_line_start = true;
                        self.prev = '\n';
                        i = nl + 1;
                        continue;
                    }
                    // 1–2 backticks then content: ordinary code text.
                    for _ in 0..run.len {
                        text.push('`');
                    }
                    self.at_line_start = false;
                    self.prev = '`';
                    i = run.end;
                    continue;
                }
                text.push(ch);
                self.at_line_start = ch == '\n';
                self.prev = ch;
                i += 1;
                continue;
            }

            // ---- Line-start structures: fence open, heading, bullet ----
            if self.at_line_start {
                if ch == ' ' || ch == '\t' {
                    // Preserve leading indentation; stay at line start so a
                    // bullet/heading after indent is still recognized.
                    text.push(ch);
                    self.prev = ch;
                    i += 1;
                    continue;
                }
                if ch == '`' {
                    let run = backtick_run(&buf, i);
                    if run.end == buf.len() && run.len < 3 {
                        self.pending = buf[i..].iter().collect();
                        break 'outer;
                    }
                    if run.len >= 3 {
                        let Some(nl) = find_newline(&buf, run.end) else {
                            self.pending = buf[i..].iter().collect();
                            break 'outer;
                        };
                        let lang: String = buf[run.end..nl].iter().collect();
                        if !text.is_empty() {
                            events.push(MdEvent::Text(std::mem::take(&mut text)));
                        }
                        events.push(MdEvent::CodeStart {
                            lang: lang.trim().to_string(),
                        });
                        self.in_fence = true;
                        self.at_line_start = true;
                        self.prev = '\n';
                        i = nl + 1;
                        continue;
                    }
                    // 1–2 backticks: fall through to inline code handling.
                }
                if ch == '#' {
                    let hashes = hash_run(&buf, i);
                    if hashes.end == buf.len() {
                        self.pending = buf[i..].iter().collect();
                        break 'outer;
                    }
                    if hashes.len <= 6 && buf[hashes.end] == ' ' {
                        self.heading = true;
                        self.open(&mut text, &format!("{BOLD}{ACCENT}"));
                        self.at_line_start = false;
                        self.prev = ' ';
                        i = hashes.end + 1;
                        continue;
                    }
                    // Not a heading (e.g. "#tag"): emit hashes literally below.
                }
                if ch == '-' || ch == '*' {
                    let Some(&nxt) = buf.get(i + 1) else {
                        self.pending = buf[i..].iter().collect();
                        break 'outer;
                    };
                    if nxt == ' ' {
                        text.push_str(&format!("{ACCENT}{BULLET}{RESET} "));
                        self.at_line_start = false;
                        self.prev = ' ';
                        i += 2;
                        continue;
                    }
                    // Not a bullet: `*` may still open emphasis below.
                }
            }

            // ---- Inside an inline code span: only backtick / newline matter ----
            if self.code {
                match ch {
                    '`' => {
                        self.code = false;
                        self.close(&mut text);
                        self.prev = '`';
                        self.at_line_start = false;
                        i += 1;
                    }
                    '\n' => {
                        self.reset_line_styles(&mut text);
                        text.push('\n');
                        self.at_line_start = true;
                        self.prev = '\n';
                        i += 1;
                    }
                    _ => {
                        text.push(ch);
                        self.prev = ch;
                        self.at_line_start = false;
                        i += 1;
                    }
                }
                continue;
            }

            // ---- Inline styling ----
            match ch {
                '\n' => {
                    self.reset_line_styles(&mut text);
                    text.push('\n');
                    self.at_line_start = true;
                    self.prev = '\n';
                    i += 1;
                }
                '`' => {
                    self.code = true;
                    self.open(&mut text, &format!("{ACCENT}"));
                    self.prev = '`';
                    self.at_line_start = false;
                    i += 1;
                }
                '*' => {
                    let Some(&nxt) = buf.get(i + 1) else {
                        self.pending = buf[i..].iter().collect();
                        break 'outer;
                    };
                    if nxt == '*' {
                        self.toggle_bold(&mut text);
                        self.prev = '*';
                        self.at_line_start = false;
                        i += 2;
                    } else {
                        if !self.try_emphasis('*', nxt, &mut text) {
                            text.push('*');
                        }
                        self.prev = '*';
                        self.at_line_start = false;
                        i += 1;
                    }
                }
                '_' => {
                    let Some(&nxt) = buf.get(i + 1) else {
                        self.pending = buf[i..].iter().collect();
                        break 'outer;
                    };
                    if !self.try_emphasis('_', nxt, &mut text) {
                        text.push('_');
                    }
                    self.prev = '_';
                    self.at_line_start = false;
                    i += 1;
                }
                _ => {
                    text.push(ch);
                    self.prev = ch;
                    self.at_line_start = false;
                    i += 1;
                }
            }
        }

        if !text.is_empty() {
            events.push(MdEvent::Text(text));
        }
        events
    }

    pub fn finish(&mut self) -> Vec<MdEvent> {
        let mut events = Vec::new();
        let mut text = String::new();
        // Any undecided markers become literal text at end of stream.
        if !self.pending.is_empty() {
            text.push_str(&std::mem::take(&mut self.pending));
        }
        self.reset_line_styles(&mut text);
        if !text.is_empty() {
            events.push(MdEvent::Text(text));
        }
        if self.in_fence {
            events.push(MdEvent::CodeEnd);
            self.in_fence = false;
        }
        events
    }

    fn reset_line_styles(&mut self, text: &mut String) {
        if self.bold || self.italic || self.code || self.heading {
            text.push_str(&format!("{RESET}"));
            self.bold = false;
            self.italic = false;
            self.code = false;
            self.heading = false;
        }
    }

    fn toggle_bold(&mut self, text: &mut String) {
        if self.bold {
            self.bold = false;
            self.close(text);
        } else {
            self.bold = true;
            self.open(text, &format!("{BOLD}"));
        }
    }

    /// Attempt to open or close emphasis at a `*`/`_` marker using simple
    /// flanking rules. Returns whether the marker was consumed as emphasis
    /// (vs. left as a literal character). `next` is the following char.
    fn try_emphasis(&mut self, marker: char, next: char, text: &mut String) -> bool {
        if self.italic {
            // Close only when the marker hugs the end of a word.
            if !self.prev.is_whitespace() {
                self.italic = false;
                self.close(text);
                return true;
            }
            false
        } else {
            let prev_ok =
                self.prev == '\n' || self.prev.is_whitespace() || self.prev.is_ascii_punctuation();
            let next_ok = !next.is_whitespace();
            // `_` must not sit inside a word (protects snake_case).
            let underscore_intraword =
                marker == '_' && self.prev.is_alphanumeric() && next.is_alphanumeric();
            if prev_ok && next_ok && !underscore_intraword {
                self.italic = true;
                self.open(text, &format!("{ITALIC}"));
                true
            } else {
                false
            }
        }
    }
}

struct Run {
    len: usize,
    end: usize,
}

fn backtick_run(buf: &[char], start: usize) -> Run {
    let mut end = start;
    while end < buf.len() && buf[end] == '`' {
        end += 1;
    }
    Run {
        len: end - start,
        end,
    }
}

fn hash_run(buf: &[char], start: usize) -> Run {
    let mut end = start;
    while end < buf.len() && buf[end] == '#' {
        end += 1;
    }
    Run {
        len: end - start,
        end,
    }
}

fn find_newline(buf: &[char], from: usize) -> Option<usize> {
    (from..buf.len()).find(|&k| buf[k] == '\n')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ColorMode;

    /// Concatenate the text of all events (ignoring fence signals).
    fn text_of(events: &[MdEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                MdEvent::Text(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    /// Feed `input` one char at a time (worst case) and collect all events.
    fn feed_char_by_char(input: &str) -> Vec<MdEvent> {
        crate::theme::init(ColorMode::Always, false, crate::config::ThemePreset::Cyan);
        let mut md = MarkdownInline::new();
        let mut events = Vec::new();
        for ch in input.chars() {
            events.extend(md.feed(&ch.to_string()));
        }
        events.extend(md.finish());
        events
    }

    #[test]
    fn bold_marker_split_across_chunks() {
        crate::theme::init(ColorMode::Always, false, crate::config::ThemePreset::Cyan);
        let mut md = MarkdownInline::new();
        let mut events = md.feed("say **bo");
        events.extend(md.feed("ld** now"));
        events.extend(md.finish());
        let out = text_of(&events);
        assert!(out.contains("\x1b[1mbold"));
        assert!(out.contains("say "));
        assert!(out.contains(" now"));
        assert!(!out.contains('*'));
    }

    #[test]
    fn italic_with_star_and_underscore() {
        let star = text_of(&feed_char_by_char("an *italic* word"));
        assert!(star.contains("\x1b[3mitalic"));
        assert!(!star.contains('*'));
        let under = text_of(&feed_char_by_char("an _italic_ word"));
        assert!(under.contains("\x1b[3mitalic"));
        assert!(!under.contains('_'));
    }

    #[test]
    fn snake_case_is_not_italicized() {
        let out = text_of(&feed_char_by_char("call some_function_name here"));
        assert!(out.contains("some_function_name"));
        assert!(!out.contains("\x1b[3m"));
    }

    #[test]
    fn math_asterisks_are_not_styled() {
        let out = text_of(&feed_char_by_char("compute 2 * 3 * 4"));
        assert!(out.contains("2 * 3 * 4"));
        assert!(!out.contains("\x1b[3m"));
    }

    #[test]
    fn inline_code_is_colored_and_closes() {
        let out = text_of(&feed_char_by_char("run `cargo test` now"));
        assert!(out.contains("\x1b[96mcargo test"));
        assert!(!out.contains('`'));
    }

    #[test]
    fn unclosed_bold_resets_at_newline() {
        let out = text_of(&feed_char_by_char("**bold never closes\nnext line"));
        // A RESET appears before the newline, and the second line is unstyled.
        let lines: Vec<&str> = out.split('\n').collect();
        assert!(lines[0].contains("\x1b[1m"));
        assert!(lines[0].contains("\x1b[0m"));
        assert!(!lines[1].contains("\x1b[1m"));
    }

    #[test]
    fn unclosed_marker_flushes_literal_at_finish() {
        // A trailing lone '*' with nothing after it stays literal.
        let out = text_of(&feed_char_by_char("trailing star *"));
        assert!(out.ends_with('*'));
    }

    #[test]
    fn heading_styles_only_its_line() {
        let out = text_of(&feed_char_by_char("## Title\nbody text"));
        let lines: Vec<&str> = out.split('\n').collect();
        assert!(lines[0].contains("\x1b[1m"));
        assert!(lines[0].contains("Title"));
        assert!(!lines[0].contains('#'));
        assert!(!lines[1].contains("\x1b[1m"));
        assert!(lines[1].contains("body text"));
    }

    #[test]
    fn bullet_marker_replaced_same_visible_width() {
        let out = text_of(&feed_char_by_char("- first item"));
        // The "- " became "• " (bullet + space) — same 2 visible columns.
        assert!(out.contains('•'));
        assert!(!out.contains("- first"));
        // Visible width of the leading marker is still 2.
        assert_eq!(crate::theme::visible_len(&out).min(2), 2);
    }

    #[test]
    fn fenced_block_emits_start_and_end_with_language() {
        let events = feed_char_by_char("before\n```rust\nlet x = 1;\n```\nafter");
        assert!(events
            .iter()
            .any(|e| matches!(e, MdEvent::CodeStart { lang } if lang == "rust")));
        assert!(events.iter().any(|e| matches!(e, MdEvent::CodeEnd)));
        // The fence lines themselves are not emitted as text.
        let out = text_of(&events);
        assert!(!out.contains("```"));
        assert!(out.contains("let x = 1;"));
        assert!(out.contains("before"));
        assert!(out.contains("after"));
    }

    #[test]
    fn fence_backticks_split_across_chunks() {
        crate::theme::init(ColorMode::Always, false, crate::config::ThemePreset::Cyan);
        let mut md = MarkdownInline::new();
        let mut events = md.feed("``");
        events.extend(md.feed("`py\ncode\n```\n"));
        events.extend(md.finish());
        assert!(events
            .iter()
            .any(|e| matches!(e, MdEvent::CodeStart { lang } if lang == "py")));
        assert!(events.iter().any(|e| matches!(e, MdEvent::CodeEnd)));
    }

    #[test]
    fn no_inline_styling_inside_fence() {
        let events = feed_char_by_char("```\nnot **bold** and not _italic_\n```\n");
        let out = text_of(&events);
        // The markers survive verbatim inside the fence.
        assert!(out.contains("**bold**"));
        assert!(out.contains("_italic_"));
        assert!(!out.contains("\x1b[1m"));
    }

    #[test]
    fn no_color_strips_markers_without_escapes() {
        crate::theme::init(ColorMode::Never, false, crate::config::ThemePreset::Cyan);
        let mut md = MarkdownInline::new();
        let mut events = md.feed("**bold** and `code` and # not-heading");
        events.extend(md.finish());
        let out = text_of(&events);
        assert!(!out.contains('\x1b'));
        assert!(out.contains("bold"));
        assert!(out.contains("code"));
        assert!(!out.contains("**"));
        crate::theme::init(ColorMode::Always, false, crate::config::ThemePreset::Cyan);
    }
}
