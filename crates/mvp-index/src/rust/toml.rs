// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The part of TOML a `Cargo.toml` reader needs, without a TOML crate
//! (ADR-0013 allows none in `mvp-index`): table headers, `key = value`
//! statements with dotted and quoted keys, single-line strings, booleans
//! and one level of inline table. Every other value (numbers, dates,
//! arrays, multi-line strings, nested tables) is skipped as a whole, across
//! lines, so no text inside a string or an array is ever read as a key
//! (D41). The scanner is a loop over bytes: no nesting costs the stack.
//! Keys and strings are borrowed from the text unless they hold an
//! escape: a manifest may declare thousands of dependencies.

use std::borrow::Cow;

/// A dotted key's parts.
pub type Key<'t> = Vec<Cow<'t, str>>;

/// One statement of a manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Statement<'t> {
    /// `[a.b]`, or `[[a.b]]` (`array`). A header the scanner cannot read
    /// has an empty path, so what follows it belongs to no table it knows.
    Table { path: Key<'t>, array: bool },
    /// `a.b = value`, at `line` (1-based).
    Entry {
        key: Key<'t>,
        value: Value<'t>,
        line: u32,
    },
}

/// A value, as far as the reader needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value<'t> {
    /// A basic or literal string on one line.
    Str(Cow<'t, str>),
    Bool(bool),
    /// `{ k = v, … }`: its entries, whose values are never tables.
    Table(Vec<(Key<'t>, Value<'t>)>),
    /// Anything else, or a value the scanner cannot read.
    Other,
}

/// The statements of `text`, in order. A line the scanner cannot read is
/// skipped.
pub fn statements(text: &str) -> Vec<Statement<'_>> {
    let mut scanner = Scanner {
        text,
        b: text.as_bytes(),
        i: 0,
        line: 1,
    };
    let mut out = Vec::new();
    loop {
        scanner.blank();
        let Some(c) = scanner.peek() else { break };
        let line = scanner.line;
        if c == b'[' {
            let array = scanner.starts(b"[[");
            scanner.i += if array { 2 } else { 1 };
            let path = scanner
                .key()
                .filter(|_| scanner.close(if array { b"]]" } else { b"]" }))
                .filter(|_| scanner.line_end())
                .unwrap_or_default();
            scanner.rest_of_line();
            out.push(Statement::Table { path, array });
            continue;
        }
        let Some(key) = scanner.key() else {
            scanner.rest_of_line();
            continue;
        };
        scanner.spaces();
        if scanner.peek() != Some(b'=') {
            scanner.rest_of_line();
            continue;
        }
        scanner.i += 1;
        scanner.spaces();
        let value = scanner.value(true);
        if scanner.line_end() {
            out.push(Statement::Entry { key, value, line });
        }
        scanner.rest_of_line();
    }
    out
}

struct Scanner<'a> {
    text: &'a str,
    b: &'a [u8],
    i: usize,
    line: u32,
}

