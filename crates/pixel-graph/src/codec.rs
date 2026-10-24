// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! A flat text encoding for what a rule file stores and reads back: a
//! record is a list of fields, a list is a list of records. Rule files use
//! it for `Edge::site` and a file's facts; nothing else reads them.
//!
//! Fields are separated by U+001F, records by U+001E; a backslash escapes
//! itself and both separators, so any text round-trips, hostile source
//! included.

use std::borrow::Cow;

const FIELD: char = '\u{1f}';
const RECORD: char = '\u{1e}';
const ESCAPE: char = '\\';

/// Encodes one record.
pub fn record<S: AsRef<str>>(fields: &[S]) -> String {
    let mut out = String::new();
    record_into(fields, &mut out);
    out
}

fn record_into<S: AsRef<str>>(fields: &[S], out: &mut String) {
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            out.push(FIELD);
        }
        escape_into(field.as_ref(), out);
    }
}

/// Encodes a list of records, with no spare capacity: a file's facts
/// are kept until the index is written.
pub fn records<R: AsRef<[S]>, S: AsRef<str>>(list: &[R]) -> String {
    records_of(list)
}

/// [`records`], from the records in turn.
pub fn records_of<R: AsRef<[S]>, S: AsRef<str>>(list: impl IntoIterator<Item = R>) -> String {
    let mut out = String::new();
    for (i, fields) in list.into_iter().enumerate() {
        if i > 0 {
            out.push(RECORD);
        }
        record_into(fields.as_ref(), &mut out);
    }
    out.shrink_to_fit();
    out
}

/// Decodes one record. The empty string is one empty field.
pub fn fields(text: &str) -> Vec<String> {
    split(text, FIELD)
}

/// Decodes a list of records. The empty string is no record.
pub fn list(text: &str) -> Vec<Vec<String>> {
    each(text).collect()
}

/// Decodes a list of records one at a time, as [`list`] does.
pub fn each(text: &str) -> impl Iterator<Item = Vec<String>> + '_ {
    let mut rest = (!text.is_empty()).then_some(text);
    std::iter::from_fn(move || {
        let mut text = rest?;
        let mut record = vec![String::new()];
        // Runs of plain text are copied whole; the separators and the
        // escape are one byte each.
        loop {
            let field = record.last_mut().expect("one field");
            let Some(at) = special(text) else {
                field.push_str(text);
                rest = None;
                return Some(record);
            };
            field.push_str(&text[..at]);
            let c = text.as_bytes()[at] as char;
            text = &text[at + 1..];
            match c {
                ESCAPE => {
                    if let Some(next) = text.chars().next() {
                        field.push(next);
                        text = &text[next.len_utf8()..];
                    }
                }
                FIELD => record.push(String::new()),
                _ => {
                    rest = Some(text);
                    return Some(record);
                }
            }
        }
    })
}

/// Calls `f` with each record of a list, as [`each`] decodes it, each
/// field borrowed from `text` unless it holds an escape: a list of many
/// records is read with no allocation but for those.
pub fn each_borrowed<'t>(text: &'t str, mut f: impl FnMut(&[Cow<'t, str>])) {
    each_borrowed_while(text, |record| {
        f(record);
        true
    });
}

/// [`each_borrowed`], up to the first record for which `f` is false.
pub fn each_borrowed_while<'t>(text: &'t str, mut f: impl FnMut(&[Cow<'t, str>]) -> bool) {
    let mut record: Vec<Cow<'t, str>> = Vec::new();
    let mut rest = (!text.is_empty()).then_some(text);
    while let Some(mut text) = rest {
        record.clear();
        // The field being read: borrowed while it holds no escape.
        let mut field = Cow::Borrowed("");
        loop {
            let Some(at) = special(text) else {
                append(&mut field, text);
                record.push(field);
                rest = None;
                break;
            };
            append(&mut field, &text[..at]);
            let c = text.as_bytes()[at] as char;
            text = &text[at + 1..];
            match c {
                ESCAPE => {
                    if let Some(next) = text.chars().next() {
                        field.to_mut().push(next);
                        text = &text[next.len_utf8()..];
                    }
                }
                FIELD => record.push(std::mem::take(&mut field)),
                _ => {
                    record.push(field);
                    rest = Some(text);
                    break;
                }
            }
        }
        if !f(&record) {
            return;
        }
    }
}

/// Where the first separator or escape of `text` is: each is one ASCII
/// byte, which no other character's encoding holds.
fn special(text: &str) -> Option<usize> {
    text.bytes()
        .position(|b| b == FIELD as u8 || b == RECORD as u8 || b == ESCAPE as u8)
}

/// Appends `run` to a field, borrowing it when the field is still empty.
fn append<'t>(field: &mut Cow<'t, str>, run: &'t str) {
    if field.is_empty() {
        *field = Cow::Borrowed(run);
    } else if !run.is_empty() {
        field.to_mut().push_str(run);
    }
}

fn escape_into(text: &str, out: &mut String) {
    if special(text).is_none() {
        return out.push_str(text);
    }
    for c in text.chars() {
        if matches!(c, FIELD | RECORD | ESCAPE) {
            out.push(ESCAPE);
        }
        out.push(c);
    }
}

/// Splits on unescaped `sep` and unescapes each part.
fn split(text: &str, sep: char) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == ESCAPE {
            if let Some(next) = chars.next() {
                parts.last_mut().expect("one part").push(next);
            }
        } else if c == sep {
            parts.push(String::new());
        } else {
            parts.last_mut().expect("one part").push(c);
        }
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_of_hostile_text_round_trips() {
        let hostile = [
            "plain",
            "",
            "a\u{1f}b",
            "c\u{1e}d",
            "back\\slash\\",
            "line\nbreak",
        ];
        assert_eq!(fields(&record(&hostile)), hostile);
    }

    #[test]
    fn a_list_reads_the_same_borrowed() {
        for text in [
            "",
            "a",
            "a\u{1f}b\u{1e}c",
            "\u{1e}",
            "\u{1f}\u{1f}",
            "x\\\u{1e}y\u{1f}z\\",
            "a\\",
            "\\\u{1f}a\u{1f}\\\\b",
            "é\\\u{1f}ü\u{1f}x\u{1e}\\\\",
            "dep\u{1f}\u{1f}c1\u{1f}\u{1f}workspace\u{1f}\u{1e}dep\u{1f}dev\u{1f}c\\2\u{1f}p\u{1f}path\u{1f}..\\c2",
        ] {
            let mut borrowed: Vec<Vec<String>> = Vec::new();
            each_borrowed(text, |record| {
                borrowed.push(record.iter().map(ToString::to_string).collect());
            });
            assert_eq!(borrowed, list(text), "{text:?}");
        }
    }

    #[test]
    fn a_list_of_records_round_trips() {
        let list_in = vec![
            vec!["include".to_owned(), "Trackable".to_owned()],
            vec!["x\u{1e}y\\".to_owned()],
            vec![String::new()],
        ];
        assert_eq!(list(&records(&list_in)), list_in);
        assert!(list(&records::<Vec<String>, String>(&[])).is_empty());
    }

    #[test]
    fn a_list_reads_escapes_in_any_text() {
        let lists = [
            ("a\\", vec![vec!["a"]]),
            ("\u{1e}", vec![vec![""], vec![""]]),
            (
                "é\\\u{1f}ü\u{1f}x\u{1e}\\\\",
                vec![vec!["é\u{1f}ü", "x"], vec!["\\"]],
            ),
        ];
        for (text, decoded) in lists {
            assert_eq!(list(text), decoded, "{text:?}");
        }
    }
}
