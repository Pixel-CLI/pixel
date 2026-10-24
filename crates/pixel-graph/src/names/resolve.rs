// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Resolution for the name-resolved languages (`specs/graph.md`).
//!
//! The tiers of Pixel's `pixel-graph` `resolve.rs`, over a [`World`] in memory
//! and with their strength stated by [`Decision`]:
//!
//! - **T0**, a name defined in the caller's own file: `Proved` only when the
//!   call has no real receiver and exactly one definition of that name can
//!   be the callee; several candidates are `Likely`.
//! - **T1**, a name an import binds to a file: `Proved` when the one item the
//!   import names is defined once in the file it resolves to.
//! - **T2**, a name with exactly one defining file in the repository:
//!   `Likely`, never `Proved`.
//! - A **real receiver** (anything but `this`/`self`) caps every result at
//!   `Likely`: the receiver's type is not known.
//! - Otherwise `Open`: a site never fans out, and a tie is never broken by
//!   symbol order.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use pixel_model::{Edge, EdgeKind, Symbol, SymbolKind, leaf};

use super::imports::{Files, Resolved};
use crate::codec;
use crate::lang::{Decision, World};

/// What a language adds to the shared rules.
pub(super) struct Dialect {
    /// A call without a receiver reaches the methods of the enclosing type
    /// (Java, C#, Kotlin: an implicit `this`).
    pub bare_methods: bool,
    /// The file an import spec names, from the importer's path, and whether
    /// the spec proves it.
    pub resolve_import: fn(&str, &str, &Files) -> Option<Resolved>,
    /// A name with one defining file repository-wide is a `Likely` target
    /// (T2). The generic languages have no import resolution to back it.
    pub unique_name: bool,
}

/// A callable definition, by index in `World::symbols`.
#[derive(Clone, Copy)]
struct Candidate {
    file: usize,
    symbol: usize,
    kind: SymbolKind,
    line: u32,
}

/// One import binding a name: T1 accepts a definition named `source` in
/// `file` for a call on a line of `scope` (empty: anywhere in the file).
struct Target {
    file: usize,
    /// The import's spec proves the file; a suffix match does not.
    proved: bool,
    source: String,
    scope: Vec<(u32, u32)>,
}

fn callable(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Class | SymbolKind::Struct
    )
}

fn kind_priority(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Function => 0,
        SymbolKind::Method => 1,
        SymbolKind::Class => 2,
        SymbolKind::Struct => 3,
        _ => 9,
    }
}

/// The candidate a tie falls to: functions first, then methods, classes and
/// structs, then the earliest line.
fn best(cands: &[Candidate]) -> Option<&Candidate> {
    cands
        .iter()
        .min_by_key(|c| (kind_priority(c.kind), c.line, c.symbol))
}

/// True iff a call on `line` sees names bound for `scope`. A call whose
/// line is unknown sees only the file-wide ones.
fn in_scope(scope: &[(u32, u32)], line: u32) -> bool {
    scope.is_empty()
        || scope
            .iter()
            .any(|&(start, end)| start <= line && line <= end)
}

/// How many lines the range of `scope` holding `line` spans: the innermost
/// scope has the narrowest one. A file-wide scope spans everything.
fn scope_width(scope: &[(u32, u32)], line: u32) -> u32 {
    scope
        .iter()
        .find(|&&(start, end)| start <= line && line <= end)
        .map_or(u32::MAX, |&(start, end)| end - start)
}

/// True iff `receiver` is a real receiver expression: not absent and not a
/// self pseudo-receiver, which resolves against the enclosing type.
fn has_real_receiver(receiver: Option<&str>) -> bool {
    receiver.is_some_and(|r| {
        let r = r.trim();
        !r.is_empty() && !is_self(r)
    })
}

fn is_self(receiver: &str) -> bool {
    matches!(receiver.trim(), "self" | "Self" | "this" | "super")
}

