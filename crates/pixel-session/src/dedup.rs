// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Message normalization + stable dedup hashing.
//!
//! Dedup identity: `sha256(surface + kind + normalize(message) + top3AppFrames)`.
//! Normalization strips volatile fragments (URLs, timestamps, hex ids, big
//! numbers) so repeats of the same logical error collapse into one row.

use sha2::{Digest, Sha256};

use crate::types::{Frame, Surface};

/// Replace volatile message fragments with stable placeholders.
pub fn normalize_message(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut chars = message.chars().peekable();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if word.is_empty() {
            return;
        }
        out.push_str(&classify_word(word));
        word.clear();
    };
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            flush(&mut word, &mut out);
            // Collapse runs of whitespace into one space.
            while chars.peek().is_some_and(|n| n.is_whitespace()) {
                chars.next();
            }
            out.push(' ');
        } else {
            word.push(c);
        }
    }
    flush(&mut word, &mut out);
    out.trim().to_owned()
}

/// Classify one whitespace-delimited token, replacing volatile forms.
fn classify_word(word: &str) -> String {
    if word.starts_with("http://") || word.starts_with("https://") {
        // Preserve trailing punctuation that is clearly sentence-level.
        let trailing: String = word
            .chars()
            .rev()
            .take_while(|c| matches!(c, '.' | ',' | ')' | '\'' | '"'))
            .collect();
        let trailing: String = trailing.chars().rev().collect();
        return format!("<url>{trailing}");
    }
    // Token-internal replacements on the bare word (strip common punctuation
    // wrappers so `(deadbeefcafe1234)` still normalizes).
    replace_spans(word)
}

