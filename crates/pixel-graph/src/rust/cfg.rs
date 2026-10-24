// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `#[cfg(…)]` predicates as resolution reads them (F2): the conjuncts an
//! attribute puts on what it is attached to, with the ones Rust proves
//! true dropped and a provably false predicate made the single conjunct
//! [`FALSE`]. Nothing is assumed about the target: `unix`, `test` or a
//! feature is neither true nor false, only carried or not.

use super::types::MAX_NESTING;

/// The conjunct of what never exists: `cfg(any())`, `cfg(not(all()))`,
/// `cfg(false)` and any predicate Rust evaluates to false whatever the
/// target.
pub const FALSE: &str = "false";

/// What an attribute's text (without white space, as `types::compact`
/// gives it) puts on its item: the conjuncts of `#[cfg(p)]`, or one
/// opaque conjunct for a `#[cfg_attr(…)]` that may add a `cfg`; nothing
/// for any other attribute.
pub fn conjuncts(attribute: &str) -> Vec<String> {
    let inner = attribute
        .strip_prefix("#[")
        .and_then(|a| a.strip_suffix(']'))
        .unwrap_or_default();
    if let Some(predicate) = inner.strip_prefix("cfg(").and_then(|p| p.strip_suffix(')')) {
        let mut out = Vec::new();
        conjuncts_of(predicate, 0, &mut out);
        if out.iter().any(|c| c == FALSE) {
            return vec![FALSE.to_owned()];
        }
        return out;
    }
    if inner.starts_with("cfg_attr(") && inner.contains("cfg(") {
        return vec![inner.to_owned()];
    }
    Vec::new()
}

