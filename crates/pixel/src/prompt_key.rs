// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Single-key terminal input for the interactive setup: the arrows move
//! between Yes and No, Enter confirms, and `y`/`n` answer outright — the
//! inquirer-style pick instead of typing a letter and pressing Enter.
//! Hand-rolled on termios (`libc` is already in the tree): the setup asks
//! one bounded question at a time, so a small raw-mode adapter replaces a
//! prompt framework. Raw mode exists only while a question is on screen;
//! the text prompts of the classify setup keep their canonical terminal.

use std::io::IsTerminal;

/// One decoded keypress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Move the highlight to No.
    Left,
    /// Move the highlight to Yes.
    Right,
    /// Confirm the highlighted choice.
    Enter,
    /// Answer yes outright.
    Yes,
    /// Answer no outright.
    No,
    /// `q`, Esc, Ctrl-C or Ctrl-D: stop without saving further answers.
    Cancel,
}

/// The first byte of a keypress, before any escape sequence continues.
/// `None` means the byte starts (or belongs to) a sequence the caller has
/// to finish, or is noise to ignore.
pub fn decode_byte(byte: u8) -> Option<Key> {
    match byte {
        b'\r' | b'\n' => Some(Key::Enter),
        b'y' | b'Y' => Some(Key::Yes),
        b'n' | b'N' => Some(Key::No),
        b'q' | 0x03 | 0x04 => Some(Key::Cancel),
        _ => None,
    }
}

/// A complete `ESC [` sequence: the second and third byte after the escape.
/// Arrows decode; a sequence that dies before its second byte (a lone Esc
/// press, timed out) cancels; any other sequence is ignored so a stray
/// function-key prefix never answers the question for the user.
pub fn decode_escape(second: Option<u8>, third: Option<u8>) -> Option<Key> {
    match (second, third) {
        (Some(b'['), Some(b'C')) => Some(Key::Right),
        (Some(b'['), Some(b'D')) => Some(Key::Left),
        (None, _) => Some(Key::Cancel),
        _ => None,
    }
}

/// One keypress against the highlighted choice.
pub enum Step {
    /// Move the highlight.
    Highlight(bool),
    /// Settle the question: `Some` is the answer, `None` a cancel.
    Settle(Option<bool>),
}

pub fn step(key: Key, picked: bool) -> Step {
    match key {
        Key::Right => Step::Highlight(true),
        Key::Left => Step::Highlight(false),
        Key::Enter => Step::Settle(Some(picked)),
        Key::Yes => Step::Settle(Some(true)),
        Key::No => Step::Settle(Some(false)),
        Key::Cancel => Step::Settle(None),
    }
}

/// Whether a live terminal can take raw-mode keys. `setup` builds one for
/// the session; tests build [`KeyReader::inert`] and keep the line path.
pub struct KeyReader {
    active: bool,
}

impl KeyReader {
    /// Live when both stdin (the keys) and stderr (the rendering) are a
    /// terminal.
    #[cfg_attr(test, mutants::skip)] // needs a real terminal to tell live from inert; the mode split is tested through `forced`
    pub fn enable() -> Self {
        let active = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
        Self { active }
    }

    /// Never reads keys: the caller falls back to line input. Test-only —
    /// the only production caller builds with [`KeyReader::enable`].
    #[cfg(test)]
    pub fn inert() -> Self {
        Self { active: false }
    }

    /// Test constructor: pretend the terminal is (or is not) live without
    /// consulting the real one.
    #[cfg(test)]
    pub fn forced(active: bool) -> Self {
        Self { active }
    }

    pub fn active(&self) -> bool {
        self.active
    }
}

/// Raw mode for one question. Restored on drop, so a cancel or a panic
/// never leaves the terminal without echo or line discipline.
pub struct RawGuard {
    original: libc::termios,
}

impl RawGuard {
    /// Take stdin into raw mode: no canonical line buffering, no echo, no
    /// signal generation (Ctrl-C arrives as a byte and is a cancel).
    /// `None` when the termios calls fail; the caller then falls back to
    /// line input.
    #[cfg_attr(test, mutants::skip)] // libc adapter; needs a real tty, the key mapping is tested pure.
    pub fn new() -> Option<Self> {
        // SAFETY: zeroed memory is a valid (if meaningless) termios buffer
        // that the tcgetattr call immediately overwrites.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: a single tcgetattr on stdin with a valid, zeroed buffer.
        if unsafe { libc::tcgetattr(0, &mut original) } != 0 {
            return None;
        }
        let mut raw = original;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: TCSANOW applies the descriptor's own modified settings.
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        Some(Self { original })
    }

    /// The next key, blocking until one decodes. Unknown bytes (a paste, a
    /// stray function-key prefix) are ignored rather than answered.
    #[cfg_attr(test, mutants::skip)] // libc adapter; needs a real tty, the key mapping is tested pure.
    pub fn read_key(&mut self) -> Key {
        loop {
            let Some(byte) = self.read_byte() else {
                return Key::Cancel;
            };
            match byte {
                0x1b => {
                    if let Some(key) = self.escape() {
                        return key;
                    }
                }
                byte => {
                    if let Some(key) = decode_byte(byte) {
                        return key;
                    }
                }
            }
        }
    }

