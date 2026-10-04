//! The TTY option picker: arrow keys move a `❯` marker over a rendered
//! option list, Enter chooses, a digit picks immediately. Only the redraw
//! region is rewritten (`CSI n A` + `CSI J`), so the transcript keeps one
//! clean block instead of a reprint per keypress. `j`/`k` move for vi
//! hands; any other byte is ignored.
//!
//! The interactive half is two pieces: [`Keys`], the byte-to-action state
//! machine (pure, unit-tested), and [`pick`], the terminal adapter that
//! holds raw mode on fd 0 for the duration of the choice and drives the
//! redraw. Non-TTY callers never reach this module — they keep the
//! numbered `Choice>` prompt.

use std::io::{BufRead, Write};

/// What one input byte means to the picker.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Key {
    Up,
    Down,
    Choose,
    Digit(usize),
    Ignore,
    /// The first byte of a CSI escape (`ESC [`); the next byte resolves.
    EscapeStart,
    /// A byte arrived after `ESC [` that is not `A` or `B`.
    EscapeOther,
}

/// Byte-at-a-time decoder: arrows are the three-byte sequence `ESC [ A/B`,
/// digits and Enter are single bytes. State lives in `escape`, so a partial
/// sequence never consumes the following keystroke.
struct Keys {
    /// 0 = idle, 1 = saw ESC, 2 = saw `ESC [` (a CSI sequence).
    escape: u8,
}

impl Keys {
    fn new() -> Self {
        Keys { escape: 0 }
    }

    fn feed(&mut self, byte: u8) -> Key {
        match self.escape {
            1 => {
                self.escape = 0;
                if byte == b'[' {
                    self.escape = 2;
                    return Key::EscapeStart;
                }
                return Key::EscapeOther;
            }
            2 => {
                self.escape = 0;
                return match byte {
                    b'A' => Key::Up,
                    b'B' => Key::Down,
                    _ => Key::EscapeOther,
                };
            }
            _ => {}
        }
        match byte {
            0x1b => {
                self.escape = 1;
                Key::EscapeStart
            }
            // A bare `[` is ordinary input — `ESC [` is handled above.
            b'\r' | b'\n' => Key::Choose,
            b'j' => Key::Down,
            b'k' => Key::Up,
            b'0'..=b'9' => Key::Digit((byte - b'0') as usize),
            _ => Key::Ignore,
        }
    }
}

/// Terminal width in columns, probing stdout then stderr then stdin.
/// A wrapped row breaks the repaint math (`CSI n A` counts logical
/// lines), so rows get truncated to fit instead.
#[cfg_attr(test, mutants::skip)] // real tty; tests pass a width explicitly
pub(crate) fn terminal_width() -> usize {
    // SAFETY: `ws` is fully initialized by a successful ioctl before the
    // ws_col read; the fds are the standard descriptors.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        for fd in [1, 2, 0] {
            if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
                return ws.ws_col as usize;
            }
        }
    }
    80
}

/// Visible width of a string containing ANSI sequences: `ESC [ … letter`
/// runs count as zero columns.
pub(crate) fn visible_len(text: &str) -> usize {
    let mut len = 0;
    let mut escape = false;
    for ch in text.chars() {
        if escape {
            if ch.is_ascii_alphabetic() {
                escape = false;
            }
        } else if ch == '\x1b' {
            escape = true;
        } else {
            len += 1;
        }
    }
    len
}

/// Clip `label` so the rendered row stays within `cols` — a row that wraps
/// occupies extra visual lines the repaint move does not account for.
fn clip(label: &str, cols: usize) -> String {
    // "  ❯ " prefix is 4 columns; reserve one more so the last column
    // never carries the final character (some terminals wrap on it).
    let budget = cols.saturating_sub(5);
    let mut taken: String = label.chars().take(budget).collect();
    if taken.len() < label.len() {
        taken.pop();
        taken.push('…');
    }
    taken
}

/// One rendered option row: `❯` marks the cursor, unchosen rows are dimmed.
fn row(index: usize, current: usize, label: &str, cols: usize) -> String {
    let label = clip(label, cols);
    if index == current {
        format!("  \x1b[32m❯\x1b[0m {label}")
    } else {
        format!("    \x1b[2m{label}\x1b[0m")
    }
}

/// Redraw the whole block: jump to its first line, clear to end of screen,
/// reprint each row. Each row ends with a newline, so after printing the
/// cursor sits one line BELOW the block — the move up covers all `count`
/// lines, not `count - 1`. Rows are clipped to the terminal width so a
/// repaint never has to account for wrapping.
fn repaint(
    stdout: &mut dyn Write,
    options: &[&str],
    current: usize,
    cols: usize,
) -> Result<(), String> {
    write!(stdout, "\x1b[{}A\x1b[0J", options.len()).map_err(|e| e.to_string())?;
    for (index, label) in options.iter().enumerate() {
        writeln!(stdout, "{}", row(index, current, label, cols)).map_err(|e| e.to_string())?;
    }
    stdout.flush().map_err(|e| e.to_string())
}

