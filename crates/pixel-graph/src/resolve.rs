// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Tiered call-graph resolution.
//!
//! Tiers (a name NEVER fans out to multiple definition sites as edges):
//! - T0: callee defined in the same file → `Exact`
//! - T1: callee defined in exactly one file the caller imports → `Exact`
//! - T2: callee name defined in exactly one file repo-wide → `Probable`
//! - otherwise → `unresolved_calls` row (feeds the epistemic envelope)
//!
//! Receiver honesty: a call with a real receiver expression (`x.parse()`,
//! `SymbolKind::parse`) can never be `Exact` from name-only resolution — the
//! receiver's type is not tracked, so linking it to a same-name function/
//! method would be a guess. Name-only matches are capped at `Probable`; Ruby
//! constant receivers can prove a unique owner through lexical lookup. Calls whose
//! receiver is `self`/`Self`/`this` (or absent) keep the normal tier, since
//! those resolve against the enclosing type's own methods.
//!
//! Two receiver-shaped tiebreaks extend the receiver rules, both capped at
//! `Probable`. A receiver path whose last segment names the type of exactly
//! one candidate (`pixel_git::GitRunner::new` ↔ `GitRunner::new`) links to
//! it where the name tiers would otherwise stay unresolved or shadow-vetoed;
//! the receiver names the implementing type, so a trait-impl candidate counts
//! here (`Options::default()`). And when the graph holds exactly one callable
//! definition of the name, it sits in the caller's own file as an inherent
//! method, and the receiver is a value/path (`w.push_call()`, `idx.decide()`),
//! that sole candidate is returned — no other definition exists to shadow it,
//! and a value receiver never names a trait implementor.
//!
//! A real receiver whose callee name is also defined in the caller's own file
//! is otherwise `Unresolved`: T0 would link the call
//! (`pixel_graph::build::build_graph` inside `api.rs`) to the caller's own
//! same-name symbol — a shadow, not the callee. The unresolved row keeps the
//! envelope honest (`lower_bound`, `unresolved_same_name`) instead of an edge
//! to the wrong definition.
//!
//! A third receiver rule, also capped at `Probable`, reads a receiver path as
//! a module (`pixel_rank::signals::is_test_path`, `gitsync::blob_size`): the
//! free function of that name in the one Rust file whose module path ends
//! with the receiver's segments (exactly, from the caller's module, for a
//! `crate::`/`self::`/`super::` path). The extractor also records the stated type
//! of a local receiver in place of the expression (`runner.current_branch()`
//! with `runner: &GitRunner` is stored with receiver `GitRunner`), which is
//! what lets the type tiebreak pick `GitRunner::current_branch` among
//! same-name functions.
//!
//! T1 matches on the names an import binds (`imports.bindings`), not on the
//! file it resolves to: a wildcard or file-level import proves no binding.
//! Under an alias the call site writes the importer's local name while the
//! candidate carries the source name (`use a::push as leased;` →
//! `leased()` calls `push`), so T1 looks the local name up and matches
//! candidates on the source; the source name alone is not in scope there.
//!
//! Ruby constant receivers use lexical class/module scopes instead of name tiers.
//! Their unique methods are Exact; Rails dispatch conventions remain Probable.

use std::collections::{HashMap, HashSet};

use rusqlite::params;

use crate::extract::ruby_callbacks::{ReferenceKind, reference_kind};
use crate::store::{
    EdgeKind, EdgeRow, ExecCached, GraphStore, StoreError, SymbolKind, Tier, decode_bindings,
    decode_scope,
};

mod ruby;

#[derive(Debug, Default, Clone)]
pub struct ResolveStats {
    pub exact: u64,
    pub probable: u64,
    pub unresolved: u64,
}

/// One extracted call site awaiting resolution (symbol ids already assigned).
#[derive(Debug, Clone)]
pub struct PendingCall {
    pub callee_name: String,
    pub enclosing_symbol_id: Option<i64>,
    pub site_line: u32,
    /// Receiver expression text if this is a method/field call (`x.m()`,
    /// `a::b()`), else `None` for a plain call (`m()`). Used to cap
    /// non-`self` receiver calls at `Probable`.
    pub receiver: Option<String>,
}

/// All pending calls of one file.
#[derive(Debug, Clone)]
pub struct FileCalls {
    pub file_id: i64,
    pub calls: Vec<PendingCall>,
}

/// One extracted callback/reference site awaiting resolution (symbol ids
/// already assigned). A symbol passed as an argument to a call (e.g.
/// `schema.plugin(tenantScopePlugin)`). Resolves to a `References` edge,
/// which is weaker than `Calls` — it means "may be invoked", not "directly
/// called". All resolved references use `Tier::Probable`.
#[derive(Debug, Clone)]
pub struct PendingReference {
    pub name: String,
    pub enclosing_symbol_id: Option<i64>,
    pub site_line: u32,
    /// The callee that received this argument, when known.
    pub arg_of: Option<String>,
}

/// All pending references of one file.
#[derive(Debug, Clone)]
pub struct FileReferences {
    pub file_id: i64,
    pub references: Vec<PendingReference>,
}

/// Per-call resolution decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Exact(i64),
    Probable(i64),
    Unresolved,
}

#[derive(Clone, Copy)]
struct Candidate {
    file_id: i64,
    symbol_id: i64,
    kind: SymbolKind,
    start_line: u32,
    /// Method declared in a trait impl, so an implementor (or a std type the
    /// graph never sees) may be the real callee. Excluded from the receiver
    /// relaxation below.
    trait_impl: bool,
}

/// One import binding a name: T1 accepts a definition named `source` in
/// `file_id` for a call on a line of `scope` (empty: anywhere in the file).
struct ImportTarget {
    file_id: i64,
    source: String,
    scope: Vec<(u32, u32)>,
}

/// True iff a call on `site_line` sees names bound for `scope`. A call whose
/// line is unknown sees only the file-wide ones.
fn in_scope(scope: &[(u32, u32)], site_line: Option<u32>) -> bool {
    scope.is_empty()
        || site_line.is_some_and(|line| {
            scope
                .iter()
                .any(|&(start, end)| start <= line && line <= end)
        })
}

/// How many lines the range of `scope` holding `site_line` spans: the
/// innermost scope has the narrowest one, since a nested block or module
/// lies inside every range of the scopes around it. A file-wide scope spans
/// everything.
fn scope_width(scope: &[(u32, u32)], site_line: Option<u32>) -> u32 {
    site_line
        .and_then(|line| {
            scope
                .iter()
                .find(|&&(start, end)| start <= line && line <= end)
        })
        .map_or(u32::MAX, |&(start, end)| end - start)
}

