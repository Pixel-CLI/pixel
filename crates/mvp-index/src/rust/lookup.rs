// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Names and paths: the lexical lookup of a name from a scope, a path's
//! segments one after the other, `use` bindings and globs. Every function
//! answers with a [`Res`]; only `Found` proves a target.

use super::modules::{ModId, Target, applies};
use super::resolve::{DEPTH, Def, Entry, Index, Ns, Pick, Pos, Res};
use super::{ItemKind, Leaf, ScopeKind, Vis};

/// Whether an item binds a name in `ns`.
fn in_ns(kind: &ItemKind, ns: Ns) -> bool {
    match ns {
        Ns::Type => matches!(
            kind,
            ItemKind::Module { .. }
                | ItemKind::Struct { .. }
                | ItemKind::Enum { .. }
                | ItemKind::Union { .. }
                | ItemKind::Trait { .. }
                | ItemKind::Alias { .. }
                | ItemKind::ExternCrate { .. }
        ),
        Ns::Value => matches!(
            kind,
            ItemKind::Fn
                | ItemKind::Const
                | ItemKind::Static
                | ItemKind::Foreign
                | ItemKind::Struct { value: true, .. }
        ),
        Ns::Macro => matches!(kind, ItemKind::Macro { .. }),
    }
}

/// Names the std prelude gives code in `ns` when nothing in scope binds
/// them (every edition, primitive types included): what takes a
/// conditional binding's place where it is absent. Any macro may be one.
/// The values are rustc 1.99's: `std::mem`'s `size_of`, `size_of_val`,
/// `align_of` and `align_of_val` are in every edition's prelude since 1.80.
fn prelude(name: &str, ns: Ns) -> bool {
    const VALUES: [&str; 9] = [
        "drop",
        "size_of",
        "size_of_val",
        "align_of",
        "align_of_val",
        "Some",
        "None",
        "Ok",
        "Err",
    ];
    const TYPES: [&str; 61] = [
        "AsMut",
        "AsRef",
        "AsyncFn",
        "AsyncFnMut",
        "AsyncFnOnce",
        "Box",
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
        "FromIterator",
        "Future",
        "Into",
        "IntoFuture",
        "IntoIterator",
        "Iterator",
        "Option",
        "Ord",
        "PartialEq",
        "PartialOrd",
        "Result",
        "Send",
        "Sized",
        "String",
        "Sync",
        "ToOwned",
        "ToString",
        "TryFrom",
        "TryInto",
        "Unpin",
        "Vec",
        "alloc",
        "bool",
        "char",
        "core",
        "f128",
        "f16",
        "f32",
        "f64",
        "i128",
        "i16",
        "i32",
        "i64",
        "i8",
        "isize",
        "std",
        "str",
        "u128",
        "u16",
        "u32",
        "u64",
        "u8",
        "usize",
    ];
    match ns {
        Ns::Value => VALUES.contains(&name),
        Ns::Type => TYPES.contains(&name),
        Ns::Macro => true,
    }
}

