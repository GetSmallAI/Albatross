use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::Write;
use std::time::Instant;

use crate::agent::AgentEvent;
use crate::config::{DisplayConfig, ToolDisplay};
use crate::markdown::{MarkdownInline, MdEvent};

use crate::theme::{
    content_width, fade_header, Style, ACCENT, BANG, BOLT, BRANCH, BRANCH_END, CHECK, DOT, FAIL,
    HOOK_STOP, OK, PAD, PENDING, POINT, SUB, TEXT,
};

// Map the renderer's palette onto the shared theme. Notably `DIM` no longer
// means ANSI faint (which was the unreadable culprit) — it's now the theme's
// readable bright-black, same as `GRAY`.
const RESET: Style = crate::theme::RESET;
const BOLD: Style = crate::theme::BOLD;
const GREEN: Style = crate::theme::SUCCESS;
const YELLOW: Style = crate::theme::WARN;
const RED: Style = crate::theme::ERROR;
const GRAY: Style = crate::theme::MUTED;
const DIM: Style = crate::theme::MUTED;
const MAGENTA: Style = crate::theme::MAGENTA;

fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
}

fn formatter_for(name: &str, args: &Value) -> String {
    match name {
        "shell" => {
            let s = args.get("command").and_then(Value::as_str).unwrap_or("");
            format!("command={}", trunc(s, 50))
        }
        "file_read" | "file_write" | "file_edit" => {
            let s = args.get("path").and_then(Value::as_str).unwrap_or("");
            format!("path={}", trunc(s, 50))
        }
        "glob" | "grep" => {
            let s = args.get("pattern").and_then(Value::as_str).unwrap_or("");
            format!("pattern={}", trunc(s, 50))
        }
        "list_dir" => {
            let s = args.get("path").and_then(Value::as_str).unwrap_or(".");
            format!("path={}", trunc(s, 50))
        }
        "task" => {
            let s = args.get("task").and_then(Value::as_str).unwrap_or("");
            trunc(s, 60)
        }
        _ => default_format(args),
    }
}

fn default_format(args: &Value) -> String {
    if let Some(obj) = args.as_object() {
        if let Some((k, v)) = obj.iter().next() {
            let vs = match v {
                Value::String(s) => s.clone(),
                v => v.to_string(),
            };
            return format!("{k}={}", trunc(&vs, 50));
        }
    }
    String::new()
}

fn label_past(name: &str) -> &'static str {
    match name {
        "shell" => "Ran",
        "file_read" => "Read",
        "file_write" => "Wrote",
        "file_edit" => "Edited",
        "glob" => "Explored",
        "grep" => "Searched",
        "list_dir" => "Listed",
        "task" => "Delegated",
        _ => "Used",
    }
}

fn label_noun(name: &str) -> &'static str {
    match name {
        "shell" => "shell command",
        "file_read" | "file_write" | "file_edit" => "file",
        "glob" | "grep" => "pattern",
        "list_dir" => "directory",
        _ => "tool",
    }
}

fn tool_color(name: &str) -> Style {
    match name {
        "shell" => RED,
        "file_write" | "file_edit" => YELLOW,
        "grep" => MAGENTA,
        _ => YELLOW,
    }
}

fn summarize_output(output: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<Value>(output) {
        if let Some(err) = parsed.get("error").and_then(Value::as_str) {
            return format!("{RED}error: {}{RESET}", trunc(err, 60));
        }
        if let Some(n) = parsed.get("totalLines").and_then(Value::as_u64) {
            return format!("{n} lines");
        }
        if let Some(n) = parsed.get("count").and_then(Value::as_u64) {
            let kind = if parsed.get("matches").is_some() {
                "matches"
            } else {
                "entries"
            };
            return format!("{n} {kind}");
        }
        if let Some(summary) = parsed.get("summary").and_then(Value::as_str) {
            let first_line = summary.split('\n').next().unwrap_or("");
            return trunc(first_line, 60);
        }
        if parsed.get("written").is_some() {
            let bytes = parsed.get("bytes").and_then(Value::as_u64).unwrap_or(0);
            return format!("wrote {bytes} bytes");
        }
        if parsed.get("edited").is_some() {
            if parsed.get("verified").and_then(Value::as_bool) == Some(false) {
                return format!("{YELLOW}edited (unverified — disk differs){RESET}");
            }
            return "edited".to_string();
        }
        if let Some(code) = parsed.get("exitCode").and_then(Value::as_i64) {
            let to = if parsed
                .get("timedOut")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                " (timeout)"
            } else {
                ""
            };
            return format!("exit {code}{to}");
        }
    }
    let first_line = output.split('\n').next().unwrap_or("");
    trunc(first_line, 60)
}

