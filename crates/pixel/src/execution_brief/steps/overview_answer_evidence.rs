// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `overview-answer-evidence`: for a question that names one Cargo member
//! ("what does the `x` crate expose"), the evidence its answer needs: the
//! repository's `AGENTS.md` heads, the crate's docs and public facade, its
//! manifest, its tracked files, what each dependency is for and up to three
//! examples, bounded at 24 000 characters.
//!
//! Ported from `probes/overview-answer-evidence/probe`. The member is the one
//! non-root `Cargo.toml` package whose name or directory name the prompt
//! holds as a whole name (no `[\w-]` beside it); two or none say nothing, and
//! so does a repository of more than 4 000 tracked paths. Like the script, a
//! dependency whose `description` is not a string is printed as Python would
//! print it (`{'workspace': True}`), and a `[package]` without a `name`
//! ends the step with nothing printed.

use std::collections::BTreeMap;

use pixel_plugin::ProbeMode;
use toml_edit::{DocumentMut, Item, TableLike, Value};

use super::text::{head, last_segment, splitlines};
use super::{Ctx, Step};

pub(super) const STEP: Step = Step {
    name: "overview-answer-evidence",
    when_kinds: &[],
    skip_kinds: &["history", "impact", "review"],
    when_prompt: Some(r"(?i)\b(?:crate|library)\b"),
    when_gate_closed: true,
    budget_ms: 2500,
    mode: ProbeMode::Lead,
    run,
};

const MAX_TOTAL: usize = 24_000;
const MAX_DOC_LINES: usize = 60;
const MAX_DOC_CHARS: usize = 240;
const EXAMPLE_LINES: usize = 65;
const MAX_TRACKED_PATHS: usize = 4_000;
const MAX_FILE_BYTES: u64 = 300_000;
const TRUNCATED: &str = "... evidence truncated at the configured character budget";

/// A tracked `Cargo.toml` and what it parsed to (nothing, when it cannot be).
struct Manifest {
    path: String,
    doc: DocumentMut,
}

impl Manifest {
    fn package(&self) -> Option<&dyn TableLike> {
        self.doc.get("package").and_then(Item::as_table_like)
    }

    /// The package's name, when it has a non-empty one.
    fn name(&self) -> Option<&str> {
        self.package()?
            .get("name")?
            .as_str()
            .filter(|name| !name.is_empty())
    }

    fn directory(&self) -> &str {
        directory(&self.path)
    }
}

/// `path.rsplit("/", 1)[0]`: the path itself when it has no slash.
fn directory(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(dir, _)| dir)
}


/// The text of the tracked file `path` (a regular file of at most
/// [`MAX_FILE_BYTES`], never a link).
fn read(ctx: &Ctx, files: &[String], path: &str) -> Option<String> {
    files
        .iter()
        .any(|file| file == path)
        .then(|| ctx.read(path, MAX_FILE_BYTES))?
}

fn run(ctx: &Ctx) -> String {
    let Some(files) = ctx.files() else {
        return String::new();
    };
    if files.len() > MAX_TRACKED_PATHS {
        return String::new();
    }
    let mut manifests = Vec::new();
    for path in files
        .iter()
        .filter(|path| path.as_str() == "Cargo.toml" || path.ends_with("/Cargo.toml"))
    {
        if ctx.timed_out() {
            return String::new();
        }
        let doc = read(ctx, files, path)
            .and_then(|text| text.parse().ok())
            .unwrap_or_default();
        manifests.push(Manifest {
            path: path.clone(),
            doc,
        });
    }
    let folded = ctx.prompt.to_lowercase();
    let mut found = Vec::new();
    for manifest in &manifests {
        let Some(package) = manifest.package().filter(|package| !package.is_empty()) else {
            continue;
        };
        if manifest.path == "Cargo.toml" {
            continue;
        }
        // `package["name"]` of the script: a package without one ends it.
        let Some(name) = package.get("name").and_then(Item::as_str) else {
            return String::new();
        };
        let directory = last_segment(manifest.directory());
        if [name, directory]
            .iter()
            .any(|name| has_name(&folded, &name.to_lowercase()))
        {
            found.push(manifest);
        }
    }
    let [member] = found[..] else {
        return String::new();
    };
    let Some(lines) = evidence(ctx, files, &manifests, member) else {
        return String::new();
    };
    bounded(&lines)
}

/// Whether `haystack` holds `needle` as a whole name: not preceded or
/// followed by a name character (`[\w-]`).
fn has_name(haystack: &str, needle: &str) -> bool {
    let name_char = |ch: char| ch.is_alphanumeric() || ch == '_' || ch == '-';
    haystack.match_indices(needle).any(|(start, found)| {
        !haystack[..start].chars().next_back().is_some_and(name_char)
            && !haystack[start + found.len()..]
                .chars()
                .next()
                .is_some_and(name_char)
    })
}

