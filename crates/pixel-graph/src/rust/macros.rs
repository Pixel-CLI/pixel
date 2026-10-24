// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Macros invoked where items go (D23): among a module's items, an impl's
//! or a trait's. Such a macro may expand to items no row holds: any item
//! of its scope (the mark extraction leaves there stands for them), and
//! impls, inherent ones of the types of its crate and trait impls of any
//! type its crate can name. Which impls a macro writes cannot be read
//! without expanding it: its rules may invoke other macros, paste names,
//! or pass items through from their input, and a procedural macro runs
//! code. So the rule is blunt: a method probe
//! (`x.m()`, `T::m`, `<T>::m`, `<T as Tr>::m` on a type of the
//! workspace) is unresolved in a crate that compiles a macro invoked where
//! items go, its own or one of a workspace library it depends on,
//! transitively, as an impl reaches a caller only from a crate compiled
//! into it. So is it where such a crate applies a derive or attribute
//! macro of the workspace (a procedural macro a workspace crate defines,
//! matched by the last segment of its path, or by a name a `use` gives
//! it, `r#` dropped; `derive` itself may be one a `use` names so), which
//! may write any item beside the one it applies to. Extraction reads
//! attributes and derive lists from the tree, through every `cfg_attr`
//! and every `derive(…)` of one; an attribute or a derive it cannot read
//! (a doc comment in it, a token where a path goes, `cfg_attr`s nested
//! past `types::MAX_NESTING`) may be any of them, and a function carrying
//! such an attribute may define a derive of any name. A
//! `macro_rules!` definition writes nothing until it is invoked, and
//! counts for nothing; code below the walker's depth cap that holds an
//! impl, a `macro_rules!` or a module counts as a macro (D17).
//!
//! `cfg(test)` holds only in the crate under test, never in a crate
//! compiled as a dependency, so a macro under it counts only for callers
//! in its own crate.
//!
//! Two macros write no impl of a type of the workspace, and count for
//! nothing: std's `thread_local!`, which defines the statics it names, and
//! the `lazy_static` crate's `lazy_static!`, which also writes a struct of
//! each static's name, with impls of `Deref` and `LazyStatic` on it.
//! Extraction reads the statics; the struct may shadow a glob's type of
//! the same name, so the scope of a `lazy_static!` call is marked as a
//! macro's, though it counts for nothing. Both put a static's attributes
//! on the item they write for it (the const std's writes, the struct
//! `lazy_static!` writes), where a derive or an attribute macro may write
//! anything, so a call holding an attribute that is not inert (a `derive`
//! included, extraction's `holds_attribute`) is not theirs. A call by one
//! of these names is theirs only when nothing else may be the macro
//! invoked: no
//! `macro_rules!` and no function-like procedural macro of a crate its
//! crate compiles has the name, no `use` there gives the name to anything
//! else (`#[macro_use]`, an import or a re-export could bring either), its
//! path does not resolve into the workspace nor to what the rules cannot
//! read (a glob of a dependency, std's included, may hold a macro of the
//! name), and nothing else counts in the crates its crate compiles (an
//! expansion there may define a macro of the name, in textual scope at the
//! call). A dependency's macro of either name is taken for theirs.
//! Otherwise the call counts, and its scope is marked as a macro's: it may
//! define any item there.
//!
//! A derive of std (`Clone`, `Debug`, …) writes the impl of its own trait,
//! whose methods the probe knows (`methods::DERIVED`), and needs no more.
//! A derive of a dependency is a procedural macro: its own trait's impl
//! would matter only where that trait is in scope, but it may write any
//! item beside its type, an inherent method among them (`getset`,
//! `derive-new`, `typed-builder` write some), which comes before a trait's
//! wherever the call is, or an impl of one of std's prelude traits
//! (`derive_more::AsRef`). The rules cannot list what it writes, so a
//! probe on a type carrying a derive that is not std's, or one the rules
//! cannot read, is unresolved too ([`Index::foreign_derive`]; one of the
//! workspace counts for its whole crate already).
//!
//! What the rules still read wrong: a dependency's attribute macro may
//! rewrite the item it applies to and add others, and is read as an inert
//! attribute (the rows do not tell one from a derive's helper attribute,
//! `#[serde(…)]`); a dependency's derive may write an inherent impl of
//! another type of its crate, or a type that shadows a glob's (a
//! builder's), and only the probe on the type carrying it is unresolved
//! (counting it crate-wide would leave unresolved most probes of every
//! crate deriving serde's or clap's traits, and marking its scope every
//! name a glob brings there); a macro invoked in a block, a statement's or
//! an expression's, may write an impl too, and is not read, but for a
//! `thread_local!` or `lazy_static!` holding an attribute that is not
//! inert, which counts as one among items does. Every rule was
//! checked with rustc and cargo 1.99: a macro-defined `macro_rules!` may
//! not shadow std's `thread_local!` (E0659), but may define a
//! `lazy_static!` the lazy_static crate is not there to take.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use super::cfg;
use super::modules::{Target, applies};
use super::resolve::{Index, Ns, Pos, Res};
use super::types::unraw;
use super::{DepKind, ItemKind, Leaf, MacroCall, ProcKind, UNREAD_NAME, Use, Vis};

