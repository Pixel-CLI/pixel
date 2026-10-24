// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! A definition's **shape**: the declaration header up to the body opener,
//! whitespace-normalised (`specs/graph.md`). It is what a caller depends on:
//! a parameter added on a wrapped line changes it, an edit inside the body
//! does not. `diff_shapes` compares the symbols stored for a file with the
//! ones a fresh extraction gives, which is the check the edit hooks run.

use std::collections::BTreeMap;

use pixel_model::{Lang, Symbol, SymbolKind};

/// The longest shape kept, in characters; a header past it is cut at a
/// character boundary.
pub const SHAPE_MAX_CHARS: usize = 4096;

/// Where a language's declaration header ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderEnd {
    /// At the first `{` or `;` outside brackets (Rust, and the C family).
    Brace,
    /// At the end of the first line whose brackets are balanced and that
    /// does not continue (Ruby).
    Newline,
}

fn header_end(lang: Lang) -> HeaderEnd {
    match lang {
        Lang::Rust
        | Lang::TypeScript
        | Lang::Go
        | Lang::Java
        | Lang::CSharp
        | Lang::Php
        | Lang::C
        | Lang::Swift
        | Lang::Kotlin => HeaderEnd::Brace,
        // A colon or a `do` ends the line that opens the body.
        Lang::Ruby | Lang::Python | Lang::Elixir | Lang::Lua => HeaderEnd::Newline,
    }
}

/// The line comment opener of a language: text from it to the end of the
/// line is not part of the header.
fn comment_opener(lang: Lang) -> &'static str {
    match lang {
        Lang::Rust
        | Lang::TypeScript
        | Lang::Go
        | Lang::Java
        | Lang::CSharp
        | Lang::Php
        | Lang::C
        | Lang::Swift
        | Lang::Kotlin => "//",
        Lang::Ruby | Lang::Python | Lang::Elixir => "#",
        Lang::Lua => "--",
    }
}

/// The shape of the definition that spans `line..=end_line` (1-based) of
/// `content`. Empty for a symbol that has no header (`File`, `Package`).
pub fn shape_of(lang: Lang, kind: SymbolKind, content: &str, line: u32, end_line: u32) -> String {
    if matches!(kind, SymbolKind::File | SymbolKind::Package) {
        return String::new();
    }
    let first = line.saturating_sub(1) as usize;
    let count = end_line.saturating_sub(line) as usize + 1;
    let lines: Vec<&str> = content.split('\n').skip(first).take(count).collect();
    normalise(&header_text(lang, &lines).0)
}

/// How many lines the declaration header of the definition spanning
/// `line..=end_line` takes: the lines a change must touch to change the
/// symbol's shape.
pub fn header_lines(lang: Lang, content: &str, line: u32, end_line: u32) -> u32 {
    let first = line.saturating_sub(1) as usize;
    let count = end_line.saturating_sub(line) as usize + 1;
    let lines: Vec<&str> = content.split('\n').skip(first).take(count).collect();
    header_text(lang, &lines).1.max(1)
}

/// The raw header: the lines of the definition up to where its header ends.
fn header_text(lang: Lang, lines: &[&str]) -> (String, u32) {
    let opener = comment_opener(lang);
    let end = header_end(lang);
    let mut out = String::new();
    let mut depth: i32 = 0;
    for (used, raw) in (1u32..).zip(lines) {
        let code = strip_comment(raw.strip_suffix('\r').unwrap_or(raw), opener);
        let mut in_string = false;
        let mut escaped = false;
        for (at, c) in code.char_indices() {
            if in_string {
                match c {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => in_string = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => in_string = true,
                '(' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                '{' | ';' if end == HeaderEnd::Brace && depth <= 0 => {
                    out.push_str(&code[..at]);
                    return (out, used);
                }
                _ => {}
            }
        }
        out.push_str(code);
        out.push('\n');
        if end == HeaderEnd::Newline && depth <= 0 && !continues(code) {
            return (out, used);
        }
    }
    (out, lines.len() as u32)
}

/// Whether a line of a newline-terminated header runs on to the next one:
/// it ends in a comma, a backslash or an operator that wants a right side.
fn continues(code: &str) -> bool {
    let trimmed = code.trim_end();
    trimmed.ends_with([',', '\\', '|', '+', '-', '*', '/', '=', '<', '&'])
}

/// `code` without a line comment: the opener counts only outside a string.
fn strip_comment<'a>(code: &'a str, opener: &str) -> &'a str {
    let mut in_string = false;
    let mut escaped = false;
    for (at, c) in code.char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
        } else if c == '"' {
            in_string = true;
        } else if code[at..].starts_with(opener) {
            return &code[..at];
        }
    }
    code
}