    #[cfg_attr(test, mutants::skip)] // libc adapter; needs a real tty, the byte mapping is tested pure
    fn read_byte(&mut self) -> Option<u8> {
        let mut byte = 0u8;
        // SAFETY: a one-byte read into a local buffer on stdin.
        let n = unsafe { libc::read(0, (&mut byte as *mut u8).cast(), 1) };
        if n == 1 { Some(byte) } else { None }
    }

    /// After an escape byte: read the rest of the sequence with a 100 ms
    /// timeout, so a lone Esc cancels instead of hanging the picker.
    #[cfg_attr(test, mutants::skip)] // libc adapter; needs a real tty, the sequence mapping is tested pure
    fn escape(&mut self) -> Option<Key> {
        self.set_decaying();
        let second = self.read_byte();
        let third = self.read_byte();
        self.set_blocking();
        decode_escape(second, third)
    }

    #[cfg_attr(test, mutants::skip)] // libc adapter; needs a real tty
    fn set_decaying(&mut self) {
        self.set_timeouts(0, 1);
    }

    #[cfg_attr(test, mutants::skip)] // libc adapter; needs a real tty
    fn set_blocking(&mut self) {
        self.set_timeouts(1, 0);
    }

    #[cfg_attr(test, mutants::skip)] // libc adapter; needs a real tty
    fn set_timeouts(&mut self, vmin: u8, vtime: u8) {
        // SAFETY: zeroed memory is a valid (if meaningless) termios buffer
        // that the tcgetattr call immediately overwrites.
        let mut termios: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: a tcgetattr on stdin into a valid buffer, to edit only
        // the two timeout bytes before writing the settings back.
        if unsafe { libc::tcgetattr(0, &mut termios) } != 0 {
            return;
        }
        termios.c_cc[libc::VMIN] = vmin;
        termios.c_cc[libc::VTIME] = vtime;
        // SAFETY: TCSANOW applies the descriptor's own modified settings.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &termios) };
    }
}

impl Drop for RawGuard {
    #[cfg_attr(test, mutants::skip)] // libc adapter; the restore needs a real tty to observe
    fn drop(&mut self) {
        // SAFETY: restoring the settings this guard read from stdin.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.original) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_keys_decode_to_their_answers() {
        assert_eq!(decode_byte(b'\r'), Some(Key::Enter));
        assert_eq!(decode_byte(b'\n'), Some(Key::Enter));
        assert_eq!(decode_byte(b'y'), Some(Key::Yes));
        assert_eq!(decode_byte(b'Y'), Some(Key::Yes));
        assert_eq!(decode_byte(b'n'), Some(Key::No));
        assert_eq!(decode_byte(b'N'), Some(Key::No));
        assert_eq!(decode_byte(b'q'), Some(Key::Cancel));
        assert_eq!(decode_byte(0x03), Some(Key::Cancel), "Ctrl-C cancels");
        assert_eq!(decode_byte(0x04), Some(Key::Cancel), "Ctrl-D cancels");
        assert_eq!(decode_byte(b'x'), None, "noise is ignored, not answered");
    }

    #[test]
    fn arrow_sequences_decode_and_everything_else_is_ignored() {
        assert_eq!(decode_escape(Some(b'['), Some(b'C')), Some(Key::Right));
        assert_eq!(decode_escape(Some(b'['), Some(b'D')), Some(Key::Left));
        // A sequence that dies before its second byte (the 100 ms timeout
        // fired after a lone Esc) cancels, so it can never hang the picker.
        assert_eq!(decode_escape(None, Some(b'[')), Some(Key::Cancel));
        assert_eq!(decode_escape(None, None), Some(Key::Cancel));
        // Known non-arrow sequences (Home, F1, ...) are ignored: the picker
        // keeps waiting instead of answering for the user.
        assert_eq!(decode_escape(Some(b'['), Some(b'H')), None);
        assert_eq!(decode_escape(Some(b'O'), Some(b'A')), None);
        assert_eq!(decode_escape(Some(b'['), None), None);
    }

    #[test]
    fn arrows_move_the_highlight_enter_confirms_and_cancel_settles_none() {
        assert!(matches!(step(Key::Right, false), Step::Highlight(true)));
        assert!(matches!(step(Key::Left, true), Step::Highlight(false)));
        assert!(matches!(step(Key::Right, true), Step::Highlight(true)));
        assert!(matches!(step(Key::Enter, false), Step::Settle(Some(false))));
        assert!(matches!(step(Key::Enter, true), Step::Settle(Some(true))));
        assert!(matches!(step(Key::Yes, false), Step::Settle(Some(true))));
        assert!(matches!(step(Key::No, true), Step::Settle(Some(false))));
        assert!(matches!(step(Key::Cancel, true), Step::Settle(None)));
    }
}
