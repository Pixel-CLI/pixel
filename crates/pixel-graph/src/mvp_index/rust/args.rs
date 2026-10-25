// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Generic arguments (D43): which impls of a generic type apply to a value
//! of it. An impl's self type keeps the arguments written on it, and so
//! does a stated type (`Layer::args`); `impl G<u8>` applies to a `G<u8>`
//! only, `impl<T> G<T>` to every `G`. A parameter keeps what the impl
//! requires of it: `impl<T: Tr> G<T>` is for every `G` too when `G`
//! declares `T: Tr` itself, since rustc rejects any other `G` as
//! ill-formed (E0277). A parameter held to more (`impl<T: Copy> G<T>`, a
//! `where` predicate on a type built on it or on `Self`, a second use as in
//! `impl<T> G<T, T>`, or, without `?Sized`, one `G` declares `?Sized`) is
//! for some arguments only, and the rules do not evaluate bounds: such an
//! impl may apply, except to `Self` in its own body. A lifetime bound
//! applies to every argument (rustc picks the method first and checks
//! lifetimes after). Two arguments are the same type only when the rules
//! prove it: the same text whose names resolve alike at both places, or
//! two paths resolving to one type of the workspace; two different types
//! of the workspace, two different primitives, or one of each, differ.
//! Anything else may be either. An impl that may apply gives the name
//! candidates that stand only when nothing else provides it, since a
//! program calling the name compiles only if one does.
//!
//! A type written through an alias (`impl Grid` with `type Grid = M<u8>`,
//! `impl<T> A<T>` with `type A<T> = X<u8, T>`, a value of type `&Mask`) is
//! the alias's definition with the arguments written on the alias in place
//! of its parameters, each read where it is written: `X<u8, T>`, `u8` read
//! at the alias. The rules expand an alias so only when it is exact: an
//! alias of a struct, enum or union written with every parameter of the
//! alias, which one crate compiles. An impl on any other alias (an alias of
//! an alias, a parameter left to its default, an alias several crates
//! compile) is not attached, and every probe on its type is unresolved
//! (`Index::aliased`, D43, D20); one on an alias of a type outside the
//! workspace may be on a pointer the probe looks through, and its items
//! may be any type's. A value of an alias the rules cannot expand has
//! unknown arguments.

use rustc_hash::FxHashMap as HashMap;

use super::modules::ModId;
use super::resolve::{ANY_CALLER, Def, Impl, Index, Ns, Pos, Res, TraitOf};
use super::types::{BOUNDED, CONSTRAINED, is_param};
use super::{ItemKind, Layer, ScopeKind};

/// An impl's parameter that is any `Sized` argument of its type, which
/// declares it `?Sized`: never written, set by `Index::effective_args`.
const SIZED: &str = "_!";

/// One generic argument of a value's type: its text, and where it is
/// written, which its names are read at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arg {
    pub at: Pos,
    pub text: String,
}

/// The arguments of the type a method is called on: known, `Self`'s in the
/// body of an impl (file, scope), or unknown (a constructor's, a
/// literal's, an alias's the rules cannot expand).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Args {
    Known(Vec<Arg>),
    Own(usize, u32, Vec<Arg>),
    Unknown,
}

impl Args {
    /// `values`, each written at `at`.
    pub fn at(at: Pos, values: &[String]) -> Vec<Arg> {
        values
            .iter()
            .map(|text| Arg {
                at,
                text: text.clone(),
            })
            .collect()
    }
}

/// The arguments an attached impl is for, where they are not those
/// written on its self type (`Index::effective`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Effective {
    pub args: Vec<String>,
    /// Where each of `args` is written: `None` on the impl, read in each
    /// crate compiling it; else in an alias's definition.
    pub at: Vec<Option<Pos>>,
}

/// One argument of the type an alias names: the one written on the alias
/// for its parameter `k`, or one its definition writes, read there.
enum Expanded {
    Param(usize),
    Text(Pos, String),
}

/// What [`Index::fit`] reads of an impl, but for the value's `Self`: its
/// file, the scope around it, and the arguments it is for, with where each
/// is written.
pub type FitKey<'s> = (usize, Option<u32>, &'s [String], &'s [Option<Pos>]);