/// The derives of std, whose impls `methods::DERIVED` knows (`Copy` and
/// `Eq` add no method).
const STD_DERIVES: [&str; 9] = [
    "Clone",
    "Copy",
    "Debug",
    "Default",
    "Eq",
    "Hash",
    "Ord",
    "PartialEq",
    "PartialOrd",
];

/// The macros of the workspace by name (D23): the derive and attribute
/// macros with the names a `use` gives them, and, by crate, the names a
/// macro invoked as `name!` may be one of the crate's under: a
/// `macro_rules!`, a function-like procedural macro, or any name a `use`
/// gives to something of another name.
#[derive(Default)]
struct Defined<'a> {
    derive: HashSet<&'a str>,
    attribute: HashSet<&'a str>,
    bang: HashMap<Option<usize>, HashSet<&'a str>>,
}

/// Whether a macro counts in one crate, for every caller and only under
/// `cfg(test)`.
#[derive(Clone, Copy, Default)]
struct Counted {
    every: bool,
    own: bool,
}

impl Counted {
    fn add(&mut self, chain: &[String]) {
        if chain.iter().any(|c| c == "test") {
            self.own = true;
        } else {
            self.every = true;
        }
    }
}

impl Index<'_> {
    /// Reads every macro invoked where items go and every derive or
    /// attribute macro of the workspace applied (D23): marks the scopes
    /// those may define items in that extraction left unmarked (the
    /// derives', the attributes', the calls of `thread_local!` and
    /// `lazy_static!` that are not std's or the dependency's, and those of
    /// `lazy_static!` that are, which write a struct), and finds
    /// the crates whose method probes are unresolved. Runs before any
    /// other lookup is kept: the memos of those it made are dropped.
    pub fn read_macros(&mut self) {
        let defined = self.macros_defined();
        let mut counted: HashMap<Option<usize>, Counted> = HashMap::default();
        let mut marks: Vec<(usize, u32)> = Vec::new();
        // The calls by a name of the statics macros whose name and path
        // are theirs, by instance: (file, scope, crate, `#[cfg]`s).
        let mut statics: Vec<(usize, u32, Option<usize>, Vec<String>)> = Vec::new();
        // The crates compiled with a macro of each statics macro's name.
        let named: HashMap<&str, HashSet<Option<usize>>> = ["thread_local", "lazy_static"]
            .into_iter()
            .map(|name| {
                let has = |krate| {
                    defined
                        .bang
                        .get(&krate)
                        .is_some_and(|names| names.contains(name))
                };
                (name, self.compiled_with(has))
            })
            .collect();
        for (file, source) in self.files.iter().enumerate() {
            for call in &source.facts.macro_calls {
                for &module in self.instances(file, call.scope) {
                    let chain = cfg::union(&call.cfg, &self.tree.modules[module].cfg);
                    if cfg::is_false(&chain) {
                        continue;
                    }
                    let krate = self.tree.modules[module].krate;
                    let pos = Pos {
                        file,
                        scope: call.scope,
                        module,
                    };
                    let name = call.path.last().filter(|_| call.statics);
                    let theirs = name
                        .and_then(|name| named.get(name.as_str()))
                        .is_some_and(|compiled| !compiled.contains(&krate))
                        && self.as_caller(chain.clone(), || self.statics_path(pos, call));
                    if theirs {
                        // `lazy_static!` writes a struct of each static's
                        // name, which may shadow a glob's type.
                        if name.is_some_and(|name| name == "lazy_static") {
                            marks.push((file, call.scope));
                        }
                        statics.push((file, call.scope, krate, chain));
                    } else {
                        counted.entry(krate).or_default().add(&chain);
                        if call.statics {
                            marks.push((file, call.scope));
                        }
                    }
                }
            }
            for (scope, cfg) in self.procedural_uses(file, &defined) {
                marks.push((file, scope));
                for &module in self.instances(file, scope) {
                    let chain = cfg::union(cfg, &self.tree.modules[module].cfg);
                    if !cfg::is_false(&chain) {
                        let krate = self.tree.modules[module].krate;
                        counted.entry(krate).or_default().add(&chain);
                    }
                }
            }
        }
        drop(named);
        drop(defined);
        // The crates where a macro counts: their own, or one of a crate
        // they compile outside `cfg(test)`.
        let every = |counted: &HashMap<Option<usize>, Counted>| {
            self.compiled_with(|with| counted.get(&with).is_some_and(|c| c.every))
        };
        let own = |krate: Option<usize>, counted: &HashMap<Option<usize>, Counted>| {
            counted.get(&krate).is_some_and(|c| c.own)
        };
        // Anything else counting where a statics call is compiled may
        // define a macro of its name; under `cfg(test)` only, the call is
        // that macro's only there too.
        let counting = every(&counted);
        let shadowed: Vec<_> = statics
            .into_iter()
            .filter_map(|(file, scope, krate, chain)| {
                if counting.contains(&krate) {
                    Some((file, scope, krate, chain))
                } else if own(krate, &counted) {
                    Some((file, scope, krate, cfg::union(&chain, &["test".to_owned()])))
                } else {
                    None
                }
            })
            .collect();
        for (file, scope, krate, chain) in shadowed {
            counted.entry(krate).or_default().add(&chain);
            marks.push((file, scope));
        }
        let counting = every(&counted);
        let crates = self.tree.crates.len();
        self.macro_crates = std::iter::once(None)
            .chain((0..crates).map(Some))
            .filter(|krate| own(*krate, &counted) || counting.contains(krate))
            .collect();
        marks.sort_unstable();
        marks.dedup();
        for (file, scope) in marks {
            self.mark(file, scope);
        }
        self.memo.borrow_mut().clear();
        self.in_scope.borrow_mut().clear();
    }

    /// The macros of the workspace by name.
    fn macros_defined(&self) -> Defined<'_> {
        let mut defined = Defined::default();
        for (file, source) in self.files.iter().enumerate() {
            let mut bang: Vec<(u32, &str)> = Vec::new();
            for item in &source.facts.items {
                if matches!(item.kind, ItemKind::Macro { .. }) && !cfg::is_false(&item.cfg) {
                    bang.push((item.scope, unraw(&item.name)));
                }
            }
            for leaf in &source.facts.uses {
                if let Some((_, to)) = renamed(leaf) {
                    bang.push((leaf.scope, to));
                }
            }
            for proc in &source.facts.proc_macros {
                match proc.kind {
                    ProcKind::Bang => bang.push((0, &proc.name)),
                    ProcKind::Derive => {
                        defined.derive.insert(proc.name.as_str());
                    }
                    ProcKind::Attribute => {
                        defined.attribute.insert(proc.name.as_str());
                    }
                }
            }
            for (scope, name) in bang {
                for &module in self.instances(file, scope) {
                    let krate = self.tree.modules[module].krate;
                    defined.bang.entry(krate).or_default().insert(name);
                }
            }
        }
        // A derive or attribute macro is used by the name a `use` gives it
        // (`use pm::add_get as traced;`, through re-exports too), matched
        // by name as the macro is: each name is followed once along the
        // renames of it.
        let mut renames: HashMap<&str, Vec<&str>> = HashMap::default();
        for leaf in self.files.iter().flat_map(|source| &source.facts.uses) {
            if let Some((from, to)) = renamed(leaf) {
                renames.entry(from).or_default().push(to);
            }
        }
        for names in [&mut defined.derive, &mut defined.attribute] {
            let mut pending: Vec<&str> = names.iter().copied().collect();
            while let Some(name) = pending.pop() {
                for &to in renames.get(name).into_iter().flatten() {
                    if names.insert(to) {
                        pending.push(to);
                    }
                }
            }
        }
        defined
    }

    /// The scopes, with their `#[cfg]`s, of a file's items that invoke a
    /// derive or attribute macro of the workspace by its name (the last
    /// segment of the path written). A derive or an attribute the rules
    /// cannot read may be any of them, and a derive macro whose name they
    /// cannot read may be any derive.
    fn procedural_uses<'s>(&'s self, file: usize, defined: &Defined) -> Vec<(u32, &'s [String])> {
        let facts = &self.files[file].facts;
        let derive = |name: &str| match name {
            UNREAD_NAME => !defined.derive.is_empty(),
            _ => defined.derive.contains(name) || defined.derive.contains(UNREAD_NAME),
        };
        let attribute = |name: &str| {
            derive(name)
                || match name {
                    UNREAD_NAME => !defined.attribute.is_empty(),
                    _ => defined.attribute.contains(name),
                }
        };
        let derived = facts.items.iter().filter(|item| match &item.kind {
            ItemKind::Struct { derives, .. }
            | ItemKind::Enum { derives, .. }
            | ItemKind::Union { derives } => derives.iter().any(|d| derive(d)),
            _ => false,
        });
        let attributed = facts.attributes.iter().filter(|used| attribute(&used.name));
        derived
            .map(|item| (item.scope, item.cfg.as_slice()))
            .chain(attributed.map(|used| (used.scope, used.cfg.as_slice())))
            .collect()
    }

    /// Whether the path of a call at `pos` by the name of std's
    /// `thread_local!` or the `lazy_static` crate's macro is theirs: it
    /// names nothing of the workspace nor what the rules cannot read (a
    /// glob of a dependency, a module a macro marked). Whether a macro of
    /// a crate its crate compiles has the name or is given it by a `use`,
    /// and whether an expansion may define one, are read apart.
    fn statics_path(&self, pos: Pos, call: &MacroCall) -> bool {
        match self.path(pos, &call.path, Ns::Macro, 0) {
            Res::External => true,
            Res::Absent => call.path.len() == 1,
            Res::Found(_) | Res::Unknown => false,
        }
    }

    /// Leaves the mark extraction leaves where a macro is invoked among
    /// items (a public glob of nothing) in a scope of a file.
    fn mark(&mut self, file: usize, scope: u32) {
        let source = &mut self.files[file];
        let leaf = source.facts.uses.len() as u32;
        source.facts.uses.push(Use {
            scope,
            vis: Vis::Public,
            path: Vec::new(),
            leaf: Leaf::Glob,
            local: None,
            cfg: Vec::new(),
        });
        source.globs.entry(scope).or_default().push(leaf);
    }

    /// The crates compiled with one that answers `test`: a crate is
    /// compiled with itself, its package's library for its other targets,
    /// the workspace libraries its package depends on for that target, and
    /// their normal dependencies, transitively. A file no target reaches is
    /// a crate of its own (`None`). Each crate is read once: the libraries
    /// compiled with one that answers are found along the normal
    /// dependencies backwards, as a workspace may hold many crates.
    fn compiled_with(&self, test: impl Fn(Option<usize>) -> bool) -> HashSet<Option<usize>> {
        let crates = &self.tree.crates;
        let dependencies = |package: usize| self.tree.dependencies(package);
        // The crates that, compiled as a dependency, compile one that
        // answers: with none answering, none (the dependencies, which a
        // workspace may declare by the million, are not read).
        let mut pending: Vec<usize> = (0..crates.len()).filter(|&c| test(Some(c))).collect();
        if pending.is_empty() {
            return std::iter::once(None).filter(|_| test(None)).collect();
        }
        let mut reaching = vec![false; crates.len()];
        for &krate in &pending {
            reaching[krate] = true;
        }
        let mut libs: HashMap<usize, Vec<usize>> = HashMap::default();
        // By library, the crates depending on it: `u32`s, as a workspace
        // may declare many dependencies.
        let mut dependents: Vec<Vec<u32>> = vec![Vec::new(); crates.len()];
        for (krate, c) in crates.iter().enumerate() {
            if c.target == Target::Lib {
                libs.entry(c.package).or_default().push(krate);
            }
            for dependency in dependencies(c.package) {
                if let (DepKind::Normal, Some(lib)) = (dependency.kind(), dependency.lib()) {
                    dependents[lib as usize].push(krate as u32);
                }
            }
        }
        while let Some(lib) = pending.pop() {
            for &dependent in &dependents[lib] {
                if !reaching[dependent as usize] {
                    reaching[dependent as usize] = true;
                    pending.push(dependent as usize);
                }
            }
        }
        let reached = |krate: usize| {
            let (target, package) = (crates[krate].target, crates[krate].package);
            let own = !matches!(target, Target::Lib | Target::Build)
                && libs
                    .get(&package)
                    .into_iter()
                    .flatten()
                    .any(|lib| reaching[*lib]);
            own || dependencies(package).any(|dependency| {
                applies(dependency.kind(), target)
                    && dependency.lib().is_some_and(|lib| reaching[lib as usize])
            })
        };
        std::iter::once(None)
            .filter(|_| test(None))
            .chain(
                (0..crates.len())
                    .filter(|&krate| test(Some(krate)) || reached(krate))
                    .map(Some),
            )
            .collect()
    }

    /// Whether the method probes of the code at `pos` are unresolved: a
    /// macro counts in a crate its crate compiles (D23).
    pub fn macro_probe(&self, pos: Pos) -> bool {
        self.macro_crates
            .contains(&self.tree.modules[pos.module].krate)
    }

    /// Whether a type carries a derive that is not std's: a dependency's,
    /// which may write any item beside it (D23), or one the rules cannot
    /// read; one of the workspace counts for its whole crate already.
    pub fn foreign_derive(&self, ty: (usize, u32)) -> bool {
        let derives = match self.files[ty.0].item_kind(ty.1) {
            Some(
                ItemKind::Struct { derives, .. }
                | ItemKind::Enum { derives, .. }
                | ItemKind::Union { derives },
            ) => derives,
            _ => return false,
        };
        derives
            .iter()
            .any(|derive| !STD_DERIVES.contains(&derive.as_str()))
    }
}

/// The name a `use` takes and the other one it gives it (`use pm::add_get
/// as traced;`), each without `r#`; `None` when it keeps the name.
fn renamed(leaf: &Use) -> Option<(&str, &str)> {
    let (from, to) = (unraw(leaf.path.last()?), unraw(leaf.local.as_deref()?));
    (from != to).then_some((from, to))
}
