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

/// One decoded keypress in a list picker: several rows, each independently
/// checked, and a cursor that moves rather than a single Yes/No highlight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKey {
    /// Move the cursor up.
    Up,
    /// Move the cursor down.
    Down,
    /// Toggle the row under the cursor.
    Space,
    /// Check every row this scope can use.
    All,
    /// Uncheck every row this scope can use.
    None,
    /// Answer with what is checked.
    Enter,
    /// Stop without answering.
    Cancel,
}

/// Decode one byte in a list picker. `None` is noise to ignore, so a paste or
/// a stray byte never answers for the user.
pub fn decode_list_byte(byte: u8) -> Option<ListKey> {
    match byte {
        b'\r' | b'\n' => Some(ListKey::Enter),
        b' ' => Some(ListKey::Space),
        b'a' | b'A' => Some(ListKey::All),
        b'n' | b'N' => Some(ListKey::None),
        b'k' => Some(ListKey::Up),
        b'j' => Some(ListKey::Down),
        b'q' | 0x03 | 0x04 => Some(ListKey::Cancel),
        _ => None,
    }
}

/// Decode a complete `ESC [` sequence in a list picker: `[A`/`[B` are the
/// vertical arrows and `[D`/`[C` the horizontal ones, which the Apply/Cancel
/// row reads as its left/right pick (in a vertical list they move the same
/// way). A sequence that died before its second byte is a lone Esc press,
/// and cancels.
pub fn decode_list_escape(second: Option<u8>, third: Option<u8>) -> Option<ListKey> {
    match (second, third) {
        (Some(b'['), Some(b'A' | b'D')) => Some(ListKey::Up),
        (Some(b'['), Some(b'B' | b'C')) => Some(ListKey::Down),
        (None, _) => Some(ListKey::Cancel),
        _ => None,
    }
}

/// What a keystroke did to the list, so the caller knows whether to repaint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListOutcome {
    /// The cursor or a checkbox moved: repaint the rows.
    Redraw,
    /// Answer with the checked rows.
    Confirm,
    /// Stop without answering.
    Cancel,
    /// Nothing this picker uses: repaint nothing.
    Ignored,
}

/// The state of a list picker: where the cursor is and what is checked. Pure,
/// so the key map is tested without a terminal.
#[derive(Debug, Clone)]
pub struct ListState {
    /// The row the cursor is on.
    pub cursor: usize,
    /// Which rows are checked, one entry per row.
    pub checked: Vec<bool>,
}

impl ListState {
    /// Start from `default` (1-based row numbers), with the cursor on the first
    /// checked row, or on the first row.
    pub fn new(len: usize, default: &[usize]) -> Self {
        let mut checked = vec![false; len];
        for index in default {
            if let Some(slot) = index.checked_sub(1).and_then(|i| checked.get_mut(i)) {
                *slot = true;
            }
        }
        let cursor = checked.iter().position(|on| *on).unwrap_or(0);
        Self {
            cursor: cursor.min(len.saturating_sub(1)),
            checked,
        }
    }

