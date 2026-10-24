// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The Rust rule file: tree-sitter-rust and the rules carried from v1's
//! `pixel-graph` (ADR-0005, `legacy/v1-reuse.md`): symbols, call sites,
//! argument references and `use` leaves, the receiver typing of method
//! calls, and the resolution of each site on Rust's own module model:
//! crates from `Cargo.toml`, modules from `mod` declarations and the
//! standard layout, items and `use` bindings per scope.
//!
//! # Contract between extraction and resolution
//!
//! Resolution never parses a file. Everything it reads is in the rows
//! extraction produced, possibly in an earlier run:
//!
//! - **Files.** Rust reads every `*.rs` file and every `Cargo.toml`
//!   (`manifest::wants`). A manifest yields one `Package` symbol (the
//!   package name, at the line of its `name` key) and [`Manifest`] facts.
//! - **Symbols.** `fn` → `Function` (`f`), or `Method` when it is an
//!   associated function: `B::r` in `impl a::B<T>` (the impl type's last
//!   path segment, generics dropped), `<B as Tr>::q` in `impl Tr for B`
//!   (Rust's own fully qualified syntax), `Tr::m` in `trait Tr` with or
//!   without a body. `struct` → `Struct`, `enum` → `Enum`, each variant →
//!   `Variant` (`E::V`), `trait` → `Trait`, `const` (associated ones
//!   included) → `Const`, `static` → `Static`, `type` outside an impl or a
//!   trait → `Type`, `macro_rules!` → `Macro`, `mod x { … }` → `Module`.
//!   Every name but a method's and a variant's is unqualified (v1's S-3 to
//!   S-8). `mod x;` is not a symbol: the module is the `File` symbol of its
//!   file. Test code is indexed like any code.
//! - **Sites.** Every site's `context` is a [`Context`]: the [`Scope`] of
//!   the file holding it, which the file's facts turn into the module path,
//!   the blocks and the impl type at the site, and a [`Form`]: the path or
//!   name as written, or the method and what the code states of its
//!   receiver. `src` is the innermost definition enclosing the site (the
//!   `File` symbol at top level); `line` is the line of the callee's name,
//!   of the argument, of the `use` leaf (v1 gave a chain's calls the line
//!   of its first token, K-4).
//!
//!   | kind | form | target |
//!   | --- | --- | --- |
//!   | `Call` | `Path`: `f()`, `Self(x)` | the path on one line (`Vec::<u8>::new`) |
//!   | `Call` | `PathCall`: `a::b::f()`, `T::new()`, `Self::f()`, `Tr::m(&mut x)`, with what the code states of the first argument ([`Argument`]) | as above |
//!   | `Call` | `Qualified`: `<T as Tr>::m()` | the path on one line (`<T as Tr>::m`) |
//!   | `Call` | `Method`: `x.f()`, with what the code states of `x` ([`Receiver`]) | `x.f`, the receiver cut to its last 47 characters past 48 |
//!   | `Call` | `Opaque`: a local or a parameter called by name (a closure), a path rooted at a generic parameter | as above |
//!   | `Reference` | `Path`: a function, constant or constructor passed as an argument (`run(helper)`, `map(Self::wrap)`) | as above |
//!   | `Import` | `Use`: one per `use` leaf | the leaf path without whitespace (`crate::a::b`, `crate::a::self`, `a::*`) |
//!
//!   Calls inside macro token trees are not recorded (v1's K-6). An
//!   argument that names a local, a parameter or a generic parameter is a
//!   value and gives no site. A site the grammar may have misread (inside
//!   an `ERROR` node, or in a function with a parse error, where a lost
//!   `let` would change what a name means) is `Opaque`, and so is a `use`
//!   with an error in it or with groups nested past `types::MAX_NESTING`
//!   (its scope then proves no name it does not bind otherwise). Syntax that
//!   nests without bound (a path's segments, references, pointers, `+`
//!   bounds, `use` groups) is read by loops or up to that cap, never by a
//!   recursion as deep as the input.
//! - **Facts.** A source file's facts are its [`Facts`]: its scopes (inline
//!   modules, blocks that declare items, impls with their self type, generic
//!   arguments included, each parameter with what the impl requires of it
//!   (D43), and trait, traits), the bounds a type declares on its
//!   parameters, the items and `use`
//!   bindings of each scope (with their visibility, the derives of a struct,
//!   an enum or a union, whether a trait declares a type or const
//!   parameter, an alias's target, a `mod`'s `#[path]`), the
//!   variants of each enum and the
//!   associated items of each impl and trait (their visibility; how they take
//!   `self`: by value, `&`, `&mut`, typed; what they return). A definition is
//!   tied to its symbol by its uid without the path (`#name#kind`, `~n` from
//!   the second repetition), which the driver's `extract::uid` builds. A
//!   scope holding names no row records (an `ERROR` node, or a macro invoked
//!   among module, impl or trait items, `m!(…);` among a file's included,
//!   which may define anything) carries a `pub` glob of the empty path,
//!   which resolution can never read, from that module or any other; the
//!   statics `thread_local!` and `lazy_static!` name, when the call holds
//!   no attribute but inert ones, are items of their scope with no symbol
//!   instead, and resolution marks the scope when the name may be another
//!   macro. Every macro invoked where items go is a
//!   [`MacroCall`] (D23), and so is code below the walker's depth cap that
//!   holds an impl, a `macro_rules!` or a module, with the empty path
//!   (D17).
//! - **Targets.** A resolved `Call` points at a `Function`, `Method`,
//!   `Struct` (a tuple or unit struct's constructor) or `Variant`; a
//!   resolved `Reference` at one of those or a `Const` or `Static`; a
//!   resolved `Import` at the item it names, a `Module` symbol for an
//!   inline module, or the `File` symbol of a module's file.
//!
//! # What counts as resolved
//!
//! An edge is resolved only when Rust's own name resolution proves its
//! target (C-10: "a call the rules cannot resolve gives no edge";
//! `CONTEXT.md`: avoid "guessed edge"). A path naming anything outside the
//! workspace (std, a dependency, the prelude) is unresolved; so is a name
//! that two definitions of one module or two methods of one probe step
//! carry (cfg variants, whatever `#[cfg]`s the site carries), a target
//! only some configurations have when another candidate takes its place
//! in the others, a glob that cannot be read, and a method call on a value
//! whose type the code does not state. v1's name-only tiers (kind
//! priority, the only file defining a name, the receiver cap, the sole
//! inherent method, a module path matched by suffix) prove nothing and are
//! not ported. Candidates come from Rust files only. `resolve` lists every
//! rule.
//!
//! A method call follows Rust's method probe (`methods`): inherent methods
//! before trait methods at each autoref step, a workspace trait's methods
//! only where the trait is in scope, which a binding of any name brings it
//! into (renamed, `as _`, re-exported, through a glob; D21). The
//! receiver's own references are not read, so a call resolves only when
//! `T`, `&T` and `&mut T` would all reach the same method.
//!
//! What the rules cannot see, and so may get wrong, is what no row holds:
//! items and impls a macro expands to inside a block (a name a statement
//! macro defines can shadow the item the rules find), the methods an
//! attribute macro of a dependency adds or rewrites, the items a derive of
//! a dependency writes beside its type, such as an impl of another type or
//! a type that shadows a glob's (a macro of the workspace leaves the
//! probes it may change unresolved, a derive that is not std's the probes
//! on its own type, `macros`), the item an attribute macro of the
//! workspace applies to, read as written, a dependency's macro named
//! `thread_local` or `lazy_static`, read as std's or the `lazy_static`
//! crate's, a trait outside the
//! workspace in scope at a method call whose
//! impl for the type no row holds (a dependency's blanket impl, such as
//! `Itertools` for every iterator), which may compete or win at an earlier
//! step of the probe (an impl the rows hold of a trait outside the
//! workspace and the std prelude is taken only where such a trait may be in
//! scope, and only when nothing else provides the name), the default methods an
//! impl of a trait the rules cannot name gives its type, and the 2015
//! edition's `use` paths, which are read as 2018's.

