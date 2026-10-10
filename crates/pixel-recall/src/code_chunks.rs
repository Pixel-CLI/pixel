// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Where `pixel search-meaning` cuts a code file into the chunks it embeds
//! and scores lexically: along the file's symbols.
//!
//! The symbols come from `pixel-graph`'s tree-sitter extraction
//! ([`pixel_graph::extract::extract_file`]), the one that builds the call
//! graph, so every language the graph reads is cut by its functions, methods,
//! classes and modules. A symbol keeps the comment, attribute and decorator
//! lines right above it (its doc comment), so the prose that describes a
//! function is scored and embedded with it.
//!
//! The cut covers the whole file: the lines between symbols (imports,
//! top-level statements, a class's fields) become pieces of their own, so no
//! text leaves the search. Consecutive pieces are then packed together while
//! the chunk stays within [`PACK_MAX`] bytes, so a run of one-line constants
//! or accessors is not embedded line by line, while a function of any size
//! up to [`CHUNK_MAX`] is a chunk of its own, never cut and never diluted by
//! its neighbours. A symbol larger than [`CHUNK_MAX`] is cut along the
//! symbols nested in it, and one with none, like any piece still too large,
//! falls back to [`chunk_offsets`] windows over its bytes.
//!
//! A file the extraction does not read (an unsupported language, a
//! generated blob, a grammar failure) is cut into [`chunk_offsets`] windows,
//! as every file was before.

use crate::embed::{CHUNK_MAX, chunk_offsets};

/// Largest chunk that consecutive pieces are packed into, in bytes. A piece
/// larger than this is a chunk of its own.
///
/// Measured (2026-09-27, 45-query NL harness of #325 and the ten
/// `ndcg_relevance` qrels): packing up to [`CHUNK_MAX`] gave back most of
/// what symbol chunks win (r@1 32/45 against 39/45 at 400) and lost an
/// r@10 case on TypeScript; 250 to 600 bytes form a plateau (r@1 38 to 40,
/// r@10 43 of 45), and 400 is its middle.
pub const PACK_MAX: usize = 400;

/// The chunks of the file at the repository-relative `path` with contents
/// `text`, as byte ranges in file order. Together they cover every
/// non-blank line of `text`; ranges never overlap except between the
/// windows of one oversize piece, which overlap as [`chunk_offsets`] does.
pub fn code_chunks(path: &str, text: &str) -> Vec<(usize, usize)> {
    named_chunks(path, text)
        .into_iter()
        .map(|chunk| (chunk.start, chunk.end))
        .collect()
}

/// One chunk of [`named_chunks`]: its byte range, its inclusive 1-based line
/// range and the symbol it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedChunk {
    pub start: usize,
    pub end: usize,
    pub first_line: u32,
    pub last_line: u32,
    /// The name of the first symbol that starts inside the chunk (the
    /// outermost on a tie); for a window cut from inside a larger symbol,
    /// the innermost symbol around it. `None` for lines between symbols and
    /// for a file the extraction does not read.
    pub symbol: Option<String>,
}

/// [`code_chunks`] with the symbol each chunk belongs to, from the same
/// extraction.
pub fn named_chunks(path: &str, text: &str) -> Vec<NamedChunk> {
    let lines = Lines::new(text);
    let named = |start: usize, end: usize, symbol: Option<String>| {
        let first_line = lines.line_of(start);
        NamedChunk {
            start,
            end,
            first_line,
            last_line: lines.line_of(end.saturating_sub(1)).max(first_line),
            symbol,
        }
    };
    let Some(extraction) = pixel_graph::extract::extract_file(path, text.as_bytes()) else {
        return chunk_offsets(text)
            .into_iter()
            .map(|(start, end)| named(start, end, None))
            .collect();
    };
    let spans = symbol_spans(&extraction.symbols, lines.count());
    let mut pieces = Vec::new();
    cut(
        &lines,
        &spans,
        (1, lines.count()),
        comment_prefixes(extraction.lang),
        &mut pieces,
    );
    pack(text, &lines, &pieces)
        .into_iter()
        .map(|(start, end)| {
            let chunk = named(start, end, None);
            let symbol = chunk_symbol(&extraction.symbols, (chunk.first_line, chunk.last_line));
            NamedChunk { symbol, ..chunk }
        })
        .collect()
}