fn bare(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

impl<'a> Scanner<'a> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn starts(&self, s: &[u8]) -> bool {
        self.b[self.i..].starts_with(s)
    }

    /// Moves past one byte, counting lines.
    fn bump(&mut self) {
        if self.peek() == Some(b'\n') {
            self.line += 1;
        }
        self.i += 1;
    }

    fn spaces(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.i += 1;
        }
    }

    /// Blank lines, white space and comments, before a statement or inside
    /// an array or an inline table.
    fn blank(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n') => self.bump(),
                Some(b'#') => {
                    while self.peek().is_some_and(|c| c != b'\n') {
                        self.i += 1;
                    }
                }
                _ => return,
            }
        }
    }

    /// Past the end of the current line, whatever is on it.
    fn rest_of_line(&mut self) {
        while let Some(c) = self.peek() {
            self.bump();
            if c == b'\n' {
                return;
            }
        }
    }

    /// True when only white space or a comment is left on the line.
    fn line_end(&mut self) -> bool {
        self.spaces();
        match self.peek() {
            None | Some(b'\n') => true,
            Some(b'\r') => self.b.get(self.i + 1).is_none_or(|c| *c == b'\n'),
            Some(b'#') => true,
            Some(_) => false,
        }
    }

    fn close(&mut self, closer: &[u8]) -> bool {
        self.spaces();
        if self.starts(closer) {
            self.i += closer.len();
            true
        } else {
            false
        }
    }

    /// A dotted key: bare, `"basic"` or `'literal'` parts.
    fn key(&mut self) -> Option<Key<'a>> {
        let mut parts = Vec::new();
        loop {
            self.spaces();
            let part = match self.peek()? {
                b'"' => {
                    self.i += 1;
                    self.basic()?
                }
                b'\'' => {
                    self.i += 1;
                    self.literal()?
                }
                c if bare(c) => {
                    let start = self.i;
                    while self.peek().is_some_and(bare) {
                        self.i += 1;
                    }
                    // ASCII: the text's own characters.
                    Cow::Borrowed(&self.text[start..self.i])
                }
                _ => return None,
            };
            parts.push(part);
            self.spaces();
            if self.peek() != Some(b'.') {
                return Some(parts);
            }
            self.i += 1;
        }
    }

    /// A basic string's content, its opening quote consumed, its escapes
    /// read as TOML reads them: `\b \t \n \f \r \" \\`, `\uHHHH` and
    /// `\UHHHHHHHH`, and TOML 1.1's `\e` and `\xHH`. `None` at a line's
    /// end, and for an escape TOML does not have or a code that is no
    /// Unicode scalar value, which make cargo reject the manifest.
    fn basic(&mut self) -> Option<Cow<'a, str>> {
        // Borrowed up to its first escape.
        let start = self.i;
        while let Some(c) = self.peek() {
            match c {
                b'"' => {
                    self.i += 1;
                    return Some(Cow::Borrowed(&self.text[start..self.i - 1]));
                }
                b'\\' => break,
                b'\n' => return None,
                _ => self.i += 1,
            }
        }
        let mut out = self.b[start..self.i].to_vec();
        loop {
            match self.peek()? {
                b'"' => {
                    self.i += 1;
                    return Some(Cow::Owned(String::from_utf8(out).unwrap_or_else(
                        |invalid| String::from_utf8_lossy(invalid.as_bytes()).into_owned(),
                    )));
                }
                b'\n' => return None,
                b'\\' => {
                    self.i += 1;
                    let escaped = match self.peek()? {
                        b'b' => '\u{8}',
                        b't' => '\t',
                        b'n' => '\n',
                        b'f' => '\u{c}',
                        b'r' => '\r',
                        b'e' => '\u{1b}',
                        b'"' => '"',
                        b'\\' => '\\',
                        c @ (b'x' | b'u' | b'U') => {
                            let digits = match c {
                                b'x' => 2,
                                b'u' => 4,
                                _ => 8,
                            };
                            let hex = self.b.get(self.i + 1..self.i + 1 + digits)?;
                            if !hex.iter().all(u8::is_ascii_hexdigit) {
                                return None;
                            }
                            let code = hex.iter().fold(0u32, |code, digit| {
                                code * 16 + char::from(*digit).to_digit(16).unwrap_or(0)
                            });
                            self.i += digits;
                            char::from_u32(code)?
                        }
                        _ => return None,
                    };
                    let mut buffer = [0; 4];
                    out.extend_from_slice(escaped.encode_utf8(&mut buffer).as_bytes());
                    self.i += 1;
                }
                c => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
    }

    /// A literal string's content, its opening quote consumed.
    fn literal(&mut self) -> Option<Cow<'a, str>> {
        let start = self.i;
        loop {
            match self.peek()? {
                b'\'' => {
                    let content = Cow::Borrowed(&self.text[start..self.i]);
                    self.i += 1;
                    return Some(content);
                }
                b'\n' => return None,
                _ => self.i += 1,
            }
        }
    }

    /// Past a multi-line string whose opening `quote` × 3 is consumed, up to
    /// its closing delimiter and the up to two quotes the content may end
    /// with. A basic one (`"""`) honours backslash escapes.
    fn multiline(&mut self, quote: u8) {
        let closer = [quote; 3];
        while self.peek().is_some() {
            if quote == b'"' && self.peek() == Some(b'\\') {
                self.bump();
                self.bump();
            } else if self.starts(&closer) {
                self.i += 3;
                let mut extra = 0;
                while extra < 2 && self.peek() == Some(quote) {
                    self.i += 1;
                    extra += 1;
                }
                return;
            } else {
                self.bump();
            }
        }
    }

    /// Past a string of any kind, its first quote not consumed; its content
    /// when it is a single-line one.
    fn string(&mut self) -> Option<Cow<'a, str>> {
        let quote = self.peek()?;
        if self.starts(&[quote; 3]) {
            self.i += 3;
            self.multiline(quote);
            return None;
        }
        self.i += 1;
        if quote == b'"' {
            self.basic()
        } else {
            self.literal()
        }
    }

    /// Past an array or an inline table, its opening bracket not consumed:
    /// a depth counter, strings and comments skipped, so a bracket inside
    /// either is not counted.
    fn skip_nested(&mut self) {
        let mut depth = 0usize;
        while let Some(c) = self.peek() {
            match c {
                b'"' | b'\'' => {
                    if self.string().is_none() && self.peek() == Some(b'\n') {
                        // An unterminated single-line string: the rest of the
                        // line is not read.
                        self.bump();
                    }
                }
                b'#' => {
                    while self.peek().is_some_and(|c| c != b'\n') {
                        self.i += 1;
                    }
                }
                b'[' | b'{' => {
                    depth += 1;
                    self.i += 1;
                }
                b']' | b'}' => {
                    self.i += 1;
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return;
                    }
                }
                _ => self.bump(),
            }
        }
    }

    /// A value; an inline table is read when `table` holds (one level).
    fn value(&mut self, table: bool) -> Value<'a> {
        match self.peek() {
            Some(b'"' | b'\'') => self.string().map_or(Value::Other, Value::Str),
            Some(b'{') if table => {
                let start = (self.i, self.line);
                match self.inline_table() {
                    Some(entries) => Value::Table(entries),
                    None => {
                        (self.i, self.line) = start;
                        self.skip_nested();
                        Value::Other
                    }
                }
            }
            Some(b'[' | b'{') => {
                self.skip_nested();
                Value::Other
            }
            Some(_) => {
                let start = self.i;
                while self.peek().is_some_and(|c| {
                    !matches!(c, b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}' | b'#')
                }) {
                    self.i += 1;
                }
                match &self.b[start..self.i] {
                    b"true" => Value::Bool(true),
                    b"false" => Value::Bool(false),
                    _ => Value::Other,
                }
            }
            None => Value::Other,
        }
    }

    /// `{ k = v, … }`, its brace not consumed; `None` when malformed.
    fn inline_table(&mut self) -> Option<Vec<(Key<'a>, Value<'a>)>> {
        self.i += 1;
        let mut entries = Vec::new();
        loop {
            self.blank();
            if self.peek()? == b'}' {
                self.i += 1;
                return Some(entries);
            }
            let key = self.key()?;
            self.spaces();
            if self.peek()? != b'=' {
                return None;
            }
            self.i += 1;
            self.spaces();
            let value = self.value(false);
            entries.push((key, value));
            self.blank();
            match self.peek()? {
                b',' => self.i += 1,
                b'}' => {}
                _ => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry<'t>(key: &'t str, value: Value<'t>, line: u32) -> Statement<'t> {
        Statement::Entry {
            key: key.split('.').map(Cow::Borrowed).collect(),
            value,
            line,
        }
    }

    fn parts(parts: &[&'static str]) -> Key<'static> {
        parts.iter().copied().map(Cow::Borrowed).collect()
    }

    fn string(text: &str) -> Value<'_> {
        Value::Str(Cow::Borrowed(text))
    }

    #[test]
    fn strings_arrays_and_inline_tables_are_skipped_whole() {
        let text = "[package]\nname = \"real\" # the name\nkeywords = [\n  \"a\", # ] and \"\n  \"]\",\n  [\"[\", \"{\"],\n]\n\
                    description = '''\n[lib]\nname = \"fake\"\n'''\nauthors = [\"\"\"\nname = \"fake2\"\n\"\"\", 'x']\n\
                    readme = \"\"\"one \\\"\"\" line\"\"\"\n\"quoted.key\" = 'v'\ninline = { a = \"}\", b = [1, {c = \"]\"}] }\n\
                    dep = { path = \"../d\", workspace = true }\n\n[[bin]]\npath = \"src/real.rs\"\n";
        assert_eq!(
            statements(text),
            [
                Statement::Table {
                    path: parts(&["package"]),
                    array: false
                },
                entry("name", string("real"), 2),
                entry("keywords", Value::Other, 3),
                entry("description", Value::Other, 8),
                entry("authors", Value::Other, 12),
                entry("readme", Value::Other, 15),
                Statement::Entry {
                    key: parts(&["quoted.key"]),
                    value: string("v"),
                    line: 16
                },
                entry(
                    "inline",
                    Value::Table(vec![
                        (parts(&["a"]), string("}")),
                        (parts(&["b"]), Value::Other),
                    ]),
                    17
                ),
                entry(
                    "dep",
                    Value::Table(vec![
                        (parts(&["path"]), string("../d")),
                        (parts(&["workspace"]), Value::Bool(true)),
                    ]),
                    18
                ),
                Statement::Table {
                    path: parts(&["bin"]),
                    array: true
                },
                entry("path", string("src/real.rs"), 21),
            ]
        );
    }

    #[test]
    fn a_line_the_scanner_cannot_read_is_skipped() {
        let text = "[target.'cfg(unix)'.dependencies]\nunterminated = \"x\nok = true\n\
                    [bad header\nafter = 1 2\nlast.key = false\n";
        assert_eq!(
            statements(text),
            [
                Statement::Table {
                    path: parts(&["target", "cfg(unix)", "dependencies"]),
                    array: false
                },
                entry("unterminated", Value::Other, 2),
                entry("ok", Value::Bool(true), 3),
                Statement::Table {
                    path: Vec::new(),
                    array: false
                },
                entry("last.key", Value::Bool(false), 6),
            ]
        );
    }

    #[test]
    fn brackets_nested_without_bound_cost_no_stack() {
        let text = format!(
            "deep = {}{}\nafter = true\n",
            "[".repeat(100_000),
            "]".repeat(100_000)
        );
        assert_eq!(
            statements(&text),
            [
                entry("deep", Value::Other, 1),
                entry("after", Value::Bool(true), 2)
            ]
        );
    }
}