/// Arrow-key pick over `options` on a raw terminal. Returns the chosen
/// index, or `None` on EOF (an agent piping stdin still sees the block once;
/// the caller falls back to the numbered prompt or a default).
///
/// `raw` toggles fd-0 raw mode for the pick; production is [`TermiosRaw`],
/// tests drive the pure parts directly.
pub fn pick(
    options: &[&str],
    keys: &mut dyn BufRead,
    stdout: &mut dyn Write,
    raw: &mut dyn RawMode,
) -> Result<Option<usize>, String> {
    if options.is_empty() {
        return Ok(None);
    }
    raw.enter()?;
    let _restore = RawRestore(raw);
    let cols = terminal_width();
    write!(stdout, "\x1b[?25l").map_err(|e| e.to_string())?; // hide cursor
    let mut current = 0usize;
    for (index, label) in options.iter().enumerate() {
        writeln!(stdout, "{}", row(index, current, label, cols)).map_err(|e| e.to_string())?;
    }
    stdout.flush().map_err(|e| e.to_string())?;
    let mut decoder = Keys::new();
    let mut byte = [0u8; 1];
    let chosen = loop {
        match keys.read(&mut byte) {
            Ok(0) | Err(_) => break None,
            Ok(_) => match decoder.feed(byte[0]) {
                Key::Up => {
                    current = current.saturating_sub(1);
                    repaint(stdout, options, current, cols)?;
                }
                Key::Down => {
                    current = (current + 1).min(options.len() - 1);
                    repaint(stdout, options, current, cols)?;
                }
                Key::Digit(digit) if digit >= 1 && digit <= options.len() => {
                    // Swallow the rest of the digit's line so a scripted
                    // answer does not leak its newline into the next prompt.
                    let _ = keys.read_until(b'\n', &mut Vec::new());
                    break Some(digit - 1);
                }
                Key::Choose => break Some(current),
                _ => {}
            },
        }
    };
    write!(stdout, "\x1b[?25h").map_err(|e| e.to_string())?; // show cursor
    Ok(chosen)
}

/// Leaves the injected [`RawMode`] on drop, whatever exit `pick` takes.
struct RawRestore<'a>(&'a mut dyn RawMode);

impl Drop for RawRestore<'_> {
    fn drop(&mut self) {
        self.0.leave();
    }
}

/// fd-0 raw mode for the duration of a pick: canonical mode and echo off,
/// `leave` restores. `enter` is the only fallible step; `leave` failures are
/// ignored (a reset terminal still echoes).
pub trait RawMode {
    fn enter(&mut self) -> Result<(), String>;
    fn leave(&mut self);
}

/// The production [`RawMode`]: `tcgetattr`/`tcsetattr` on stdin.
#[derive(Default)]
pub struct TermiosRaw {
    saved: Option<libc::termios>,
}

