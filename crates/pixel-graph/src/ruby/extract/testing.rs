// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Renders a Ruby file's rows as short lines, so that a test reads its
//! fixture and the rows it expects side by side. Every line names the
//! site's owner symbol and ends with its cref, innermost first.

use pixel_model::{Edge, EdgeKind, FileRows, Lang};

use crate::ruby::{ASSOCIATION, Context, MIXIN, ROUTE};

/// The rows the driver makes of `source` at `path`: uids, owners, the
/// `File` symbol.
pub fn rows(path: &str, source: &str) -> FileRows {
    crate::extract::rows(path, Lang::Ruby, source.as_bytes())
}

/// `kind name line-end_line | signature`; the `File` symbol has none.
pub fn symbols(rows: &FileRows) -> Vec<String> {
    rows.symbols
        .iter()
        .map(|s| {
            let head = format!("{} {} {}-{}", s.kind, s.name, s.line, s.end_line);
            if s.signature.is_empty() {
                head
            } else {
                format!("{head} | {}", s.signature)
            }
        })
        .collect()
}

/// `line owner: name [cref]`, with `on receiver` when the call has one.
pub fn calls(rows: &FileRows) -> Vec<String> {
    render(rows, |edge, context| {
        (edge.kind == EdgeKind::Call).then(|| {
            if context.qualifier.is_empty() {
                context.name.clone()
            } else {
                format!("{} on {}", context.name, context.qualifier)
            }
        })
    })
}

/// `line owner: name (qualifier) [cref]` for the references to a method:
/// callbacks, `send`, aliases, delegators.
pub fn references(rows: &FileRows) -> Vec<String> {
    render(rows, |edge, context| {
        (edge.kind == EdgeKind::Reference && !context.qualifier.starts_with(MIXIN))
            .then(|| format!("{} ({})", context.name, context.qualifier))
    })
}

/// `line owner: kind constant [cref]` for each declared ancestor; `-` for
/// one that is not a literal constant.
pub fn ancestors(rows: &FileRows) -> Vec<String> {
    render(rows, |edge, context| {
        let kind = context.qualifier.strip_prefix(MIXIN)?;
        (edge.kind == EdgeKind::Reference).then(|| {
            let name = if context.name.is_empty() {
                "-"
            } else {
                &context.name
            };
            format!("{kind} {name}")
        })
    })
}

/// `line owner: macro class_path [cref]`.
pub fn associations(rows: &FileRows) -> Vec<String> {
    render(rows, |edge, context| {
        let kind = context.qualifier.strip_prefix(ASSOCIATION)?;
        (edge.kind == EdgeKind::Association).then(|| format!("{kind} {}", context.name))
    })
}

/// `line owner: method spec -> "path" [cref]`.
pub fn imports(rows: &FileRows) -> Vec<String> {
    render(rows, |edge, context| {
        (edge.kind == EdgeKind::Import).then(|| {
            let path = context.extra.first().cloned().unwrap_or_default();
            format!("{} {} -> {path:?}", context.qualifier, context.name)
        })
    })
}

/// `line owner: VERB /path Controller#action [cref]`.
pub fn routes(rows: &FileRows) -> Vec<String> {
    render(rows, |edge, context| {
        let controller = context.qualifier.strip_prefix(ROUTE)?;
        (edge.kind == EdgeKind::Route)
            .then(|| format!("{} {controller}#{}", context.extra.join(" "), context.name))
    })
}

fn render(rows: &FileRows, line: impl Fn(&Edge, &Context) -> Option<String>) -> Vec<String> {
    rows.edges
        .iter()
        .filter_map(|edge| {
            let context = Context::decode(&edge.context);
            let what = line(edge, &context)?;
            let owner = rows
                .symbols
                .iter()
                .find(|s| s.uid == edge.src)
                .map_or("?", |s| s.name.as_str());
            Some(format!(
                "{} {owner}: {what} [{}]",
                edge.line,
                context.cref.join(", ")
            ))
        })
        .collect()
}