/// Incremental word-wrapper for streamed assistant text.
///
/// Wraps to `inner` visible columns, preserves hard newlines and a logical
/// line's leading indentation (so lists and code keep their shape), and
/// re-emits `gutter` after every line break so the answer stays aligned under
/// the panel. `feed` returns the exact text to write for a streamed chunk;
/// `finish` flushes the trailing buffered word. State persists across chunks,
/// so it wraps correctly even when words arrive split across deltas.
///
/// ANSI-blind: escape sequences (`ESC [ … m`) travel with the word they
/// belong to but count as zero visible width, so injected color/bold codes
/// never skew the wrap column. In verbatim mode (used for fenced code blocks)
/// text passes through untouched except that hard newlines re-emit the gutter
/// — long code lines wrap at the terminal edge, preserving copy-paste fidelity.
struct StreamWrap {
    inner: usize,
    gutter: String,
    col: usize,
    indent: usize,
    word: String,
    at_line_start: bool,
    need_space: bool,
    in_escape: bool,
    verbatim: bool,
}

impl StreamWrap {
    fn new(inner: usize, gutter: impl Into<String>) -> Self {
        Self {
            inner: inner.max(8),
            gutter: gutter.into(),
            col: 0,
            indent: 0,
            word: String::new(),
            at_line_start: true,
            need_space: false,
            in_escape: false,
            verbatim: false,
        }
    }

    /// Toggle verbatim (no-wrap) mode. Flushes any pending word first and
    /// resets line state, returning the flushed remainder so nothing is lost.
    /// Used by the streamed-markdown code-fence handler.
    fn set_verbatim(&mut self, on: bool) -> String {
        let mut out = String::new();
        self.flush_word(&mut out);
        self.verbatim = on;
        self.col = 0;
        self.indent = 0;
        self.at_line_start = true;
        self.need_space = false;
        self.in_escape = false;
        out
    }

    fn line_break(&mut self, out: &mut String, hard: bool) {
        out.push('\n');
        out.push_str(&self.gutter);
        self.col = 0;
        self.need_space = false;
        if hard {
            self.indent = 0;
            self.at_line_start = true;
        }
    }

    fn flush_word(&mut self, out: &mut String) {
        if self.word.is_empty() {
            return;
        }
        let word = std::mem::take(&mut self.word);
        let wlen = crate::theme::visible_len(&word);

        // A single token longer than the line (e.g. a URL): hard-break it.
        // Escape sequences are emitted verbatim and never counted or split.
        if wlen > self.inner {
            if self.need_space && self.col > self.indent && self.col < self.inner {
                out.push(' ');
                self.col += 1;
            }
            let mut in_esc = false;
            for ch in word.chars() {
                if in_esc {
                    out.push(ch);
                    if ch == 'm' {
                        in_esc = false;
                    }
                    continue;
                }
                if ch == '\x1b' {
                    in_esc = true;
                    out.push(ch);
                    continue;
                }
                if self.col >= self.inner {
                    self.line_break(out, false);
                    for _ in 0..self.indent {
                        out.push(' ');
                    }
                    self.col = self.indent;
                }
                out.push(ch);
                self.col += 1;
            }
            self.need_space = false;
            return;
        }

        let sep = usize::from(self.need_space && self.col > self.indent);
        if self.col + sep + wlen > self.inner {
            self.line_break(out, false);
            for _ in 0..self.indent {
                out.push(' ');
            }
            self.col = self.indent;
        } else if sep == 1 {
            out.push(' ');
            self.col += 1;
        }
        out.push_str(&word);
        self.col += wlen;
        self.need_space = false;
    }

    fn feed(&mut self, s: &str) -> String {
        let mut out = String::new();
        if self.verbatim {
            // No wrapping, no word buffering: emit as-is, but re-indent each
            // physical line with the gutter so code stays under the panel.
            for ch in s.chars() {
                out.push(ch);
                if ch == '\n' {
                    out.push_str(&self.gutter);
                }
            }
            return out;
        }
        for ch in s.chars() {
            // An in-flight escape sequence: buffer every char with the current
            // word (zero visible width) until the terminating `m`.
            if self.in_escape {
                self.word.push(ch);
                if ch == 'm' {
                    self.in_escape = false;
                }
                continue;
            }
            match ch {
                '\x1b' => {
                    // Start of a zero-width escape. Buffer it with the word but
                    // don't let it flip `at_line_start` or count as content.
                    self.in_escape = true;
                    self.word.push(ch);
                }
                '\n' => {
                    self.flush_word(&mut out);
                    self.line_break(&mut out, true);
                }
                ' ' | '\t' => {
                    if self.at_line_start {
                        // Preserve a logical line's leading indentation.
                        if self.col < self.inner {
                            out.push(' ');
                            self.col += 1;
                            self.indent = self.col;
                        }
                    } else {
                        self.flush_word(&mut out);
                        self.need_space = true;
                    }
                }
                _ => {
                    self.at_line_start = false;
                    self.word.push(ch);
                }
            }
        }
        out
    }

