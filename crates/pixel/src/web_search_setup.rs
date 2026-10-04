//! The web-search provider install step and configuration.
//!
//! `pixel install` (through the interactive global setup) and `pixel config
//! setup` propose a web-search provider: your own SearXNG instance (queries
//! stay off public services), a keyed Perplexity account, or a skip that
//! keeps the free public chain as the fallback — the same picker shape as
//! the classify-engine step. Unlike classify, the question is asked even
//! when a provider is already stored: setup is the only change surface for
//! web search (there is no `config web-search` subcommand), so a re-run
//! must stay able to switch providers.
//!
//! Everything interactive is gated on a TTY: a scripted install (CI, pipes)
//! prints the choices as a suggestion and moves on. The SearXNG URL is
//! stored in the global config under `web_search.searxng_url`; the
//! Perplexity key under `remote_keys.perplexity`, the same secret store
//! `pixel config remote-key` uses — 0600, never echoed back.

use std::io::{BufRead, Write};

/// SearXNG — your own instance, queries stay private.
pub const SEARXNG_LABEL: &str = "SearXNG — your own instance (queries stay private; recommended)";
/// Perplexity — a keyed `/search` account.
pub const PERPLEXITY_LABEL: &str = "Perplexity — API key (PERPLEXITY_API_KEY; keyed results)";
/// Skip — the free public chain keeps working as the fallback.
pub const SKIP_LABEL: &str = "skip — the free public chain stays the fallback";

/// Parse the interactive answer ("1"/"2") into a provider choice.
fn parse_choice(answer: &str) -> Option<&'static str> {
    match answer.trim() {
        "1" => Some("searxng"),
        "2" => Some("perplexity"),
        _ => None,
    }
}

/// The install-time step: propose the provider, then dispatch to the chosen
/// setup. When stdin is not a TTY the proposal is printed as a suggestion
/// and nothing interactive happens.
#[cfg_attr(test, mutants::skip)] // Runtime config adapter; interactive policy is tested by `install_step_with`
pub fn install_step(
    tty: bool,
    stdin: &mut dyn BufRead,
    stdout: &mut dyn Write,
) -> Result<(), String> {
    let mut terminal = TermiosEcho;
    install_step_with(
        tty,
        stdin,
        stdout,
        crate::config_cmd::set_web_search_searxng_url,
        crate::config_cmd::set_web_search_perplexity_key,
        crate::config_cmd::remove_web_search_searxng_url,
        &mut terminal,
    )
}

fn install_step_with(
    tty: bool,
    stdin: &mut dyn BufRead,
    stdout: &mut dyn Write,
    store_searxng: impl FnOnce(&str) -> Result<(), String>,
    store_key: impl FnOnce(&str) -> Result<(), String>,
    remove_searxng: impl FnOnce() -> Result<(), String>,
    terminal: &mut dyn EchoFlag,
) -> Result<(), String> {
    writeln!(stdout, "Web search provider:").map_err(|e| e.to_string())?;
    writeln!(stdout, "  [1] {SEARXNG_LABEL}").map_err(|e| e.to_string())?;
    writeln!(stdout, "  [2] {PERPLEXITY_LABEL}").map_err(|e| e.to_string())?;
    writeln!(stdout, "  [3] {SKIP_LABEL}").map_err(|e| e.to_string())?;
    if !tty {
        writeln!(stdout, "web search provider: not configured (non-interactive install) — run `pixel config setup` in a terminal to choose")
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
    write!(stdout, "Choice> ").map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    stdin
        .read_line(&mut line)
        .map_err(|e| format!("read choice: {e}"))?;
    match parse_choice(&line) {
        Some("searxng") => ask_searxng_url(stdin, stdout, store_searxng),
        Some("perplexity") => {
            let stored = ask_perplexity_key(stdin, stdout, store_key, terminal)?;
            if stored {
                // Picker-specific only: switching to Perplexity must
                // deactivate a stored SearXNG, or `pixel web-search` and
                // `pixel config` would keep resolving SearXNG above it. The
                // field is deleted rather than emptied (`validate` rejects
                // an empty `searxng_url`), and only after a non-empty key
                // was actually stored — an empty key or a failed read
                // leaves the working SearXNG provider in place.
                remove_searxng()?;
            }
            Ok(())
        }
        _ => {
            writeln!(
                stdout,
                "web search provider: skipped — the free public chain stays the fallback"
            )
            .map_err(|e| e.to_string())?;
            Ok(())
        }
    }
}

/// The SearXNG base-URL prompt: the smallest non-empty answer is stored.
fn ask_searxng_url(
    stdin: &mut dyn BufRead,
    stdout: &mut dyn Write,
    store: impl FnOnce(&str) -> Result<(), String>,
) -> Result<(), String> {
    write!(
        stdout,
        "SearXNG base URL (e.g. https://searxng.example.com; stored in the global config, never the repo)> "
    )
    .map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())?;
    let mut url = String::new();
    stdin
        .read_line(&mut url)
        .map_err(|e| format!("read URL: {e}"))?;
    let url = url.trim();
    if url.is_empty() {
        writeln!(
            stdout,
            "web search provider: skipped (no URL) — the free public chain stays the fallback"
        )
        .map_err(|e| e.to_string())?;
        return Ok(());
    }
    store(url)?;
    writeln!(stdout, "web search provider: searxng — URL stored").map_err(|e| e.to_string())
}

