// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Which files are Ruby (names, extensions, binstubs, the generated-blob
//! guard) and the manifest facts of a Gemfile, a gemspec, a Gemfile.lock.
//!
//! The file rules are v1's language gate (`crates/pixel-graph/src/
//! extract.rs:146-266`: `lang_of`, `is_binstub_candidate`,
//! `has_ruby_shebang`, `is_generated_blob`); the facts are what v1's
//! project boundary reads (`crates/pixel-graph/src/imports/ruby.rs:
//! 268-437`: `lock_gems`, `read_gemfile`, `read_gemspec`). The manifests
//! are read as data, never evaluated: a computed path, `eval_gemfile` or a
//! `$LOAD_PATH` edit is not modelled.

use tree_sitter::Node;

use super::Fact;

/// Ruby files known by their whole name: Bundler's `Gemfile` and the
/// Ruby-DSL build files whose tools evaluate them as Ruby (v1
/// `extract.rs:148`).
pub const RUBY_FILE_NAMES: &[&str] = &["Gemfile", "Rakefile", "Guardfile", "Capfile"];

/// The extensions of Ruby source (v1 `extract.rs:166`).
const RUBY_EXTENSIONS: &[&str] = &["rb", "rake", "gemspec", "ru"];

pub use crate::guard::is_generated_blob;

/// Deepest node the manifest readers visit, as the walker's cap (v1 reads
/// manifests without one; a cap keeps a hostile Gemfile off the stack).
const MAX_DEPTH: usize = 512;

/// From the path alone: a Ruby source by name or extension, a binstub
/// candidate (its shebang decides in [`accepts`]), or a `Gemfile.lock`,
/// which yields facts only (v1 `build.rs:391-399`, `is_graph_candidate`),
/// as a fixture file does ([`fixture_set`]).
pub fn wants(path: &str) -> bool {
    is_ruby_by_name(path)
        || is_binstub_candidate(path)
        || is_lockfile(path)
        || fixture_set(path).is_some()
}

/// After reading: a binstub needs a Ruby shebang, and a generated blob is
/// refused, as v1's gates do (`build.rs:319-379`). A lockfile or a
/// fixture file is read as text, never parsed, so the blob guard does
/// not apply to it.
pub fn accepts(path: &str, content: &[u8]) -> bool {
    if is_lockfile(path) || fixture_set(path).is_some() {
        return true;
    }
    if !is_ruby_by_name(path) && !has_ruby_shebang(content) {
        return false;
    }
    !is_generated_blob(content)
}

/// v1's `lang_of(path) == "ruby"`: one of [`RUBY_FILE_NAMES`], or a Ruby
/// extension. Only the file name counts: `pkg.rb/README` is not Ruby.
pub fn is_ruby_by_name(path: &str) -> bool {
    let file = file_name(path);
    RUBY_FILE_NAMES.contains(&file)
        || file
            .rsplit_once('.')
            .is_some_and(|(_, extension)| RUBY_EXTENSIONS.contains(&extension))
}

/// An extensionless file directly in a `bin/` or `exe/` directory
/// (`bin/rails`, `exe/mygem`, `engines/x/bin/setup`), where Rails and
/// Bundler put binstubs. Never Ruby by its place alone: `bin/dev` is often
/// a shell script, so [`has_ruby_shebang`] decides.
pub fn is_binstub_candidate(path: &str) -> bool {
    let mut parts = path.rsplit('/');
    let file = parts.next().unwrap_or(path);
    !file.is_empty()
        && !file.contains('.')
        && matches!(parts.next(), Some("bin" | "exe"))
        && !is_ruby_by_name(path)
}

/// True iff the first line is a Ruby shebang: an interpreter whose base
/// name is `ruby` (`#!/usr/bin/ruby -w`), or `env` whose first word not
/// starting with `-` is `ruby` (`#!/usr/bin/env -S ruby --disable-gems`).
pub fn has_ruby_shebang(content: &[u8]) -> bool {
    let line = content.split(|b| *b == b'\n').next().unwrap_or_default();
    let Some(rest) = line.strip_prefix(b"#!") else {
        return false;
    };
    let line = String::from_utf8_lossy(rest);
    let mut words = line.split_whitespace();
    let Some(interpreter) = words.next() else {
        return false;
    };
    let base = interpreter.rsplit('/').next().unwrap_or(interpreter);
    if base == "env" {
        words.find(|w| !w.starts_with('-')) == Some("ruby")
    } else {
        base == "ruby"
    }
}

