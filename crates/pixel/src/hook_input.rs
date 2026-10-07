// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The one bounded stdin reader the hook entry points share.
//!
//! A coding agent writes the hook payload on stdin and waits for the verdict.
//! Reading the pipe to EOF means a host that emits an oversized payload makes
//! Pixel allocate it in full before any decision, so `task-event` and the
//! retired verbs (which only drain the pipe) read through [`read_bounded`] and
//! stop at [`MAX_HOOK_INPUT`] instead. What the cap bounds is that
//! *allocation*: a payload past the cap is refused after `cap + 1` bytes and
//! the rest is never drained. It does not bound the *wait* — a writer that
//! sends fewer than `cap + 1` bytes and holds the pipe open still stalls the
//! read exactly as before, and only the host's own hook timeout
//! (`HOOK_TIMEOUT`, 10 s, registered by `pixel install`) ends that wait.
//! `task-event` has no silent path, so an over-cap payload takes its
//! unavailable envelope, which denies `PreToolUse` on an enforced session.
//!
//! The cap is the task-event cap, [`MAX_HOOK_INPUT`]: the largest hook payload
//! an installed host is expected to emit.

use std::io::Read;

/// The largest hook payload read from stdin, in bytes.
pub(crate) const MAX_HOOK_INPUT: u64 = 1_048_576;

/// Read one hook payload from stdin through [`MAX_HOOK_INPUT`].
///
/// `None` when the payload cannot be used: a read error, a payload over the
/// cap, or one that is empty once trimmed. Callers exit 0 on `None`, leaving
/// the native tool untouched — the same outcome as the unparseable payload
/// they already reject.
pub(crate) fn read_hook_payload() -> Option<String> {
    read_bounded(&mut std::io::stdin(), MAX_HOOK_INPUT)
}

/// The bounded read behind [`read_hook_payload`], split out so a test drives
/// the cap without the process-global stdin.
///
/// `None` on a read error, over the cap, or empty once trimmed. The cap is
/// read as `cap + 1` bytes: a payload exactly at the cap is accepted, and the
/// first byte past it is what makes the read fail the check, so the reader
/// never has to drain an oversized pipe to learn its length.
pub(crate) fn read_bounded<R: Read>(reader: &mut R, cap: u64) -> Option<String> {
    let mut raw = String::new();
    reader
        .take(cap.saturating_add(1))
        .read_to_string(&mut raw)
        .ok()?;
    if raw.len() as u64 > cap || raw.trim().is_empty() {
        return None;
    }
    Some(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A payload exactly at the cap is accepted: the reader must reject only
    /// what is *over* it, or a host that grows its payload silently loses
    /// every hook.
    #[test]
    fn bounded_read_should_accept_a_payload_exactly_at_the_cap() {
        let payload = "a".repeat(MAX_HOOK_INPUT as usize);
        assert_eq!(
            read_bounded(&mut payload.as_bytes(), MAX_HOOK_INPUT),
            Some(payload)
        );
    }

    /// One byte over the cap is refused, and the refusal is what keeps the
    /// entry points from parsing — and waiting on — an unbounded pipe.
    #[test]
    fn bounded_read_should_refuse_a_payload_one_byte_over_the_cap() {
        let payload = "a".repeat(MAX_HOOK_INPUT as usize + 1);
        assert_eq!(read_bounded(&mut payload.as_bytes(), MAX_HOOK_INPUT), None);
    }

    /// Far over the cap, not just adjacent to it: a reader that only compared
    /// the trailing byte would pass the previous test and still accept this.
    #[test]
    fn bounded_read_should_refuse_a_payload_far_over_the_cap() {
        let payload = "a".repeat(MAX_HOOK_INPUT as usize * 4);
        assert_eq!(read_bounded(&mut payload.as_bytes(), MAX_HOOK_INPUT), None);
    }

    /// An empty or whitespace-only payload carries no event, so it takes the
    /// same exit-0 path as an oversized one rather than reaching a parser.
    #[test]
    fn bounded_read_should_refuse_an_empty_or_blank_payload() {
        for payload in ["", "   ", "\n\t \r\n"] {
            assert_eq!(read_bounded(&mut payload.as_bytes(), MAX_HOOK_INPUT), None);
        }
    }

    /// A real hook payload at a realistic size survives the cap untouched,
    /// byte for byte: the reader adds no trimming or normalization that a
    /// caller parsing JSON would notice.
    #[test]
    fn bounded_read_should_return_a_typical_payload_unchanged() {
        let payload = r#"{"hook_event_name":"PreToolUse","tool_name":"Grep","cwd":"/tmp/x"}"#;
        assert_eq!(
            read_bounded(&mut payload.as_bytes(), MAX_HOOK_INPUT),
            Some(payload.to_string())
        );
    }

    /// The shared cap is the task-event cap, so the two spellings cannot
    /// drift apart: a payload `task-event` accepts is one the retired verbs drain.
    #[test]
    fn the_shared_cap_should_equal_the_task_event_cap() {
        assert_eq!(MAX_HOOK_INPUT, crate::task_hook::MAX_INPUT);
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
        let mut reader = FailAfter {
            good: b"{\"hook_event_name\":",
            failed: false,
        };
        assert_eq!(read_bounded(&mut reader, MAX_HOOK_INPUT), None);
    }
}
