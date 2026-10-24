// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The one bounded stdin reader the hook entry points share
//! (`specs/hosts.md`, "One entry point").
//!
//! A coding agent writes the hook payload on stdin and waits for the verdict.
//! Reading the pipe to EOF means a host that emits an oversized payload makes
//! PIXEL allocate it in full before any decision, so the hook reads through
//! [`read_hook_payload`] and stops at [`MAX_HOOK_INPUT`]: a payload past the
//! cap is refused after `cap + 1` bytes and the rest is never drained.
//!
//! The read is bounded in time too. A host that writes its payload and holds
//! the pipe open (or never writes at all) would otherwise stall the hook until
//! the host's own timeout killed it: no exit 0, no answer. At [`STDIN_WAIT`]
//! the hook stops waiting for the end of the pipe and takes what arrived when
//! it is one whole JSON object, and nothing otherwise, so a truncated payload
//! is never parsed as if it were whole. A payload the hook cannot use is
//! silence: it prints nothing and exits 0.

use std::io::Read;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// The largest hook payload read from stdin, in bytes.
pub(crate) const MAX_HOOK_INPUT: u64 = 1_048_576;

/// How long a hook waits for its host to close stdin. A host writes the
/// payload as it spawns the hook and closes the pipe; one second leaves a
/// 1 MiB payload ample time on a loaded machine and, with the brief's window,
/// stays inside the host deadlines (`Host::deadline`).
pub(crate) const STDIN_WAIT: Duration = Duration::from_secs(1);

/// Read one hook payload from stdin through [`MAX_HOOK_INPUT`], waiting at
/// most [`STDIN_WAIT`] for the host to close the pipe.
///
/// `None` when the payload cannot be used: a read error, a payload over the
/// cap, one that is empty once trimmed, or a pipe still open at the deadline
/// on anything but one whole JSON object. Callers treat `None` as the
/// unparseable payload they already reject, and exit 0.
pub(crate) fn read_hook_payload() -> Option<String> {
    read_within(std::io::stdin(), MAX_HOOK_INPUT, STDIN_WAIT)
}

/// The bounded read behind [`read_hook_payload`], split out so a test drives
/// the cap and the deadline without the process-global stdin.
///
/// `None` on a read error, over the cap, or empty once trimmed. The cap is
/// read as `cap + 1` bytes: a payload exactly at the cap is accepted, and the
/// first byte past it is what makes the read fail the check, so the reader
/// never has to drain an oversized pipe to learn its length. The bytes are
/// read on a thread of their own: after `wait`, the caller goes on with what
/// arrived when it is one whole JSON object, or with nothing. A read still
/// blocked then keeps its thread, which ends with the process: every hook
/// entry point exits as soon as it has answered.
pub(crate) fn read_within<R: Read + Send + 'static>(
    reader: R,
    cap: u64,
    wait: Duration,
) -> Option<String> {
    let arrived = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&arrived);
    let (done, finished) = mpsc::channel();
    std::thread::Builder::new()
        .name("pixel-hook-stdin".into())
        .spawn(move || {
            let mut reader = reader.take(cap.saturating_add(1));
            let mut chunk = vec![0_u8; 64 * 1024];
            let read = loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break true,
                    Ok(n) => match sink.lock() {
                        Ok(mut bytes) => bytes.extend_from_slice(&chunk[..n]),
                        Err(_) => break false,
                    },
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break false,
                }
            };
            let _ = done.send(read);
        })
        .ok()?;
    let closed = match finished.recv_timeout(wait) {
        Ok(true) => true,
        // A read error: whatever arrived before it is not trusted.
        Ok(false) => return None,
        Err(_) => false,
    };
    let bytes = arrived.lock().ok()?.clone();
    if !closed && !serde_json::from_slice::<serde_json::Value>(&bytes).is_ok_and(|v| v.is_object())
    {
        return None;
    }
    let raw = String::from_utf8(bytes).ok()?;
    if raw.len() as u64 > cap || raw.trim().is_empty() {
        return None;
    }
    Some(raw)
}

/// Runs a hook's work, then exits 0 whatever happened in it, a panic
/// included: a host blocks the prompt or the tool call on any other exit,
/// and an unwinding panic exits 101. The panic message still reaches stderr
/// through the default panic hook. What the work printed before panicking
/// stays and nothing is added: a hook prints one JSON object or nothing, so
/// a fallback object after the response would make two.
///
/// Panics on the hook's worker threads never reach here; the decision
/// window already turns them into its unavailable envelope.
#[cfg_attr(test, mutants::skip)] // process::exit boundary; the CLI hook guarantee tests drive it
pub(crate) fn exit_0_after(work: impl FnOnce()) -> ! {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
    std::process::exit(0)
}