/// The terminal's echo flag behind a seam: production is a libc termios
/// adapter on fd 0, tests substitute an in-memory fake so the "the key is
/// typed blind, then echo is restored" contract is observable without a tty.
trait EchoFlag {
    /// Whether echo is currently enabled.
    // Only the tests observe the echo state back (the key-typed-blind
    // contract); production calls just mute/restore.
    #[allow(dead_code)]
    fn echoed(&self) -> bool;
    /// Clear echo, returning the previous state, or `None` when the
    /// terminal cannot be muted (a pipe, a CI run, a failed termios call).
    fn mute(&mut self) -> Option<bool>;
    /// Restore echo to a previously captured state.
    fn restore(&mut self, prior: bool);
}

/// The pure part of muting: clip the `ECHO` flag. Canonical mode (the line
/// discipline `read_line` relies on) is kept — only `ECHO` is cleared, so
/// the key is typed blind but still line-buffered. Extracted so a mutation
/// that leaves echo enabled is caught by a unit test instead of hiding
/// behind a real-tty adapter skip.
fn with_echo_clipped(termios: libc::termios) -> libc::termios {
    let mut muted = termios;
    muted.c_lflag &= !libc::ECHO;
    muted
}

/// Production echo control: libc termios on stdin (fd 0). Muting changes
/// only the `ECHO` flag; restoring flips that one flag back.
struct TermiosEcho;

impl EchoFlag for TermiosEcho {
    #[cfg_attr(test, mutants::skip)] // libc adapter; the flag logic is tested pure via `with_echo_clipped`
    fn echoed(&self) -> bool {
        // SAFETY: zeroed memory is a valid (if meaningless) termios buffer
        // that the tcgetattr call immediately overwrites.
        // SAFETY: `zeroed` is only valid here because every field is
        // overwritten before the value escapes (c_lflag below, then the
        // assert reads only c_lflag).
        let mut termios: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: a single tcgetattr on stdin with a valid, zeroed buffer.
        if unsafe { libc::tcgetattr(0, &mut termios) } != 0 {
            return true; // unknown state → assume echo stays on
        }
        termios.c_lflag & libc::ECHO != 0
    }