/// Replace hex ids (>=8 hex chars), ISO timestamps, and big numbers (>=5
/// digits) inside a token.
fn replace_spans(word: &str) -> String {
    // ISO timestamp: cheap detection — starts with dddd-dd-dd and contains ':'.
    let bytes = word.as_bytes();
    let looks_iso = word.len() >= 16
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes.get(4) == Some(&b'-')
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes.get(7) == Some(&b'-')
        && word.contains(':');
    if looks_iso {
        return "<ts>".to_owned();
    }
    let mut out = String::with_capacity(word.len());
    let chars: Vec<char> = word.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_hexdigit() {
            let start = i;
            let mut any_alpha = false;
            let mut all_digit = true;
            while i < chars.len() && chars[i].is_ascii_hexdigit() {
                if chars[i].is_ascii_alphabetic() {
                    any_alpha = true;
                }
                if !chars[i].is_ascii_digit() {
                    all_digit = false;
                }
                i += 1;
            }
            let len = i - start;
            // Boundary check: a hex run glued to more alphanumerics is an
            // identifier, not a hex id.
            let bounded = i >= chars.len() || !chars[i].is_alphanumeric();
            if bounded && len >= 8 && (any_alpha || all_digit) {
                out.push_str("<hex>");
            } else if bounded && all_digit && len >= 5 {
                out.push_str("<num>");
            } else {
                out.extend(&chars[start..i]);
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

fn is_app_file(file: &str) -> bool {
    !file.is_empty()
        && !file.contains("node_modules")
        && !file.starts_with("node:")
        && !file.starts_with("bun:")
}

/// The (up to) three application frames that anchor the dedup identity.
pub fn top_app_frames(frames: Option<&[Frame]>) -> Vec<String> {
    frames
        .unwrap_or_default()
        .iter()
        .filter_map(|f| f.best_location())
        .filter(|(file, _, _)| is_app_file(file))
        .take(3)
        .map(|(file, line, _)| format!("{file}:{line}"))
        .collect()
}

/// Stable 64-hex dedup hash for one error identity.
pub fn dedup_hash(
    surface: Surface,
    kind: Option<&str>,
    message: &str,
    frames: Option<&[Frame]>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(surface.as_str());
    hasher.update("\n");
    hasher.update(kind.unwrap_or(""));
    hasher.update("\n");
    hasher.update(normalize_message(message));
    for frame in top_app_frames(frames) {
        hasher.update("\n");
        hasher.update(frame);
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_urls() {
        assert_eq!(
            normalize_message("failed to fetch http://localhost:3000/api/x?id=1 now"),
            "failed to fetch <url> now"
        );
    }

    #[test]
    fn normalize_strips_long_hex() {
        assert_eq!(
            normalize_message("session deadbeefcafe1234 expired"),
            "session <hex> expired"
        );
    }

    #[test]
    fn normalize_keeps_short_hexish_words() {
        assert_eq!(normalize_message("cafe bad"), "cafe bad");
    }

    #[test]
    fn normalize_strips_iso_timestamps() {
        assert_eq!(
            normalize_message("at 2026-08-27T10:11:12.123Z boom"),
            "at <ts> boom"
        );
    }

    #[test]
    fn normalize_strips_big_numbers_keeps_small() {
        assert_eq!(
            normalize_message("port 3000 pid 123456"),
            "port 3000 pid <num>"
        );
    }

    #[test]
    fn normalize_collapses_whitespace() {
        assert_eq!(normalize_message("a   b\n c"), "a b c");
    }

    #[test]
    fn normalize_leaves_identifiers_alone() {
        assert_eq!(
            normalize_message("undefined is not an object (evaluating 'api.sessions.x')"),
            "undefined is not an object (evaluating 'api.sessions.x')"
        );
    }

    fn frame(file: &str, line: u32) -> Frame {
        Frame {
            raw: format!("at {file}:{line}"),
            file: Some(file.to_owned()),
            line: Some(line),
            ..Frame::default()
        }
    }

    #[test]
    fn top_app_frames_filters_and_caps() {
        let frames = vec![
            frame("node:internal/modules", 1),
            frame("/repo/node_modules/react/index.js", 2),
            frame("src/routes/chat.tsx", 88),
            frame("src/lib/api.ts", 12),
            frame("src/main.tsx", 3),
            frame("src/extra.ts", 4),
        ];
        assert_eq!(
            top_app_frames(Some(&frames)),
            vec![
                "src/routes/chat.tsx:88",
                "src/lib/api.ts:12",
                "src/main.tsx:3"
            ]
        );
    }

    #[test]
    fn top_app_frames_prefers_mapped_location() {
        let f = Frame {
            raw: "at bundle".into(),
            file: Some("/x/assets/app.js".into()),
            line: Some(1),
            mapped_file: Some("src/a.tsx".into()),
            mapped_line: Some(9),
            ..Frame::default()
        };
        assert_eq!(top_app_frames(Some(&[f])), vec!["src/a.tsx:9"]);
    }

    #[test]
    fn hash_stable_across_volatile_fragments() {
        let a = dedup_hash(
            Surface::BrowserWindow,
            Some("TypeError"),
            "cannot load http://localhost:3000/a",
            None,
        );
        let b = dedup_hash(
            Surface::BrowserWindow,
            Some("TypeError"),
            "cannot load http://localhost:3000/b?x=1",
            None,
        );
        assert_eq!(a, b);
    }

    #[test]
    fn hash_differs_by_surface_kind_frames() {
        let base = dedup_hash(Surface::BrowserWindow, Some("TypeError"), "boom", None);
        assert_ne!(
            base,
            dedup_hash(Surface::ServerConsole, Some("TypeError"), "boom", None)
        );
        assert_ne!(
            base,
            dedup_hash(Surface::BrowserWindow, Some("RangeError"), "boom", None)
        );
        let fa = [frame("src/a.ts", 1)];
        let fb = [frame("src/b.ts", 1)];
        assert_ne!(
            dedup_hash(Surface::Reported, None, "boom", Some(&fa)),
            dedup_hash(Surface::Reported, None, "boom", Some(&fb)),
        );
    }

    #[test]
    fn hash_is_64_hex() {
        let h = dedup_hash(Surface::Tsc, Some("TS2322"), "type mismatch", None);
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