/// The name [`NamedChunk::symbol`] gives the chunk on `lines` (inclusive,
/// 1-based), among the extraction's `symbols`.
fn chunk_symbol(symbols: &[pixel_graph::extract::RawSymbol], lines: Span) -> Option<String> {
    let around = || {
        symbols.iter().filter(|symbol| {
            symbol.kind != pixel_graph::SymbolKind::Script
                && symbol.start_line <= lines.1
                && symbol.end_line >= lines.0
        })
    };
    around()
        .filter(|symbol| symbol.start_line >= lines.0)
        .min_by_key(|symbol| (symbol.start_line, std::cmp::Reverse(symbol.end_line)))
        .or_else(|| around().max_by_key(|symbol| symbol.start_line))
        .map(|symbol| symbol.name.clone())
}

/// An inclusive, 1-based line range.
type Span = (u32, u32);

/// Line starts of a text, for 1-based inclusive line ranges.
struct Lines<'a> {
    text: &'a str,
    /// Byte offset of each line's first byte; a final newline opens no line.
    starts: Vec<usize>,
}

impl<'a> Lines<'a> {
    fn new(text: &'a str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            text.match_indices('\n')
                .map(|(offset, _)| offset + 1)
                .filter(|&start| start < text.len()),
        );
        Self { text, starts }
    }

    /// The number of lines.
    fn count(&self) -> u32 {
        u32::try_from(self.starts.len()).unwrap_or(u32::MAX)
    }

    /// The bytes of lines `first..=last`, the last one's newline included.
    fn bytes(&self, first: u32, last: u32) -> (usize, usize) {
        let end = self
            .starts
            .get(last as usize)
            .copied()
            .unwrap_or(self.text.len());
        (self.starts[first as usize - 1], end)
    }

    /// The 1-based line holding the byte at `offset`.
    fn line_of(&self, offset: usize) -> u32 {
        let passed = self.starts.partition_point(|&start| start <= offset);
        u32::try_from(passed).unwrap_or(u32::MAX).max(1)
    }

    /// The text of line `line`, without its newline.
    fn line(&self, line: u32) -> &'a str {
        let (start, end) = self.bytes(line, line);
        self.text[start..end].trim_end_matches(['\n', '\r'])
    }
}

/// The line spans of `symbols` a chunk can follow, inside `1..=line_count`,
/// sorted by first line and, among equal first lines, outermost first;
/// each span once. The synthetic whole-file script symbol of a Ruby file is
/// no boundary and is left out.
fn symbol_spans(symbols: &[pixel_graph::extract::RawSymbol], line_count: u32) -> Vec<(u32, u32)> {
    let mut spans: Vec<(u32, u32)> = symbols
        .iter()
        .filter(|symbol| symbol.kind != pixel_graph::SymbolKind::Script)
        .filter(|symbol| (1..=line_count).contains(&symbol.start_line))
        .map(|symbol| {
            (
                symbol.start_line,
                symbol.end_line.clamp(symbol.start_line, line_count),
            )
        })
        .collect();
    spans.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    spans.dedup();
    spans
}

/// The outermost spans of `spans` (sorted as [`symbol_spans`] returns them),
/// each with the spans nested in it. Two spans that overlap without nesting
/// become one.
fn outermost(spans: &[Span]) -> Vec<(Span, &[Span])> {
    // (first line, last line, index of the first nested span, index past the last)
    let mut groups: Vec<(u32, u32, usize, usize)> = Vec::new();
    for (index, &(first, last)) in spans.iter().enumerate() {
        match groups.last_mut() {
            Some(group) if first <= group.1 => {
                group.1 = group.1.max(last);
                group.3 = index + 1;
            }
            _ => groups.push((first, last, index + 1, index + 1)),
        }
    }
    groups
        .into_iter()
        .map(|(first, last, from, to)| ((first, last), &spans[from..to]))
        .collect()
}