/// Symbol-name index + import graph snapshot used for tier decisions.
pub struct ResolveIndex {
    by_name: HashMap<String, Vec<Candidate>>,
    ruby_files: HashSet<i64>,
    ruby_constants: ruby::Index,
    /// Class/module symbols supply the owner of a Ruby class-body reference.
    containers: HashSet<i64>,
    /// symbol_id → qualified name, for the type-qualified receiver tiebreak
    /// (`pixel_git::GitRunner` + `new` ↔ `GitRunner::new`). Kept beside the
    /// `Copy` candidate rows so the tier code stays copy-based.
    qualified_of: HashMap<i64, String>,
    /// (file_id, local name) → the imports binding that name. T1 requires
    /// the callee to be one of them, and the call site to sit where the
    /// import's names are in scope.
    import_bindings: HashMap<(i64, String), Vec<ImportTarget>>,
    /// file_id → the module path a Rust file defines (`rust_module_path`)
    /// and how many of its leading segments name its Cargo target's crate
    /// root (`rust_crate_depth`, `None` outside `src/`), for the module-path
    /// receiver rule (`module_match`).
    rust_modules: HashMap<i64, (Vec<String>, Option<usize>)>,
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

fn best(cands: &[Candidate]) -> Option<i64> {
    cands
        .iter()
        .min_by_key(|c| (kind_priority(c.kind), c.start_line, c.symbol_id))
        .map(|c| c.symbol_id)
}

impl ResolveIndex {
    pub fn build(store: &GraphStore) -> Result<Self, StoreError> {
        let conn = store.conn();
        let ruby_files = {
            let mut stmt = conn.prepare("SELECT id FROM files WHERE lang = 'ruby'")?;
            let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
            rows.collect::<Result<HashSet<_>, _>>()?
        };
        let rust_modules = {
            let mut stmt = conn.prepare("SELECT id, path FROM files WHERE lang = 'rust'")?;
            let rows = stmt.query_map([], |row| {
                let path = row.get::<_, String>(1)?;
                Ok((
                    row.get::<_, i64>(0)?,
                    (rust_module_path(&path), rust_crate_depth(&path)),
                ))
            })?;
            rows.collect::<Result<HashMap<_, _>, _>>()?
        };
        let mut by_name: HashMap<String, Vec<Candidate>> = HashMap::new();
        let mut qualified_of: HashMap<i64, String> = HashMap::new();
        let mut containers = HashSet::new();
        {
            let mut stmt = conn.prepare(
                "SELECT name, file_id, id, kind, start_line, trait_impl, qualified FROM symbols",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Candidate {
                        file_id: r.get(1)?,
                        symbol_id: r.get(2)?,
                        kind: SymbolKind::parse(&r.get::<_, String>(3)?),
                        start_line: r.get(4)?,
                        trait_impl: r.get(5)?,
                    },
                    r.get::<_, String>(6)?,
                ))
            })?;
            for row in rows {
                let (name, cand, qualified) = row?;
                if matches!(cand.kind, SymbolKind::Class | SymbolKind::Module) {
                    containers.insert(cand.symbol_id);
                }
                qualified_of.insert(cand.symbol_id, qualified);
                if callable(cand.kind) {
                    by_name.entry(name).or_default().push(cand);
                }
            }
        }
        let mut import_bindings: HashMap<(i64, String), Vec<ImportTarget>> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT file_id, resolved_file_id, bindings, scope FROM imports
                  WHERE resolved_file_id IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            for row in rows {
                let (fid, dst_opt, bindings_csv, scope) = row?;
                if let Some(dst) = dst_opt {
                    let scope = decode_scope(&scope);
                    // An empty column means wildcard or unknown and grants
                    // no T1 Exact confidence.
                    for b in decode_bindings(&bindings_csv) {
                        import_bindings
                            .entry((fid, b.local))
                            .or_default()
                            .push(ImportTarget {
                                file_id: dst,
                                source: b.source,
                                scope: scope.clone(),
                            });
                    }
                }
            }
        }
        Ok(Self {
            by_name,
            ruby_files,
            ruby_constants: ruby::Index::build(store)?,
            containers,
            qualified_of,
            import_bindings,
            rust_modules,
        })
    }

    /// True iff some symbol in the graph is named `name`.
    pub fn defines(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// True iff `name` can name a symbol from `file_id`. An alias counts only
    /// when the imported file defines its source: it names no symbol itself,
    /// and a same-name definition elsewhere is not what it means there. Any
    /// other name — imported under its own name or not imported — counts
    /// when some symbol carries it, as before aliases were tracked.
    /// Only the imports that apply on `site_line` count: an alias in another
    /// module does not hide a same-name function of this one.
    fn names_a_symbol(&self, file_id: i64, name: &str, site_line: Option<u32>) -> bool {
        let targets = self.applicable_targets(file_id, name, site_line);
        if targets.is_empty() {
            return self.defines(name);
        }
        targets.iter().any(|t| {
            if t.source == name {
                self.defines(name)
            } else {
                self.defines_in_file(t.file_id, &t.source)
            }
        })
    }

    /// The imports binding `name` in `file_id` that apply on `site_line`:
    /// those in scope there, and of those only the innermost, since a nested
    /// block's or module's `use` shadows the ones around it.
    fn applicable_targets(
        &self,
        file_id: i64,
        name: &str,
        site_line: Option<u32>,
    ) -> Vec<&ImportTarget> {
        let Some(targets) = self.import_bindings.get(&(file_id, name.to_string())) else {
            return Vec::new();
        };
        let visible: Vec<&ImportTarget> = targets
            .iter()
            .filter(|t| in_scope(&t.scope, site_line))
            .collect();
        let Some(narrowest) = visible
            .iter()
            .map(|t| scope_width(&t.scope, site_line))
            .min()
        else {
            return Vec::new();
        };
        visible
            .into_iter()
            .filter(|t| scope_width(&t.scope, site_line) == narrowest)
            .collect()
    }

    /// True iff an import of `file_id` in scope on `site_line` binds `name`
    /// as an alias of another name. The alias is what `name` means there, so
    /// a same-name definition elsewhere can never be its target, even as T2's
    /// sole candidate.
    fn binds_an_alias(&self, file_id: i64, name: &str, site_line: Option<u32>) -> bool {
        self.applicable_targets(file_id, name, site_line)
            .iter()
            .any(|t| t.source != name)
    }

    /// The tier decision for one call from `caller_file_id` to `name`.
    /// `receiver` is the receiver expression text (if any) of the call site;
    /// a real receiver (not `self`/`Self`/`this`) caps the result at
    /// `Probable` because the receiver's type is unknown to the resolver.
    /// A real receiver whose name is also defined in the caller's own file is
    /// `Unresolved`: T0 would otherwise point the call at the caller's
    /// same-name symbol (the shadow) instead of the receiver's own callee.
    ///
    /// Two receiver-shaped exceptions fire before that veto:
    ///
    /// - `receiver-type-match`: the receiver is a value/path whose last
    ///   segment is the type of exactly one same-name candidate
    ///   (`pixel_git::GitRunner::new` → `GitRunner::new`). That candidate is
    ///   returned as `Probable` — the type identity is still a guess, but a
    ///   better-evidenced one than the caller's own same-name symbol. A
    ///   trait-impl candidate counts here: the receiver names the implementing
    ///   type, so `Options::default()` can only call `Options`'s `Default`
    ///   impl. Outside the shadow case it only fires where the name tiers
    ///   refused (`Unresolved`), so an import-resolved `Exact` target is never
    ///   second-guessed.
    /// - `sole-local-method`: when the graph holds exactly one callable
    ///   definition of `name`, it sits in the caller's own file, it is an
    ///   inherent method, and the receiver text is a value/path rather than a
    ///   chained expression, there is no competing definition T0 could shadow
    ///   and no trait implementor the graph cannot see. The call gets that
    ///   sole candidate as `Probable` — never `Exact`.
    ///
    /// A same-file free function (`path.exists()`), a trait-impl method on a
    /// value receiver (`x.clone()` next to `Box::clone`), a chained receiver
    /// (`words.iter().count()`), two same-name methods in one file
    /// (`A::walk` beside `B::walk`), and any name with a definition in
    /// another file keep the shadow veto.
    ///
    /// Without a site line, only the imports in scope in the whole file count
    /// for T1; [`Self::decide_at`] places the call. Without the calling
    /// symbol either, a Ruby call on `self` (bare or written) to a name
    /// defined in its file and elsewhere cannot be matched to the caller's
    /// class and stays `Unresolved`; the resolver paths pass the caller.
    /// Passed method symbols also need their receiving DSL: use
    /// [`Self::decide_reference`] for reference rows, never this call API.
    /// A relative Ruby constant needs a site line to establish lexical scope;
    /// only an absolute (`::Foo`) constant can resolve without that input.
    pub fn decide(&self, caller_file_id: i64, name: &str, receiver: Option<&str>) -> Decision {
        self.decide_from(caller_file_id, None, name, receiver, None)
    }

    /// [`Self::decide`] for a call on `site_line`, locating import bindings
    /// and Ruby lexical class/module scopes.
    pub fn decide_at(
        &self,
        caller_file_id: i64,
        name: &str,
        receiver: Option<&str>,
        site_line: u32,
    ) -> Decision {
        self.decide_from(caller_file_id, None, name, receiver, Some(site_line))
    }

    /// Resolve a passed method using its enclosing owner and receiving DSL.
    ///
    /// Rails symbol callbacks name instance methods of the declaring class/module.
    /// Literal `send` on self uses the enclosing method's kind, or the class method
    /// from a class body. A generated alias or delegator (`alias_method`,
    /// `delegate ... to:`) names a method of its own owner and kind. An unrelated class never supplies a fallback, and reopened
    /// definitions remain unresolved because their load order is unknown.
    /// Both initial references and stored references replay this rule; ordinary
    /// identifier arguments retain the name/import lookup of [`Self::decide_at`].
    pub fn decide_reference(
        &self,
        file_id: i64,
        caller_id: Option<i64>,
        name: &str,
        arg_of: Option<&str>,
        site_line: u32,
    ) -> Decision {
        let Some(kind) = self.ruby_reference_kind(file_id, arg_of) else {
            return self.decide_at(file_id, name, None, site_line);
        };
        let target = caller_id.and_then(|caller| {
            let qualified = self.qualified_of.get(&caller)?;
            let (owner, separator) = if self.containers.contains(&caller) {
                (
                    qualified.as_str(),
                    if kind == ReferenceKind::Send {
                        '.'
                    } else {
                        '#'
                    },
                )
            } else if matches!(kind, ReferenceKind::Send | ReferenceKind::Alias) {
                ruby_owner(qualified)?
            } else {
                return None;
            };
            let mut candidates = self.by_name.get(name)?.iter().filter(|candidate| {
                self.ruby_files.contains(&candidate.file_id)
                    && self
                        .qualified_of
                        .get(&candidate.symbol_id)
                        .and_then(|q| ruby_owner(q))
                        == Some((owner, separator))
            });
            let first = candidates.next()?;
            candidates.next().is_none().then_some(first.symbol_id)
        });
        target.map_or(Decision::Unresolved, Decision::Probable)
    }

    fn ruby_reference_kind(&self, file_id: i64, arg_of: Option<&str>) -> Option<ReferenceKind> {
        if self.ruby_files.contains(&file_id) {
            arg_of.and_then(reference_kind)
        } else {
            None
        }
    }

    fn decide_from(
        &self,
        caller_file_id: i64,
        caller_symbol_id: Option<i64>,
        name: &str,
        receiver: Option<&str>,
        site_line: Option<u32>,
    ) -> Decision {
        let (receiver, method_call) = split_method_receiver(receiver);
        if self.ruby_files.contains(&caller_file_id)
            && let Some(receiver) = receiver
            && let Some(decision) =
                self.ruby_constants
                    .decide(caller_file_id, receiver, name, site_line)
        {
            return decision;
        }
        if self.ruby_files.contains(&caller_file_id)
            && self.ambiguous_local_name(caller_file_id, name)
        {
            match receiver.map(str::trim) {
                // Ruby sends a call without receiver to `self`, so a bare
                // `access_logs(...)` names the caller's own method as surely
                // as `self.access_logs(...)` does.
                None | Some("self") => {
                    return caller_symbol_id
                        .and_then(|id| self.ruby_self_target(caller_file_id, id, name))
                        .map_or(Decision::Unresolved, Decision::Exact);
                }
                Some(_) => {}
            }
        }
        if has_real_receiver(receiver) && self.defines_in_file(caller_file_id, name) {
            if let Some(r) = receiver
                && let Some(id) = self.receiver_match(caller_file_id, r, name, method_call)
            {
                return Decision::Probable(id);
            }
            if let Some(r) = receiver
                && is_value_receiver(r)
                && let Some(id) = self.sole_inherent_method(name)
            {
                return Decision::Probable(id);
            }
            return Decision::Unresolved;
        }
        let raw = self.decide_raw(caller_file_id, name, site_line);
        if has_real_receiver(receiver) {
            if let Decision::Exact(id) = raw {
                // Downgrade: a non-self receiver means we cannot confirm the
                // callee is the same definition the receiver's type resolves
                // to.
                return Decision::Probable(id);
            }
            // The name tiers refused (several files define the name); the
            // receiver still names a type the graph knows, so that candidate
            // is better evidence than nothing.
            if matches!(raw, Decision::Unresolved)
                && let Some(r) = receiver
                && let Some(id) = self.receiver_match(caller_file_id, r, name, method_call)
            {
                return Decision::Probable(id);
            }
        }
        raw
    }

    fn ambiguous_local_name(&self, caller_file_id: i64, name: &str) -> bool {
        let Some(candidates) = self.by_name.get(name) else {
            return false;
        };
        let local_count = candidates
            .iter()
            .filter(|candidate| candidate.file_id == caller_file_id)
            .count();
        local_count > 1
            || (local_count == 1
                && candidates
                    .iter()
                    .any(|candidate| candidate.file_id != caller_file_id))
    }

    /// The caller's own method `name`: a candidate of the caller's class and
    /// kind (`#` instance, `.` class) in the caller's file. `None` when that
    /// file has none, or when another file defines the same owner's method
    /// too: a class reopened elsewhere can redefine it, and the definition
    /// Ruby keeps is whichever loads last, which the graph cannot tell.
    fn ruby_self_target(
        &self,
        caller_file_id: i64,
        caller_symbol_id: i64,
        name: &str,
    ) -> Option<i64> {
        let caller_owner = ruby_owner(self.qualified_of.get(&caller_symbol_id)?)?;
        let same_owner: Vec<Candidate> = self
            .by_name
            .get(name)?
            .iter()
            .copied()
            .filter(|candidate| {
                self.qualified_of
                    .get(&candidate.symbol_id)
                    .and_then(|qualified| ruby_owner(qualified))
                    == Some(caller_owner)
            })
            .collect();
        if same_owner
            .iter()
            .any(|candidate| candidate.file_id != caller_file_id)
        {
            return None;
        }
        best(&same_owner)
    }

    /// The candidate a receiver names: the method of the type it ends with
    /// (`qualified_match`), else, for a path call (`util::run()`, never the
    /// method call `util.run()`, which calls a method of the value `util`),
    /// the free function of the module it spells (`module_match`).
    fn receiver_match(
        &self,
        caller_file_id: i64,
        receiver: &str,
        name: &str,
        method_call: bool,
    ) -> Option<i64> {
        self.qualified_match(receiver, name).or_else(|| {
            (!method_call)
                .then(|| self.module_match(caller_file_id, receiver, name))
                .flatten()
        })
    }

    /// The sole free function `name` defined in the Rust file whose module
    /// path the receiver spells. A path anchored at the caller (`crate::`,
    /// `self::`, `super::`) is resolved against the caller's own module and
    /// must equal the file's path; any other path names a module by its
    /// trailing segments: `pixel_rank::signals` is
    /// `crates/pixel-rank/src/signals.rs`, `gitsync` any `gitsync.rs` or
    /// `gitsync/mod.rs`. Methods never match: a module path calls a free
    /// function, and a type path is `qualified_match`'s. Two files ending the
    /// same way (`a/util.rs` and `b/util.rs` for `util::f`) are ambiguous and
    /// return `None`, as do a receiver that is not a plain path, a caller
    /// that is not a Rust file, and an anchor the caller's module cannot
    /// place (a file outside `src/`, `super` past the crate root). `crate`
    /// is the caller's Cargo target: a binary under `src/bin/` is its own
    /// crate, not the library's. The caller's module is its file's: inside an inline
    /// `mod`, `self`/`super` read one level off, which the `Probable` tier
    /// allows for.
    fn module_match(&self, caller_file_id: i64, receiver: &str, name: &str) -> Option<i64> {
        if !is_value_receiver(receiver) {
            return None;
        }
        let (caller, depth) = self.rust_modules.get(&caller_file_id)?;
        let segments: Vec<&str> = receiver.trim().split("::").collect();
        let target = if matches!(segments[0], "crate" | "self" | "super") {
            Some(anchor_module_path(caller, (*depth)?, &segments)?)
        } else {
            None
        };
        let mut hit: Option<i64> = None;
        for cand in self.by_name.get(name)? {
            let Some((module, _)) = self.rust_modules.get(&cand.file_id) else {
                continue;
            };
            let names_it = target
                .as_ref()
                .map_or_else(|| ends_with_path(module, &segments), |t| module == t);
            if cand.kind != SymbolKind::Function || !names_it {
                continue;
            }
            if hit.is_some() {
                return None;
            }
            hit = Some(cand.symbol_id);
        }
        hit
    }

    /// The sole callable candidate of `name` whose qualified name starts with
    /// the receiver path's last segment (`pixel_git::GitRunner` + `new` →
    /// `GitRunner::new`). The receiver names the implementing type, so a
    /// trait-impl candidate matches too: `Options::default()` can only call
    /// `Options`'s `Default` impl, unlike `opts.default()`, which
    /// `sole_inherent_method_in` refuses. More than one matching candidate —
    /// the same type name in two files, or an inherent method beside a
    /// trait-impl one — is ambiguous and returns `None`, as does a receiver
    /// that is not a plain value/path (`get_store().open()`).
    fn qualified_match(&self, receiver: &str, name: &str) -> Option<i64> {
        if !is_value_receiver(receiver) {
            return None;
        }
        let segment = receiver.trim().rsplit("::").next()?;
        let prefix = format!("{segment}::");
        let mut hit: Option<i64> = None;
        for cand in self.by_name.get(name)? {
            let Some(qualified) = self.qualified_of.get(&cand.symbol_id) else {
                continue;
            };
            if qualified.starts_with(&prefix) {
                if hit.is_some() {
                    return None;
                }
                hit = Some(cand.symbol_id);
            }
        }
        hit
    }

    /// The symbol id of the graph's sole callable definition of `name`, when
    /// it is an inherent method. `None` when the name has no definition, or
    /// when the one definition is a trait-impl method or a free function.
    /// `decide` only calls this under the shadow veto — the name is already
    /// known to be defined in the caller's own file — so the sole definition
    /// is that file's. Two definitions (`A::walk` beside `B::walk`, a
    /// competing file) never reach the single-candidate slice.
    fn sole_inherent_method(&self, name: &str) -> Option<i64> {
        let [candidate] = self.by_name.get(name)?.as_slice() else {
            return None;
        };
        (!candidate.trait_impl && candidate.kind == SymbolKind::Method)
            .then_some(candidate.symbol_id)
    }

    /// True iff `name` has a callable definition in `file_id` — the T0 case
    /// `decide` must not use for a call with a real receiver.
    fn defines_in_file(&self, file_id: i64, name: &str) -> bool {
        self.by_name
            .get(name)
            .is_some_and(|cands| cands.iter().any(|c| c.file_id == file_id))
    }

    /// Tier decision ignoring receiver type (the original name-only logic).
    fn decide_raw(&self, caller_file_id: i64, name: &str, site_line: Option<u32>) -> Decision {
        let cands = self.by_name.get(name).map_or(&[][..], Vec::as_slice);
        // T0: same file.
        let same_file: Vec<Candidate> = cands
            .iter()
            .copied()
            .filter(|c| c.file_id == caller_file_id)
            .collect();
        if let Some(id) = best(&same_file) {
            return Decision::Exact(id);
        }
        // T1: defined in exactly one file that explicitly imported this name.
        // File-level or wildcard imports cannot prove an unqualified binding,
        // so they remain eligible only for repo-wide T2 Probable resolution.
        if let Some(decision) = self.import_tier(caller_file_id, name, site_line) {
            return decision;
        }
        if self.binds_an_alias(caller_file_id, name, site_line) {
            return Decision::Unresolved;
        }
        // T2: unique definition file repo-wide.
        let files: HashSet<i64> = cands.iter().map(|c| c.file_id).collect();
        if files.len() == 1
            && let Some(id) = best(cands)
        {
            return Decision::Probable(id);
        }
        Decision::Unresolved
    }

    /// T1: the definitions the imports binding `name` in `caller_file_id`
    /// point at — a symbol named after the binding's source in the file the
    /// import resolved to — counting only the imports that apply on
    /// `site_line` (`applicable_targets`: in scope, innermost first). `None`
    /// when none binds `name` or none lands on a definition (T2 decides);
    /// `Unresolved` when the applicable imports name several items, even in
    /// one file, since a name never fans out and the resolver will not pick
    /// between them by symbol order.
    fn import_tier(
        &self,
        caller_file_id: i64,
        name: &str,
        site_line: Option<u32>,
    ) -> Option<Decision> {
        let targets = self.applicable_targets(caller_file_id, name, site_line);
        let items: HashSet<(i64, &str)> = targets
            .iter()
            .map(|t| (t.file_id, t.source.as_str()))
            .collect();
        let [(file_id, source)] = items.into_iter().collect::<Vec<_>>()[..] else {
            return (!targets.is_empty()).then_some(Decision::Unresolved);
        };
        let hits: Vec<Candidate> = self
            .by_name
            .get(source)?
            .iter()
            .copied()
            .filter(|c| c.file_id == file_id)
            .collect();
        best(&hits).map(Decision::Exact)
    }
}

