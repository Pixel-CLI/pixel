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
    install_step_with(
        tty,
        stdin,
        stdout,
        crate::config_cmd::set_web_search_searxng_url,
        crate::config_cmd::set_web_search_perplexity_key,
    )
}

fn install_step_with(
    tty: bool,
    stdin: &mut dyn BufRead,
    stdout: &mut dyn Write,
    store_searxng: impl FnOnce(&str) -> Result<(), String>,
    store_key: impl FnOnce(&str) -> Result<(), String>,
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
            // Picker-specific only: switching to Perplexity must deactivate
            // a stored SearXNG, or `pixel web-search` and `pixel config`
            // would keep resolving SearXNG above it. The general setters
            // (and the `PIXEL_WEB_SEARCH_URL` override) keep their
            // coexistence behaviour.
            store_searxng("")?;
            ask_perplexity_key(stdin, stdout, store_key)
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

/// Disable echo for one secret read, restoring the terminal on drop. Unlike
/// [`crate::prompt_key::RawGuard`], canonical mode (the line discipline
/// `read_line` relies on) is kept: only `ECHO` is cleared, so the key is
/// typed blind but still line-buffered. `None` when stdin is not a
/// configurable terminal (a pipe, a CI run); the caller then reads with
/// echo, as it would on a redirect.
struct EchoGuard {
    original: libc::termios,
}

impl EchoGuard {
    #[cfg_attr(test, mutants::skip)] // libc adapter; needs a real tty to matter
    fn new() -> Option<Self> {
        // SAFETY: zeroed memory is a valid (if meaningless) termios buffer
        // that the tcgetattr call immediately overwrites.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: a single tcgetattr on stdin with a valid, zeroed buffer.
        if unsafe { libc::tcgetattr(0, &mut original) } != 0 {
            return None;
        }
        let mut muted = original;
        muted.c_lflag &= !libc::ECHO;
        // SAFETY: TCSANOW applies the descriptor's own modified settings.
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &muted) } != 0 {
            return None;
        }
        Some(Self { original })
    }
}

impl Drop for EchoGuard {
    #[cfg_attr(test, mutants::skip)] // libc adapter; the restore needs a real tty to observe
    fn drop(&mut self) {
        // SAFETY: restoring the settings this guard read from stdin.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.original) };
    }
}

/// The Perplexity key prompt: an empty answer stores nothing, exactly like
/// `pixel config remote-key`'s empty-key rule. The echo is muted while the
/// key is typed, so it never lands in terminal scrollback or a recording.
fn ask_perplexity_key(
    stdin: &mut dyn BufRead,
    stdout: &mut dyn Write,
    store: impl FnOnce(&str) -> Result<(), String>,
) -> Result<(), String> {
    write!(
        stdout,
        "Perplexity API key (stored in the global Pixel config, never printed)> "
    )
    .map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())?;
    let mut key = String::new();
    let _mute = EchoGuard::new();
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
        return Ok(());
    }
    store(key)?;
    writeln!(stdout, "web search provider: perplexity — key stored").map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn perplexity_choice_stores_the_key_clears_searxng_and_never_echoes_it() {
        let mut cleared = None;
        let mut stored = None;
        let mut output = Vec::new();
        install_step_with(
            true,
            &mut std::io::Cursor::new(b"2\npplx-secret-key\n".to_vec()),
            &mut output,
            |url| {
                cleared = Some(url.to_string());
                Ok(())
            },
            |key| {
                stored = Some(key.to_string());
                Ok(())
            },
        )
        .unwrap();
        // Switching to Perplexity deactivates a stored SearXNG so it wins
        // the provider precedence instead of masking the new choice.
        assert_eq!(cleared, Some(String::new()));
        assert_eq!(stored, Some("pplx-secret-key".to_string()));
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("web search provider: perplexity — key stored"));
        assert!(!text.contains("pplx-secret-key"), "{text}");
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
        )
        .unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("the free public chain stays the fallback")
        );
    }
}
