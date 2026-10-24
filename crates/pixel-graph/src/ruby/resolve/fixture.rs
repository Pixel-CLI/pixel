// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Hand-built Ruby generations for the resolution tests: the rows
//! extraction stores, written one by one, then decided by [`resolve`].
//!
//! Each builder adds exactly the row its name says. A file's `File` symbol
//! (what owns its top-level code) is added the first time the file is
//! named. A site's source is named by the symbol's name in that file (the
//! path for the `File` symbol); a site's context is the [`Context`] of the
//! contract in `ruby/mod.rs`.

use rustc_hash::FxHashMap as HashMap;

use pixel_model::{Edge, EdgeKind, Symbol, SymbolKind};

use super::resolve;
use crate::extract::uid;
use crate::lang::World;
use crate::ruby::{ASSOCIATION, Context, Fact, MIXIN, ROUTE};

#[derive(Default)]
pub struct Generation {
    files: Vec<(String, String)>,
    symbols: Vec<Symbol>,
    edges: Vec<Edge>,
    repetitions: HashMap<(String, String, SymbolKind), u32>,
}

impl Generation {
    /// A file with no definition: only its `File` symbol.
    pub fn file(&mut self, path: &str) -> &mut Self {
        self.add_file(path);
        self
    }

    /// A definition of `kind` named `name` in `path`.
    pub fn def(
        &mut self,
        path: &str,
        kind: SymbolKind,
        name: &str,
        line: u32,
        end_line: u32,
    ) -> &mut Self {
        self.add_file(path);
        self.add_symbol(path, kind, name, line, end_line);
        self
    }

    /// A top-level `def`.
    pub fn function(&mut self, path: &str, name: &str, line: u32, end_line: u32) -> &mut Self {
        self.def(path, SymbolKind::Function, name, line, end_line)
    }

    /// The manifest facts of `path` (a Gemfile, a gemspec, a lockfile),
    /// replacing any it had.
    pub fn facts(&mut self, path: &str, facts: &[Fact]) -> &mut Self {
        self.add_file(path);
        let file = self.files.iter_mut().find(|(p, _)| p == path);
        file.expect("file added").1 = Fact::encode_all(facts);
        self
    }

    /// A call of `name` on `receiver` (`""` for none) from `src`.
    pub fn call(
        &mut self,
        path: &str,
        src: &str,
        line: u32,
        receiver: &str,
        name: &str,
        cref: &[&str],
    ) -> usize {
        let target = match receiver {
            "" => name.to_owned(),
            receiver => format!("{receiver}.{name}"),
        };
        let context = context(name, receiver, cref, &[]);
        self.site(path, src, EdgeKind::Call, line, target, context)
    }

    /// A method reference: `qualifier` is a callback (`before_action`),
    /// `send`, `public_send`, `ALIAS` or `DELEGATE`.
    pub fn reference(
        &mut self,
        path: &str,
        src: &str,
        line: u32,
        qualifier: &str,
        name: &str,
        cref: &[&str],
    ) -> usize {
        let context = context(name, qualifier, cref, &[]);
        self.site(
            path,
            src,
            EdgeKind::Reference,
            line,
            name.to_owned(),
            context,
        )
    }

    /// An ancestor declaration of `owner`: `kind` is `superclass`,
    /// `include`, `included:include`, ...; `cref` is where Ruby evaluates
    /// `target` (outside the class for a superclass).
    pub fn mixin(
        &mut self,
        path: &str,
        owner: &str,
        line: u32,
        kind: &str,
        target: &str,
        cref: &[&str],
    ) -> usize {
        let context = context(target, &format!("{MIXIN}{kind}"), cref, &[]);
        let kind = EdgeKind::Reference;
        self.site(path, owner, kind, line, target.to_owned(), context)
    }

    /// A route of the routes file `path` to `controller#action`.
    pub fn route(&mut self, path: &str, line: u32, controller: &str, action: &str) -> usize {
        let qualifier = format!("{ROUTE}{controller}");
        let context = context(action, &qualifier, &[], &["GET", "/"]);
        let target = format!("{controller}#{action}");
        self.site(path, path, EdgeKind::Route, line, target, context)
    }

    /// An association `macro_` of `owner` naming the class `class_path`.
    pub fn association(
        &mut self,
        path: &str,
        owner: &str,
        line: u32,
        macro_: &str,
        class_path: &str,
    ) -> usize {
        let qualifier = format!("{ASSOCIATION}{macro_}");
        let context = context(class_path, &qualifier, &[], &[]);
        let target = class_path.to_owned();
        self.site(path, owner, EdgeKind::Association, line, target, context)
    }