mod args;
mod bindings;
mod cfg;
mod extract;
mod lookup;
mod macros;
mod manifest;
mod methods;
mod modules;
mod resolve;
mod toml;
mod types;
mod uses;

use pixel_model::Lang;

use crate::codec;
use crate::lang::{Extraction, Rules, World};

/// Bumped by any change to the grammar or these rules.
pub const VERSION: u32 = 4;

pub struct Rust;

impl Rules for Rust {
    fn lang(&self) -> Lang {
        Lang::Rust
    }

    fn version(&self) -> u32 {
        VERSION
    }

    fn wants(&self, path: &str) -> bool {
        manifest::wants(path)
    }

    fn accepts(&self, path: &str, content: &[u8]) -> bool {
        manifest::accepts(path, content)
    }

    fn parse(&self, _path: &str, content: &[u8]) -> Option<tree_sitter::Tree> {
        extract::parse_within(content, crate::guard::PARSE_BUDGET)
    }

    fn extract(&self, path: &str, content: &[u8]) -> Extraction {
        if manifest::is_manifest(path) {
            manifest::extract(content)
        } else {
            extract::extract(content)
        }
    }

    fn resolve(&self, world: &World<'_>) -> Vec<Option<usize>> {
        resolve::resolve(world)
    }
}

/// What a Rust site's `context` holds (see the table above).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    /// The innermost scope of the file holding the site, an index into
    /// [`Facts::scopes`]: it gives the module, the blocks and the impl
    /// around the site.
    pub scope: u32,
    pub form: Form,
    /// The `#[cfg]` conjuncts the site sits under in its file: those of
    /// every item, module, impl and statement around it (F2).
    pub cfg: Vec<String>,
}

/// The shape of a site, as resolution reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Form {
    /// A path, generic arguments dropped: `["a", "b", "f"]`; `[""]` first
    /// for a leading `::`.
    Path(Vec<String>),
    /// `<T as Tr>::rest` (`trait_path` empty for `<T>::rest`).
    Qualified {
        /// `T`, or `U` of `&U` and `&mut U` (`by_ref`): its outer path as a
        /// layer, none when it is no path (a primitive, a tuple, a pointer,
        /// `&&U`, `dyn Tr`). `None` when it is a generic parameter of the
        /// site or built on one: the trait's own item is what the code
        /// names (D20).
        ty: Option<TypeRef>,
        /// `T` is `&U` (`Some(false)`) or `&mut U` (`Some(true)`).
        by_ref: Option<bool>,
        trait_path: Vec<String>,
        /// The generic arguments written on `Tr`, as `Layer::args` (D20).
        trait_args: Vec<String>,
        rest: Vec<String>,
    },
    /// A call of a path of two segments or more, and what the code states
    /// of its first argument, which gives `Self` when the path names a
    /// trait's method (`Tr::m(&mut x)`).
    PathCall { path: Vec<String>, first: Argument },
    /// A method call `receiver.name(…)`.
    Method { name: String, receiver: Receiver },
    /// Nothing an item could be: a local or parameter called by name, a path
    /// rooted at a generic parameter, a callee shape the rules do not read.
    Opaque,
    /// One `use` leaf: the path it names (without the trailing `self` or
    /// `*`) and what it binds.
    Use { path: Vec<String>, leaf: Leaf },
}

/// What the code states about the value a method is called on (v1's
/// receiver typing, `extract.rs:1459-1636`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Receiver {
    /// `self`: the impl's type, or the trait in its own body.
    SelfValue,
    /// A local or parameter whose binding states its type.
    Type(TypeRef),
    /// `T::f(…)`, as the receiver or the value of the local's `let`: the
    /// type when the function returns `Self` or `T`; when `unwrapped`
    /// (`T::f(…)?`, `T::f(…).unwrap()`, `T::f(…).expect(…)`), when it
    /// returns std's `Result` or `Option` of `Self` or `T`.
    Ctor {
        scope: u32,
        path: Vec<String>,
        method: String,
        unwrapped: bool,
    },
    /// `T { … }`, as the receiver or the value of the local's `let`.
    Literal { scope: u32, path: Vec<String> },
    /// Nothing states it.
    Unknown,
}

/// What the code states of a path call's first argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Argument {
    /// Its type, as a method's receiver's (a stated type, a constructor's,
    /// a literal's, `self`), behind the references written on the argument
    /// and on its binding's type, outermost first (`true` for `&mut`).
    Typed { refs: Vec<bool>, receiver: Receiver },
    /// A value whose binding's type is a generic parameter of the code
    /// around it, behind references or not.
    Generic,
    /// None, or nothing states its type.
    Unknown,
}

/// A type as written, read where it is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeRef {
    /// The scope the type is written in.
    pub scope: u32,
    /// The outer path type, then, when it is `Box`, `Rc` or `Arc`, the type
    /// it points to, and so on. References are already unwrapped.
    pub layers: Vec<Layer>,
}

/// One path type: `a::B` of `&a::B<T>`, or the trait of `dyn Tr` and
/// `impl Tr` (`dynamic`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layer {
    pub path: Vec<String>,
    pub dynamic: bool,
    /// The generic arguments written on a type's last segment (D43), each
    /// on one line (`types::squash`): `_` for a generic parameter of the
    /// items around it, `?` for a type built on one or left to inference
    /// (`G<_>`); lifetimes are left out. On an impl's self type, a
    /// parameter of the impl held to bounds is `types::BOUNDED` and them,
    /// one held to more `types::CONSTRAINED`.
    pub args: Vec<String>,
}

/// What a `use` leaf binds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leaf {
    /// The item the path names, under its own name or an alias.
    Name,
    /// `a::{self}`: the module `a`, in the type namespace only.
    SelfModule,
    /// `a::*`.
    Glob,
}

/// The declarations of one source file that resolution reads back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// Scope 0 is the file.
    pub scopes: Vec<Scope>,
    pub items: Vec<Item>,
    pub variants: Vec<Variant>,
    pub assocs: Vec<Assoc>,
    pub uses: Vec<Use>,
    /// The macros invoked where items go (D23), and code below the
    /// walker's depth cap holding an impl, a `macro_rules!` or a module
    /// (D17), which may define as much.
    pub macro_calls: Vec<MacroCall>,
    /// The bounds the structs, enums and unions with bounded parameters
    /// declare (D43).
    pub bounds: Vec<Declared>,
    /// The procedural macros the file's functions define (D23).
    pub proc_macros: Vec<ProcMacro>,
    /// The attributes on items (of a module, an impl, a trait or a block)
    /// that are not Rust's own: a macro of the workspace among them may
    /// write anything (D23).
    pub attributes: Vec<AttributeUse>,
}

/// A procedural macro a function defines, by the name it is invoked by
/// (D23): a crate depending on its crate may invoke it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcMacro {
    pub kind: ProcKind,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcKind {
    /// `#[proc_macro]`: `name!(…)`.
    Bang,
    /// `#[proc_macro_derive(Name)]`: `#[derive(Name)]`.
    Derive,
    /// `#[proc_macro_attribute]`: `#[name]` on an item.
    Attribute,
}

/// An attribute on an item (D23): the last segment of its path, in the
/// scope holding the item, under the `#[cfg]`s around it;
/// [`UNREAD_NAME`] for one the rules cannot read.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AttributeUse {
    pub scope: u32,
    pub name: String,
    pub cfg: Vec<String>,
}

/// The name extraction gives an attribute or a derive it cannot read, and
/// a derive macro whose name it cannot read (D23): it may be any macro's.
/// No identifier is written so.
pub const UNREAD_NAME: &str = "?";

