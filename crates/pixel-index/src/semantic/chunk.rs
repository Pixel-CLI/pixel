// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Cutting files into the chunks that get embedded.
//!
//! A parsed file is cut at its symbols: one chunk per symbol holding its own
//! lines (a container's chunk stops where its first child starts, so no line
//! is embedded twice), preceded by the doc comment above it. A file the graph
//! has no symbols for (prose, config, data) is cut into overlapping windows.
//! The chunker reads symbol spans it is handed: the graph store already
//! parsed the file, and parsing it again here is what Pixel's chunker did.

use xxhash_rust::xxh3::xxh3_128;

/// Bumped when the cutting changes what a chunk's text is: every stored
/// vector is keyed by it ([`chunk_key`]), so a bump re-embeds the corpus.
pub const CHUNKER_VERSION: u32 = 1;

/// A chunk's text stops here (bytes, cut at a character boundary).
pub const CHUNK_MAX: usize = 1500;

/// A window of an unparsed file: at most this many lines...
pub const WINDOW_LINES: usize = 40;

/// ...overlapping the next by this many.
pub const WINDOW_OVERLAP: usize = 6;

/// At most this many comment lines above a symbol belong to it.
const DOC_LINES: usize = 12;

/// A file of at most this many bytes is read; a longer one is skipped: a
/// generated bundle or a data dump embeds noise.
pub const FILE_MAX: usize = 512 * 1024;

/// A symbol's place in its file, as the graph store holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolSpan {
    /// The qualified name.
    pub name: String,
    /// 1-based first line.
    pub line: u32,
    /// 1-based last line, inclusive.
    pub end_line: u32,
}

/// One embeddable piece of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub path: String,
    /// 1-based, inclusive.
    pub start_line: u32,
    pub end_line: u32,
    /// The symbol the chunk belongs to, when it belongs to one.
    pub symbol: Option<String>,
    /// What is embedded and searched: `path`, the symbol and the lines.
    pub text: String,
}

impl Chunk {
    /// The first line of the chunk's lines on one line, `cap` characters at
    /// most: what a hit shows.
    pub fn snippet(&self, cap: usize) -> String {
        let body = self
            .text
            .lines()
            .skip(1 + usize::from(self.symbol.is_some()))
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("");
        body.chars().take(cap).collect()
    }
}

/// The content-addressed key of a chunk's vector: its text hashed with the
/// model, its revision and [`CHUNKER_VERSION`], so a vector is reused only
/// when the same model would embed the same text the same way.
pub fn chunk_key(model_id: &str, revision: u32, text: &str) -> u128 {
    let mut seed = Vec::with_capacity(model_id.len() + text.len() + 16);
    seed.extend_from_slice(model_id.as_bytes());
    seed.push(0);
    seed.extend_from_slice(&revision.to_le_bytes());
    seed.extend_from_slice(&CHUNKER_VERSION.to_le_bytes());
    seed.extend_from_slice(text.as_bytes());
    xxh3_128(&seed)
}