/// What the arguments read at one place prove they are, by text
/// ([`Index::arg_type`]).
pub type ArgTypes = HashMap<String, Option<ArgType<'static>>>;

/// Whether an impl applies to a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    Yes,
    No,
    Maybe,
}

impl Fit {
    /// Every one of `fits` at once: no when one is no, yes when all are.
    fn all(fits: impl IntoIterator<Item = Fit>) -> Fit {
        let mut out = Fit::Yes;
        for fit in fits {
            match fit {
                Fit::No => return Fit::No,
                Fit::Maybe => out = Fit::Maybe,
                Fit::Yes => {}
            }
        }
        out
    }
}

/// The primitive types: a name of one no scope binds is that type.
const PRIMITIVES: [&str; 18] = [
    "bool", "char", "f128", "f16", "f32", "f64", "i128", "i16", "i32", "i64", "i8", "isize", "str",
    "u128", "u16", "u32", "u64", "u8",
];

/// Words of a type's text that name nothing (`dyn Tr`, `&mut T`, `fn()`).
const KEYWORDS: [&str; 9] = [
    "as", "const", "dyn", "extern", "fn", "for", "impl", "mut", "unsafe",
];

impl Index<'_> {
    /// The arguments an impl is for: those its self type is written with,
    /// each parameter read against its type's declared bounds.
    pub fn impl_args(&self, imp: &Impl) -> &[String] {
        if let Some(effective) = self.effective.get(&(imp.file, imp.scope)) {
            return &effective.args;
        }
        match self.files[imp.file]
            .facts
            .scopes
            .get(imp.scope as usize)
            .map(|s| &s.kind)
        {
            Some(ScopeKind::Impl {
                self_type: Some(layer),
                ..
            }) => &layer.args,
            _ => &[],
        }
    }

    /// Whether an impl is for every argument: `impl<T> G<T>`, or `impl<T:
    /// Tr> G<T>` when `G` declares `T: Tr` itself.
    pub fn for_every_argument(&self, imp: &Impl) -> bool {
        let args = self.impl_args(imp);
        !args.is_empty() && args.iter().all(|a| a == "_")
    }

    /// What an impl whose self type is written `written` at `scope` of
    /// `file`, naming the alias `alias` of the type `ty`, is for: the
    /// alias's definition with `written` in place of its parameters, each
    /// parameter of the impl read against the bounds `ty` declares
    /// ([`Index::effective_args`]); one the alias puts in two places is held
    /// to more than its bounds. `None` when the rules cannot expand the
    /// alias exactly.
    pub fn alias_impl_args(
        &self,
        (file, scope): (usize, u32),
        written: &[String],
        alias: (usize, u32),
        ty: (usize, u32),
    ) -> Option<Effective> {
        let expanded = self.expand_alias(alias, written.len())?;
        let mut uses = vec![0usize; written.len()];
        for arg in &expanded {
            if let Expanded::Param(k) = arg {
                uses[*k] += 1;
            }
        }
        let (args, at): (Vec<String>, Vec<Option<Pos>>) = expanded
            .into_iter()
            .map(|arg| match arg {
                Expanded::Param(k) if is_param(&written[k]) && uses[k] > 1 => {
                    (CONSTRAINED.to_owned(), None)
                }
                Expanded::Param(k) => (written[k].clone(), None),
                Expanded::Text(at, text) => (text, Some(at)),
            })
            .unzip();
        Some(Effective {
            args: self.effective_args(file, scope, &args, ty),
            at,
        })
    }

    /// The arguments of a value whose type `layer` is written at `pos`,
    /// its path naming `named`, which is `ty` or an alias of it: those
    /// written on it, or the alias's definition with them in place of its
    /// parameters; unknown when the rules cannot expand the alias.
    pub fn value_args(&self, pos: Pos, named: Res, layer: &Layer, ty: (usize, u32)) -> Args {
        let alias = match named {
            Res::Found(Def::Item(file, item)) if (file, item) == ty => {
                return Args::Known(Args::at(pos, &layer.args));
            }
            Res::Found(Def::Item(file, item)) => (file, item),
            _ => return Args::Unknown,
        };
        match self.expand_alias(alias, layer.args.len()) {
            Some(expanded) => Args::Known(
                expanded
                    .into_iter()
                    .map(|arg| match arg {
                        Expanded::Param(k) => Arg {
                            at: pos,
                            text: layer.args[k].clone(),
                        },
                        Expanded::Text(at, text) => Arg { at, text },
                    })
                    .collect(),
            ),
            None => Args::Unknown,
        }
    }

    /// The arguments of the type the alias `alias` names when `written`
    /// arguments are written on it: `None` when the rules cannot tell
    /// them, for an alias of another alias, one of another number of
    /// parameters (a default fills the rest), or one several crates
    /// compile.
    fn expand_alias(&self, alias: (usize, u32), written: usize) -> Option<Vec<Expanded>> {
        let item = self.files[alias.0].facts.items.get(alias.1 as usize)?;
        let ItemKind::Alias {
            target: Some(target),
            params,
        } = &item.kind
        else {
            return None;
        };
        if target.dynamic || *params as usize != written {
            return None;
        }
        let at = Pos {
            file: alias.0,
            scope: item.scope,
            module: self.sole_instance(alias.0, item.scope)?,
        };
        match self.path(at, &target.path, Ns::Type, 0) {
            Res::Found(Def::Item(file, item))
                if matches!(
                    self.files[file].item_kind(item),
                    Some(ItemKind::Struct { .. } | ItemKind::Enum { .. } | ItemKind::Union { .. })
                ) => {}
            _ => return None,
        }
        Some(
            target
                .args
                .iter()
                .map(
                    |arg| match arg.strip_prefix('$').and_then(|k| k.parse::<usize>().ok()) {
                        Some(k) if k < written => Expanded::Param(k),
                        Some(_) => Expanded::Text(at, "?".to_owned()),
                        None => Expanded::Text(at, arg.clone()),
                    },
                )
                .collect(),
        )
    }

    /// The arguments of an impl's self type `written` at `scope` of `file`,
    /// for the type `ty`: a parameter is `_` (every argument) when the
    /// bounds `ty` declares there imply the impl's, which every value of
    /// `ty` meets (rustc rejects any other as ill-formed, E0277); [`SIZED`]
    /// when, besides, `ty` declares it `?Sized` (an impl's parameter is
    /// `Sized` unless it says otherwise, which the rules do not read); else
    /// [`CONSTRAINED`]. A bound is implied by one of the same text whose
    /// names resolve alike at both places.
    pub fn effective_args(
        &self,
        file: usize,
        scope: u32,
        written: &[String],
        ty: (usize, u32),
    ) -> Vec<String> {
        let declared = self.files[ty.0]
            .facts
            .bounds
            .iter()
            .find(|d| d.item == ty.1)
            .map(|d| d.bounds.as_slice())
            .unwrap_or_default();
        let at_type = self.files[ty.0]
            .facts
            .items
            .get(ty.1 as usize)
            .map_or(0, |item| item.scope);
        written
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let held = declared.get(i).map_or(&[][..], Vec::as_slice);
                let required = if arg == "_" {
                    Vec::new()
                } else if let Some(bounds) = arg.strip_prefix(BOUNDED) {
                    crate::codec::fields(bounds)
                } else {
                    return arg.clone();
                };
                let implied = required.iter().all(|bound| {
                    held.contains(bound) && self.same_bound((file, scope), (ty.0, at_type), bound)
                });
                match (implied, held.iter().any(|h| h == "?Sized")) {
                    (false, _) => CONSTRAINED.to_owned(),
                    (true, false) => "_".to_owned(),
                    (true, true) => SIZED.to_owned(),
                }
            })
            .collect()
    }

    /// Whether a bound's names resolve alike at two places, in every crate
    /// compiling either.
    fn same_bound(&self, a: (usize, u32), b: (usize, u32), bound: &str) -> bool {
        let (left, right) = (self.instances(a.0, a.1), self.instances(b.0, b.1));
        !left.is_empty()
            && !right.is_empty()
            && left.iter().all(|&ma| {
                right.iter().all(|&mb| {
                    let here = Pos {
                        file: a.0,
                        scope: a.1,
                        module: ma,
                    };
                    let there = Pos {
                        file: b.0,
                        scope: b.1,
                        module: mb,
                    };
                    self.same_names(here, there, bound)
                })
            })
    }

    /// Whether `imp` applies to a value with `args`; `None` when the
    /// value's arguments are unknown and the impl is not for every
    /// argument (the caller weighs the impls against each other). An impl
    /// written without arguments is for the type's defaults, the only ones
    /// of a type that has no parameter. An impl applies to `Self` in its
    /// own body.
    pub fn fit(&self, imp: &Impl, args: &Args) -> Option<Fit> {
        // `impl_args` and `for_every_argument`, the effective arguments
        // looked up once: a probe asks this of every impl of its type.
        let effective = self.effective.get(&(imp.file, imp.scope));
        let written = match effective {
            Some(effective) => effective.args.as_slice(),
            None => self.impl_args(imp),
        };
        if !written.is_empty() && written.iter().all(|a| a == "_") {
            return Some(Fit::Yes);
        }
        let values = match args {
            Args::Own(file, scope, ..) if (*file, *scope) == (imp.file, imp.scope) => {
                return Some(Fit::Yes);
            }
            Args::Known(values) | Args::Own(_, _, values) => values,
            Args::Unknown => return None,
        };
        // No argument on either side, in every crate compiling the impl,
        // of which an attached one has one at least.
        if written.is_empty() && values.is_empty() {
            return Some(Fit::Yes);
        }
        let at = effective.map_or(&[][..], |effective| effective.at.as_slice());
        Some(self.written_fit(imp, written, at, values))
    }

    /// [`Index::fit`], asked once of consecutive impls of one scope that
    /// write the same arguments (`last` holds the previous one's answer): a
    /// type may carry hundreds of impls (`PartialEq<S>` for each `S`), and
    /// each would read the scopes of the impl and of the value again
    /// (candidates-fit-every-impl).
    pub fn fit_once<'s>(
        &'s self,
        imp: &Impl,
        args: &Args,
        last: &mut Option<(FitKey<'s>, Option<Fit>)>,
    ) -> Option<Fit> {
        if matches!(args, Args::Own(file, scope, _) if (*file, *scope) == (imp.file, imp.scope)) {
            return self.fit(imp, args);
        }
        let (written, at) = match self.effective.get(&(imp.file, imp.scope)) {
            Some(effective) => (effective.args.as_slice(), effective.at.as_slice()),
            None => (self.impl_args(imp), &[][..]),
        };
        let parent = self.files[imp.file]
            .facts
            .scopes
            .get(imp.scope as usize)
            .and_then(|s| s.parent);
        let key = (imp.file, parent, written, at);
        match last {
            Some((seen, fit)) if *seen == key => *fit,
            _ => {
                let fit = self.fit(imp, args);
                *last = Some((key, fit));
                fit
            }
        }
    }

    /// Whether the generic arguments an impl writes on its trait are those
    /// `values`, written at `at`, give it (D20): none on both sides is the
    /// trait's defaults on both; a different number may be either.
    pub fn trait_fit(&self, imp: &Impl, at: Pos, values: &[String]) -> Fit {
        let written = match self.files[imp.file]
            .facts
            .scopes
            .get(imp.scope as usize)
            .map(|s| &s.kind)
        {
            Some(ScopeKind::Impl { trait_args, .. }) => trait_args.as_slice(),
            _ => &[],
        };
        self.written_fit(imp, written, &[], &Args::at(at, values))
    }

    /// Whether the arguments `written` on `imp` (or, where `written_at`
    /// says so, in an alias's definition) are `values`.
    fn written_fit(
        &self,
        imp: &Impl,
        written: &[String],
        written_at: &[Option<Pos>],
        values: &[Arg],
    ) -> Fit {
        if written.len() != values.len() {
            return Fit::Maybe;
        }
        let scope = self.files[imp.file]
            .facts
            .scopes
            .get(imp.scope as usize)
            .and_then(|s| s.parent)
            .unwrap_or(0);
        // A file several crates compile: each reads the arguments in its
        // own crate, and they must agree.
        let mut answer = None;
        for &module in self.instances(imp.file, scope) {
            let here = Pos {
                file: imp.file,
                scope,
                module,
            };
            let fit = Fit::all(written.iter().zip(values).enumerate().map(|(i, (w, v))| {
                let at_impl = written_at.get(i).copied().flatten().unwrap_or(here);
                self.arg_fit(at_impl, w, v.at, &v.text)
            }));
            answer = match answer {
                None => Some(fit),
                Some(seen) if seen == fit => Some(fit),
                Some(_) => Some(Fit::Maybe),
            };
        }
        answer.unwrap_or(Fit::Maybe)
    }

    /// Whether an impl's argument `w`, written at `at_impl`, is the value's
    /// `v`, written at `at_value`. `_` in an impl is any type, [`SIZED`] any
    /// `Sized` one (a primitive other than `str` is); `_` in the value is a
    /// generic parameter, which only such an impl takes. A parameter the
    /// impl constrains may be any.
    fn arg_fit(&self, at_impl: Pos, w: &str, at_value: Pos, v: &str) -> Fit {
        match (w, v) {
            ("_", _) => Fit::Yes,
            (SIZED, _) => match self.arg_type(at_value, v) {
                Some(ArgType::Primitive(p)) if p != "str" => Fit::Yes,
                _ => Fit::Maybe,
            },
            _ if is_param(w) => Fit::Maybe,
            (_, "_") => Fit::No,
            ("?", _) | (_, "?") => Fit::Maybe,
            _ if w == v && self.same_names(at_impl, at_value, w) => Fit::Yes,
            _ => match (self.arg_type(at_impl, w), self.arg_type(at_value, v)) {
                (Some(a), Some(b)) if a == b => Fit::Yes,
                (Some(_), Some(_)) => Fit::No,
                _ => Fit::Maybe,
            },
        }
    }

    /// [`Index::find_arg_type`], memoized by where the argument is read,
    /// for any caller unless that read a `#[cfg]`, and by its text: a probe
    /// asks it of every impl providing the name, and of each impl again at
    /// every site (candidates-fit-every-impl).
    fn arg_type(&self, pos: Pos, text: &str) -> Option<ArgType<'static>> {
        let at = (pos.file, pos.scope, pos.module);
        let known = {
            let memo = self.arg_types.borrow();
            match memo
                .get(&(at, ANY_CALLER))
                .and_then(|types| types.get(text))
            {
                Some(found) => Some(*found),
                None => memo
                    .get(&(at, self.caller_id()))
                    .and_then(|types| types.get(text))
                    .map(|found| {
                        self.sensitive.set(true);
                        *found
                    }),
            }
        };
        if let Some(found) = known {
            return found;
        }
        let (found, touched) = self.tracking(|| self.find_arg_type(pos, text));
        let caller = if touched {
            self.caller_id()
        } else {
            ANY_CALLER
        };
        self.arg_types
            .borrow_mut()
            .entry((at, caller))
            .or_default()
            .insert(text.to_owned(), found);
        found
    }

    /// The type an argument proves it is: a struct, enum or union of the
    /// workspace its path resolves to, or a primitive no scope rebinds.
    fn find_arg_type(&self, pos: Pos, text: &str) -> Option<ArgType<'static>> {
        if !text
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ':')
        {
            return None;
        }
        if let Some(primitive) = PRIMITIVES.iter().find(|p| **p == text) {
            return (self.scoped(pos, text) == Res::Absent)
                .then_some(ArgType::Primitive(primitive));
        }
        let path: Vec<String> = text.split("::").map(str::to_owned).collect();
        match self.path(pos, &path, Ns::Type, 0) {
            Res::Found(Def::Item(file, item))
                if matches!(
                    self.files[file].item_kind(item),
                    Some(ItemKind::Struct { .. } | ItemKind::Enum { .. } | ItemKind::Union { .. })
                ) =>
            {
                Some(ArgType::Workspace(file, item))
            }
            _ => None,
        }
    }

    /// Whether every name a type's text starts a path with means the same at
    /// `a` and at `b`: one binding of the scopes, or none at either, where
    /// it is a crate name of one crate or a name of std's prelude.
    fn same_names(&self, a: Pos, b: Pos, text: &str) -> bool {
        roots(text).all(|name| match name {
            "crate" | "self" | "super" => a.module == b.module,
            "Self" => false,
            name => match (self.scoped(a, name), self.scoped(b, name)) {
                (Res::Found(x), Res::Found(y)) => x == y,
                (Res::Absent, Res::Absent) => {
                    self.same_crate(a.module, b.module)
                        || matches!(name, "std" | "core")
                        || (self.extern_crate(a.module, name) == Res::Absent
                            && self.extern_crate(b.module, name) == Res::Absent)
                }
                _ => false,
            },
        })
    }

    fn same_crate(&self, a: ModId, b: ModId) -> bool {
        let krate = self.tree.modules[a].krate;
        krate.is_some() && krate == self.tree.modules[b].krate
    }

    /// Whether an impl without an item `name` of its own provides one of
    /// its trait's, when the trait is the workspace's.
    pub fn trait_provides(&self, imp: &Impl, name: &str) -> bool {
        matches!(imp.trait_of, TraitOf::Workspace(file, item)
            if self.files[file]
                .trait_scope
                .get(&item)
                .is_some_and(|scope| !self.files[file].assocs_named(*scope, name).is_empty()))
    }

    /// The arguments of a value a constructor or a literal makes: `Self`'s
    /// (`Self::new()`, `Self { … }`); a type's path in an expression drops
    /// them, so any other's are unknown.
    pub fn written_args(&self, pos: Pos, path: &[String]) -> Args {
        match path {
            [only] if only == "Self" => self.self_args(pos),
            _ => Args::Unknown,
        }
    }

    /// The arguments of `Self` at `pos`: the self type of the impl around
    /// it, where the impl (or the alias it is written with) writes them,
    /// each parameter a generic one of the code there.
    pub fn self_args(&self, pos: Pos) -> Args {
        let scopes = &self.files[pos.file].facts.scopes;
        let mut current = Some(pos.scope);
        while let Some(scope) = current {
            let Some(entry) = scopes.get(scope as usize) else {
                return Args::Unknown;
            };
            match &entry.kind {
                ScopeKind::Impl {
                    self_type: Some(layer),
                    by_ref: false,
                    blanket: false,
                    ..
                } => {
                    let at = Pos {
                        scope: entry.parent.unwrap_or(0),
                        ..pos
                    };
                    let (args, written_at) = match self.effective.get(&(pos.file, scope)) {
                        Some(effective) => (&effective.args, effective.at.as_slice()),
                        None => (&layer.args, &[][..]),
                    };
                    let values = args
                        .iter()
                        .enumerate()
                        .map(|(i, a)| Arg {
                            at: written_at.get(i).copied().flatten().unwrap_or(at),
                            text: if is_param(a) || a == SIZED { "_" } else { a }.to_owned(),
                        })
                        .collect();
                    return Args::Own(pos.file, scope, values);
                }
                ScopeKind::Block => current = entry.parent,
                _ => return Args::Unknown,
            }
        }
        Args::Unknown
    }
}