/// The bounds a struct, enum or union (`item`) declares on each of its type
/// and const parameters, in order, as `bindings::declared_bounds` reads
/// them (`?Sized` among them): every value of the type meets them, so an
/// impl requiring no more of a parameter is for every argument there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declared {
    pub item: u32,
    pub bounds: Vec<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    /// `None` for the file.
    pub parent: Option<u32>,
    pub kind: ScopeKind,
    /// The `#[cfg]` conjuncts of the scope in its file (see [`Item::cfg`]).
    pub cfg: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    File,
    /// The body of `mod x { … }`, the item at index `item`.
    Module {
        item: u32,
    },
    /// A block that declares items or `use`s.
    Block,
    /// An impl block; its associated items are [`Assoc`]s of this scope.
    Impl {
        /// The self type's path, `None` when it is not a path, or a generic
        /// parameter of the impl (`blanket`).
        self_type: Option<Layer>,
        /// `impl Tr for &T` or `impl Tr for &mut T`.
        by_ref: bool,
        /// `impl Tr for &mut T` (D20).
        mutable: bool,
        /// `impl<T> Tr for T`.
        blanket: bool,
        /// Empty for an inherent impl.
        trait_path: Vec<String>,
        /// The generic arguments written on the trait, as the self type's
        /// (D20): a parameter the self type also names is
        /// `types::CONSTRAINED`.
        trait_args: Vec<String>,
    },
    /// A trait's body; `item` is the trait.
    Trait {
        item: u32,
    },
}

/// An item declared in a module or block scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub scope: u32,
    pub name: String,
    /// The symbol's uid without the path; empty when the item is no symbol.
    pub uid: String,
    pub vis: Vis,
    pub kind: ItemKind,
    /// The `#[cfg]` conjuncts the item carries in its file: its own, its
    /// impl's or trait's, its enclosing modules', blocks' and statements'
    /// (F2); `cfg::FALSE` alone when it never exists.
    pub cfg: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Fn,
    Struct {
        /// A tuple or unit struct: its name is also a value.
        value: bool,
        derives: Vec<String>,
        /// A plain `#[derive(…)]` names `Default`, so `T::default()` is
        /// `T` (`methods::constructed`). One a `cfg_attr` applies proves
        /// nothing: its predicate may be false, and `default` another
        /// trait's.
        plain_default: bool,
    },
    Enum {
        derives: Vec<String>,
        /// As a struct's.
        plain_default: bool,
    },
    Union {
        derives: Vec<String>,
    },
    Trait {
        /// Whether the trait declares a type or const parameter, whose
        /// argument a path to its method infers (D20).
        generic: bool,
    },
    Const,
    Static,
    /// `type A = …;`. On `target`, an argument that is the alias's own
    /// parameter `k` is written `$k` (`types::alias_args`), so that what is
    /// written on the alias can take its place (D43).
    Alias {
        target: Option<Layer>,
        /// How many type and const parameters the alias declares.
        params: u32,
    },
    Macro {
        /// `#[macro_export]`: the macro is an item of the crate root.
        exported: bool,
    },
    Module {
        /// The body's scope for `mod x { … }`; `None` for `mod x;`.
        body: Option<u32>,
        /// Its `#[path]`: for `mod x;` the file, for `mod x { … }` the
        /// directory of the files its own `mod y;` name.
        path: ModPath,
    },
    /// `extern crate krate as name;`.
    ExternCrate {
        krate: String,
    },
    /// A function or static of an `extern` block: a value with no symbol.
    Foreign,
}

/// A macro invoked where items go (D23): among the items of a module, an
/// impl or a trait, which may expand to impls. A file records each once.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MacroCall {
    pub scope: u32,
    /// The macro's path as written; empty for code below the depth cap.
    pub path: Vec<String>,
    pub cfg: Vec<String>,
    /// Named `thread_local` or `lazy_static`, holding no attribute but
    /// inert ones: std's or the `lazy_static` crate's, which define statics
    /// (`lazy_static!` a struct of each one's name too) and write no impl
    /// of a type of the workspace, unless resolution finds the name may be
    /// another macro.
    pub statics: bool,
}

/// A `mod`'s `#[path]` attribute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModPath {
    /// None: the module is where the layout says.
    Layout,
    /// `#[path = "…"]`, a plain string.
    Literal(String),
    /// One the rules cannot read: `cfg_attr(…, path = …)`, a string with
    /// escapes, a raw string.
    Unknown,
}

impl ModPath {
    fn encode(&self) -> String {
        match self {
            ModPath::Layout => String::new(),
            ModPath::Literal(path) => format!("={path}"),
            ModPath::Unknown => "?".to_owned(),
        }
    }

    fn decode(text: &str) -> ModPath {
        match (text, text.strip_prefix('=')) {
            ("", _) => ModPath::Layout,
            (_, Some(path)) => ModPath::Literal(path.to_owned()),
            _ => ModPath::Unknown,
        }
    }
}

/// A variant of the enum at `item`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variant {
    pub item: u32,
    pub name: String,
    pub uid: String,
    pub cfg: Vec<String>,
}

/// An associated function or constant of the impl or trait scope `owner`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assoc {
    pub owner: u32,
    pub name: String,
    pub uid: String,
    /// As written: what an inherent impl's item shows outside the impl's
    /// module (D19); an item of a trait or a trait impl has none and is
    /// seen wherever the trait is.
    pub vis: Vis,
    pub kind: AssocKind,
    pub cfg: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssocKind {
    Fn {
        receiver: SelfKind,
        /// The return type's outer path (`Self`, `Store`), when it is one;
        /// none for an `async fn`, which returns a future of it.
        returns: Option<Layer>,
        /// `false` for a trait method without a default body.
        body: bool,
    },
    Const,
}

/// How an associated function takes `self`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelfKind {
    /// No `self`: callable as a path only.
    None,
    /// `self`, `mut self`, `self: Self`.
    Value,
    /// `&self`, `self: &Self`.
    Ref,
    /// `&mut self`, `self: &mut Self`: Rust's method probe tries `&T`
    /// before `&mut T`, so it is told from `Ref`.
    RefMut,
    /// `self: Box<Self>` and other typed forms.
    Typed,
}

/// One `use` leaf, as the scope it sits in binds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Use {
    pub scope: u32,
    pub vis: Vis,
    pub path: Vec<String>,
    pub leaf: Leaf,
    /// The name it binds; `None` for a glob, `_`, or a leaf naming no
    /// item.
    pub local: Option<String>,
    pub cfg: Vec<String>,
}

/// An item's or a `use`'s visibility, as glob imports read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Vis {
    Private,
    Public,
    Crate,
    Super,
    /// `pub(in path)`: the path as written, empty when the rules cannot
    /// read it.
    Restricted(Vec<String>),
}

/// What a `Cargo.toml` declares (read by `manifest::read`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    /// `[package] name`; `None` for a virtual workspace manifest.
    pub package: Option<String>,
    /// `[lib] name`.
    pub lib_name: Option<String>,
    /// `[lib] path`, relative to the manifest.
    pub lib_path: Option<String>,
    /// `[[bin]]` tables: `name` and `path`, each empty when absent.
    pub bins: Vec<(String, String)>,
    /// `[package] workspace`: the workspace root's directory, relative to
    /// the manifest, when it is not the nearest one above.
    pub workspace_root: Option<String>,
    /// A `[workspace]` table: this manifest is a workspace root.
    pub workspace: bool,
    /// The package's dependencies, of every table: `[dependencies]`,
    /// `[dev-dependencies]`, `[build-dependencies]` and their
    /// `[target.….…]` forms.
    pub dependencies: Vec<Dependency>,
    /// `[workspace.dependencies]`, which a member inherits with
    /// `workspace = true`.
    pub workspace_dependencies: Vec<Dependency>,
}

/// One dependency of a manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dependency {
    pub kind: DepKind,
    /// Its key: the name the package gives it.
    pub key: String,
    /// `package = "…"`: the dependency is renamed, and this is the package
    /// it names.
    pub package: Option<String>,
    pub source: DepSource,
}

/// A dependency as a manifest's facts hold it, its text borrowed:
/// resolution reads a workspace's, which may be millions, one at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DependencyRef<'t> {
    pub kind: DepKind,
    pub key: &'t str,
    pub package: Option<&'t str>,
    pub source: DepSourceRef<'t>,
}