#[cfg_attr(test, mutants::skip)] // libc adapter; needs a real tty
impl RawMode for TermiosRaw {
    fn enter(&mut self) -> Result<(), String> {
        // SAFETY: `saved` is fully overwritten by `tcgetattr` before it is
        // read or stored; `tcsetattr` borrows the raw termios only for the
        // call. fd 0 is the conventional stdin descriptor.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return Err("tcgetattr failed".into());
            }
            let mut raw = saved;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                return Err("tcsetattr failed".into());
            }
            self.saved = Some(saved);
            Ok(())
        }
    }

    fn leave(&mut self) {
        if let Some(saved) = self.saved.take() {
            // SAFETY: `saved` is a live termios captured by `enter`; restoring
            // it borrows only for the call.
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &saved);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a byte string through the decoder, collecting non-Ignore keys.
    fn keys(input: &[u8]) -> Vec<Key> {
        let mut decoder = Keys::new();
        input
            .iter()
            .map(|&b| decoder.feed(b))
            .filter(|k| *k != Key::Ignore)
            .collect()
    }

    #[test]
    fn arrows_decode_as_up_and_down() {
        let esc = Key::EscapeStart;
        assert_eq!(keys(b"\x1b[A"), vec![esc, esc, Key::Up]);
        assert_eq!(keys(b"\x1b[B"), vec![esc, esc, Key::Down]);
        // Up and Down are not inverted.
        assert_ne!(keys(b"\x1b[A"), keys(b"\x1b[B"));
    }

    #[test]
    fn a_partial_escape_does_not_eat_the_next_keystroke() {
        // ESC then a non-[ byte resolves as EscapeOther; the following
        // digit still decodes.
        assert_eq!(
            keys(b"\x1bx2"),
            vec![Key::EscapeStart, Key::EscapeOther, Key::Digit(2)]
        );
    }

    #[test]
    fn digits_and_enter_decode() {
        assert_eq!(keys(b"3\r"), vec![Key::Digit(3), Key::Choose]);
    }

    /// A no-op raw-mode seam for driving `pick` in tests.
    struct NoopRaw;

    impl RawMode for NoopRaw {
        fn enter(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn leave(&mut self) {}
    }

    /// `pick` over canned keys, capturing the painted output.
    fn picked(options: &[&str], input: &[u8]) -> (Option<usize>, String) {
        let mut keys = std::io::Cursor::new(input.to_vec());
        let mut out = Vec::new();
        let result = pick(options, &mut keys, &mut out, &mut NoopRaw).unwrap();
        (result, String::from_utf8_lossy(&out).into_owned())
    }

    #[test]
    fn j_and_k_move_like_the_arrows_and_a_bare_bracket_is_ignored() {
        assert_eq!(keys(b"j"), vec![Key::Down]);
        assert_eq!(keys(b"k"), vec![Key::Up]);
        // `[` alone is ordinary input — only `ESC [` starts a sequence.
        assert_eq!(keys(b"[A"), Vec::<Key>::new());
    }

    #[test]
    fn down_moves_to_the_next_row_and_clamps_at_the_last() {
        let options = ["a", "b", "c"];
        let (one, _) = picked(&options, b"\x1b[B\r");
        assert_eq!(one, Some(1));
        // Down past the end clamps instead of wrapping or overrunning.
        let (three, _) = picked(&options, b"\x1b[B\x1b[B\x1b[B\r");
        assert_eq!(three, Some(2));
        // Up from the top stays on the first row.
        let (top, _) = picked(&options, b"\x1b[A\x1b[B\r");
        assert_eq!(top, Some(1));
    }

    #[test]
    fn digits_pick_their_row_and_out_of_range_digits_do_not() {
        let options = ["a", "b", "c"];
        assert_eq!(picked(&options, b"2\n").0, Some(1));
        assert_eq!(picked(&options, b"3\n").0, Some(2));
        // A digit beyond the list is ignored; Enter then takes row 0.
        assert_eq!(picked(&options, b"9\r").0, Some(0));
    }

    #[test]
    fn repaint_moves_up_the_full_block_and_reprints_every_row() {
        let mut out = Vec::new();
        repaint(&mut out, &["a", "b", "c"], 1, 80).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("\x1b[3A\x1b[0J"), "{text:?}");
        for label in ["a", "b", "c"] {
            assert!(text.contains(label), "{text:?}");
        }
    }

    #[test]
    fn raw_mode_leaves_when_the_pick_returns() {
        struct Tracking(std::rc::Rc<std::cell::Cell<bool>>);
        impl RawMode for Tracking {
            fn enter(&mut self) -> Result<(), String> {
                Ok(())
            }
            fn leave(&mut self) {
                self.0.set(true);
            }
        }
        let left = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let mut keys = std::io::Cursor::new(b"\r".to_vec());
            let mut out = Vec::new();
            let result = pick(&["a"], &mut keys, &mut out, &mut Tracking(left.clone()));
            assert_eq!(result.unwrap(), Some(0));
            // `_restore` drops inside `pick`, so leave already ran.
            assert!(left.get(), "raw mode restored on success");
        }
    }

    #[test]
    fn visible_len_counts_columns_not_escape_bytes() {
        assert_eq!(visible_len("ab"), 2);
        assert_eq!(visible_len("\x1b[32m❯\x1b[0m x"), 3);
        // A non-letter byte does not terminate an escape run.
        assert_eq!(visible_len("\x1b7x"), 0);
        assert_eq!(visible_len("plain"), 5);
    }

    #[test]
    fn row_marks_the_cursor_and_dims_the_rest() {
        let marked = row(1, 1, "two", 80);
        assert!(marked.contains('❯'), "{marked}");
        let dim = row(0, 1, "one", 80);
        assert!(dim.contains("\x1b[2m"), "{dim}");
        assert!(!dim.contains('❯'), "{dim}");
    }

    #[test]
    fn a_row_that_would_wrap_is_clipped_instead() {
        // At 24 columns the label budget is 19: a longer label ellipsizes
        // so the row stays on one visual line.
        let narrow = row(0, 0, "a very long option label indeed", 24);
        let plain = narrow.replace("\x1b[32m", "").replace("\x1b[0m", "");
        assert!(plain.chars().count() <= 24, "{plain:?}");
        assert!(plain.ends_with('…'), "{plain}");

        // A label inside the budget is untouched.
        let wide = row(0, 0, "short", 80);
        assert!(wide.contains("short"), "{wide}");
    }
}