    fn finish(&mut self) -> String {
        let mut out = String::new();
        self.flush_word(&mut out);
        out
    }
}

struct PendingCall {
    name: String,
    call_id: String,
    args: Value,
    output: Option<String>,
}

pub struct TuiRenderer {
    display: DisplayConfig,
    tool_start: HashMap<String, Instant>,
    streaming: bool,
    grouped_pending: Vec<PendingCall>,
    grouped_category: String,
    minimal_batch: BTreeMap<String, usize>,
    /// Per-turn flag: have we already printed the "thinking…" header for the
    /// current burst of reasoning deltas? Reset at end_turn so each turn
    /// gets its own header.
    reasoning_header_shown: bool,
    /// `Some` while the assistant's answer panel (`╭─ response … ╰─`) is open
    /// and streaming. Holds the word-wrap state so content stays inside the
    /// panel. Closed when a tool call/reasoning interrupts or the turn ends.
    answer_wrap: Option<StreamWrap>,
    /// Inline-markdown state for the open answer panel, paired with
    /// `answer_wrap`. Styles bold/italic/code/headings/bullets and frames
    /// fenced code blocks as the answer streams.
    answer_md: Option<MarkdownInline>,
    /// The configured tool display, remembered so `/verbose off` can restore it.
    base_tool_display: ToolDisplay,
    trace_enabled: bool,
}

/// Route one markdown event to the answer wrapper: styled text flows through
/// the wrapper; fence signals flip verbatim mode and print framing rules. Kept
/// free-standing so it can borrow the wrapper without the whole renderer.
fn write_md_event(out: &mut impl Write, wrap: &mut StreamWrap, ev: MdEvent) {
    match ev {
        MdEvent::Text(s) => {
            let chunk = wrap.feed(&s);
            let _ = write!(out, "{chunk}");
        }
        MdEvent::CodeStart { lang } => {
            // A newline+gutter always precedes a fence, so the rule (which has
            // no leading PAD) lands correctly under the answer gutter.
            let flushed = wrap.set_verbatim(true);
            let _ = write!(out, "{flushed}");
            let _ = writeln!(out, "{}", crate::theme::code_fence_rule(&lang));
            let _ = write!(out, "{PAD}");
        }
        MdEvent::CodeEnd => {
            let flushed = wrap.set_verbatim(false);
            let _ = write!(out, "{flushed}");
            let _ = writeln!(out, "{}", crate::theme::code_fence_rule(""));
            let _ = write!(out, "{PAD}{TEXT}");
        }
    }
}

impl TuiRenderer {
    pub fn new(display: DisplayConfig) -> Self {
        let base_tool_display = display.tool_display;
        Self {
            display,
            tool_start: HashMap::new(),
            streaming: false,
            grouped_pending: Vec::new(),
            grouped_category: String::new(),
            minimal_batch: BTreeMap::new(),
            reasoning_header_shown: false,
            answer_wrap: None,
            answer_md: None,
            base_tool_display,
            trace_enabled: false,
        }
    }

    /// Toggle reasoning panel visibility at runtime. Returns the new state.
    pub fn set_reasoning(&mut self, on: bool) -> bool {
        self.display.reasoning = on;
        on
    }

    pub fn reasoning_enabled(&self) -> bool {
        self.display.reasoning
    }

    /// Toggle the verbose tool view at runtime. `on` switches to the detailed
    /// per-call view; `off` restores the configured display.
    pub fn set_verbose(&mut self, on: bool) {
        self.display.tool_display = if on {
            ToolDisplay::Verbose
        } else {
            self.base_tool_display
        };
    }

    pub fn verbose_enabled(&self) -> bool {
        matches!(self.display.tool_display, ToolDisplay::Verbose)
    }

    pub fn set_trace(&mut self, on: bool) {
        self.trace_enabled = on;
    }

    #[allow(dead_code)]
    pub fn trace_enabled(&self) -> bool {
        self.trace_enabled
    }