/// [`DepSource`], its path borrowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepSourceRef<'t> {
    Path(&'t str),
    Workspace,
    Other,
}

impl<'t> DepSourceRef<'t> {
    /// A record's source field and path: `None` for a path's without one.
    fn read(source: Option<&str>, path: Option<&'t str>) -> Option<DepSourceRef<'t>> {
        match (source, path) {
            (Some("path"), Some(path)) => Some(DepSourceRef::Path(path)),
            (Some("path"), None) => None,
            (Some("w" | "workspace"), _) => Some(DepSourceRef::Workspace),
            _ => Some(DepSourceRef::Other),
        }
    }
}

/// The table a dependency is declared in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepKind {
    Normal,
    Dev,
    Build,
}

/// Where a dependency comes from, as far as resolution needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DepSource {
    /// `path = "…"`, relative to the manifest declaring it.
    Path(String),
    /// `workspace = true`: the workspace root's dependency of that key.
    Workspace,
    /// A registry, a git repository, or what the reader cannot read: never
    /// a package of the workspace.
    Other,
}

// --- Encoding ------------------------------------------------------------

fn join(path: &[String]) -> String {
    path.join("::")
}

fn split(text: &str) -> Vec<String> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.split("::").map(str::to_owned).collect()
    }
}

fn number(text: &str) -> Option<u32> {
    text.parse().ok()
}

impl Layer {
    /// The path, then `<` and the arguments when there are any: no path
    /// segment holds a `<`.
    fn encode(&self) -> String {
        let mut out = if self.dynamic {
            format!("dyn:{}", join(&self.path))
        } else {
            join(&self.path)
        };
        if !self.args.is_empty() {
            out.push('<');
            out.push_str(&codec::record(&self.args));
        }
        out
    }

    fn decode(text: &str) -> Option<Layer> {
        if text.is_empty() {
            return None;
        }
        let (text, args) = match text.split_once('<') {
            Some((path, args)) => (path, codec::fields(args)),
            None => (text, Vec::new()),
        };
        let (path, dynamic) = match text.strip_prefix("dyn:") {
            Some(path) => (path, true),
            None => (text, false),
        };
        Some(Layer {
            path: split(path),
            dynamic,
            args,
        })
    }
}

fn encode_layer(layer: Option<&Layer>) -> String {
    layer.map(Layer::encode).unwrap_or_default()
}

impl TypeRef {
    fn encode(&self) -> String {
        let mut fields = vec![self.scope.to_string()];
        fields.extend(self.layers.iter().map(Layer::encode));
        codec::record(&fields)
    }

    fn decode(text: &str) -> Option<TypeRef> {
        let fields = codec::fields(text);
        let (scope, layers) = fields.split_first()?;
        Some(TypeRef {
            scope: number(scope)?,
            layers: layers
                .iter()
                .map(|l| Layer::decode(l))
                .collect::<Option<_>>()?,
        })
    }
}

impl Receiver {
    fn encode(&self) -> String {
        let fields: Vec<String> = match self {
            Receiver::SelfValue => vec!["self".to_owned()],
            Receiver::Type(ty) => vec!["type".to_owned(), ty.encode()],
            Receiver::Ctor {
                scope,
                path,
                method,
                unwrapped,
            } => vec![
                "ctor".to_owned(),
                scope.to_string(),
                join(path),
                method.clone(),
                if *unwrapped { "?" } else { "" }.to_owned(),
            ],
            Receiver::Literal { scope, path } => {
                vec!["literal".to_owned(), scope.to_string(), join(path)]
            }
            Receiver::Unknown => vec!["unknown".to_owned()],
        };
        codec::record(&fields)
    }

    fn decode(text: &str) -> Option<Receiver> {
        let fields = codec::fields(text);
        let field = |i: usize| fields.get(i).map(String::as_str);
        Some(match field(0)? {
            "self" => Receiver::SelfValue,
            "type" => Receiver::Type(TypeRef::decode(field(1)?)?),
            "ctor" => Receiver::Ctor {
                scope: number(field(1)?)?,
                path: split(field(2)?),
                method: field(3)?.to_owned(),
                unwrapped: match field(4)? {
                    "?" => true,
                    "" => false,
                    _ => return None,
                },
            },
            "literal" => Receiver::Literal {
                scope: number(field(1)?)?,
                path: split(field(2)?),
            },
            "unknown" => Receiver::Unknown,
            _ => return None,
        })
    }
}

impl Argument {
    fn encode(&self) -> String {
        let fields: Vec<String> = match self {
            Argument::Typed { refs, receiver } => vec![
                "typed".to_owned(),
                refs.iter()
                    .map(|mutable| if *mutable { 'm' } else { 'r' })
                    .collect(),
                receiver.encode(),
            ],
            Argument::Generic => vec!["generic".to_owned()],
            Argument::Unknown => vec!["unknown".to_owned()],
        };
        codec::record(&fields)
    }

    fn decode(text: &str) -> Option<Argument> {
        let fields = codec::fields(text);
        let field = |i: usize| fields.get(i).map(String::as_str);
        Some(match field(0)? {
            "typed" => Argument::Typed {
                refs: field(1)?.chars().map(|c| c == 'm').collect(),
                receiver: Receiver::decode(field(2)?)?,
            },
            "generic" => Argument::Generic,
            "unknown" => Argument::Unknown,
            _ => return None,
        })
    }
}

impl Leaf {
    fn encode(self) -> &'static str {
        match self {
            Leaf::Name => "name",
            Leaf::SelfModule => "self",
            Leaf::Glob => "glob",
        }
    }

    fn decode(text: &str) -> Option<Leaf> {
        Some(match text {
            "name" => Leaf::Name,
            "self" => Leaf::SelfModule,
            "glob" => Leaf::Glob,
            _ => return None,
        })
    }
}

impl Context {
    pub fn encode(&self) -> String {
        let (tag, form): (&str, Vec<String>) = match &self.form {
            Form::Path(path) => ("path", vec![join(path)]),
            Form::Qualified {
                ty,
                by_ref,
                trait_path,
                trait_args,
                rest,
            } => (
                "qualified",
                vec![
                    ty.as_ref().map(TypeRef::encode).unwrap_or_default(),
                    join(trait_path),
                    join(rest),
                    match by_ref {
                        None => "",
                        Some(false) => "&",
                        Some(true) => "&mut",
                    }
                    .to_owned(),
                    codec::record(trait_args),
                ],
            ),
            Form::PathCall { path, first } => ("pathcall", vec![join(path), first.encode()]),
            Form::Method { name, receiver } => ("method", vec![name.clone(), receiver.encode()]),
            Form::Opaque => ("opaque", Vec::new()),
            Form::Use { path, leaf } => ("use", vec![join(path), leaf.encode().to_owned()]),
        };
        let mut fields = vec![tag.to_owned(), self.scope.to_string(), conjuncts(&self.cfg)];
        fields.extend(form);
        codec::record(&fields)
    }

    /// `None` for text this version did not write.
    pub fn decode(text: &str) -> Option<Context> {
        let fields = codec::fields(text);
        let field = |i: usize| fields.get(i).map(String::as_str);
        let scope = number(field(1)?)?;
        let cfg = decode_conjuncts(field(2)?);
        let form = match field(0)? {
            "path" => Form::Path(split(field(3)?)),
            "qualified" => Form::Qualified {
                ty: match field(3)? {
                    "" => None,
                    text => Some(TypeRef::decode(text)?),
                },
                by_ref: match field(6).unwrap_or_default() {
                    "&" => Some(false),
                    "&mut" => Some(true),
                    _ => None,
                },
                trait_path: split(field(4)?),
                trait_args: decode_list(field(7).unwrap_or_default()),
                rest: split(field(5)?),
            },
            "pathcall" => Form::PathCall {
                path: split(field(3)?),
                first: Argument::decode(field(4)?)?,
            },
            "method" => Form::Method {
                name: field(3)?.to_owned(),
                receiver: Receiver::decode(field(4)?)?,
            },
            "opaque" => Form::Opaque,
            "use" => Form::Use {
                path: split(field(3)?),
                leaf: Leaf::decode(field(4)?)?,
            },
            _ => return None,
        };
        Some(Context { scope, form, cfg })
    }
}