/// Whitespace runs become one space, the ends are trimmed, and the text is
/// capped at [`SHAPE_MAX_CHARS`] characters.
fn normalise(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match joined.char_indices().nth(SHAPE_MAX_CHARS) {
        Some((at, _)) => joined[..at].to_owned(),
        None => joined,
    }
}

/// How a symbol's shape moved between two extractions of one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShapeChange {
    /// The symbol is still there with another shape.
    Reshaped {
        uid: String,
        old: String,
        new: String,
    },
    /// The symbol is gone and no symbol of the same shape appeared.
    Removed { uid: String, old: String },
    /// The symbol is gone and a symbol of the same shape appeared in the
    /// same file under another name: a rename.
    Renamed { uid: String, to: String },
}

impl ShapeChange {
    /// The uid of the symbol as it was stored.
    pub fn uid(&self) -> &str {
        match self {
            ShapeChange::Reshaped { uid, .. }
            | ShapeChange::Removed { uid, .. }
            | ShapeChange::Renamed { uid, .. } => uid,
        }
    }
}

/// The symbol's shape with the first occurrence of its own name replaced, so
/// that two symbols differing only in their name compare equal: the header
/// of a renamed function names it.
fn without_name(symbol: &Symbol) -> String {
    symbol
        .shape
        .replacen(pixel_model::leaf(&symbol.name), "\u{1}", 1)
}