/// Cut lines `range` (inclusive), which holds `spans`, into pieces in line
/// order that cover it exactly: each outermost symbol with its doc comment,
/// and the lines between symbols. A symbol larger than [`CHUNK_MAX`] with
/// symbols nested in it is cut along them instead.
fn cut(
    lines: &Lines,
    spans: &[(u32, u32)],
    range: (u32, u32),
    comments: &[&str],
    pieces: &mut Vec<(u32, u32)>,
) {
    let mut next = range.0;
    for ((first, last), nested) in outermost(spans) {
        let first = doc_start(lines, first, next, comments);
        if first > next {
            pieces.push((next, first - 1));
        }
        let (start, end) = lines.bytes(first, last);
        if end - start > CHUNK_MAX && !nested.is_empty() {
            cut(lines, nested, (first, last), comments, pieces);
        } else {
            pieces.push((first, last));
        }
        next = last + 1;
    }
    if next <= range.1 {
        pieces.push((next, range.1));
    }
}

/// The first line of the symbol starting at `first` once the comment lines
/// right above it are attached, never above `floor`: the contiguous run of
/// lines that start with one of `comments` (a blank line ends it).
fn doc_start(lines: &Lines, first: u32, floor: u32, comments: &[&str]) -> u32 {
    let above = (floor..first)
        .rev()
        .take_while(|&line| is_comment(lines.line(line), comments))
        .count();
    first - u32::try_from(above).unwrap_or(0)
}

fn is_comment(line: &str, comments: &[&str]) -> bool {
    let line = line.trim_start();
    comments.iter().any(|prefix| line.starts_with(prefix))
}

/// The prefixes of the lines that document the symbol below them in the
/// language `lang` ([`pixel_graph::extract::lang_of`]): comments, and the
/// attributes, annotations and decorators written between a doc comment and
/// its symbol.
fn comment_prefixes(lang: &str) -> &'static [&'static str] {
    match lang {
        "rust" => &["//", "/*", "*", "#["],
        "python" => &["#", "@"],
        "ruby" => &["#"],
        "elixir" => &["#", "@"],
        "lua" => &["--"],
        // TypeScript, JavaScript, Go, Java, C#, PHP, C, Swift. C#'s
        // `[Attribute]` lines need no prefix: its grammar puts them inside
        // the declaration, so they are already in the symbol's span.
        _ => &["//", "/*", "*", "@"],
    }
}