/// A list of `#[cfg]` conjuncts as one field.
fn conjuncts(cfg: &[String]) -> String {
    if cfg.is_empty() {
        String::new()
    } else {
        codec::record(cfg)
    }
}

fn decode_conjuncts(text: &str) -> Vec<String> {
    if text.is_empty() {
        Vec::new()
    } else {
        codec::fields(text)
    }
}

impl Vis {
    fn encode(&self) -> String {
        match self {
            Vis::Private => String::new(),
            Vis::Public => "pub".to_owned(),
            Vis::Crate => "crate".to_owned(),
            Vis::Super => "super".to_owned(),
            Vis::Restricted(path) => format!("in:{}", join(path)),
        }
    }

    fn decode(text: &str) -> Vis {
        match text {
            "pub" => Vis::Public,
            "crate" => Vis::Crate,
            "super" => Vis::Super,
            "" => Vis::Private,
            other => Vis::Restricted(split(other.strip_prefix("in:").unwrap_or_default())),
        }
    }
}

impl SelfKind {
    fn encode(self) -> &'static str {
        match self {
            SelfKind::None => "",
            SelfKind::Value => "value",
            SelfKind::Ref => "ref",
            SelfKind::RefMut => "mut",
            SelfKind::Typed => "typed",
        }
    }

    fn decode(text: &str) -> SelfKind {
        match text {
            "value" => SelfKind::Value,
            "ref" => SelfKind::Ref,
            "mut" => SelfKind::RefMut,
            "typed" => SelfKind::Typed,
            _ => SelfKind::None,
        }
    }
}

fn flag(on: bool) -> String {
    if on { "1" } else { "" }.to_owned()
}

fn optional(index: Option<u32>) -> String {
    index.map(|i| i.to_string()).unwrap_or_default()
}

impl Facts {
    pub fn encode(&self) -> String {
        let mut records: Vec<Vec<String>> = Vec::new();
        for scope in &self.scopes {
            let mut record = vec![
                "scope".to_owned(),
                optional(scope.parent),
                conjuncts(&scope.cfg),
            ];
            match &scope.kind {
                ScopeKind::File => record.push("file".to_owned()),
                ScopeKind::Module { item } => {
                    record.extend(["module".to_owned(), item.to_string()]);
                }
                ScopeKind::Block => record.push("block".to_owned()),
                ScopeKind::Impl {
                    self_type,
                    by_ref,
                    mutable,
                    blanket,
                    trait_path,
                    trait_args,
                } => record.extend([
                    "impl".to_owned(),
                    encode_layer(self_type.as_ref()),
                    match (by_ref, mutable) {
                        (true, true) => "mut".to_owned(),
                        (by_ref, _) => flag(*by_ref),
                    },
                    flag(*blanket),
                    join(trait_path),
                    codec::record(trait_args),
                ]),
                ScopeKind::Trait { item } => {
                    record.extend(["trait".to_owned(), item.to_string()]);
                }
            }
            records.push(record);
        }
        for item in &self.items {
            let mut record = vec![
                "item".to_owned(),
                item.scope.to_string(),
                item.name.clone(),
                item.uid.clone(),
                item.vis.encode(),
                conjuncts(&item.cfg),
            ];
            match &item.kind {
                ItemKind::Fn => record.push("fn".to_owned()),
                ItemKind::Struct {
                    value,
                    derives,
                    plain_default,
                } => record.extend([
                    "struct".to_owned(),
                    flag(*value),
                    codec::record(derives),
                    flag(*plain_default),
                ]),
                ItemKind::Enum {
                    derives,
                    plain_default,
                } => {
                    record.extend([
                        "enum".to_owned(),
                        codec::record(derives),
                        flag(*plain_default),
                    ]);
                }
                ItemKind::Union { derives } => {
                    record.extend(["union".to_owned(), codec::record(derives)]);
                }
                ItemKind::Trait { generic } => {
                    record.extend(["trait".to_owned(), flag(*generic)]);
                }
                ItemKind::Const => record.push("const".to_owned()),
                ItemKind::Static => record.push("static".to_owned()),
                ItemKind::Alias { target, params } => {
                    record.extend([
                        "alias".to_owned(),
                        encode_layer(target.as_ref()),
                        params.to_string(),
                    ]);
                }
                ItemKind::Macro { exported } => {
                    record.extend(["macro".to_owned(), flag(*exported)]);
                }
                ItemKind::Module { body, path } => {
                    record.extend(["mod".to_owned(), optional(*body), path.encode()]);
                }
                ItemKind::ExternCrate { krate } => {
                    record.extend(["crate".to_owned(), krate.clone()]);
                }
                ItemKind::Foreign => record.push("foreign".to_owned()),
            }
            records.push(record);
        }
        for variant in &self.variants {
            records.push(vec![
                "variant".to_owned(),
                variant.item.to_string(),
                variant.name.clone(),
                variant.uid.clone(),
                conjuncts(&variant.cfg),
            ]);
        }
        for assoc in &self.assocs {
            let mut record = vec![
                "assoc".to_owned(),
                assoc.owner.to_string(),
                assoc.name.clone(),
                assoc.uid.clone(),
                conjuncts(&assoc.cfg),
                assoc.vis.encode(),
            ];
            match &assoc.kind {
                AssocKind::Fn {
                    receiver,
                    returns,
                    body,
                } => record.extend([
                    "fn".to_owned(),
                    receiver.encode().to_owned(),
                    encode_layer(returns.as_ref()),
                    flag(*body),
                ]),
                AssocKind::Const => record.push("const".to_owned()),
            }
            records.push(record);
        }
        for call in &self.macro_calls {
            records.push(vec![
                "macrocall".to_owned(),
                call.scope.to_string(),
                join(&call.path),
                conjuncts(&call.cfg),
                flag(call.statics),
            ]);
        }
        for defined in &self.proc_macros {
            let kind = match defined.kind {
                ProcKind::Bang => "bang",
                ProcKind::Derive => "derive",
                ProcKind::Attribute => "attribute",
            };
            records.push(vec![
                "procmacro".to_owned(),
                kind.to_owned(),
                defined.name.clone(),
            ]);
        }
        for used in &self.attributes {
            records.push(vec![
                "attribute".to_owned(),
                used.scope.to_string(),
                used.name.clone(),
                conjuncts(&used.cfg),
            ]);
        }
        for declared in &self.bounds {
            let mut record = vec!["bounds".to_owned(), declared.item.to_string()];
            record.extend(declared.bounds.iter().map(|b| codec::record(b)));
            records.push(record);
        }
        for leaf in &self.uses {
            records.push(vec![
                "use".to_owned(),
                leaf.scope.to_string(),
                leaf.vis.encode(),
                join(&leaf.path),
                leaf.leaf.encode().to_owned(),
                leaf.local.clone().unwrap_or_default(),
                conjuncts(&leaf.cfg),
            ]);
        }
        codec::records(&records)
    }