/// `Gemfile.lock`, at the root or in a project directory.
pub fn is_lockfile(path: &str) -> bool {
    path == "Gemfile.lock" || path.ends_with("/Gemfile.lock")
}

/// The fixture set a file of Rails' default fixture path holds, at the
/// root or in a project directory: its path under `test/fixtures/`
/// without `.yml` (`test/fixtures/admin/users.yml` → `admin/users`),
/// read for typed receivers (ADR-0038), never parsed.
pub fn fixture_set(path: &str) -> Option<&str> {
    const DIR: &str = "test/fixtures/";
    let stem = path.strip_suffix(".yml")?;
    let set = match stem.strip_prefix(DIR) {
        Some(set) => set,
        None => &stem[stem.find(&format!("/{DIR}"))? + DIR.len() + 1..],
    };
    (!set.is_empty() && !set.ends_with('/')).then_some(set)
}

/// The facts of a fixture file: whether it names its model class
/// (`_fixture: model_class:`), which the rules do not read.
pub fn fixture_facts(content: &[u8]) -> Vec<Fact> {
    let names = String::from_utf8_lossy(content)
        .lines()
        .any(|line| line.trim_start().starts_with("model_class:"));
    if names {
        vec![Fact::FixtureClass]
    } else {
        Vec::new()
    }
}

/// `Gemfile`, at the root or in a project directory (not `Gemfile.rb`).
pub fn is_gemfile(path: &str) -> bool {
    path == "Gemfile" || path.ends_with("/Gemfile")
}

/// `x.gemspec`; a file named only `.gemspec` is not one.
pub fn is_gemspec(path: &str) -> bool {
    let file = file_name(path);
    file.ends_with(".gemspec") && file.len() > ".gemspec".len()
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The facts of a parsed Gemfile or gemspec; nothing for any other file.
pub fn facts(path: &str, root: Node, src: &[u8]) -> Vec<Fact> {
    let mut facts = Vec::new();
    if is_gemfile(path) {
        gemfile(src, root, None, &mut facts, 0);
    }
    if is_gemspec(path) {
        let mut name = None;
        let mut paths = None;
        gemspec(src, root, &mut name, &mut paths, 0);
        facts.extend(name.map(|name| Fact::GemspecName { name }));
        facts.extend(paths.map(|paths| Fact::RequirePaths { paths }));
    }
    facts
}

/// The gems a `Gemfile.lock` lists as specs (four-space indented lines
/// under `specs:`) in its `GEM` and `GIT` sections, transitive ones
/// included; `PATH` specs are local and left out (v1 `lock_gems`).
pub fn lock_gems(content: &[u8]) -> Vec<Fact> {
    let lock = String::from_utf8_lossy(content);
    let mut gems = Vec::new();
    let mut section = "";
    for line in lock.lines() {
        if !line.starts_with(' ') {
            section = line.trim();
            continue;
        }
        if !matches!(section, "GEM" | "GIT") {
            continue;
        }
        let Some(spec) = line.strip_prefix("    ") else {
            continue;
        };
        if spec.starts_with(' ') {
            continue;
        }
        if let Some(name) = spec.split(' ').next().filter(|n| !n.is_empty()) {
            gems.push(Fact::LockGem {
                name: name.to_owned(),
            });
        }
    }
    gems
}

/// v1 `walk_gemfile`: every receiver-less `gem`, `gemspec` and `path`
/// call, whatever block (`group`, `platforms`) holds it.
fn gemfile(src: &[u8], node: Node, path_block: Option<&str>, facts: &mut Vec<Fact>, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut block_dir: Option<String> = None;
    if node.kind() == "call" && node.child_by_field_name("receiver").is_none() {
        let method = text(src, node.child_by_field_name("method"));
        let args = node.child_by_field_name("arguments");
        let first = args
            .and_then(|a| named(a).into_iter().next())
            .and_then(|n| literal(src, n));
        match method.as_deref() {
            Some("gem") => {
                if let Some(name) = first {
                    gem(src, args, name, path_block, facts);
                }
            }
            Some("gemspec") => {
                if let Some(path) = option(src, args, "path") {
                    facts.push(Fact::PathGem { path });
                }
            }
            Some("path") if node.child_by_field_name("block").is_some() => block_dir = first,
            _ => {}
        }
    }
    let dir = block_dir.as_deref().or(path_block);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        gemfile(src, child, dir, facts, depth + 1);
    }
}