/// True iff `receiver` is a plain identifier or a `.`/`::` path of them. A
/// chained expression (`words.iter().filter(..)`) names a value produced
/// elsewhere.
fn is_value_receiver(receiver: &str) -> bool {
    let r = receiver.trim();
    !r.is_empty()
        && r.replace("::", ".").split('.').all(|segment| {
            segment
                .chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || matches!(c, '_' | '$'))
                && segment
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '_' | '$'))
        })
}

/// The index of every file, keyed by path, with its symbols and imports.
struct Index<'w> {
    world: &'w World<'w>,
    dialect: &'w Dialect,
    files: HashMap<&'w str, usize>,
    /// The file of each symbol.
    file_of: Vec<Option<usize>>,
    /// The symbol of each edge's `src`, by uid.
    symbol_of_uid: HashMap<&'w str, usize>,
    /// Callable definitions by the last segment of their name.
    by_leaf: HashMap<&'w str, Vec<Candidate>>,
    /// The `File` symbol of each file.
    file_symbol: HashMap<usize, usize>,
    bindings: HashMap<(usize, String), Vec<Target>>,
    paths: Files<'w>,
}

/// One import as a file's facts record it.
pub(super) struct FactImport {
    pub spec: String,
    pub scope: Vec<(u32, u32)>,
    pub bindings: Vec<(String, String)>,
}

/// The facts of one file: its imports, one record each.
pub(super) fn encode_facts(imports: &[FactImport]) -> String {
    let records: Vec<Vec<String>> = imports
        .iter()
        .map(|import| {
            let mut fields = vec![
                import.spec.clone(),
                import
                    .scope
                    .iter()
                    .map(|(start, end)| format!("{start}-{end}"))
                    .collect::<Vec<_>>()
                    .join(","),
            ];
            for (local, source) in &import.bindings {
                fields.push(local.clone());
                fields.push(source.clone());
            }
            fields
        })
        .collect();
    codec::records(&records)
}

pub(super) fn decode_facts(facts: &str) -> Vec<FactImport> {
    codec::list(facts)
        .into_iter()
        .filter(|fields| fields.len() >= 2)
        .map(|fields| {
            let scope = fields[1]
                .split(',')
                .filter_map(|range| {
                    let (start, end) = range.split_once('-')?;
                    Some((start.parse().ok()?, end.parse().ok()?))
                })
                .collect();
            let bindings = fields[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|[local, source]| (local.clone(), source.clone()))
                .collect();
            FactImport {
                spec: fields[0].clone(),
                scope,
                bindings,
            }
        })
        .collect()
}

impl<'w> Index<'w> {
    fn build(world: &'w World<'w>, dialect: &'w Dialect) -> Self {
        let files: HashMap<&str, usize> = world
            .files
            .iter()
            .enumerate()
            .map(|(at, (path, _))| (path.as_str(), at))
            .collect();
        let paths = Files::new(world.files.iter().map(|(path, _)| path.as_str()));
        let mut file_of = Vec::with_capacity(world.symbols.len());
        let mut symbol_of_uid = HashMap::default();
        let mut by_leaf: HashMap<&str, Vec<Candidate>> = HashMap::default();
        let mut file_symbol = HashMap::default();
        for (at, symbol) in world.symbols.iter().enumerate() {
            let file = files.get(symbol.file.as_str()).copied();
            file_of.push(file);
            symbol_of_uid.insert(symbol.uid.as_str(), at);
            let Some(file) = file else { continue };
            if symbol.kind == SymbolKind::File {
                file_symbol.insert(file, at);
            }
            if callable(symbol.kind) {
                by_leaf
                    .entry(leaf(&symbol.name))
                    .or_default()
                    .push(Candidate {
                        file,
                        symbol: at,
                        kind: symbol.kind,
                        line: symbol.line,
                    });
            }
        }
        let mut bindings: HashMap<(usize, String), Vec<Target>> = HashMap::default();
        for (file, (path, facts)) in world.files.iter().enumerate() {
            for import in decode_facts(facts) {
                let Some((target, proved)) = (dialect.resolve_import)(&import.spec, path, &paths)
                    .and_then(|r| Some((files.get(r.path.as_str()).copied()?, r.proved)))
                else {
                    continue;
                };
                // An empty list is a wildcard or an unknown: no `Proved`.
                for (local, source) in import.bindings {
                    bindings.entry((file, local)).or_default().push(Target {
                        file: target,
                        proved,
                        source,
                        scope: import.scope.clone(),
                    });
                }
            }
        }
        Index {
            world,
            dialect,
            files,
            file_of,
            symbol_of_uid,
            by_leaf,
            file_symbol,
            bindings,
            paths,
        }
    }

