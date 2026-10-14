// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Types and their associated items: impl blocks attached to the type they
//! implement, `T::f`, `<T as Tr>::f`, and method calls on a receiver whose
//! type the code states.
//!
//! A method call follows Rust's method probe (the reference's "Method call
//! expressions"): the receiver's type `R`, then `&R`, then `&mut R`, then
//! the same after each dereference; at each step the inherent methods whose
//! `self` fits and that the caller can see (privacy, D19) come first, then
//! the methods of the traits in scope. The
//! rules know `R` only up to its references (`x: &T` is read as a `T`), so
//! a call resolves only when every receiver among `T`, `&T` and `&mut T`
//! that finds a method finds the same one; a method the rows do not hold
//! (a derive, a blanket impl, an impl for `&T`) at a step before it makes
//! the call unresolved.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use super::args::{Args, Effective, Fit};
use super::cfg;
use super::modules::ModId;
use super::resolve::{DEPTH, Def, Impl, InScope, Index, NamedImpls, Ns, Pick, Pos, Res, TraitOf};
use super::types::{POINTERS, is_param};
use super::{
    Argument, Assoc, AssocKind, ItemKind, Layer, Receiver, ScopeKind, SelfKind, TypeRef, Vis,
};

/// Methods a `Box`, `Rc` or `Arc` has itself, inherent or through a std
/// trait it implements: called on the pointer, they are not the pointee's.
const POINTER_METHODS: [&str; 39] = [
    "as_mut",
    "as_ref",
    "borrow",
    "borrow_mut",
    "call",
    "call_mut",
    "call_once",
    "clamp",
    "clone",
    "clone_from",
    "cmp",
    "deref",
    "deref_mut",
    "drop",
    "eq",
    "flush",
    "fmt",
    "from",
    "ge",
    "gt",
    "hash",
    "into",
    "into_iter",
    "le",
    "lt",
    "max",
    "min",
    "ne",
    "next",
    "partial_cmp",
    "poll",
    "read",
    "seek",
    "source",
    "to_owned",
    "to_string",
    "try_from",
    "try_into",
    "write",
];

/// Methods std gives every type (or every type of a wide class) through a
/// blanket impl no symbol carries (`From<T> for T`, `Into`, `TryFrom`,
/// `TryInto`, `Borrow`, `ToOwned`, `ToString`, `Any`), with how each takes
/// `self`. A visible impl of one of these names competes with the blanket
/// one (`T::from(t)` with `t: T` is the reflexive `From`).
const STD_BLANKET: [(&str, SelfKind); 10] = [
    ("borrow", SelfKind::Ref),
    ("borrow_mut", SelfKind::RefMut),
    ("clone_into", SelfKind::Ref),
    ("from", SelfKind::None),
    ("into", SelfKind::Value),
    ("to_owned", SelfKind::Ref),
    ("to_string", SelfKind::Ref),
    ("try_from", SelfKind::None),
    ("try_into", SelfKind::Value),
    ("type_id", SelfKind::Ref),
];

/// The associated functions a `#[derive]` of a std trait adds, which no
/// symbol carries, with how each takes `self`.
const DERIVED: [(&str, &[(&str, SelfKind)]); 7] = [
    (
        "Clone",
        &[("clone", SelfKind::Ref), ("clone_from", SelfKind::RefMut)],
    ),
    ("Debug", &[("fmt", SelfKind::Ref)]),
    ("Default", &[("default", SelfKind::None)]),
    ("PartialEq", &[("eq", SelfKind::Ref), ("ne", SelfKind::Ref)]),
    (
        "PartialOrd",
        &[
            ("partial_cmp", SelfKind::Ref),
            ("lt", SelfKind::Ref),
            ("le", SelfKind::Ref),
            ("gt", SelfKind::Ref),
            ("ge", SelfKind::Ref),
        ],
    ),
    (
        "Ord",
        &[
            ("cmp", SelfKind::Ref),
            ("max", SelfKind::Value),
            ("min", SelfKind::Value),
            ("clamp", SelfKind::Value),
        ],
    ),
    (
        "Hash",
        &[("hash", SelfKind::Ref), ("hash_slice", SelfKind::None)],
    ),
];

/// The traits of the std prelude of every edition (rustc 1.99; `TryFrom`,
/// `TryInto` and `FromIterator` join it in 2021, `Future` and
/// `IntoFuture` in 2024): an impl of one gives its type methods every
/// caller reaches.
const PRELUDE_TRAITS: [&str; 29] = [
    "AsMut",
    "AsRef",
    "AsyncFn",
    "AsyncFnMut",
    "AsyncFnOnce",
    "Clone",
    "Copy",
    "Default",
    "DoubleEndedIterator",
    "Drop",
    "Eq",
    "ExactSizeIterator",
    "Extend",
    "Fn",
    "FnMut",
    "FnOnce",
    "From",
    "Into",
    "IntoIterator",
    "Iterator",
    "Ord",
    "PartialEq",
    "PartialOrd",
    "Send",
    "Sized",
    "Sync",
    "ToOwned",
    "ToString",
    "Unpin",
];

/// The traits of [`PRELUDE_TRAITS`] that take a type parameter, whose
/// argument a call infers from its own: two impls of one for a type
/// (`Extend<T>`, `Extend<&'a T>`) may both apply to a value.
const GENERIC_PRELUDE_TRAITS: [&str; 13] = [
    "AsMut",
    "AsRef",
    "AsyncFn",
    "AsyncFnMut",
    "AsyncFnOnce",
    "Extend",
    "Fn",
    "FnMut",
    "FnOnce",
    "From",
    "Into",
    "PartialEq",
    "PartialOrd",
];

/// One step of the method probe, as the receiver type it tries: the `self`
/// a method of `T` takes to fit it (`None` for `&&T` and `&mut &T`, which
/// no method of `T` fits), and the `self` of a blanket impl's method that
/// fits it besides `self` by value (which fits every step, its `Self`
/// being the step's type).
#[derive(Clone, Copy)]
struct Step {
    own: Option<SelfKind>,
    blanket: SelfKind,
}

const fn step(own: Option<SelfKind>, blanket: SelfKind) -> Step {
    Step { own, blanket }
}

/// The probe for a receiver of type `T`: `T`, `&T`, `&mut T`.
const FROM_VALUE: &[Step] = &[
    step(Some(SelfKind::Value), SelfKind::Value),
    step(Some(SelfKind::Ref), SelfKind::Ref),
    step(Some(SelfKind::RefMut), SelfKind::RefMut),
];

/// For `&T`: `&T`, `&&T`, `&mut &T`, then `T`, `&T`, `&mut T`.
const FROM_REF: &[Step] = &[
    step(Some(SelfKind::Ref), SelfKind::Ref),
    step(None, SelfKind::Ref),
    step(None, SelfKind::RefMut),
    step(Some(SelfKind::Value), SelfKind::Value),
    step(Some(SelfKind::RefMut), SelfKind::RefMut),
];

/// For `&mut T`: `&mut T`, `&&mut T`, `&mut &mut T`, then `T`, `&T`,
/// `&mut T`.
const FROM_MUT: &[Step] = &[
    step(Some(SelfKind::RefMut), SelfKind::RefMut),
    step(None, SelfKind::Ref),
    step(None, SelfKind::RefMut),
    step(Some(SelfKind::Value), SelfKind::Value),
    step(Some(SelfKind::Ref), SelfKind::Ref),
];

/// Methods std implements for every reference `&T` or `&mut T` whose `T`
/// has them (`Clone` for all `&T`; `Debug`, `Display`, `PartialEq`, `Ord`,
/// `Hash`, `AsRef`, `Borrow`, `Deref` and the like when `T` does): they fit
/// the `&&T` and `&&mut T` steps, before `T`'s methods taking `self`.
const ON_REFERENCES: [&str; 18] = [
    "as_mut",
    "as_ref",
    "borrow",
    "clone",
    "cmp",
    "deref",
    "eq",
    "fmt",
    "ge",
    "gt",
    "hash",
    "le",
    "lt",
    "max",
    "min",
    "ne",
    "partial_cmp",
    "to_string",
];

