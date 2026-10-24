// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Deciding every Rust site of a generation from stored rows alone, on
//! Rust's own name resolution (decision 1 of the port):
//!
//! - **Scopes.** A name is looked up from the site outward: the blocks
//!   around it (their items and `use`s, then their globs), then its module
//!   (items and `use`s, then globs), then the crate names its package's
//!   dependencies give it (a workspace library only through a path
//!   dependency). An inline module does not see its parent's names, and
//!   a block's items are visible in the whole block. A local, a parameter or
//!   a generic parameter of the site's name is never an item (`Form::Opaque`).
//! - **Paths.** `crate::`, `self::`, `super::`, `Self`, `::krate`, a
//!   workspace crate's name, a name bound by `use` or defined in scope; then
//!   each segment inside the module, enum, trait or type the previous one
//!   named. A path through anything outside the workspace is unresolved.
//! - **Globs.** A glob proves a name only when every glob of the scope is
//!   of a workspace module or enum and exactly one provides the name
//!   (visibility read as Rust does), and no item or `use` of the scope
//!   binds it: explicit beats glob. A glob the rules cannot read leaves a
//!   name it might provide unresolved, except a `use` path's first segment
//!   an outer scope provides (Rust rejects that ambiguity in an import).
//!   A path into another module sees only what that module shows it: a
//!   private `use` or glob there binds nothing here.
//! - **Duplicates.** Two definitions of one name in one scope and
//!   namespace prove neither, whatever `#[cfg]`s the site carries (below).
//! - **`#[cfg]`** (F2). Each item, `use`, associated item, variant and
//!   site carries the conjuncts of the `#[cfg]`s around it (its own, its
//!   impl's or trait's, its enclosing modules' and blocks', the statement's,
//!   and those of the `mod x;` declarations leading to its file). What a
//!   predicate Rust proves false (`any()`, `not(all())`, `false`) holds is
//!   absent; a site in it is unresolved. A binding is conditional for a
//!   site when it carries a conjunct the site does not. Among the
//!   candidates of one stage (a scope's items and `use`s, its globs, an
//!   enum's variants, a probe step) the unconditional ones decide, since a
//!   conditional one beside them would be a second definition wherever it
//!   exists (rustc's E0428, E0592, E0659, E0034); conditional ones alone
//!   decide only when they agree (`unix` and `windows` variants do not).
//!   Where they so decide among the bindings of a name, the stage answers
//!   what it answers where `#[cfg]`s are not read, or nothing; among the
//!   methods of a probe, nothing: a macro the rules do not read (a
//!   statement's, an attribute's, a derive's) may write a binding of the
//!   name between the site and them, or a method the probe takes first
//!   (an inherent one before a trait's, a by-value one a step earlier),
//!   which rustc takes; so `unix` and `windows` variants stay ambiguous
//!   for a site carrying one of them too. A path naming the trait
//!   (`<T as Tr>::f`, `Tr::f`) still picks among the impls of `Tr` for `T`
//!   or the items of `Tr`: no macro adds one beside them. A conditional
//!   answer stands only when nothing else could take its place where it is
//!   absent: no later stage of the lexical lookup, crate name or name of
//!   the std prelude, no glob of the module a path enters, no other
//!   candidate of the probe or of `T::f`, no default of the trait an impl
//!   overrides.
//! - **Associated items** (`T::f`, `Self::f`): `T`'s inherent impls first,
//!   those the caller can see (an inherent item is seen from its impl's
//!   module and that module's descendants unless its visibility says more,
//!   and rustc skips one the caller cannot see, D19); else exactly one
//!   trait impl of `T` whose trait is in scope (a trait of the std prelude
//!   always; another trait outside the workspace only where an impl
//!   around the site implements one or a binding outside the workspace or
//!   one the rules cannot read may bring it, and then only when nothing
//!   else provides `f`), or one trait default it inherits, provides `f`, and nothing
//!   hidden competes: no `#[derive]` of std, no blanket impl of the
//!   workspace, no blanket impl std gives every type (`From<T> for T`,
//!   `Into`, `ToString`, …), no macro among the impl's items, no impl
//!   whose self type the rules cannot resolve or that the crates compiling
//!   its file read as different types (any type may have its items, D18).
//! - **Macros where items go** (D23, `macros`): a macro invoked among a
//!   module's, an impl's or a trait's items may define any item there,
//!   seen from every module, and write impls, which no row shows. A method
//!   probe (`x.f()`, `T::f`, `<T>::f`, `<T as Tr>::f`) is unresolved in a
//!   crate compiling such a macro, its own or a workspace library's it
//!   depends on, or a derive or attribute macro of the workspace
//!   (`cfg(test)` ones only for callers in their crate), and on a type
//!   carrying a derive that is not std's. `thread_local!` and
//!   `lazy_static!` count for nothing when they are std's and the
//!   `lazy_static` crate's: no macro of the workspace has the name or is
//!   given it, the path names nothing of the workspace, and nothing else
//!   counts where the call is compiled. A `lazy_static!` call's scope is
//!   marked all the same: it writes a struct of each static's name.
//! - **Qualified paths** (`<T as Tr>::f`, D20): the item of the impl of
//!   `Tr` the rules see for exactly `T` (a type of the workspace, its
//!   generic arguments, behind one `&` or `&mut` at most) and for the
//!   arguments the path writes on `Tr` (`Tr<u8>` and `Tr<u16>` are two
//!   traits; none on either side is the trait's defaults), else the default
//!   of `Tr` that impl takes; with no such impl (a type outside the
//!   workspace, a primitive, a pointer, a blanket impl's, an alias the
//!   crates compiling its file read as two types, trait arguments left to
//!   inference) the path is unresolved. A generic parameter names the
//!   trait's own item. A path to a trait's method (`Tr::m(&mut x)`) takes
//!   `Self` from its first argument as the method's `self` takes it, when
//!   the code states the argument's type, and names the item `<Self as
//!   Tr>::m` would when `Tr` has no type or const parameter: the call
//!   infers the trait's arguments where `<Self as Tr>` takes its defaults,
//!   so a generic trait's path is unresolved. A generic parameter, `dyn
//!   Tr` or `impl Tr` names the trait's own item, and any other argument,
//!   or none, leaves it unresolved, as does the path passed as a value
//!   (`map(Tr::m)`).
//! - **Method calls** (`x.f()`): only when the code states `x`'s type
//!   (`Receiver`): a value `T::f(…)` makes is a `T` when `f` returns
//!   `Self` or `T` (an `async fn` returns a future); what `?`, `.unwrap()`
//!   or `.expect(…)` takes out of one, when `f` returns std's `Result` or
//!   `Option` of it, named as the prelude gives it, by its path in `std` or
//!   `core`, or through one alias of the workspace (pixel#108; a
//!   dependency's or std's own alias is not read). Then Rust's method
//!   probe over that type's methods (see
//!   `methods`), which must pick the same method whether `x` is a `T`, a
//!   `&T` or a `&mut T`. An `impl … for &T` providing the name, or a typed
//!   `self` (`self: Box<Self>`), leaves it unresolved. A type reached
//!   through `Box`, `Rc` or `Arc` excludes the methods those pointers have
//!   themselves, blanket impls, and trait methods of traits outside the
//!   workspace. `dyn Tr`, `impl Tr` and `self` in a trait's body call the
//!   trait's own method, unless a blanket impl provides the name too.
//! - **Generic arguments** (D43, `args`): an impl for `G<u8>` is a
//!   candidate for a value stated to be a `G<u8>` only, `impl<T> G<T>` for
//!   every `G`, and `impl<T: Tr> G<T>` for every `G` only when `G` itself
//!   declares `T: Tr`; a parameter held to more (a bound, a `where`
//!   predicate, a second use, `Sized` where `G` says `?Sized`) applies to
//!   some arguments, which the rules do not evaluate. Arguments are the
//!   same type when their names resolve alike, different when they
//!   resolve to two types of the workspace or two primitives. An impl that
//!   may apply gives candidates that stand only when nothing else provides
//!   the name (in `<T as Tr>::f`, when no impl of `Tr` is proven for `T`,
//!   coherence ruling out a second); impls for different arguments
//!   providing the name when the value's are unknown (`T::f`, a
//!   constructor's value) leave it unresolved. A type written through an
//!   alias, on an impl or a value, is the alias's definition with the
//!   arguments written on the alias in its parameters' places; an impl on
//!   an alias the rules cannot expand (an alias of an alias, a parameter
//!   left to its default, an alias several crates compile) may apply to
//!   any value.
//! - **Imports.** A `use` leaf resolves to the item, inline module or module
//!   file its path names.
//!
//! A file several crates compile (`mod util;` in `lib.rs` and `main.rs`)
//! has each site decided in each; the site resolves only when every crate
//! agrees.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::cell::{Cell, RefCell};
use std::panic::AssertUnwindSafe;

