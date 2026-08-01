use anyhow::{anyhow, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::theme::{
    fade_header, ACCENT, BOLD, MUTED, PAD, POINT, PROMPT_CHAR, RESET, TEXT, WARN, WARN_MARK,
};

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

pub async fn read_composer_with_history_outcome(
    history: Vec<String>,
    commands: Vec<(String, String)>,
    footer: ComposerFooter,
    details: Option<ComposerDetails>,
) -> Result<ReadLineOutcome> {
    tokio::task::spawn_blocking(move || read_composer_outcome(&history, &commands, footer, details))
        .await?
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerFooter {
    KeyboardHint,
    Session(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerDetails {
    pub body: String,
    pub expanded_by_default: bool,
}

fn render_value(value: &str) -> String {
    value.replace('\n', "⏎")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ComposerFrame {
    text: String,
    rows: usize,
    cursor_row: usize,
    cursor_col: usize,
}

#[derive(Clone, Copy)]
struct ComposerPresentation<'a> {
    footer: &'a ComposerFooter,
    details: Option<&'a str>,
    details_expanded: bool,
}

#[derive(Debug, Default)]
struct ComposerRegion {
    cursor_row: Option<usize>,
}

impl ComposerRegion {
    fn replace(&mut self, frame: ComposerFrame) -> String {
        let mut output = self.clear();
        if output.is_empty() {
            output.push('\r');
        }
        output.push_str(&frame.text);

        let up = frame
            .rows
            .saturating_sub(1)
            .saturating_sub(frame.cursor_row);
        if up > 0 {
            output.push_str(&format!("\x1b[{up}A"));
        }
        output.push('\r');
        if frame.cursor_col > 0 {
            output.push_str(&format!("\x1b[{}C", frame.cursor_col));
        }
        self.cursor_row = Some(frame.cursor_row);
        output
    }

    fn clear(&mut self) -> String {
        let Some(cursor_row) = self.cursor_row.take() else {
            return String::new();
        };
        let mut output = String::new();
        if cursor_row > 0 {
            output.push_str(&format!("\x1b[{cursor_row}A"));
        }
        output.push_str("\r\x1b[0J");
        output
    }

    fn finish(&mut self, submitted: Option<&str>) -> String {
        let mut output = self.clear();
        let Some(submitted) = submitted.filter(|value| !value.trim().is_empty()) else {
            return output;
        };

        // Once submitted, a message is transcript content rather than an
        // editable prompt. Give it the same role header as an assistant
        // response; the arrow remains exclusive to the active composer.
        output.push_str(&format!("\r\n{}\r\n", fade_header("user")));
        for line in submitted.lines() {
            output.push_str(&format!("\r{PAD}{TEXT}{line}{RESET}\r\n"));
        }
        output
    }
}

fn render_composer(
    chars: &[char],
    cursor: usize,
    commands: &[(String, String)],
    sel: usize,
    dismissed: bool,
    presentation: ComposerPresentation<'_>,
    term_cols: usize,
) -> ComposerFrame {
    let ComposerPresentation {
        footer,
        details,
        details_expanded,
    } = presentation;
    let width = term_cols.max(20);
    let rule = if crate::theme::ascii_enabled() {
        "-"
    } else {
        "─"
    };
    let (top_left, side, bottom_left) = if crate::theme::ascii_enabled() {
        ("+-", "|", "+-")
    } else {
        ("╭─", "│", "╰─")
    };
    let label = " message ";
    let top_prefix = format!("{PAD}{top_left}{label}");
    let top_fill = rule.repeat(width.saturating_sub(top_prefix.chars().count()));
    let cursor = cursor.min(chars.len());
    let editor_col = PAD.chars().count() + side.chars().count() + 1 + 2;
    let editor_width = width.saturating_sub(editor_col + 1).max(1);
    let (editor_lines, cursor_line, cursor_in_line) =
        layout_composer_text(chars, cursor, editor_width);

    let value: String = chars.iter().collect();
    let matches = completion_matches(&value, cursor, chars.len(), commands, dismissed);
    let selected = if matches.is_empty() {
        0
    } else {
        sel.min(matches.len() - 1)
    };
    let ghost = matches
        .get(selected)
        .and_then(|(name, _)| name.strip_prefix(value.as_str()))
        .unwrap_or("");

    let expanded_details = details.filter(|_| details_expanded);
    let detail_rows = expanded_details
        .map(|value| value.lines().count() + 1)
        .unwrap_or(0);
    let mut text = String::new();
    if let Some(details) = expanded_details {
        for (index, line) in details.lines().enumerate() {
            if index > 0 {
                text.push_str("\r\n");
            }
            text.push_str(&crate::theme::truncate_visible(line, width));
        }
        text.push_str("\r\n\r\n");
    }
    text.push_str(&format!("{MUTED}{top_prefix}{top_fill}{RESET}"));
    for (index, editor_line) in editor_lines.iter().enumerate() {
        let indicator = if index == 0 {
            format!("{ACCENT}{PROMPT_CHAR}{RESET} ")
        } else {
            "  ".to_string()
        };
        text.push_str(&format!(
            "\r\n{MUTED}{PAD}{side}{RESET} {indicator}{editor_line}"
        ));
        if index == editor_lines.len() - 1 && !ghost.is_empty() {
            let room = editor_width.saturating_sub(editor_line.chars().count());
            let visible_ghost = truncate(ghost, room);
            text.push_str(&format!("{MUTED}{visible_ghost}{RESET}"));
        }
    }

    let mut menu_rows = 0usize;
    if !matches.is_empty() {
        let name_width = matches
            .iter()
            .map(|(name, _)| name.chars().count())
            .max()
            .unwrap_or(8)
            .min(18);
        let start = if matches.len() <= MENU_MAX_ROWS || selected < MENU_MAX_ROWS {
            0
        } else {
            selected + 1 - MENU_MAX_ROWS
        };
        let shown = (matches.len() - start).min(MENU_MAX_ROWS);
        if start > 0 {
            text.push_str(&format!("\r\n{MUTED}{PAD}{side}   … {start} above{RESET}"));
            menu_rows += 1;
        }
        for (offset, (name, description)) in matches.iter().skip(start).take(shown).enumerate() {
            let index = start + offset;
            let description_width = width.saturating_sub(10 + name_width).max(1);
            let description = truncate(description, description_width);
            if index == selected {
                text.push_str(&format!(
                    "\r\n{MUTED}{PAD}{side}{RESET}  {ACCENT}{POINT} {BOLD}{name:<name_width$}{RESET}  {MUTED}{description}{RESET}"
                ));
            } else {
                text.push_str(&format!(
                    "\r\n{MUTED}{PAD}{side}    {name:<name_width$}  {description}{RESET}"
                ));
            }
            menu_rows += 1;
        }
        if start + shown < matches.len() {
            text.push_str(&format!(
                "\r\n{MUTED}{PAD}{side}   … +{} more{RESET}",
                matches.len() - start - shown
            ));
            menu_rows += 1;
        }
    }

    let base_footer = match footer {
        ComposerFooter::KeyboardHint if width < 46 => "Enter send · ^J newline",
        ComposerFooter::KeyboardHint => "Enter send · Ctrl+J newline",
        ComposerFooter::Session(context) => context,
    };
    let footer_text = if details.is_some() {
        let shortcut = if width < 46 {
            if details_expanded {
                "^O hide"
            } else {
                "^O details"
            }
        } else if details_expanded {
            "Ctrl+O hide"
        } else {
            "Ctrl+O details"
        };
        format!("{shortcut} · {base_footer}")
    } else {
        base_footer.to_string()
    };
    let footer = truncate(&format!("{PAD}{bottom_left} {footer_text}"), width);
    text.push_str(&format!("\r\n{MUTED}{footer}{RESET}"));

    ComposerFrame {
        text,
        rows: detail_rows + 1 + editor_lines.len() + menu_rows + 1,
        cursor_row: detail_rows + 1 + cursor_line,
        cursor_col: editor_col + cursor_in_line,
    }
}

fn layout_composer_text(
    chars: &[char],
    cursor: usize,
    width: usize,
) -> (Vec<String>, usize, usize) {
    let width = width.max(1);
    let mut lines = vec![String::new()];
    let mut positions = vec![(0usize, 0usize)];
    let mut row = 0usize;
    let mut col = 0usize;

    for character in chars {
        if *character == '\n' {
            lines.push(String::new());
            row += 1;
            col = 0;
        } else {
            if col == width {
                lines.push(String::new());
                row += 1;
                col = 0;
            }
            lines[row].push(*character);
            col += 1;
        }
        positions.push((row, col));
    }

    let (cursor_row, cursor_col) = positions[cursor.min(chars.len())];
    (lines, cursor_row, cursor_col)
}

enum InputSurface {
    Plain {
        prompt: String,
        prompt_cols: usize,
    },
    Composer {
        region: ComposerRegion,
        footer: ComposerFooter,
        details: Option<String>,
        details_expanded: bool,
    },
}

#[derive(Clone, Copy)]
struct EditorView<'a> {
    chars: &'a [char],
    cursor: usize,
    selected: usize,
    dismissed: bool,
    term_cols: usize,
}

impl InputSurface {
    fn redraw(&mut self, view: EditorView<'_>, commands: &[(String, String)]) -> String {
        match self {
            Self::Plain {
                prompt,
                prompt_cols,
            } => render_input(
                prompt,
                *prompt_cols,
                view.chars,
                view.cursor,
                commands,
                view.selected,
                view.dismissed,
                view.term_cols,
            ),
            Self::Composer {
                region,
                footer,
                details,
                details_expanded,
            } => region.replace(render_composer(
                view.chars,
                view.cursor,
                commands,
                view.selected,
                view.dismissed,
                ComposerPresentation {
                    footer,
                    details: details.as_deref(),
                    details_expanded: *details_expanded,
                },
                view.term_cols,
            )),
        }
    }

    fn finish(
        &mut self,
        view: EditorView<'_>,
        commands: &[(String, String)],
        submitted: bool,
    ) -> String {
        match self {
            Self::Plain {
                prompt,
                prompt_cols,
            } => {
                let mut output = render_input(
                    prompt,
                    *prompt_cols,
                    view.chars,
                    view.cursor,
                    commands,
                    view.selected,
                    true,
                    view.term_cols,
                );
                output.push_str("\r\n");
                output
            }
            Self::Composer { region, .. } => {
                let value = submitted.then(|| view.chars.iter().collect::<String>());
                region.finish(value.as_deref())
            }
        }
    }

    fn toggle_details(&mut self) -> bool {
        let Self::Composer {
            details,
            details_expanded,
            ..
        } = self
        else {
            return false;
        };
        if details.is_none() {
            return false;
        }
        *details_expanded = !*details_expanded;
        true
    }
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

/// Text to submit when the user presses Enter. An open completion menu is an
/// explicit selection surface: submit its highlighted command rather than the
/// incomplete prefix still present in the editor.
fn submitted_text(
    chars: &[char],
    cursor: usize,
    selected: usize,
    commands: &[(String, String)],
    dismissed: bool,
) -> String {
    let typed: String = chars.iter().collect();
    let matches = completion_matches(&typed, cursor, chars.len(), commands, dismissed);
    matches
        .get(selected.min(matches.len().saturating_sub(1)))
        .map(|(name, _)| (*name).clone())
        .unwrap_or(typed)
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
    read_line_outcome(
        InputSurface::Plain {
            prompt: prompt.to_string(),
            prompt_cols: crate::theme::visible_len(prompt),
        },
        history,
        commands,
    )
}

fn read_composer_outcome(
    history: &[String],
    commands: &[(String, String)],
    footer: ComposerFooter,
    details: Option<ComposerDetails>,
) -> Result<ReadLineOutcome> {
    let details_expanded = details
        .as_ref()
        .map(|details| details.expanded_by_default)
        .unwrap_or(false);
    read_line_outcome(
        InputSurface::Composer {
            region: ComposerRegion::default(),
            footer,
            details: details.map(|details| details.body),
            details_expanded,
        },
        history,
        commands,
    )
}

fn write_surface(
    out: &mut impl Write,
    surface: &mut InputSurface,
    commands: &[(String, String)],
    view: EditorView<'_>,
) -> Result<()> {
    let frame = surface.redraw(view, commands);
    write!(out, "{frame}")?;
    out.flush()?;
    Ok(())
}

fn read_line_outcome(
    mut surface: InputSurface,
    history: &[String],
    commands: &[(String, String)],
) -> Result<ReadLineOutcome> {
    let mut out = std::io::stdout();
    let _cursor = crate::cursor::CursorGuard::text_input()?;
    crossterm::terminal::enable_raw_mode()?;
    let mut term_cols = crossterm::terminal::size()
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
        macro_rules! redraw {
            () => {
                write_surface(
                    &mut out,
                    &mut surface,
                    commands,
                    EditorView {
                        chars: &chars,
                        cursor,
                        selected: sel,
                        dismissed,
                        term_cols,
                    },
                )?
            };
        }
        redraw!();
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
            let event = crossterm::event::read()?;
            if let Event::Resize(cols, _) = event {
                term_cols = cols as usize;
                redraw!();
                continue;
            }
            if let Event::Key(KeyEvent {
                code,
                modifiers,
                kind,
                ..
            }) = event
            {
                if kind == KeyEventKind::Release {
                    continue;
                }
                if let Some(outcome) = control_key_outcome(code, modifiers) {
                    let final_frame = surface.finish(
                        EditorView {
                            chars: &chars,
                            cursor,
                            selected: sel,
                            dismissed,
                            term_cols,
                        },
                        commands,
                        false,
                    );
                    write!(out, "{final_frame}")?;
                    out.flush()?;
                    return Ok(outcome);
                }
                match code {
                    KeyCode::Char('o') if modifiers.contains(KeyModifiers::CONTROL) => {
                        if surface.toggle_details() {
                            redraw!();
                        }
                    }
                    KeyCode::Enter => {
                        let submitted = submitted_text(&chars, cursor, sel, commands, dismissed);
                        let submitted_chars: Vec<char> = submitted.chars().collect();
                        let final_frame = surface.finish(
                            EditorView {
                                chars: &submitted_chars,
                                cursor: submitted_chars.len(),
                                selected: sel,
                                dismissed,
                                term_cols,
                            },
                            commands,
                            true,
                        );
                        write!(out, "{final_frame}")?;
                        out.flush()?;
                        return Ok(ReadLineOutcome::Line(submitted));
                    }
                    KeyCode::Char('j') if modifiers.contains(KeyModifiers::CONTROL) => {
                        chars.insert(cursor, '\n');
                        cursor += 1;
                        sel = 0;
                        dismissed = false;
                        redraw!();
                    }
                    KeyCode::Esc => {
                        dismissed = true;
                        redraw!();
                    }
                    KeyCode::Backspace if cursor > 0 => {
                        chars.remove(cursor - 1);
                        cursor -= 1;
                        sel = 0;
                        dismissed = false;
                        redraw!();
                    }
                    KeyCode::Left if modifiers.contains(KeyModifiers::ALT) => {
                        cursor = prev_word(&chars, cursor);
                        redraw!();
                    }
                    KeyCode::Right if modifiers.contains(KeyModifiers::ALT) => {
                        cursor = next_word(&chars, cursor);
                        redraw!();
                    }
                    KeyCode::Left if cursor > 0 => {
                        cursor -= 1;
                        redraw!();
                    }
                    KeyCode::Right if cursor < chars.len() => {
                        cursor += 1;
                        redraw!();
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
                            redraw!();
                        }
                    }
                    KeyCode::Right => {
                        if let Some(name) = selected_name(&chars, cursor, sel, dismissed) {
                            chars = name.chars().collect();
                            cursor = chars.len();
                            sel = 0;
                            dismissed = false;
                            redraw!();
                        }
                    }
                    // Up/Down navigate the menu when it's open, else the history.
                    KeyCode::Up if match_count(&chars, cursor, dismissed) > 0 => {
                        sel = sel.saturating_sub(1);
                        redraw!();
                    }
                    KeyCode::Down if match_count(&chars, cursor, dismissed) > 0 => {
                        let n = match_count(&chars, cursor, dismissed);
                        sel = (sel + 1).min(n - 1);
                        redraw!();
                    }
                    KeyCode::Up if !history.is_empty() => {
                        history_idx = history_idx.saturating_sub(1);
                        chars = history[history_idx].chars().collect();
                        cursor = chars.len();
                        sel = 0;
                        dismissed = false;
                        redraw!();
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
                        redraw!();
                    }
                    KeyCode::Char(c) => {
                        chars.insert(cursor, c);
                        cursor += 1;
                        sel = 0;
                        dismissed = false;
                        redraw!();
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOption {
    pub label: String,
    pub shortcut: Option<char>,
}

impl SelectOption {
    pub fn new(label: impl Into<String>, shortcut: Option<char>) -> Self {
        Self {
            label: label.into(),
            shortcut: shortcut.map(|key| key.to_ascii_lowercase()),
        }
    }
}

fn shortcut_selection(options: &[SelectOption], pressed: char) -> Option<usize> {
    let pressed = pressed.to_ascii_lowercase();
    options
        .iter()
        .position(|option| option.shortcut == Some(pressed))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectPrompt {
    pub title: String,
    pub body: Vec<String>,
    pub options: Vec<SelectOption>,
    pub default_idx: usize,
}

/// Rich single-choice prompt for consequential actions. Unlike
/// [`select_from_list`], the frame is removed after the decision so callers can
/// replace it with a compact transcript receipt.
pub async fn select_from_prompt(prompt: SelectPrompt) -> Result<Option<usize>> {
    if prompt.options.is_empty() {
        return Ok(None);
    }
    let outcome =
        tokio::task::spawn_blocking(move || read_select_prompt_outcome(&prompt)).await??;
    match outcome {
        SelectOutcome::Selected(index) => Ok(Some(index)),
        SelectOutcome::Cancelled => Ok(None),
        SelectOutcome::Interrupted => {
            crate::cursor::restore();
            std::process::exit(0)
        }
        SelectOutcome::Eof => Err(anyhow!("input closed")),
    }
}

fn render_select_prompt(prompt: &SelectPrompt, selected: usize) -> (String, usize) {
    render_select_prompt_at_width(prompt, selected, crate::theme::cols())
}

fn wrap_plain_line(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let characters = line.chars().collect::<Vec<_>>();
    if characters.is_empty() {
        return vec![String::new()];
    }
    characters
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

fn render_select_prompt_at_width(
    prompt: &SelectPrompt,
    selected: usize,
    width: usize,
) -> (String, usize) {
    let width = width.max(20);
    let selected = selected.min(prompt.options.len().saturating_sub(1));
    let mut frame = String::new();
    let mut rows = 0usize;

    frame.push_str(&format!(
        "{PAD}{WARN}{WARN_MARK}{RESET} {BOLD}{}{RESET}\r\n",
        prompt.title
    ));
    rows += 1;

    let body_indent = format!("{PAD}  ");
    let body_width = width.saturating_sub(body_indent.chars().count()).max(1);
    for (index, line) in prompt.body.iter().enumerate() {
        for wrapped in wrap_plain_line(line, body_width) {
            if index == 0 {
                frame.push_str(&format!("{body_indent}{BOLD}{wrapped}{RESET}\r\n"));
            } else if line.starts_with("Why approval is needed:") {
                frame.push_str(&format!("{body_indent}{WARN}{wrapped}{RESET}\r\n"));
            } else {
                frame.push_str(&format!("{body_indent}{wrapped}{RESET}\r\n"));
            }
            rows += 1;
        }
    }
    if !prompt.body.is_empty() {
        frame.push_str("\r\n");
        rows += 1;
    }

    for (index, option) in prompt.options.iter().enumerate() {
        let number = index + 1;
        let shortcut = option
            .shortcut
            .map(|key| format!("[{key}] "))
            .unwrap_or_default();
        let prefix_width = PAD.chars().count()
            + 2
            + number.to_string().chars().count()
            + 2
            + shortcut.chars().count();
        let label_width = width.saturating_sub(prefix_width).max(1);
        for (line_index, wrapped) in wrap_plain_line(&option.label, label_width)
            .into_iter()
            .enumerate()
        {
            if line_index == 0 && index == selected {
                frame.push_str(&format!(
                    "{PAD}{ACCENT}{POINT} {BOLD}{number}) {shortcut}{wrapped}{RESET}\r\n"
                ));
            } else if line_index == 0 {
                frame.push_str(&format!(
                    "{PAD}  {MUTED}{number}){RESET} {shortcut}{wrapped}{RESET}\r\n"
                ));
            } else {
                frame.push_str(&format!("{}{wrapped}{RESET}\r\n", " ".repeat(prefix_width)));
            }
            rows += 1;
        }
    }

    let hint = if width < 58 {
        "↑/↓ move · Enter confirm · Esc deny"
    } else {
        "↑/↓ move · Enter confirm · 1-9 jump · Esc deny"
    };
    frame.push_str(&format!("{PAD}{MUTED}{hint}{RESET}"));
    rows += 1;
    (frame, rows)
}

fn read_select_prompt_outcome(prompt: &SelectPrompt) -> Result<SelectOutcome> {
    let count = prompt.options.len();
    if count == 0 {
        return Ok(SelectOutcome::Cancelled);
    }
    let mut selected = prompt.default_idx.min(count - 1);
    let mut out = std::io::stdout();

    crate::cursor::set_state(crate::cursor::CursorState::Passive)?;
    crossterm::terminal::enable_raw_mode()?;
    let result = (|| -> Result<SelectOutcome> {
        let mut first = true;
        let mut previous_rows = 0usize;
        loop {
            let (frame, rows) = render_select_prompt(prompt, selected);
            if !first {
                let up = previous_rows.saturating_sub(1);
                if up > 0 {
                    write!(out, "\x1b[{up}A")?;
                }
                write!(out, "\r\x1b[0J")?;
            } else {
                write!(out, "\r")?;
            }
            first = false;
            write!(out, "{frame}")?;
            out.flush()?;
            previous_rows = rows;

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
                if let Some(control) = control_key_outcome(code, modifiers) {
                    clear_select_frame(&mut out, previous_rows)?;
                    return Ok(match control {
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
                        selected = (selected + 1).min(count - 1);
                        break;
                    }
                    KeyCode::Home => {
                        selected = 0;
                        break;
                    }
                    KeyCode::End => {
                        selected = count - 1;
                        break;
                    }
                    KeyCode::Enter => {
                        clear_select_frame(&mut out, previous_rows)?;
                        return Ok(SelectOutcome::Selected(selected));
                    }
                    KeyCode::Esc => {
                        clear_select_frame(&mut out, previous_rows)?;
                        return Ok(SelectOutcome::Cancelled);
                    }
                    KeyCode::Char(character) => {
                        if let Some(index) = shortcut_selection(&prompt.options, character) {
                            clear_select_frame(&mut out, previous_rows)?;
                            return Ok(SelectOutcome::Selected(index));
                        }
                        if let Some(digit) = character.to_digit(10).map(|value| value as usize) {
                            if (1..=count.min(9)).contains(&digit) {
                                clear_select_frame(&mut out, previous_rows)?;
                                return Ok(SelectOutcome::Selected(digit - 1));
                            }
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

fn clear_select_frame(out: &mut impl Write, rows: usize) -> Result<()> {
    let up = rows.saturating_sub(1);
    if up > 0 {
        write!(out, "\x1b[{up}A")?;
    }
    write!(out, "\r\x1b[0J")?;
    out.flush()?;
    Ok(())
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
    fn enter_submits_the_highlighted_command_completion() {
        let commands = vec![
            ("/clear".into(), "clear the screen".into()),
            ("/close".into(), "close the session".into()),
        ];
        let typed: Vec<char> = "/cl".chars().collect();

        assert_eq!(
            submitted_text(&typed, typed.len(), 0, &commands, false),
            "/clear"
        );
        assert_eq!(
            submitted_text(&typed, typed.len(), 1, &commands, false),
            "/close"
        );
        assert_eq!(
            submitted_text(&typed, typed.len(), 0, &commands, true),
            "/cl"
        );
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
    fn rich_select_prompt_renders_context_and_explicit_shortcuts() {
        let prompt = SelectPrompt {
            title: "Permission required".into(),
            body: vec![
                "Read directory outside workspace".into(),
                "/tmp/Albatross".into(),
            ],
            options: vec![
                SelectOption::new("Allow once", Some('y')),
                SelectOption::new("Allow this directory for the session", Some('s')),
                SelectOption::new("Deny", Some('n')),
            ],
            default_idx: 0,
        };

        let (frame, rows) = render_select_prompt(&prompt, 1);

        assert_eq!(rows, 8, "title + body + gap + 3 options + hint");
        assert!(frame.contains("Permission required"));
        assert!(frame.contains("Read directory outside workspace"));
        assert!(frame.contains("/tmp/Albatross"));
        assert!(frame.contains("▸"));
        assert!(frame.contains("2) [s] Allow this directory for the session"));
        assert!(frame.contains("[y] Allow once"));
        assert!(frame.contains("[s] Allow this directory for the session"));
        assert!(frame.contains("Esc deny"));
    }

    #[test]
    fn rich_select_prompt_maps_direct_shortcuts_to_options() {
        let options = vec![
            SelectOption::new("Allow once", Some('y')),
            SelectOption::new("Allow for session", Some('s')),
            SelectOption::new("Deny", Some('n')),
        ];

        assert_eq!(shortcut_selection(&options, 'Y'), Some(0));
        assert_eq!(shortcut_selection(&options, 's'), Some(1));
        assert_eq!(shortcut_selection(&options, 'n'), Some(2));
        assert_eq!(shortcut_selection(&options, 'x'), None);
    }

    #[test]
    fn rich_select_prompt_wraps_long_content_without_breaking_repaint_rows() {
        let prompt = SelectPrompt {
            title: "Permission required".into(),
            body: vec!["/Users/example/a-very-long-workspace-name/src/application.rs".into()],
            options: vec![SelectOption::new(
                "Allow every file_read call this session — broader access",
                Some('a'),
            )],
            default_idx: 0,
        };

        let (frame, rows) = render_select_prompt_at_width(&prompt, 0, 42);

        assert_eq!(rows, frame.lines().count());
        assert!(
            frame
                .lines()
                .all(|line| crate::theme::visible_len(line) <= 42),
            "frame exceeded terminal width: {frame:?}"
        );
        assert!(rows > 5, "body and option should both wrap: {frame:?}");
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

    fn render_hint_composer(
        chars: &[char],
        cursor: usize,
        commands: &[(String, String)],
        selected: usize,
        dismissed: bool,
        width: usize,
    ) -> ComposerFrame {
        render_composer(
            chars,
            cursor,
            commands,
            selected,
            dismissed,
            ComposerPresentation {
                footer: &ComposerFooter::KeyboardHint,
                details: None,
                details_expanded: false,
            },
            width,
        )
    }

    #[test]
    fn composer_gives_editing_a_dedicated_three_row_surface() {
        let chars: Vec<char> = "hello".chars().collect();

        let frame = render_hint_composer(&chars, chars.len(), &[], 0, false, 80);

        assert_eq!(frame.rows, 3, "header + editor + keyboard hint");
        assert!(frame.text.contains("╭─ message"));
        assert!(frame.text.contains('│'));
        assert!(frame.text.contains('❯'));
        assert!(frame.text.contains("hello"));
        assert!(frame.text.contains("╰─ Enter send · Ctrl+J newline"));
        assert_eq!(frame.cursor_row, 1);
        assert_eq!(frame.cursor_col, 11);
    }

    #[test]
    fn composer_replaces_the_keyboard_hint_with_live_session_context() {
        let chars = Vec::new();
        let footer = ComposerFooter::Session("grok-4.5 · edit · ask · main*".into());

        let frame = render_composer(
            &chars,
            0,
            &[],
            0,
            false,
            ComposerPresentation {
                footer: &footer,
                details: None,
                details_expanded: false,
            },
            80,
        );

        assert!(frame.text.contains("╰─ grok-4.5 · edit · ask · main*"));
        assert!(!frame.text.contains("Enter send"));
        assert_eq!(frame.rows, 3);
    }

    #[test]
    fn composer_toggles_owned_tool_details_without_losing_the_draft() {
        let chars: Vec<char> = "draft stays".chars().collect();
        let footer = ComposerFooter::Session("grok-4.5 · edit · ask · main*".into());
        let details =
            "  ● last turn details\n    Ran  command=demo-output · exit 0\n      alpha\n      beta";

        let collapsed = render_composer(
            &chars,
            chars.len(),
            &[],
            0,
            false,
            ComposerPresentation {
                footer: &footer,
                details: Some(details),
                details_expanded: false,
            },
            80,
        );
        let expanded = render_composer(
            &chars,
            chars.len(),
            &[],
            0,
            false,
            ComposerPresentation {
                footer: &footer,
                details: Some(details),
                details_expanded: true,
            },
            80,
        );

        assert!(collapsed.text.contains("Ctrl+O details"));
        assert!(!collapsed.text.contains("command=demo-output"));
        assert!(expanded.text.contains("Ctrl+O hide"));
        assert!(expanded.text.contains("command=demo-output"));
        assert!(expanded.text.contains("draft stays"));
        assert_eq!(expanded.cursor_col, collapsed.cursor_col);
        assert!(expanded.cursor_row > collapsed.cursor_row);
    }

    #[test]
    fn expanded_tool_details_never_wrap_beyond_the_owned_terminal_width() {
        let chars: Vec<char> = "draft".chars().collect();
        let details = format!("  ● last turn details\n      {}", "x".repeat(100));

        let frame = render_composer(
            &chars,
            chars.len(),
            &[],
            0,
            false,
            ComposerPresentation {
                footer: &ComposerFooter::KeyboardHint,
                details: Some(&details),
                details_expanded: true,
            },
            36,
        );

        for line in frame.text.split("\r\n") {
            assert!(
                crate::theme::visible_len(line) <= 36,
                "composer-owned row wrapped past its tracked width: {line:?}"
            );
        }
    }

    #[test]
    fn composer_redraw_clears_only_its_owned_rows_and_restores_the_edit_cursor() {
        let mut region = ComposerRegion::default();
        let first_chars: Vec<char> = "hello".chars().collect();
        let first = render_hint_composer(&first_chars, first_chars.len(), &[], 0, false, 80);
        let initial = region.replace(first);

        assert!(initial.starts_with('\r'));
        assert!(initial.ends_with("\x1b[1A\r\x1b[11C"));

        let second_chars: Vec<char> = "hello!".chars().collect();
        let second = render_hint_composer(&second_chars, second_chars.len(), &[], 0, false, 80);
        let repaint = region.replace(second);

        assert!(repaint.starts_with("\x1b[1A\r\x1b[0J"));
        assert!(repaint.ends_with("\x1b[1A\r\x1b[12C"));
    }

    #[test]
    fn composer_submission_replaces_the_editor_with_one_durable_user_receipt() {
        let mut region = ComposerRegion::default();
        let chars: Vec<char> = "find the tests".chars().collect();
        let _ = region.replace(render_hint_composer(&chars, chars.len(), &[], 0, false, 80));

        let submitted = region.finish(Some("find the tests"));

        assert!(submitted.starts_with("\x1b[1A\r\x1b[0J"));
        assert!(submitted.contains("user"));
        assert!(
            !submitted.contains('❯'),
            "the prompt arrow belongs only to the active composer: {submitted:?}"
        );
        assert!(submitted.contains("find the tests"));
        assert!(submitted.ends_with("\r\n"));
        assert!(
            !submitted.ends_with("\r\n\r\n"),
            "the activity region owns the breathing row: {submitted:?}"
        );
        assert_eq!(region.clear(), "", "the temporary frame was released");
    }

    #[test]
    fn composer_multiline_input_owns_each_visual_row_and_tracks_the_cursor() {
        let chars: Vec<char> = "first\nsecond".chars().collect();

        let frame = render_hint_composer(&chars, chars.len(), &[], 0, false, 80);

        assert_eq!(frame.rows, 4, "header + two editor rows + hint");
        assert_eq!(frame.cursor_row, 2);
        assert_eq!(frame.cursor_col, 12);
        assert!(frame.text.contains("first\r\n"));
        assert!(frame.text.contains("second"));
        assert!(!frame.text.contains("first\nsecond"));
    }

    #[test]
    fn composer_wraps_inside_a_narrow_terminal_without_losing_row_ownership() {
        let chars: Vec<char> = "123456789012345678901234567890".chars().collect();

        let frame = render_hint_composer(&chars, chars.len(), &[], 0, false, 20);

        assert_eq!(frame.rows, 5, "header + three wrapped editor rows + hint");
        assert_eq!(frame.cursor_row, 3);
        assert_eq!(frame.cursor_col, 10);
        assert!(
            frame
                .text
                .split("\r\n")
                .all(|row| crate::theme::visible_len(row) <= 20),
            "composer row exceeded the terminal width: {:?}",
            frame.text
        );
    }

    #[test]
    fn composer_keeps_command_completions_inside_its_owned_surface() {
        let chars: Vec<char> = "/co".chars().collect();

        let frame = render_hint_composer(&chars, chars.len(), &cmds(), 1, false, 80);

        assert_eq!(frame.rows, 6, "header + editor + 3 matches + hint");
        assert_eq!(frame.cursor_row, 1);
        assert!(frame.text.contains("mpare"), "selected ghost is visible");
        for name in ["/compact", "/compare", "/config"] {
            assert!(frame.text.contains(name), "missing completion {name}");
        }

        let mut region = ComposerRegion::default();
        let painted = region.replace(frame);
        assert!(painted.ends_with("\x1b[4A\r\x1b[9C"));
    }

    #[test]
    fn composer_resize_releases_the_previous_height_before_owning_the_new_height() {
        let chars: Vec<char> = "123456789012345678901234567890".chars().collect();
        let mut region = ComposerRegion::default();
        let _ = region.replace(render_hint_composer(&chars, chars.len(), &[], 0, false, 80));

        let narrow = region.replace(render_hint_composer(&chars, chars.len(), &[], 0, false, 20));
        assert!(narrow.starts_with("\x1b[1A\r\x1b[0J"));

        let wide_again =
            region.replace(render_hint_composer(&chars, chars.len(), &[], 0, false, 80));
        assert!(wide_again.starts_with("\x1b[3A\r\x1b[0J"));
    }

    #[test]
    fn interrupted_composer_clears_the_draft_without_creating_a_user_receipt() {
        let chars: Vec<char> = "unfinished thought".chars().collect();
        let mut region = ComposerRegion::default();
        let _ = region.replace(render_hint_composer(&chars, chars.len(), &[], 0, false, 80));

        let interrupted = region.finish(None);

        assert_eq!(interrupted, "\x1b[1A\r\x1b[0J");
        assert!(!interrupted.contains("unfinished thought"));
    }
}