/// The conjuncts of `predicate`: the parts of an `all(…)`, each of them
/// flattened in turn, those Rust proves true dropped.
fn conjuncts_of(predicate: &str, depth: usize, out: &mut Vec<String>) {
    match parse(predicate, depth) {
        Some(Pred::All(parts)) if depth < MAX_NESTING => {
            for part in parts {
                conjuncts_of(part, depth + 1, out);
            }
        }
        parsed => match parsed.map(|p| eval(&p, depth)) {
            Some(Truth::True) => {}
            Some(Truth::False) => out.push(FALSE.to_owned()),
            _ => out.push(predicate.to_owned()),
        },
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Truth {
    True,
    False,
    Unknown,
}

/// One level of a predicate; its parts are the texts of the inner ones.
enum Pred<'a> {
    All(Vec<&'a str>),
    Any(Vec<&'a str>),
    Not(&'a str),
    Bool(bool),
    /// `unix`, `feature = "x"`: true on some targets, false on others.
    Atom,
}

/// The outer level of `text`; `None` when it is not a predicate.
fn parse(text: &str, depth: usize) -> Option<Pred<'_>> {
    if depth > MAX_NESTING {
        return None;
    }
    match text {
        "true" => return Some(Pred::Bool(true)),
        "false" => return Some(Pred::Bool(false)),
        _ => {}
    }
    if let Some(args) = text.strip_prefix("all(").and_then(|t| t.strip_suffix(')')) {
        return Some(Pred::All(split_arguments(args)?));
    }
    if let Some(args) = text.strip_prefix("any(").and_then(|t| t.strip_suffix(')')) {
        return Some(Pred::Any(split_arguments(args)?));
    }
    if let Some(arg) = text.strip_prefix("not(").and_then(|t| t.strip_suffix(')')) {
        return match split_arguments(arg)?.as_slice() {
            [one] => Some(Pred::Not(one)),
            _ => None,
        };
    }
    let name = text.split('=').next().unwrap_or_default();
    let identifier = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':');
    identifier.then_some(Pred::Atom)
}

/// `a,b(c,d),e="f,g"` → `["a", "b(c,d)", "e=\"f,g\""]`; `None` when the
/// brackets or quotes do not balance.
fn split_arguments(text: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth = depth.checked_sub(1)?,
            ',' if !quoted && depth == 0 => {
                parts.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if quoted || depth != 0 {
        return None;
    }
    let last = &text[start..];
    if !last.is_empty() {
        parts.push(last);
    }
    Some(parts)
}

/// Rust's value of a predicate on every target: true or false when the
/// target cannot change it, unknown otherwise.
fn eval(predicate: &Pred, depth: usize) -> Truth {
    let inner = |text: &str| parse(text, depth + 1).map_or(Truth::Unknown, |p| eval(&p, depth + 1));
    match predicate {
        Pred::Bool(true) => Truth::True,
        Pred::Bool(false) => Truth::False,
        Pred::Atom => Truth::Unknown,
        Pred::Not(part) => match inner(part) {
            Truth::True => Truth::False,
            Truth::False => Truth::True,
            Truth::Unknown => Truth::Unknown,
        },
        Pred::All(parts) => {
            let values: Vec<Truth> = parts.iter().map(|p| inner(p)).collect();
            if values.contains(&Truth::False) {
                Truth::False
            } else if values.iter().all(|v| *v == Truth::True) {
                Truth::True
            } else {
                Truth::Unknown
            }
        }
        Pred::Any(parts) => {
            let values: Vec<Truth> = parts.iter().map(|p| inner(p)).collect();
            if values.contains(&Truth::True) {
                Truth::True
            } else if values.iter().all(|v| *v == Truth::False) {
                Truth::False
            } else {
                Truth::Unknown
            }
        }
    }
}

/// `outer` with `more` added, sorted and without repeats; [`FALSE`] alone
/// when either holds it.
pub fn union(outer: &[String], more: &[String]) -> Vec<String> {
    if outer.iter().chain(more).any(|c| c == FALSE) {
        return vec![FALSE.to_owned()];
    }
    let mut out: Vec<String> = outer.iter().chain(more).cloned().collect();
    out.sort();
    out.dedup();
    out
}

/// Whether every conjunct of `cfg` is one of `carried` (sorted).
pub fn carried_by(cfg: &[String], carried: &[String]) -> bool {
    cfg.iter().all(|c| carried.binary_search(c).is_ok())
}

pub fn is_false(cfg: &[String]) -> bool {
    cfg.iter().any(|c| c == FALSE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(attribute: &str) -> Vec<String> {
        conjuncts(attribute)
    }

    #[test]
    fn a_predicate_rust_proves_false_is_false_and_one_it_proves_true_is_dropped() {
        // rustc 1.99 compiles none of the first group and all of the second.
        for never in [
            "#[cfg(any())]",
            "#[cfg(not(all()))]",
            "#[cfg(false)]",
            "#[cfg(all(unix,any()))]",
            "#[cfg(not(any(true,unix)))]",
        ] {
            assert_eq!(of(never), [FALSE], "{never}");
        }
        for always in [
            "#[cfg(all())]",
            "#[cfg(true)]",
            "#[cfg(all(true,any(false,not(false))))]",
            "#[cfg(any(unix,true))]",
        ] {
            assert!(of(always).is_empty(), "{always}");
        }
    }

    #[test]
    fn an_all_is_its_parts_and_anything_else_is_one_conjunct() {
        assert_eq!(
            of("#[cfg(all(test,not(feature=\"a,b\"),all(unix)))]"),
            ["test", "not(feature=\"a,b\")", "unix"]
        );
        assert_eq!(of("#[cfg(any(unix,windows))]"), ["any(unix,windows)"]);
        assert_eq!(
            of("#[cfg_attr(test,cfg(unix))]"),
            ["cfg_attr(test,cfg(unix))"]
        );
        assert!(of("#[derive(Debug)]").is_empty());
        assert!(of("#[cfg_attr(test,derive(Debug))]").is_empty());
        // Not a predicate Rust knows: kept whole, never proven.
        assert_eq!(of("#[cfg(all(unix),)]"), ["all(unix),"]);
    }

    #[test]
    fn predicates_nested_without_bound_cost_no_stack() {
        let deep = format!(
            "#[cfg({}unix{})]",
            "not(".repeat(10_000),
            ")".repeat(10_000)
        );
        assert_eq!(of(&deep).len(), 1);
    }
}