/// The module path a Rust source file defines, as a `use` path spells it:
/// the crate (the directory holding `src/`, `-` read as `_`), then one
/// segment per directory and file under `src/`, where `lib.rs`, `main.rs`
/// and `mod.rs` stand for their directory's module
/// (`crates/pixel-rank/src/signals.rs` → `pixel_rank::signals`). A path
/// without `src/` keeps its directories and file stem.
fn rust_module_path(path: &str) -> Vec<String> {
    let parts: Vec<&str> = path.split('/').collect();
    let (krate, rest) = match parts.iter().rposition(|p| *p == "src") {
        Some(src) => (src.checked_sub(1).map(|i| parts[i]), &parts[src + 1..]),
        None => (None, &parts[..]),
    };
    let mut module: Vec<String> = krate.map(|k| k.replace('-', "_")).into_iter().collect();
    for (i, part) in rest.iter().enumerate() {
        let last = i + 1 == rest.len();
        let stem = if last {
            part.strip_suffix(".rs").unwrap_or(part)
        } else {
            part
        };
        if !(last && matches!(stem, "lib" | "main" | "mod")) {
            module.push(stem.to_string());
        }
    }
    module
}

/// How many leading segments of `rust_module_path(path)` name the crate root
/// of the Cargo target the file belongs to: the package directory holding
/// `src/` (when there is one), plus `bin::<name>` for a binary target under
/// `src/bin/` (`src/bin/<name>.rs`, `src/bin/<name>/main.rs` and the modules
/// beside them), which is a crate of its own, not the library's. `None` for a
/// file outside `src/` (`tests/`, `examples/`, a script), whose target root
/// the path does not tell.
/// <https://doc.rust-lang.org/cargo/reference/cargo-targets.html>
fn rust_crate_depth(path: &str) -> Option<usize> {
    let parts: Vec<&str> = path.split('/').collect();
    let src = parts.iter().rposition(|p| *p == "src")?;
    let package = usize::from(src > 0);
    let binary = parts.len() > src + 2 && parts[src + 1] == "bin";
    Some(package + if binary { 2 } else { 0 })
}

/// A Rust method call's receiver, as the extractor records it: the value's
/// text or stated type behind a leading `.` (`.runner`, `.GitRunner`), which
/// no path or expression starts with. Returns the receiver without the mark
/// and whether it was there; a path receiver (`gitsync`, `a::b`) and every
/// other language's receiver come back as they are.
fn split_method_receiver(receiver: Option<&str>) -> (Option<&str>, bool) {
    match receiver.and_then(|r| r.strip_prefix('.')) {
        Some(value) => (Some(value), true),
        None => (receiver, false),
    }
}

/// The module path an anchored `segments` names from a caller in `caller`
/// (whose first `depth` segments are the crate): `crate::a` is the crate's
/// `a`, `self::a` the caller's child `a`, each leading `super` one level up.
/// `None` when a `super` climbs past the crate root.
fn anchor_module_path(caller: &[String], depth: usize, segments: &[&str]) -> Option<Vec<String>> {
    let (base, rest) = match segments.first() {
        Some(&"crate") => (&caller[..depth.min(caller.len())], &segments[1..]),
        Some(&"self") => (caller, &segments[1..]),
        _ => {
            let ups = segments.iter().take_while(|s| **s == "super").count();
            let keep = caller.len().checked_sub(ups).filter(|k| *k >= depth)?;
            (&caller[..keep], &segments[ups..])
        }
    };
    let mut path = base.to_vec();
    path.extend(rest.iter().map(ToString::to_string));
    Some(path)
}

/// True iff `module` ends with `segments` (and `segments` is not empty).
fn ends_with_path(module: &[String], segments: &[&str]) -> bool {
    !segments.is_empty()
        && module.len() >= segments.len()
        && module[module.len() - segments.len()..]
            .iter()
            .zip(segments)
            .all(|(m, s)| m == s)
}

/// True iff `receiver` is a real receiver expression (not absent and not one
/// of the self-pseudo-receivers). `self`/`Self`/`this`/`crate`/`super` resolve
/// against the enclosing type/module, so they keep the normal tier.
fn ruby_owner(qualified: &str) -> Option<(&str, char)> {
    let separator = qualified.rfind(['#', '.'])?;
    Some((
        &qualified[..separator],
        qualified[separator..].chars().next()?,
    ))
}

fn has_real_receiver(receiver: Option<&str>) -> bool {
    match receiver {
        None => false,
        Some(r) => {
            let r = r.trim();
            !r.is_empty()
                && !matches!(
                    r,
                    "self" | "Self" | "this" | "crate" | "super" | "Self::" | "self."
                )
        }
    }
}

/// True iff `receiver` is a plain identifier (`w`, `idx`, `Walker`) or a
/// `::`-separated path (`crate::store`). A chained expression
/// (`words.iter().filter(..)`) names a value produced elsewhere, so the
/// sole-local-method relaxation in `decide` must not treat it as that file's
/// method call.
fn is_value_receiver(receiver: &str) -> bool {
    let r = receiver.trim();
    !r.is_empty() && r.split("::").all(is_plain_ident)
}