use pixel_model::{Edge, EdgeKind, SymbolKind, uid_path};

use super::args::{ArgTypes, Effective};
use super::cfg;
use super::modules::{self, ModId, Tree};
use super::{Argument, Context, Facts, Form, ItemKind, Manifest, ScopeKind, SelfKind};
use crate::extract::uid;
use crate::lang::World;

/// Every site of the generation, decided. A panic in the rules (a bug, as
/// facts are read defensively) leaves every site unresolved rather than
/// failing the run, as v1's extraction did for a file (G-4).
pub fn resolve(world: &World<'_>) -> Vec<Option<usize>> {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let index = Index::build(world);
        world.edges.iter().map(|edge| index.decide(edge)).collect()
    }))
    .unwrap_or_else(|_| vec![None; world.edges.len()])
}

/// Past this many nested steps (a `use` resolved through another, a glob
/// through a glob, an alias through an alias) a name is unresolved.
pub const DEPTH: u32 = 32;

/// A name's namespace in Rust.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ns {
    Type,
    Value,
    Macro,
}

/// What a name or a path resolves to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Def {
    /// An item: (file, item).
    Item(usize, u32),
    /// An enum variant: (file, variant).
    Variant(usize, u32),
    /// An associated function or constant: (file, assoc).
    Assoc(usize, u32),
    Module(ModId),
}