/// Debug builds only: `PIXEL_HOOK_TEST_PANIC=<stage>` makes a hook panic at
/// that stage, so the CLI tests can prove [`exit_0_after`] on the real
/// binary. A release build has no such variable.
#[cfg(debug_assertions)]
pub(crate) const TEST_PANIC_ENV: &str = "PIXEL_HOOK_TEST_PANIC";

/// Panics when [`TEST_PANIC_ENV`] names `stage` in a debug build; does
/// nothing otherwise.
#[cfg_attr(test, mutants::skip)] // test seam: only the CLI tests set the variable
pub(crate) fn test_panic(stage: &str) {
    #[cfg(debug_assertions)]
    assert!(
        !std::env::var(TEST_PANIC_ENV).is_ok_and(|value| value == stage),
        "{TEST_PANIC_ENV}={stage}"
    );
    #[cfg(not(debug_assertions))]
    let _ = stage;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The read of a pipe its host wrote `bytes` to and closed.
    fn closed(bytes: &[u8]) -> Option<String> {
        read_within(
            std::io::Cursor::new(bytes.to_vec()),
            MAX_HOOK_INPUT,
            STDIN_WAIT,
        )
    }

    /// A payload exactly at the cap is accepted: the reader must reject only
    /// what is *over* it, or a host that grows its payload silently loses
    /// every hook.
    #[test]
    fn bounded_read_should_accept_a_payload_exactly_at_the_cap() {
        let payload = "a".repeat(MAX_HOOK_INPUT as usize);
        assert_eq!(closed(payload.as_bytes()), Some(payload));
    }

    /// One byte over the cap is refused, and the refusal is what keeps the
    /// entry points from parsing — and waiting on — an unbounded pipe.
    #[test]
    fn bounded_read_should_refuse_a_payload_one_byte_over_the_cap() {
        let payload = "a".repeat(MAX_HOOK_INPUT as usize + 1);
        assert_eq!(closed(payload.as_bytes()), None);
    }

    /// Far over the cap, not just adjacent to it: a reader that only compared
    /// the trailing byte would pass the previous test and still accept this.
    #[test]
    fn bounded_read_should_refuse_a_payload_far_over_the_cap() {
        let payload = "a".repeat(MAX_HOOK_INPUT as usize * 4);
        assert_eq!(closed(payload.as_bytes()), None);
    }

    /// An empty or whitespace-only payload carries no event, so it takes the
    /// same exit-0 path as an oversized one rather than reaching a parser.
    #[test]
    fn bounded_read_should_refuse_an_empty_or_blank_payload() {
        for payload in ["", "   ", "\n\t \r\n"] {
            assert_eq!(closed(payload.as_bytes()), None);
        }
    }

    /// A real hook payload at a realistic size survives the cap untouched,
    /// byte for byte: the reader adds no trimming or normalization that a
    /// caller parsing JSON would notice.
    #[test]
    fn bounded_read_should_return_a_typical_payload_unchanged() {
        let payload = r#"{"hook_event_name":"PreToolUse","tool_name":"Grep","cwd":"/tmp/x"}"#;
        assert_eq!(closed(payload.as_bytes()), Some(payload.to_string()));
    }

    /// A reader that yields `good` and then fails, so the partial input a real
    /// pipe can leave behind is checked rather than trusted.
    struct FailAfter {
        good: &'static [u8],
        failed: bool,
    }

    impl std::io::Read for FailAfter {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.failed {
                return Err(std::io::Error::other("injected read failure"));
            }
            self.failed = true;
            let n = self.good.len().min(buf.len());
            buf[..n].copy_from_slice(&self.good[..n]);
            Ok(n)
        }
    }

    /// A read that fails partway must yield `None`, not the partial input: the
    /// entry points treat `None` as "cannot use this payload", and a truncated
    /// payload that slipped through would be parsed as if it were whole.
    #[test]
    fn bounded_read_should_refuse_a_payload_whose_read_fails_partway() {
        let reader = FailAfter {
            good: b"{\"hook_event_name\":",
            failed: false,
        };
        assert_eq!(read_within(reader, MAX_HOOK_INPUT, STDIN_WAIT), None);
    }

    /// The timed read fails the same way: a payload that is whole JSON but
    /// whose pipe then errors is not trusted either.
    #[test]
    fn timed_read_should_refuse_a_payload_whose_read_fails_after_it() {
        let reader = FailAfter {
            good: b"{\"hook_event_name\":\"Stop\"}",
            failed: false,
        };
        assert_eq!(read_within(reader, MAX_HOOK_INPUT, STDIN_WAIT), None);
    }

    /// A read a signal interrupts is retried, not taken for a broken pipe:
    /// a hook interrupted while its host writes still gets the whole payload.
    #[test]
    fn timed_read_should_retry_a_read_a_signal_interrupted() {
        struct Interrupted(Option<std::io::Cursor<&'static [u8]>>);
        impl std::io::Read for Interrupted {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                match &mut self.0 {
                    None => {
                        self.0 = Some(std::io::Cursor::new(br#"{"prompt":"x"}"#));
                        Err(std::io::ErrorKind::Interrupted.into())
                    }
                    Some(rest) => rest.read(buf),
                }
            }
        }
        assert_eq!(
            read_within(Interrupted(None), MAX_HOOK_INPUT, STDIN_WAIT).as_deref(),
            Some(r#"{"prompt":"x"}"#)
        );
    }

    /// A pipe that yields `head`, then blocks until the test ends: a host that
    /// wrote its payload and did not close stdin.
    struct HeldOpen {
        head: Option<&'static [u8]>,
        release: mpsc::Receiver<()>,
    }

    impl std::io::Read for HeldOpen {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if let Some(head) = self.head.take() {
                buf[..head.len()].copy_from_slice(head);
                return Ok(head.len());
            }
            // Blocks until the sender is dropped, then reads as an error so
            // a test never sees this pipe end cleanly.
            let _ = self.release.recv();
            Err(std::io::Error::other("released"))
        }
    }

    fn held(head: Option<&'static [u8]>) -> (HeldOpen, mpsc::Sender<()>) {
        let (release_tx, release) = mpsc::channel();
        (HeldOpen { head, release }, release_tx)
    }

    /// A whole payload on a pipe its host never closes is answered at the
    /// deadline, not when the host kills the hook: the wait is bounded, and
    /// the payload is the one the host wrote.
    #[test]
    fn timed_read_should_take_a_whole_object_from_a_pipe_left_open() {
        let payload: &'static [u8] = br#"{"hook_event_name":"UserPromptSubmit","prompt":"x"}"#;
        let (reader, _release) = held(Some(payload));
        let wait = Duration::from_millis(200);
        let started = std::time::Instant::now();
        let read = read_within(reader, MAX_HOOK_INPUT, wait);
        let took = started.elapsed();
        assert_eq!(read.as_deref().map(str::as_bytes), Some(payload));
        assert!(took >= wait, "returned before the deadline: {took:?}");
        assert!(
            took < Duration::from_secs(5),
            "held past the deadline: {took:?}"
        );
    }

    /// Half a payload on a pipe left open is nothing: the prefix of an object
    /// is not an object, and parsing it would read a truncated event as whole.
    /// A whole value that is not an object, or nothing written at all, is
    /// nothing too, in the same bounded time.
    #[test]
    fn timed_read_should_refuse_a_partial_or_absent_payload_on_a_pipe_left_open() {
        for head in [
            Some(&br#"{"hook_event_name":"Pre"#[..]),
            Some(&b"[1, 2]"[..]),
            None,
        ] {
            let (reader, _release) = held(head);
            let started = std::time::Instant::now();
            assert_eq!(
                read_within(reader, MAX_HOOK_INPUT, Duration::from_millis(100)),
                None,
                "{head:?}"
            );
            assert!(started.elapsed() < Duration::from_secs(5), "{head:?}");
        }
    }

    /// A closed pipe answers as soon as it closes, with the cap and the
    /// blank-payload rule of [`closed`]: the deadline only matters to a
    /// pipe left open.
    #[test]
    fn timed_read_should_keep_the_cap_and_answer_a_closed_pipe_at_once() {
        let read = |bytes: Vec<u8>, wait: Duration| {
            read_within(std::io::Cursor::new(bytes), MAX_HOOK_INPUT, wait)
        };
        let started = std::time::Instant::now();
        let typical = "not json, but the whole of what the host sent";
        assert_eq!(
            read(typical.as_bytes().to_vec(), Duration::from_secs(30)).as_deref(),
            Some(typical)
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        let at_cap = "a".repeat(MAX_HOOK_INPUT as usize);
        let over = "a".repeat(MAX_HOOK_INPUT as usize + 1);
        assert_eq!(
            read(at_cap.clone().into_bytes(), STDIN_WAIT).map(|raw| raw.len()),
            Some(at_cap.len())
        );
        assert_eq!(read(over.into_bytes(), STDIN_WAIT), None);
        assert_eq!(read(b" \n\t".to_vec(), STDIN_WAIT), None);
        // Bytes that are not UTF-8 are no payload.
        assert_eq!(read(vec![b'{', 0xff, b'}'], STDIN_WAIT), None);
    }
}