fn is_plain_ident(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Resolve the given in-memory pending calls, writing edges / unresolved
/// rows into the store. Used by `build::build_graph` after extraction.
pub fn resolve_calls(
    store: &GraphStore,
    pending: &[FileCalls],
) -> Result<ResolveStats, StoreError> {
    let idx = ResolveIndex::build(store)?;
    let mut stats = ResolveStats::default();
    for fc in pending {
        for call in &fc.calls {
            let Some(src_id) = call.enclosing_symbol_id else {
                // Top-level call site: no source symbol to hang an edge on.
                store.insert_unresolved_call(
                    fc.file_id,
                    &call.callee_name,
                    None,
                    call.site_line,
                    call.receiver.as_deref(),
                    "calls",
                )?;
                stats.unresolved += 1;
                continue;
            };
            match idx.decide_from(
                fc.file_id,
                Some(src_id),
                &call.callee_name,
                call.receiver.as_deref(),
                Some(call.site_line),
            ) {
                Decision::Exact(dst) => {
                    store.insert_edge(&EdgeRow {
                        src_id,
                        dst_id: dst,
                        kind: EdgeKind::Calls,
                        tier: Tier::Exact,
                        site_line: call.site_line,
                        receiver: call.receiver.clone(),
                        callee: Some(call.callee_name.clone()),
                    })?;
                    stats.exact += 1;
                }
                Decision::Probable(dst) => {
                    store.insert_edge(&EdgeRow {
                        src_id,
                        dst_id: dst,
                        kind: EdgeKind::Calls,
                        tier: Tier::Probable,
                        site_line: call.site_line,
                        receiver: call.receiver.clone(),
                        callee: Some(call.callee_name.clone()),
                    })?;
                    stats.probable += 1;
                }
                Decision::Unresolved => {
                    store.insert_unresolved_call(
                        fc.file_id,
                        &call.callee_name,
                        Some(src_id),
                        call.site_line,
                        call.receiver.as_deref(),
                        "calls",
                    )?;
                    stats.unresolved += 1;
                }
            }
        }
    }
    Ok(stats)
}

/// Resolve the given in-memory pending references (symbols passed as
/// arguments to calls), writing `References` edges / unresolved rows into
/// the store. Used by `build::build_graph` after extraction. Mirrors
/// `resolve_calls` but inserts `EdgeKind::References` edges and always
/// uses `Tier::Probable` (we don't know if the callee actually invokes
/// the arg). A reference whose name no symbol carries is a plain value
/// (`g(x)`), not a callback, and is dropped; only a named function the
/// resolver could not pick goes to `unresolved_calls`, where the epistemic
/// envelope counts it.
pub fn resolve_references(
    store: &GraphStore,
    pending: &[FileReferences],
) -> Result<ResolveStats, StoreError> {
    let idx = ResolveIndex::build(store)?;
    let mut stats = ResolveStats::default();
    for fr in pending {
        for r#ref in &fr.references {
            // A known Ruby method-symbol reference must survive even before its
            // definition is indexed; resolve_all can attach a later reopened file.
            if idx
                .ruby_reference_kind(fr.file_id, r#ref.arg_of.as_deref())
                .is_none()
                && !idx.names_a_symbol(fr.file_id, &r#ref.name, Some(r#ref.site_line))
            {
                continue;
            }
            let Some(src_id) = r#ref.enclosing_symbol_id else {
                // Top-level reference site: no source symbol to hang an edge on.
                store.insert_unresolved_call(
                    fr.file_id,
                    &r#ref.name,
                    None,
                    r#ref.site_line,
                    r#ref.arg_of.as_deref(),
                    "references",
                )?;
                stats.unresolved += 1;
                continue;
            };
            match idx.decide_reference(
                fr.file_id,
                Some(src_id),
                &r#ref.name,
                r#ref.arg_of.as_deref(),
                r#ref.site_line,
            ) {
                Decision::Exact(dst) | Decision::Probable(dst) => {
                    store.insert_edge(&EdgeRow {
                        src_id,
                        dst_id: dst,
                        kind: EdgeKind::References,
                        tier: Tier::Probable,
                        site_line: r#ref.site_line,
                        receiver: r#ref.arg_of.clone(),
                        callee: Some(r#ref.name.clone()),
                    })?;
                    stats.probable += 1;
                }
                Decision::Unresolved => {
                    store.insert_unresolved_call(
                        fr.file_id,
                        &r#ref.name,
                        Some(src_id),
                        r#ref.site_line,
                        r#ref.arg_of.as_deref(),
                        "references",
                    )?;
                    stats.unresolved += 1;
                }
            }
        }
    }
    Ok(stats)
}

/// What an incremental batch changed that a stored decision can read, so
/// that [`resolve_affected`] and [`reconsider_resolved_calls`] re-decide only
/// the rows and edges whose answer can differ. A decision on a row (file,
/// name, receiver, caller) reads:
///
/// - the candidates of its name (`by_name`, their qualified names, kinds and
///   files) — `names` holds every symbol name a batch file defined before
///   **or** after the change: a definition that disappears can make an
///   ambiguous name unique, one that appears can make a unique one ambiguous;
/// - its own file's imports, scopes and caller symbols, and an aliased
///   import's source name, which differs from the name the call wrote —
///   `files` holds the batch's files and the files importing one of them;
/// - for a Ruby constant receiver, the class/module constants its segments
///   name, and the owner's methods the Ruby rules read (`Foo.new` reads
///   `Foo.new` and `Foo#initialize`, `perform_later` reads `#perform`) —
///   `constants` holds the last segment of the batch's classes and modules.
///   A method's owner is the class or module whose body declares it in the
///   same file, and the receiver that resolves to that owner ends with that
///   segment, so a changed `#initialize` or `#perform` retries the receiver
///   rows through its class. A `def Foo.bar` outside `Foo`'s body has no
///   owner in its qualified name, so `ruby::Index` never reads it.
///
/// `replayed` holds the names of the rows the update itself moved back to
/// `unresolved_calls` (a demoted incoming edge, a reconsidered one): under
/// an alias the name the call wrote is not the target's.
///
/// Over-inclusion only costs time (an unchanged input gives the same
/// decision); omission leaves a stale row, which
/// `an_incremental_update_should_store_what_a_full_build_stores` rules out.
#[derive(Debug, Default)]
pub struct Affected {
    pub names: HashSet<String>,
    pub files: HashSet<i64>,
    pub constants: HashSet<String>,
    pub replayed: HashSet<String>,
}

impl Affected {
    /// Record the definitions a batch changed. `before` and `after` list the
    /// batch files' definitions as a decision reads them — path, name,
    /// qualified name, kind, trait impl — and only those present a different
    /// number of times on each side count: a file rewritten with the same
    /// definitions changes no candidate set, so no decision outside it. Its
    /// new symbol ids and lines are the demotion's concern (`write_rows`
    /// moves every incoming edge back and records it in `replayed`).
    pub fn record_changed_definitions(&mut self, before: &[Definition], after: &[Definition]) {
        let mut count: HashMap<&Definition, i64> = HashMap::new();
        for definition in before {
            *count.entry(definition).or_default() -= 1;
        }
        for definition in after {
            *count.entry(definition).or_default() += 1;
        }
        for (definition, n) in count {
            if n != 0 {
                self.record_definition(definition);
            }
        }
    }

    /// A changed definition: its name, and the constant a Ruby receiver
    /// reaches it through. A class or module counts by its last segment
    /// (`class A::C::B` is named as written, while the receiver it shadows,
    /// `B.run` inside `A::C`, only spells `B`); a method by its owner's
    /// (`Widget#initialize` is what `Widget.new` reads).
    fn record_definition(&mut self, definition: &Definition) {
        let Definition {
            name,
            qualified,
            kind,
            ..
        } = definition;
        self.names.insert(name.clone());
        let constant = if matches!(kind.as_str(), "class" | "module") {
            Some(name.as_str())
        } else {
            ruby_owner(qualified).map(|(owner, _)| owner)
        };
        if let Some(constant) = constant {
            let segment = constant.rsplit("::").next().unwrap_or(constant);
            self.constants.insert(segment.to_string());
        }
    }

    /// The call names whose decision can read a changed name: the batch's
    /// definitions and the rows the update moved back.
    fn call_names(&self) -> HashSet<&str> {
        self.names
            .iter()
            .chain(&self.replayed)
            .map(String::as_str)
            .collect()
    }

    /// True iff a call on `receiver` can resolve differently: one of its
    /// constant segments is a changed class or module (`Gamma::Delta`,
    /// `Widget.new`), compared whole, never as a prefix.
    fn reads_receiver(&self, receiver: &str) -> bool {
        receiver
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .any(|segment| self.constants.contains(segment))
    }
}

/// One definition as [`Affected::record_changed_definitions`] compares it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Definition {
    pub path: String,
    pub name: String,
    pub qualified: String,
    pub kind: String,
    pub trait_impl: bool,
}

struct RetryRow {
    id: i64,
    file_id: i64,
    name: String,
    enclosing: i64,
    site_line: u32,
    receiver: Option<String>,
    kind: String,
}

const RETRY_COLUMNS: &str =
    "SELECT u.id, u.file_id, u.name, u.enclosing_symbol_id, u.site_line, u.receiver, u.kind
       FROM unresolved_calls u
       JOIN symbols s ON s.id = u.enclosing_symbol_id
      WHERE u.enclosing_symbol_id IS NOT NULL";

fn retry_rows(
    store: &GraphStore,
    filter: &str,
    param: Option<&dyn rusqlite::ToSql>,
) -> Result<Vec<RetryRow>, StoreError> {
    let mut stmt = store
        .conn()
        .prepare_cached(&format!("{RETRY_COLUMNS}{filter}"))?;
    let map = |r: &rusqlite::Row<'_>| {
        Ok(RetryRow {
            id: r.get(0)?,
            file_id: r.get(1)?,
            name: r.get(2)?,
            enclosing: r.get(3)?,
            site_line: r.get(4)?,
            receiver: r.get(5)?,
            kind: r
                .get::<_, Option<String>>(6)?
                .unwrap_or_else(|| "calls".to_string()),
        })
    };
    let rows = match param {
        Some(p) => stmt.query_map([p], map)?.collect::<Result<_, _>>()?,
        None => stmt.query_map([], map)?.collect::<Result<_, _>>()?,
    };
    Ok(rows)
}

/// Re-attempt resolution of every stored `unresolved_calls` row against the
/// current index. Rows that resolve become edges and are deleted; the rest
/// stay (keeping the epistemic envelope honest). The stored `receiver` is
/// replayed so the receiver downgrade stays consistent across
/// re-resolutions. An incremental update calls [`resolve_affected`], which
/// retries only the rows its batch can have changed; this full retry is
/// the reference that one is held equal to.
pub fn resolve_all(store: &mut GraphStore) -> Result<ResolveStats, StoreError> {
    let rows = retry_rows(store, "", None)?;
    retry(store, rows)
}

/// [`resolve_all`] restricted to the rows whose decision reads something
/// `affected` changed (see [`Affected`]): rows named by a changed name, rows
/// of an affected file, and receiver rows naming a changed constant. Each
/// selection goes through an index except the receiver scan, which runs only
/// when the batch changed a class or a module.
pub fn resolve_affected(
    store: &mut GraphStore,
    affected: &Affected,
) -> Result<ResolveStats, StoreError> {
    let mut rows: HashMap<i64, RetryRow> = HashMap::new();
    for name in affected.call_names() {
        for row in retry_rows(store, " AND u.name = ?1", Some(&name))? {
            rows.insert(row.id, row);
        }
    }
    for file in &affected.files {
        for row in retry_rows(store, " AND u.file_id = ?1", Some(file))? {
            rows.insert(row.id, row);
        }
    }
    if !affected.constants.is_empty() {
        for row in retry_rows(store, " AND u.receiver IS NOT NULL", None)? {
            if row
                .receiver
                .as_deref()
                .is_some_and(|r| affected.reads_receiver(r))
            {
                rows.insert(row.id, row);
            }
        }
    }
    let mut rows: Vec<RetryRow> = rows.into_values().collect();
    rows.sort_by_key(|row| row.id);
    retry(store, rows)
}

fn retry(store: &mut GraphStore, rows: Vec<RetryRow>) -> Result<ResolveStats, StoreError> {
    let idx = ResolveIndex::build(store)?;
    let mut stats = ResolveStats::default();
    for row in &rows {
        let decision = if row.kind == "references" {
            idx.decide_reference(
                row.file_id,
                Some(row.enclosing),
                &row.name,
                row.receiver.as_deref(),
                row.site_line,
            )
        } else {
            idx.decide_from(
                row.file_id,
                Some(row.enclosing),
                &row.name,
                row.receiver.as_deref(),
                Some(row.site_line),
            )
        };
        let (dst, tier) = match decision {
            Decision::Exact(d) => (d, Tier::Exact),
            Decision::Probable(d) => (d, Tier::Probable),
            Decision::Unresolved => {
                stats.unresolved += 1;
                continue;
            }
        };
        let edge_kind = if row.kind == "references" {
            EdgeKind::References
        } else {
            EdgeKind::Calls
        };
        // References are always Probable — we don't know if the callee
        // actually invokes the passed arg.
        let tier = if edge_kind == EdgeKind::References {
            Tier::Probable
        } else {
            tier
        };
        store.insert_edge(&EdgeRow {
            src_id: row.enclosing,
            dst_id: dst,
            kind: edge_kind,
            tier,
            site_line: row.site_line,
            receiver: row.receiver.clone(),
            callee: Some(row.name.clone()),
        })?;
        store.conn().exec_cached(
            "DELETE FROM unresolved_calls WHERE id = ?1",
            params![row.id],
        )?;
        match tier {
            Tier::Exact => stats.exact += 1,
            Tier::Probable => stats.probable += 1,
        }
    }
    Ok(stats)
}