/// The outcome of a lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Res {
    Found(Def),
    /// Bound to something outside the workspace, or to nothing the scope
    /// holds (the prelude, a dependency).
    External,
    /// Not bound here: the search goes on outward.
    Absent,
    /// Bound, but the rules cannot prove to what.
    Unknown,
}

impl Res {
    /// One answer from the candidates of one scope: a single target, or
    /// unknown when they disagree.
    pub fn combine(results: impl IntoIterator<Item = Res>) -> Res {
        let mut found = None;
        let mut external = false;
        for result in results {
            match result {
                Res::Unknown => return Res::Unknown,
                Res::Absent => {}
                Res::External => external = true,
                Res::Found(def) => {
                    if found.is_some_and(|f| f != def) {
                        return Res::Unknown;
                    }
                    found = Some(def);
                }
            }
        }
        match (found, external) {
            (Some(_), true) => Res::Unknown,
            (Some(def), false) => Res::Found(def),
            (None, true) => Res::External,
            (None, false) => Res::Absent,
        }
    }
}

/// Where a lookup starts: a scope of a file, as a module of one crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pos {
    pub file: usize,
    pub scope: u32,
    pub module: ModId,
}

/// A name a scope binds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    Item(u32),
    Use(u32),
}

/// One source file's facts with the tables lookups read.
pub struct SourceFile<'w> {
    pub path: &'w str,
    pub facts: Facts,
    /// The module scope of each scope (itself for a module or the file).
    pub module_scope: Vec<u32>,
    /// Scope → name → the items and `use`s binding it.
    pub names: HashMap<u32, HashMap<String, Vec<Entry>>>,
    /// Scope → its glob `use`s.
    pub globs: HashMap<u32, Vec<u32>>,
    /// Scope → every `use` leaf naming an item, `as _` included: what
    /// brings a trait into scope.
    pub named_uses: HashMap<u32, Vec<u32>>,
    /// Impl or trait scope → name → its associated items.
    pub assocs: HashMap<u32, HashMap<String, Vec<u32>>>,
    /// Enum item → name → its variants (two under different `#[cfg]`s).
    pub variants: HashMap<u32, HashMap<String, Vec<u32>>>,
    /// Trait item → its scope.
    pub trait_scope: HashMap<u32, u32>,
    /// Inline module scope → its `mod` item.
    pub module_item: HashMap<u32, u32>,
    /// Module scope → the `mod` and `macro_rules!` items declared in it or
    /// in its blocks: what the module tree reads.
    pub declarations: HashMap<u32, Vec<u32>>,
}