/// Cuts one file. `symbols` are the file's symbols in any order; an empty
/// list cuts windows.
pub fn chunk_file(path: &str, text: &str, symbols: &[SymbolSpan]) -> Vec<Chunk> {
    if text.len() > FILE_MAX || text.contains('\0') {
        return Vec::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    if symbols.is_empty() {
        return windows(path, &lines);
    }
    let mut spans: Vec<&SymbolSpan> = symbols
        .iter()
        .filter(|s| s.line >= 1 && s.end_line >= s.line && (s.line as usize) <= lines.len())
        .collect();
    spans.sort_by_key(|s| (s.line, std::cmp::Reverse(s.end_line)));
    let mut chunks = Vec::new();
    for (index, span) in spans.iter().enumerate() {
        // The symbol's own lines stop where a symbol nested in it starts.
        let child = spans[index + 1..]
            .iter()
            .find(|next| next.line > span.line && next.line <= span.end_line)
            .map(|next| next.line - 1);
        let end = child.unwrap_or(span.end_line).min(lines.len() as u32);
        // A doc comment never reaches back past the end of an earlier symbol.
        let floor = spans[..index]
            .iter()
            .map(|p| p.end_line)
            .filter(|e| *e < span.line)
            .max()
            .map_or(1, |e| e + 1);
        let start = doc_start(&lines, span.line, floor);
        chunks.push(build(path, &lines, start, end, Some(&span.name), span.line));
    }
    chunks.retain(|chunk| !chunk.text.trim().is_empty());
    chunks
}

/// The first line of the doc comment directly above `line` (and not above
/// `floor`), or `line`.
fn doc_start(lines: &[&str], line: u32, floor: u32) -> u32 {
    let mut start = line;
    while start > floor && line - start < DOC_LINES as u32 {
        let above = lines[(start - 2) as usize].trim_start();
        let comment = above.starts_with("///")
            || above.starts_with("//!")
            || above.starts_with("//")
            || above.starts_with('#')
            || above.starts_with("/*")
            || above.starts_with('*')
            || above.starts_with("--")
            || above.starts_with("\"\"\"");
        if !comment {
            break;
        }
        start -= 1;
    }
    start
}

fn build(
    path: &str,
    lines: &[&str],
    start: u32,
    end: u32,
    symbol: Option<&str>,
    first: u32,
) -> Chunk {
    let body = lines[(start - 1) as usize..end as usize].join("\n");
    let mut text = String::with_capacity(body.len() + path.len() + 32);
    text.push_str(path);
    text.push('\n');
    if let Some(name) = symbol {
        text.push_str(name);
        text.push('\n');
    }
    text.push_str(cap_at_boundary(&body, CHUNK_MAX));
    Chunk {
        path: path.to_owned(),
        start_line: first.min(end).max(start),
        end_line: end,
        symbol: symbol.map(str::to_owned),
        text,
    }
}

fn windows(path: &str, lines: &[&str]) -> Vec<Chunk> {
    let mut out = Vec::new();
    let step = WINDOW_LINES - WINDOW_OVERLAP;
    let mut from = 0;
    while from < lines.len() {
        let to = (from + WINDOW_LINES).min(lines.len());
        let chunk = build(
            path,
            lines,
            from as u32 + 1,
            to as u32,
            None,
            from as u32 + 1,
        );
        if !chunk.text.lines().skip(1).all(|l| l.trim().is_empty()) {
            out.push(chunk);
        }
        if to == lines.len() {
            break;
        }
        from += step;
    }
    out
}

/// `text` cut to at most `max` bytes at a character boundary.
fn cap_at_boundary(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(name: &str, line: u32, end_line: u32) -> SymbolSpan {
        SymbolSpan {
            name: name.to_owned(),
            line,
            end_line,
        }
    }

    const SOURCE: &str = "\
//! module doc
use std::fmt;

/// Reads the retry limit.
/// Falls back to three.
pub fn retry_limit() -> u32 {
    3
}

struct Client {
    limit: u32,
}

impl Client {
    fn send(&self) {
        let _ = self.limit;
    }
}
";

    #[test]
    fn chunk_file_should_cut_at_symbols_with_the_doc_comment_above() {
        let symbols = [
            span("retry_limit", 6, 8),
            span("Client", 10, 12),
            span("Client", 14, 18),
            span("Client.send", 15, 17),
        ];
        let chunks = chunk_file("src/lib.rs", SOURCE, &symbols);
        assert_eq!(chunks.len(), 4);
        let first = &chunks[0];
        assert_eq!(first.symbol.as_deref(), Some("retry_limit"));
        assert_eq!((first.start_line, first.end_line), (6, 8));
        assert!(first.text.starts_with("src/lib.rs\nretry_limit\n/// Reads the retry limit.\n/// Falls back to three.\npub fn retry_limit"));
        // The container stops where its method starts.
        let container = chunks.iter().find(|c| c.start_line == 14).unwrap();
        assert_eq!(container.end_line, 14);
        assert!(!container.text.contains("self.limit"));
        let method = chunks
            .iter()
            .find(|c| c.symbol.as_deref() == Some("Client.send"))
            .unwrap();
        assert_eq!((method.start_line, method.end_line), (15, 17));
    }

    #[test]
    fn chunk_file_should_not_pull_a_neighbours_lines_into_a_doc_comment() {
        let text = "// a\nfn a() {}\n// b\nfn b() {}\n";
        let chunks = chunk_file("x.rs", text, &[span("a", 2, 2), span("b", 4, 4)]);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[1].text.contains("// b\nfn b"));
        assert!(!chunks[1].text.contains("fn a"));
    }

    #[test]
    fn chunk_file_should_cut_windows_of_a_file_with_no_symbols() {
        let text: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        let chunks = chunk_file("notes.md", &text, &[]);
        assert_eq!(
            chunks
                .iter()
                .map(|c| (c.start_line, c.end_line))
                .collect::<Vec<_>>(),
            vec![(1, 40), (35, 74), (69, 100)]
        );
        assert!(chunks.iter().all(|c| c.symbol.is_none()));
    }

    #[test]
    fn chunk_file_should_skip_what_is_too_big_or_binary_or_blank() {
        assert!(chunk_file("a.txt", &"x".repeat(FILE_MAX + 1), &[]).is_empty());
        assert!(chunk_file("a.bin", "ab\0cd", &[]).is_empty());
        assert!(chunk_file("a.txt", "", &[]).is_empty());
        assert!(chunk_file("a.txt", "\n\n   \n", &[]).is_empty());
        // A span past the end of the file is ignored, not a panic.
        assert!(chunk_file("a.rs", "fn a() {}\n", &[span("z", 9, 12)]).is_empty());
    }

    #[test]
    fn chunk_file_should_cap_a_long_symbol_at_a_character_boundary() {
        let body = "é".repeat(2000);
        let text = format!("fn big() {{\n{body}\n}}\n");
        let chunks = chunk_file("big.rs", &text, &[span("big", 1, 3)]);
        assert_eq!(chunks.len(), 1);
        let embedded = chunks[0].text.len() - "big.rs\nbig\n".len();
        assert!(embedded <= CHUNK_MAX, "{embedded}");
    }

    #[test]
    fn chunk_key_should_change_with_the_model_the_revision_and_the_text() {
        let base = chunk_key("m", 1, "text");
        assert_eq!(base, chunk_key("m", 1, "text"));
        assert_ne!(base, chunk_key("n", 1, "text"));
        assert_ne!(base, chunk_key("m", 2, "text"));
        assert_ne!(base, chunk_key("m", 1, "other"));
    }

    #[test]
    fn snippet_should_skip_the_path_and_symbol_header() {
        let chunks = chunk_file("src/lib.rs", SOURCE, &[span("retry_limit", 6, 8)]);
        assert_eq!(chunks[0].snippet(160), "/// Reads the retry limit.");
        assert_eq!(chunks[0].snippet(8), "/// Read");
    }
}