/// What a method is called on: a type, or a trait (`dyn Tr`, `impl Tr`,
/// `self` in a trait's body); `pointer` when reached through `Box`, `Rc` or
/// `Arc`.
enum Recv {
    Type {
        ty: (usize, u32),
        pointer: bool,
        args: Args,
    },
    Trait {
        tr: (usize, u32),
        pointer: bool,
    },
}

/// What the bindings of one scope give a trait (`Index::binds_trait`).
#[derive(Default)]
pub struct TraitBound {
    /// Each name it is bound under (`None` for `as _`), with whether that
    /// binding may be absent where the caller exists.
    pub names: Vec<(Option<String>, bool)>,
    /// A binding the rules cannot read may give it.
    pub unknown: bool,
    /// A binding names something outside the workspace, which may be a
    /// trait whose items the rules do not know.
    pub external: bool,
}

/// The associated items of one name a type has, each with whether the
/// caller may exist without it (`#[cfg]`, F2).
#[derive(Default)]
struct Candidates {
    /// From its inherent impls: (file, assoc, conditional).
    inherent: Vec<(usize, u32, bool)>,
    /// From its trait impls and the defaults of their traits, the traits of
    /// the workspace only when they are in scope: (file, assoc, a trait
    /// outside the workspace, conditional).
    traits: Vec<(usize, u32, bool, bool)>,
    /// What a derive or a blanket impl adds under the name, which no row
    /// holds: how it takes `self`, and whether it is a blanket impl (which
    /// fits `&T` and `&mut T` as well as `T`).
    hidden: Vec<(SelfKind, bool)>,
    /// An `impl … for &T` provides the name: its methods fit receivers the
    /// probe below does not place.
    by_ref: bool,
    /// An impl of the type, or a trait whose defaults it takes, invokes a
    /// macro among its items, which may define the name.
    macro_made: bool,
    /// A trait impl whose only items of the name are conditional, over a
    /// default of its trait: which runs depends on the configuration.
    overridden: bool,
    /// Something the rules cannot place may provide the name: an impl whose
    /// type they cannot resolve (D18), an inherent item whose visibility
    /// they cannot follow (D19), impls for different generic arguments
    /// when the value's are unknown (D43), an impl of a generic trait that
    /// may apply beside another provider.
    unsure: bool,
    /// A pick among them where the caller's `#[cfg]`s told some apart: an
    /// unconditional one beside a conditional one ([`Index::told_apart`]).
    told: Cell<bool>,
}

impl Candidates {
    /// Every candidate of the name, rows or not.
    fn total(&self) -> usize {
        self.inherent.len() + self.traits.len() + self.hidden.len() + usize::from(self.by_ref)
    }

    fn inherent_pick(&self, keep: impl Fn(usize, u32) -> bool) -> Pick {
        self.note(Pick::told(
            self.inherent
                .iter()
                .filter(|(f, a, _)| keep(*f, *a))
                .map(|(f, a, c)| (Res::Found(Def::Assoc(*f, *a)), *c)),
        ))
    }

    fn trait_pick(&self, keep: impl Fn(usize, u32) -> bool) -> Pick {
        self.note(Pick::told(
            self.traits
                .iter()
                .filter(|(f, a, _, _)| keep(*f, *a))
                .map(|(f, a, _, c)| (Res::Found(Def::Assoc(*f, *a)), *c)),
        ))
    }

    fn note(&self, (pick, told): (Pick, bool)) -> Pick {
        self.told.set(self.told.get() || told);
        pick
    }
}

