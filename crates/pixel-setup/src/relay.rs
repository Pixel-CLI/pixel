// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The metrics relay (`specs/metrics.md`): when a tool result already carries
//! the finalized `🟩 pixel …` block of an `pixel` call, the hosts that draw a
//! hook's `systemMessage` show it to the user outside the collapsed tool
//! output. Everything here is a read of the result's text; a miss is silence.

use serde_json::{Value, json};

use crate::hosts::Host;

/// Characters of a tree row's frame: `│`, `├`, `└`.
const FRAME: [char; 3] = ['│', '├', '└'];

/// The marker every block's header line starts with.
const HEADER: &str = "🟩 pixel ";

/// Whether `line` is the header of a block: the marker, then the invocation id
/// `#` and six lowercase hex digits.
fn is_header(line: &str) -> bool {
    line.starts_with(HEADER)
        && line.rsplit_once('#').is_some_and(|(_, id)| {
            id.trim_end().len() == 6
                && id
                    .trim_end()
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
}

/// Whether `line` is a frame-only row (`│`, `└────`): the box without a number.
fn is_frame_only(line: &str) -> bool {
    let line = line.trim();
    !line.is_empty()
        && line.starts_with(['│', '└'])
        && line
            .chars()
            .all(|c| c == '│' || c == '└' || c == '─' || c.is_whitespace())
}

/// The finalized block at the very end of `text`: the identity header, then its
/// tree rows, frame-only rows dropped. Anchored at the end, so a `🟩` line
/// quoted by the output (a search hit in PIXEL's own sources) is never taken for
/// the call's footer.
pub fn footer(text: &str) -> Option<String> {
    let mut lines: Vec<&str> = text.lines().collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    let mut at = lines.len();
    while at > 0 && lines[at - 1].trim_start().starts_with(FRAME) {
        at -= 1;
    }
    let header = at
        .checked_sub(1)
        .filter(|&header| is_header(lines[header]))?;
    let block: Vec<&str> = lines[header..]
        .iter()
        .copied()
        .filter(|line| !is_frame_only(line))
        .collect();
    Some(block.join("\n"))
}

/// Every distinct footer among the string leaves of `payload` outside the
/// keys that hold what the user or the model sent in (`tool_input`, `args`,
/// `prompt`), in the order met.
pub fn footers(payload: &Value) -> Vec<String> {
    fn walk(value: &Value, found: &mut Vec<String>) {
        match value {
            Value::String(text) => {
                if let Some(block) = footer(text)
                    && !found.contains(&block)
                {
                    found.push(block);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, found)),
            Value::Object(map) => map
                .iter()
                .filter(|(key, _)| {
                    !matches!(key.as_str(), "tool_input" | "input" | "args" | "prompt")
                })
                .for_each(|(_, item)| walk(item, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    walk(payload, &mut found);
    found
}

/// What `host` is told when a tool result carried `blocks`, or `None` when the
/// host's user already reads the block in the result or the extension relays
/// it itself.
///
/// - Claude Code and Gemini CLI: the last block as `systemMessage` (their
///   shell results keep stderr out of what the user reads).
/// - Codex: the same, but only when exactly one block names this result, so a
///   result quoting an old block never relays a stale one.
/// - Devin: nothing, its tool result is drawn with the block in it.
/// - Antigravity: stored and delivered at the end of the invocation
///   (`hook/pending.rs`); pi and OpenCode: relayed by their extension.
pub fn relay_response(host: Host, blocks: &[String]) -> Option<Value> {
    let block = match host {
        Host::Claude | Host::Gemini => blocks.last()?,
        Host::Codex => match blocks {
            [only] => only,
            _ => return None,
        },
        Host::Devin | Host::Antigravity | Host::Pi | Host::OpenCode => return None,
    };
    Some(json!({"systemMessage": block}))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = "🟩 pixel find-symbol ❀ 55.2ms ❀ #753329\n  │\n  ├─ ⏱ 97% faster\n  ├─ § saved ~1269 tok\n  │\n  └────────────";
    const RELAYED: &str =
        "🟩 pixel find-symbol ❀ 55.2ms ❀ #753329\n  ├─ ⏱ 97% faster\n  ├─ § saved ~1269 tok";

    #[test]
    fn the_footer_at_the_end_of_a_result_is_found_without_its_empty_frame_rows() {
        let result = format!("src/a.rs:1 run\n\n{BLOCK}\n");
        assert_eq!(footer(&result).as_deref(), Some(RELAYED));
        assert_eq!(footer(BLOCK).as_deref(), Some(RELAYED));
    }

    #[test]
    fn a_block_quoted_in_the_middle_of_the_output_is_not_the_footer() {
        let quoted = format!("a hit:\n{BLOCK}\nand more output\n");
        assert_eq!(footer(&quoted), None);
        // Only the last block of a result that holds two.
        let two = format!("{BLOCK}\nthen\n{}", BLOCK.replace("753329", "0a1b2c"));
        assert!(footer(&two).unwrap().contains("#0a1b2c"));
    }

    #[test]
    fn a_header_needs_the_identity_and_the_marker() {
        for text in [
            "🟩 pixel find-symbol ❀ 55.2ms\n  └──",
            "🟩 pixel find-symbol ❀ 55.2ms ❀ #75332\n  └──",
            "🟩 pixel find-symbol ❀ 55.2ms ❀ #75332G\n  └──",
            "pixel find-symbol ❀ 55.2ms ❀ #753329\n  └──",
            "  ├─ a row with no header\n  └──",
            "",
        ] {
            assert_eq!(footer(text), None, "{text:?}");
        }
    }

    #[test]
    fn footers_are_read_from_the_result_and_not_from_what_was_sent_in() {
        let payload = json!({
            "tool_name": "Bash",
            "tool_input": {"command": BLOCK},
            "tool_response": {"stdout": "x", "stderr": BLOCK},
        });
        assert_eq!(footers(&payload), [RELAYED.to_owned()]);
        let quoted_only = json!({"tool_input": {"command": BLOCK}, "prompt": BLOCK});
        assert!(footers(&quoted_only).is_empty());
        let nested = json!({"toolCall": {"result": [{"text": BLOCK}, {"text": BLOCK}]}});
        assert_eq!(footers(&nested).len(), 1, "a block seen twice is one block");
    }

    #[test]
    fn each_host_is_relayed_the_way_it_draws_the_line() {
        let one = [RELAYED.to_owned()];
        let two = [RELAYED.to_owned(), RELAYED.replace("753329", "0a1b2c")];
        for host in [Host::Claude, Host::Gemini] {
            assert_eq!(
                relay_response(host, &one),
                Some(json!({"systemMessage": RELAYED}))
            );
            assert_eq!(
                relay_response(host, &two).unwrap()["systemMessage"],
                two[1],
                "the latest block"
            );
        }
        assert_eq!(
            relay_response(Host::Codex, &one),
            Some(json!({"systemMessage": RELAYED}))
        );
        assert_eq!(
            relay_response(Host::Codex, &two),
            None,
            "an ambiguous result is silent"
        );
        for host in [Host::Devin, Host::Antigravity, Host::Pi, Host::OpenCode] {
            assert_eq!(relay_response(host, &one), None, "{host:?}");
        }
        assert_eq!(relay_response(Host::Claude, &[]), None);
    }
}