    /// Apply one key. A row in `disabled` is never changed by any key: the
    /// picker cannot offer what this scope cannot use, so `Space`, `All` and
    /// `None` all leave it as it was.
    pub fn apply(&mut self, key: ListKey, disabled: &[usize]) -> ListOutcome {
        let togglable = |index: usize| !disabled.contains(&(index + 1));
        match key {
            ListKey::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                ListOutcome::Redraw
            }
            ListKey::Down => {
                self.cursor = (self.cursor + 1).min(self.checked.len().saturating_sub(1));
                ListOutcome::Redraw
            }
            ListKey::Space => {
                if let Some(slot) = self.checked.get_mut(self.cursor)
                    && togglable(self.cursor)
                {
                    *slot = !*slot;
                    return ListOutcome::Redraw;
                }
                ListOutcome::Ignored
            }
            ListKey::All | ListKey::None => {
                let on = key == ListKey::All;
                for (index, slot) in self.checked.iter_mut().enumerate() {
                    if togglable(index) {
                        *slot = on;
                    }
                }
                ListOutcome::Redraw
            }
            ListKey::Enter => ListOutcome::Confirm,
            ListKey::Cancel => ListOutcome::Cancel,
        }
    }

    /// The 1-based numbers of the checked rows, in row order.
    pub fn picked(&self) -> Vec<usize> {
        self.checked
            .iter()
            .enumerate()
            .filter(|(_, on)| **on)
            .map(|(index, _)| index + 1)
            .collect()
    }
}

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

    /// The next key in a list picker, on the same byte and escape plumbing as
    /// [`RawGuard::read_key`]: the vertical arrows, space and `a`/`n` a yes/no
    /// question has no use for.
    #[cfg_attr(test, mutants::skip)] // libc adapter; needs a real tty, the key map is tested pure
    pub fn read_list_key(&mut self) -> ListKey {
        loop {
            let Some(byte) = self.read_byte() else {
                return ListKey::Cancel;
            };
            if byte == 0x1b {
                let (second, third) = self.escape_sequence();
                if let Some(key) = decode_list_escape(second, third) {
                    return key;
                }
            } else if let Some(key) = decode_list_byte(byte) {
                return key;
            }
        }
    }

    /// The rest of an escape sequence: a second byte with a 100 ms timeout,
    /// then a third with the same, so a lone Esc press cancels instead of
    /// hanging the picker waiting for a sequence that never comes.
    #[cfg_attr(test, mutants::skip)] // libc adapter; needs a real tty
    fn escape_sequence(&mut self) -> (Option<u8>, Option<u8>) {
        self.set_decaying();
        let second = self.read_byte();
        let third = self.read_byte();
        self.set_blocking();
        (second, third)
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
        let (second, third) = self.escape_sequence();
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
    fn list_keys_map_the_byte_the_arrow_and_the_vim_shorthand() {
        assert_eq!(decode_list_byte(b' '), Some(ListKey::Space));
        assert_eq!(decode_list_byte(b'a'), Some(ListKey::All));
        assert_eq!(decode_list_byte(b'n'), Some(ListKey::None));
        assert_eq!(decode_list_byte(b'j'), Some(ListKey::Down));
        assert_eq!(decode_list_byte(b'k'), Some(ListKey::Up));
        assert_eq!(decode_list_byte(b'\r'), Some(ListKey::Enter));
        assert_eq!(decode_list_byte(b'q'), Some(ListKey::Cancel));
        assert_eq!(decode_list_byte(0x03), Some(ListKey::Cancel));
        assert_eq!(decode_list_byte(b'z'), None, "noise is ignored");
        assert_eq!(
            decode_list_escape(Some(b'['), Some(b'A')),
            Some(ListKey::Up)
        );
        assert_eq!(
            decode_list_escape(Some(b'['), Some(b'B')),
            Some(ListKey::Down)
        );
        assert_eq!(
            decode_list_escape(None, None),
            Some(ListKey::Cancel),
            "a lone Esc"
        );
        assert_eq!(
            decode_list_escape(Some(b'['), Some(b'D')),
            Some(ListKey::Up),
            "left moves like up"
        );
        assert_eq!(
            decode_list_escape(Some(b'['), Some(b'C')),
            Some(ListKey::Down),
            "right moves like down — the Apply/Cancel row's ←/→ pick"
        );
    }

    #[test]
    fn the_list_state_starts_on_the_first_default_row() {
        let state = ListState::new(4, &[2, 4]);
        assert_eq!(state.checked, vec![false, true, false, true]);
        assert_eq!(state.cursor, 1, "the cursor opens on a checked row");
        assert_eq!(state.picked(), vec![2, 4]);

        let empty = ListState::new(3, &[]);
        assert_eq!(empty.cursor, 0);
        assert_eq!(empty.picked(), Vec::<usize>::new());
        assert_eq!(
            ListState::new(0, &[1]).cursor,
            0,
            "a list with no rows cannot point past its end"
        );
    }

    #[test]
    fn the_list_state_moves_toggles_and_stops_at_both_ends() {
        let mut state = ListState::new(3, &[1]);
        assert_eq!(state.apply(ListKey::Down, &[]), ListOutcome::Redraw);
        assert_eq!(state.cursor, 1);
        assert_eq!(state.apply(ListKey::Down, &[]), ListOutcome::Redraw);
        assert_eq!(state.apply(ListKey::Down, &[]), ListOutcome::Redraw);
        assert_eq!(state.cursor, 2, "the cursor stops on the last row");
        assert_eq!(state.apply(ListKey::Up, &[]), ListOutcome::Redraw);
        assert_eq!(state.apply(ListKey::Up, &[]), ListOutcome::Redraw);
        assert_eq!(state.apply(ListKey::Up, &[]), ListOutcome::Redraw);
        assert_eq!(state.cursor, 0, "and on the first");

        assert_eq!(state.apply(ListKey::Space, &[]), ListOutcome::Redraw);
        assert_eq!(state.checked, vec![false, false, false], "row 1 was on");
        assert_eq!(state.picked(), Vec::<usize>::new());
        assert_eq!(state.apply(ListKey::Space, &[]), ListOutcome::Redraw);

        assert_eq!(state.apply(ListKey::Enter, &[]), ListOutcome::Confirm);
        assert_eq!(state.apply(ListKey::Cancel, &[]), ListOutcome::Cancel);
    }

    #[test]
    fn all_and_none_answer_every_togglable_row() {
        let mut state = ListState::new(4, &[]);
        assert_eq!(state.apply(ListKey::All, &[2]), ListOutcome::Redraw);
        assert_eq!(
            state.picked(),
            vec![1, 3, 4],
            "a row this scope cannot use is never checked by `all`"
        );
        assert_eq!(state.apply(ListKey::None, &[2]), ListOutcome::Redraw);
        assert_eq!(state.picked(), Vec::<usize>::new());
    }

    #[test]
    fn none_leaves_a_disabled_row_exactly_as_it_was() {
        // A disabled row can only be checked by the default it came in with,
        // and no key takes that away: the picker cannot offer what this scope
        // cannot use, so it does not quietly undo it either.
        let mut state = ListState::new(2, &[2]);
        assert_eq!(state.apply(ListKey::None, &[2]), ListOutcome::Redraw);
        assert_eq!(
            state.picked(),
            vec![2],
            "`none` cleared a row the picker cannot toggle"
        );
        assert_eq!(state.apply(ListKey::All, &[2]), ListOutcome::Redraw);
        assert_eq!(
            state.picked(),
            vec![1, 2],
            "`all` checks the rows it can and leaves the disabled one as it was"
        );
    }

    #[test]
    fn a_disabled_row_cannot_be_toggled_and_says_so_by_not_repainting() {
        let mut state = ListState::new(2, &[]);
        state.cursor = 1;
        assert_eq!(state.apply(ListKey::Space, &[2]), ListOutcome::Ignored);
        assert_eq!(state.picked(), Vec::<usize>::new());
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