    pub fn handle(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Text { delta } => self.render_text(&delta),
            AgentEvent::ToolCall {
                name,
                call_id,
                args,
                depth,
            } => {
                self.end_answer();
                self.render_tool_call(&name, &call_id, args, depth)
            }
            // Progress is rendered by the session loader. Keeping this out of
            // the transcript avoids duplicate tool rows.
            AgentEvent::ToolExecutionStarted { .. } => {}
            AgentEvent::ToolResult {
                name,
                call_id,
                output,
                depth,
            } => self.render_tool_result(&name, &call_id, &output, depth),
            AgentEvent::ToolOutputCompacted {
                name,
                summary,
                depth,
                ..
            } => self.render_compaction_notice(&name, &summary, depth),
            AgentEvent::Reasoning { delta } => {
                if self.display.reasoning {
                    self.end_answer();
                }
                self.render_reasoning(&delta)
            }
            AgentEvent::ContextCompacted { notice, .. } => {
                self.end_answer();
                println!("{notice}");
            }
            AgentEvent::HookNotice(notice) => {
                self.end_answer();
                self.render_hook_notice(&notice);
            }
            AgentEvent::StepLimitReached { max_steps } => {
                self.end_answer();
                self.end_streaming();
                println!(
                    "  {YELLOW}⚠ stopped after {max_steps} steps (step budget){RESET} {DIM}— the task may be unfinished. Send \"continue\" to resume, or raise maxSteps in config.{RESET}"
                );
            }
        }
    }

    pub fn end_turn(&mut self) {
        self.end_answer();
        self.flush_grouped();
        self.flush_minimal();
        self.end_reasoning();
        self.end_streaming();
        self.reasoning_header_shown = false;
    }

    /// Close the assistant answer: flush the last buffered word and end the
    /// line. No footer/border — the header-only treatment needs no closing rule.
    /// No-op if no answer is open.
    fn end_answer(&mut self) {
        let Some(mut wrap) = self.answer_wrap.take() else {
            return;
        };
        let mut out = std::io::stdout();
        // Drain any buffered markdown (unclosed markers → literal, an open
        // fence → CodeEnd) before flushing the wrapper's trailing word.
        if let Some(mut md) = self.answer_md.take() {
            for ev in md.finish() {
                write_md_event(&mut out, &mut wrap, ev);
            }
        }
        let tail = wrap.finish();
        let _ = write!(out, "{tail}");
        let _ = writeln!(out, "{RESET}");
        let _ = out.flush();
    }

    /// Close out the reasoning panel before switching to other output (text,
    /// tool calls). Adds a trailing newline only if we actually printed one.
    fn end_reasoning(&mut self) {
        if self.reasoning_header_shown {
            let mut out = std::io::stdout();
            let _ = writeln!(out, "{RESET}");
            let _ = out.flush();
        }
    }

    fn end_streaming(&mut self) {
        if self.streaming {
            let mut out = std::io::stdout();
            let _ = writeln!(out, "{RESET}");
            let _ = out.flush();
            self.streaming = false;
        }
    }

    fn render_text(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.flush_minimal();
        // Switching from reasoning to answer text — close the panel cleanly
        // so the answer doesn't appear glued to the dim trace.
        if self.reasoning_header_shown {
            self.end_reasoning();
            self.reasoning_header_shown = false;
        }
        let mut out = std::io::stdout();
        if self.answer_wrap.is_none() {
            let _ = writeln!(out);
            let _ = writeln!(out, "{}", fade_header("response"));
            let _ = write!(out, "{PAD}{TEXT}");
            // Wrap to the real content width so the answer fills the terminal
            // (like naturally-wrapped text) instead of overflowing.
            self.answer_wrap = Some(StreamWrap::new(content_width(), PAD));
            self.answer_md = Some(MarkdownInline::new());
        }
        if let (Some(md), Some(wrap)) = (self.answer_md.as_mut(), self.answer_wrap.as_mut()) {
            for ev in md.feed(delta) {
                write_md_event(&mut out, wrap, ev);
            }
        }
        let _ = out.flush();
    }

    fn render_reasoning(&mut self, delta: &str) {
        if !self.display.reasoning {
            return;
        }
        self.end_streaming();
        let mut out = std::io::stdout();
        if !self.reasoning_header_shown {
            let _ = writeln!(out, "{GRAY}  thinking…{RESET}");
            let _ = write!(out, "{DIM}  ");
            self.reasoning_header_shown = true;
        }
        let dimmed = delta.replace('\n', "\n  ");
        let _ = write!(out, "{dimmed}");
        let _ = out.flush();
    }

    fn render_tool_call(&mut self, name: &str, call_id: &str, args: Value, depth: u32) {
        if matches!(self.display.tool_display, ToolDisplay::Hidden) {
            return;
        }
        if depth > 0 && !self.trace_enabled {
            return;
        }
        // The plan is rendered as a checklist at call time (the args carry the
        // plan), independent of the configured tool display style, and never
        // routed through the grouped/minimal batching.
        if name == "update_plan" {
            self.render_plan(&args);
            return;
        }
        self.end_streaming();
        self.tool_start.insert(call_id.to_string(), Instant::now());

        if depth > 0 {
            let arg_str = formatter_for(name, &args);
            let indent = "  ".repeat(depth as usize);
            println!(
                "{PAD}{indent}{DIM}{SUB} {name}{RESET} {GRAY}{}{RESET}",
                if arg_str.is_empty() {
                    String::new()
                } else {
                    format!(" {arg_str}")
                }
            );
            return;
        }

        match self.display.tool_display {
            ToolDisplay::Emoji => {
                let color = tool_color(name);
                let arg_str = formatter_for(name, &args);
                let sep = if arg_str.is_empty() { "" } else { " " };
                println!("  {color}{BOLT}{RESET} {DIM}{name}{sep}{arg_str}{RESET}");
            }
            ToolDisplay::Grouped => {
                let category = label_past(name).to_string();
                if category != self.grouped_category {
                    self.flush_grouped();
                    self.grouped_category = category;
                }
                self.grouped_pending.push(PendingCall {
                    name: name.to_string(),
                    call_id: call_id.to_string(),
                    args,
                    output: None,
                });
            }
            ToolDisplay::Minimal => {
                *self.minimal_batch.entry(name.to_string()).or_insert(0) += 1;
            }
            ToolDisplay::Verbose => {
                println!("{PAD}{ACCENT}→{RESET} {BOLD}{name}{RESET}");
                if let Some(obj) = args.as_object() {
                    for (k, v) in obj {
                        let vs = match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        let vs = vs.replace('\n', "⏎");
                        println!("{PAD}    {GRAY}{k}:{RESET} {}", trunc(&vs, 160));
                    }
                } else if !args.is_null() {
                    println!("{PAD}    {GRAY}{}{RESET}", trunc(&args.to_string(), 160));
                }
            }
            ToolDisplay::Hidden => {}
        }
    }

    /// Draw the model's task plan as a checklist box. Flushes any in-flight
    /// grouped/minimal output first so the plan sits on its own.
    fn render_plan(&mut self, args: &Value) {
        self.end_answer();
        self.flush_grouped();
        self.flush_minimal();
        self.end_streaming();
        let Some(steps) = args.get("steps").and_then(Value::as_array) else {
            return;
        };
        if steps.is_empty() {
            return;
        }
        let done = steps
            .iter()
            .filter(|s| s.get("status").and_then(Value::as_str) == Some("done"))
            .count();
        // Leading blank: the plan block owns its own gap (no trailing blank).
        println!();
        println!(
            "{PAD}{ACCENT}{DOT}{RESET} {BOLD}Plan{RESET}  {GRAY}{done}/{} done{RESET}",
            steps.len()
        );
        for s in steps {
            let text = s.get("step").and_then(Value::as_str).unwrap_or("");
            let status = s.get("status").and_then(Value::as_str).unwrap_or("pending");
            let (mark, body) = match status {
                "done" => (
                    format!("{GREEN}{CHECK}{RESET}"),
                    format!("{GRAY}{text}{RESET}"),
                ),
                "in_progress" => (
                    format!("{YELLOW}{POINT}{RESET}"),
                    format!("{BOLD}{text}{RESET}"),
                ),
                _ => (
                    format!("{GRAY}{PENDING}{RESET}"),
                    format!("{GRAY}{text}{RESET}"),
                ),
            };
            println!("{PAD}  {mark} {body}");
        }
    }

    fn render_tool_result(&mut self, name: &str, call_id: &str, output: &str, depth: u32) {
        if matches!(self.display.tool_display, ToolDisplay::Hidden) {
            return;
        }
        if depth > 0 && !self.trace_enabled {
            return;
        }
        // The plan was already drawn at call time; its result carries no new
        // information for the user.
        if name == "update_plan" {
            return;
        }
        let ms = self
            .tool_start
            .get(call_id)
            .map(|s| s.elapsed().as_millis() as f64 / 1000.0)
            .unwrap_or(0.0);
        let dur = format!("({:.1}s)", ms);

        if depth > 0 {
            let indent = "  ".repeat(depth as usize);
            let summary = summarize_output(output);
            println!(
                "{PAD}{indent}{DIM}{SUB} {name} {dur}{RESET} {GRAY}{}{RESET}",
                if summary.is_empty() {
                    String::new()
                } else {
                    format!(" {summary}")
                }
            );
            return;
        }

        match self.display.tool_display {
            ToolDisplay::Emoji => {
                println!("  {GREEN}{OK}{RESET} {DIM}{name} {dur}{RESET}");
            }
            ToolDisplay::Grouped => {
                if let Some(p) = self
                    .grouped_pending
                    .iter_mut()
                    .find(|p| p.call_id == call_id)
                {
                    p.output = Some(output.to_string());
                }
            }
            ToolDisplay::Verbose => {
                println!("{PAD}  {GRAY}←{RESET} {DIM}{dur}{RESET}");
                for line in trunc(output, 1200).lines() {
                    println!("{PAD}    {GRAY}{line}{RESET}");
                }
            }
            _ => {}
        }
    }

    fn render_compaction_notice(&mut self, name: &str, summary: &str, depth: u32) {
        if depth > 0 && !self.trace_enabled {
            return;
        }
        let indent = if depth > 0 {
            "  ".repeat(depth as usize)
        } else {
            String::new()
        };
        println!("{PAD}{indent}{DIM}{SUB} {name} output compacted: {summary}{RESET}");
    }

    fn render_hook_notice(&mut self, notice: &crate::hooks::HookNotice) {
        use crate::hooks::HookNoticeLevel;

        let (mark, label, color) = match notice.level {
            HookNoticeLevel::Warning => (BANG, "hook warning", YELLOW),
            HookNoticeLevel::Blocked => (FAIL, "hook blocked", RED),
            HookNoticeLevel::Denied => (FAIL, "hook denied", RED),
            HookNoticeLevel::Stopped => (HOOK_STOP, "hook stopped", YELLOW),
            HookNoticeLevel::Feedback => (SUB, "hook", DIM),
        };
        println!(
            "{PAD}{color}{mark}{RESET} {DIM}{label} {}:{RESET} {}",
            notice.event.as_str(),
            notice.message
        );
    }

    fn flush_grouped(&mut self) {
        if self.grouped_pending.is_empty() {
            return;
        }
        // A block owns exactly one leading blank line and no trailing blank, so
        // it separates cleanly from whatever came before without stacking a
        // second blank against the next block's own leading gap.
        println!();
        let pending = std::mem::take(&mut self.grouped_pending);
        let first = &pending[0];
        let label = label_past(&first.name);

        if pending.len() == 1 {
            let arg_str = formatter_for(&first.name, &first.args);
            println!("{PAD}{ACCENT}{DOT}{RESET} {BOLD}{label}{RESET}  {TEXT}{arg_str}{RESET}");
            if let Some(out) = &first.output {
                let summary = summarize_output(out);
                if !summary.is_empty() {
                    println!("{PAD}  {GRAY}{BRANCH_END} {summary}{RESET}");
                }
            }
        } else {
            println!("{PAD}{ACCENT}{DOT}{RESET} {BOLD}{label}{RESET}");
            let n = pending.len();
            for (i, p) in pending.iter().enumerate() {
                let is_last = i == n - 1;
                let branch = if is_last { BRANCH_END } else { BRANCH };
                let arg_str = formatter_for(&p.name, &p.args);
                let summary = p
                    .output
                    .as_ref()
                    .map(|o| format!("  {GRAY}{}{RESET}", summarize_output(o)))
                    .unwrap_or_default();
                println!("{PAD}  {GRAY}{branch}{RESET} {TEXT}{arg_str}{RESET}{summary}");
            }
        }
        self.grouped_category.clear();
    }

    fn flush_minimal(&mut self) {
        if self.minimal_batch.is_empty() {
            return;
        }
        let parts: Vec<String> = self
            .minimal_batch
            .iter()
            .map(|(name, count)| {
                let past = label_past(name).to_lowercase();
                let noun = label_noun(name);
                let plural = if *count == 1 { "" } else { "s" };
                format!("{past} {count} {noun}{plural}")
            })
            .collect();
        println!("  {GRAY}{}{RESET}", parts.join(", "));
        self.minimal_batch.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::StreamWrap;

    /// Feed `text` through the wrapper one char at a time (worst case for an
    /// incremental wrapper) and return the visible lines, gutter stripped.
    fn wrapped_lines(text: &str, inner: usize) -> Vec<String> {
        let mut w = StreamWrap::new(inner, "");
        let mut out = String::new();
        for ch in text.chars() {
            out.push_str(&w.feed(&ch.to_string()));
        }
        out.push_str(&w.finish());
        out.split('\n').map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_line_exceeds_inner_width() {
        let text = "Absolutely! To get started, we need to set up a basic Python API. \
                    A common approach is to use Flask, a lightweight framework for building APIs.";
        for inner in [20usize, 40, 72] {
            for line in wrapped_lines(text, inner) {
                assert!(
                    line.chars().count() <= inner,
                    "line {:?} exceeds inner={inner}",
                    line
                );
            }
        }
    }

    #[test]
    fn wraps_at_word_boundaries_not_mid_word() {
        let lines = wrapped_lines("alpha beta gamma delta", 11);
        // Greedy wrap at width 11: "alpha beta" (10) then "gamma delta" (11).
        assert_eq!(
            lines,
            vec!["alpha beta".to_string(), "gamma delta".to_string()]
        );
    }

    #[test]
    fn preserves_hard_newlines() {
        let lines = wrapped_lines("one\ntwo\nthree", 80);
        assert_eq!(lines, vec!["one", "two", "three"]);
    }

    #[test]
    fn preserves_leading_indentation_with_hanging_indent() {
        // A list item whose text wraps should keep its 4-space indent on the
        // continuation line.
        let lines = wrapped_lines("    1. install the dependency now", 16);
        assert_eq!(lines[0], "    1. install");
        assert!(
            lines[1].starts_with("    "),
            "continuation kept indent: {:?}",
            lines[1]
        );
        assert!(lines.iter().all(|l| l.chars().count() <= 16));
    }

    #[test]
    fn hard_breaks_an_overlong_token() {
        let url = "https://example.com/a/very/long/path/that/cannot/fit";
        let lines = wrapped_lines(url, 20);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|l| l.chars().count() <= 20));
        // All characters survive the break.
        assert_eq!(lines.concat(), url);
    }

    #[test]
    fn splitting_a_word_across_feeds_still_wraps_correctly() {
        // Same text, fed in two arbitrary chunks, must wrap identically.
        let mut w = StreamWrap::new(11, "");
        let mut out = String::new();
        out.push_str(&w.feed("alpha be"));
        out.push_str(&w.feed("ta gamma delta"));
        out.push_str(&w.finish());
        assert_eq!(out, "alpha beta\ngamma delta");
    }

    #[test]
    fn ansi_codes_do_not_count_toward_width() {
        // A bold-wrapped word plus enough plain words to force a wrap; the
        // escapes must not push any visible line past `inner`.
        let text = "\x1b[1mbold\x1b[0m word wraps around here";
        let lines = wrapped_lines(text, 10);
        for line in &lines {
            assert!(
                crate::theme::visible_len(line) <= 10,
                "visible width of {:?} exceeds 10",
                line
            );
        }
        // The escape codes survived intact in the output.
        let joined = lines.join("\n");
        assert!(joined.contains("\x1b[1m"));
        assert!(joined.contains("\x1b[0m"));
    }

    #[test]
    fn escape_split_across_feeds_stays_zero_width() {
        // The escape's `\x1b[` and `1mword` arrive in separate chunks.
        let mut w = StreamWrap::new(11, "");
        let mut out = String::new();
        out.push_str(&w.feed("\x1b["));
        out.push_str(&w.feed("1mword\x1b[0m plus more"));
        out.push_str(&w.finish());
        for line in out.split('\n') {
            assert!(crate::theme::visible_len(line) <= 11, "line {:?}", line);
        }
        assert!(out.contains("\x1b[1mword"));
    }

    #[test]
    fn overlong_token_with_ansi_never_splits_an_escape() {
        // A styled token longer than the line must hard-break, but never in
        // the middle of the `\x1b[…m` sequence.
        let mut w = StreamWrap::new(8, "");
        let mut out = String::new();
        out.push_str(&w.feed("\x1b[92maaaaaaaaaaaaaaaa\x1b[0m"));
        out.push_str(&w.finish());
        // Walk each physical line: once we see ESC we must reach its `m`
        // before the line ends, i.e. no escape is left dangling by a break.
        for line in out.split('\n') {
            let mut in_esc = false;
            for ch in line.chars() {
                if in_esc {
                    if ch == 'm' {
                        in_esc = false;
                    }
                } else if ch == '\x1b' {
                    in_esc = true;
                }
            }
            assert!(!in_esc, "escape split across a line break: {:?}", line);
        }
        assert!(out.contains("\x1b[92m"));
        assert!(out.contains("\x1b[0m"));
    }

    #[test]
    fn markdown_then_wrap_respects_width_with_styling() {
        // Styled markdown streamed through MarkdownInline into StreamWrap must
        // never produce a visible line wider than `inner`, despite the ANSI
        // codes injected for bold/italic/code.
        use crate::markdown::{MarkdownInline, MdEvent};
        crate::theme::init(crate::config::ColorMode::Always, false);
        let input =
            "Here is **bold text** and `inline code` plus _emphasis_ that must wrap cleanly around.";
        let mut md = MarkdownInline::new();
        let mut wrap = StreamWrap::new(20, "");
        let mut out = String::new();
        for ch in input.chars() {
            for ev in md.feed(&ch.to_string()) {
                if let MdEvent::Text(s) = ev {
                    out.push_str(&wrap.feed(&s));
                }
            }
        }
        for ev in md.finish() {
            if let MdEvent::Text(s) = ev {
                out.push_str(&wrap.feed(&s));
            }
        }
        out.push_str(&wrap.finish());
        for line in out.split('\n') {
            assert!(
                crate::theme::visible_len(line) <= 20,
                "styled line {:?} exceeds width 20 (visible {})",
                line,
                crate::theme::visible_len(line)
            );
        }
        // Styling actually happened.
        assert!(out.contains("\x1b[1m"));
    }

    #[test]
    fn verbatim_mode_passes_through_with_gutter() {
        let mut w = StreamWrap::new(10, ">>");
        let mut out = String::new();
        let _ = w.set_verbatim(true);
        // A code line far longer than `inner` is NOT wrapped; newlines re-emit
        // the gutter.
        out.push_str(&w.feed("let x = a_very_long_identifier_here;\nnext"));
        out.push_str(&w.finish());
        assert_eq!(out, "let x = a_very_long_identifier_here;\n>>next");
    }
}