    /// What can be read of `text`: a malformed record is skipped.
    pub fn decode(text: &str) -> Facts {
        let mut facts = Facts::default();
        for record in codec::list(text) {
            match record.first().map(String::as_str) {
                Some("scope") => facts.scopes.extend(decode_scope(&record)),
                Some("item") => facts.items.extend(decode_item(&record)),
                Some("variant") => facts.variants.extend(decode_variant(&record)),
                Some("assoc") => facts.assocs.extend(decode_assoc(&record)),
                Some("use") => facts.uses.extend(decode_use(&record)),
                Some("macrocall") => facts.macro_calls.extend(decode_macro_call(&record)),
                Some("bounds") => facts.bounds.extend(decode_declared(&record)),
                Some("procmacro") => {
                    let kind = match record.get(1).map(String::as_str) {
                        Some("bang") => Some(ProcKind::Bang),
                        Some("derive") => Some(ProcKind::Derive),
                        Some("attribute") => Some(ProcKind::Attribute),
                        _ => None,
                    };
                    if let (Some(kind), Some(name)) = (kind, record.get(2)) {
                        facts.proc_macros.push(ProcMacro {
                            kind,
                            name: name.clone(),
                        });
                    }
                }
                Some("attribute") => {
                    let scope = record.get(1).and_then(|text| number(text));
                    if let (Some(scope), Some(name), Some(cfg)) =
                        (scope, record.get(2), record.get(3))
                    {
                        facts.attributes.push(AttributeUse {
                            scope,
                            name: name.clone(),
                            cfg: decode_conjuncts(cfg),
                        });
                    }
                }
                _ => {}
            }
        }
        if facts.scopes.is_empty() {
            facts.scopes.push(Scope {
                parent: None,
                kind: ScopeKind::File,
                cfg: Vec::new(),
            });
        }
        facts
    }
}

fn decode_scope(record: &[String]) -> Option<Scope> {
    let field = |i: usize| record.get(i).map(String::as_str);
    let parent = match field(1)? {
        "" => None,
        text => Some(number(text)?),
    };
    let cfg = decode_conjuncts(field(2)?);
    let kind = match field(3)? {
        "file" => ScopeKind::File,
        "module" => ScopeKind::Module {
            item: number(field(4)?)?,
        },
        "block" => ScopeKind::Block,
        "impl" => ScopeKind::Impl {
            self_type: Layer::decode(field(4)?),
            by_ref: !field(5)?.is_empty(),
            mutable: field(5)? == "mut",
            blanket: !field(6)?.is_empty(),
            trait_path: split(field(7)?),
            trait_args: decode_list(field(8).unwrap_or_default()),
        },
        "trait" => ScopeKind::Trait {
            item: number(field(4)?)?,
        },
        _ => return None,
    };
    Some(Scope { parent, kind, cfg })
}

fn decode_item(record: &[String]) -> Option<Item> {
    let field = |i: usize| record.get(i).map(String::as_str);
    let list = |i: usize| -> Option<Vec<String>> {
        let text = field(i)?;
        Some(if text.is_empty() {
            Vec::new()
        } else {
            codec::fields(text)
        })
    };
    let kind = match field(6)? {
        "fn" => ItemKind::Fn,
        // A record without the flag reads as no plain `Default`, which
        // proves less.
        "struct" => ItemKind::Struct {
            value: !field(7)?.is_empty(),
            derives: list(8)?,
            plain_default: field(9).is_some_and(|flag| !flag.is_empty()),
        },
        "enum" => ItemKind::Enum {
            derives: list(7)?,
            plain_default: field(8).is_some_and(|flag| !flag.is_empty()),
        },
        "union" => ItemKind::Union { derives: list(7)? },
        // A record without the flag reads as generic, which proves less.
        "trait" => ItemKind::Trait {
            generic: field(7).is_none_or(|flag| !flag.is_empty()),
        },
        "const" => ItemKind::Const,
        "static" => ItemKind::Static,
        "alias" => ItemKind::Alias {
            target: Layer::decode(field(7)?),
            params: number(field(8)?)?,
        },
        "macro" => ItemKind::Macro {
            exported: !field(7)?.is_empty(),
        },
        "mod" => ItemKind::Module {
            body: match field(7)? {
                "" => None,
                text => Some(number(text)?),
            },
            path: ModPath::decode(field(8)?),
        },
        "crate" => ItemKind::ExternCrate {
            krate: field(7)?.to_owned(),
        },
        "foreign" => ItemKind::Foreign,
        _ => return None,
    };
    Some(Item {
        scope: number(field(1)?)?,
        name: field(2)?.to_owned(),
        uid: field(3)?.to_owned(),
        vis: Vis::decode(field(4)?),
        kind,
        cfg: decode_conjuncts(field(5)?),
    })
}

fn decode_variant(record: &[String]) -> Option<Variant> {
    let field = |i: usize| record.get(i).map(String::as_str);
    Some(Variant {
        item: number(field(1)?)?,
        name: field(2)?.to_owned(),
        uid: field(3)?.to_owned(),
        cfg: decode_conjuncts(field(4)?),
    })
}

/// A list `codec::record` wrote: the empty string is no field.
fn decode_list(text: &str) -> Vec<String> {
    if text.is_empty() {
        Vec::new()
    } else {
        codec::fields(text)
    }
}

fn decode_declared(record: &[String]) -> Option<Declared> {
    Some(Declared {
        item: number(record.get(1)?)?,
        bounds: record[2.min(record.len())..]
            .iter()
            .map(|text| decode_list(text))
            .collect(),
    })
}

fn decode_macro_call(record: &[String]) -> Option<MacroCall> {
    let field = |i: usize| record.get(i).map(String::as_str);
    Some(MacroCall {
        scope: number(field(1)?)?,
        path: split(field(2)?),
        cfg: decode_conjuncts(field(3)?),
        statics: !field(4)?.is_empty(),
    })
}

fn decode_use(record: &[String]) -> Option<Use> {
    let field = |i: usize| record.get(i).map(String::as_str);
    Some(Use {
        scope: number(field(1)?)?,
        vis: Vis::decode(field(2)?),
        path: split(field(3)?),
        leaf: Leaf::decode(field(4)?)?,
        local: Some(field(5)?.to_owned()).filter(|l| !l.is_empty()),
        cfg: decode_conjuncts(field(6)?),
    })
}

fn decode_assoc(record: &[String]) -> Option<Assoc> {
    let field = |i: usize| record.get(i).map(String::as_str);
    let kind = match field(6)? {
        "fn" => AssocKind::Fn {
            receiver: SelfKind::decode(field(7)?),
            returns: Layer::decode(field(8)?),
            body: !field(9)?.is_empty(),
        },
        "const" => AssocKind::Const,
        _ => return None,
    };
    Some(Assoc {
        owner: number(field(1)?)?,
        name: field(2)?.to_owned(),
        uid: field(3)?.to_owned(),
        vis: Vis::decode(field(5)?),
        kind,
        cfg: decode_conjuncts(field(4)?),
    })
}

impl Manifest {
    pub fn encode(&self) -> String {
        let mut records: Vec<Vec<&str>> = Vec::new();
        if let Some(name) = &self.package {
            records.push(vec!["package", name]);
        }
        if self.lib_name.is_some() || self.lib_path.is_some() {
            records.push(vec![
                "lib",
                self.lib_name.as_deref().unwrap_or_default(),
                self.lib_path.as_deref().unwrap_or_default(),
            ]);
        }
        for (name, path) in &self.bins {
            records.push(vec!["bin", name, path]);
        }
        if let Some(root) = &self.workspace_root {
            records.push(vec!["root", root]);
        }
        if self.workspace {
            records.push(vec!["workspace"]);
        }
        for dependency in &self.workspace_dependencies {
            let (fields, len) = dependency.encode("wsdep");
            records.push(fields[..len].to_vec());
        }
        // The package's dependencies last: `decode_package` stops at them.
        // A run of them of one kind and source, none renamed, is one record
        // of their keys (and paths): a manifest may declare thousands.
        let mut run = None;
        for dependency in &self.dependencies {
            let Some(group) = dependency.group() else {
                let (fields, len) = dependency.encode(DEPENDENCY);
                records.push(fields[..len].to_vec());
                run = None;
                continue;
            };
            if run != Some(group) {
                records.push(vec![GROUP, group.0, group.1]);
                run = Some(group);
            }
            let open = records.last_mut().expect("the run's record");
            match &dependency.source {
                // A key that is the path's last segment is not written.
                DepSource::Path(path) if last_segment(path) == dependency.key => {
                    open.extend(["", path]);
                }
                DepSource::Path(path) => open.extend([dependency.key.as_str(), path]),
                DepSource::Workspace | DepSource::Other => open.push(&dependency.key),
            }
        }
        codec::records_of(records.iter().map(Vec::as_slice))
    }

