use std::io::{self, Write};

use crossterm::cursor::{Hide, SetCursorStyle, Show};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorState {
    Passive,
    TextInput,
    Restored,
}

pub fn write_state(out: &mut impl Write, state: CursorState) -> io::Result<()> {
    match state {
        CursorState::Passive => write!(out, "{Hide}"),
        CursorState::TextInput => write!(out, "{}{Show}", SetCursorStyle::SteadyBar),
        CursorState::Restored => write!(out, "{}{Show}", SetCursorStyle::DefaultUserShape),
    }
}

pub fn set_state(state: CursorState) -> io::Result<()> {
    let mut out = std::io::stdout();
    write_state(&mut out, state)?;
    out.flush()
}

/// Owns one cursor phase and restores the correct parent phase when dropped.
///
/// A session guard restores the user's terminal preference. A text-input guard
/// returns to the session's passive (hidden-cursor) state.
pub struct CursorGuard {
    restore_to: CursorState,
}

impl CursorGuard {
    pub fn interactive_session() -> io::Result<Self> {
        set_state(CursorState::Passive)?;
        Ok(Self {
            restore_to: CursorState::Restored,
        })
    }

    pub fn text_input() -> io::Result<Self> {
        set_state(CursorState::TextInput)?;
        Ok(Self {
            restore_to: CursorState::Passive,
        })
    }
}

impl Drop for CursorGuard {
    fn drop(&mut self) {
        let _ = set_state(self.restore_to);
    }
}

pub fn restore() {
    let _ = set_state(CursorState::Restored);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_states_emit_the_expected_terminal_contract() {
        let mut output = Vec::new();

        write_state(&mut output, CursorState::Passive).unwrap();
        write_state(&mut output, CursorState::TextInput).unwrap();
        write_state(&mut output, CursorState::Restored).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\x1b[?25l\x1b[6 q\x1b[?25h\x1b[0 q\x1b[?25h"
        );
    }
}