#[cfg(test)]
mod verbose_tests {
    use super::*;
    use crate::config::{DisplayConfig, ToolDisplay};

    #[test]
    fn hidden_reasoning_does_not_close_streaming_answer() {
        let mut r = TuiRenderer::new(DisplayConfig::default());
        assert!(!r.reasoning_enabled());

        r.handle(crate::agent::AgentEvent::Text {
            delta: "Hel".into(),
        });
        assert!(r.answer_wrap.is_some());

        r.handle(crate::agent::AgentEvent::Reasoning {
            delta: "thinking".into(),
        });

        assert!(r.answer_wrap.is_some());
        r.end_turn();
    }

    #[test]
    fn verbose_toggle_restores_configured_display() {
        let mut r = TuiRenderer::new(DisplayConfig::default());
        assert!(!r.verbose_enabled());
        r.set_verbose(true);
        assert!(r.verbose_enabled());
        r.set_verbose(false);
        assert!(!r.verbose_enabled());
        assert!(matches!(r.display.tool_display, ToolDisplay::Grouped));
    }
}

#[cfg(test)]
mod thinking_model_tests {
    use super::*;
    use crate::agent::AgentEvent;
    use crate::config::DisplayConfig;

    fn renderer_reasoning_off() -> TuiRenderer {
        let cfg = DisplayConfig {
            reasoning: false,
            ..Default::default()
        };
        TuiRenderer::new(cfg)
    }