    fn symbol(&self, at: usize) -> &Symbol {
        &self.world.symbols[at]
    }

    /// The imports that bind `name` in `file` and apply on `line`, innermost
    /// scope first.
    fn applicable(&self, file: usize, name: &str, line: u32) -> Vec<&Target> {
        let Some(targets) = self.bindings.get(&(file, name.to_owned())) else {
            return Vec::new();
        };
        let visible: Vec<&Target> = targets
            .iter()
            .filter(|t| in_scope(&t.scope, line))
            .collect();
        let Some(narrowest) = visible.iter().map(|t| scope_width(&t.scope, line)).min() else {
            return Vec::new();
        };
        visible
            .into_iter()
            .filter(|t| scope_width(&t.scope, line) == narrowest)
            .collect()
    }

    /// True iff an import in scope on `line` binds `name` as an alias of
    /// another name. The alias is what `name` means there, so a same-name
    /// definition elsewhere is never its target, not even as T2's sole one.
    fn binds_an_alias(&self, file: usize, name: &str, line: u32) -> bool {
        self.applicable(file, name, line)
            .iter()
            .any(|t| t.source != name)
    }

    fn defines_in_file(&self, file: usize, name: &str) -> bool {
        self.by_leaf
            .get(name)
            .is_some_and(|cands| cands.iter().any(|c| c.file == file))
    }

    /// T1: the definitions the imports binding `name` point at.
    fn import_tier(&self, file: usize, name: &str, line: u32) -> Option<Decision> {
        let targets = self.applicable(file, name, line);
        let items: HashSet<(usize, &str)> = targets
            .iter()
            .map(|t| (t.file, t.source.as_str()))
            .collect();
        let [(target_file, source)] = items.into_iter().collect::<Vec<_>>()[..] else {
            return (!targets.is_empty()).then_some(Decision::Open);
        };
        let proved = targets.iter().all(|t| t.proved);
        let hits: Vec<Candidate> = self
            .by_leaf
            .get(source)?
            .iter()
            .copied()
            .filter(|c| c.file == target_file)
            .collect();
        match hits.as_slice() {
            [] => None,
            [only] if proved => Some(Decision::Proved(only.symbol)),
            _ => best(&hits).map(|c| Decision::Likely(c.symbol)),
        }
    }

    /// The tier decision for a name, ignoring the receiver's type.
    fn decide_raw(&self, file: usize, name: &str, line: u32, reach_methods: bool) -> Decision {
        let cands = self.by_leaf.get(name).map_or(&[][..], Vec::as_slice);
        // T0: the caller's own file.
        let same_file: Vec<Candidate> = cands.iter().copied().filter(|c| c.file == file).collect();
        let lexical: Vec<Candidate> = same_file
            .iter()
            .copied()
            .filter(|c| reach_methods || c.kind != SymbolKind::Method)
            .collect();
        match lexical.as_slice() {
            [only] => return Decision::Proved(only.symbol),
            [] => {}
            _ => return best(&lexical).map_or(Decision::Open, |c| Decision::Likely(c.symbol)),
        }
        // T1: an item an import binds, defined in the file it resolves to.
        if let Some(decision) = self.import_tier(file, name, line) {
            return decision;
        }
        if self.binds_an_alias(file, name, line) {
            return Decision::Open;
        }
        // T2: one defining file repository-wide.
        let defining: HashSet<usize> = cands.iter().map(|c| c.file).collect();
        if self.dialect.unique_name
            && defining.len() == 1
            && let Some(c) = best(cands)
        {
            return Decision::Likely(c.symbol);
        }
        Decision::Open
    }

