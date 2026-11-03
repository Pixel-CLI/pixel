// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Which files are Rust's (v1's file gate, `extract.rs:151-266`) and what a
//! `Cargo.toml` declares: the package, its `[lib]` and its `[[bin]]`s.
//!
//! `Cargo.toml` is read by [`read`], written for the few keys resolution
//! needs over the TOML subset of `toml`, because `mvp_index` takes no TOML
//! crate (ADR-0013's table). It reads `[package]`, `[lib]` and `[[bin]]`
//! tables and, in them, `key = "string"` or `key = 'string'` statements;
//! multi-line strings and arrays are skipped whole, so a line inside one is
//! never read as a key (D41). It reads every dependency table (D22):
//! `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`, their
//! `[target.….…]` forms and `[workspace.dependencies]`, each dependency as
//! `key = "version"`, `key = { … }`, dotted `key.field = …` or a
//! `[dependencies.key]` table, of which it keeps `path`, `package` and
//! `workspace`. It does not read: dotted keys for the package's own fields
//! (`package.name = …`), inline tables for `[lib]` (`lib = { … }`), a
//! multi-line string's value, `[[test]]`, `[[example]]`, `[[bench]]`,
//! `package.build`, the `autobins`-style switches, inherited package fields,
//! `[patch]` and `[replace]` (a patched registry dependency stays outside
//! the workspace). Each gap leaves a path unresolved, never resolved to the
//! wrong crate: a workspace library is a crate name only through a path
//! dependency the reader read.

use mvp_model::SymbolKind;

use rustc_hash::FxHashMap as HashMap;
use std::borrow::Cow;

use super::toml::{self, Statement, Value};
use super::{DepKind, DepSource, Dependency, Manifest};
use crate::mvp_index::lang::{Def, Extraction};
use crate::mvp_index::text;

pub use crate::mvp_index::guard::is_generated_blob;

/// A `*.rs` file (the extension of the file name, not of a directory:
/// v1's G-1), or a `Cargo.toml`.
pub fn wants(path: &str) -> bool {
    is_manifest(path)
        || file_name(path)
            .rsplit_once('.')
            .is_some_and(|(_, ext)| ext == "rs")
}