    fn renderer_reasoning_on() -> TuiRenderer {
        let cfg = DisplayConfig {
            reasoning: true,
            ..Default::default()
        };
        TuiRenderer::new(cfg)
    }

    /// Thinking models (qwen3, deepseek-r1) emit empty `content` strings
    /// alongside each `reasoning` token. An empty Text event must not open
    /// a new response block — that was the root cause of the per-word rendering
    /// bug (#thinking-model-response-headers).
    #[test]
    fn empty_text_delta_does_not_open_answer_block() {
        let mut r = renderer_reasoning_off();
        assert!(r.answer_wrap.is_none());
        r.handle(AgentEvent::Text {
            delta: String::new(),
        });
        assert!(
            r.answer_wrap.is_none(),
            "empty Text delta must not open an answer_wrap block"
        );
    }

    /// When reasoning display is OFF, a Reasoning event must not close the
    /// current answer block. Before the fix, `end_answer()` was called
    /// unconditionally, destroying the in-progress response on every
    /// reasoning token.
    #[test]
    fn reasoning_event_preserves_answer_block_when_reasoning_display_off() {
        let mut r = renderer_reasoning_off();
        // Open an answer block by sending real text.
        r.handle(AgentEvent::Text {
            delta: "hello".to_string(),
        });
        assert!(r.answer_wrap.is_some(), "setup: answer_wrap should be open");
        // Reasoning event should NOT close it when reasoning=false.
        r.handle(AgentEvent::Reasoning {
            delta: "thinking...".to_string(),
        });
        assert!(
            r.answer_wrap.is_some(),
            "answer_wrap must survive a Reasoning event when reasoning display is off"
        );
    }

    /// When reasoning display is ON, a Reasoning event closes the answer block
    /// (to print the reasoning panel). This is the intended behaviour.
    #[test]
    fn reasoning_event_closes_answer_block_when_reasoning_display_on() {
        let mut r = renderer_reasoning_on();
        r.handle(AgentEvent::Text {
            delta: "hello".to_string(),
        });
        assert!(r.answer_wrap.is_some(), "setup: answer_wrap should be open");
        r.handle(AgentEvent::Reasoning {
            delta: "thinking...".to_string(),
        });
        assert!(
            r.answer_wrap.is_none(),
            "answer_wrap should be closed when reasoning display is on"
        );
    }
}