impl<'w> SourceFile<'w> {
    fn new(path: &'w str, facts: Facts) -> SourceFile<'w> {
        let mut module_scope = Vec::with_capacity(facts.scopes.len());
        let mut trait_scope = HashMap::default();
        let mut module_item = HashMap::default();
        for (index, scope) in facts.scopes.iter().enumerate() {
            let own = index as u32;
            let module = match (&scope.kind, scope.parent) {
                (ScopeKind::File | ScopeKind::Module { .. }, _) | (_, None) => own,
                (_, Some(parent)) => module_scope.get(parent as usize).copied().unwrap_or(0),
            };
            module_scope.push(module);
            match scope.kind {
                ScopeKind::Trait { item } => {
                    trait_scope.insert(item, own);
                }
                ScopeKind::Module { item } => {
                    module_item.insert(own, item);
                }
                _ => {}
            }
        }
        let mut names: HashMap<u32, HashMap<String, Vec<Entry>>> = HashMap::default();
        let mut declarations: HashMap<u32, Vec<u32>> = HashMap::default();
        // What a provably false `#[cfg]` removes is absent for resolution
        // (F2): it binds no name, provides no associated item or variant.
        for (index, item) in facts.items.iter().enumerate() {
            if cfg::is_false(&item.cfg) {
                continue;
            }
            if matches!(item.kind, ItemKind::Module { .. } | ItemKind::Macro { .. })
                && let Some(module) = module_scope.get(item.scope as usize)
            {
                declarations.entry(*module).or_default().push(index as u32);
            }
            names
                .entry(item.scope)
                .or_default()
                .entry(item.name.clone())
                .or_default()
                .push(Entry::Item(index as u32));
        }
        let mut globs: HashMap<u32, Vec<u32>> = HashMap::default();
        let mut named_uses: HashMap<u32, Vec<u32>> = HashMap::default();
        for (index, leaf) in facts.uses.iter().enumerate() {
            if cfg::is_false(&leaf.cfg) {
                continue;
            }
            if leaf.leaf == super::Leaf::Name {
                named_uses.entry(leaf.scope).or_default().push(index as u32);
            }
            match (&leaf.local, leaf.leaf) {
                (_, super::Leaf::Glob) => globs.entry(leaf.scope).or_default().push(index as u32),
                (Some(local), _) => names
                    .entry(leaf.scope)
                    .or_default()
                    .entry(local.clone())
                    .or_default()
                    .push(Entry::Use(index as u32)),
                (None, _) => {}
            }
        }
        let mut assocs: HashMap<u32, HashMap<String, Vec<u32>>> = HashMap::default();
        for (index, assoc) in facts.assocs.iter().enumerate() {
            if cfg::is_false(&assoc.cfg) {
                continue;
            }
            assocs
                .entry(assoc.owner)
                .or_default()
                .entry(assoc.name.clone())
                .or_default()
                .push(index as u32);
        }
        let mut variants: HashMap<u32, HashMap<String, Vec<u32>>> = HashMap::default();
        for (index, variant) in facts.variants.iter().enumerate() {
            if cfg::is_false(&variant.cfg) {
                continue;
            }
            variants
                .entry(variant.item)
                .or_default()
                .entry(variant.name.clone())
                .or_default()
                .push(index as u32);
        }
        SourceFile {
            path,
            facts,
            module_scope,
            names,
            globs,
            named_uses,
            assocs,
            variants,
            trait_scope,
            module_item,
            declarations,
        }
    }

    /// The `mod` and `macro_rules!` items of a module scope.
    pub fn declared_in(&self, module: u32) -> &[u32] {
        self.declarations.get(&module).map_or(&[], Vec::as_slice)
    }

    /// The associated items of `owner` named `name`.
    pub fn assocs_named(&self, owner: u32, name: &str) -> &[u32] {
        self.assocs
            .get(&owner)
            .and_then(|by_name| by_name.get(name))
            .map_or(&[], Vec::as_slice)
    }

    pub fn item_kind(&self, item: u32) -> Option<&ItemKind> {
        self.facts.items.get(item as usize).map(|i| &i.kind)
    }
}

/// A workspace trait asked about at a position: (module, file, scope,
/// trait file, trait item).
pub type TraitAt = (ModId, usize, u32, usize, u32);

/// An impl block attached to the type it implements.
#[derive(Clone, Copy, Debug)]
pub struct Impl {
    pub file: usize,
    pub scope: u32,
    pub trait_of: TraitOf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraitOf {
    Inherent,
    /// A trait of the workspace: (file, item).
    Workspace(usize, u32),
    /// A trait of the std prelude of every edition (`Clone`, `Iterator`,
    /// `From`), named so or by its std path: in scope everywhere.
    Prelude,
    /// A trait outside the workspace, or one the rules cannot name.
    Other,
}

/// Everything resolution reads, built once per generation.
pub struct Index<'w> {
    pub world: &'w World<'w>,
    pub files: Vec<SourceFile<'w>>,
    pub by_path: HashMap<&'w str, usize>,
    pub symbols: HashMap<&'w str, usize>,
    pub tree: Tree,
    /// Type item → the impls of that type (by value, not `impl … for &T`).
    pub impls: HashMap<(usize, u32), Vec<Impl>>,
    /// Type item → its impls by the names of their associated items: a
    /// probe looks a name up once, not once per impl.
    pub impl_assocs: HashMap<(usize, u32), NamedImpls>,
    /// Type item → the names of the associated items its `impl … for &T`
    /// blocks provide, whose receivers the probe does not place.
    pub by_ref: HashMap<(usize, u32), HashSet<String>>,
    /// (type item, `&mut`) → its `impl … for &T` or `&mut T` blocks, which
    /// `<&T as Tr>::f` names (D20).
    pub ref_impls: HashMap<((usize, u32), bool), Vec<Impl>>,
    /// Method names some blanket impl (`impl<T> Tr for T`) provides, with
    /// how each takes `self`.
    pub blanket: HashMap<String, Vec<SelfKind>>,
    /// Method names some impl on `Box`, `Rc` or `Arc` provides.
    pub pointer_methods: HashSet<String>,
    /// The names of the associated items of impls the rules cannot attach
    /// to one type (D18): any type may have them.
    pub unplaced: HashSet<String>,
    /// The arguments an attached impl, by (file, scope), is for, where they
    /// differ from those written on its self type: each parameter read
    /// against the bounds its type declares (`args::Index::effective_args`),
    /// and an alias's definition in place of the alias (D43).
    pub effective: HashMap<(usize, u32), Effective>,
    /// The types an impl on an alias the rules cannot expand exactly is
    /// for, not attached: every probe on them is unresolved (D43, D20).
    pub aliased: HashSet<(usize, u32)>,
    /// The crates whose method probes are unresolved: a macro invoked
    /// where items go, or a derive or attribute macro of the workspace,
    /// counts in a crate compiled into them (D23).
    pub macro_crates: HashSet<Option<usize>>,
    /// Whether a workspace trait is in scope where it was asked about, for
    /// a caller.
    pub in_scope: RefCell<HashMap<(TraitAt, u32), InScope>>,
    /// `use` bindings already resolved, by (module, file, use, namespace)
    /// and caller.
    pub memo: RefCell<HashMap<(UseAt, u32), Res>>,
    /// `use` bindings being resolved, with their caller: met again, they
    /// bind nothing.
    pub resolving: RefCell<HashSet<(UseAt, u32)>>,
    /// Each file's `#[cfg]` conjuncts from the `mod` declarations leading
    /// to it, every crate compiling it together.
    pub file_cfg: Vec<Vec<String>>,
    /// The `#[cfg]` conjuncts the code being resolved carries (sorted):
    /// the site's and its module's, or an impl's while it is attached
    /// (F2); and their number among the callers met, for the memos.
    pub caller: RefCell<(Vec<String>, u32)>,
    pub callers: RefCell<HashMap<Vec<String>, u32>>,
    /// Set when what is being resolved read a binding or a module with
    /// `#[cfg]` conjuncts: only then does its answer depend on the caller,
    /// and only then is it memoized for that caller alone.
    pub sensitive: Cell<bool>,
    /// Set when a glob stage the caller's `#[cfg]`s told apart is met in a
    /// glob walk, for the walk's first stage to ask again (`globs`).
    pub told_globs: Cell<bool>,
    /// The first stage of a glob walk as every caller answers it, by
    /// (file, scope, module, namespace, importer, depth) and name, with
    /// whether it read a `#[cfg]` (`globs`).
    pub every_globs: RefCell<HashMap<GlobsAt, ByName>>,
    /// What an argument's text proves it is where it is read, by (file,
    /// scope, module) and caller, then text (`Index::arg_type`).
    pub arg_types: RefCell<HashMap<(ArgAt, u32), ArgTypes>>,
}

/// The memo key's caller for an answer no caller changes.
pub const ANY_CALLER: u32 = u32::MAX;

/// The caller that carries every `#[cfg]` conjunct ([`Index::as_every_caller`]).
const EVERY_CALLER: u32 = u32::MAX - 1;

/// Name → the impls of a type with associated items of that name, by
/// their index in [`Index::impls`], in order, with those items.
pub type NamedImpls = HashMap<String, Vec<(usize, Vec<u32>)>>;

/// A `use` binding asked about: (module, file, use, namespace).
pub type UseAt = (ModId, usize, u32, Ns);

/// A glob stage asked about: (file, scope, module, namespace, importer,
/// depth).
pub type GlobsAt = (usize, u32, ModId, Ns, ModId, u32);

/// Where an argument is read: (file, scope, module).
pub type ArgAt = (usize, u32, ModId);

/// Name → an answer, with whether it read a `#[cfg]`.
pub type ByName = HashMap<String, (Pick, bool)>;

/// Whether a trait is in scope at a position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InScope {
    No,
    Yes,
    /// Only where a `#[cfg]` the caller does not carry holds.
    Conditional,
}