    /// `require "feature"` from `path`.
    pub fn require(&mut self, path: &str, line: u32, feature: &str) -> usize {
        self.import(path, line, "require", feature, feature)
    }

    /// A load of `spec` through `method`, stored with the path extraction
    /// computes: `./x` for `require_relative "x"`, `""` for `require "./x"`.
    pub fn import(&mut self, path: &str, line: u32, method: &str, spec: &str, load: &str) -> usize {
        let context = context(spec, method, &[], &[load]);
        self.site(path, path, EdgeKind::Import, line, spec.to_owned(), context)
    }

    /// The rows, as resolution reads them.
    pub fn world(&self) -> World<'_> {
        World {
            files: &self.files,
            symbols: &self.symbols,
            edges: &self.edges,
            unread: &[],
        }
    }

    /// For each site, the uid of the symbol it resolves to.
    pub fn resolve(&self) -> Vec<Option<String>> {
        let decided = resolve(&self.world());
        assert_eq!(decided.len(), self.edges.len(), "one decision per site");
        decided
            .into_iter()
            .map(|target| target.map(|i| self.symbols[i].uid.clone()))
            .collect()
    }

    /// The same rows with the files in reverse order, each file's rows in
    /// their own order: how an incremental run, whose fresh files come
    /// after the kept ones, can hand them to the rules.
    pub fn files_reversed(&self) -> Generation {
        let paths: Vec<&String> = self.files.iter().rev().map(|(path, _)| path).collect();
        let file_of: HashMap<&str, &str> = self
            .symbols
            .iter()
            .map(|s| (s.uid.as_str(), s.file.as_str()))
            .collect();
        let mut reversed = Generation::default();
        for path in paths {
            let in_file = |s: &&Symbol| &s.file == path;
            reversed
                .files
                .extend(self.files.iter().filter(|(p, _)| p == path).cloned());
            reversed
                .symbols
                .extend(self.symbols.iter().filter(in_file).cloned());
            let edges = self
                .edges
                .iter()
                .filter(|e| file_of[e.src.as_str()] == path.as_str());
            reversed.edges.extend(edges.cloned());
        }
        reversed
    }

    /// Every site as `(source uid, line, target as written, target uid)`,
    /// sorted: the rows a run stores, whatever order it decided them in.
    pub fn decisions(&self) -> Vec<(String, u32, String, Option<String>)> {
        let mut rows: Vec<_> = self
            .edges
            .iter()
            .zip(self.resolve())
            .map(|(e, dst)| (e.src.clone(), e.line, e.target.clone(), dst))
            .collect();
        rows.sort();
        rows
    }

    fn add_file(&mut self, path: &str) {
        if self.files.iter().any(|(p, _)| p == path) {
            return;
        }
        self.files.push((path.to_owned(), String::new()));
        self.add_symbol(path, SymbolKind::File, path, 1, 1000);
    }

    fn add_symbol(&mut self, path: &str, kind: SymbolKind, name: &str, line: u32, end_line: u32) {
        let key = (path.to_owned(), name.to_owned(), kind);
        let count = self.repetitions.entry(key).or_insert(0);
        *count += 1;
        self.symbols.push(Symbol {
            uid: uid(path, name, kind, *count),
            file: path.to_owned(),
            kind,
            name: name.to_owned(),
            signature: String::new(),
            shape: String::new(),
            line,
            end_line,
        });
    }

    fn site(
        &mut self,
        path: &str,
        src: &str,
        kind: EdgeKind,
        line: u32,
        target: String,
        context: Context,
    ) -> usize {
        self.add_file(path);
        let src = self
            .symbols
            .iter()
            .rev()
            .find(|s| s.file == path && s.name == src)
            .unwrap_or_else(|| panic!("no symbol {src} in {path}"))
            .uid
            .clone();
        self.edges.push(Edge {
            src,
            dst: None,
            kind,
            tier: pixel_model::Tier::Open,
            line,
            target,
            context: context.encode(),
        });
        self.edges.len() - 1
    }
}

fn context(name: &str, qualifier: &str, cref: &[&str], extra: &[&str]) -> Context {
    let owned = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect();
    Context {
        name: name.to_owned(),
        qualifier: qualifier.to_owned(),
        cref: owned(cref),
        extra: owned(extra),
    }
}