/// The shape changes between the `stored` symbols of a file and the `fresh`
/// ones, sorted by the stored uid. A symbol with an empty shape has no
/// header and never changes one. Comparison is by uid (`path#name#kind`),
/// which carries no line number, so a symbol that only moved is unchanged.
pub fn diff_shapes(stored: &[Symbol], fresh: &[Symbol]) -> Vec<ShapeChange> {
    let fresh_by_uid: BTreeMap<&str, &Symbol> = fresh.iter().map(|s| (s.uid.as_str(), s)).collect();
    let stored_uids: std::collections::BTreeSet<&str> =
        stored.iter().map(|s| s.uid.as_str()).collect();
    let mut changes = Vec::new();
    for old in stored.iter().filter(|s| !s.shape.is_empty()) {
        match fresh_by_uid.get(old.uid.as_str()) {
            Some(new) if new.shape != old.shape => changes.push(ShapeChange::Reshaped {
                uid: old.uid.clone(),
                old: old.shape.clone(),
                new: new.shape.clone(),
            }),
            Some(_) => {}
            None => {
                // Added symbols are the fresh ones the store did not hold.
                let anonymous = without_name(old);
                let renamed_to = fresh
                    .iter()
                    .filter(|s| !stored_uids.contains(s.uid.as_str()))
                    .find(|s| s.kind == old.kind && without_name(s) == anonymous);
                changes.push(match renamed_to {
                    Some(to) => ShapeChange::Renamed {
                        uid: old.uid.clone(),
                        to: to.uid.clone(),
                    },
                    None => ShapeChange::Removed {
                        uid: old.uid.clone(),
                        old: old.shape.clone(),
                    },
                });
            }
        }
    }
    changes.sort_by(|a, b| a.uid().cmp(b.uid()));
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust(content: &str, line: u32, end: u32) -> String {
        shape_of(Lang::Rust, SymbolKind::Function, content, line, end)
    }

    fn ruby(content: &str, line: u32, end: u32) -> String {
        shape_of(Lang::Ruby, SymbolKind::Method, content, line, end)
    }

    #[test]
    fn a_rust_header_stops_at_the_body_opener() {
        let src = "pub fn total(items: &[u32], tax: f64) -> f64 {\n    0.0\n}\n";
        assert_eq!(
            rust(src, 1, 3),
            "pub fn total(items: &[u32], tax: f64) -> f64"
        );
    }

    #[test]
    fn a_wrapped_parameter_list_is_part_of_the_shape() {
        let before = "fn f(a: u32,\n     b: u32) -> u32 {\n    a + b\n}\n";
        let after = "fn f(a: u32,\n     b: u32,\n     c: u32) -> u32 {\n    a + b\n}\n";
        assert_eq!(rust(before, 1, 4), "fn f(a: u32, b: u32) -> u32");
        assert_eq!(rust(after, 1, 5), "fn f(a: u32, b: u32, c: u32) -> u32");
    }

    #[test]
    fn a_body_edit_leaves_the_shape_alone() {
        let one = "fn f(a: u32) -> u32 {\n    a + 1\n}\n";
        let two = "fn f(a: u32) -> u32 {\n    let b = a * 2;\n    b\n}\n";
        assert_eq!(rust(one, 1, 3), rust(two, 1, 4));
    }

    #[test]
    fn a_brace_inside_brackets_or_a_string_does_not_end_the_header() {
        let src = "fn f(a: [u8; { 3 }], s: &str) -> u32 where T: Fn() {\n    0\n}\n";
        assert_eq!(
            rust(src, 1, 3),
            "fn f(a: [u8; { 3 }], s: &str) -> u32 where T: Fn()"
        );
        let with_string = "fn g(sep: &str = \"{\") -> u32 {\n    0\n}\n";
        assert_eq!(rust(with_string, 1, 3), "fn g(sep: &str = \"{\") -> u32");
    }

    #[test]
    fn a_trailing_comment_is_not_part_of_the_header() {
        let src = "fn f(a: u32) -> u32 { // adds\n    a\n}\n";
        assert_eq!(rust(src, 1, 3), "fn f(a: u32) -> u32");
        let before_brace = "fn f(a: u32) // the input\n    -> u32 {\n    a\n}\n";
        assert_eq!(rust(before_brace, 1, 4), "fn f(a: u32) -> u32");
    }

    #[test]
    fn a_declaration_with_no_body_ends_at_its_semicolon() {
        let src = "    fn area(&self) -> f64;\n";
        assert_eq!(rust(src, 1, 1), "fn area(&self) -> f64");
    }

    #[test]
    fn a_ruby_header_is_its_first_balanced_line() {
        let src = "def charge(amount, currency: :usd)\n  amount\nend\n";
        assert_eq!(ruby(src, 1, 3), "def charge(amount, currency: :usd)");
        let wrapped = "def charge(amount,\n           currency: :usd)\n  amount\nend\n";
        assert_eq!(ruby(wrapped, 1, 4), "def charge(amount, currency: :usd)");
        let continued = "def charge amount,\n           currency\n  amount\nend\n";
        assert_eq!(ruby(continued, 1, 4), "def charge amount, currency");
    }

    #[test]
    fn a_ruby_comment_is_not_part_of_the_header() {
        let src = "class Invoice < Base # billing\n  x\nend\n";
        assert_eq!(
            shape_of(Lang::Ruby, SymbolKind::Class, src, 1, 3),
            "class Invoice < Base"
        );
    }

    #[test]
    fn a_file_or_package_has_no_shape() {
        for kind in [SymbolKind::File, SymbolKind::Package] {
            assert_eq!(shape_of(Lang::Rust, kind, "mod x;\n", 1, 1), "");
        }
    }

    #[test]
    fn a_long_header_is_cut_at_the_cap_on_a_character_boundary() {
        let params: String = (0..2000).map(|i| format!("é{i}: u8, ")).collect();
        let src = format!("fn f({params}) {{}}\n");
        let shape = rust(&src, 1, 1);
        assert_eq!(shape.chars().count(), SHAPE_MAX_CHARS);
        assert!(shape.starts_with("fn f(é0: u8,"));
    }

    #[test]
    fn crlf_and_tabs_normalise_like_spaces() {
        let src = "fn f(\r\n\ta: u32,\r\n) -> u32 {\r\n    a\r\n}\r\n";
        assert_eq!(rust(src, 1, 5), "fn f( a: u32, ) -> u32");
    }

    fn symbol(name: &str, shape: &str, kind: SymbolKind) -> Symbol {
        Symbol {
            uid: format!("a.rs#{name}#{kind}"),
            file: "a.rs".to_owned(),
            kind,
            name: name.to_owned(),
            signature: shape.to_owned(),
            shape: shape.to_owned(),
            line: 1,
            end_line: 1,
        }
    }

    fn function(name: &str, shape: &str) -> Symbol {
        symbol(name, shape, SymbolKind::Function)
    }

    #[test]
    fn an_unchanged_shape_is_no_change_even_when_the_symbol_moved() {
        let stored = [function("f", "fn f(a: u32)")];
        let mut moved = function("f", "fn f(a: u32)");
        moved.line = 40;
        moved.end_line = 50;
        assert_eq!(diff_shapes(&stored, &[moved]), []);
    }

    #[test]
    fn a_changed_shape_is_reshaped_with_both_texts() {
        let stored = [function("f", "fn f(a: u32)")];
        let fresh = [function("f", "fn f(a: u32, b: u32)")];
        assert_eq!(
            diff_shapes(&stored, &fresh),
            [ShapeChange::Reshaped {
                uid: "a.rs#f#function".to_owned(),
                old: "fn f(a: u32)".to_owned(),
                new: "fn f(a: u32, b: u32)".to_owned(),
            }]
        );
    }

    #[test]
    fn a_vanished_symbol_is_removed_and_one_with_the_same_shape_under_a_new_name_is_a_rename() {
        let stored = [
            function("old", "fn old(a: u32)"),
            function("keep", "fn keep()"),
        ];
        let removed = [function("keep", "fn keep()")];
        assert_eq!(
            diff_shapes(&stored, &removed),
            [ShapeChange::Removed {
                uid: "a.rs#old#function".to_owned(),
                old: "fn old(a: u32)".to_owned(),
            }]
        );
        let renamed = [
            function("keep", "fn keep()"),
            function("new", "fn new(a: u32)"),
        ];
        assert_eq!(
            diff_shapes(&stored, &renamed),
            [ShapeChange::Renamed {
                uid: "a.rs#old#function".to_owned(),
                to: "a.rs#new#function".to_owned(),
            }]
        );
    }

    #[test]
    fn a_rename_needs_the_same_kind_and_a_symbol_the_store_did_not_hold() {
        let stored = [function("old", "fn old(a: u32)")];
        let other_kind = [symbol("new", "fn new(a: u32)", SymbolKind::Method)];
        assert!(matches!(
            diff_shapes(&stored, &other_kind)[..],
            [ShapeChange::Removed { .. }]
        ));
        // `twin` already existed with that shape: it is not what `old` became.
        let stored = [function("old", "fn x()"), function("twin", "fn x()")];
        let fresh = [function("twin", "fn x()")];
        assert!(matches!(
            diff_shapes(&stored, &fresh)[..],
            [ShapeChange::Removed { .. }]
        ));
    }

    #[test]
    fn a_symbol_without_a_header_never_changes_shape() {
        let stored = [symbol("a.rs", "", SymbolKind::File)];
        assert_eq!(diff_shapes(&stored, &[]), []);
    }

    #[test]
    fn changes_come_out_sorted_by_the_stored_uid() {
        let stored = [function("b", "fn b()"), function("a", "fn a()")];
        let fresh = [function("b", "fn b(x: u8)"), function("a", "fn a(x: u8)")];
        let uids: Vec<_> = diff_shapes(&stored, &fresh)
            .iter()
            .map(|c| c.uid().to_owned())
            .collect();
        assert_eq!(uids, ["a.rs#a#function", "a.rs#b#function"]);
    }
}