    /// The sole callable candidate of `name` whose qualified name starts with
    /// the receiver path's last segment (`Store` + `open` → `Store.open`).
    fn qualified_match(&self, receiver: &str, name: &str) -> Option<usize> {
        if !is_value_receiver(receiver) {
            return None;
        }
        let normalised = receiver.trim().replace("::", ".");
        let segment = normalised.rsplit('.').next()?;
        let mut hit = None;
        for cand in self.by_leaf.get(name)? {
            let qualified = self.symbol(cand.symbol).name.replace("::", ".");
            if qualified.starts_with(&format!("{segment}.")) {
                if hit.is_some() {
                    return None;
                }
                hit = Some(cand.symbol);
            }
        }
        hit
    }

    /// The graph's sole callable definition of `name`, when it is a method.
    fn sole_method(&self, name: &str) -> Option<usize> {
        let [candidate] = self.by_leaf.get(name)?.as_slice() else {
            return None;
        };
        (candidate.kind == SymbolKind::Method).then_some(candidate.symbol)
    }

    fn decide_call(&self, file: usize, name: &str, receiver: Option<&str>, line: u32) -> Decision {
        let real = has_real_receiver(receiver);
        let reach_methods = self.dialect.bare_methods || receiver.is_some_and(is_self);
        // A real receiver whose name is also defined in the caller's file:
        // T0 would point at the caller's same-name symbol instead of the
        // receiver's own callee, so the site is open unless the receiver
        // names the one type that defines it.
        if real && self.defines_in_file(file, name) {
            let receiver = receiver.unwrap_or_default();
            if let Some(at) = self.qualified_match(receiver, name) {
                return Decision::Likely(at);
            }
            if is_value_receiver(receiver)
                && let Some(at) = self.sole_method(name)
            {
                return Decision::Likely(at);
            }
            return Decision::Open;
        }
        let raw = self.decide_raw(file, name, line, reach_methods);
        if !real {
            return raw;
        }
        match raw {
            // A receiver of unknown type cannot confirm the definition.
            Decision::Proved(at) => Decision::Likely(at),
            Decision::Open => receiver
                .and_then(|r| self.qualified_match(r, name))
                .map_or(Decision::Open, Decision::Likely),
            likely @ Decision::Likely(_) => likely,
        }
    }

    fn decide_edge(&self, edge: &Edge) -> Decision {
        let Some(&caller) = self.symbol_of_uid.get(edge.src.as_str()) else {
            return Decision::Open;
        };
        let Some(file) = self.file_of[caller] else {
            return Decision::Open;
        };
        let fields = codec::fields(&edge.context);
        match (edge.kind, fields.first().map(String::as_str)) {
            (EdgeKind::Call, Some("c")) => {
                let name = fields.get(1).map_or("", String::as_str);
                let receiver = fields.get(2).map(String::as_str).filter(|r| !r.is_empty());
                self.decide_call(file, name, receiver, edge.line)
            }
            (EdgeKind::Reference, Some("r")) => {
                let name = fields.get(1).map_or("", String::as_str);
                self.decide_call(file, name, None, edge.line)
            }
            (EdgeKind::Import, Some("i")) => {
                let spec = fields.get(1).map_or("", String::as_str);
                let path = &self.world.files[file].0;
                (self.dialect.resolve_import)(spec, path, &self.paths)
                    .and_then(|r| Some((self.files.get(r.path.as_str()).copied()?, r.proved)))
                    .filter(|(target, _)| *target != file)
                    .and_then(|(target, proved)| {
                        let at = self.file_symbol.get(&target).copied()?;
                        Some(if proved {
                            Decision::Proved(at)
                        } else {
                            Decision::Likely(at)
                        })
                    })
                    .unwrap_or(Decision::Open)
            }
            _ => Decision::Open,
        }
    }
}