impl Index<'_> {
    /// `name` looked up from `pos` outward: the blocks around it, then its
    /// module, then (for a type) the crate names of its package.
    ///
    /// A glob the rules cannot read (one of a dependency) may provide any
    /// name, so in code it ends the search unresolved. In a `use` path
    /// (`import`) it does not: Rust rejects an import whose first segment a
    /// glob and an outer scope both provide ("ambiguous because of a conflict
    /// between a name from a glob import and an outer scope during import or
    /// macro resolution"), so a name an outer scope or the crate names
    /// provide is that one.
    ///
    /// A binding the caller may exist without (`#[cfg]`, F2) is the answer
    /// only when nothing after it binds the name: no later stage, crate
    /// name or prelude name, which would take its place where it is absent.
    pub fn lookup(&self, pos: Pos, name: &str, ns: Ns, depth: u32, import: bool) -> Res {
        if depth > DEPTH {
            return Res::Unknown;
        }
        let scopes = &self.files[pos.file].facts.scopes;
        let mut uncertain = false;
        let mut conditional: Option<Res> = None;
        let mut current = Some(pos.scope);
        while let Some(scope) = current {
            let Some(entry) = scopes.get(scope as usize) else {
                return Res::Unknown;
            };
            if !matches!(entry.kind, ScopeKind::Impl { .. } | ScopeKind::Trait { .. }) {
                let here = Pos { scope, ..pos };
                for glob in [false, true] {
                    let stage = if glob {
                        self.globs(here, name, ns, depth, pos.module, &mut Vec::new())
                    } else {
                        self.explicit(pos.file, scope, pos.module, name, ns, depth, None)
                    };
                    if stage.res == Res::Absent {
                        continue;
                    }
                    if conditional.is_some() {
                        return Res::Unknown;
                    }
                    if glob && import && stage.res == Res::Unknown {
                        uncertain = true;
                        continue;
                    }
                    match stage {
                        Pick {
                            res: found @ Res::Found(_),
                            conditional: true,
                        } => conditional = Some(found),
                        Pick {
                            conditional: true, ..
                        } => return Res::Unknown,
                        Pick { res, .. } => return res,
                    }
                }
                if matches!(entry.kind, ScopeKind::File | ScopeKind::Module { .. }) {
                    break;
                }
            }
            current = entry.parent;
        }
        let outer = if ns == Ns::Type {
            self.extern_crate(pos.module, name)
        } else {
            Res::Absent
        };
        if let Some(found) = conditional {
            return if outer == Res::Absent && !uncertain && !prelude(name, ns) {
                found
            } else {
                Res::Unknown
            };
        }
        match outer {
            Res::Absent if uncertain => Res::Unknown,
            outer => outer,
        }
    }

    /// What the scopes around `pos` bind `name` to in the type namespace,
    /// from the site out to its module: `Absent` when none does (the name
    /// is then a crate name or std's), `Unknown` when a binding the caller
    /// may exist without or a glob the rules cannot read may.
    pub fn scoped(&self, pos: Pos, name: &str) -> Res {
        let scopes = &self.files[pos.file].facts.scopes;
        let mut current = Some(pos.scope);
        while let Some(scope) = current {
            let Some(entry) = scopes.get(scope as usize) else {
                return Res::Unknown;
            };
            if !matches!(entry.kind, ScopeKind::Impl { .. } | ScopeKind::Trait { .. }) {
                let here = Pos { scope, ..pos };
                for glob in [false, true] {
                    let stage = if glob {
                        self.globs(here, name, Ns::Type, 0, pos.module, &mut Vec::new())
                    } else {
                        self.explicit(pos.file, scope, pos.module, name, Ns::Type, 0, None)
                    };
                    match stage {
                        Pick {
                            res: Res::Absent, ..
                        } => {}
                        Pick {
                            conditional: true, ..
                        } => return Res::Unknown,
                        Pick { res, .. } => return res,
                    }
                }
                if matches!(entry.kind, ScopeKind::File | ScopeKind::Module { .. }) {
                    break;
                }
            }
            current = entry.parent;
        }
        Res::Absent
    }

    /// The items and `use`s of a scope binding `name` in `ns`; with
    /// `importer`, only those it can see.
    #[allow(clippy::too_many_arguments)]
    pub fn explicit(
        &self,
        file: usize,
        scope: u32,
        module: ModId,
        name: &str,
        ns: Ns,
        depth: u32,
        importer: Option<ModId>,
    ) -> Pick {
        let source = &self.files[file];
        let Some(entries) = source.names.get(&scope).and_then(|names| names.get(name)) else {
            return Pick::ABSENT;
        };
        let chain = &self.tree.modules[module].cfg;
        let told = Pick::told(entries.iter().map(|entry| match *entry {
            Entry::Item(item) => {
                let declared = &source.facts.items[item as usize];
                let conditional = self.conditional(&declared.cfg, chain);
                if !in_ns(&declared.kind, ns) {
                    return (Res::Absent, conditional);
                }
                match importer.map(|i| self.visible(&declared.vis, module, i)) {
                    Some(Some(false)) => return (Res::Absent, conditional),
                    Some(None) => return (Res::Unknown, conditional),
                    _ => {}
                }
                (self.item_res(file, item, module, depth), conditional)
            }
            Entry::Use(leaf) => {
                let declared = &source.facts.uses[leaf as usize];
                let conditional = self.conditional(&declared.cfg, chain);
                match importer.map(|i| self.visible(&declared.vis, module, i)) {
                    Some(Some(false)) => return (Res::Absent, conditional),
                    Some(None) => return (Res::Unknown, conditional),
                    _ => {}
                }
                (
                    self.use_binding(file, leaf, module, ns, depth + 1),
                    conditional,
                )
            }
        }));
        self.stage(told, || {
            self.explicit(file, scope, module, name, ns, depth, importer)
        })
    }

    /// A stage whose bindings of a name the caller's `#[cfg]`s told apart
    /// answers what it answers where `#[cfg]`s are not read, or nothing
    /// ([`Index::untold`]).
    fn stage(&self, (pick, told): (Pick, bool), again: impl FnOnce() -> Pick) -> Pick {
        match self.untold(pick.res, told, || again().res) {
            res if res == pick.res => pick,
            res => Pick {
                res,
                conditional: false,
            },
        }
    }

    /// What an item is as a name: a module item is the submodule it
    /// declares, `extern crate` the crate it names.
    fn item_res(&self, file: usize, item: u32, module: ModId, depth: u32) -> Res {
        match self.files[file].item_kind(item) {
            Some(ItemKind::Module { .. }) => self.tree.modules[module]
                .children
                .get(&item)
                .map_or(Res::Unknown, |child| Res::Found(Def::Module(*child))),
            Some(ItemKind::ExternCrate { krate }) if krate == "self" => self.crate_root(module),
            Some(ItemKind::ExternCrate { krate }) => {
                if depth > DEPTH {
                    return Res::Unknown;
                }
                match self.extern_crate(module, krate) {
                    Res::Absent => Res::External,
                    other => other,
                }
            }
            Some(_) => Res::Found(Def::Item(file, item)),
            None => Res::Unknown,
        }
    }

    /// The globs of a scope (see `resolve`): `importer` is the module whose
    /// lookup started the search, `seen` the modules already asked. A name
    /// is conditional when the glob or what it provides is.
    ///
    /// Where the caller's `#[cfg]`s told the globs of a stage of the walk
    /// apart, the walk's first stage answers what it answers where
    /// `#[cfg]`s are not read, or nothing ([`Index::untold`]): it asks again
    /// once, and that answer is memoized, where each stage of a chain of
    /// globs asking again would walk the rest of the chain again.
    pub fn globs(
        &self,
        pos: Pos,
        name: &str,
        ns: Ns,
        depth: u32,
        importer: ModId,
        seen: &mut Vec<ModId>,
    ) -> Pick {
        if !seen.is_empty() {
            return self.glob_stage(pos, name, ns, depth, importer, seen);
        }
        let outer = self.told_globs.replace(false);
        let pick = self.glob_stage(pos, name, ns, depth, importer, seen);
        let told = self.told_globs.replace(outer);
        self.stage((pick, told), || {
            self.globs_as_every_caller(pos, name, ns, depth, importer)
        })
    }

    /// [`Index::globs`]'s first stage as every caller answers it, memoized.
    fn globs_as_every_caller(
        &self,
        pos: Pos,
        name: &str,
        ns: Ns,
        depth: u32,
        importer: ModId,
    ) -> Pick {
        let key = (pos.file, pos.scope, pos.module, ns, importer, depth);
        let known = self
            .every_globs
            .borrow()
            .get(&key)
            .and_then(|names| names.get(name))
            .copied();
        if let Some((pick, touched)) = known {
            self.sensitive.set(self.sensitive.get() || touched);
            return pick;
        }
        let (pick, touched) =
            self.tracking(|| self.glob_stage(pos, name, ns, depth, importer, &mut Vec::new()));
        self.every_globs
            .borrow_mut()
            .entry(key)
            .or_default()
            .insert(name.to_owned(), (pick, touched));
        pick
    }

    /// One stage of a glob walk; one the caller's `#[cfg]`s told apart is
    /// noted for the walk's first stage (`told_globs`).
    fn glob_stage(
        &self,
        pos: Pos,
        name: &str,
        ns: Ns,
        depth: u32,
        importer: ModId,
        seen: &mut Vec<ModId>,
    ) -> Pick {
        let source = &self.files[pos.file];
        let Some(globs) = source.globs.get(&pos.scope) else {
            return Pick::ABSENT;
        };
        let chain = &self.tree.modules[pos.module].cfg;
        let unknown = Pick {
            res: Res::Unknown,
            conditional: false,
        };
        let mut results = Vec::new();
        for &leaf in globs {
            let declared = &source.facts.uses[leaf as usize];
            if importer != pos.module {
                match self.visible(&declared.vis, pos.module, importer) {
                    Some(true) => {}
                    Some(false) => continue,
                    None => return unknown,
                }
            }
            let at = Pos {
                scope: declared.scope,
                ..pos
            };
            let provided = match self.use_path(at, &declared.path, Ns::Type, depth + 1) {
                Res::Found(Def::Module(module)) => {
                    self.provides(module, name, ns, depth + 1, importer, seen)
                }
                Res::Found(Def::Item(file, item))
                    if matches!(
                        self.files[file].item_kind(item),
                        Some(ItemKind::Enum { .. })
                    ) =>
                {
                    self.variant(file, item, name)
                }
                _ => return unknown,
            };
            let conditional = self.conditional(&declared.cfg, chain) || provided.conditional;
            results.push((provided.res, conditional));
        }
        let (pick, told) = Pick::told(results);
        if told {
            self.told_globs.set(true);
        }
        pick
    }

    /// The variants of an enum named `name`.
    pub fn variant(&self, file: usize, item: u32, name: &str) -> Pick {
        let source = &self.files[file];
        let Some(variants) = source.variants.get(&item).and_then(|v| v.get(name)) else {
            return Pick::ABSENT;
        };
        let told = Pick::told(variants.iter().map(|v| {
            let declared = &source.facts.variants[*v as usize];
            (
                Res::Found(Def::Variant(file, *v)),
                self.conditional(&declared.cfg, &self.file_cfg[file]),
            )
        }));
        self.stage(told, || self.variant(file, item, name))
    }

    /// The names a module gives a glob import of it from `importer`.
    fn provides(
        &self,
        module: ModId,
        name: &str,
        ns: Ns,
        depth: u32,
        importer: ModId,
        seen: &mut Vec<ModId>,
    ) -> Pick {
        if depth > DEPTH {
            return Pick {
                res: Res::Unknown,
                conditional: false,
            };
        }
        if seen.contains(&module) {
            return Pick::ABSENT;
        }
        seen.push(module);
        self.member_of(module, name, ns, depth, importer, seen)
    }

    /// `name` inside `module`, as a path's segment written in `from` reads
    /// it: only what `from` can see (a private `use` or glob of another
    /// module binds nothing there). A conditional binding is the answer:
    /// nothing comes after a module's own names in a path.
    fn in_module(&self, module: ModId, name: &str, ns: Ns, depth: u32, from: ModId) -> Res {
        self.member_of(module, name, ns, depth, from, &mut Vec::new())
            .res
    }

    /// A module's own items and `use`s binding `name` for `importer`, else
    /// its globs; a conditional own binding is unknown when a glob would
    /// take its place.
    fn member_of(
        &self,
        module: ModId,
        name: &str,
        ns: Ns,
        depth: u32,
        importer: ModId,
        seen: &mut Vec<ModId>,
    ) -> Pick {
        let (file, scope) = {
            let m = &self.tree.modules[module];
            (m.file, m.scope)
        };
        let explicit = || {
            let own = self.explicit(file, scope, module, name, ns, depth, Some(importer));
            Pick::told([
                (own.res, own.conditional),
                self.exported_macro(module, name, ns),
            ])
        };
        let explicit = self.stage(explicit(), || explicit().0);
        let pos = Pos {
            file,
            scope,
            module,
        };
        if explicit.res == Res::Absent {
            return self.globs(pos, name, ns, depth, importer, seen);
        }
        if explicit.conditional
            && self.globs(pos, name, ns, depth, importer, seen).res != Res::Absent
        {
            return Pick {
                res: Res::Unknown,
                conditional: false,
            };
        }
        explicit
    }

    /// A `#[macro_export]` macro is an item of its crate's root.
    pub fn exported_macro(&self, module: ModId, name: &str, ns: Ns) -> (Res, bool) {
        let m = &self.tree.modules[module];
        let Some(krate) = m.krate else {
            return (Res::Absent, false);
        };
        if ns != Ns::Macro || self.tree.crates[krate].root != module {
            return (Res::Absent, false);
        }
        let Some(macros) = self.tree.exported_macros.get(&(krate, name.to_owned())) else {
            return (Res::Absent, false);
        };
        let pick = || {
            Pick::told(macros.iter().map(|(file, item)| {
                let declared = &self.files[*file].facts.items[*item as usize];
                (
                    Res::Found(Def::Item(*file, *item)),
                    self.conditional(&declared.cfg, &self.file_cfg[*file]),
                )
            }))
        };
        let pick = self.stage(pick(), || pick().0);
        (pick.res, pick.conditional)
    }

    /// What a `use` binds in `ns` (memoized). A binding met again while it
    /// is being resolved binds nothing, as Rust never lets one name itself.
    pub fn use_binding(&self, file: usize, leaf: u32, module: ModId, ns: Ns, depth: u32) -> Res {
        let key = (module, file, leaf, ns);
        if let Some(found) = self.recall(&self.memo, key) {
            return found;
        }
        if depth > DEPTH {
            return Res::Unknown;
        }
        if !self.resolving.borrow_mut().insert((key, self.caller_id())) {
            return Res::Absent;
        }
        let declared = &self.files[file].facts.uses[leaf as usize];
        let pos = Pos {
            file,
            scope: declared.scope,
            module,
        };
        let (found, touched) = self.tracking(|| match declared.leaf {
            Leaf::Name => self.use_path(pos, &declared.path, ns, depth + 1),
            Leaf::SelfModule if ns == Ns::Type => self.use_path(pos, &declared.path, ns, depth + 1),
            Leaf::SelfModule | Leaf::Glob => Res::Absent,
        });
        self.resolving.borrow_mut().remove(&(key, self.caller_id()));
        self.remember(&self.memo, key, touched, found);
        found
    }

    /// A path written in code at `pos` (see `resolve`), its last segment in
    /// `ns`.
    pub fn path(&self, pos: Pos, path: &[String], ns: Ns, depth: u32) -> Res {
        self.walk(pos, path, ns, depth, false)
    }

    /// A `use` path at `pos` (see [`Index::lookup`] for what changes).
    pub fn use_path(&self, pos: Pos, path: &[String], ns: Ns, depth: u32) -> Res {
        self.walk(pos, path, ns, depth, true)
    }

    fn walk(&self, pos: Pos, path: &[String], ns: Ns, depth: u32, import: bool) -> Res {
        if depth > DEPTH {
            return Res::Unknown;
        }
        let Some((first, rest)) = path.split_first() else {
            return Res::Unknown;
        };
        let (mut current, rest) = match first.as_str() {
            "" => match rest.split_first() {
                Some((krate, rest)) => (self.extern_crate(pos.module, krate), rest),
                None => return Res::Unknown,
            },
            "crate" => (self.crate_root(pos.module), rest),
            "self" => (Res::Found(Def::Module(pos.module)), rest),
            "super" => {
                let mut module = self.tree.modules[pos.module].parent;
                let mut rest = rest;
                while let Some(("super", tail)) = rest.split_first().map(|(s, t)| (s.as_str(), t)) {
                    module = module.and_then(|m| self.tree.modules[m].parent);
                    rest = tail;
                }
                let found = module.map_or(Res::Unknown, |m| Res::Found(Def::Module(m)));
                (found, rest)
            }
            "Self" => {
                let found = self.self_type(pos, depth + 1);
                if rest.is_empty() && ns == Ns::Value {
                    return self.as_value(found);
                }
                (found, rest)
            }
            name if rest.is_empty() => return self.lookup(pos, name, ns, depth + 1, import),
            name => (self.lookup(pos, name, Ns::Type, depth + 1, import), rest),
        };
        for (index, segment) in rest.iter().enumerate() {
            let Res::Found(def) = current else {
                return match current {
                    Res::Absent => Res::External,
                    other => other,
                };
            };
            let ns = if index + 1 == rest.len() {
                ns
            } else {
                Ns::Type
            };
            current = self.member(pos, def, segment, ns, depth + 1);
        }
        current
    }

    /// `Self` as a value: a tuple or unit struct's constructor.
    fn as_value(&self, found: Res) -> Res {
        match found {
            Res::Found(Def::Item(file, item))
                if matches!(
                    self.files[file].item_kind(item),
                    Some(ItemKind::Struct { value: true, .. })
                ) =>
            {
                found
            }
            Res::Found(_) => Res::Unknown,
            other => other,
        }
    }

    /// `name` inside what a path's previous segment named, the path being
    /// written at `pos`.
    fn member(&self, pos: Pos, def: Def, name: &str, ns: Ns, depth: u32) -> Res {
        if depth > DEPTH {
            return Res::Unknown;
        }
        match def {
            Def::Module(module) => self.in_module(module, name, ns, depth, pos.module),
            Def::Item(file, item) => match self.files[file].item_kind(item) {
                Some(ItemKind::Enum { .. }) => {
                    let variant = self.variant(file, item, name);
                    let assoc = || self.assoc_path(pos, (file, item), name);
                    match variant.res {
                        Res::Absent if ns == Ns::Value => assoc(),
                        Res::Absent => Res::Unknown,
                        _ => variant.alone(
                            ns == Ns::Value && !matches!(assoc(), Res::External | Res::Absent),
                        ),
                    }
                }
                Some(ItemKind::Struct { .. } | ItemKind::Union { .. }) if ns == Ns::Value => {
                    self.assoc_path(pos, (file, item), name)
                }
                Some(ItemKind::Alias { .. }) => match self.type_def(Res::Found(def), depth + 1) {
                    Res::Found(target) if target != def => {
                        self.member(pos, target, name, ns, depth + 1)
                    }
                    Res::Found(_) => Res::Unknown,
                    other => other,
                },
                Some(ItemKind::Trait { .. }) => self.trait_item((file, item), name, false),
                _ => Res::Unknown,
            },
            Def::Variant(..) | Def::Assoc(..) => Res::Unknown,
        }
    }

    /// The root module of `module`'s crate.
    fn crate_root(&self, module: ModId) -> Res {
        self.tree.modules[module]
            .krate
            .map_or(Res::Unknown, |krate| {
                Res::Found(Def::Module(self.tree.crates[krate].root))
            })
    }

    /// A crate name as the crate of `module` sees it in its extern prelude
    /// (D22): its package's own library (not from the library itself nor
    /// from its build script), and the dependencies of its package that
    /// give it that name. A dependency outside the workspace is external;
    /// two that disagree prove neither. `std` and `core` are in every
    /// crate's extern prelude, outside the workspace.
    pub fn extern_crate(&self, module: ModId, name: &str) -> Res {
        if self.tree.modules[module].krate.is_none() {
            return Res::Absent;
        }
        let found = self.crates_named(module, name);
        if found.is_empty() && matches!(name, "std" | "core") {
            return Res::External;
        }
        Res::combine(found)
    }

    /// Whether `name` (`std` or `core`) is the standard library's crate
    /// in the extern prelude of `module`'s crate: no library of its package
    /// and no dependency takes the name.
    pub fn is_std(&self, module: ModId, name: &str) -> bool {
        matches!(name, "std" | "core")
            && self.tree.modules[module].krate.is_some()
            && self.crates_named(module, name).is_empty()
    }

    /// The crates of the workspace and the dependencies `module`'s crate
    /// sees under `name` in its extern prelude.
    fn crates_named(&self, module: ModId, name: &str) -> Vec<Res> {
        let Some(own) = self.tree.modules[module].krate else {
            return Vec::new();
        };
        let own = &self.tree.crates[own];
        let mut found = Vec::new();
        if !matches!(own.target, Target::Lib | Target::Build) {
            for &lib in self.tree.libs.get(name).into_iter().flatten() {
                if self.tree.crates[lib].package == own.package {
                    found.push(Res::Found(Def::Module(self.tree.crates[lib].root)));
                }
            }
        }
        for dependency in self.tree.externs_named(own.package, name) {
            if applies(dependency.kind(), own.target) {
                found.push(dependency.lib().map_or(Res::External, |lib| {
                    Res::Found(Def::Module(self.tree.crates[lib as usize].root))
                }));
            }
        }
        found
    }

    /// Whether an item or `use` of `owner` with `vis` is seen from
    /// `importer`; `None` when the rules cannot tell (a `pub(in path)`
    /// whose path they cannot follow).
    pub fn visible(&self, vis: &Vis, owner: ModId, importer: ModId) -> Option<bool> {
        Some(match vis {
            Vis::Public => true,
            Vis::Crate => {
                let krate = self.tree.modules[owner].krate;
                krate.is_some() && krate == self.tree.modules[importer].krate
            }
            Vis::Super => {
                let parent = self.tree.modules[owner].parent.unwrap_or(owner);
                self.tree.within(importer, parent)
            }
            Vis::Private => self.tree.within(importer, owner),
            Vis::Restricted(path) => self.tree.within(importer, self.restricted_to(path, owner)?),
        })
    }

    /// The module `pub(in path)` on an item of `owner` names (D39): `crate`,
    /// `self` and `super` as in any path, then submodules by name; it must
    /// be `owner` or one of its ancestors, as rustc requires. A path not
    /// starting with one of those is read from the crate root (the 2015
    /// edition; 2018 rejects it).
    fn restricted_to(&self, path: &[String], owner: ModId) -> Option<ModId> {
        let root = || {
            self.tree.modules[owner]
                .krate
                .map(|krate| self.tree.crates[krate].root)
        };
        let (mut module, rest) = match path.split_first()? {
            (first, rest) if first == "crate" => (root()?, rest),
            (first, rest) if first == "self" => (owner, rest),
            (first, rest) if first == "super" => (self.tree.modules[owner].parent?, rest),
            _ => (root()?, path),
        };
        for segment in rest {
            module = if segment == "super" {
                self.tree.modules[module].parent?
            } else {
                let m = &self.tree.modules[module];
                let items = &self.files[m.file].facts.items;
                let named: Vec<ModId> = m
                    .children
                    .iter()
                    .filter(|(item, _)| {
                        items
                            .get(**item as usize)
                            .is_some_and(|i| &i.name == segment)
                    })
                    .map(|(_, child)| *child)
                    .collect();
                match named.as_slice() {
                    [child] => *child,
                    _ => return None,
                }
            };
        }
        self.tree.within(owner, module).then_some(module)
    }

    /// `Self` at `pos`: the type of the impl around it, or the trait in a
    /// trait's body.
    pub fn self_type(&self, pos: Pos, depth: u32) -> Res {
        let scopes = &self.files[pos.file].facts.scopes;
        let mut current = Some(pos.scope);
        while let Some(scope) = current {
            let Some(entry) = scopes.get(scope as usize) else {
                return Res::Unknown;
            };
            match &entry.kind {
                ScopeKind::Impl {
                    self_type,
                    by_ref,
                    blanket,
                    ..
                } => {
                    let Some(layer) = self_type.as_ref().filter(|l| !l.dynamic) else {
                        return Res::Unknown;
                    };
                    if *by_ref || *blanket {
                        return Res::Unknown;
                    }
                    let outer = Pos {
                        scope: entry.parent.unwrap_or(0),
                        ..pos
                    };
                    return self
                        .type_def(self.path(outer, &layer.path, Ns::Type, depth + 1), depth);
                }
                ScopeKind::Trait { item } => return Res::Found(Def::Item(pos.file, *item)),
                ScopeKind::File | ScopeKind::Module { .. } => return Res::Unknown,
                ScopeKind::Block => current = entry.parent,
            }
        }
        Res::Unknown
    }

    /// A type as its definition: an alias followed to the struct, enum,
    /// union or trait it names (aliases of a crate several targets compile
    /// stay unresolved).
    pub fn type_def(&self, found: Res, depth: u32) -> Res {
        let Res::Found(Def::Item(file, item)) = found else {
            return match found {
                Res::Found(_) => Res::Unknown,
                other => other,
            };
        };
        match self.files[file].item_kind(item) {
            Some(
                ItemKind::Struct { .. }
                | ItemKind::Enum { .. }
                | ItemKind::Union { .. }
                | ItemKind::Trait { .. },
            ) => found,
            Some(ItemKind::Alias { target, .. }) => {
                if depth > DEPTH {
                    return Res::Unknown;
                }
                let Some(target) = target.as_ref().filter(|t| !t.dynamic) else {
                    return Res::Unknown;
                };
                let scope = self.files[file].facts.items[item as usize].scope;
                let Some(module) = self.sole_instance(file, scope) else {
                    return Res::Unknown;
                };
                let pos = Pos {
                    file,
                    scope,
                    module,
                };
                self.type_def(self.path(pos, &target.path, Ns::Type, depth + 1), depth + 1)
            }
            _ => Res::Unknown,
        }
    }

    /// A `use` leaf's target: the item it names, or the module (or enum) a
    /// `self` or glob leaf names. A leaf binds a name in each namespace
    /// where its path finds one; the edge points at the one item found,
    /// whatever an unreadable glob may add in another namespace, and two
    /// different items prove neither.
    pub fn import(&self, pos: Pos, path: &[String], leaf: Leaf) -> Res {
        match leaf {
            Leaf::Name => {
                let each =
                    [Ns::Type, Ns::Value, Ns::Macro].map(|ns| self.use_path(pos, path, ns, 0));
                let found = each.iter().filter(|r| matches!(r, Res::Found(_)));
                match Res::combine(found.copied()) {
                    Res::Absent => Res::combine(each),
                    one => one,
                }
            }
            Leaf::SelfModule | Leaf::Glob => match self.use_path(pos, path, Ns::Type, 0) {
                found @ Res::Found(Def::Module(_)) => found,
                found @ Res::Found(Def::Item(file, item))
                    if matches!(
                        self.files[file].item_kind(item),
                        Some(ItemKind::Enum { .. })
                    ) =>
                {
                    found
                }
                Res::Found(_) => Res::Unknown,
                other => other,
            },
        }
    }
}