/// Reconsider resolved calls whose target names a changed definition
/// carries ([`Affected::record_changed_definitions`]). Adding a same-name definition can make a previously unique target
/// ambiguous. A Ruby receiver call is also replayed when it reads what the
/// batch changed ([`Affected`]): a new constant can shadow its owner, and a
/// new `Foo.new` can take its dispatch, without redefining the callee's
/// name. Both `Calls` and `References` edges are reconsidered — a
/// reference to a previously-unique `handler` is just as stale when a second
/// definition appears. The names of the rows it moves back are recorded in
/// `affected.replayed` for [`resolve_affected`].
pub fn reconsider_resolved_calls(
    store: &mut GraphStore,
    affected: &mut Affected,
) -> Result<(), StoreError> {
    struct ResolvedCall {
        file_id: i64,
        name: String,
        enclosing: i64,
        site_line: u32,
        receiver: Option<String>,
        kind: String,
    }
    let mut calls = Vec::new();
    let mut replay: Vec<i64> = Vec::new();
    // An edge whose target's name changed is the loop below's; a Ruby
    // receiver edge also reads its owner's constant, which no name carries.
    if !affected.constants.is_empty() {
        let mut stmt = store.conn().prepare(
            "SELECT e.id, src.file_id, COALESCE(e.callee, dst.name), e.src_id,
                    e.site_line, e.receiver, e.kind
               FROM edges e JOIN symbols src ON src.id=e.src_id
               JOIN symbols dst ON dst.id=e.dst_id JOIN files f ON f.id=src.file_id
              WHERE f.lang='ruby' AND e.kind='calls' AND e.receiver IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                ResolvedCall {
                    file_id: row.get(1)?,
                    name: row.get(2)?,
                    enclosing: row.get(3)?,
                    site_line: row.get(4)?,
                    receiver: row.get(5)?,
                    kind: row.get(6)?,
                },
            ))
        })?;
        for row in rows {
            let (id, call) = row?;
            let receiver = call.receiver.as_deref().unwrap_or_default();
            if affected.reads_receiver(receiver) {
                replay.push(id);
                calls.push(call);
            }
        }
    }
    for id in replay {
        store
            .conn()
            .exec_cached("DELETE FROM edges WHERE id = ?1", params![id])?;
    }
    for name in &affected.names {
        let found: Vec<ResolvedCall> = {
            let mut stmt = store.conn().prepare(
                "SELECT src.file_id, COALESCE(e.callee, dst.name), e.src_id, e.site_line, e.receiver, e.kind
                   FROM edges e
                   JOIN symbols src ON src.id = e.src_id
                   JOIN symbols dst ON dst.id = e.dst_id
                  WHERE e.kind IN ('calls', 'references') AND dst.name = ?1",
            )?;
            let rows = stmt.query_map(params![name], |row| {
                Ok(ResolvedCall {
                    file_id: row.get(0)?,
                    name: row.get(1)?,
                    enclosing: row.get(2)?,
                    site_line: row.get(3)?,
                    receiver: row.get(4)?,
                    kind: row.get(5)?,
                })
            })?;
            rows.collect::<Result<_, _>>()?
        };
        calls.extend(found);
        store.conn().exec_cached(
            "DELETE FROM edges
              WHERE kind IN ('calls', 'references')
                AND dst_id IN (SELECT id FROM symbols WHERE name = ?1)",
            params![name],
        )?;
    }
    for call in calls {
        affected.replayed.insert(call.name.clone());
        store.insert_unresolved_call(
            call.file_id,
            &call.name,
            Some(call.enclosing),
            call.site_line,
            call.receiver.as_deref(),
            &call.kind,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::ImportBinding;
    use crate::store::GraphStore;

    #[test]
    fn ruby_callback_references_should_resolve_only_the_declaring_owners_instance_method() {
        for (owner, target, rival, expected) in [
            ("Record", "Record#check", "Other#check", true),
            ("Record", "Other#check", "Third#check", false),
            ("Record", "Record.check", "Other#check", false),
            ("Rules", "Rules#check", "Other#check", true),
            ("Record", "Record#check", "Record#check", false),
        ] {
            let mut store = GraphStore::open_in_memory().unwrap();
            let local = store.replace_file("record.rb", "local", "ruby").unwrap();
            let remote = store.replace_file("other.rb", "remote", "ruby").unwrap();
            let caller = store
                .insert_symbol(local, "owner", owner, owner, SymbolKind::Class, 1, 10, "")
                .unwrap();
            let target_id = store
                .insert_symbol(
                    local,
                    "target",
                    "check",
                    target,
                    SymbolKind::Method,
                    5,
                    6,
                    "",
                )
                .unwrap();
            let rival_id = store
                .insert_symbol(
                    remote,
                    "rival",
                    "check",
                    rival,
                    SymbolKind::Method,
                    1,
                    2,
                    "",
                )
                .unwrap();
            let pending = [FileReferences {
                file_id: local,
                references: vec![PendingReference {
                    name: "check".into(),
                    enclosing_symbol_id: Some(caller),
                    site_line: 2,
                    arg_of: Some("validate".into()),
                }],
            }];
            let stats = resolve_references(&store, &pending).unwrap();
            let idx = ResolveIndex::build(&store).unwrap();
            assert_eq!(
                idx.decide_reference(local, None, "check", Some("validate"), 2),
                Decision::Unresolved
            );
            assert_eq!(
                idx.decide_reference(local, Some(target_id), "check", Some("validate"), 2),
                Decision::Unresolved,
                "callbacks require a declaring class/module, not a method caller"
            );
            assert_eq!(
                stats.probable,
                u64::from(expected),
                "{owner} -> {target}, rival {rival}"
            );
            assert_eq!(stats.unresolved, u64::from(!expected));
            assert_eq!(store.edges_to(rival_id, None).unwrap().len(), 0);
            let edges = store.edges_to(target_id, None).unwrap();
            assert_eq!(edges.len(), usize::from(expected));
            if expected {
                assert_eq!(
                    (edges[0].src_id, edges[0].kind, edges[0].tier),
                    (caller, EdgeKind::References, Tier::Probable)
                );
            }
        }
    }

    #[test]
    fn kind_priority_prefers_functions_then_methods_classes_structs() {
        let order = [
            SymbolKind::Function,
            SymbolKind::Method,
            SymbolKind::Class,
            SymbolKind::Struct,
            SymbolKind::Enum,
        ]
        .map(kind_priority);
        assert_eq!(order, [0, 1, 2, 3, 9]);
    }

    #[test]
    fn resolve_calls_counts_every_decision() {
        let (store, local_file, local_f, _, _) = fixture();
        let call = |name: &str, src: Option<i64>, receiver: Option<&str>| PendingCall {
            callee_name: name.into(),
            enclosing_symbol_id: src,
            site_line: 1,
            receiver: receiver.map(str::to_string),
        };
        let pending = [FileCalls {
            file_id: local_file,
            calls: vec![
                call("f", Some(local_f), None),
                call("f", Some(local_f), None),
                call("g", Some(local_f), None),
                call("nosuch", Some(local_f), None),
                call("f", None, None),
            ],
        }];
        let stats = resolve_calls(&store, &pending).unwrap();
        assert_eq!(
            (stats.exact, stats.probable, stats.unresolved),
            (2, 1, 2),
            "{stats:?}"
        );
    }

    #[test]
    fn resolve_references_counts_top_level_sites_as_unresolved() {
        let (store, local_file, _, _, _) = fixture();
        let r = |line| PendingReference {
            name: "g".into(),
            enclosing_symbol_id: None,
            site_line: line,
            arg_of: Some("register".into()),
        };
        let pending = [FileReferences {
            file_id: local_file,
            references: vec![r(1), r(2)],
        }];
        let stats = resolve_references(&store, &pending).unwrap();
        assert_eq!((stats.probable, stats.unresolved), (0, 2), "{stats:?}");
    }

    /// Two Rust files that both define `f`: the caller's own `src/local.rs`
    /// and the `src/remote.rs` a qualified `other_crate::f()` names. `g` is
    /// defined in `src/remote.rs` only.
    fn fixture() -> (GraphStore, i64, i64, i64, i64) {
        let mut store = GraphStore::open_in_memory().unwrap();
        let local_file = store
            .replace_file("src/local.rs", "oid-local", "rust")
            .unwrap();
        let remote_file = store
            .replace_file("src/remote.rs", "oid-remote", "rust")
            .unwrap();
        let local_f = insert(&store, local_file, "src/local.rs", "f");
        let remote_f = insert(&store, remote_file, "src/remote.rs", "f");
        let remote_g = insert(&store, remote_file, "src/remote.rs", "g");
        (store, local_file, local_f, remote_f, remote_g)
    }

    fn insert(store: &GraphStore, file_id: i64, path: &str, name: &str) -> i64 {
        store
            .insert_symbol(
                file_id,
                &format!("{path}#{name}#function"),
                name,
                name,
                SymbolKind::Function,
                1,
                3,
                "",
            )
            .unwrap()
    }

    #[test]
    fn rust_module_path_reads_the_crate_and_the_files_under_src() {
        let cases = [
            ("crates/pixel-rank/src/signals.rs", "pixel_rank::signals"),
            ("crates/pixel-rank/src/lib.rs", "pixel_rank"),
            ("crates/pixel/src/main.rs", "pixel"),
            ("crates/a/src/store/mod.rs", "a::store"),
            ("crates/a/src/store/rows.rs", "a::store::rows"),
            ("src/lib.rs", ""),
            ("src/gitsync.rs", "gitsync"),
            ("tools/gen.rs", "tools::gen"),
            ("crates/a/src/lib/util.rs", "a::lib::util"),
        ];
        for (path, want) in cases {
            assert_eq!(rust_module_path(path).join("::"), want, "{path}");
        }
    }

    #[test]
    fn ends_with_path_compares_the_trailing_segments() {
        let module: Vec<String> = ["pixel_index", "gitsync"].map(String::from).to_vec();
        assert!(ends_with_path(&module, &["gitsync"]));
        assert!(ends_with_path(&module, &["pixel_index", "gitsync"]));
        assert!(!ends_with_path(&module, &["pixel_rank", "gitsync"]));
        assert!(!ends_with_path(&module, &["pixel_index"]));
        assert!(!ends_with_path(&module, &["x", "pixel_index", "gitsync"]));
        assert!(
            !ends_with_path(&module, &[]),
            "an empty path names no module"
        );
    }

    /// `is_test_path` in three files, one of them a method: the module path
    /// picks the free function of the file it spells, an anchored one from
    /// the caller's module; a path two files end with, a method-only match, a
    /// chained receiver, an unknown module and an anchor no caller module
    /// places pick nothing.
    #[test]
    fn module_match_picks_the_free_function_of_the_named_module() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let rank = store
            .replace_file("crates/pixel-rank/src/signals.rs", "o1", "rust")
            .unwrap();
        let graph = store
            .replace_file("crates/pixel-graph/src/signals.rs", "o2", "rust")
            .unwrap();
        let concept = store
            .replace_file("crates/pixel-graph/src/concept.rs", "o3", "rust")
            .unwrap();
        let ts = store
            .replace_file("src/signals.ts", "o4", "typescript")
            .unwrap();
        let rank_fn = insert(
            &store,
            rank,
            "crates/pixel-rank/src/signals.rs",
            "is_test_path",
        );
        let graph_fn = insert(
            &store,
            graph,
            "crates/pixel-graph/src/signals.rs",
            "is_test_path",
        );
        insert(&store, ts, "src/signals.ts", "is_test_path");
        store
            .insert_symbol(
                concept,
                "crates/pixel-graph/src/concept.rs#Concept::is_test_path#method",
                "is_test_path",
                "Concept::is_test_path",
                SymbolKind::Method,
                1,
                3,
                "",
            )
            .unwrap();
        let rank_lib = store
            .replace_file("crates/pixel-rank/src/lib.rs", "o5", "rust")
            .unwrap();
        let rank_sub = store
            .replace_file("crates/pixel-rank/src/sub/deep.rs", "o6", "rust")
            .unwrap();
        let idx = ResolveIndex::build(&store).unwrap();
        let cases = [
            (concept, "pixel_rank::signals", Some(rank_fn)),
            (concept, "pixel_graph::signals", Some(graph_fn)),
            (concept, "signals", None),
            (concept, "concept", None),
            (concept, "pixel_graph::concept", None),
            (concept, "pixel_rank::signals()", None),
            (concept, "unknown", None),
            // Anchored paths resolve against the caller's module, exactly.
            (rank_lib, "crate::signals", Some(rank_fn)),
            (graph, "crate::signals", Some(graph_fn)),
            (rank_lib, "self::signals", Some(rank_fn)),
            (rank_sub, "super::super::signals", Some(rank_fn)),
            (rank_sub, "super::signals", None),
            (rank_lib, "crate::pixel_rank::signals", None),
            (rank_lib, "super::signals", None),
            (ts, "crate::signals", None),
            (ts, "pixel_rank::signals", None),
        ];
        for (caller, receiver, want) in cases {
            assert_eq!(
                idx.module_match(caller, receiver, "is_test_path"),
                want,
                "{receiver} from file {caller}"
            );
        }
        assert_eq!(
            idx.receiver_match(concept, "pixel_rank::signals", "is_test_path", false),
            Some(rank_fn),
            "receiver_match falls back to the module path"
        );
        assert_eq!(
            idx.receiver_match(concept, "pixel_rank::signals", "is_test_path", true),
            None,
            "but not for a method call"
        );
        assert_eq!(
            idx.module_match(concept, "pixel_rank::signals", "absent"),
            None
        );
    }

    #[test]
    fn anchored_module_paths_start_from_the_caller() {
        let caller: Vec<String> = ["a", "store", "rows"].map(String::from).to_vec();
        let cases: [(&[&str], Option<&str>); 5] = [
            (&["crate", "x"], Some("a::x")),
            (&["self", "x"], Some("a::store::rows::x")),
            (&["super", "x"], Some("a::store::x")),
            (&["super", "super", "x"], Some("a::x")),
            (&["super", "super", "super", "x"], None),
        ];
        for (segments, want) in cases {
            assert_eq!(
                anchor_module_path(&caller, 1, segments).map(|p| p.join("::")),
                want.map(String::from),
                "{segments:?}"
            );
        }
        assert_eq!(
            anchor_module_path(&["gitsync".to_string()], 0, &["super", "x"]).map(|p| p.join("::")),
            Some("x".to_string()),
            "a crate without a named directory climbs to its root"
        );
    }

    #[test]
    fn rust_crate_depth_counts_the_target_root() {
        let cases = [
            ("crates/pixel-rank/src/signals.rs", Some(1)),
            ("crates/pixel-rank/src/lib.rs", Some(1)),
            ("src/signals.rs", Some(0)),
            ("crates/a/src/bin/foo.rs", Some(3)),
            ("crates/a/src/bin/foo/main.rs", Some(3)),
            ("crates/a/src/bin/foo/util.rs", Some(3)),
            ("src/bin/foo.rs", Some(2)),
            ("crates/a/src/bin.rs", Some(1)),
            ("crates/a/src/store/rows.rs", Some(1)),
            ("crates/a/src/bin", Some(1)),
            ("src/bin", Some(0)),
            ("tools/gen.rs", None),
            ("crates/a/tests/all/main.rs", None),
        ];
        for (path, want) in cases {
            assert_eq!(rust_crate_depth(path), want, "{path}");
        }
    }

    #[test]
    fn split_method_receiver_reads_the_dot_mark() {
        assert_eq!(
            split_method_receiver(Some(".runner")),
            (Some("runner"), true)
        );
        assert_eq!(
            split_method_receiver(Some("gitsync")),
            (Some("gitsync"), false)
        );
        assert_eq!(split_method_receiver(None), (None, false));
    }

    /// A binary is its own crate: `crate::util` from `src/bin/foo.rs` is
    /// `src/bin/foo/util.rs`, never the library's `src/util.rs`; and a method
    /// call `util.run()` never reaches the free `util::run`, which the path
    /// call `util::run()` does. A caller in another language never uses the
    /// module rule.
    #[test]
    fn module_paths_follow_the_cargo_target_and_the_call_form() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let lib_util = store
            .replace_file("crates/a/src/util.rs", "o1", "rust")
            .unwrap();
        let bin_util = store
            .replace_file("crates/a/src/bin/foo/util.rs", "o2", "rust")
            .unwrap();
        let bin = store
            .replace_file("crates/a/src/bin/foo.rs", "o3", "rust")
            .unwrap();
        let lib = store
            .replace_file("crates/a/src/lib.rs", "o4", "rust")
            .unwrap();
        let script = store
            .replace_file("crates/a/tests/it.rs", "o5", "rust")
            .unwrap();
        let ts = store
            .replace_file("web/app.ts", "o6", "typescript")
            .unwrap();
        let other = store
            .replace_file("crates/b/src/other.rs", "o7", "rust")
            .unwrap();
        let lib_run = insert(&store, lib_util, "crates/a/src/util.rs", "run");
        let bin_run = insert(&store, bin_util, "crates/a/src/bin/foo/util.rs", "run");
        insert(&store, other, "crates/b/src/other.rs", "run");
        let idx = ResolveIndex::build(&store).unwrap();
        let cases = [
            (bin, "crate::util", Some(bin_run)),
            (lib, "crate::util", Some(lib_run)),
            (bin, "self::util", Some(bin_run)),
            (script, "crate::util", None),
            (ts, "a::util", None),
        ];
        for (caller, receiver, want) in cases {
            assert_eq!(
                idx.module_match(caller, receiver, "run"),
                want,
                "{receiver} from {caller}"
            );
        }
        assert_eq!(
            idx.decide(lib, "run", Some("crate::util")),
            Decision::Probable(lib_run)
        );
        assert_eq!(
            idx.decide(lib, "run", Some(".util")),
            Decision::Unresolved,
            "a method call on a value named `util` is not `util::run`"
        );
    }

    #[test]
    fn qualified_call_with_a_locally_defined_name_is_unresolved() {
        let (store, caller_file, _local_f, _remote_f, _remote_g) = fixture();
        let idx = ResolveIndex::build(&store).unwrap();
        // `other_crate::f()` inside `src/local.rs` names another module; T0
        // must not link it to the caller's own `f` (the shadow).
        assert_eq!(
            idx.decide(caller_file, "f", Some("other_crate")),
            Decision::Unresolved
        );
    }

    #[test]
    fn unqualified_and_self_receiver_calls_keep_the_t0_definition() {
        let (store, caller_file, local_f, _remote_f, _remote_g) = fixture();
        let idx = ResolveIndex::build(&store).unwrap();
        // No receiver: the plain `f()` is T0 Exact, unchanged.
        assert_eq!(idx.decide(caller_file, "f", None), Decision::Exact(local_f));
        // `self::f()` / `Self::f()` / `this.f()` keep their exemption.
        for receiver in ["self", "Self", "this"] {
            assert_eq!(
                idx.decide(caller_file, "f", Some(receiver)),
                Decision::Exact(local_f),
                "receiver {receiver:?} keeps the T0 definition"
            );
        }
    }

    #[test]
    fn ruby_unqualified_t0_is_unresolved_when_other_files_define_the_name() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let local = store
            .replace_file("lib/local.rb", "oid-local", "ruby")
            .unwrap();
        let remote = store
            .replace_file("lib/remote.rb", "oid-remote", "ruby")
            .unwrap();
        insert(&store, local, "lib/local.rb", "application");
        insert(&store, remote, "lib/remote.rb", "application");
        let idx = ResolveIndex::build(&store).unwrap();
        assert_eq!(idx.decide(local, "application", None), Decision::Unresolved);
    }

    #[test]
    fn ruby_self_call_resolves_only_with_matching_local_owner() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let local = store
            .replace_file("lib/local.rb", "oid-local", "ruby")
            .unwrap();
        let remote = store
            .replace_file("lib/remote.rb", "oid-remote", "ruby")
            .unwrap();
        let caller = store
            .insert_symbol(
                local,
                "local#App#run#method",
                "run",
                "App#run",
                SymbolKind::Method,
                1,
                3,
                "run",
            )
            .unwrap();
        let unrelated_caller = store
            .insert_symbol(
                local,
                "local#Admin#run#method",
                "run",
                "Admin#run",
                SymbolKind::Method,
                5,
                7,
                "run",
            )
            .unwrap();
        let local_target = store
            .insert_symbol(
                local,
                "local#App#application#method",
                "application",
                "App#application",
                SymbolKind::Method,
                9,
                11,
                "application",
            )
            .unwrap();
        store
            .insert_symbol(
                remote,
                "remote#Other#application#method",
                "application",
                "Other#application",
                SymbolKind::Method,
                1,
                3,
                "application",
            )
            .unwrap();
        let idx = ResolveIndex::build(&store).unwrap();
        assert_eq!(
            idx.decide_from(local, Some(caller), "application", Some("self"), None),
            Decision::Exact(local_target)
        );
        assert_eq!(
            idx.decide_from(
                local,
                Some(unrelated_caller),
                "application",
                Some("self"),
                None
            ),
            Decision::Unresolved
        );
    }

    /// Ruby sends a call without receiver to `self`: `access_logs(...)` in
    /// `App#run` reaches `App#access_logs` even when another class elsewhere
    /// defines the name, and only for a caller of that class and kind.
    #[test]
    fn ruby_bare_call_should_resolve_to_the_callers_own_method_like_a_self_call() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let local = store
            .replace_file("app/services/app.rb", "oid-local", "ruby")
            .unwrap();
        let remote = store
            .replace_file("app/controllers/other.rb", "oid-remote", "ruby")
            .unwrap();
        let caller = store
            .insert_symbol(
                local,
                "app#App#run#method",
                "run",
                "App#run",
                SymbolKind::Method,
                1,
                3,
                "run",
            )
            .unwrap();
        let class_method_caller = store
            .insert_symbol(
                local,
                "app#App.build#method",
                "build",
                "App.build",
                SymbolKind::Method,
                5,
                7,
                "build",
            )
            .unwrap();
        let unrelated_caller = store
            .insert_symbol(
                local,
                "app#Admin#run#method",
                "run",
                "Admin#run",
                SymbolKind::Method,
                9,
                11,
                "run",
            )
            .unwrap();
        let local_target = store
            .insert_symbol(
                local,
                "app#App#access_logs#method",
                "access_logs",
                "App#access_logs",
                SymbolKind::Method,
                13,
                15,
                "access_logs",
            )
            .unwrap();
        store
            .insert_symbol(
                remote,
                "other#Other#access_logs#method",
                "access_logs",
                "Other#access_logs",
                SymbolKind::Method,
                1,
                3,
                "access_logs",
            )
            .unwrap();
        let idx = ResolveIndex::build(&store).unwrap();

        assert_eq!(
            idx.decide_from(local, Some(caller), "access_logs", None, Some(2)),
            Decision::Exact(local_target),
            "a bare call names the caller's own instance method"
        );
        assert_eq!(
            idx.decide_from(
                local,
                Some(class_method_caller),
                "access_logs",
                None,
                Some(6)
            ),
            Decision::Unresolved,
            "self in a class method is the class, which has no `access_logs`"
        );
        assert_eq!(
            idx.decide_from(local, Some(unrelated_caller), "access_logs", None, Some(10)),
            Decision::Unresolved,
            "another class of the same file does not define it"
        );
        assert_eq!(
            idx.decide(local, "access_logs", None),
            Decision::Unresolved,
            "without the calling symbol the owner cannot be checked"
        );
    }

    /// A class reopened in another file that defines the same method again
    /// leaves the override to load order: neither a bare call nor
    /// `self.name` gets an Exact edge to the caller's file's definition.
    #[test]
    fn ruby_self_call_should_stay_unresolved_when_a_reopened_class_redefines_it() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let local = store
            .replace_file("app/models/app.rb", "oid-local", "ruby")
            .unwrap();
        let reopened = store
            .replace_file("config/initializers/app_patch.rb", "oid-patch", "ruby")
            .unwrap();
        let caller = store
            .insert_symbol(
                local,
                "app#App#run#method",
                "run",
                "App#run",
                SymbolKind::Method,
                1,
                3,
                "run",
            )
            .unwrap();
        store
            .insert_symbol(
                local,
                "app#App#access_logs#method",
                "access_logs",
                "App#access_logs",
                SymbolKind::Method,
                5,
                7,
                "access_logs",
            )
            .unwrap();
        store
            .insert_symbol(
                reopened,
                "patch#App#access_logs#method",
                "access_logs",
                "App#access_logs",
                SymbolKind::Method,
                1,
                3,
                "access_logs",
            )
            .unwrap();
        let idx = ResolveIndex::build(&store).unwrap();

        for receiver in [None, Some("self")] {
            assert_eq!(
                idx.decide_from(local, Some(caller), "access_logs", receiver, Some(2)),
                Decision::Unresolved,
                "receiver {receiver:?}: the reopened class may override the local definition"
            );
        }
    }

    #[test]
    fn ruby_same_file_duplicate_owners_are_ambiguous_without_remote_candidates() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let file = store.replace_file("lib/local.rb", "oid", "ruby").unwrap();
        let caller = store
            .insert_symbol(
                file,
                "local#App#run#method",
                "run",
                "App#run",
                SymbolKind::Method,
                1,
                3,
                "run",
            )
            .unwrap();
        let app_target = store
            .insert_symbol(
                file,
                "local#App#application#method",
                "application",
                "App#application",
                SymbolKind::Method,
                5,
                7,
                "application",
            )
            .unwrap();
        store
            .insert_symbol(
                file,
                "local#Admin#application#method",
                "application",
                "Admin#application",
                SymbolKind::Method,
                9,
                11,
                "application",
            )
            .unwrap();

        let idx = ResolveIndex::build(&store).unwrap();
        assert_eq!(
            idx.decide_from(file, Some(caller), "application", Some("self"), None),
            Decision::Exact(app_target)
        );
        assert_eq!(idx.decide(file, "application", None), Decision::Unresolved);
    }

    #[test]
    fn ruby_unique_unqualified_t0_stays_exact() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let file = store.replace_file("lib/local.rb", "oid", "ruby").unwrap();
        let local = insert(&store, file, "lib/local.rb", "helper");
        let idx = ResolveIndex::build(&store).unwrap();
        assert_eq!(idx.decide(file, "helper", None), Decision::Exact(local));
    }

    #[test]
    fn real_receiver_with_a_name_defined_elsewhere_stays_probable() {
        let (store, caller_file, _local_f, _remote_f, remote_g) = fixture();
        let idx = ResolveIndex::build(&store).unwrap();
        // `x.g()`: `g` has no local definition to shadow, so the unique
        // repo-wide definition stays the (receiver-capped) Probable answer.
        assert_eq!(
            idx.decide(caller_file, "g", Some("x")),
            Decision::Probable(remote_g)
        );
    }

    /// Single-file store for the sole-local-method relaxation: `src/local.rs`
    /// is the only file, so every name defined there has exactly one file.
    fn local_only() -> (GraphStore, i64) {
        let mut store = GraphStore::open_in_memory().unwrap();
        let file = store.replace_file("src/local.rs", "oid", "rust").unwrap();
        (store, file)
    }

    /// Insert a symbol with an explicit kind and trait-impl flag; `line`
    /// disambiguates duplicate names in one file (the uid is per line).
    fn insert_at(
        store: &GraphStore,
        file_id: i64,
        name: &str,
        kind: SymbolKind,
        line: u32,
        trait_impl: bool,
    ) -> i64 {
        let id = store
            .insert_symbol(
                file_id,
                &format!("f{file_id}#{name}#{line}"),
                name,
                name,
                kind,
                line,
                line + 1,
                "",
            )
            .unwrap();
        if trait_impl {
            store.mark_trait_impl(id).unwrap();
        }
        id
    }

    #[test]
    fn sole_inherent_method_resolves_a_value_receiver_at_probable_never_exact() {
        let (store, file) = local_only();
        let method = insert_at(&store, file, "push_call", SymbolKind::Method, 2, false);
        let idx = ResolveIndex::build(&store).unwrap();
        // `w.push_call()`: the only definition of the name is this file's
        // method, so the shadow veto relaxes to the sole candidate — capped
        // at Probable, since the receiver's type is still unknown.
        assert_eq!(
            idx.decide(file, "push_call", Some("w")),
            Decision::Probable(method)
        );
        assert_eq!(
            idx.decide(file, "push_call", Some("crate::extract")),
            Decision::Probable(method)
        );
    }

    #[test]
    fn two_same_named_methods_in_one_file_keep_the_shadow_veto() {
        let (store, file) = local_only();
        // `A::walk` and `B::walk` are two candidates for an unknown receiver:
        // either could be the callee, so neither is picked.
        let late = insert_at(&store, file, "walk", SymbolKind::Method, 9, false);
        let early = insert_at(&store, file, "walk", SymbolKind::Method, 4, false);
        assert_ne!(early, late);
        let idx = ResolveIndex::build(&store).unwrap();
        assert_eq!(idx.decide(file, "walk", Some("w")), Decision::Unresolved);
    }

    #[test]
    fn a_free_function_with_a_receiver_keeps_the_shadow_veto() {
        let (store, file) = local_only();
        insert_at(&store, file, "exists", SymbolKind::Function, 2, false);
        let idx = ResolveIndex::build(&store).unwrap();
        // `path.exists()` is `Path::exists`, not this file's free `fn
        // exists`: a function has no receiver, so the call cannot target it.
        assert_eq!(
            idx.decide(file, "exists", Some("path")),
            Decision::Unresolved
        );
    }

    #[test]
    fn a_trait_impl_method_keeps_the_shadow_veto() {
        let (store, file) = local_only();
        insert_at(&store, file, "clone", SymbolKind::Method, 2, true);
        let idx = ResolveIndex::build(&store).unwrap();
        // `path.clone()` is `Clone::clone` for a std type the graph never
        // sees; the file's `Box::clone` is not evidence the call targets it.
        assert_eq!(
            idx.decide(file, "clone", Some("path")),
            Decision::Unresolved
        );
    }

    #[test]
    fn a_trait_impl_among_several_local_candidates_keeps_the_shadow_veto() {
        let (store, file) = local_only();
        insert_at(&store, file, "dims", SymbolKind::Method, 2, false);
        insert_at(&store, file, "dims", SymbolKind::Method, 8, true);
        let idx = ResolveIndex::build(&store).unwrap();
        // The inherent `dims` cannot be told apart from the trait impl's
        // `dims` on an unknown receiver, so the call stays unresolved.
        assert_eq!(
            idx.decide(file, "dims", Some("embedder")),
            Decision::Unresolved
        );
    }

    #[test]
    fn a_chained_receiver_keeps_the_shadow_veto() {
        let (store, file) = local_only();
        insert_at(&store, file, "count", SymbolKind::Method, 2, false);
        let idx = ResolveIndex::build(&store).unwrap();
        // `words.iter().filter(..).count()` is `Iterator::count`, not the
        // file's own method: the receiver is an expression, not a value.
        assert_eq!(
            idx.decide(file, "count", Some("words.iter().filter(..)")),
            Decision::Unresolved
        );
    }

    #[test]
    fn a_second_file_definition_keeps_the_shadow_veto() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let local = store.replace_file("src/local.rs", "oid", "rust").unwrap();
        let remote = store.replace_file("src/remote.rs", "oid", "rust").unwrap();
        insert_at(&store, local, "run", SymbolKind::Method, 2, false);
        insert_at(&store, remote, "run", SymbolKind::Method, 2, false);
        let idx = ResolveIndex::build(&store).unwrap();
        // `x.run()` with a competing definition elsewhere is ambiguous, so
        // the shadow veto stays (the cited `graph::build::f()` regression).
        assert_eq!(idx.decide(local, "run", Some("x")), Decision::Unresolved);
    }

    #[test]
    fn value_receiver_shapes() {
        assert!(is_value_receiver("w"));
        assert!(is_value_receiver("a1"));
        assert!(is_value_receiver("_private"));
        assert!(is_value_receiver("Store"));
        assert!(is_value_receiver("crate::store"));
        assert!(is_value_receiver("  idx  "));
        assert!(!is_value_receiver("m.path"));
        assert!(!is_value_receiver("words.iter().filter(..)"));
        assert!(!is_value_receiver("response[\"text\"]"));
        assert!(!is_value_receiver(""));
        assert!(!is_value_receiver("1bad"));
    }

    /// Two files, each with its own `open`: `src/local.rs` has `Other::open`
    /// (the caller's file) and `src/remote.rs` has `Store::open`.
    fn two_opens() -> (GraphStore, i64, i64, i64, i64) {
        let mut store = GraphStore::open_in_memory().unwrap();
        let local = store.replace_file("src/local.rs", "oid", "rust").unwrap();
        let remote = store.replace_file("src/remote.rs", "oid", "rust").unwrap();
        let other_open = insert_named(&store, local, "Other", "open");
        let store_open = insert_named(&store, remote, "Store", "open");
        (store, local, remote, other_open, store_open)
    }

    /// Insert an inherent method `Type::name` whose qualified name carries the
    /// type, mirroring what the extractor writes for an `impl` block.
    fn insert_named(store: &GraphStore, file_id: i64, ty: &str, name: &str) -> i64 {
        store
            .insert_symbol(
                file_id,
                &format!("f{file_id}#{ty}::{name}#method"),
                name,
                &format!("{ty}::{name}"),
                SymbolKind::Method,
                1,
                3,
                "",
            )
            .unwrap()
    }

    /// `insert_named` for a method declared in a trait impl.
    fn insert_trait_named(store: &GraphStore, file_id: i64, ty: &str, name: &str) -> i64 {
        let id = insert_named(store, file_id, ty, name);
        store.mark_trait_impl(id).unwrap();
        id
    }

    #[test]
    fn receiver_path_type_matches_the_unique_qualified_candidate() {
        let (store, local, _remote, other_open, store_open) = two_opens();
        let idx = ResolveIndex::build(&store).unwrap();
        // `pixel_git::GitRunner::new`-shaped: the receiver names `Store`, so
        // the call links to `Store::open` even though the caller's own file
        // defines `Other::open` (which the shadow veto would otherwise
        // refuse to resolve at all).
        assert_eq!(
            idx.decide(local, "open", Some("pixel_remote::Store")),
            Decision::Probable(store_open)
        );
        // A single-segment receiver does the same.
        assert_eq!(
            idx.decide(local, "open", Some("Store")),
            Decision::Probable(store_open)
        );
        // Without the type path the call stays shadowed to the local
        // `Other::open`, never guessed at the other one.
        assert_eq!(idx.decide(local, "open", Some("o")), Decision::Unresolved);
        assert_ne!(other_open, store_open);
    }

    #[test]
    fn a_type_qualified_trait_method_links_to_the_implementor() {
        let (store, file) = local_only();
        let default = insert_trait_named(&store, file, "Options", "default");
        let idx = ResolveIndex::build(&store).unwrap();
        // `Options::default()` names the implementing type, so the trait impl
        // is the only possible callee — unlike `opts.default()`, where the
        // receiver's type (and therefore the implementor) is unknown.
        assert_eq!(
            idx.decide(file, "default", Some("Options")),
            Decision::Probable(default)
        );
        assert_eq!(
            idx.decide(file, "default", Some("opts")),
            Decision::Unresolved
        );
    }

    #[test]
    fn two_same_named_types_make_the_path_match_ambiguous() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let a = store.replace_file("src/a.rs", "oid", "rust").unwrap();
        let b = store.replace_file("src/b.rs", "oid", "rust").unwrap();
        insert_named(&store, a, "Store", "open");
        insert_named(&store, b, "Store", "open");
        let idx = ResolveIndex::build(&store).unwrap();
        // `x::Store::open` with two `Store::open` candidates is ambiguous:
        // no edge, no fan-out.
        assert_eq!(
            idx.decide(a, "open", Some("x::Store")),
            Decision::Unresolved
        );
    }

    #[test]
    fn path_prefix_must_be_the_whole_type_segment() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let a = store.replace_file("src/a.rs", "oid", "rust").unwrap();
        let b = store.replace_file("src/b.rs", "oid", "rust").unwrap();
        insert_named(&store, a, "WalkerHelper", "walk");
        insert_named(&store, b, "Other", "walk");
        let idx = ResolveIndex::build(&store).unwrap();
        // `Walker` is a prefix of `WalkerHelper` but not the type: no match,
        // and the two-file ambiguity keeps the call unresolved.
        assert_eq!(idx.decide(a, "walk", Some("Walker")), Decision::Unresolved);
    }

    #[test]
    fn a_chained_receiver_never_path_matches() {
        let (store, local, _remote, _other_open, _store_open) = two_opens();
        let idx = ResolveIndex::build(&store).unwrap();
        // `get_store().open()` is not a receiver path even though the text
        // ends in `::Store`: the expression is a call, not a name.
        assert_eq!(idx.qualified_match("get_store()::Store", "open"), None);
        // The same call therefore falls through to the shadow veto (the
        // caller's file defines `Other::open`).
        assert_eq!(
            idx.decide(local, "open", Some("get_store()::Store")),
            Decision::Unresolved
        );
    }

    #[test]
    fn an_import_resolved_exact_target_is_not_second_guessed_by_a_path() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let local = store.replace_file("src/local.rs", "oid", "rust").unwrap();
        let a = store.replace_file("src/a.rs", "oid", "rust").unwrap();
        let b = store.replace_file("src/b.rs", "oid", "rust").unwrap();
        let a_open = insert_named(&store, a, "A", "open");
        insert_named(&store, b, "B", "open");
        // `local.rs` imports the binding `open` from `a.rs`; T1 resolves
        // `open()` to `A::open` as Exact. A receiver naming `B` must not
        // swap that for a Probable guess at `B::open`.
        store
            .insert_import(
                local,
                "crate::a::open",
                Some(a),
                &[crate::extract::ImportBinding::named("open")],
            )
            .unwrap();
        let idx = ResolveIndex::build(&store).unwrap();
        assert_eq!(
            idx.decide(local, "open", Some("B")),
            Decision::Probable(a_open),
            "Exact(A::open) downgraded to Probable, never swapped for B::open"
        );
    }

    /// T1 under an alias: the call writes the local name, the candidate
    /// carries the source name. Two imports binding one name to definitions
    /// in two files never fan out; an import whose file holds no definition
    /// of the source leaves the decision to T2.
    #[test]
    fn import_tier_matches_the_local_name_against_the_source_definition() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let local = store.replace_file("src/local.rs", "oid", "rust").unwrap();
        let a = store.replace_file("src/a.rs", "oid", "rust").unwrap();
        let b = store.replace_file("src/b.rs", "oid", "rust").unwrap();
        let c = store.replace_file("src/c.rs", "oid", "rust").unwrap();
        let a_push = insert(&store, a, "src/a.rs", "push");
        insert(&store, b, "src/b.rs", "push");
        let c_open = insert(&store, c, "src/c.rs", "open");
        store
            .insert_import(
                local,
                "crate::a::push as leased",
                Some(a),
                &[ImportBinding::aliased("push", "leased")],
            )
            .unwrap();
        store
            .insert_import(
                local,
                "crate::a::push as both",
                Some(a),
                &[ImportBinding::aliased("push", "both")],
            )
            .unwrap();
        store
            .insert_import(
                local,
                "crate::b::push as both",
                Some(b),
                &[ImportBinding::aliased("push", "both")],
            )
            .unwrap();
        // `open` is bound to a.rs, which does not define it: the import
        // proves nothing, and c.rs's sole `open` is T2's Probable.
        store
            .insert_import(
                local,
                "crate::a::open",
                Some(a),
                &[ImportBinding::named("open")],
            )
            .unwrap();
        let idx = ResolveIndex::build(&store).unwrap();
        assert_eq!(idx.decide(local, "leased", None), Decision::Exact(a_push));
        assert_eq!(
            idx.decide(local, "push", None),
            Decision::Unresolved,
            "only aliases are bound; two files define push"
        );
        assert_eq!(idx.decide(local, "both", None), Decision::Unresolved);
        assert_eq!(idx.decide(local, "open", None), Decision::Probable(c_open));
    }

    /// A scoped import counts for a call on one of its lines, both ends
    /// included; a file-wide one counts everywhere, even where the call's
    /// line is unknown, and a scoped one never does then.
    #[test]
    fn in_scope_accepts_the_lines_of_the_scope_only() {
        let scope = [(3, 5), (9, 9)];
        let cases: Vec<(Option<u32>, bool)> = [2, 3, 4, 5, 6, 9, 10]
            .iter()
            .map(|&line| (Some(line), in_scope(&scope, Some(line))))
            .collect();
        assert_eq!(
            cases,
            [
                (Some(2), false),
                (Some(3), true),
                (Some(4), true),
                (Some(5), true),
                (Some(6), false),
                (Some(9), true),
                (Some(10), false),
            ]
        );
        assert!(!in_scope(&scope, None));
        assert!(in_scope(&[], None));
        assert!(in_scope(&[], Some(7)));
    }

    /// Two imports bind `run` for the same lines to two items of one file:
    /// nothing says which one the call means, so T1 must not pick one by
    /// symbol order.
    #[test]
    fn import_tier_leaves_two_items_bound_for_the_same_lines_unresolved() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let local = store.replace_file("src/local.rs", "oid", "rust").unwrap();
        let utils = store.replace_file("src/utils.rs", "oid", "rust").unwrap();
        insert(&store, utils, "src/utils.rs", "early");
        insert(&store, utils, "src/utils.rs", "later");
        for source in ["early", "later"] {
            store
                .insert_import(
                    local,
                    &format!("crate::utils::{source} as run"),
                    Some(utils),
                    &[ImportBinding::aliased(source, "run")],
                )
                .unwrap();
        }
        let idx = ResolveIndex::build(&store).unwrap();
        assert_eq!(idx.decide_at(local, "run", None, 3), Decision::Unresolved);
    }

    #[test]
    fn scope_width_is_the_span_of_the_range_holding_the_line() {
        let scope = [(1, 1), (4, 9)];
        assert_eq!(scope_width(&scope, Some(5)), 5);
        assert_eq!(scope_width(&scope, Some(1)), 0);
        assert_eq!(scope_width(&[], Some(5)), u32::MAX);
        assert_eq!(scope_width(&scope, None), u32::MAX);
    }

    /// `resolve_affected` retries the rows whose decision reads what the
    /// batch changed, and only those: that is what keeps an incremental
    /// update's cost with its batch instead of the whole table (Task 833).
    /// No row here can resolve, so `unresolved` counts the rows retried.
    #[test]
    fn resolve_affected_should_retry_only_the_rows_reading_a_changed_input() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let file = store.replace_file("app/caller.rb", "oid", "ruby").unwrap();
        let other = store.replace_file("app/other.rb", "oid2", "ruby").unwrap();
        let caller = store
            .insert_symbol(file, "go", "go", "Caller#go", SymbolKind::Method, 1, 9, "")
            .unwrap();
        let elsewhere = store
            .insert_symbol(
                other,
                "run",
                "run",
                "Other#run",
                SymbolKind::Method,
                1,
                3,
                "",
            )
            .unwrap();
        for (name, receiver) in [
            ("alpha", None),
            ("beta", Some("Gamma::Delta")),
            ("new", Some("Widget")),
            ("step", Some("Widget.new")),
            ("perform_later", Some("Job")),
            ("perform_now", Some("Job.set")),
        ] {
            store
                .insert_unresolved_call(file, name, Some(caller), 2, receiver, "calls")
                .unwrap();
        }
        store
            .insert_unresolved_call(other, "zeta", Some(elsewhere), 2, None, "calls")
            .unwrap();
        let retried = |store: &mut GraphStore, affected: Affected| {
            resolve_affected(store, &affected).unwrap().unresolved
        };
        let names = |names: &[&str]| Affected {
            names: names.iter().map(|n| (*n).to_string()).collect(),
            ..Affected::default()
        };
        assert_eq!(retried(&mut store, Affected::default()), 0);
        assert_eq!(
            retried(&mut store, names(&["alpha"])),
            1,
            "the row's own name"
        );
        assert_eq!(
            retried(&mut store, names(&["perform"])),
            0,
            "`Job.perform_later` reads `Job#perform` through its class, not the name"
        );
        let def = |path: &str, name: &str, qualified: &str, kind: &str| Definition {
            path: path.into(),
            name: name.into(),
            qualified: qualified.into(),
            kind: kind.into(),
            trait_impl: false,
        };
        let mut job = Affected::default();
        job.record_changed_definitions(
            &[],
            &[def("app/job.rb", "perform", "Job#perform", "method")],
        );
        assert_eq!(
            retried(&mut store, job),
            2,
            "a new `Job#perform` retries both job dispatches through `Job`"
        );
        let mut nested = Affected::default();
        nested
            .record_changed_definitions(&[def("app/w.rb", "A::Widget", "A::Widget", "class")], &[]);
        assert_eq!(
            retried(&mut store, nested),
            2,
            "`class A::Widget` is reached through `Widget`: `Widget.new`, `Widget.new.step`"
        );
        for (constant, expected) in [("Delta", 1), ("Gamma", 1), ("Gam", 0), ("Widget", 2)] {
            let affected = Affected {
                constants: HashSet::from([constant.to_string()]),
                ..Affected::default()
            };
            assert_eq!(
                retried(&mut store, affected),
                expected,
                "constant {constant}"
            );
        }
        let replayed = Affected {
            replayed: HashSet::from(["zeta".to_string()]),
            ..Affected::default()
        };
        assert_eq!(
            retried(&mut store, replayed),
            1,
            "a row the update moved back"
        );
        let files = Affected {
            files: HashSet::from([file]),
            ..Affected::default()
        };
        assert_eq!(
            retried(&mut store, files),
            6,
            "every row of an affected file"
        );
        assert_eq!(resolve_all(&mut store).unwrap().unresolved, 7);
    }

    /// Only a definition that changed can change a decision elsewhere: a
    /// file rewritten with the same definitions must leave every other row
    /// and edge alone, or each edit of a file defining `initialize` or
    /// `call` re-decides every call to those names in the repository.
    #[test]
    fn affected_should_record_only_the_definitions_a_batch_changed() {
        let def = |path: &str, name: &str, qualified: &str, kind: &str, trait_impl| Definition {
            path: path.into(),
            name: name.into(),
            qualified: qualified.into(),
            kind: kind.into(),
            trait_impl,
        };
        let same = [
            def("a.rb", "W", "W", "class", false),
            def("a.rb", "initialize", "W#initialize", "method", false),
        ];
        let recorded = |before: &[Definition], after: &[Definition]| {
            let mut affected = Affected::default();
            affected.record_changed_definitions(before, after);
            let mut names: Vec<String> = affected.names.into_iter().collect();
            let mut constants: Vec<String> = affected.constants.into_iter().collect();
            names.sort();
            constants.sort();
            (names, constants)
        };
        let none = (Vec::<String>::new(), Vec::<String>::new());
        assert_eq!(recorded(&same, &same), none, "same definitions, new body");
        let moved = [def("b.rb", "W", "W", "class", false), same[1].clone()];
        assert_eq!(
            recorded(&same, &moved),
            (vec!["W".into()], vec!["W".into()]),
            "a definition in another file is another candidate"
        );
        let twice = [same[0].clone(), same[1].clone(), same[1].clone()];
        assert_eq!(
            recorded(&same, &twice),
            (vec!["initialize".into()], vec!["W".into()]),
            "a second same-owner definition makes the name ambiguous; the method counts through its owner"
        );
        let rust = [def("x.rs", "fmt", "W::fmt", "method", false)];
        let as_trait = [def("x.rs", "fmt", "W::fmt", "method", true)];
        assert_eq!(
            recorded(&rust, &as_trait),
            (vec!["fmt".into()], Vec::new()),
            "a trait impl is no inherent method; a Rust path has no Ruby owner"
        );
        assert_eq!(
            recorded(&[def("m.rb", "B", "A::C::B", "class", false)], &[]),
            (vec!["B".into()], vec!["B".into()]),
        );
        assert_eq!(
            recorded(&[], &[def("m.rb", "A::C::B", "A::C::B", "module", false)]),
            (vec!["A::C::B".into()], vec!["B".into()]),
            "a constant counts by the segment a receiver spells"
        );
    }
}