/// One `gem "name", ...`: a local gem by its literal `path:` or its
/// `path "dir" do` block, else a remote gem with its literal `require:`.
fn gem(
    src: &[u8],
    args: Option<Node>,
    name: String,
    path_block: Option<&str>,
    facts: &mut Vec<Fact>,
) {
    let path_value = option_node(src, args, "path");
    let remote = ["git", "github", "gitlab", "bitbucket"]
        .iter()
        .any(|key| option_node(src, args, key).is_some());
    match (path_value.and_then(|v| literal(src, v)), path_block) {
        (Some(path), _) => facts.push(Fact::PathGem { path }),
        // A computed `path:` names a local gem the rules cannot place:
        // neither local nor external.
        (None, _) if path_value.is_some() => {}
        // In a `path "dir" do` block a gem lives in `dir/<name>`.
        (None, Some(dir)) if !remote => facts.push(Fact::PathGem {
            path: format!("{dir}/{name}"),
        }),
        _ => facts.push(Fact::Gem {
            name,
            require: option(src, args, "require"),
        }),
    }
}

/// v1 `walk_gemspec`: the first literal `x.name = "..."` and the first
/// literal `x.require_paths = [...]`, whatever the receiver is called.
fn gemspec(
    src: &[u8],
    node: Node,
    name: &mut Option<String>,
    paths: &mut Option<Vec<String>>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    if node.kind() == "assignment"
        && let Some(left) = node
            .child_by_field_name("left")
            .filter(|l| l.kind() == "call")
        && let Some(right) = node.child_by_field_name("right")
    {
        match text(src, left.child_by_field_name("method")).as_deref() {
            Some("name") => {
                if let Some(literal) = literal(src, right) {
                    name.get_or_insert(literal);
                }
            }
            Some("require_paths") => {
                let list = match right.kind() {
                    "array" => named(right)
                        .into_iter()
                        .map(|n| literal(src, n))
                        .collect::<Option<Vec<_>>>(),
                    _ => literal(src, right).map(|p| vec![p]),
                };
                if let Some(list) = list.filter(|l| !l.is_empty()) {
                    paths.get_or_insert(list);
                }
            }
            _ => {}
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        gemspec(src, child, name, paths, depth + 1);
    }
}

/// Named children but comments.
fn named(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|n| n.kind() != "comment")
        .collect()
}

fn text(src: &[u8], node: Option<Node>) -> Option<String> {
    node.map(|n| String::from_utf8_lossy(&src[n.byte_range()]).into_owned())
}

/// A string without interpolation, or a simple symbol (v1's manifest
/// `literal`: a quoted symbol `:"x"` is not one here).
fn literal(src: &[u8], node: Node) -> Option<String> {
    match node.kind() {
        "string" => {
            let mut out = String::new();
            let mut cursor = node.walk();
            for part in node.children(&mut cursor) {
                match part.kind() {
                    "string_content" => out.push_str(&text(src, Some(part))?),
                    "interpolation" => return None,
                    _ => {}
                }
            }
            Some(out)
        }
        "simple_symbol" => text(src, Some(node)).map(|t| t[1..].to_owned()),
        _ => None,
    }
}

/// The value of a `key: value` (or `:key => value`) argument.
fn option_node<'t>(src: &[u8], args: Option<Node<'t>>, key: &str) -> Option<Node<'t>> {
    named(args?).into_iter().find_map(|arg| {
        if arg.kind() != "pair" {
            return None;
        }
        let k = text(src, arg.child_by_field_name("key"))?;
        (k.trim_start_matches(':').trim_end_matches(':') == key)
            .then(|| arg.child_by_field_name("value"))
            .flatten()
    })
}

/// The literal value of a `key:` argument.
fn option(src: &[u8], args: Option<Node>, key: &str) -> Option<String> {
    option_node(src, args, key).and_then(|v| literal(src, v))
}

#[cfg(test)]
mod tests;
