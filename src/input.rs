use anyhow::{anyhow, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::theme::{ACCENT, BOLD, MUTED, PAD, POINT, RESET};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryEntry {
    value: String,
}

#[derive(Debug, Clone)]
pub struct InputHistory {
    path: String,
    max_entries: usize,
    entries: Vec<String>,
    enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadLineOutcome {
    Line(String),
    Eof,
    Interrupted,
}

impl InputHistory {
    pub fn load(path: String, max_entries: usize, enabled: bool) -> Self {
        let mut entries = Vec::new();
        if enabled {
            if let Ok(text) = fs::read_to_string(&path) {
                for line in text.lines() {
                    if let Ok(entry) = serde_json::from_str::<HistoryEntry>(line) {
                        if !entry.value.trim().is_empty() {
                            entries.push(entry.value);
                        }
                    }
                }
            }
        }
        let max_entries = max_entries.max(1);
        if entries.len() > max_entries {
            entries = entries[entries.len() - max_entries..].to_vec();
        }
        Self {
            path,
            max_entries,
            entries,
            enabled,
        }
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn push(&mut self, value: &str) -> Result<()> {
        if !self.enabled || value.trim().is_empty() {
            return Ok(());
        }
        if self
            .entries
            .last()
            .map(|last| last == value)
            .unwrap_or(false)
        {
            return Ok(());
        }
        self.entries.push(value.to_string());
        if self.entries.len() > self.max_entries {
            self.entries.remove(0);
        }
        if let Some(parent) = Path::new(&self.path).parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let line = serde_json::to_string(&HistoryEntry {
            value: value.to_string(),
        })?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        Ok(())
    }
}

pub async fn plain_read_line(prompt: String) -> Result<String> {
    plain_read_line_with_history(prompt, Vec::new(), Vec::new()).await
}

/// `commands` are `(name, description)` slash-commands offered as completions
/// (empty for sub-prompts that don't want completion).
pub async fn plain_read_line_with_history(
    prompt: String,
    history: Vec<String>,
    commands: Vec<(String, String)>,
) -> Result<String> {
    match plain_read_line_with_history_outcome(prompt, history, commands).await? {
        ReadLineOutcome::Line(line) => Ok(line),
        ReadLineOutcome::Eof => Err(anyhow!("input closed")),
        ReadLineOutcome::Interrupted => {
            crate::cursor::restore();
            std::process::exit(0)
        }
    }
}

pub async fn plain_read_line_with_history_outcome(
    prompt: String,
    history: Vec<String>,
    commands: Vec<(String, String)>,
) -> Result<ReadLineOutcome> {
    tokio::task::spawn_blocking(move || read_plain_outcome(&prompt, &history, &commands)).await?
}

fn render_value(value: &str) -> String {
    value.replace('\n', "⏎")
}

/// Maximum number of command rows shown in the completion menu at once.
const MENU_MAX_ROWS: usize = 8;

/// Slash-commands the current line is a prefix of, for the completion menu.
/// Empty when: not a `/`-line, the cursor isn't at the end, completion was
/// dismissed, or the only match is exactly what's already typed.
fn completion_matches<'a>(
    line: &str,
    cursor: usize,
    len: usize,
    commands: &'a [(String, String)],
    dismissed: bool,
) -> Vec<&'a (String, String)> {
    if dismissed || cursor != len || !line.starts_with('/') {
        return Vec::new();
    }
    let matches: Vec<&(String, String)> = commands
        .iter()
        .filter(|(n, _)| n.starts_with(line))
        .collect();
    if matches.len() == 1 && matches[0].0 == line {
        return Vec::new();
    }
    matches
}

/// Build the full redraw string for the input line plus (optionally) the
/// completion menu, leaving the cursor parked at the logical edit position.
///
/// Sequence: clear the input line and everything below it, draw the prompt +
/// text + dim ghost (the selected match's remainder), then — if there are
/// matches — draw the menu on the lines beneath and move the cursor back up to
/// the input line. Pure (returns the bytes to write) so it can be unit-tested.
#[allow(clippy::too_many_arguments)]
fn render_input(
    prompt: &str,
    prompt_cols: usize,
    chars: &[char],
    cursor: usize,
    commands: &[(String, String)],
    sel: usize,
    dismissed: bool,
    term_cols: usize,
) -> String {
    let line: String = chars.iter().collect();
    let display = render_value(&line);
    let matches = completion_matches(&line, cursor, chars.len(), commands, dismissed);
    let sel = if matches.is_empty() {
        0
    } else {
        sel.min(matches.len() - 1)
    };
    let ghost = matches
        .get(sel)
        .and_then(|(n, _)| n.strip_prefix(line.as_str()))
        .filter(|r| !r.is_empty())
        .unwrap_or("")
        .to_string();

    let mut s = String::new();
    // Clear current line + everything below (removes a previously drawn menu).
    s.push_str("\r\x1b[0J");
    s.push_str(prompt);
    s.push_str(&display);
    if !ghost.is_empty() {
        s.push_str(&format!("{MUTED}{ghost}{RESET}"));
    }

    if matches.is_empty() {
        // No menu: park the cursor at the logical position.
        let back = ghost.chars().count() + chars.len().saturating_sub(cursor);
        if back > 0 {
            s.push_str(&format!("\x1b[{back}D"));
        }
        return s;
    }

    // Draw the menu beneath the input line.
    let name_w = matches
        .iter()
        .map(|(n, _)| n.len())
        .max()
        .unwrap_or(8)
        .min(18);
    let start = if matches.len() <= MENU_MAX_ROWS || sel < MENU_MAX_ROWS {
        0
    } else {
        // Keep the highlighted row inside the visible completion window as the
        // user arrows down past the first page.
        sel + 1 - MENU_MAX_ROWS
    };
    let shown = (matches.len() - start).min(MENU_MAX_ROWS);
    let mut rows = 0;
    if start > 0 {
        s.push_str(&format!("\r\n  {MUTED}… {start} above{RESET}"));
        rows += 1;
    }
    for (offset, (name, desc)) in matches.iter().skip(start).take(shown).enumerate() {
        let i = start + offset;
        s.push_str("\r\n");
        rows += 1;
        // Leave room for: 2 gutter + 2 marker + name_w + 2 gap.
        let desc_room = term_cols.saturating_sub(6 + name_w);
        let desc = truncate(desc, desc_room);
        if i == sel {
            s.push_str(&format!(
                "  {ACCENT}▸ {BOLD}{name:<name_w$}{RESET}  {MUTED}{desc}{RESET}"
            ));
        } else {
            s.push_str(&format!("    {name:<name_w$}  {MUTED}{desc}{RESET}"));
        }
    }
    if start + shown < matches.len() {
        s.push_str(&format!(
            "\r\n  {MUTED}… +{} more{RESET}",
            matches.len() - start - shown
        ));
        rows += 1;
    }
    // Move cursor back up to the input line, then to the logical column.
    s.push_str(&format!("\x1b[{rows}A\r"));
    let col = prompt_cols + cursor;
    if col > 0 {
        s.push_str(&format!("\x1b[{col}C"));
    }
    s
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn prev_word(chars: &[char], mut cursor: usize) -> usize {
    while cursor > 0 && chars[cursor - 1].is_whitespace() {
        cursor -= 1;
    }
    while cursor > 0 && !chars[cursor - 1].is_whitespace() {
        cursor -= 1;
    }
    cursor
}

fn next_word(chars: &[char], mut cursor: usize) -> usize {
    while cursor < chars.len() && !chars[cursor].is_whitespace() {
        cursor += 1;
    }
    while cursor < chars.len() && chars[cursor].is_whitespace() {
        cursor += 1;
    }
    cursor
}

fn read_plain_outcome(
    prompt: &str,
    history: &[String],
    commands: &[(String, String)],
) -> Result<ReadLineOutcome> {
    let mut out = std::io::stdout();
    write!(out, "{prompt}")?;
    out.flush()?;
    let _cursor = crate::cursor::CursorGuard::text_input()?;
    crossterm::terminal::enable_raw_mode()?;
    let prompt_cols = crate::theme::visible_len(prompt);
    let term_cols = crossterm::terminal::size()
        .map(|(c, _)| c as usize)
        .unwrap_or(80);

    let result = (|| -> Result<ReadLineOutcome> {
        let mut chars: Vec<char> = Vec::new();
        let mut cursor = 0usize;
        let mut history_idx = history.len();
        // Completion-menu state: which row is selected, and whether the menu was
        // dismissed (Esc) until the next edit.
        let mut sel = 0usize;
        let mut dismissed = false;

        let redraw = |out: &mut std::io::Stdout,
                      chars: &[char],
                      cursor: usize,
                      sel: usize,
                      dismissed: bool|
         -> Result<()> {
            let s = render_input(
                prompt,
                prompt_cols,
                chars,
                cursor,
                commands,
                sel,
                dismissed,
                term_cols,
            );
            write!(out, "{s}")?;
            out.flush()?;
            Ok(())
        };
        // Number of completion matches for the current edit state (0 = no menu).
        let match_count = |chars: &[char], cursor: usize, dismissed: bool| -> usize {
            let line: String = chars.iter().collect();
            completion_matches(&line, cursor, chars.len(), commands, dismissed).len()
        };
        // Name of the currently selected completion, if the menu is open.
        let selected_name =
            |chars: &[char], cursor: usize, sel: usize, dismissed: bool| -> Option<String> {
                let line: String = chars.iter().collect();
                let m = completion_matches(&line, cursor, chars.len(), commands, dismissed);
                if m.is_empty() {
                    None
                } else {
                    Some(m[sel.min(m.len() - 1)].0.clone())
                }
            };

        loop {
            if let Event::Key(KeyEvent {
                code,
                modifiers,
                kind,
                ..
            }) = crossterm::event::read()?
            {
                if kind == KeyEventKind::Release {
                    continue;
                }
                if let Some(outcome) = control_key_outcome(code, modifiers) {
                    // See the Enter branch: `\r\n`, not `\n`, while raw mode is on.
                    redraw(&mut out, &chars, cursor, sel, true)?;
                    write!(out, "\r\n")?;
                    out.flush()?;
                    return Ok(outcome);
                }
                match code {
                    KeyCode::Enter => {
                        // Clear any open menu, then drop to the next line. Raw mode
                        // is still active here, so a bare `\n` only line-feeds and
                        // leaves the cursor in the input's last column — `\r` returns
                        // it to column 0 so the caller's output isn't shifted right.
                        redraw(&mut out, &chars, cursor, sel, true)?;
                        write!(out, "\r\n")?;
                        out.flush()?;
                        return Ok(ReadLineOutcome::Line(chars.iter().collect()));
                    }
                    KeyCode::Char('j') if modifiers.contains(KeyModifiers::CONTROL) => {
                        chars.insert(cursor, '\n');
                        cursor += 1;
                        sel = 0;
                        dismissed = false;
                        redraw(&mut out, &chars, cursor, sel, dismissed)?;
                    }
                    KeyCode::Esc => {
                        dismissed = true;
                        redraw(&mut out, &chars, cursor, sel, dismissed)?;
                    }
                    KeyCode::Backspace if cursor > 0 => {
                        chars.remove(cursor - 1);
                        cursor -= 1;
                        sel = 0;
                        dismissed = false;
                        redraw(&mut out, &chars, cursor, sel, dismissed)?;
                    }
                    KeyCode::Left if modifiers.contains(KeyModifiers::ALT) => {
                        cursor = prev_word(&chars, cursor);
                        redraw(&mut out, &chars, cursor, sel, dismissed)?;
                    }
                    KeyCode::Right if modifiers.contains(KeyModifiers::ALT) => {
                        cursor = next_word(&chars, cursor);
                        redraw(&mut out, &chars, cursor, sel, dismissed)?;
                    }
                    KeyCode::Left if cursor > 0 => {
                        cursor -= 1;
                        redraw(&mut out, &chars, cursor, sel, dismissed)?;
                    }
                    KeyCode::Right if cursor < chars.len() => {
                        cursor += 1;
                        redraw(&mut out, &chars, cursor, sel, dismissed)?;
                    }
                    // Tab accepts the selected completion (+ trailing space, ready
                    // for args). Right at end-of-line accepts it without the space.
                    KeyCode::Tab => {
                        if let Some(name) = selected_name(&chars, cursor, sel, dismissed) {
                            chars = name.chars().collect();
                            chars.push(' ');
                            cursor = chars.len();
                            sel = 0;
                            dismissed = false;
                            redraw(&mut out, &chars, cursor, sel, dismissed)?;
                        }
                    }
                    KeyCode::Right => {
                        if let Some(name) = selected_name(&chars, cursor, sel, dismissed) {
                            chars = name.chars().collect();
                            cursor = chars.len();
                            sel = 0;
                            dismissed = false;
                            redraw(&mut out, &chars, cursor, sel, dismissed)?;
                        }
                    }
                    // Up/Down navigate the menu when it's open, else the history.
                    KeyCode::Up if match_count(&chars, cursor, dismissed) > 0 => {
                        sel = sel.saturating_sub(1);
                        redraw(&mut out, &chars, cursor, sel, dismissed)?;
                    }
                    KeyCode::Down if match_count(&chars, cursor, dismissed) > 0 => {
                        let n = match_count(&chars, cursor, dismissed);
                        sel = (sel + 1).min(n - 1);
                        redraw(&mut out, &chars, cursor, sel, dismissed)?;
                    }
                    KeyCode::Up if !history.is_empty() => {
                        history_idx = history_idx.saturating_sub(1);
                        chars = history[history_idx].chars().collect();
                        cursor = chars.len();
                        sel = 0;
                        dismissed = false;
                        redraw(&mut out, &chars, cursor, sel, dismissed)?;
                    }
                    KeyCode::Down if !history.is_empty() => {
                        if history_idx + 1 < history.len() {
                            history_idx += 1;
                            chars = history[history_idx].chars().collect();
                        } else {
                            history_idx = history.len();
                            chars.clear();
                        }
                        cursor = chars.len();
                        sel = 0;
                        dismissed = false;
                        redraw(&mut out, &chars, cursor, sel, dismissed)?;
                    }
                    KeyCode::Char(c) => {
                        chars.insert(cursor, c);
                        cursor += 1;
                        sel = 0;
                        dismissed = false;
                        redraw(&mut out, &chars, cursor, sel, dismissed)?;
                    }
                    _ => {}
                }
            }
        }
    })();
    crossterm::terminal::disable_raw_mode()?;
    result
}

fn control_key_outcome(code: KeyCode, modifiers: KeyModifiers) -> Option<ReadLineOutcome> {
    if !modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    match code {
        KeyCode::Char('d') => Some(ReadLineOutcome::Eof),
        KeyCode::Char('c') => Some(ReadLineOutcome::Interrupted),
        _ => None,
    }
}

/// Interactive single-choice menu (↑/↓, Enter, number keys, q/Esc).
///
/// Returns `Some(index)` on confirm, `None` on cancel. Ctrl-C exits the process
/// the same way as [`plain_read_line`].
pub async fn select_from_list(
    title: String,
    options: Vec<String>,
    default_idx: usize,
) -> Result<Option<usize>> {
    if options.is_empty() {
        return Ok(None);
    }
    let outcome =
        tokio::task::spawn_blocking(move || read_select_outcome(&title, &options, default_idx))
            .await??;
    match outcome {
        SelectOutcome::Selected(i) => Ok(Some(i)),
        SelectOutcome::Cancelled => Ok(None),
        SelectOutcome::Interrupted => {
            crate::cursor::restore();
            std::process::exit(0)
        }
        SelectOutcome::Eof => Err(anyhow!("input closed")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SelectOutcome {
    Selected(usize),
    Cancelled,
    Interrupted,
    Eof,
}

/// Visible option rows in a select menu before it starts scrolling. Long
/// lists (e.g. OpenRouter /models) stay usable without flooding the terminal.
const SELECT_MAX_ROWS: usize = 12;

/// Pure frame for the select menu. Returns the bytes to write and the number of
/// lines drawn (so the interactive loop can move the cursor back up to redraw).
fn render_select_menu(title: &str, options: &[String], selected: usize) -> (String, usize) {
    let n = options.len();
    let selected = if n == 0 { 0 } else { selected.min(n - 1) };
    let mut s = String::new();
    let mut rows = 0usize;

    s.push_str(&format!("{PAD}{BOLD}{title}{RESET}\r\n"));
    rows += 1;

    let start = if n <= SELECT_MAX_ROWS || selected < SELECT_MAX_ROWS {
        0
    } else {
        // Keep the highlighted row inside the window as the user arrows past
        // the first page (same strategy as the slash-command completion menu).
        selected + 1 - SELECT_MAX_ROWS
    };
    let shown = (n - start).min(SELECT_MAX_ROWS);

    if start > 0 {
        s.push_str(&format!("{PAD}{MUTED}… {start} above{RESET}\r\n"));
        rows += 1;
    }

    for (offset, label) in options.iter().skip(start).take(shown).enumerate() {
        let i = start + offset;
        let num = i + 1;
        if i == selected {
            s.push_str(&format!(
                "{PAD}{ACCENT}{POINT} {BOLD}{num}) {label}{RESET}\r\n"
            ));
        } else {
            s.push_str(&format!("{PAD}  {MUTED}{num}){RESET} {label}\r\n"));
        }
        rows += 1;
    }

    if start + shown < n {
        s.push_str(&format!(
            "{PAD}{MUTED}… +{} more{RESET}\r\n",
            n - start - shown
        ));
        rows += 1;
    }

    s.push_str(&format!(
        "{PAD}{MUTED}↑/↓ move · Enter select · 1-9 jump · q cancel{RESET}"
    ));
    rows += 1;
    (s, rows)
}

fn read_select_outcome(
    title: &str,
    options: &[String],
    default_idx: usize,
) -> Result<SelectOutcome> {
    let n = options.len();
    if n == 0 {
        return Ok(SelectOutcome::Cancelled);
    }
    let mut selected = default_idx.min(n - 1);
    let mut out = std::io::stdout();

    crate::cursor::set_state(crate::cursor::CursorState::Passive)?;
    crossterm::terminal::enable_raw_mode()?;
    let result = (|| -> Result<SelectOutcome> {
        let mut first = true;
        // Track the previous frame height: overflow markers can add/remove a
        // line as the window scrolls, so we cannot assume a fixed row count.
        let mut prev_rows = 0usize;
        loop {
            let (frame, rows) = render_select_menu(title, options, selected);
            if !first {
                // Cursor sits on the last drawn line (hint has no trailing
                // newline), so go up `prev_rows - 1` to the title, then clear.
                let up = prev_rows.saturating_sub(1);
                if up > 0 {
                    write!(out, "\x1b[{up}A")?;
                }
                write!(out, "\r\x1b[0J")?;
            } else {
                // Defensively start the title at column 0 even though shared
                // raw-mode line input now exits with CRLF.
                write!(out, "\r")?;
            }
            first = false;
            write!(out, "{frame}")?;
            out.flush()?;
            prev_rows = rows;

            loop {
                let Event::Key(KeyEvent {
                    code,
                    modifiers,
                    kind,
                    ..
                }) = crossterm::event::read()?
                else {
                    continue;
                };
                if kind == KeyEventKind::Release {
                    continue;
                }
                if let Some(ctrl) = control_key_outcome(code, modifiers) {
                    // Park the cursor on the next line so the next println
                    // doesn't overwrite the menu.
                    write!(out, "\r\n")?;
                    out.flush()?;
                    return Ok(match ctrl {
                        ReadLineOutcome::Interrupted => SelectOutcome::Interrupted,
                        ReadLineOutcome::Eof => SelectOutcome::Eof,
                        ReadLineOutcome::Line(_) => unreachable!(),
                    });
                }
                match code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.saturating_sub(1);
                        break;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = (selected + 1).min(n - 1);
                        break;
                    }
                    KeyCode::Home => {
                        selected = 0;
                        break;
                    }
                    KeyCode::End => {
                        selected = n - 1;
                        break;
                    }
                    KeyCode::Enter => {
                        // Final paint so the confirmed row stays highlighted,
                        // then drop to the next line for subsequent output.
                        let (frame, _rows) = render_select_menu(title, options, selected);
                        let up = prev_rows.saturating_sub(1);
                        if up > 0 {
                            write!(out, "\x1b[{up}A")?;
                        }
                        write!(out, "\r\x1b[0J{frame}\r\n")?;
                        out.flush()?;
                        return Ok(SelectOutcome::Selected(selected));
                    }
                    KeyCode::Esc => {
                        write!(out, "\r\n")?;
                        out.flush()?;
                        return Ok(SelectOutcome::Cancelled);
                    }
                    KeyCode::Char('q') | KeyCode::Char('Q') => {
                        write!(out, "\r\n")?;
                        out.flush()?;
                        return Ok(SelectOutcome::Cancelled);
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        let digit = c.to_digit(10).unwrap_or(0) as usize;
                        // Single-digit jump only (same as the original wizard).
                        // Lists with 10+ items use arrows past item 9.
                        if (1..=n.min(9)).contains(&digit) {
                            selected = digit - 1;
                            // Number jump confirms immediately (power-user path
                            // matching the old "type a number + Enter" flow).
                            let (frame, _rows) = render_select_menu(title, options, selected);
                            let up = prev_rows.saturating_sub(1);
                            if up > 0 {
                                write!(out, "\x1b[{up}A")?;
                            }
                            write!(out, "\r\x1b[0J{frame}\r\n")?;
                            out.flush()?;
                            return Ok(SelectOutcome::Selected(selected));
                        }
                    }
                    _ => {}
                }
            }
        }
    })();
    crossterm::terminal::disable_raw_mode()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_persists_jsonl_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let mut history = InputHistory::load(path.display().to_string(), 2, true);
        history.push("one").unwrap();
        history.push("two").unwrap();
        history.push("three").unwrap();
        let history = InputHistory::load(path.display().to_string(), 2, true);
        assert_eq!(history.entries(), &["two".to_string(), "three".to_string()]);
    }

    fn cmds() -> Vec<(String, String)> {
        vec![
            ("/compact".into(), "compact".into()),
            ("/compare".into(), "compare".into()),
            ("/config".into(), "config".into()),
            ("/help".into(), "help".into()),
        ]
    }

    #[test]
    fn matches_only_for_slash_prefix_at_end() {
        let c = cmds();
        assert_eq!(completion_matches("/co", 3, 3, &c, false).len(), 3);
        // not a slash command
        assert!(completion_matches("co", 2, 2, &c, false).is_empty());
        // cursor not at end → no menu (don't fight mid-line editing)
        assert!(completion_matches("/co", 1, 3, &c, false).is_empty());
        // dismissed (Esc)
        assert!(completion_matches("/co", 3, 3, &c, true).is_empty());
        // exact unique match → already complete, no menu
        assert!(completion_matches("/help", 5, 5, &c, false).is_empty());
        // no matches
        assert!(completion_matches("/zzz", 4, 4, &c, false).is_empty());
    }

    #[test]
    fn render_shows_selected_ghost_and_menu_rows() {
        let chars: Vec<char> = "/co".chars().collect();
        let out = render_input("> ", 2, &chars, chars.len(), &cmds(), 1, false, 80);
        // Selected row is index 1 (/compare) → ghost is its remainder "mpare".
        assert!(out.contains("mpare"), "ghost of selected match: {out:?}");
        // All three matches appear as menu rows.
        for name in ["/compact", "/compare", "/config"] {
            assert!(out.contains(name), "menu row {name} missing: {out:?}");
        }
        // The selected row is marked with the accent pointer.
        assert!(out.contains("▸"), "selected marker missing: {out:?}");
        // It clears below and restores the cursor up onto the input line.
        assert!(out.starts_with("\r\x1b[0J"));
        assert!(
            out.contains("\x1b[3A"),
            "cursor moves back up 3 rows: {out:?}"
        );
    }

    #[test]
    fn render_no_menu_when_no_matches() {
        let chars: Vec<char> = "hello".chars().collect();
        let out = render_input("> ", 2, &chars, chars.len(), &cmds(), 0, false, 80);
        assert!(!out.contains('▸'));
        assert!(!out.contains("\r\n"));
    }

    #[test]
    fn render_completion_window_follows_selected_row() {
        let commands: Vec<(String, String)> = (0..12)
            .map(|i| (format!("/cmd{i:02}"), format!("command {i}")))
            .collect();
        let chars: Vec<char> = "/".chars().collect();
        let out = render_input("> ", 2, &chars, chars.len(), &commands, 10, false, 80);

        assert!(
            out.contains("/cmd10"),
            "selected row should be visible: {out:?}"
        );
        assert!(out.contains("▸"), "selected marker missing: {out:?}");
        assert!(
            out.contains("… 3 above"),
            "top overflow marker missing: {out:?}"
        );
        assert!(
            out.contains("/cmd03"),
            "window should start near selected row: {out:?}"
        );
        assert!(
            !out.contains("/cmd00"),
            "first page should scroll out once selection moves below it: {out:?}"
        );
    }

    #[test]
    fn visible_width_ignores_ansi() {
        assert_eq!(crate::theme::visible_len("  \x1b[96m❯\x1b[0m "), 4);
        assert_eq!(crate::theme::visible_len("abc"), 3);
    }

    #[test]
    fn word_movement_skips_whitespace() {
        let chars: Vec<char> = "one two".chars().collect();
        assert_eq!(prev_word(&chars, chars.len()), 4);
        assert_eq!(next_word(&chars, 0), 4);
    }

    #[test]
    fn control_keys_map_to_interactive_read_outcomes() {
        assert_eq!(
            control_key_outcome(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Some(ReadLineOutcome::Eof)
        );
        assert_eq!(
            control_key_outcome(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(ReadLineOutcome::Interrupted)
        );
        assert_eq!(
            control_key_outcome(KeyCode::Char('d'), KeyModifiers::NONE),
            None
        );
    }

    #[test]
    fn select_menu_highlights_selected_row_and_counts_lines() {
        let options = vec!["ollama".into(), "openai *".into(), "openrouter".into()];
        let (frame, rows) = render_select_menu("Backend", &options, 1);
        assert_eq!(rows, 5, "title + 3 options + hint");
        assert!(frame.contains("Backend"), "title missing: {frame:?}");
        assert!(frame.contains("▸"), "selected marker missing: {frame:?}");
        // Title and rows share the left gutter (PAD). The interactive loop also
        // CR's to column 0 before the first paint so this indent is stable.
        assert!(
            frame.starts_with(PAD),
            "title must start at the left gutter: {frame:?}"
        );
        let first_option = frame.lines().nth(1).expect("first option");
        assert!(
            first_option.starts_with(PAD),
            "options must share the title gutter: {first_option:?}"
        );
        // Selected row keeps number+label contiguous (bold). Unselected rows
        // insert a RESET between the muted number and the label.
        assert!(
            frame.contains("2) openai *"),
            "selected row missing: {frame:?}"
        );
        assert!(frame.contains("ollama"), "row 1 label missing: {frame:?}");
        assert!(
            frame.contains("openrouter"),
            "row 3 label missing: {frame:?}"
        );
        assert!(frame.contains("1)"), "row 1 number missing: {frame:?}");
        assert!(frame.contains("3)"), "row 3 number missing: {frame:?}");
        assert!(frame.contains("↑/↓ move"), "hint missing: {frame:?}");
        let sel_pos = frame.find("2) openai *").expect("selected label");
        let pointer_pos = frame.find('▸').expect("pointer");
        assert!(
            pointer_pos < sel_pos,
            "pointer should precede selected label"
        );
    }

    #[test]
    fn select_menu_clamps_selected_index() {
        let options = vec!["a".into(), "b".into()];
        let (frame, rows) = render_select_menu("Pick", &options, 99);
        assert_eq!(rows, 4, "title + 2 options + hint");
        // Out-of-range selection clamps to last item (index 1 → "2) b").
        let pointer = frame.find('▸').expect("pointer");
        let b_pos = frame.find("2) b").expect("b row");
        let a_pos = frame.find('a').expect("a label");
        assert!(
            a_pos < pointer && pointer < b_pos,
            "pointer should sit on the last row when clamped: {frame:?}"
        );
    }

    #[test]
    fn select_menu_scrolls_long_lists() {
        let options: Vec<String> = (1..=20).map(|i| format!("model-{i:02}")).collect();
        let (frame, rows) = render_select_menu("Model", &options, 15);
        // title + "… above" + 12 options + "… more" + hint
        assert_eq!(rows, 16, "windowed frame height: {frame:?}");
        assert!(
            frame.contains("… 4 above"),
            "top overflow missing: {frame:?}"
        );
        assert!(
            frame.contains("… +4 more"),
            "bottom overflow missing: {frame:?}"
        );
        assert!(
            frame.contains("16) model-16"),
            "selected row should be visible: {frame:?}"
        );
        assert!(
            !frame.contains("model-01"),
            "first page should scroll out: {frame:?}"
        );
        assert!(frame.contains("▸"), "selected marker missing: {frame:?}");
    }
}
