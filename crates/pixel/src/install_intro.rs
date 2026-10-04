// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Plays [`pixel_install::intro`] full screen before the install banner:
//! the gate that decides whether a person is there to watch, and the loop
//! that holds the terminal while they do. Any key skips it, Ctrl-C
//! included: signals are off while it plays, so the install still runs.
//! A terminating signal (SIGTERM/SIGHUP) is caught so the terminal is
//! handed back before the process exits.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use pixel_install::intro::{self, Canvas, Intro};

/// How long the loop waits for a key between two frames.
const FRAME: Duration = Duration::from_millis(25);

/// Whether this install plays the intro: stdout and stdin on a terminal
/// whose `TERM` is not dumb, no `--json`, and none of `PIXEL_NO_INTRO`,
/// `NO_COLOR` or `CI` set to a non-empty value.
pub(crate) fn should_play(
    json: bool,
    stdout_tty: bool,
    stdin_tty: bool,
    env: impl Fn(&str) -> Option<OsString>,
) -> bool {
    let set = |key: &str| env(key).is_some_and(|v| !v.is_empty());
    !json
        && stdout_tty
        && stdin_tty
        && !set("PIXEL_NO_INTRO")
        && !set("NO_COLOR")
        && !set("CI")
        && env("TERM").is_some_and(|term| !term.is_empty() && term != "dumb")
}

/// Whether `COLORTERM` advertises 24-bit colour; without it the intro
/// falls back to the xterm-256 cube.
pub(crate) fn truecolor(colorterm: Option<&OsStr>) -> bool {
    matches!(
        colorterm.and_then(OsStr::to_str),
        Some("truecolor" | "24bit")
    )
}

/// What the loop needs from a terminal.
pub(crate) trait Screen {
    /// Columns and rows; `None` when the terminal will not say.
    fn size(&mut self) -> Option<(u16, u16)>;
    fn write(&mut self, bytes: &str);
    /// Wait up to `timeout` for a key, consuming it; `true` when one came.
    fn key_within(&mut self, timeout: Duration) -> bool;
}

/// How a run ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The terminal was too small or would not give its size: nothing drawn.
    NotShown,
    /// A key cut it short.
    Skipped,
    /// The terminal shrank below the minimum mid-run.
    Shrunk,
    Finished,
}

/// Play the intro on `screen`, `clock` giving the seconds since the start,
/// until [`intro::END`] or the first key. A termination signal recorded in
/// `terminate` ends it early through this same path, so `LEAVE` is written
/// and the [`Tty`] drops. Every run that drew a frame leaves the alternate
/// screen before it returns.
pub(crate) fn run(
    screen: &mut dyn Screen,
    clock: &mut dyn FnMut() -> f32,
    truecolor: bool,
    terminate: &AtomicI32,
) -> Outcome {
    let Some(mut shown) = screen
        .size()
        .filter(|&(cols, rows)| intro::fits(cols, rows))
    else {
        return Outcome::NotShown;
    };
    screen.write(intro::ENTER);
    let mut intro = Intro::new();
    let mut prev: Option<Canvas> = None;
    let outcome = loop {
        // A termination signal pending from `play`'s handler ends the intro
        // as a skip, so the terminal is handed back before the process dies.
        if terminate.load(Ordering::Relaxed) != 0 {
            break Outcome::Skipped;
        }
        let t = clock();
        if t > intro::END {
            break Outcome::Finished;
        }
        let size = screen.size().unwrap_or(shown);
        if size != shown {
            // `diff` repaints every cell when the size changed
            screen.write(intro::CLEAR);
            shown = size;
        }
        let Some(frame) = intro.frame(size.0, size.1, t) else {
            break Outcome::Shrunk;
        };
        screen.write(&intro::diff(&frame, prev.as_ref(), truecolor));
        prev = Some(frame);
        if screen.key_within(FRAME) {
            break Outcome::Skipped;
        }
    };
    screen.write(intro::LEAVE);
    outcome
}