    pub fn decode(text: &str) -> Manifest {
        Manifest {
            dependencies: Manifest::decode_dependencies(text),
            ..Manifest::decode_package(text)
        }
    }

    /// The manifest without its `dependencies`, which
    /// [`Manifest::decode_dependencies`] reads: resolution reads them one
    /// package at a time, as a workspace may declare many.
    pub fn decode_package(text: &str) -> Manifest {
        let mut manifest = Manifest::default();
        // Read borrowed, up to the first of the dependencies, whose run
        // may hold thousands.
        codec::each_borrowed_while(text, |record| {
            let field = |i: usize| record.get(i).map(ToString::to_string);
            let some = |i: usize| field(i).filter(|t| !t.is_empty());
            match record.first().map(AsRef::as_ref) {
                Some("package") => manifest.package = some(1),
                Some("lib") => {
                    manifest.lib_name = some(1);
                    manifest.lib_path = some(2);
                }
                Some("bin") => manifest
                    .bins
                    .push((field(1).unwrap_or_default(), field(2).unwrap_or_default())),
                Some("root") => manifest.workspace_root = some(1),
                Some("workspace") => manifest.workspace = true,
                Some("wsdep") => manifest
                    .workspace_dependencies
                    .extend(Dependency::decode(record)),
                Some(DEPENDENCY | GROUP | "dep") => return false,
                _ => {}
            }
            true
        });
        manifest
    }

    /// The manifest's `dependencies`.
    pub fn decode_dependencies(text: &str) -> Vec<Dependency> {
        let mut out = Vec::new();
        Manifest::each_dependency(text, &mut |dependency| {
            out.push(Dependency {
                kind: dependency.kind,
                key: dependency.key.to_owned(),
                package: dependency.package.map(str::to_owned),
                source: match dependency.source {
                    DepSourceRef::Path(path) => DepSource::Path(path.to_owned()),
                    DepSourceRef::Workspace => DepSource::Workspace,
                    DepSourceRef::Other => DepSource::Other,
                },
            });
        });
        out
    }

    /// Calls `f` with each of the manifest's `dependencies`, in order, its
    /// fields borrowed from `text` unless they hold an escape.
    pub fn each_dependency(text: &str, f: &mut dyn FnMut(DependencyRef<'_>)) {
        codec::each_borrowed(text, |record| {
            let field = |i: usize| record.get(i).map(AsRef::as_ref);
            let source = DepSourceRef::read;
            match field(0) {
                Some(DEPENDENCY | "dep") if record.len() >= 5 => {
                    let (Some(kind), Some(key)) = (field(1), field(2)) else {
                        return;
                    };
                    let Some(source) = source(field(4), field(5)) else {
                        return;
                    };
                    f(DependencyRef {
                        kind: DepKind::decode(kind),
                        key,
                        package: field(3).filter(|package| !package.is_empty()),
                        source,
                    });
                }
                // A run: its keys, each a path's followed by its path.
                Some(GROUP) if record.len() >= 3 => {
                    let kind = DepKind::decode(&record[1]);
                    let paths = &record[2] == "path";
                    for each in record[3..].chunks(if paths { 2 } else { 1 }) {
                        let path = each.get(1).map(AsRef::as_ref);
                        let Some(source) = source(field(2), path) else {
                            continue;
                        };
                        let key = match (each[0].as_ref(), path) {
                            ("", Some(path)) => last_segment(path),
                            (key, _) => key,
                        };
                        f(DependencyRef {
                            kind,
                            key,
                            package: None,
                            source,
                        });
                    }
                }
                _ => {}
            }
        });
    }
}

/// The tag of a package's dependency record (`dep` before, still read).
const DEPENDENCY: &str = "d";

/// The tag of a run of a package's dependencies of one kind and source,
/// none renamed: their keys, each a path's followed by its path, and
/// empty where it is the path's last segment.
const GROUP: &str = "g";

/// What follows a path's last `/`.
fn last_segment(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

impl DepKind {
    fn encode(self) -> &'static str {
        match self {
            DepKind::Normal => "",
            DepKind::Dev => "dev",
            DepKind::Build => "build",
        }
    }

    fn decode(text: &str) -> DepKind {
        match text {
            "dev" => DepKind::Dev,
            "build" => DepKind::Build,
            _ => DepKind::Normal,
        }
    }
}

impl Dependency {
    /// The run it joins in its manifest's facts, by kind and source, when
    /// it is not renamed and its key is not empty.
    fn group(&self) -> Option<(&'static str, &'static str)> {
        let source = match self.source {
            DepSource::Path(_) => "path",
            DepSource::Workspace => "w",
            DepSource::Other => "",
        };
        (self.package.is_none() && !self.key.is_empty()).then(|| (self.kind.encode(), source))
    }