/// The evidence lines for `member`, or `None` when its library root cannot
/// be read.
fn evidence(
    ctx: &Ctx,
    files: &[String],
    manifests: &[Manifest],
    member: &Manifest,
) -> Option<Vec<String>> {
    let directory = member.directory();
    let package_description = member
        .package()
        .and_then(|package| package.get("description"));
    let by_name: BTreeMap<&str, &Manifest> = manifests
        .iter()
        .filter_map(|manifest| Some((manifest.name()?, manifest)))
        .collect();

    let mut out = vec!["### Repository instructions".to_owned()];
    let guidance: Vec<&String> = files
        .iter()
        .filter(|path| last_segment(path) == "AGENTS.md")
        .collect();
    if guidance.is_empty() {
        out.push("No tracked AGENTS.md instruction files are present.".to_owned());
    }
    for path in guidance.into_iter().take(8) {
        if let Some(text) = read(ctx, files, path) {
            out.push(format!("{path}:"));
            out.extend(
                splitlines(&text)
                    .into_iter()
                    .take(12)
                    .map(|line| head(line, MAX_DOC_CHARS).to_owned()),
            );
        }
    }

    let (source, main_source) = crate_source(ctx, files, directory, member)?;
    let description = one_line(&py_text(package_description), 160);
    out.push(format!("### {source} — crate purpose and public surface"));
    out.push(if description.is_empty() {
        "crate-level documentation:".to_owned()
    } else {
        description
    });
    out.extend(leading_docs(&main_source, MAX_DOC_LINES));
    out.push(format!(
        "### {} — line-numbered package manifest",
        member.path
    ));
    if let Some(text) = read(ctx, files, &member.path) {
        out.extend(numbered(&text, 100));
    }
    out.push(format!("### {source} — line-numbered crate facade"));
    out.extend(numbered(&main_source, 100));

    let prefix = format!("{directory}/");
    let mut member_files: Vec<&str> = files
        .iter()
        .filter_map(|path| path.strip_prefix(prefix.as_str()))
        .collect();
    member_files.sort_unstable();
    out.push(format!(
        "### Tracked files in {directory} ({} total)",
        member_files.len()
    ));
    out.extend(member_files.iter().take(100).map(ToString::to_string));
    if member_files.len() > 100 {
        out.push(format!("... {} paths omitted", member_files.len() - 100));
    }

    for (key, label) in [
        ("dependencies", "dependencies"),
        ("dev-dependencies", "development dependencies"),
    ] {
        let Some(table) = member.doc.get(key).and_then(Item::as_table_like) else {
            continue;
        };
        if !table.is_empty() {
            out.push(format!("{label}:"));
        }
        let mut deps: Vec<(&str, &Item)> = table.iter().collect();
        deps.sort_unstable_by_key(|(name, _)| *name);
        for (name, spec) in deps {
            if ctx.timed_out() {
                return None;
            }
            let local = by_name.get(name).copied();
            let optional = spec
                .as_table_like()
                .and_then(|spec| spec.get("optional"))
                .and_then(Item::as_bool)
                == Some(true);
            let described = local
                .and_then(|local| local.package())
                .and_then(|package| package.get("description"))
                .map(|item| py_text(Some(item)))
                .filter(|text| !text.is_empty());
            out.push(format!(
                "- {name}{}{}",
                if optional { " (optional)" } else { "" },
                described.map_or_else(String::new, |text| format!(" — {}", one_line(&text, 160)))
            ));
            if let Some(local) = local
                && let Some((dep_source, dep_text)) =
                    crate_source(ctx, files, local.directory(), local)
            {
                let docs = leading_docs(&dep_text, 12);
                if !docs.is_empty() {
                    out.push(format!("  {dep_source} — leading module documentation"));
                    out.extend(docs.into_iter().map(|line| format!("  {line}")));
                }
            }
        }
    }

    let examples = format!("{directory}/examples/");
    for example in files
        .iter()
        .filter(|path| path.starts_with(&examples) && path.ends_with(".rs"))
        .take(3)
    {
        let Some(text) = read(ctx, files, example) else {
            continue;
        };
        let lines = splitlines(&text);
        out.push(format!("### {example} — example source"));
        out.extend(numbered_lines(&lines, EXAMPLE_LINES));
    }
    Some(out)
}

/// The library root of `manifest`, whose directory is `directory`: its
/// `[lib].path`, else `src/lib.rs`; with the text read from it.
fn crate_source(
    ctx: &Ctx,
    files: &[String],
    directory: &str,
    manifest: &Manifest,
) -> Option<(String, String)> {
    let configured = manifest
        .doc
        .get("lib")
        .and_then(Item::as_table_like)
        .and_then(|lib| lib.get("path"))
        .and_then(Item::as_str)
        .filter(|path| !path.is_empty());
    let path = configured.map_or_else(
        || format!("{directory}/src/lib.rs"),
        |path| format!("{directory}/{path}"),
    );
    let text = read(ctx, files, &path)?;
    Some((path, text))
}


/// `text` on one line, whitespace collapsed, cut to `limit` characters.
fn one_line(text: &str, limit: usize) -> String {
    head(&text.split_whitespace().collect::<Vec<_>>().join(" "), limit).to_owned()
}