/// A terminating signal delivered while the intro played; `0` means none.
/// The intro loop watches it, and `play` re-raises it once the terminal is
/// handed back. Only ever written-from/read in signal-handler-safe spots.
static TERMINATE: AtomicI32 = AtomicI32::new(0);

/// The signals whose normal action is to end the process; catching them lets
/// the terminal be restored before they take effect.
const TERMINATE_SIGNALS: [libc::c_int; 2] = [libc::SIGTERM, libc::SIGHUP];

/// The pending termination signal, if any, clearing it.
fn take_terminate() -> Option<libc::c_int> {
    let sig = TERMINATE.swap(0, Ordering::SeqCst);
    (sig != 0).then_some(sig)
}

/// The handler `play` installs for [`TERMINATE_SIGNALS`]: records the signal
/// so the intro loop breaks and the terminal is handed back, then `play`
/// re-raises it. `SA_RESETHAND` restores the default on entry, so the
/// re-raised signal actually terminates.
#[cfg_attr(test, mutants::skip)] // libc adapter; its single store is tested via `run`
extern "C" fn on_terminate(sig: libc::c_int) {
    TERMINATE.store(sig, Ordering::SeqCst);
}

/// Catches [`TERMINATE_SIGNALS`] for as long as it is alive; dropping it
/// restores the actions it put in place.
struct TerminateGuard {
    prev: [libc::sigaction; 2],
}

impl TerminateGuard {
    #[cfg_attr(test, mutants::skip)] // libc adapter; the flag logic is tested pure
    fn install() -> Option<Self> {
        let zeroed = || {
            // SAFETY: zeroed memory is a valid (if meaningless) sigaction
            // buffer that sigaction or sigemptyset immediately overwrites.
            unsafe { std::mem::zeroed() }
        };
        let mut prev: [libc::sigaction; 2] = std::array::from_fn(|_| zeroed());
        let mut act: libc::sigaction = zeroed();
        // SAFETY: the handler's address, what `sa_sigaction` stores; the
        // signature matches the `sa_sigaction` shape the flags select.
        act.sa_sigaction = on_terminate as *const () as usize;
        act.sa_flags = libc::SA_RESETHAND;
        // SAFETY: a zeroed mask the call fills in; the mask is for the
        // blocked-during-handler set, and the handler only stores an int.
        unsafe { libc::sigemptyset(&mut act.sa_mask) };
        for (i, sig) in TERMINATE_SIGNALS.iter().enumerate() {
            // SAFETY: `act` is valid, and `prev[i]` a buffer sigaction fills.
            if unsafe { libc::sigaction(*sig, &act, &mut prev[i]) } != 0 {
                return None;
            }
        }
        Some(Self { prev })
    }
}

impl Drop for TerminateGuard {
    #[cfg_attr(test, mutants::skip)] // libc adapter
    fn drop(&mut self) {
        for (i, sig) in TERMINATE_SIGNALS.iter().enumerate() {
            // SAFETY: restoring the action `install` read for this signal.
            unsafe { libc::sigaction(*sig, &self.prev[i], std::ptr::null_mut()) };
        }
    }
}

/// Key input for the intro: no line buffering, no echo, and no signals, so
/// Ctrl-C reaches the loop as a key that skips instead of killing the
/// install. Reads never block: the loop polls.
fn keys_raw(mut termios: libc::termios) -> libc::termios {
    termios.c_lflag &= !libc::ICANON;
    termios.c_lflag &= !libc::ECHO;
    termios.c_lflag &= !libc::ISIG;
    termios.c_cc[libc::VMIN] = 0;
    termios.c_cc[libc::VTIME] = 0;
    termios
}

/// The real terminal: stdin in [`keys_raw`] mode, stdout for the frames.
/// Dropping it restores stdin's settings and, when a frame is still on the
/// alternate screen (a panic mid-run), leaves it.
struct Tty {
    saved: libc::termios,
    entered: bool,
}