/// The byte ranges of `pieces` (line ranges in order) packed into chunks:
/// consecutive pieces share a chunk while it stays within [`PACK_MAX`]
/// bytes; a larger piece is a chunk of its own, cut into [`chunk_offsets`]
/// windows when it exceeds [`CHUNK_MAX`] (a packed chunk never does, since
/// `PACK_MAX < CHUNK_MAX`). Blank chunks are dropped.
fn pack(text: &str, lines: &Lines, pieces: &[(u32, u32)]) -> Vec<(usize, usize)> {
    let mut packed = Vec::new();
    let mut open: Option<(usize, usize)> = None;
    for &(first, last) in pieces {
        let (start, end) = lines.bytes(first, last);
        match open {
            Some((chunk_start, _)) if end - chunk_start <= PACK_MAX => {
                open = Some((chunk_start, end));
            }
            _ => packed.extend(open.replace((start, end))),
        }
    }
    packed.extend(open);
    packed
        .into_iter()
        .flat_map(|(start, end)| {
            chunk_offsets(&text[start..end])
                .into_iter()
                .map(move |(from, to)| (start + from, start + to))
        })
        .filter(|&(start, end)| !text[start..end].trim().is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts<'a>(text: &'a str, chunks: &[(usize, usize)]) -> Vec<&'a str> {
        chunks
            .iter()
            .map(|&(start, end)| &text[start..end])
            .collect()
    }

    /// The pieces [`code_chunks`] packs for `path`, as line ranges.
    fn pieces(path: &str, text: &str) -> Vec<(u32, u32)> {
        let extraction = pixel_graph::extract::extract_file(path, text.as_bytes()).unwrap();
        let lines = Lines::new(text);
        let mut pieces = Vec::new();
        cut(
            &lines,
            &symbol_spans(&extraction.symbols, lines.count()),
            (1, lines.count()),
            comment_prefixes(extraction.lang),
            &mut pieces,
        );
        pieces
    }

    /// Every non-blank line of `text` lies inside some chunk.
    fn assert_covers(path: &str, text: &str) {
        let chunks = code_chunks(path, text);
        let mut offset = 0;
        for line in text.split_inclusive('\n') {
            let (start, end) = (offset, offset + line.len());
            offset = end;
            if line.trim().is_empty() {
                continue;
            }
            assert!(
                chunks.iter().any(|&(from, to)| from <= start && end <= to)
                    || chunks.iter().any(|&(from, to)| from <= start && start < to)
                        && chunks.iter().any(|&(from, to)| from < end && end <= to),
                "{path}: line {line:?} is in no chunk of {chunks:?}"
            );
        }
    }

    /// A doc comment and the attributes, annotations or decorators under it
    /// are cut with the symbol they describe, in every language the graph
    /// reads, and a blank line or a line of code ends the run: `use`,
    /// `import` and `package` lines stay out.
    #[test]
    fn doc_comments_are_cut_with_their_symbol_in_every_language() {
        let cases: [(&str, &[&str], &[Span]); 8] = [
            (
                "src/lib.rs",
                &[
                    "use std::fmt;",
                    "/// Bills the parking.",
                    "#[inline]",
                    "fn bill() {",
                    "    let total = 1;",
                    "}",
                    "const RATE: u32 = 2;",
                    "",
                    "struct Meter {",
                    "    seconds: u32,",
                    "}",
                ],
                &[(1, 1), (2, 6), (7, 7), (8, 8), (9, 11)],
            ),
            (
                "src/billing.ts",
                &[
                    "import x from 'y';",
                    "/**",
                    " * Bills.",
                    " */",
                    "@Injectable()",
                    "export class Billing {",
                    "  total(): number { return 1; }",
                    "}",
                ],
                &[(1, 1), (2, 8)],
            ),
            (
                "src/bill.js",
                &[
                    "const x = 1;",
                    "// Bills.",
                    "function bill() {",
                    "  return 1;",
                    "}",
                ],
                &[(1, 1), (2, 5)],
            ),
            (
                "app/billing.rb",
                &[
                    "require 'x'",
                    "# Charges a card.",
                    "class Billing",
                    "  def total",
                    "    1",
                    "  end",
                    "end",
                ],
                &[(1, 1), (2, 7)],
            ),
            (
                "bill.py",
                &[
                    "import os",
                    "# Bills.",
                    "@cached",
                    "def bill():",
                    "    return 1",
                ],
                &[(1, 1), (2, 5)],
            ),
            (
                "Billing.cs",
                &[
                    "using System;",
                    "/// <summary>Bills.</summary>",
                    "[Serializable]",
                    "public class Billing {",
                    "  public int Total() { return 1; }",
                    "}",
                ],
                &[(1, 1), (2, 6)],
            ),
            (
                "bill.go",
                &[
                    "package main",
                    "// Bill bills.",
                    "func Bill() int {",
                    "  return 1",
                    "}",
                ],
                &[(1, 1), (2, 5)],
            ),
            (
                "bill.lua",
                &[
                    "local x = 1",
                    "-- Bills.",
                    "local function bill()",
                    "  return 1",
                    "end",
                ],
                &[(1, 1), (2, 5)],
            ),
        ];
        for (path, lines, expected) in cases {
            let text = lines.join("\n");
            assert_eq!(pieces(path, &text), expected, "{path}");
        }
        // The graph's generic walker finds no Elixir symbol in a fixture
        // this small; the prefixes are still the language's.
        assert_eq!(comment_prefixes("elixir"), ["#", "@"]);
    }

    /// A function `bytes` long, its body padded with statements.
    fn function(name: &str, bytes: usize) -> String {
        let mut text = format!("fn {name}() {{\n");
        while text.len() + 2 < bytes {
            let pad = (bytes - 2 - text.len()).min(40);
            text.push_str(&format!("{:<1$}\n", "//", pad.max(3) - 1));
        }
        text.push_str("}\n");
        text
    }

    /// The doc comment travels with the function it describes even where
    /// packing would have put it with the function before: `a` and the
    /// doc comment fit in one chunk, the three together do not. Cut without
    /// the attachment, the comment is packed into `a`'s chunk and `b` is
    /// embedded without its description.
    #[test]
    fn a_doc_comment_lands_in_its_function_chunk_not_the_previous_one() {
        let a = function("apply_rate", 330);
        let doc = "/// Settles the ledger at the end of the day.\n";
        let b = "fn settle() {\n    run();\n}\n";
        assert!(a.len() + doc.len() <= PACK_MAX, "a and the doc fit");
        assert!(a.len() + doc.len() + b.len() > PACK_MAX, "not with b");
        let text = format!("{a}{doc}{b}");
        let chunks = code_chunks("src/ledger.rs", &text);
        assert_eq!(texts(&text, &chunks), [a.clone(), format!("{doc}{b}")]);
    }

    /// A language the graph does not read (Kotlin, shell) is cut into the
    /// windows every file had before; the same bytes in a Rust file are cut
    /// along their functions, so the path, not the text, decides.
    #[test]
    fn a_file_in_an_unsupported_language_falls_back_to_windows() {
        let text: String = (0..200)
            .map(|index| format!("fn handler_{index:03}() {{}}\n"))
            .collect();
        assert!(text.len() > CHUNK_MAX);
        assert_eq!(code_chunks("App.kt", &text), chunk_offsets(&text));
        assert_eq!(code_chunks("deploy.sh", &text), chunk_offsets(&text));
        let symbols = code_chunks("src/handlers.rs", &text);
        assert_ne!(symbols, chunk_offsets(&text));
        assert!(symbols.iter().all(|&(start, end)| end - start <= PACK_MAX));
    }

    /// A file tree-sitter is not pointed at (a generated, minified blob) or
    /// cannot read is not lost: it is cut into windows, like before.
    #[test]
    fn an_unparsed_file_falls_back_to_windows_without_losing_text() {
        let blob = "var a=1;".repeat(9_000);
        assert!(pixel_graph::extract::is_generated_blob(blob.as_bytes()));
        assert_eq!(code_chunks("dist/app.js", &blob), chunk_offsets(&blob));
        // Broken syntax still parses (tree-sitter recovers); whatever it
        // makes of it, every line stays in a chunk.
        let broken = format!(
            "fn ok() {{}}\nfn broken( {{ let = ;\n{}\n}}\n",
            "x ".repeat(900)
        );
        assert_covers("src/broken.rs", &broken);
    }

    /// A function larger than a chunk becomes windows over its own bytes,
    /// with the lines before it a chunk of their own; blank lines between
    /// two such functions make no chunk.
    #[test]
    fn an_oversize_symbol_is_split_into_windows_of_its_own_bytes() {
        let big = function("reconcile", 4_000);
        let other = function("replay", 1_800);
        let text = format!("use std::fmt;\n{big}\n\n\n{other}");
        let head = "use std::fmt;\n".len();
        let shifted = |from: usize, body: &str| {
            chunk_offsets(body)
                .into_iter()
                .map(move |(start, end)| (from + start, from + end))
        };
        let mut expected = vec![(0, head)];
        expected.extend(shifted(head, &big));
        expected.extend(shifted(head + big.len() + 3, &other));
        assert_eq!(code_chunks("src/sync.rs", &text), expected);
    }

    /// An `impl` larger than a chunk (the graph records its methods, not
    /// the block) is cut along its methods: each method, doc comment
    /// included, is exactly one chunk, never windows.
    #[test]
    fn an_oversize_impl_is_cut_along_its_methods() {
        let method = |name: &str| {
            format!(
                "    /// Doc of {name}.\n{}",
                function(name, 600)
                    .lines()
                    .map(|line| format!("    {line}\n"))
                    .collect::<String>()
            )
        };
        let methods = [method("open"), method("close"), method("reset")];
        let text = format!("impl Meter {{\n{}}}\n", methods.concat());
        assert!(text.len() > CHUNK_MAX);
        let chunks = code_chunks("src/meter.rs", &text);
        let cut = texts(&text, &chunks);
        for method in &methods {
            assert!(cut.contains(&method.as_str()), "{method:?} in {cut:?}");
        }
        assert_eq!(cut.len(), 5, "the header, three methods, the closing brace");
        assert_covers("src/meter.rs", &text);
    }

    /// A symbol of exactly [`CHUNK_MAX`] bytes stays one piece, nested
    /// symbols or not; one byte more and it is cut along them.
    #[test]
    fn a_symbol_is_cut_along_its_nested_ones_only_above_chunk_max() {
        let module_of = |bytes: usize| {
            let methods = format!("{}{}", function("open", 600), function("close", 600));
            let head = "mod meter {\n";
            let pad = bytes - head.len() - methods.len() - "}\n".len();
            format!("{head}{}\n{methods}}}\n", "/".repeat(pad - 1))
        };
        let exact = module_of(CHUNK_MAX);
        assert_eq!(exact.len(), CHUNK_MAX);
        let lines = u32::try_from(exact.lines().count()).unwrap();
        assert_eq!(pieces("src/meter.rs", &exact), [(1, lines)]);
        let over = module_of(CHUNK_MAX + 1);
        let cut = pieces("src/meter.rs", &over);
        assert_eq!(cut.len(), 4, "head, two methods, closing brace: {cut:?}");
    }

    /// A small symbol with nested ones stays one piece wherever it sits:
    /// the size compared with [`CHUNK_MAX`] is its own, not its offset.
    #[test]
    fn a_small_symbol_deep_in_a_file_is_not_cut_along_its_nested_ones() {
        let head = function("pad", 1_600);
        let text = format!("{head}mod meter {{\n    fn open() {{}}\n}}\n");
        let first = u32::try_from(head.lines().count()).unwrap();
        assert_eq!(
            pieces("src/meter.rs", &text),
            [(1, first), (first + 1, first + 3)]
        );
    }

    /// Imports, top-level statements and a class's fields are in chunks
    /// too: nothing between the symbols leaves the search.
    #[test]
    fn text_between_symbols_stays_in_a_chunk() {
        let big = function("first", 1_400);
        let rust = format!(
            "use crate::ledger;\n{big}static TOP_LEVEL_MARKER: u8 = 0;\nlet_it_be!();\n{big}"
        );
        assert_covers("src/lib.rs", &rust);
        let chunks = code_chunks("src/lib.rs", &rust);
        assert!(
            texts(&rust, &chunks)
                .iter()
                .any(|chunk| chunk.contains("let_it_be!();")),
            "a macro call between two functions"
        );
        let ruby = format!(
            "require 'json'\nclass A\n  attr_reader :x\n{}\nend\nputs 'top level'\n",
            (0..40)
                .map(|i| format!("  def m{i}; {i}; end\n"))
                .collect::<String>()
        );
        assert_covers("lib/a.rb", &ruby);
        let python = format!(
            "import os\nTOP = 1\n{}\nif __name__ == '__main__':\n    main()\n",
            (0..40)
                .map(|i| format!("def f{i}():\n    return {i}\n\n"))
                .collect::<String>()
        );
        assert_covers("tool.py", &python);
        let ts = format!(
            "import x from 'y';\nconst top = 1;\n{}\nexport default top;\n",
            (0..40)
                .map(|i| format!("function f{i}() {{ return {i}; }}\n"))
                .collect::<String>()
        );
        assert_covers("src/a.ts", &ts);
    }

    /// Consecutive pieces share a chunk up to exactly [`PACK_MAX`] bytes,
    /// wherever they sit in the file; a piece of exactly [`CHUNK_MAX`] bytes
    /// is one chunk, one byte more is windows.
    #[test]
    fn pack_should_merge_up_to_pack_max_and_window_above_chunk_max() {
        let line = |bytes: usize| format!("{}\n", "x".repeat(bytes - 1));
        let text = [
            line(200),
            line(200),
            line(2),
            line(CHUNK_MAX),
            line(CHUNK_MAX + 1),
            line(150),
            line(250),
        ]
        .concat();
        let lines = Lines::new(&text);
        let pieces = [(1, 1), (2, 2), (3, 3), (4, 4), (5, 5), (6, 6), (7, 7)];
        let chunks = pack(&text, &lines, &pieces);
        let big = 402 + CHUNK_MAX;
        let tail = big + CHUNK_MAX + 1;
        let mut expected = vec![(0, 400), (400, 402), (402, big)];
        expected.extend(
            chunk_offsets(&text[big..tail])
                .into_iter()
                .map(|(start, end)| (big + start, big + end)),
        );
        // Far from the top of the file, two small pieces still pack, up to
        // exactly `PACK_MAX` bytes.
        expected.push((tail, tail + 400));
        assert_eq!(chunks, expected);
        assert_eq!(expected.len(), 6, "the oversize line is two windows");
    }

    #[test]
    fn lines_index_every_line_and_no_phantom_one_after_a_final_newline() {
        let lines = Lines::new("ab\r\ncd\nef");
        assert_eq!(lines.count(), 3);
        assert_eq!(lines.bytes(1, 1), (0, 4));
        assert_eq!(lines.bytes(2, 3), (4, 9));
        assert_eq!(lines.line(1), "ab");
        assert_eq!(lines.line(3), "ef");
        let ended = Lines::new("ab\ncd\n");
        assert_eq!(ended.count(), 2);
        assert_eq!(ended.bytes(2, 2), (3, 6));
    }

    /// The comment run above a symbol stops at `floor` (the line after the
    /// previous piece), at a blank line and at code, and counts indented
    /// comments.
    #[test]
    fn doc_start_climbs_comment_lines_down_to_the_floor_only() {
        let lines = Lines::new("// a\n// b\n// c\n    // d\nfn x() {}\n");
        assert_eq!(doc_start(&lines, 5, 1, &["//"]), 1);
        assert_eq!(doc_start(&lines, 5, 3, &["//"]), 3, "never above the floor");
        assert_eq!(doc_start(&lines, 5, 5, &["//"]), 5);
        assert_eq!(
            doc_start(&lines, 5, 1, &["#"]),
            5,
            "not this language's comment"
        );
        let gapped = Lines::new("// a\n\n// b\nfn x() {}\n");
        assert_eq!(
            doc_start(&gapped, 4, 1, &["//"]),
            3,
            "a blank line ends the run"
        );
    }

    fn symbol(
        kind: pixel_graph::SymbolKind,
        start_line: u32,
        end_line: u32,
    ) -> pixel_graph::extract::RawSymbol {
        pixel_graph::extract::RawSymbol {
            name: "s".into(),
            qualified: "s".into(),
            kind,
            start_line,
            end_line,
            sig: String::new(),
            trait_impl: false,
            module_decl: false,
        }
    }

    /// Spans are sorted outermost first, each once, clamped to the file;
    /// the Ruby file-scope script symbol and spans outside the file are no
    /// boundaries.
    #[test]
    fn symbol_spans_sort_dedup_clamp_and_drop_the_script_scope() {
        use pixel_graph::SymbolKind::{Class, Function, Method, Script};
        let symbols = [
            symbol(Method, 3, 4),
            symbol(Class, 2, 9),
            symbol(Script, 1, 10),
            symbol(Function, 3, 4),
            symbol(Function, 3, 12),
            symbol(Function, 0, 2),
            symbol(Function, 11, 11),
            symbol(Function, 6, 5),
        ];
        assert_eq!(
            symbol_spans(&symbols, 10),
            [(2, 9), (3, 10), (3, 4), (6, 6)]
        );
    }

    /// Nested spans stay under their outermost one; two that overlap
    /// without nesting become one span.
    #[test]
    fn outermost_groups_nested_spans_and_merges_overlaps() {
        let spans = [(1, 10), (2, 3), (5, 6), (12, 14), (13, 20), (22, 22)];
        let groups = outermost(&spans);
        assert_eq!(
            groups,
            [
                ((1, 10), &spans[1..3]),
                ((12, 20), &spans[4..5]),
                ((22, 22), &spans[6..6]),
            ]
        );
    }

    fn named(
        name: &str,
        kind: pixel_graph::SymbolKind,
        start_line: u32,
        end_line: u32,
    ) -> pixel_graph::extract::RawSymbol {
        pixel_graph::extract::RawSymbol {
            name: name.into(),
            ..symbol(kind, start_line, end_line)
        }
    }

    /// A chunk takes the name of the first symbol that starts inside it,
    /// the outermost on a tie, however many others it holds or touches.
    #[test]
    fn chunk_symbol_should_name_the_first_symbol_starting_in_the_chunk() {
        use pixel_graph::SymbolKind::{Class, Function, Method};
        let symbols = [
            named("Outer", Class, 2, 9),
            named("inner", Method, 3, 4),
            named("later", Function, 5, 9),
        ];
        assert_eq!(chunk_symbol(&symbols, (1, 12)).as_deref(), Some("Outer"));
        assert_eq!(chunk_symbol(&symbols, (3, 4)).as_deref(), Some("inner"));
        // Starting on the chunk's last line counts; the one enclosing it does not.
        assert_eq!(chunk_symbol(&symbols, (3, 5)).as_deref(), Some("inner"));
        assert_eq!(chunk_symbol(&symbols, (4, 5)).as_deref(), Some("later"));
    }

    /// A window cut from inside a symbol has no symbol starting in it and
    /// takes the innermost one around it; a chunk between symbols has none.
    #[test]
    fn chunk_symbol_should_name_the_innermost_enclosing_symbol_or_none() {
        use pixel_graph::SymbolKind::{Class, Function, Method, Script};
        let symbols = [
            named("Outer", Class, 2, 9),
            named("inner", Method, 5, 8),
            named("script", Script, 1, 20),
        ];
        assert_eq!(chunk_symbol(&symbols, (6, 7)).as_deref(), Some("inner"));
        assert_eq!(chunk_symbol(&symbols, (9, 9)).as_deref(), Some("Outer"));
        assert_eq!(chunk_symbol(&symbols, (10, 12)), None, "between symbols");
        assert_eq!(
            chunk_symbol(&symbols, (1, 1)),
            None,
            "only the script scope"
        );
        let touching = [named("f", Function, 1, 3)];
        assert_eq!(chunk_symbol(&touching, (3, 5)).as_deref(), Some("f"));
        assert_eq!(chunk_symbol(&touching, (4, 5)), None);
    }

    /// Each chunk reports its inclusive lines and its symbol: imports above
    /// the first function have none, a function with its doc comment has its
    /// own name, and the windows of one long function all have it.
    #[test]
    fn named_chunks_should_give_each_chunk_its_lines_and_symbol() {
        let padding = |count: usize| -> String {
            (0..count)
                .map(|n| format!("    let value_{n} = {n}; // padding padding padding\n"))
                .collect()
        };
        let text = format!(
            "use std::fmt;\n\n/// Bills the parking.\npub fn bill() -> u32 {{\n{}    0\n}}\n\npub fn long() -> u32 {{\n{}    0\n}}\n",
            padding(8),
            padding(70)
        );
        let chunks = named_chunks("src/lib.rs", &text);
        let lines = Lines::new(&text);
        let summary: Vec<(u32, u32, Option<&str>)> = chunks
            .iter()
            .map(|chunk| (chunk.first_line, chunk.last_line, chunk.symbol.as_deref()))
            .collect();
        assert_eq!(
            summary[0],
            (1, 2, None),
            "the import and the blank line stay apart from the function: {summary:?}"
        );
        assert_eq!(summary[1], (3, 14, Some("bill")), "{summary:?}");
        assert_eq!(
            chunks[1].start,
            lines.bytes(3, 3).0,
            "the doc comment opens the chunk"
        );
        let windows = &summary[2..];
        assert!(windows.len() >= 3, "a long function is cut: {summary:?}");
        assert!(
            windows.iter().all(|window| window.2 == Some("long")),
            "{summary:?}"
        );
        assert_eq!(windows.last().unwrap().1, lines.count(), "{summary:?}");
        assert!(
            windows.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "windows advance: {summary:?}"
        );
        assert_eq!(
            code_chunks("src/lib.rs", &text),
            chunks
                .iter()
                .map(|chunk| (chunk.start, chunk.end))
                .collect::<Vec<_>>(),
            "the same cut as code_chunks"
        );
    }

    /// A file the extraction does not read is cut into windows with lines
    /// and no symbol.
    #[test]
    fn named_chunks_should_cut_an_unparsed_file_into_windows_without_symbols() {
        let text: String = (1..=120)
            .map(|n| format!("line {n} of the manual with some padding words\n"))
            .collect();
        let chunks = named_chunks("docs/manual.md", &text);
        assert!(chunks.len() >= 3, "{chunks:?}");
        assert!(chunks.iter().all(|chunk| chunk.symbol.is_none()));
        assert_eq!(chunks[0].first_line, 1);
        assert_eq!(chunks.last().unwrap().last_line, 120);
    }
}