pub fn is_manifest(path: &str) -> bool {
    file_name(path) == "Cargo.toml"
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Refuses a generated blob (v1's G-2): deterministic, a function of the
/// bytes alone, so the same tree always gives the same rows.
pub fn accepts(path: &str, content: &[u8]) -> bool {
    is_manifest(path) || !is_generated_blob(content)
}

/// A manifest's `Package` symbol and its facts.
pub fn extract(content: &[u8]) -> Extraction {
    let text = String::from_utf8_lossy(content);
    let (manifest, name_line) = read(&text);
    let mut defs = Vec::new();
    if let (Some(name), Some(line)) = (&manifest.package, name_line) {
        defs.push(Def {
            kind: SymbolKind::Package,
            name: name.clone(),
            signature: text::signature(text::line(&text, line).unwrap_or("")),
            line,
            end_line: line,
        });
    }
    Extraction {
        defs,
        sites: Vec::new(),
        facts: manifest.encode(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Table<'t> {
    Package,
    Lib,
    Bin,
    /// `[dependencies]` and its kin: entries are dependencies.
    Dependencies(DepKind),
    /// `[dependencies.key]`: entries are the fields of one dependency.
    Dependency(DepKind, Cow<'t, str>),
    /// `[workspace.dependencies]`.
    WorkspaceDependencies,
    /// `[workspace.dependencies.key]`.
    WorkspaceDependency(Cow<'t, str>),
    Other,
}

/// The kind of dependency table a header's path names, before any key:
/// `dependencies`, `dev-dependencies`, `build-dependencies` (and their
/// underscored spellings), alone or under `target.<platform>`.
fn dependency_table<'p, 't>(path: &'p [Cow<'t, str>]) -> Option<(DepKind, &'p [Cow<'t, str>])> {
    let kind = |name: &str| match name {
        "dependencies" => Some(DepKind::Normal),
        "dev-dependencies" | "dev_dependencies" => Some(DepKind::Dev),
        "build-dependencies" | "build_dependencies" => Some(DepKind::Build),
        _ => None,
    };
    match path {
        [first, rest @ ..] if first.as_ref() == "target" => match rest {
            [_, name, key @ ..] => Some((kind(name)?, key)),
            _ => None,
        },
        [name, key @ ..] => Some((kind(name)?, key)),
        [] => None,
    }
}

fn table_of<'t>(path: &[Cow<'t, str>], array: bool) -> Table<'t> {
    let words: Vec<&str> = path.iter().map(AsRef::as_ref).collect();
    match (words.as_slice(), array) {
        (["package"], false) => Table::Package,
        (["lib"], false) => Table::Lib,
        (["bin"], true) => Table::Bin,
        (["workspace", "dependencies"], false) => Table::WorkspaceDependencies,
        (["workspace", "dependencies", _], false) => Table::WorkspaceDependency(path[2].clone()),
        (_, false) => match dependency_table(path) {
            Some((kind, [])) => Table::Dependencies(kind),
            Some((kind, [key])) => Table::Dependency(kind, key.clone()),
            _ => Table::Other,
        },
        _ => Table::Other,
    }
}

/// A dependency as its statements declare it, before its source is known.
struct Declared {
    kind: DepKind,
    key: String,
    package: Option<String>,
    path: Option<String>,
    workspace: bool,
    /// One of the workspace's `[workspace.dependencies]`.
    inherited: bool,
}

impl Declared {
    fn set(&mut self, field: &str, value: Value) {
        match (field, value) {
            ("package", Value::Str(package)) => self.package = Some(package.into_owned()),
            ("path", Value::Str(path)) => self.path = Some(path.into_owned()),
            ("workspace", Value::Bool(on)) => self.workspace = on,
            _ => {}
        }
    }

    fn finish(self) -> Dependency {
        let source = match (self.workspace, self.path) {
            (true, _) => DepSource::Workspace,
            (false, Some(path)) => DepSource::Path(path),
            (false, None) => DepSource::Other,
        };
        Dependency {
            kind: self.kind,
            key: self.key,
            package: self.package,
            source,
        }
    }
}

/// What `text` declares, and the line of `[package] name`.
pub fn read(text: &str) -> (Manifest, Option<u32>) {
    let mut manifest = Manifest::default();
    let mut name_line = None;
    let mut table = Table::Other;
    let mut declared: Vec<Declared> = Vec::new();
    // The dependency each key of the current header names, so that
    // `key.path = …` and `[dependencies.key]` fields join the dependency
    // they belong to; its key is given it when the header ends: a
    // manifest may declare thousands.
    let mut by_key: HashMap<Cow<str>, usize> = HashMap::default();
    let keys = |by_key: &mut HashMap<Cow<str>, usize>, declared: &mut Vec<Declared>| {
        for (key, index) in by_key.drain() {
            declared[index].key = key.into_owned();
        }
    };
    for statement in toml::statements(text) {
        let (key, value, line) = match statement {
            Statement::Table { path, array } => {
                keys(&mut by_key, &mut declared);
                if path
                    .first()
                    .is_some_and(|first| first.as_ref() == "workspace")
                {
                    manifest.workspace = true;
                }
                table = table_of(&path, array);
                if table == Table::Bin {
                    manifest.bins.push((String::new(), String::new()));
                }
                continue;
            }
            Statement::Entry { key, value, line } => (key, value, line),
        };
        // A dependency and the field set.
        let mut key = key;
        let (kind, inherited, name, field) = match (&table, key.as_mut_slice()) {
            (Table::Dependencies(kind), [name, field @ ..]) => {
                (*kind, false, std::mem::take(name), &*field)
            }
            (Table::WorkspaceDependencies, [name, field @ ..]) => {
                (DepKind::Normal, true, std::mem::take(name), &*field)
            }
            (Table::Dependency(kind, name), field) => (*kind, false, name.clone(), &*field),
            (Table::WorkspaceDependency(name), field) => {
                (DepKind::Normal, true, name.clone(), &*field)
            }
            _ => {
                package_entry(&mut manifest, &mut name_line, &table, &key, value, line);
                continue;
            }
        };
        let index = match by_key.get(name.as_ref()) {
            Some(index) => *index,
            None => {
                declared.push(Declared {
                    kind,
                    key: String::new(),
                    package: None,
                    path: None,
                    workspace: false,
                    inherited,
                });
                by_key.insert(name, declared.len() - 1);
                declared.len() - 1
            }
        };
        match (field, value) {
            ([], Value::Table(fields)) => {
                for (field, value) in fields {
                    if let [field] = field.as_slice() {
                        declared[index].set(field, value);
                    }
                }
            }
            ([field], value) => declared[index].set(field, value),
            _ => {}
        }
    }
    keys(&mut by_key, &mut declared);
    for dependency in declared {
        if dependency.inherited {
            manifest.workspace_dependencies.push(dependency.finish());
        } else {
            manifest.dependencies.push(dependency.finish());
        }
    }
    (manifest, name_line)
}

/// A `key = "string"` of `[package]`, `[lib]` or `[[bin]]`.
fn package_entry(
    manifest: &mut Manifest,
    name_line: &mut Option<u32>,
    table: &Table,
    key: &[Cow<str>],
    value: Value,
    line: u32,
) {
    let ([key], Value::Str(value)) = (key, value) else {
        return;
    };
    let value = value.into_owned();
    match (table, key.as_ref()) {
        (Table::Package, "name") => {
            manifest.package = Some(value);
            *name_line = Some(line);
        }
        (Table::Package, "workspace") => manifest.workspace_root = Some(value),
        (Table::Lib, "name") => manifest.lib_name = Some(value),
        (Table::Lib, "path") => manifest.lib_path = Some(value),
        (Table::Bin, "name") => {
            if let Some(bin) = manifest.bins.last_mut() {
                bin.0 = value;
            }
        }
        (Table::Bin, "path") => {
            if let Some(bin) = manifest.bins.last_mut() {
                bin.1 = value;
            }
        }
        _ => {}
    }
}