impl Tty {
    #[cfg_attr(test, mutants::skip)] // libc adapter; the mode it sets is tested pure via `keys_raw`
    fn open() -> Option<Self> {
        // SAFETY: zeroed memory is a valid (if meaningless) termios buffer
        // that the tcgetattr call immediately overwrites.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: a single tcgetattr on stdin with a valid, zeroed buffer.
        if unsafe { libc::tcgetattr(0, &mut saved) } != 0 {
            return None;
        }
        let raw = keys_raw(saved);
        // SAFETY: TCSANOW applies the descriptor's own modified settings.
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        Some(Self {
            saved,
            entered: false,
        })
    }
}

impl Screen for Tty {
    #[cfg_attr(test, mutants::skip)] // libc adapter; a size needs a real tty to observe
    fn size(&mut self) -> Option<(u16, u16)> {
        // SAFETY: zeroed memory is a valid winsize that ioctl overwrites.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: TIOCGWINSZ writes one winsize into the valid buffer.
        let ok = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0;
        (ok && ws.ws_col > 0 && ws.ws_row > 0).then_some((ws.ws_col, ws.ws_row))
    }

    #[cfg_attr(test, mutants::skip)] // stdout adapter; the bytes are tested through `run`
    fn write(&mut self, bytes: &str) {
        if bytes == intro::ENTER {
            self.entered = true;
        } else if bytes == intro::LEAVE {
            self.entered = false;
        }
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(bytes.as_bytes());
        let _ = out.flush();
    }

    #[cfg_attr(test, mutants::skip)] // libc adapter; a key press needs a real tty to observe
    fn key_within(&mut self, timeout: Duration) -> bool {
        let mut fds = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
        // SAFETY: one valid pollfd, count 1.
        if unsafe { libc::poll(&mut fds, 1, ms) } <= 0 {
            return false;
        }
        let mut buf = [0u8; 64];
        // SAFETY: reads at most `buf.len()` bytes into the valid buffer.
        unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) > 0 }
    }
}

impl Drop for Tty {
    #[cfg_attr(test, mutants::skip)] // libc adapter; the restore needs a real tty to observe
    fn drop(&mut self) {
        if self.entered {
            self.write(intro::LEAVE);
        }
        // SAFETY: TCSANOW puts back the settings `open` read from stdin.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.saved) };
    }
}