/// Decides every site of the language's world.
pub(super) fn decide(world: &World<'_>, dialect: &Dialect) -> Vec<Decision> {
    let index = Index::build(world, dialect);
    world
        .edges
        .iter()
        .map(|edge| index.decide_edge(edge))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::names::imports::resolve_ts;
    use pixel_model::Tier;

    const TS: Dialect = Dialect {
        bare_methods: false,
        resolve_import: resolve_ts,
        unique_name: true,
    };

    fn symbol(file: &str, name: &str, kind: SymbolKind, line: u32) -> Symbol {
        Symbol {
            uid: format!("{file}#{name}#{kind}"),
            file: file.to_owned(),
            kind,
            name: name.to_owned(),
            signature: String::new(),
            shape: String::new(),
            line,
            end_line: line + 2,
        }
    }

    fn file_symbol(file: &str) -> Symbol {
        symbol(file, file, SymbolKind::File, 1)
    }

    fn call(src: &Symbol, line: u32, name: &str, receiver: &str) -> Edge {
        Edge {
            src: src.uid.clone(),
            dst: None,
            kind: EdgeKind::Call,
            tier: Tier::Open,
            line,
            target: name.to_owned(),
            context: codec::record(&["c", name, receiver]),
        }
    }

    fn import(src: &Symbol, line: u32, spec: &str) -> Edge {
        Edge {
            src: src.uid.clone(),
            dst: None,
            kind: EdgeKind::Import,
            tier: Tier::Open,
            line,
            target: spec.to_owned(),
            context: codec::record(&["i", spec]),
        }
    }

    /// Decides `edges` over `files` (path, imports) and `symbols`.
    fn run(
        files: &[(&str, Vec<FactImport>)],
        symbols: &[Symbol],
        edges: &[Edge],
        dialect: &Dialect,
    ) -> Vec<Decision> {
        let files: Vec<(String, String)> = files
            .iter()
            .map(|(path, imports)| ((*path).to_owned(), encode_facts(imports)))
            .collect();
        decide(
            &World {
                files: &files,
                symbols,
                edges,
                unread: &[],
            },
            dialect,
        )
    }

    fn binding(spec: &str, names: &[(&str, &str)]) -> FactImport {
        FactImport {
            spec: spec.to_owned(),
            scope: Vec::new(),
            bindings: names
                .iter()
                .map(|(local, source)| ((*local).to_owned(), (*source).to_owned()))
                .collect(),
        }
    }

    /// The symbol index `decisions[i]` targets, for readable assertions.
    fn target(symbols: &[Symbol], decision: Decision) -> Option<(String, Tier)> {
        decision
            .target()
            .map(|(at, tier)| (symbols[at].name.clone(), tier))
    }

    #[test]
    fn a_bare_call_to_the_one_function_of_its_file_is_proved() {
        let main = symbol("a.ts", "main", SymbolKind::Function, 1);
        let helper = symbol("a.ts", "helper", SymbolKind::Function, 10);
        let symbols = [file_symbol("a.ts"), main.clone(), helper];
        let decided = run(
            &[("a.ts", vec![])],
            &symbols,
            &[call(&main, 3, "helper", "")],
            &TS,
        );
        assert_eq!(
            target(&symbols, decided[0]),
            Some(("helper".to_owned(), Tier::Proved))
        );
    }

    #[test]
    fn a_bare_call_never_reaches_a_method_but_this_does() {
        let run_it = symbol("a.ts", "Job.run", SymbolKind::Method, 5);
        let caller = symbol("a.ts", "main", SymbolKind::Function, 1);
        let symbols = [file_symbol("a.ts"), caller.clone(), run_it];
        let edges = [call(&caller, 2, "run", ""), call(&caller, 3, "run", "this")];
        let decided = run(&[("a.ts", vec![])], &symbols, &edges, &TS);
        // The bare call finds the method only through T2, as a guess.
        assert_eq!(
            target(&symbols, decided[0]),
            Some(("Job.run".to_owned(), Tier::Likely))
        );
        assert_eq!(
            target(&symbols, decided[1]),
            Some(("Job.run".to_owned(), Tier::Proved))
        );
    }

    #[test]
    fn the_same_name_twice_in_a_file_is_a_guess_not_a_proof() {
        let a = symbol("a.ts", "A.render", SymbolKind::Method, 2);
        let b = symbol("a.ts", "B.render", SymbolKind::Method, 20);
        let caller = symbol("a.ts", "main", SymbolKind::Function, 40);
        let symbols = [file_symbol("a.ts"), a, b, caller.clone()];
        let decided = run(
            &[("a.ts", vec![])],
            &symbols,
            &[call(&caller, 41, "render", "this")],
            &TS,
        );
        assert_eq!(
            target(&symbols, decided[0]),
            Some(("A.render".to_owned(), Tier::Likely))
        );
    }

    #[test]
    fn a_named_import_proves_the_item_in_the_file_it_resolves_to() {
        let greet = symbol("lib/a.ts", "greet", SymbolKind::Function, 1);
        let other = symbol("lib/b.ts", "greet", SymbolKind::Function, 1);
        let main = symbol("main.ts", "main", SymbolKind::Function, 3);
        let symbols = [
            file_symbol("lib/a.ts"),
            greet,
            file_symbol("lib/b.ts"),
            other,
            file_symbol("main.ts"),
            main.clone(),
        ];
        let decided = run(
            &[
                ("lib/a.ts", vec![]),
                ("lib/b.ts", vec![]),
                ("main.ts", vec![binding("./lib/a", &[("greet", "greet")])]),
            ],
            &symbols,
            &[call(&main, 4, "greet", "")],
            &TS,
        );
        let proved = decided[0].target().unwrap();
        assert_eq!(symbols[proved.0].file, "lib/a.ts");
        assert_eq!(proved.1, Tier::Proved);
    }

    #[test]
    fn an_alias_means_its_source_and_never_falls_back_to_a_same_name_definition() {
        let push = symbol("lib/a.ts", "push", SymbolKind::Function, 1);
        let leased = symbol("lib/c.ts", "leased", SymbolKind::Function, 1);
        let main = symbol("main.ts", "main", SymbolKind::Function, 3);
        let symbols = [
            file_symbol("lib/a.ts"),
            push,
            file_symbol("lib/c.ts"),
            leased,
            file_symbol("main.ts"),
            main.clone(),
        ];
        let files = [
            ("lib/a.ts", vec![]),
            ("lib/c.ts", vec![]),
            ("main.ts", vec![binding("./lib/a", &[("leased", "push")])]),
        ];
        let decided = run(&files, &symbols, &[call(&main, 4, "leased", "")], &TS);
        // `leased` is `push` here, and `lib/c.ts`'s own `leased` is not it.
        assert_eq!(
            target(&symbols, decided[0]),
            Some(("push".to_owned(), Tier::Proved))
        );
        // The same alias where the source is not defined there stays open.
        let missing = [
            ("lib/a.ts", vec![]),
            ("lib/c.ts", vec![]),
            ("main.ts", vec![binding("./lib/a", &[("leased", "gone")])]),
        ];
        let decided = run(&missing, &symbols, &[call(&main, 4, "leased", "")], &TS);
        assert_eq!(decided[0], Decision::Open);
    }

    #[test]
    fn a_unique_name_elsewhere_is_likely_and_two_defining_files_stay_open() {
        let util = symbol("lib/util.ts", "slug", SymbolKind::Function, 1);
        let main = symbol("main.ts", "main", SymbolKind::Function, 3);
        let symbols = [
            file_symbol("lib/util.ts"),
            util,
            file_symbol("main.ts"),
            main.clone(),
        ];
        let files = [("lib/util.ts", vec![]), ("main.ts", vec![])];
        let decided = run(&files, &symbols, &[call(&main, 4, "slug", "")], &TS);
        assert_eq!(
            target(&symbols, decided[0]),
            Some(("slug".to_owned(), Tier::Likely))
        );
        let twin = symbol("lib/other.ts", "slug", SymbolKind::Function, 1);
        let symbols = [
            file_symbol("lib/util.ts"),
            symbol("lib/util.ts", "slug", SymbolKind::Function, 1),
            file_symbol("lib/other.ts"),
            twin,
            file_symbol("main.ts"),
            main.clone(),
        ];
        let files = [
            ("lib/util.ts", vec![]),
            ("lib/other.ts", vec![]),
            ("main.ts", vec![]),
        ];
        let decided = run(&files, &symbols, &[call(&main, 4, "slug", "")], &TS);
        assert_eq!(decided[0], Decision::Open);
    }

    #[test]
    fn a_real_receiver_caps_the_result_at_likely_and_never_uses_the_shadow() {
        let save_a = symbol("a.ts", "Repo.save", SymbolKind::Method, 5);
        let main = symbol("a.ts", "main", SymbolKind::Function, 1);
        let save_b = symbol("b.ts", "Cache.save", SymbolKind::Method, 5);
        let symbols = [
            file_symbol("a.ts"),
            main.clone(),
            save_a,
            file_symbol("b.ts"),
            save_b,
        ];
        let files = [("a.ts", vec![]), ("b.ts", vec![])];
        // `cache.save()`: `save` is defined in this file and elsewhere. The
        // receiver names no type the graph knows, so the site stays open.
        let decided = run(&files, &symbols, &[call(&main, 3, "save", "cache")], &TS);
        assert_eq!(decided[0], Decision::Open);
        // `Cache.save()`: the receiver is the type of exactly one candidate.
        let decided = run(&files, &symbols, &[call(&main, 3, "save", "Cache")], &TS);
        assert_eq!(
            target(&symbols, decided[0]),
            Some(("Cache.save".to_owned(), Tier::Likely))
        );
    }

    #[test]
    fn a_proved_target_behind_a_real_receiver_is_downgraded() {
        let helper = symbol("a.ts", "helper", SymbolKind::Function, 9);
        let main = symbol("a.ts", "main", SymbolKind::Function, 1);
        let symbols = [file_symbol("a.ts"), main.clone(), helper];
        // `obj.helper()` with `helper` defined in another file only.
        let other = symbol("b.ts", "helper", SymbolKind::Function, 1);
        let symbols_elsewhere = [
            file_symbol("a.ts"),
            main.clone(),
            file_symbol("b.ts"),
            other,
        ];
        let files = [("a.ts", vec![]), ("b.ts", vec![])];
        let decided = run(
            &files,
            &symbols_elsewhere,
            &[call(&main, 3, "helper", "obj")],
            &TS,
        );
        assert_eq!(
            target(&symbols_elsewhere, decided[0]),
            Some(("helper".to_owned(), Tier::Likely))
        );
        // The same name in the caller's own file is the shadow: open.
        let decided = run(
            &[("a.ts", vec![])],
            &symbols,
            &[call(&main, 3, "helper", "obj")],
            &TS,
        );
        assert_eq!(decided[0], Decision::Open);
    }

    #[test]
    fn a_bare_method_dialect_reaches_the_enclosing_types_methods() {
        let java = Dialect {
            bare_methods: true,
            resolve_import: resolve_ts,
            unique_name: true,
        };
        let run_it = symbol("A.ts", "Job.run", SymbolKind::Method, 5);
        let caller = symbol("A.ts", "Job.start", SymbolKind::Method, 2);
        let symbols = [file_symbol("A.ts"), caller.clone(), run_it];
        let decided = run(
            &[("A.ts", vec![])],
            &symbols,
            &[call(&caller, 3, "run", "")],
            &java,
        );
        assert_eq!(
            target(&symbols, decided[0]),
            Some(("Job.run".to_owned(), Tier::Proved))
        );
    }

    #[test]
    fn an_import_site_is_proved_to_the_file_symbol_it_resolves_to() {
        let main = symbol("main.ts", "main", SymbolKind::Function, 3);
        let symbols = [
            file_symbol("lib/a.ts"),
            file_symbol("main.ts"),
            main.clone(),
        ];
        let files = [("lib/a.ts", vec![]), ("main.ts", vec![])];
        let edges = [
            import(&symbols[1], 1, "./lib/a"),
            import(&symbols[1], 2, "react"),
            import(&symbols[1], 3, "./gone"),
            import(&symbols[0], 1, "../main"),
        ];
        let decided = run(&files, &symbols, &edges, &TS);
        assert_eq!(decided[0], Decision::Proved(0));
        assert_eq!(decided[1], Decision::Open);
        assert_eq!(decided[2], Decision::Open);
        assert_eq!(decided[3], Decision::Proved(1));
    }

    #[test]
    fn an_edge_whose_source_is_unknown_is_open() {
        let ghost = symbol("x.ts", "ghost", SymbolKind::Function, 1);
        let decided = run(
            &[("a.ts", vec![])],
            &[file_symbol("a.ts")],
            &[call(&ghost, 1, "f", "")],
            &TS,
        );
        assert_eq!(decided[0], Decision::Open);
    }

    #[test]
    fn facts_round_trip_with_scopes_and_bindings() {
        let imports = vec![
            FactImport {
                spec: "./a".to_owned(),
                scope: vec![(1, 9), (20, 30)],
                bindings: vec![("x".to_owned(), "y".to_owned())],
            },
            FactImport {
                spec: "./b\u{1f}weird".to_owned(),
                scope: Vec::new(),
                bindings: Vec::new(),
            },
        ];
        let back = decode_facts(&encode_facts(&imports));
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].spec, "./a");
        assert_eq!(back[0].scope, [(1, 9), (20, 30)]);
        assert_eq!(back[0].bindings, [("x".to_owned(), "y".to_owned())]);
        assert_eq!(back[1].spec, "./b\u{1f}weird");
        assert!(back[1].scope.is_empty() && back[1].bindings.is_empty());
        assert!(decode_facts("").is_empty());
    }

    #[test]
    fn scope_helpers_pick_the_innermost_range() {
        let scope = [(1, 100), (10, 20)];
        assert!(in_scope(&scope, 15));
        assert!(!in_scope(&[(10, 20)], 25));
        assert!(in_scope(&[], 25));
        assert_eq!(scope_width(&scope, 15), 99);
        assert_eq!(scope_width(&[(10, 20)], 15), 10);
        assert_eq!(scope_width(&[(10, 20)], 50), u32::MAX);
    }

    #[test]
    fn receiver_shapes() {
        assert!(is_value_receiver("w"));
        assert!(is_value_receiver("a.b.c"));
        assert!(is_value_receiver("crate::store"));
        assert!(!is_value_receiver("words.iter()"));
        assert!(!is_value_receiver(""));
        assert!(!has_real_receiver(None));
        assert!(!has_real_receiver(Some("this")));
        assert!(has_real_receiver(Some("service")));
    }
}