/// What one stage of a lookup binds: its answer, and whether that binding
/// may be absent where the caller exists, under a `#[cfg]` the caller
/// does not carry (F2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pick {
    pub res: Res,
    pub conditional: bool,
}

impl Pick {
    pub const ABSENT: Pick = Pick {
        res: Res::Absent,
        conditional: false,
    };

    /// One answer from the candidates of one stage, each with whether it
    /// is conditional: the unconditional ones when there are any (a
    /// conditional one beside them would be a second definition wherever it
    /// exists, which rustc rejects: E0428, E0592, E0659, E0034), else the
    /// conditional ones, which unknown variants (`unix`, `windows`) leave
    /// ambiguous.
    pub fn of(candidates: impl IntoIterator<Item = (Res, bool)>) -> Pick {
        Pick::told(candidates).0
    }

    /// [`Pick::of`]'s answer, and whether the caller's `#[cfg]`s told the
    /// candidates apart: an unconditional one beside a conditional one.
    pub fn told(candidates: impl IntoIterator<Item = (Res, bool)>) -> (Pick, bool) {
        let mut sure = Res::Absent;
        let mut maybe = Res::Absent;
        for (res, conditional) in candidates {
            let side = if conditional { &mut maybe } else { &mut sure };
            *side = Res::combine([*side, res]);
        }
        if sure != Res::Absent {
            let pick = Pick {
                res: sure,
                conditional: false,
            };
            return (pick, maybe != Res::Absent);
        }
        let pick = Pick {
            res: maybe,
            conditional: maybe != Res::Absent,
        };
        (pick, false)
    }