impl Index<'_> {
    /// Attaches every impl block to the type it implements, and notes the
    /// names of the methods of blanket impls, of impls for `&T`, of impls
    /// on pointers, and of impls the rules cannot place on one type.
    pub fn attach_impls(&mut self) {
        let mut impls: HashMap<(usize, u32), Vec<Impl>> = HashMap::new();
        let mut by_ref: HashMap<(usize, u32), HashSet<String>> = HashMap::new();
        let mut ref_impls: HashMap<((usize, u32), bool), Vec<Impl>> = HashMap::new();
        let mut blanket: HashMap<String, Vec<SelfKind>> = HashMap::new();
        let mut pointer_methods = HashSet::new();
        let mut unplaced = HashSet::new();
        let mut effective = HashMap::new();
        let mut aliased = HashSet::new();
        for (file, source) in self.files.iter().enumerate() {
            for (index, scope) in source.facts.scopes.iter().enumerate() {
                let ScopeKind::Impl {
                    self_type,
                    by_ref: is_by_ref,
                    mutable,
                    blanket: is_blanket,
                    trait_path,
                    ..
                } = &scope.kind
                else {
                    continue;
                };
                if cfg::is_false(&scope.cfg) {
                    continue;
                }
                let index = index as u32;
                let parent = scope.parent.unwrap_or(0);
                let mut methods: Vec<(String, SelfKind)> = source
                    .facts
                    .assocs
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| a.owner == index && !cfg::is_false(&a.cfg))
                    .map(|(i, a)| (a.name.clone(), self.self_kind(file, i as u32)))
                    .collect();
                // Its paths are read as the impl's own code reads them.
                let as_impl =
                    |module: ModId| cfg::union(&scope.cfg, &self.tree.modules[module].cfg);
                let traits: Vec<TraitOf> = self
                    .instances(file, parent)
                    .iter()
                    .map(|module| {
                        let pos = Pos {
                            file,
                            scope: parent,
                            module: *module,
                        };
                        self.as_caller(as_impl(*module), || self.trait_of(pos, trait_path))
                    })
                    .collect();
                if let [TraitOf::Workspace(f, i)] = traits.as_slice() {
                    methods.extend(
                        self.defaults((*f, *i))
                            .filter(|(name, _)| !methods.iter().any(|(own, _)| own == name))
                            .collect::<Vec<_>>(),
                    );
                }
                if *is_blanket {
                    for (name, kind) in methods {
                        blanket.entry(name).or_default().push(kind);
                    }
                    continue;
                }
                let Some(layer) = self_type.as_ref().filter(|l| !l.dynamic) else {
                    continue;
                };
                if layer
                    .path
                    .last()
                    .is_some_and(|last| POINTERS.contains(&last.as_str()))
                {
                    pointer_methods.extend(methods.iter().map(|(name, _)| name.clone()));
                }
                // What the self type's path names (an alias, D43), and the
                // type that is.
                let named: Vec<(Res, Res)> = self
                    .instances(file, parent)
                    .iter()
                    .map(|module| {
                        let pos = Pos {
                            file,
                            scope: parent,
                            module: *module,
                        };
                        self.as_caller(as_impl(*module), || {
                            let named = self.path(pos, &layer.path, Ns::Type, 0);
                            (named, self.type_def(named, 0))
                        })
                    })
                    .collect();
                let (Some(&(name, ty)), Some(&trait_of)) = (named.first(), traits.first()) else {
                    continue;
                };
                let agreed =
                    named.iter().all(|n| *n == (name, ty)) && traits.iter().all(|t| *t == trait_of);
                // Written through an alias in some crate (D43).
                let alias = named.iter().any(|(n, _)| {
                    matches!(*n, Res::Found(Def::Item(af, ai))
                        if matches!(self.files[af].item_kind(ai), Some(ItemKind::Alias { .. })))
                });
                let (tf, ti) = match ty {
                    Res::Found(Def::Item(tf, ti)) if agreed => (tf, ti),
                    // A type outside the workspace in every crate. Through an
                    // alias it may be a pointer the probe looks through
                    // (`type B = Box<N>`), whose methods come first: its
                    // items may be any type's, as below.
                    Res::External | Res::Absent if agreed && !alias => continue,
                    // A self type the rules cannot resolve (an unreadable
                    // glob may bind it), or crates compiling the file that
                    // read the impl as two types or traits: its items may
                    // be any type's (D18).
                    _ => {
                        unplaced.extend(methods.into_iter().map(|(name, _)| name));
                        continue;
                    }
                };
                if !self.is_type((tf, ti)) {
                    continue;
                }
                let attached = Impl {
                    file,
                    scope: index,
                    trait_of,
                };
                match name {
                    Res::Found(Def::Item(af, ai)) if (af, ai) != (tf, ti) => {
                        match self.alias_impl_args((file, parent), &layer.args, (af, ai), (tf, ti))
                        {
                            Some(args) => {
                                effective.insert((file, index), args);
                            }
                            // An alias the rules cannot expand exactly: every
                            // probe on its type is unresolved.
                            None => {
                                aliased.insert((tf, ti));
                                continue;
                            }
                        }
                    }
                    _ => {
                        let args = self.effective_args(file, parent, &layer.args, (tf, ti));
                        if args != layer.args {
                            effective.insert(
                                (file, index),
                                Effective {
                                    args,
                                    at: Vec::new(),
                                },
                            );
                        }
                    }
                }
                if *is_by_ref {
                    by_ref
                        .entry((tf, ti))
                        .or_default()
                        .extend(methods.into_iter().map(|(name, _)| name));
                    ref_impls
                        .entry(((tf, ti), *mutable))
                        .or_default()
                        .push(attached);
                    continue;
                }
                impls.entry((tf, ti)).or_default().push(attached);
            }
        }
        let mut impl_assocs: HashMap<(usize, u32), NamedImpls> = HashMap::new();
        for (ty, attached) in &impls {
            for (index, imp) in attached.iter().enumerate() {
                let Some(by_name) = self.files[imp.file].assocs.get(&imp.scope) else {
                    continue;
                };
                let named = impl_assocs.entry(*ty).or_default();
                for (name, assocs) in by_name {
                    named
                        .entry(name.clone())
                        .or_default()
                        .push((index, assocs.clone()));
                }
            }
        }
        self.impls = impls;
        self.impl_assocs = impl_assocs;
        self.by_ref = by_ref;
        self.ref_impls = ref_impls;
        self.blanket = blanket;
        self.pointer_methods = pointer_methods;
        self.unplaced = unplaced;
        self.effective = effective;
        self.aliased = aliased;
    }

    fn trait_of(&self, pos: Pos, trait_path: &[String]) -> TraitOf {
        if trait_path.is_empty() {
            return TraitOf::Inherent;
        }
        let res = self.path(pos, trait_path, Ns::Type, 0);
        if let Res::Found(Def::Item(file, item)) = res
            && matches!(
                self.files[file].item_kind(item),
                Some(ItemKind::Trait { .. })
            )
        {
            return TraitOf::Workspace(file, item);
        }
        let path = match trait_path {
            [first, rest @ ..] if first.is_empty() => rest,
            path => path,
        };
        let prelude = match path {
            [name] => {
                self.scoped(pos, name) == Res::Absent && PRELUDE_TRAITS.contains(&name.as_str())
            }
            [root, .., name] => {
                matches!(root.as_str(), "std" | "core")
                    && res == Res::External
                    && PRELUDE_TRAITS.contains(&name.as_str())
            }
            [] => false,
        };
        if prelude {
            TraitOf::Prelude
        } else {
            TraitOf::Other
        }
    }

    /// Whether a trait outside the workspace and the std prelude may be in
    /// scope at `pos`: an impl around the site implements one (rustc puts
    /// it in scope there), or a binding of an enclosing block or of the
    /// module names something outside the workspace or cannot be read (a
    /// macro's mark included). Else none is.
    pub fn foreign_trait_may_be_in_scope(&self, pos: Pos) -> bool {
        let scopes = &self.files[pos.file].facts.scopes;
        let mut current = Some(pos.scope);
        while let Some(scope) = current {
            let Some(entry) = scopes.get(scope as usize) else {
                return true;
            };
            if let ScopeKind::Impl { trait_path, .. } = &entry.kind
                && !trait_path.is_empty()
            {
                let outer = Pos {
                    scope: entry.parent.unwrap_or(0),
                    ..pos
                };
                let workspace = matches!(
                    self.path(outer, trait_path, Ns::Type, 0),
                    Res::Found(Def::Item(file, item))
                        if matches!(self.files[file].item_kind(item), Some(ItemKind::Trait { .. }))
                );
                if !workspace {
                    return true;
                }
            }
            if matches!(
                entry.kind,
                ScopeKind::File | ScopeKind::Module { .. } | ScopeKind::Block
            ) {
                let here = Pos { scope, ..pos };
                let bound = self.binds_trait(here, None, pos.module, 0, &mut Vec::new());
                if bound.unknown || bound.external {
                    return true;
                }
                if !matches!(entry.kind, ScopeKind::Block) {
                    return false;
                }
            }
            current = entry.parent;
        }
        false
    }

    /// A trait's methods with a default body, and how each takes `self`.
    fn defaults(&self, tr: (usize, u32)) -> impl Iterator<Item = (String, SelfKind)> + '_ {
        let source = &self.files[tr.0];
        let scope = source.trait_scope.get(&tr.1).copied();
        source
            .facts
            .assocs
            .iter()
            .filter(move |a| {
                Some(a.owner) == scope
                    && matches!(a.kind, AssocKind::Fn { body: true, .. })
                    && !cfg::is_false(&a.cfg)
            })
            .map(|a| {
                let kind = match a.kind {
                    AssocKind::Fn { receiver, .. } => receiver,
                    AssocKind::Const => SelfKind::None,
                };
                (a.name.clone(), kind)
            })
    }

    /// Whether `imp` is an impl of a trait with a type or const
    /// parameter, of the workspace or of std's prelude. A trait outside
    /// both gives candidates that stand only when nothing else provides
    /// the name already.
    fn generic_trait(&self, imp: &Impl) -> bool {
        match imp.trait_of {
            TraitOf::Workspace(file, item) => matches!(
                self.files[file].item_kind(item),
                Some(ItemKind::Trait { generic: true })
            ),
            TraitOf::Prelude => match self.files[imp.file]
                .facts
                .scopes
                .get(imp.scope as usize)
                .map(|scope| &scope.kind)
            {
                Some(ScopeKind::Impl { trait_path, .. }) => trait_path
                    .last()
                    .is_some_and(|name| GENERIC_PRELUDE_TRAITS.contains(&name.as_str())),
                _ => true,
            },
            TraitOf::Inherent | TraitOf::Other => false,
        }
    }

    fn is_type(&self, ty: (usize, u32)) -> bool {
        matches!(
            self.files[ty.0].item_kind(ty.1),
            Some(ItemKind::Struct { .. } | ItemKind::Enum { .. } | ItemKind::Union { .. })
        )
    }

    fn is_trait(&self, tr: (usize, u32)) -> bool {
        matches!(
            self.files[tr.0].item_kind(tr.1),
            Some(ItemKind::Trait { .. })
        )
    }

    fn assoc(&self, file: usize, assoc: u32) -> &Assoc {
        &self.files[file].facts.assocs[assoc as usize]
    }

    fn self_kind(&self, file: usize, assoc: u32) -> SelfKind {
        match self.assoc(file, assoc).kind {
            AssocKind::Fn { receiver, .. } => receiver,
            AssocKind::Const => SelfKind::None,
        }
    }

    /// Whether an associated item can be what `T::name` (`method` false) or
    /// `x.name()` (`method` true) calls: a method needs a `self`.
    fn fits(&self, file: usize, assoc: u32, method: bool) -> bool {
        match self.assoc(file, assoc).kind {
            AssocKind::Fn { receiver, .. } => !method || receiver != SelfKind::None,
            AssocKind::Const => !method,
        }
    }

    /// The associated items named `name` of `ty`, as the code at `pos`
    /// reaches them.
    fn candidates(
        &self,
        pos: Pos,
        ty: (usize, u32),
        name: &str,
        method: bool,
        args: &Args,
    ) -> Candidates {
        let mut found = Candidates {
            unsure: self.unplaced.contains(name),
            ..Candidates::default()
        };
        // With the value's arguments unknown, the arguments of each impl
        // providing the name (`None` for an impl for every argument).
        let mut providers: Vec<Option<&[String]>> = Vec::new();
        let mut foreign: Option<bool> = None;
        // How many impls provide the name, and whether one that may apply
        // is of a generic trait.
        let (mut providing, mut generic_may) = (0, false);
        let mut last_fit = None;
        let mut named = self
            .impl_assocs
            .get(&ty)
            .and_then(|by_name| by_name.get(name))
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .peekable();
        for (index, imp) in self.impls.get(&ty).into_iter().flatten().enumerate() {
            // Its own items of the name.
            let own = named
                .next_if(|(at, _)| *at == index)
                .map_or(&[][..], |(_, own)| own.as_slice());
            // An impl that does not provide the name, and whose body and
            // trait no macro may fill, adds nothing: its fit, which reads
            // the scopes of the impl and of the value, is not asked
            // (candidates-fit-every-impl).
            let provides = !own.is_empty() || self.trait_provides(imp, name);
            let trait_made = matches!(imp.trait_of, TraitOf::Workspace(tf, ti)
                if self.files[tf]
                    .trait_scope
                    .get(&ti)
                    .is_some_and(|scope| self.macro_made(tf, *scope)));
            if !provides && !trait_made && !self.macro_made(imp.file, imp.scope) {
                continue;
            }
            // An impl that may apply to the value (D43) gives candidates
            // that stand only when nothing else provides the name: a call of
            // it compiles only if one does.
            let may = match self.fit_once(imp, args, &mut last_fit) {
                Some(Fit::No) => continue,
                Some(Fit::Maybe) => true,
                Some(Fit::Yes) | None => false,
            };
            let source = &self.files[imp.file];
            if provides {
                providing += 1;
                generic_may |= may && self.generic_trait(imp);
                if *args == Args::Unknown {
                    let key = (!self.for_every_argument(imp)).then(|| self.impl_args(imp));
                    if !providers.contains(&key) {
                        providers.push(key);
                    }
                }
            }
            found.macro_made |= self.macro_made(imp.file, imp.scope);
            let fitting = own
                .iter()
                .copied()
                .filter(|a| self.fits(imp.file, *a, method))
                .map(|a| (a, may || self.assoc_conditional(imp.file, a)));
            match imp.trait_of {
                // An inherent item the caller cannot see is skipped, as
                // rustc's probe does (D19).
                TraitOf::Inherent => {
                    for (a, c) in fitting {
                        match self.seen_from(pos, imp.file, a) {
                            Some(true) => found.inherent.push((imp.file, a, c)),
                            Some(false) => {}
                            None => found.unsure = true,
                        }
                    }
                }
                TraitOf::Prelude => found
                    .traits
                    .extend(fitting.map(|(a, c)| (imp.file, a, true, c))),
                // Another trait outside the workspace is a candidate only
                // where it may be in scope, and then only when nothing else
                // provides the name: the rules cannot tell which trait a
                // binding outside the workspace brings.
                TraitOf::Other => {
                    if *foreign.get_or_insert_with(|| self.foreign_trait_may_be_in_scope(pos)) {
                        found
                            .traits
                            .extend(fitting.map(|(a, _)| (imp.file, a, true, true)));
                    }
                }
                TraitOf::Workspace(tf, ti) => {
                    let scope = self.trait_in_scope(pos, (tf, ti));
                    if scope == InScope::No {
                        continue;
                    }
                    let in_scope = scope == InScope::Conditional;
                    let defaults = || {
                        self.trait_assocs((tf, ti), name).into_iter().filter(|a| {
                            matches!(self.assoc(tf, *a).kind, AssocKind::Fn { body: true, .. })
                                && self.fits(tf, *a, method)
                        })
                    };
                    if own.is_empty() {
                        found.macro_made |= self.files[tf]
                            .trait_scope
                            .get(&ti)
                            .is_some_and(|scope| self.macro_made(tf, *scope));
                        let imp_cfg = &source.facts.scopes[imp.scope as usize].cfg;
                        let impl_conditional = self.conditional(imp_cfg, &self.file_cfg[imp.file]);
                        found.traits.extend(defaults().map(|a| {
                            let conditional = may
                                || in_scope
                                || impl_conditional
                                || self.assoc_conditional(tf, a);
                            (tf, a, false, conditional)
                        }));
                    } else {
                        let before = found.traits.len();
                        found
                            .traits
                            .extend(fitting.map(|(a, c)| (imp.file, a, false, in_scope || c)));
                        let added = &found.traits[before..];
                        found.overridden |= !added.is_empty()
                            && added
                                .iter()
                                .all(|(_, a, _, _)| self.assoc_conditional(imp.file, *a))
                            && defaults().next().is_some();
                    }
                }
            }
        }
        // Impls for two lists of arguments (or for one and for every
        // argument) each provide the name for some value: which one this
        // value reaches is unknown (D43).
        found.unsure |= providers.len() > 1;
        // Not so for impls of one generic trait: `Extend<T>` and, bounded,
        // `Extend<&'a T>` may both apply, and the trait's arguments, which
        // the call's arguments infer (D20), pick one. Beside another
        // provider, one that may apply is no fallback (generic-trait-impl-
        // bound).
        found.unsure |= generic_may && providing > 1;
        let derives = match self.files[ty.0].item_kind(ty.1) {
            Some(ItemKind::Struct { derives, .. } | ItemKind::Enum { derives, .. }) => {
                derives.as_slice()
            }
            _ => &[],
        };
        for (tr, methods) in DERIVED {
            if derives.iter().any(|d| d == tr) {
                found.hidden.extend(
                    methods
                        .iter()
                        .filter(|(m, _)| *m == name)
                        .map(|(_, kind)| (*kind, false)),
                );
            }
        }
        found.hidden.extend(
            STD_BLANKET
                .iter()
                .filter(|(m, _)| *m == name)
                .map(|(_, kind)| (*kind, true)),
        );
        if let Some(kinds) = self.blanket.get(name) {
            found.hidden.extend(kinds.iter().map(|kind| (*kind, true)));
        }
        found.by_ref = self
            .by_ref
            .get(&ty)
            .is_some_and(|names| names.contains(name));
        found
    }

    /// Whether the code at `pos` sees an inherent impl's item: from the
    /// impl's module and its descendants when private, as its visibility
    /// says otherwise (rustc's privacy, D19); `None` when the rules cannot
    /// follow a `pub(in path)`. A file several crates compile is a module
    /// of each, and only the caller's crate can see a restricted item.
    fn seen_from(&self, pos: Pos, file: usize, assoc: u32) -> Option<bool> {
        let declared = self.assoc(file, assoc);
        if declared.vis == Vis::Public {
            return Some(true);
        }
        let mut answer = Some(false);
        for &owner in self.instances(file, declared.owner) {
            match self.visible(&declared.vis, owner, pos.module) {
                Some(true) => return Some(true),
                Some(false) => {}
                None => answer = None,
            }
        }
        answer
    }

    /// Whether an associated item may be absent where the caller exists.
    fn assoc_conditional(&self, file: usize, assoc: u32) -> bool {
        self.conditional(&self.assoc(file, assoc).cfg, &self.file_cfg[file])
    }

    /// Whether an impl or trait body invokes a macro among its items
    /// (extraction marks it with a glob, which no `use` puts there).
    fn macro_made(&self, file: usize, scope: u32) -> bool {
        self.files[file].globs.contains_key(&scope)
    }

    /// The associated items of a trait named `name`.
    fn trait_assocs(&self, tr: (usize, u32), name: &str) -> Vec<u32> {
        let source = &self.files[tr.0];
        source
            .trait_scope
            .get(&tr.1)
            .map(|scope| source.assocs_named(*scope, name).to_vec())
            .unwrap_or_default()
    }

    /// Whether the workspace trait `tr` is in scope at `pos`, so that its
    /// methods are candidates there (Rust's rule): the trait whose impl or
    /// body holds the site, or a trait a binding of an enclosing block or of
    /// the module gives under any name (D21): its item, a `use` naming it
    /// (renamed, `as _` or re-exported included), a glob bringing it.
    /// Conditional when only a binding the caller may exist without does,
    /// or when a binding the rules cannot read may.
    pub fn trait_in_scope(&self, pos: Pos, tr: (usize, u32)) -> InScope {
        let key = (pos.module, pos.file, pos.scope, tr.0, tr.1);
        if let Some(known) = self.recall(&self.in_scope, key) {
            return known;
        }
        let (found, touched) = self.tracking(|| self.find_trait_in_scope(pos, tr));
        self.remember(&self.in_scope, key, touched, found);
        found
    }

    fn find_trait_in_scope(&self, pos: Pos, tr: (usize, u32)) -> InScope {
        let wanted = Res::Found(Def::Item(tr.0, tr.1));
        let source = &self.files[pos.file];
        let mut best = InScope::No;
        let mut current = Some(pos.scope);
        while let Some(scope) = current {
            let Some(entry) = source.facts.scopes.get(scope as usize) else {
                return best;
            };
            match &entry.kind {
                ScopeKind::Impl { trait_path, .. } if !trait_path.is_empty() => {
                    let outer = Pos {
                        scope: entry.parent.unwrap_or(0),
                        ..pos
                    };
                    if self.path(outer, trait_path, Ns::Type, 0) == wanted {
                        return InScope::Yes;
                    }
                }
                ScopeKind::Impl { .. } => {}
                ScopeKind::Trait { item } => {
                    if (pos.file, *item) == tr {
                        return InScope::Yes;
                    }
                }
                ScopeKind::File | ScopeKind::Module { .. } | ScopeKind::Block => {
                    let here = Pos { scope, ..pos };
                    let bound = self.binds_trait(here, Some(tr), pos.module, 0, &mut Vec::new());
                    if bound.names.iter().any(|(_, conditional)| !conditional) {
                        return InScope::Yes;
                    }
                    if bound.unknown || !bound.names.is_empty() {
                        best = InScope::Conditional;
                    }
                    if !matches!(entry.kind, ScopeKind::Block) {
                        return best;
                    }
                }
            }
            current = entry.parent;
        }
        best
    }

    /// The names under which the bindings of the scope at `pos` give the
    /// trait `tr` to code of `importer` (D21): the trait as an item of the
    /// scope, every `use` leaf whose path resolves to it (under its local
    /// name, or none for `as _`), and what its globs bring under a name the
    /// scope does not bind itself (an item or `use` of that name shadows a
    /// glob's). A binding of another module counts only when `importer`
    /// sees it, as for any glob. With no trait, only whether a binding the
    /// rules cannot read may bring one.
    pub fn binds_trait(
        &self,
        pos: Pos,
        tr: Option<(usize, u32)>,
        importer: ModId,
        depth: u32,
        seen: &mut Vec<ModId>,
    ) -> TraitBound {
        let mut bound = TraitBound::default();
        if depth > DEPTH {
            bound.unknown = true;
            return bound;
        }
        let wanted = tr.map(|(file, item)| Res::Found(Def::Item(file, item)));
        let source = &self.files[pos.file];
        let chain = &self.tree.modules[pos.module].cfg;
        let shown = |vis: &Vis| {
            if importer == pos.module {
                Some(true)
            } else {
                self.visible(vis, pos.module, importer)
            }
        };
        if let Some(tr) = tr
            && tr.0 == pos.file
            && let Some(item) = source.facts.items.get(tr.1 as usize)
            && item.scope == pos.scope
            && !cfg::is_false(&item.cfg)
        {
            match shown(&item.vis) {
                Some(true) => bound
                    .names
                    .push((Some(item.name.clone()), self.conditional(&item.cfg, chain))),
                Some(false) => {}
                None => bound.unknown = true,
            }
        }
        for &leaf in source.named_uses.get(&pos.scope).into_iter().flatten() {
            let declared = &source.facts.uses[leaf as usize];
            match shown(&declared.vis) {
                Some(true) => {}
                Some(false) => continue,
                None => {
                    bound.unknown = true;
                    continue;
                }
            }
            match self.use_binding(pos.file, leaf, pos.module, Ns::Type, depth + 1) {
                res if Some(res) == wanted => bound.names.push((
                    declared.local.clone(),
                    self.conditional(&declared.cfg, chain),
                )),
                Res::Unknown => bound.unknown = true,
                Res::External => bound.external = true,
                _ => {}
            }
        }
        for &leaf in source.globs.get(&pos.scope).into_iter().flatten() {
            let declared = &source.facts.uses[leaf as usize];
            match shown(&declared.vis) {
                Some(true) => {}
                Some(false) => continue,
                None => {
                    bound.unknown = true;
                    continue;
                }
            }
            let at = Pos {
                scope: declared.scope,
                ..pos
            };
            let module = match self.use_path(at, &declared.path, Ns::Type, depth + 1) {
                Res::Found(Def::Module(module)) => module,
                // An enum's variants are no trait. A dependency's module
                // holds no trait of the workspace, but may hold others.
                Res::Found(_) | Res::Absent => continue,
                Res::External => {
                    bound.external = true;
                    continue;
                }
                // A glob the rules cannot read, a macro's or an unreadable
                // region's mark included, may bring it.
                Res::Unknown => {
                    bound.unknown = true;
                    continue;
                }
            };
            if seen.contains(&module) {
                continue;
            }
            seen.push(module);
            let target = &self.tree.modules[module];
            let inner = self.binds_trait(
                Pos {
                    file: target.file,
                    scope: target.scope,
                    module,
                },
                tr,
                importer,
                depth + 1,
                seen,
            );
            bound.unknown |= inner.unknown;
            bound.external |= inner.external;
            let glob = self.conditional(&declared.cfg, chain);
            for (name, conditional) in inner.names {
                let shadowed = name.as_ref().is_some_and(|name| {
                    self.explicit(
                        pos.file,
                        pos.scope,
                        pos.module,
                        name,
                        Ns::Type,
                        depth + 1,
                        None,
                    )
                    .res != Res::Absent
                });
                if !shadowed {
                    bound.names.push((name, conditional || glob));
                }
            }
        }
        bound
    }

    /// `T::name`: an inherent associated item first, else the one item a
    /// trait in scope gives `T`; one the caller may exist without only when
    /// it is the name's only candidate, and none the caller's `#[cfg]`s
    /// pick among several ([`Index::told_apart`]).
    pub fn assoc_path(&self, pos: Pos, ty: (usize, u32), name: &str) -> Res {
        if self.unprobed(pos, ty) {
            return Res::Unknown;
        }
        let (found, cfg) = self.tracking(|| self.candidates(pos, ty, name, false, &Args::Unknown));
        self.told_apart(Self::assoc_pick(&found), cfg && found.told.get())
    }

    fn assoc_pick(found: &Candidates) -> Res {
        if found.macro_made || found.overridden || found.unsure {
            return Res::Unknown;
        }
        let others = found.total() > 1;
        let inherent = found.inherent_pick(|_, _| true);
        if inherent.res != Res::Absent {
            // A path reaches an inherent item before any trait's.
            return inherent.alone(others);
        }
        match (found.traits.is_empty(), found.hidden.is_empty()) {
            (true, true) => Res::External,
            (false, true) => found.trait_pick(|_, _| true).alone(others),
            (_, false) => Res::Unknown,
        }
    }

    /// `x.name()` with `x` of type `ty`, or of a pointer to it; `value` when
    /// `x` is known to be a `ty` itself, not a reference to one.
    fn method_on(
        &self,
        pos: Pos,
        ty: (usize, u32),
        name: &str,
        pointer: bool,
        value: bool,
        args: &Args,
    ) -> Res {
        if self.unprobed(pos, ty)
            || (pointer && (self.pointer_has(name) || self.blanket.contains_key(name)))
        {
            return Res::Unknown;
        }
        let (found, cfg) = self.tracking(|| self.candidates(pos, ty, name, true, args));
        let typed = found
            .inherent
            .iter()
            .map(|(f, a, _)| (*f, *a))
            .chain(found.traits.iter().map(|(f, a, _, _)| (*f, *a)))
            .any(|(f, a)| self.self_kind(f, a) == SelfKind::Typed);
        if found.by_ref || found.macro_made || found.overridden || found.unsure || typed {
            return Res::Unknown;
        }
        if pointer && found.traits.iter().any(|(_, _, foreign, _)| *foreign) {
            return Res::Unknown;
        }
        let receivers: &[&[Step]] = if value && !pointer {
            &[FROM_VALUE]
        } else {
            &[FROM_VALUE, FROM_REF, FROM_MUT]
        };
        let mut answer = None;
        for steps in receivers {
            match self.probe(&found, name, steps) {
                None => {}
                Some(Res::Found(def)) if answer.is_none_or(|a| a == def) => answer = Some(def),
                Some(_) => return Res::Unknown,
            }
        }
        let answer = answer.map_or(Res::External, Res::Found);
        self.told_apart(answer, cfg && found.told.get())
    }

    /// Whether no probe on `ty` from `pos` is resolved: a macro may write
    /// an impl there (D23), a derive of a dependency may write any item of
    /// `ty`, or an impl on an alias of `ty` the rules cannot expand exactly
    /// is not attached (D43, D20).
    fn unprobed(&self, pos: Pos, ty: (usize, u32)) -> bool {
        self.macro_probe(pos) || self.foreign_derive(ty) || self.aliased.contains(&ty)
    }

    /// The method the probe picks over `steps`: at each, an inherent method
    /// taking `self` that way, else the one trait method doing so, unless a
    /// method no row holds fits the step too; `None` when no step finds one
    /// (that receiver could not make the call). A method the caller may
    /// exist without is picked only when it is the name's only candidate;
    /// one the caller's `#[cfg]`s pick among several does not stand
    /// ([`Index::told_apart`], `method_on`).
    fn probe(&self, found: &Candidates, name: &str, steps: &[Step]) -> Option<Res> {
        let others = found.total() > 1;
        for step in steps {
            let blanket = found.hidden.iter().any(|(kind, blanket)| {
                *blanket && (*kind == SelfKind::Value || *kind == step.blanket)
            });
            let Some(own) = step.own else {
                if blanket || ON_REFERENCES.contains(&name) {
                    return Some(Res::Unknown);
                }
                continue;
            };
            let fits = |f: usize, a: u32| self.self_kind(f, a) == own;
            let inherent = found.inherent_pick(fits);
            if inherent.res != Res::Absent {
                return Some(inherent.alone(others));
            }
            let traits = found.trait_pick(fits);
            let hidden = blanket
                || found
                    .hidden
                    .iter()
                    .any(|(kind, blanket)| !*blanket && *kind == own);
            match (traits.res, hidden) {
                (Res::Absent, false) => {}
                (_, false) => return Some(traits.alone(others)),
                (_, true) => return Some(Res::Unknown),
            }
        }
        None
    }

    fn pointer_has(&self, name: &str) -> bool {
        POINTER_METHODS.contains(&name) || self.pointer_methods.contains(name)
    }

    /// The one associated item of a trait named `name` (a method needs a
    /// `self`); none or several, as when a supertrait declares it, is
    /// unknown. Two under different `#[cfg]`s are one for a path, which
    /// names the trait, only for a caller carrying one of them, and never
    /// for a method call ([`Index::untold`]): a macro the rules do not read
    /// may write a method the probe takes first.
    pub fn trait_item(&self, tr: (usize, u32), name: &str, method: bool) -> Res {
        if self.files[tr.0]
            .trait_scope
            .get(&tr.1)
            .is_some_and(|scope| self.macro_made(tr.0, *scope))
        {
            return Res::Unknown;
        }
        let pick = || {
            Pick::told(
                self.trait_assocs(tr, name)
                    .into_iter()
                    .filter(|a| self.fits(tr.0, *a, method))
                    .map(|a| {
                        (
                            Res::Found(Def::Assoc(tr.0, a)),
                            self.assoc_conditional(tr.0, a),
                        )
                    }),
            )
        };
        let (found, told) = pick();
        match self.untold(found.res, method && told, || pick().0.res) {
            Res::Absent => Res::Unknown,
            res => res,
        }
    }

    /// A method call: the receiver's type, then its method.
    pub fn method_call(&self, pos: Pos, receiver: &Receiver, name: &str) -> Res {
        // An impl a macro writes may take the call (D23): an inherent
        // method of the receiver's type, a trait's, or one on `dyn Tr`.
        if self.macro_probe(pos) {
            return Res::Unknown;
        }
        let value = matches!(receiver, Receiver::Ctor { .. } | Receiver::Literal { .. });
        match self.receiver_type(pos, receiver) {
            Some(Recv::Type { ty, pointer, args }) => {
                self.method_on(pos, ty, name, pointer, value, &args)
            }
            Some(Recv::Trait { tr, pointer }) => {
                if (pointer && self.pointer_has(name)) || self.blanket.contains_key(name) {
                    Res::Unknown
                } else {
                    self.trait_item(tr, name, true)
                }
            }
            None => Res::Unknown,
        }
    }

    fn receiver_type(&self, pos: Pos, receiver: &Receiver) -> Option<Recv> {
        match receiver {
            Receiver::SelfValue => match self.self_type(pos, 0) {
                Res::Found(Def::Item(file, item)) if self.is_trait((file, item)) => {
                    Some(Recv::Trait {
                        tr: (file, item),
                        pointer: false,
                    })
                }
                Res::Found(Def::Item(file, item)) if self.is_type((file, item)) => {
                    Some(Recv::Type {
                        ty: (file, item),
                        pointer: false,
                        args: self.self_args(pos),
                    })
                }
                _ => None,
            },
            Receiver::Type(ty) => self.type_ref(pos, ty),
            Receiver::Ctor {
                scope,
                path,
                method,
                unwrapped,
            } => {
                let at = Pos {
                    scope: *scope,
                    ..pos
                };
                self.constructed(at, path, method, *unwrapped)
                    .map(|(ty, args)| Recv::Type {
                        ty,
                        pointer: false,
                        args,
                    })
            }
            Receiver::Literal { scope, path } => {
                let at = Pos {
                    scope: *scope,
                    ..pos
                };
                self.literal(at, path).map(|ty| Recv::Type {
                    ty,
                    pointer: false,
                    args: self.written_args(at, path),
                })
            }
            Receiver::Unknown => None,
        }
    }

    /// A stated type: its outer path, or, when that is a std pointer, what
    /// it points to.
    fn type_ref(&self, pos: Pos, ty: &TypeRef) -> Option<Recv> {
        let pos = Pos {
            scope: ty.scope,
            ..pos
        };
        let mut pointer = false;
        for (index, layer) in ty.layers.iter().enumerate() {
            let named = self.path(pos, &layer.path, Ns::Type, 0);
            match self.type_def(named, 0) {
                Res::Found(Def::Item(file, item)) => {
                    let def = (file, item);
                    // An alias's arguments are those its definition writes.
                    let args = self.value_args(pos, named, layer, def);
                    return match (self.is_trait(def), layer.dynamic) {
                        (true, true) => Some(Recv::Trait { tr: def, pointer }),
                        (false, false) if self.is_type(def) => Some(Recv::Type {
                            ty: def,
                            pointer,
                            args,
                        }),
                        _ => None,
                    };
                }
                Res::External | Res::Absent
                    if !layer.dynamic
                        && index + 1 < ty.layers.len()
                        && layer
                            .path
                            .last()
                            .is_some_and(|last| POINTERS.contains(&last.as_str())) =>
                {
                    pointer = true;
                }
                _ => return None,
            }
        }
        None
    }

    /// `T::f(…)`: `T` when the function it calls is proven to return `Self`
    /// or `T`, or, for `T::default()`, when a plain `#[derive]` of `T`
    /// names `Default` (one a `cfg_attr` applies may not apply, and
    /// `default` be another trait's); `unwrapped`, when it is proven to
    /// return std's `Result` or `Option` of `Self` or `T`
    /// ([`Index::try_output`]). With the value's generic arguments: those
    /// [`Index::written_args`] gives, unknown when unwrapped from anything
    /// but `Self`.
    fn constructed(
        &self,
        pos: Pos,
        path: &[String],
        method: &str,
        unwrapped: bool,
    ) -> Option<((usize, u32), Args)> {
        let (ty, written) = self.constructed_type(pos, path, method, unwrapped)?;
        let args = if written {
            self.written_args(pos, path)
        } else {
            Args::Unknown
        };
        Some((ty, args))
    }

    /// [`Index::constructed`]'s type, and whether the value's generic
    /// arguments are those a constructor's path writes: false when it is
    /// unwrapped from a type written other than `Self`.
    fn constructed_type(
        &self,
        pos: Pos,
        path: &[String],
        method: &str,
        unwrapped: bool,
    ) -> Option<((usize, u32), bool)> {
        let Res::Found(Def::Item(file, item)) = self.type_def(self.path(pos, path, Ns::Type, 0), 0)
        else {
            return None;
        };
        let ty = (file, item);
        if !self.is_type(ty) {
            return None;
        }
        let found = self.candidates(pos, ty, method, false, &Args::Unknown);
        if found.macro_made || found.overridden || found.unsure {
            return None;
        }
        let others = found.total() > 1;
        let returns = |res: Res| match res {
            Res::Found(Def::Assoc(f, a)) => self.returns(f, a, ty, unwrapped).map(|w| (ty, w)),
            _ => None,
        };
        let inherent = found.inherent_pick(|_, _| true);
        if inherent.res != Res::Absent {
            return returns(inherent.alone(others));
        }
        let derived = matches!(
            self.files[file].item_kind(item),
            Some(
                ItemKind::Struct {
                    plain_default: true,
                    ..
                } | ItemKind::Enum {
                    plain_default: true,
                    ..
                }
            )
        );
        match (found.traits.is_empty(), found.hidden.is_empty()) {
            (true, _) if method == "default" && derived && !unwrapped => Some((ty, true)),
            (false, true) => returns(found.trait_pick(|_, _| true).alone(others)),
            _ => None,
        }
    }

    /// Whether an associated function's return type is `ty` (`Self` read in
    /// its impl), or, when `unwrapped`, std's `Result` or `Option` of `ty`;
    /// `Some(true)` when it is, as written, `Self` or a path to `ty`,
    /// `Some(false)` when it is unwrapped from a type written other than
    /// `Self` (whose generic arguments may be other than `Self`'s).
    fn returns(&self, file: usize, assoc: u32, ty: (usize, u32), unwrapped: bool) -> Option<bool> {
        let declared = self.assoc(file, assoc);
        let AssocKind::Fn {
            returns: Some(layer),
            ..
        } = &declared.kind
        else {
            return None;
        };
        let module = self
            .sole_instance(file, declared.owner)
            .filter(|_| !layer.dynamic)?;
        let pos = Pos {
            file,
            scope: declared.owner,
            module,
        };
        let target = Res::Found(Def::Item(ty.0, ty.1));
        if !unwrapped {
            return (self.type_def(self.path(pos, &layer.path, Ns::Type, 0), 0) == target)
                .then_some(true);
        }
        let (at, output) = self.try_output(pos, layer)?;
        let path = plain_path(&output)?;
        (self.type_def(self.path(at, &path, Ns::Type, 0), 0) == target).then_some(output == "Self")
    }

    /// The type `?` takes out of a value of type `layer`, written at `pos`
    /// (as `.unwrap()` and `.expect(…)` do): the first argument of std's
    /// `Result` or `Option`, named as the prelude gives it or by its path in
    /// `std` or `core` ([`Index::std_wrapper`]), or through an alias of the
    /// workspace whose target names one of them (`type Result<T> =
    /// std::result::Result<T, Error>;`), the argument written on the alias
    /// in its parameter's place, read where each is written. `None` when
    /// the rules cannot prove it: any other type (`anyhow::Result`, a
    /// dependency's or std's own alias such as `io::Result`, which the
    /// rules do not read), an alias of an alias, one several crates compile
    /// or whose target takes a parameter's default.
    fn try_output(&self, pos: Pos, layer: &Layer) -> Option<(Pos, String)> {
        if let Some(arity) = self.std_wrapper(pos, &layer.path) {
            return (layer.args.len() == arity).then(|| (pos, layer.args[0].clone()));
        }
        let Res::Found(Def::Item(file, item)) = self.path(pos, &layer.path, Ns::Type, 0) else {
            return None;
        };
        let declared = self.files[file].facts.items.get(item as usize)?;
        let ItemKind::Alias {
            target: Some(target),
            params,
        } = &declared.kind
        else {
            return None;
        };
        if target.dynamic || layer.args.len() > *params as usize {
            return None;
        }
        let at = Pos {
            file,
            scope: declared.scope,
            module: self.sole_instance(file, declared.scope)?,
        };
        let arity = self.std_wrapper(at, &target.path)?;
        let first = target.args.first().filter(|_| target.args.len() == arity)?;
        match first.strip_prefix('$') {
            Some(k) => {
                let k: usize = k.parse().ok()?;
                layer.args.get(k).map(|arg| (pos, arg.clone()))
            }
            None => Some((at, first.clone())),
        }
    }

    /// How many type arguments std's `Result` (2) or `Option` (1) takes
    /// when `path`, read at `pos`, names it: its prelude name where no
    /// scope, glob or crate name binds it, or its path in `std` or `core`
    /// (`std::result::Result`, `::core::option::Option`) where no scope
    /// binds that name and no dependency takes it.
    fn std_wrapper(&self, pos: Pos, path: &[String]) -> Option<usize> {
        let arity = |name: &str| match name {
            "Result" => Some(2),
            "Option" => Some(1),
            _ => None,
        };
        let full = |root: &str, module: &str, name: &str| {
            let module_of = match name {
                "Result" => "result",
                _ => "option",
            };
            module == module_of && self.is_std(pos.module, root)
        };
        match path {
            [name] => arity(name).filter(|_| self.path(pos, path, Ns::Type, 0) == Res::Absent),
            [root, module, name] => arity(name)
                .filter(|_| full(root, module, name) && self.scoped(pos, root) == Res::Absent),
            [empty, root, module, name] if empty.is_empty() => {
                arity(name).filter(|_| full(root, module, name))
            }
            _ => None,
        }
    }

    /// `T { … }`: the struct, or the enum of a variant.
    fn literal(&self, pos: Pos, path: &[String]) -> Option<(usize, u32)> {
        match self.path(pos, path, Ns::Type, 0) {
            Res::Found(Def::Variant(file, variant)) => {
                Some((file, self.files[file].facts.variants[variant as usize].item))
            }
            found => match self.type_def(found, 0) {
                Res::Found(Def::Item(file, item)) if self.is_type((file, item)) => {
                    Some((file, item))
                }
                _ => None,
            },
        }
    }

    /// `<T as Tr>::name` (D20): the item of `T`'s impl of `Tr` when it has
    /// one, else the trait's default that impl takes; `<T>::name` as
    /// `T::name`. `T` must be a type of the workspace, behind one `&` or
    /// `&mut` at most, with an impl of `Tr` the rules see for exactly that
    /// type and its generic arguments, and for the trait's arguments the
    /// path writes (`Tr<u8>` and `Tr<u16>` are two traits): with none, a
    /// blanket impl, a macro's or a dependency's takes its place, and
    /// rustc's coherence forbids a second one beside it. A generic
    /// parameter names the trait's own item.
    pub fn qualified(
        &self,
        pos: Pos,
        ty: Option<&TypeRef>,
        by_ref: Option<bool>,
        (trait_path, trait_args): (&[String], &[String]),
        rest: &[String],
    ) -> Res {
        let [name] = rest else {
            return Res::Unknown;
        };
        let named = ty.map(|ty| self.qualified_type(pos, ty));
        if trait_path.is_empty() {
            return match (named, by_ref) {
                (Some(Some((ty, _))), None) => self.assoc_path(pos, ty, name),
                _ => Res::Unknown,
            };
        }
        let tr = match self.path(pos, trait_path, Ns::Type, 0) {
            Res::Found(Def::Item(file, item)) if self.is_trait((file, item)) => (file, item),
            Res::Found(_) => return Res::Unknown,
            other => return other,
        };
        let Some(named) = named else {
            return self.trait_item(tr, name, false);
        };
        let Some((ty, args)) = named else {
            return Res::Unknown;
        };
        self.impl_item(pos, (ty, &args), by_ref, (tr, trait_args), name)
    }

    /// The item `name` of the impl of the trait `tr`, with `trait_args`
    /// written on it, for the type `ty` with `args`, behind `by_ref`; else
    /// the trait's default that impl takes (see [`Index::qualified`]).
    fn impl_item(
        &self,
        pos: Pos,
        (ty, args): ((usize, u32), &Args),
        by_ref: Option<bool>,
        (tr, trait_args): ((usize, u32), &[String]),
        name: &str,
    ) -> Res {
        if self.unprobed(pos, ty) {
            return Res::Unknown;
        }
        let all = match by_ref {
            None => self.impls.get(&ty),
            Some(mutable) => self.ref_impls.get(&(ty, mutable)),
        };
        let mut impls = Vec::new();
        let mut keys: Vec<Option<&[String]>> = Vec::new();
        let (mut proven, mut may) = (false, false);
        for imp in all.into_iter().flatten() {
            if imp.trait_of != TraitOf::Workspace(tr.0, tr.1) {
                continue;
            }
            match self.trait_fit(imp, pos, trait_args) {
                Fit::No => continue,
                Fit::Maybe => {
                    may = true;
                    continue;
                }
                Fit::Yes => {}
            }
            match self.fit(imp, args) {
                Some(Fit::No) => continue,
                Some(Fit::Maybe) => {
                    may = true;
                    continue;
                }
                Some(Fit::Yes) => proven = true,
                None => {
                    let key = (!self.for_every_argument(imp)).then(|| self.impl_args(imp));
                    if !keys.contains(&key) {
                        keys.push(key);
                    }
                }
            }
            impls.push(imp);
        }
        // An impl that may apply (a bound, D43, or trait arguments the rules
        // cannot compare, D20) is not the one when another is proven for
        // `T` and the trait's arguments: coherence forbids two.
        if may && !(proven && keys.is_empty()) {
            return Res::Unknown;
        }
        if impls.is_empty()
            || keys.len() > 1
            || impls.iter().any(|imp| self.macro_made(imp.file, imp.scope))
        {
            return Res::Unknown;
        }
        let own = Pick::of(impls.iter().flat_map(|imp| {
            self.files[imp.file]
                .assocs_named(imp.scope, name)
                .iter()
                .map(move |a| {
                    (
                        Res::Found(Def::Assoc(imp.file, *a)),
                        self.assoc_conditional(imp.file, *a),
                    )
                })
        }));
        // An impl's item the caller may exist without leaves the trait's
        // own in its place elsewhere.
        match own.res {
            Res::Absent => {}
            _ if own.conditional => return Res::Unknown,
            res => return res,
        }
        // The impl takes the trait's default, when it is there wherever the
        // caller is; a required item it does not define is a macro's.
        let impl_conditional = impls.iter().any(|imp| {
            let cfg = &self.files[imp.file].facts.scopes[imp.scope as usize].cfg;
            self.conditional(cfg, &self.file_cfg[imp.file])
        });
        if impl_conditional {
            return Res::Unknown;
        }
        match self.trait_item(tr, name, false) {
            Res::Found(Def::Assoc(file, assoc))
                if matches!(
                    self.assoc(file, assoc).kind,
                    AssocKind::Fn { body: false, .. }
                ) =>
            {
                Res::Unknown
            }
            res => res,
        }
    }

    /// `Tr::name(first, …)`, a path naming a trait's method (D20): `Self`
    /// is what the first argument gives the method's `self` (`&mut x` for
    /// `&mut self` is `x`'s type, `&x` for `self` is `&` its type), and the
    /// call names the item of `Self`'s impl of the trait as `<Self as
    /// Tr>::name` does, when the code states that type: a type of the
    /// workspace, behind one `&` or `&mut` at most, and a trait with no
    /// type or const parameter, whose arguments the call would infer where
    /// `<Self as Tr>` takes the defaults. An argument of a generic
    /// parameter, `dyn Tr` or `impl Tr` names the trait's own item (the
    /// impl is the caller's to choose), as `<X as Tr>::name` does; any
    /// other, or a method taking no `self`, is unresolved. `None` when the
    /// path names no trait's item, or names it through `Self` (in a trait's
    /// body, a generic parameter; in an impl's, a path through a type).
    pub fn trait_call(&self, pos: Pos, path: &[String], first: &Argument) -> Option<Res> {
        let (name, prefix) = path.split_last()?;
        if prefix.is_empty() || prefix.first().is_some_and(|head| head == "Self") {
            return None;
        }
        let tr = match self.path(pos, prefix, Ns::Type, 0) {
            Res::Found(Def::Item(file, item)) if self.is_trait((file, item)) => (file, item),
            _ => return None,
        };
        let own = self.trait_item(tr, name, false);
        let Res::Found(Def::Assoc(file, assoc)) = own else {
            return Some(own);
        };
        let (refs, receiver) = match first {
            Argument::Generic => return Some(own),
            Argument::Unknown => return Some(Res::Unknown),
            Argument::Typed { refs, receiver } => (refs.as_slice(), receiver),
        };
        // The references `Self` keeps once `self`'s are taken off: a
        // `&mut` reborrows as the `&` a `&self` takes.
        let kept = match (self.self_kind(file, assoc), refs.split_first()) {
            (SelfKind::Value, _) => refs,
            (SelfKind::Ref, Some((_, rest))) | (SelfKind::RefMut, Some((true, rest))) => rest,
            _ => return Some(Res::Unknown),
        };
        let by_ref = match kept {
            [] => None,
            [mutable] => Some(*mutable),
            _ => return Some(Res::Unknown),
        };
        let at = |scope: u32| Pos { scope, ..pos };
        let (ty, args) = match receiver {
            Receiver::Type(ty) => {
                if let Some(layer) = ty.layers.first().filter(|l| l.dynamic) {
                    // `dyn Tr` or `impl Tr` of this trait: dispatched at run
                    // time or by the caller.
                    let named = self.path(at(ty.scope), &layer.path, Ns::Type, 0);
                    let same = named == Res::Found(Def::Item(tr.0, tr.1)) && kept.is_empty();
                    return Some(if same { own } else { Res::Unknown });
                }
                match self.qualified_type(pos, ty) {
                    Some(named) => named,
                    None => return Some(Res::Unknown),
                }
            }
            Receiver::Ctor {
                scope,
                path,
                method,
                unwrapped,
            } => match self.constructed(at(*scope), path, method, *unwrapped) {
                Some(found) => found,
                None => return Some(Res::Unknown),
            },
            Receiver::Literal { scope, path } => match self.literal(at(*scope), path) {
                Some(ty) => (ty, self.written_args(at(*scope), path)),
                None => return Some(Res::Unknown),
            },
            Receiver::SelfValue => match self.self_type(pos, 0) {
                // `self` in a trait's body is of a generic type.
                Res::Found(Def::Item(f, i)) if self.is_trait((f, i)) => {
                    return Some(if (f, i) == tr && kept.is_empty() {
                        own
                    } else {
                        Res::Unknown
                    });
                }
                Res::Found(Def::Item(f, i)) if self.is_type((f, i)) => {
                    ((f, i), self.self_args(pos))
                }
                _ => return Some(Res::Unknown),
            },
            Receiver::Unknown => return Some(Res::Unknown),
        };
        // The trait's arguments are inferred here, from a turbofish,
        // another argument or the type the call must have, not defaulted:
        // `D::d(&a)` of `trait D<T = u8>` may run `impl D<u16> for A`,
        // which coherence lets stand beside `impl D for A`.
        if matches!(
            self.files[tr.0].item_kind(tr.1),
            Some(ItemKind::Trait { generic: true })
        ) {
            return Some(Res::Unknown);
        }
        Some(self.impl_item(pos, (ty, &args), by_ref, (tr, &[]), name))
    }

    /// The type of the workspace a qualified path's `T` names, with the
    /// arguments written on it (an alias's, those its definition writes).
    fn qualified_type(&self, pos: Pos, ty: &TypeRef) -> Option<((usize, u32), Args)> {
        let layer = ty.layers.first().filter(|l| !l.dynamic)?;
        let at = Pos {
            scope: ty.scope,
            ..pos
        };
        let named = self.path(at, &layer.path, Ns::Type, 0);
        match self.type_def(named, 0) {
            Res::Found(Def::Item(file, item)) if self.is_type((file, item)) => Some((
                (file, item),
                self.value_args(at, named, layer, (file, item)),
            )),
            _ => None,
        }
    }
}

/// A generic argument's text as a type path, its own arguments dropped
/// (`a::B<u8>` → `a`, `B`), when it is one: not a parameter, a reference,
/// a tuple or anything else the text of a path does not spell.
fn plain_path(text: &str) -> Option<Vec<String>> {
    let head = match text.split_once('<') {
        Some((head, rest)) if rest.ends_with('>') => head,
        Some(_) => return None,
        None => text,
    };
    let spelt = !head.is_empty()
        && !is_param(head)
        && head
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ':');
    spelt.then(|| head.split("::").map(str::to_owned).collect())
}