    /// Its record's fields, and how many there are: a workspace
    /// dependency's source is `w` (`workspace` before, still read), any
    /// other but a path's empty, and only a path's has a sixth.
    fn encode<'a>(&'a self, tag: &'a str) -> ([&'a str; 6], usize) {
        let kind = self.kind.encode();
        let (source, path) = match &self.source {
            DepSource::Path(path) => ("path", path.as_str()),
            DepSource::Workspace => ("w", ""),
            DepSource::Other => ("", ""),
        };
        let fields = [
            tag,
            kind,
            &self.key,
            self.package.as_deref().unwrap_or_default(),
            source,
            path,
        ];
        (fields, if source == "path" { 6 } else { 5 })
    }

    fn decode<S: AsRef<str>>(record: &[S]) -> Option<Dependency> {
        if record.len() < 5 {
            return None;
        }
        let field = |i: usize| record.get(i).map(AsRef::as_ref);
        Some(Dependency {
            kind: DepKind::decode(field(1)?),
            key: field(2)?.to_owned(),
            package: field(3).filter(|p| !p.is_empty()).map(str::to_owned),
            source: match field(4)? {
                "path" => DepSource::Path(field(5)?.to_owned()),
                "w" | "workspace" => DepSource::Workspace,
                _ => DepSource::Other,
            },
        })
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod contract_tests {
    use super::*;

    #[test]
    fn a_context_of_every_form_round_trips() {
        let contexts = [
            Context {
                scope: 0,
                cfg: Vec::new(),
                form: Form::Path(vec!["".to_owned(), "std".to_owned(), "mem".to_owned()]),
            },
            Context {
                scope: 3,
                cfg: Vec::new(),
                form: Form::Qualified {
                    by_ref: Some(true),
                    ty: Some(TypeRef {
                        scope: 3,
                        layers: vec![Layer {
                            path: vec!["T".to_owned()],
                            dynamic: false,
                            args: vec!["u8".to_owned(), "Vec<a::B>".to_owned(), "_".to_owned()],
                        }],
                    }),
                    trait_path: vec!["Tr".to_owned()],
                    trait_args: vec!["u16".to_owned(), "_".to_owned()],
                    rest: vec!["m".to_owned()],
                },
            },
            Context {
                scope: 2,
                cfg: Vec::new(),
                form: Form::Method {
                    name: "go".to_owned(),
                    receiver: Receiver::Type(TypeRef {
                        scope: 1,
                        layers: vec![
                            Layer {
                                path: vec!["Box".to_owned()],
                                dynamic: false,
                                args: vec!["dyn a::Tr".to_owned()],
                            },
                            Layer {
                                path: vec!["a".to_owned(), "Tr".to_owned()],
                                dynamic: true,
                                args: Vec::new(),
                            },
                        ],
                    }),
                },
            },
            Context {
                scope: 1,
                cfg: Vec::new(),
                form: Form::Method {
                    name: "run".to_owned(),
                    receiver: Receiver::Ctor {
                        scope: 1,
                        path: vec!["Self".to_owned()],
                        method: "new".to_owned(),
                        unwrapped: true,
                    },
                },
            },
            Context {
                scope: 1,
                cfg: Vec::new(),
                form: Form::PathCall {
                    path: vec!["crate".to_owned(), "Tr".to_owned(), "m".to_owned()],
                    first: Argument::Typed {
                        refs: vec![true, false],
                        receiver: Receiver::Literal {
                            scope: 1,
                            path: vec!["S".to_owned()],
                        },
                    },
                },
            },
            Context {
                scope: 1,
                cfg: Vec::new(),
                form: Form::PathCall {
                    path: vec!["Tr".to_owned(), "m".to_owned()],
                    first: Argument::Generic,
                },
            },
            Context {
                scope: 1,
                cfg: Vec::new(),
                form: Form::PathCall {
                    path: vec!["T".to_owned(), "new".to_owned()],
                    first: Argument::Unknown,
                },
            },
            Context {
                scope: 0,
                cfg: vec!["cfg_attr(test,cfg(unix))".to_owned()],
                form: Form::Opaque,
            },
            Context {
                scope: 4,
                cfg: Vec::new(),
                form: Form::Use {
                    path: vec!["crate".to_owned(), "a".to_owned()],
                    leaf: Leaf::Glob,
                },
            },
        ];
        for context in contexts {
            assert_eq!(Context::decode(&context.encode()), Some(context));
        }
    }

    #[test]
    fn facts_round_trip() {
        let facts = Facts {
            scopes: vec![
                Scope {
                    parent: None,
                    cfg: Vec::new(),
                    kind: ScopeKind::File,
                },
                Scope {
                    parent: Some(0),
                    cfg: Vec::new(),
                    kind: ScopeKind::Impl {
                        mutable: true,
                        self_type: Some(Layer {
                            path: vec!["B".to_owned()],
                            dynamic: false,
                            args: vec!["?".to_owned()],
                        }),
                        by_ref: true,
                        blanket: false,
                        trait_path: vec!["fmt".to_owned(), "Display".to_owned()],
                        trait_args: vec!["_+".to_owned()],
                    },
                },
                Scope {
                    parent: Some(0),
                    cfg: Vec::new(),
                    kind: ScopeKind::Module { item: 1 },
                },
            ],
            items: vec![
                Item {
                    scope: 0,
                    cfg: Vec::new(),
                    name: "B".to_owned(),
                    uid: "#B#struct".to_owned(),
                    vis: Vis::Public,
                    kind: ItemKind::Struct {
                        value: true,
                        derives: vec!["Debug".to_owned(), "Default".to_owned()],
                        plain_default: true,
                    },
                },
                Item {
                    scope: 0,
                    cfg: Vec::new(),
                    name: "inner".to_owned(),
                    uid: "#inner#module".to_owned(),
                    vis: Vis::Restricted(vec!["crate".to_owned(), "a".to_owned()]),
                    kind: ItemKind::Module {
                        body: Some(2),
                        path: ModPath::Literal("../real".to_owned()),
                    },
                },
                Item {
                    scope: 0,
                    cfg: Vec::new(),
                    name: "gen".to_owned(),
                    uid: "#gen#macro".to_owned(),
                    vis: Vis::Public,
                    kind: ItemKind::Macro { exported: false },
                },
                Item {
                    scope: 0,
                    cfg: Vec::new(),
                    name: "U".to_owned(),
                    uid: String::new(),
                    vis: Vis::Private,
                    kind: ItemKind::Union {
                        derives: vec!["Copy".to_owned(), "Get".to_owned()],
                    },
                },
            ],
            variants: vec![Variant {
                cfg: vec!["feature=\"x\"".to_owned()],
                item: 0,
                name: "V".to_owned(),
                uid: "#E::V#variant".to_owned(),
            }],
            assocs: vec![Assoc {
                cfg: vec!["test".to_owned(), "unix".to_owned()],
                owner: 1,
                name: "fmt".to_owned(),
                uid: "#<B as Display>::fmt#method".to_owned(),
                vis: Vis::Super,
                kind: AssocKind::Fn {
                    receiver: SelfKind::Ref,
                    returns: None,
                    body: true,
                },
            }],
            uses: vec![Use {
                cfg: Vec::new(),
                scope: 2,
                vis: Vis::Crate,
                path: vec!["super".to_owned()],
                leaf: Leaf::Glob,
                local: None,
            }],
            macro_calls: vec![MacroCall {
                scope: 2,
                path: vec!["crate".to_owned(), "gen".to_owned()],
                cfg: vec!["test".to_owned()],
                statics: true,
            }],
            bounds: vec![Declared {
                item: 0,
                bounds: vec![Vec::new(), vec!["?Sized".to_owned(), "Weigher".to_owned()]],
            }],
            proc_macros: vec![
                ProcMacro {
                    kind: ProcKind::Bang,
                    name: "lazy_static".to_owned(),
                },
                ProcMacro {
                    kind: ProcKind::Derive,
                    name: "Get".to_owned(),
                },
                ProcMacro {
                    kind: ProcKind::Attribute,
                    name: "add_get".to_owned(),
                },
            ],
            attributes: vec![AttributeUse {
                scope: 1,
                name: "add_get".to_owned(),
                cfg: vec!["test".to_owned()],
            }],
        };
        assert_eq!(Facts::decode(&facts.encode()), facts);
    }

    #[test]
    fn a_manifest_round_trips() {
        let manifest = Manifest {
            package: Some("mvp-index".to_owned()),
            lib_name: Some("index".to_owned()),
            lib_path: None,
            bins: vec![("gen".to_owned(), "tools/gen.rs".to_owned())],
            workspace_root: Some("..".to_owned()),
            workspace: true,
            dependencies: vec![
                Dependency {
                    kind: DepKind::Normal,
                    key: "store".to_owned(),
                    package: Some("mvp-store".to_owned()),
                    source: DepSource::Path("../store".to_owned()),
                },
                Dependency {
                    kind: DepKind::Dev,
                    key: "proptest".to_owned(),
                    package: None,
                    source: DepSource::Workspace,
                },
                // Runs of one kind and source share a record.
                Dependency {
                    kind: DepKind::Dev,
                    key: "insta".to_owned(),
                    package: None,
                    source: DepSource::Workspace,
                },
                Dependency {
                    kind: DepKind::Normal,
                    key: "a\u{1f}b".to_owned(),
                    package: None,
                    source: DepSource::Path("../a\u{1e}\\b".to_owned()),
                },
                Dependency {
                    kind: DepKind::Normal,
                    key: "c".to_owned(),
                    package: None,
                    source: DepSource::Path(String::new()),
                },
                Dependency {
                    kind: DepKind::Normal,
                    key: "d".to_owned(),
                    package: None,
                    source: DepSource::Path("../x/d".to_owned()),
                },
                Dependency {
                    kind: DepKind::Normal,
                    key: String::new(),
                    package: None,
                    source: DepSource::Path("../e".to_owned()),
                },
                Dependency {
                    kind: DepKind::Normal,
                    key: "f".to_owned(),
                    package: None,
                    source: DepSource::Path("f".to_owned()),
                },
                Dependency {
                    kind: DepKind::Build,
                    key: "cc".to_owned(),
                    package: None,
                    source: DepSource::Other,
                },
                Dependency {
                    kind: DepKind::Dev,
                    key: "proptest".to_owned(),
                    package: None,
                    source: DepSource::Workspace,
                },
            ],
            workspace_dependencies: vec![Dependency {
                kind: DepKind::Normal,
                key: "serde".to_owned(),
                package: None,
                source: DepSource::Other,
            }],
        };
        assert_eq!(Manifest::decode(&manifest.encode()), manifest);
        let package = Manifest {
            dependencies: Vec::new(),
            ..manifest.clone()
        };
        assert_eq!(Manifest::decode_package(&manifest.encode()), package);
        assert_eq!(
            Manifest::decode_dependencies(&manifest.encode()),
            manifest.dependencies
        );
    }
}