    #[cfg_attr(test, mutants::skip)] // libc adapter; the flag logic is tested pure via `with_echo_clipped`
    fn mute(&mut self) -> Option<bool> {
        // SAFETY: zeroed memory is a valid (if meaningless) termios buffer
        // that the tcgetattr call immediately overwrites.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: a single tcgetattr on stdin with a valid, zeroed buffer.
        if unsafe { libc::tcgetattr(0, &mut original) } != 0 {
            return None;
        }
        let muted = with_echo_clipped(original);
        // SAFETY: TCSANOW applies the descriptor's own modified settings.
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &muted) } != 0 {
            return None;
        }
        Some(original.c_lflag & libc::ECHO != 0)
    }

    #[cfg_attr(test, mutants::skip)] // libc adapter; the restore needs a real tty to observe
    fn restore(&mut self, prior: bool) {
        // SAFETY: zeroed memory is a valid (if meaningless) termios buffer
        // that the tcgetattr call immediately overwrites.
        // SAFETY: `zeroed` is only valid here because every field is
        // overwritten before the value escapes (c_lflag below, then the
        // assert reads only c_lflag).
        let mut termios: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: a single tcgetattr on stdin with a valid, zeroed buffer.
        if unsafe { libc::tcgetattr(0, &mut termios) } != 0 {
            return;
        }
        if prior {
            termios.c_lflag |= libc::ECHO;
        } else {
            termios.c_lflag &= !libc::ECHO;
        }
        // SAFETY: TCSANOW applies the descriptor's own modified settings.
        let result = unsafe { libc::tcsetattr(0, libc::TCSANOW, &termios) };
        if result != 0 {
            if prior {
                let _ = writeln!(
                    std::io::stderr(),
                    "warning: terminal echo may remain disabled; run `stty echo` if input remains hidden"
                );
            }
        }
    }
}

/// Mute echo for the duration of a scope, restoring it on drop — so a panic
/// or an early return never leaves the terminal typing blind.
struct MuteGuard<'a> {
    terminal: &'a mut dyn EchoFlag,
    prior: bool,
}

impl<'a> MuteGuard<'a> {
    /// Clear echo, or fail closed: a terminal that cannot be muted must not
    /// read a secret with echo on (CWE-549).
    fn new(terminal: &'a mut dyn EchoFlag) -> Result<Self, String> {
        let prior = terminal
            .mute()
            .ok_or_else(|| "could not disable terminal echo for key input".to_string())?;
        Ok(Self { terminal, prior })
    }
}

impl Drop for MuteGuard<'_> {
    fn drop(&mut self) {
        self.terminal.restore(self.prior);
    }
}