/// Play the intro on this process's terminal; the caller checked
/// [`should_play`]. A terminal that cannot be put in key mode is left alone.
#[cfg_attr(test, mutants::skip)] // the real-tty adapter over `run`, which is tested with a fake screen
pub(crate) fn play() {
    let Some(mut tty) = Tty::open() else {
        return;
    };
    // Catch SIGTERM/SIGHUP so a terminating signal hands the terminal back
    // before the process exits, instead of leaving the alternate screen up
    // or stdin in raw mode.
    let guard = TerminateGuard::install();
    let start = Instant::now();
    let mut clock = || start.elapsed().as_secs_f32();
    run(
        &mut tty,
        &mut clock,
        truecolor(std::env::var_os("COLORTERM").as_deref()),
        &TERMINATE,
    );
    // `run` wrote `LEAVE` on every break; dropping the Tty now restores the
    // raw-mode stdin. Only then re-raise a caught signal, with the handler
    // and disposition the guard put back. No signal means a plain return.
    drop(tty);
    if let Some(sig) = take_terminate() {
        drop(guard);
        // SAFETY: `raise` delivers to our own process; the handler for this
        // signal is restored, so it terminates as it would have.
        unsafe { libc::raise(sig) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A terminal that records what it was sent; `keys` lists, per frame,
    /// whether a key arrives after it; `sizes` is consumed one per query,
    /// the last one repeating.
    struct Fake {
        sizes: Vec<Option<(u16, u16)>>,
        keys: Vec<bool>,
        written: Vec<String>,
    }

    impl Fake {
        fn new(sizes: &[Option<(u16, u16)>], keys: &[bool]) -> Self {
            Self {
                sizes: sizes.to_vec(),
                keys: keys.to_vec(),
                written: Vec::new(),
            }
        }

        fn frames(&self) -> usize {
            self.written
                .iter()
                .filter(|w| w.starts_with("\x1b[?2026h"))
                .count()
        }
    }

    impl Screen for Fake {
        fn size(&mut self) -> Option<(u16, u16)> {
            if self.sizes.len() > 1 {
                self.sizes.remove(0)
            } else {
                self.sizes[0]
            }
        }

        fn write(&mut self, bytes: &str) {
            assert!(self.written.len() < 500, "the loop did not stop");
            self.written.push(bytes.to_string());
        }

        fn key_within(&mut self, timeout: Duration) -> bool {
            assert_eq!(timeout, FRAME);
            !self.keys.is_empty() && self.keys.remove(0)
        }
    }

    /// A clock reading `times` in turn, then far past the end.
    fn clock(times: &[f32]) -> impl FnMut() -> f32 {
        let mut times = Vec::from(times).into_iter();
        move || times.next().unwrap_or(f32::MAX)
    }

    const BIG: Option<(u16, u16)> = Some((100, 28));

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: Vec<(String, OsString)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), OsString::from(v)))
            .collect();
        move |key| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    #[test]
    fn a_person_at_a_terminal_sees_it() {
        let term = env(&[("TERM", "xterm-256color")]);
        assert!(should_play(false, true, true, &term));
        assert!(!should_play(true, true, true, &term), "--json");
        assert!(!should_play(false, false, true, &term), "piped stdout");
        assert!(!should_play(false, true, false, &term), "piped stdin");
    }

    #[test]
    fn an_opt_out_or_a_dumb_terminal_skips_it() {
        for key in ["PIXEL_NO_INTRO", "NO_COLOR", "CI"] {
            assert!(
                !should_play(false, true, true, env(&[("TERM", "xterm"), (key, "1")])),
                "{key}"
            );
            // set but empty counts as unset, as NO_COLOR specifies
            assert!(
                should_play(false, true, true, env(&[("TERM", "xterm"), (key, "")])),
                "{key}"
            );
        }
        assert!(!should_play(false, true, true, env(&[("TERM", "dumb")])));
        assert!(!should_play(false, true, true, env(&[("TERM", "")])));
        assert!(!should_play(false, true, true, env(&[])));
    }

    #[test]
    fn truecolor_follows_colorterm() {
        assert!(truecolor(Some(OsStr::new("truecolor"))));
        assert!(truecolor(Some(OsStr::new("24bit"))));
        assert!(!truecolor(Some(OsStr::new("yes"))));
        assert!(!truecolor(None));
    }

    #[test]
    fn keys_raw_turns_off_buffering_echo_and_signals_only() {
        // SAFETY: an all-zero termios is a valid value to start from.
        let mut base: libc::termios = unsafe { std::mem::zeroed() };
        base.c_lflag = libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN;
        base.c_cc[libc::VMIN] = 1;
        base.c_cc[libc::VTIME] = 3;
        let raw = keys_raw(base);
        assert_eq!(raw.c_lflag, libc::IEXTEN);
        assert_eq!((raw.c_cc[libc::VMIN], raw.c_cc[libc::VTIME]), (0, 0));
    }

    #[test]
    fn a_small_terminal_draws_nothing() {
        let mut fake = Fake::new(&[Some((63, 40))], &[]);
        assert_eq!(
            run(&mut fake, &mut clock(&[0.0]), true, &AtomicI32::new(0)),
            Outcome::NotShown
        );
        assert!(fake.written.is_empty());
        let mut unknown = Fake::new(&[None], &[]);
        assert_eq!(
            run(&mut unknown, &mut clock(&[0.0]), true, &AtomicI32::new(0)),
            Outcome::NotShown
        );
        assert!(unknown.written.is_empty());
    }

    #[test]
    fn it_plays_to_the_end_and_gives_the_screen_back() {
        let mut fake = Fake::new(&[BIG], &[]);
        let outcome = run(
            &mut fake,
            &mut clock(&[0.0, 1.0, intro::END]),
            true,
            &AtomicI32::new(0),
        );
        assert_eq!(outcome, Outcome::Finished);
        assert_eq!(fake.written.first().map(String::as_str), Some(intro::ENTER));
        assert_eq!(fake.written.last().map(String::as_str), Some(intro::LEAVE));
        // three frames: at the very end it still draws the last one
        assert_eq!(fake.frames(), 3);
        // only the first frame paints from the top-left corner
        assert!(fake.written[1].starts_with("\x1b[?2026h\x1b[1;1H"));
        assert!(
            !fake.written[2].starts_with("\x1b[?2026h\x1b[1;1H"),
            "a later frame sends only what changed"
        );
        assert_eq!(fake.written.len(), 5);
    }

    #[test]
    fn a_pending_terminate_signal_skips_and_hands_the_screen_back() {
        let terminate = AtomicI32::new(libc::SIGTERM);
        let mut fake = Fake::new(&[BIG], &[]);
        let outcome = run(&mut fake, &mut clock(&[0.0]), true, &terminate);
        assert_eq!(outcome, Outcome::Skipped);
        assert_eq!(fake.written.first().map(String::as_str), Some(intro::ENTER));
        assert_eq!(fake.written.last().map(String::as_str), Some(intro::LEAVE));
    }

    #[test]
    fn take_terminate_returns_and_clears_the_signal() {
        assert_eq!(take_terminate(), None);
        TERMINATE.store(libc::SIGHUP, Ordering::SeqCst);
        assert_eq!(take_terminate(), Some(libc::SIGHUP));
        assert_eq!(take_terminate(), None);
    }

    #[test]
    fn a_key_skips_the_rest() {
        let mut fake = Fake::new(&[BIG], &[false, true]);
        let outcome = run(
            &mut fake,
            &mut clock(&[0.0, 1.0, 2.0, 3.0]),
            true,
            &AtomicI32::new(0),
        );
        assert_eq!(outcome, Outcome::Skipped);
        assert_eq!(fake.frames(), 2);
        assert_eq!(fake.written.last().map(String::as_str), Some(intro::LEAVE));
    }

    #[test]
    fn a_resize_repaints_from_scratch() {
        let mut fake = Fake::new(&[BIG, BIG, Some((120, 30))], &[]);
        let outcome = run(
            &mut fake,
            &mut clock(&[1.0, 1.0]),
            false,
            &AtomicI32::new(0),
        );
        assert_eq!(outcome, Outcome::Finished);
        let clear = fake.written.iter().position(|w| w == intro::CLEAR);
        assert_eq!(clear, Some(2), "{:?}", fake.written);
        // the frame after the wipe is a full paint, drawn at the new size
        let mut fresh = Intro::new();
        let full = intro::diff(&fresh.frame(120, 30, 1.0).expect("fits"), None, false);
        assert_eq!(fake.written[3], full);
    }

    #[test]
    fn a_terminal_that_shrinks_mid_run_is_released() {
        let mut fake = Fake::new(&[BIG, BIG, Some((40, 10))], &[]);
        let outcome = run(
            &mut fake,
            &mut clock(&[0.0, 1.0, 2.0]),
            true,
            &AtomicI32::new(0),
        );
        assert_eq!(outcome, Outcome::Shrunk);
        assert_eq!(fake.frames(), 1);
        assert_eq!(fake.written.last().map(String::as_str), Some(intro::LEAVE));
    }

    #[test]
    fn a_lost_size_keeps_the_last_one() {
        let mut fake = Fake::new(&[BIG, BIG, None], &[]);
        let outcome = run(&mut fake, &mut clock(&[0.0, 1.0]), true, &AtomicI32::new(0));
        assert_eq!(outcome, Outcome::Finished);
        assert_eq!(fake.frames(), 2);
        assert!(!fake.written.iter().any(|w| w == intro::CLEAR));
    }
}