/// A type an argument is proven to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgType<'t> {
    Workspace(usize, u32),
    Primitive(&'t str),
}

/// The names a type's text starts its paths with: `a::B<Vec<u8>>` gives
/// `a`, `Vec`, `u8`; a segment after `::`, a lifetime, a number and a
/// keyword are none.
fn roots(text: &str) -> impl Iterator<Item = &str> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices().chain([(text.len(), ' ')]) {
        let word = c.is_alphanumeric() || c == '_';
        match (start, word) {
            (None, true) => start = Some(i),
            (Some(s), false) => {
                let before = &text[..s];
                let token = &text[s..i];
                let rooted = !before.ends_with("::") && !before.ends_with('\'');
                if rooted
                    && !token.starts_with(|c: char| c.is_ascii_digit())
                    && !KEYWORDS.contains(&token)
                {
                    out.push(token);
                }
                start = None;
            }
            _ => {}
        }
    }
    out.into_iter()
}

#[cfg(test)]
mod tests {
    use super::roots;

    #[test]
    fn the_roots_of_a_type_are_the_first_names_of_its_paths() {
        let found: Vec<&str> = roots("a::B<Vec<u8>,&'a mut[c::D;4],dyn fmt::Debug>").collect();
        assert_eq!(found, ["a", "Vec", "u8", "c", "fmt"]);
        let found: Vec<&str> = roots("serde_json::Value").collect();
        assert_eq!(found, ["serde_json"]);
    }
}