/// The leading `//!` or `/*! */` documentation of a Rust source.
fn leading_docs(text: &str, max_lines: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut block = false;
    for raw in splitlines(text).into_iter().take(max_lines) {
        let line = if raw.chars().count() > MAX_DOC_CHARS {
            head(raw, MAX_DOC_CHARS).to_owned() + "…"
        } else {
            raw.to_owned()
        };
        let mut stripped = line.trim().to_owned();
        if out.is_empty() && stripped.is_empty() {
            continue;
        }
        if out.is_empty() && stripped.starts_with("/*!") {
            block = true;
            stripped = stripped[3..].to_owned();
        } else if block {
            if let Some((head, _)) = stripped.split_once("*/") {
                block = false;
                stripped = head.to_owned();
            } else {
                stripped = stripped.trim_start_matches(['*', ' ']).to_owned();
            }
        } else if let Some(rest) = stripped.strip_prefix("//!") {
            stripped = rest.trim().to_owned();
        } else if stripped.is_empty() {
            continue;
        } else {
            break;
        }
        if !stripped.is_empty() {
            out.push(stripped.clone());
        }
        let trimmed = line.trim();
        if !block
            && !stripped.is_empty()
            && !["//!", "/*", "*"]
                .iter()
                .any(|prefix| trimmed.starts_with(prefix))
        {
            break;
        }
    }
    out
}

fn numbered(text: &str, limit: usize) -> Vec<String> {
    numbered_lines(&splitlines(text), limit)
}

/// `lines` numbered from 1 up to `limit`, cut to 180 characters, and a count
/// of the lines left out.
fn numbered_lines(lines: &[&str], limit: usize) -> Vec<String> {
    let mut out: Vec<String> = lines
        .iter()
        .take(limit)
        .enumerate()
        .map(|(index, line)| format!("{}| {}", index + 1, head(line, 180)))
        .collect();
    if lines.len() > limit {
        out.push(format!("... {} lines omitted", lines.len() - limit));
    }
    out
}

/// The lines as the step prints them: whole while they fit [`MAX_TOTAL`]
/// characters (a newline counted for each), then a line saying so.
fn bounded(lines: &[String]) -> String {
    let mut used = 0;
    let mut kept: Vec<&str> = Vec::new();
    for line in lines {
        let cost = line.chars().count() + 1;
        if used + cost > MAX_TOTAL {
            kept.push(TRUNCATED);
            break;
        }
        kept.push(line);
        used += cost;
    }
    kept.join("\n") + "\n"
}

/// Python's `str(value or "")` for a manifest value: what the script prints
/// for a `description` that is not a string.
fn py_text(item: Option<&Item>) -> String {
    match item {
        Some(Item::Value(Value::String(text))) => text.value().clone(),
        Some(item) if truthy(item) => repr(item),
        _ => String::new(),
    }
}

fn truthy(item: &Item) -> bool {
    match item {
        Item::None => false,
        Item::Value(Value::String(text)) => !text.value().is_empty(),
        Item::Value(Value::Integer(number)) => *number.value() != 0,
        Item::Value(Value::Float(number)) => *number.value() != 0.0,
        Item::Value(Value::Boolean(flag)) => *flag.value(),
        Item::Value(Value::Array(array)) => !array.is_empty(),
        Item::Value(Value::InlineTable(table)) => !table.is_empty(),
        Item::Value(Value::Datetime(_)) => true,
        Item::Table(table) => !table.is_empty(),
        Item::ArrayOfTables(tables) => !tables.is_empty(),
    }
}

/// Python's `repr` of a manifest value. shortcut: a datetime prints as TOML
/// writes it and a string is escaped for the quote and the backslash only,
/// upgrade when a description needs more.
fn repr(item: &Item) -> String {
    let dict = |table: &dyn TableLike| {
        let pairs: Vec<String> = table
            .iter()
            .map(|(key, value)| format!("{}: {}", quote(key), repr(value)))
            .collect();
        format!("{{{}}}", pairs.join(", "))
    };
    match item {
        Item::None => "None".to_owned(),
        Item::Value(Value::String(text)) => quote(text.value()),
        Item::Value(Value::Integer(number)) => number.value().to_string(),
        Item::Value(Value::Float(number)) => number.value().to_string(),
        Item::Value(Value::Boolean(flag)) => if *flag.value() { "True" } else { "False" }.to_owned(),
        Item::Value(Value::Datetime(date)) => date.value().to_string(),
        Item::Value(Value::Array(array)) => {
            let items: Vec<String> = array
                .iter()
                .map(|value| repr(&Item::Value(value.clone())))
                .collect();
            format!("[{}]", items.join(", "))
        }
        Item::Value(Value::InlineTable(table)) => dict(table),
        Item::Table(table) => dict(table),
        Item::ArrayOfTables(tables) => {
            let items: Vec<String> = tables.iter().map(|table| dict(table)).collect();
            format!("[{}]", items.join(", "))
        }
    }
}

fn quote(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let escaped = text.replace('\\', "\\\\").replace(quote, &format!("\\{quote}"));
    format!("{quote}{escaped}{quote}")
}

#[cfg(test)]
mod tests;