    /// The answer when nothing else can take a conditional binding's place
    /// (`others` false), else unknown.
    pub fn alone(self, others: bool) -> Res {
        match self.res {
            Res::Found(_) if self.conditional && others => Res::Unknown,
            res => res,
        }
    }
}

impl<'w> Index<'w> {
    pub fn build(world: &'w World<'w>) -> Index<'w> {
        let mut files = Vec::new();
        let mut manifests = Vec::new();
        let mut declared = Vec::new();
        for (path, facts) in world.files {
            if super::manifest::is_manifest(path) {
                manifests.push((path.as_str(), Manifest::decode_package(facts)));
                declared.push(facts.as_str());
            } else if path.ends_with(".rs") {
                files.push(SourceFile::new(path, Facts::decode(facts)));
            }
        }
        // A package's dependencies are read as they are wired, one
        // package at a time: a workspace may declare many.
        let tree = modules::build(&files, &manifests, |m, each| {
            Manifest::each_dependency(declared[m], each)
        });
        let mut index = Index {
            world,
            by_path: files.iter().enumerate().map(|(i, f)| (f.path, i)).collect(),
            files,
            symbols: world
                .symbols
                .iter()
                .enumerate()
                .map(|(i, s)| (s.uid.as_str(), i))
                .collect(),
            tree,
            impls: HashMap::default(),
            impl_assocs: HashMap::default(),
            by_ref: HashMap::default(),
            ref_impls: HashMap::default(),
            blanket: HashMap::default(),
            pointer_methods: HashSet::default(),
            unplaced: HashSet::default(),
            effective: HashMap::default(),
            aliased: HashSet::default(),
            macro_crates: HashSet::default(),
            in_scope: RefCell::new(HashMap::default()),
            memo: RefCell::new(HashMap::default()),
            resolving: RefCell::new(HashSet::default()),
            file_cfg: Vec::new(),
            caller: RefCell::new((Vec::new(), 0)),
            callers: RefCell::new(HashMap::from_iter([(Vec::new(), 0)])),
            sensitive: Cell::new(false),
            told_globs: Cell::new(false),
            every_globs: RefCell::new(HashMap::default()),
            arg_types: RefCell::new(HashMap::default()),
        };
        index.file_cfg = (0..index.files.len())
            .map(|file| {
                index.instances(file, 0).iter().fold(Vec::new(), |all, m| {
                    cfg::union(&all, &index.tree.modules[*m].cfg)
                })
            })
            .collect();
        index.read_macros();
        index.attach_impls();
        index
    }

    /// Runs `f` with `caller` as the conjuncts of the code being resolved.
    pub fn as_caller<R>(&self, caller: Vec<String>, f: impl FnOnce() -> R) -> R {
        let known = if caller.is_empty() {
            Some(0)
        } else {
            self.callers.borrow().get(&caller).copied()
        };
        let id = known.unwrap_or_else(|| {
            let mut callers = self.callers.borrow_mut();
            let next = callers.len() as u32;
            callers.insert(caller.clone(), next);
            next
        });
        let saved = self.caller.replace((caller, id));
        let out = f();
        self.caller.replace(saved);
        out
    }

    /// Runs `f` as code that carries every `#[cfg]` conjunct: no binding
    /// is conditional, so two bindings of one name are ambiguous, as they
    /// are where `#[cfg]`s are not read.
    pub fn as_every_caller<R>(&self, f: impl FnOnce() -> R) -> R {
        let saved = self.caller.replace((Vec::new(), EVERY_CALLER));
        let out = f();
        self.caller.replace(saved);
        out
    }

    /// `answer`, where the caller's `#[cfg]`s told its candidates apart
    /// (`told`: an unconditional one beside a conditional one), only when
    /// `again`, asked as every caller, gives it too; else unknown (F2): a
    /// macro the rules do not read, a statement's, an attribute's or a
    /// derive's, may write a candidate between the site and them, which
    /// rustc takes, and the one the site's `#[cfg]`s pick is then no
    /// answer.
    pub fn untold(&self, answer: Res, told: bool, again: impl FnOnce() -> Res) -> Res {
        if !told
            || answer == Res::Unknown
            || self.caller_id() == EVERY_CALLER
            || self.as_every_caller(again) == answer
        {
            return answer;
        }
        Res::Unknown
    }

    /// `answer`, unless the caller's `#[cfg]`s told its candidates apart
    /// (`told`); then unknown (F2), for the reason [`Index::untold`] gives.
    /// A method probe is not asked again as every caller, which would
    /// compute its candidates twice (probe-reask-doubles-candidates): its
    /// told candidates are two at one step, where main abstains too.
    pub fn told_apart(&self, answer: Res, told: bool) -> Res {
        if told && self.caller_id() != EVERY_CALLER {
            return Res::Unknown;
        }
        answer
    }

    /// The number of the current caller, for the memos.
    pub fn caller_id(&self) -> u32 {
        self.caller.borrow().1
    }

    /// Runs `f`, telling whether what it read depends on the caller.
    pub fn tracking<R>(&self, f: impl FnOnce() -> R) -> (R, bool) {
        let outer = self.sensitive.replace(false);
        let out = f();
        let touched = self.sensitive.get();
        self.sensitive.set(outer || touched);
        (out, touched)
    }

    /// A memoized answer for any caller, else for the current one (which
    /// makes what reads it depend on the caller too).
    pub fn recall<K: Copy + Eq + std::hash::Hash, V: Copy>(
        &self,
        memo: &RefCell<HashMap<(K, u32), V>>,
        key: K,
    ) -> Option<V> {
        let memo = memo.borrow();
        if let Some(found) = memo.get(&(key, ANY_CALLER)) {
            return Some(*found);
        }
        let found = memo.get(&(key, self.caller_id())).copied();
        if found.is_some() {
            self.sensitive.set(true);
        }
        found
    }

    /// Memoizes an answer for any caller when computing it read nothing
    /// that depends on the caller.
    pub fn remember<K: Eq + std::hash::Hash, V>(
        &self,
        memo: &RefCell<HashMap<(K, u32), V>>,
        key: K,
        touched: bool,
        value: V,
    ) {
        let caller = if touched {
            self.caller_id()
        } else {
            ANY_CALLER
        };
        memo.borrow_mut().insert((key, caller), value);
    }

    /// Whether a binding carrying `cfg`, in a file or module whose `mod`
    /// declarations carry `chain`, may be absent where the current caller
    /// exists: it carries a conjunct the caller does not (F2).
    pub fn conditional(&self, cfg: &[String], chain: &[String]) -> bool {
        if cfg.is_empty() && chain.is_empty() {
            return false;
        }
        self.sensitive.set(true);
        let caller = self.caller.borrow();
        caller.1 != EVERY_CALLER
            && !(cfg::carried_by(cfg, &caller.0) && cfg::carried_by(chain, &caller.0))
    }

    /// The module instances of a scope of `file`.
    pub fn instances(&self, file: usize, scope: u32) -> &[ModId] {
        let module_scope = self.files[file]
            .module_scope
            .get(scope as usize)
            .copied()
            .unwrap_or(0);
        self.tree
            .at
            .get(&(file, module_scope))
            .map_or(&[], Vec::as_slice)
    }

    /// The one module instance of a scope, when only one crate compiles it.
    pub fn sole_instance(&self, file: usize, scope: u32) -> Option<ModId> {
        match self.instances(file, scope) {
            [module] => Some(*module),
            _ => None,
        }
    }

    /// The target of one site, or `None` when the rules cannot prove one.
    pub fn decide(&self, edge: &Edge) -> Option<usize> {
        let src = *self.symbols.get(edge.src.as_str())?;
        let file = *self.by_path.get(self.world.symbols[src].file.as_str())?;
        let context = Context::decode(&edge.context)?;
        if context.scope as usize >= self.files[file].facts.scopes.len() {
            return None;
        }
        let mut answer = None;
        for &module in self.instances(file, context.scope) {
            let pos = Pos {
                file,
                scope: context.scope,
                module,
            };
            // The site's own conjuncts and its module's: a site in code no
            // configuration compiles is never resolved.
            let chain = &self.tree.modules[module].cfg;
            let caller = if chain.is_empty() {
                context.cfg.clone()
            } else {
                cfg::union(&context.cfg, chain)
            };
            if cfg::is_false(&caller) {
                return None;
            }
            let found = self.as_caller(caller, || self.decide_in(pos, &context.form, edge.kind))?;
            if answer.is_some_and(|a| a != found) {
                return None;
            }
            answer = Some(found);
        }
        answer
    }

    fn decide_in(&self, pos: Pos, form: &Form, kind: EdgeKind) -> Option<usize> {
        let res = match (kind, form) {
            // A trait's method passed as a value takes `Self` from where it
            // is called, which the rules do not read.
            (EdgeKind::Call | EdgeKind::Reference, Form::Path(path)) => self
                .trait_call(pos, path, &Argument::Unknown)
                .unwrap_or_else(|| self.path(pos, path, Ns::Value, 0)),
            (EdgeKind::Call, Form::PathCall { path, first }) => self
                .trait_call(pos, path, first)
                .unwrap_or_else(|| self.path(pos, path, Ns::Value, 0)),
            (
                EdgeKind::Call | EdgeKind::Reference,
                Form::Qualified {
                    ty,
                    by_ref,
                    trait_path,
                    trait_args,
                    rest,
                },
            ) => self.qualified(pos, ty.as_ref(), *by_ref, (trait_path, trait_args), rest),
            (EdgeKind::Call, Form::Method { name, receiver }) => {
                self.method_call(pos, receiver, name)
            }
            (EdgeKind::Import, Form::Use { path, leaf }) => self.import(pos, path, *leaf),
            _ => return None,
        };
        let Res::Found(def) = res else { return None };
        let symbol = self.symbol(def)?;
        let target = self.world.symbols[symbol].kind;
        let callable = matches!(
            target,
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Struct | SymbolKind::Variant
        );
        let accepted = match kind {
            EdgeKind::Call => callable,
            EdgeKind::Reference => {
                callable || matches!(target, SymbolKind::Const | SymbolKind::Static)
            }
            EdgeKind::Import => true,
            EdgeKind::Route | EdgeKind::Association => false,
        };
        accepted.then_some(symbol)
    }

    /// The symbol a definition is, when it is one.
    pub fn symbol(&self, def: Def) -> Option<usize> {
        let (file, key) = match def {
            Def::Item(file, item) => (file, &self.files[file].facts.items.get(item as usize)?.uid),
            Def::Variant(file, variant) => (
                file,
                &self.files[file].facts.variants.get(variant as usize)?.uid,
            ),
            Def::Assoc(file, assoc) => (
                file,
                &self.files[file].facts.assocs.get(assoc as usize)?.uid,
            ),
            Def::Module(module) => {
                let module = &self.tree.modules[module];
                let file = &self.files[module.file];
                if module.scope == 0 {
                    let uid = uid(file.path, file.path, SymbolKind::File, 1);
                    return self.symbols.get(uid.as_str()).copied();
                }
                let item = *file.module_item.get(&module.scope)?;
                (module.file, &file.facts.items.get(item as usize)?.uid)
            }
        };
        if key.is_empty() {
            return None;
        }
        let uid = format!("{}{key}", uid_path(self.files[file].path));
        self.symbols.get(uid.as_str()).copied()
    }
}