/// The Perplexity key prompt: an empty answer stores nothing, exactly like
/// `pixel config remote-key`'s empty-key rule. The echo is muted while the
/// key is typed, so it never lands in terminal scrollback or a recording.
/// Returns whether a non-empty key was accepted and stored.
fn ask_perplexity_key(
    stdin: &mut dyn BufRead,
    stdout: &mut dyn Write,
    store: impl FnOnce(&str) -> Result<(), String>,
    terminal: &mut dyn EchoFlag,
) -> Result<bool, String> {
    write!(
        stdout,
        "Perplexity API key (stored in the global Pixel config, never printed)> "
    )
    .map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())?;
    let mut key = String::new();
    let _mute = MuteGuard::new(terminal)?;
    stdin
        .read_line(&mut key)
        .map_err(|e| format!("read key: {e}"))?;
    let key = key.trim();
    if key.is_empty() {
        writeln!(
            stdout,
            "web search provider: skipped (no key) — the free public chain stays the fallback"
        )
        .map_err(|e| e.to_string())?;
        return Ok(false);
    }
    store(key)?;
    writeln!(stdout, "web search provider: perplexity — key stored").map_err(|e| e.to_string())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An in-memory echo flag; `mutable: false` makes `mute` refuse, the
    /// pipe / CI / failed-`tcsetattr` case. `muted` is an interior-mutable
    /// mirror of the live state the test observes without holding a second
    /// borrow on the terminal.
    struct FakeEcho {
        echoed: bool,
        mutable: bool,
        muted: std::rc::Rc<std::cell::Cell<bool>>,
    }

    impl FakeEcho {
        fn live() -> Self {
            Self {
                echoed: true,
                mutable: true,
                muted: std::rc::Rc::new(std::cell::Cell::new(false)),
            }
        }
        fn unmutable() -> Self {
            Self {
                echoed: true,
                mutable: false,
                muted: std::rc::Rc::new(std::cell::Cell::new(false)),
            }
        }
    }

    impl EchoFlag for FakeEcho {
        fn echoed(&self) -> bool {
            self.echoed
        }
        fn mute(&mut self) -> Option<bool> {
            if !self.mutable {
                return None;
            }
            let prior = self.echoed;
            self.echoed = false;
            self.muted.set(true);
            Some(prior)
        }
        fn restore(&mut self, prior: bool) {
            self.echoed = prior;
            self.muted.set(false);
        }
    }

    #[test]
    fn the_choice_parser_accepts_exactly_one_and_two() {
        assert_eq!(parse_choice("1"), Some("searxng"));
        assert_eq!(parse_choice(" 2\n"), Some("perplexity"));
        assert_eq!(parse_choice(""), None);
        assert_eq!(parse_choice("3"), None);
        assert_eq!(parse_choice("yes"), None);
        assert_eq!(parse_choice("0"), None);
    }

    #[test]
    fn the_step_prints_every_option_and_skips_without_a_tty() {
        let mut output = Vec::new();
        install_step_with(
            false,
            &mut std::io::Cursor::new(Vec::new()),
            &mut output,
            |_| panic!("non-interactive install must not store a SearXNG URL"),
            |_| panic!("non-interactive install must not store a key"),
            || panic!("non-interactive install must not remove SearXNG"),
            &mut FakeEcho::live(),
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("Web search provider:"));
        assert!(text.contains(SEARXNG_LABEL));
        assert!(text.contains(PERPLEXITY_LABEL));
        assert!(text.contains(SKIP_LABEL));
        assert!(text.contains("non-interactive install"));
    }

    #[test]
    fn searxng_choice_stores_the_url_and_an_empty_answer_stores_nothing() {
        let mut stored = None;
        let mut output = Vec::new();
        install_step_with(
            true,
            &mut std::io::Cursor::new(b"1\nhttps://sx.test\n".to_vec()),
            &mut output,
            |url| {
                stored = Some(url.to_string());
                Ok(())
            },
            |_| panic!("the SearXNG choice must not store a key"),
            || panic!("the SearXNG choice must not remove SearXNG"),
            &mut FakeEcho::live(),
        )
        .unwrap();
        assert_eq!(stored, Some("https://sx.test".to_string()));
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("web search provider: searxng — URL stored"));
        assert!(text.contains("SearXNG base URL (e.g. https://searxng.example.com"));

        let mut stored = None;
        let mut output = Vec::new();
        install_step_with(
            true,
            &mut std::io::Cursor::new(b"1\n\n".to_vec()),
            &mut output,
            |url| {
                stored = Some(url.to_string());
                Ok(())
            },
            |_| panic!("an empty URL must not store a key"),
            || panic!("an empty URL must not remove SearXNG"),
            &mut FakeEcho::live(),
        )
        .unwrap();
        assert_eq!(stored, None);
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("skipped (no URL)")
        );
    }

    #[test]
    fn perplexity_choice_stores_the_key_removes_searxng_and_never_echoes_it() {
        // A stored key removes the stale SearXNG URL so Perplexity wins the
        // provider precedence instead of SearXNG masking it.
        let mut removed = false;
        let mut stored = None;
        let mut output = Vec::new();
        install_step_with(
            true,
            &mut std::io::Cursor::new(b"2\npplx-secret-key\n".to_vec()),
            &mut output,
            |_| panic!("the Perplexity choice must not store a SearXNG URL"),
            |key| {
                stored = Some(key.to_string());
                Ok(())
            },
            || {
                removed = true;
                Ok(())
            },
            &mut FakeEcho::live(),
        )
        .unwrap();
        assert_eq!(stored, Some("pplx-secret-key".to_string()));
        assert!(removed, "a stored key removes the stale SearXNG URL");
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("web search provider: perplexity — key stored"));
        assert!(!text.contains("pplx-secret-key"), "{text}");
    }

    #[test]
    fn an_empty_perplexity_key_leaves_searxng_active() {
        // An empty key deactivates nothing: the working SearXNG provider is
        // kept until a non-empty Perplexity key is actually stored.
        let mut removed = false;
        let mut stored = false;
        let mut output = Vec::new();
        install_step_with(
            true,
            &mut std::io::Cursor::new(b"2\n\n".to_vec()),
            &mut output,
            |_| panic!("an empty key must not store a SearXNG URL"),
            |_| {
                stored = true;
                Ok(())
            },
            || {
                removed = true;
                Ok(())
            },
            &mut FakeEcho::live(),
        )
        .unwrap();
        assert!(!stored, "an empty key stores nothing");
        assert!(!removed, "an empty key keeps the working SearXNG provider");
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("skipped (no key)")
        );
    }

    #[test]
    fn key_input_mutes_echo_while_reading_and_restores_it_afterward() {
        let terminal = std::rc::Rc::new(std::cell::RefCell::new(FakeEcho::live()));
        let muted = std::rc::Rc::clone(&terminal.borrow().muted);
        let mut stored = None;
        let mut output = Vec::new();
        let ok = ask_perplexity_key(
            &mut std::io::Cursor::new(b"pplx-secret\n".to_vec()),
            &mut output,
            |key| {
                // The read happens under the same guard as the store, so
                // echo must still be muted here.
                assert!(muted.get(), "echo must be muted during the key read");
                stored = Some(key.to_string());
                Ok(())
            },
            &mut *terminal.borrow_mut(),
        )
        .unwrap();
        assert!(ok, "a non-empty key is accepted");
        assert_eq!(stored, Some("pplx-secret".to_string()));
        assert!(!muted.get(), "echo restored after the key read");
    }

    #[test]
    fn key_input_fails_closed_when_echo_cannot_be_disabled() {
        let mut terminal = FakeEcho::unmutable();
        let mut output = Vec::new();
        let err = ask_perplexity_key(
            &mut std::io::Cursor::new(b"pplx-secret\n".to_vec()),
            &mut output,
            |_| panic!("echo not muted: the key must never be read"),
            &mut terminal,
        )
        .unwrap_err();
        assert!(err.contains("could not disable terminal echo"), "{err}");
        // Nothing was read, so the key never reached a store.
        assert!(terminal.echoed(), "echo was never toggled");
    }

    #[test]
    fn clipping_is_what_hides_echo() {
        // SAFETY: `zeroed` is only valid here because every field is
        // overwritten before the value escapes (c_lflag below, then the
        // assert reads only c_lflag).
        let mut termios: libc::termios = unsafe { std::mem::zeroed() };
        termios.c_lflag |= libc::ECHO;
        let muted = with_echo_clipped(termios);
        assert_eq!(muted.c_lflag & libc::ECHO, 0, "echo flag cleared");
        // Every other flag survives: canonical mode is kept for `read_line`.
        assert_eq!(muted.c_lflag, termios.c_lflag & !libc::ECHO);
    }

    #[test]
    fn skip_and_unknown_choices_store_nothing() {
        for answers in ["3\n", "\n", "yes\n", "0\n"] {
            let mut output = Vec::new();
            install_step_with(
                true,
                &mut std::io::Cursor::new(answers.as_bytes().to_vec()),
                &mut output,
                |_| panic!("a skipped choice must not store a URL"),
                |_| panic!("a skipped choice must not store a key"),
                || panic!("a skipped choice must not remove SearXNG"),
                &mut FakeEcho::live(),
            )
            .unwrap();
        }
        let mut output = Vec::new();
        install_step_with(
            true,
            &mut std::io::Cursor::new(b"3\n".to_vec()),
            &mut output,
            |_| panic!("skip must not store a URL"),
            |_| panic!("skip must not store a key"),
            || panic!("skip must not remove SearXNG"),
            &mut FakeEcho::live(),
        )
        .unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("the free public chain stays the fallback")
        );
    }
}
