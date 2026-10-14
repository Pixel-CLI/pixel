// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The Ruby rule file: tree-sitter-ruby and the rules carried from v1's
//! `pixel-graph` (ADR-0005, `legacy/v1-reuse.md`): symbols and call sites,
//! the Ruby project boundary, binstubs, Rails routes, Active Record
//! associations, ancestors, and the resolution of each site.
//!
//! # Contract between extraction and resolution
//!
//! Resolution never parses a file. Everything it reads is in the rows
//! extraction produced, possibly in an earlier run:
//!
//! - **Symbols.** `module` → `Module`, `class` → `Class`, a `def` with an
//!   owner → `Method` (`Owner#m` for an instance method, `Owner.m` for a
//!   method of the class or module itself), a top-level `def` → `Function`
//!   (`m`). A `def` the rules cannot give an owner is the `Method` `m` with
//!   no owner: outside every owner but in a block or a top-level `class <<
//!   self`, a top-level `def self.m`, and inside an owner a `def` in a
//!   block that may run on another object (any block but plain iteration,
//!   a core collection's iterator on a literal or a constant,
//!   `included`, `prepended`, `class_methods`, `concerning`, and
//!   `class_eval` or `instance_eval` sent to the owner itself), in a
//!   concern's `included do` or `prepended do` (its includer's, a
//!   [`Fact::Block`]), in `class << x` or `def x.m` for an `x` that is
//!   neither the class nor a constant; where such a block or `x` may still
//!   be the owner, a [`Fact::Unknown`] or [`Fact::Evaluated`] says so.
//!   `def X.m` and a `def m` in `class << X`, for a constant `X`, are
//!   `N.m`, `N` the class or module this file reads `X` as
//!   ([`Fact::Definee`]). `concerning :X do` is the `Module` `Owner::X`,
//!   which its block's `def`s belong to and the owner includes. A `def` nested in a method is a method of
//!   the class the outer one was defined in. A `def` under a bare
//!   `module_function` is both `Owner.m` and `Owner#m`, the private copy
//!   Ruby makes. Generated methods (`attr_*`, `alias_method`, `alias`,
//!   `delegate`, `scope`, `define_method` and `define_singleton_method`
//!   with a literal name, sent with `send` or called on `self` too,
//!   `module_function :m`) are `Method`s; in a method body, a definer is a
//!   class macro's ([`Fact::Macro`]).
//!   `class_methods do` is the `Module` `Owner::ClassMethods`. A constant
//!   assigned in a class, module or top-level body (`X = v`, `self::X =
//!   v`, `const_set(:X, v)`; `Object.const_set(:X, v)` anywhere, the
//!   top-level `X`) where the assignment runs (else a
//!   [`Fact::MaybeConstant`]) is a `Const`, unless its value is
//!   `Struct.new`, `Class.new`, `Module.new` or `Data.define`, which make
//!   the `Class` or `Module` of that name, owner of the `def`s of their
//!   block; one assigned, and a class or module opened, in the owner's
//!   `class << self` is named in its singleton class's scope
//!   ([`singleton_scope`]: `#<Class:Owner>::X`). A class or module opened
//!   under a condition is recorded all the same, with its methods, though
//!   it may never exist (`if defined?(Rails); class Railtie`): nothing it
//!   declares proves a target (R6-07), its symbols are a known residual
//!   (R7-16). The file itself is the
//!   driver's `File` symbol, which
//!   owns top-level code (v1's `script`). A region the grammar could not
//!   parse (an `ERROR` node) yields no symbol and no site but its
//!   `require`s, and its [`Fact::Unread`] names.
//! - **Sites.** Every site's `context` is a [`Context`]: the name, a
//!   qualifier, and the lexical nesting (`cref`) at the site, read from the
//!   AST. Each [`EdgeKind`] fixes what the qualifier holds:
//!
//!   | kind | name | qualifier | src |
//!   | --- | --- | --- | --- |
//!   | `Call` | the method as written (`save`, `name=`) | the receiver's text, `""` for none; `extra` holds [`REBOUND`] where `self` is not what the source runs on | the definition the call is written in (a scope's lambda: the scope), else the file |
//!   | `Reference` | the method named (`authenticate`) | `arg_of`: a callback (`before_action`), `send`, `public_send`, [`ALIAS`], [`DELEGATE`]; `extra` as for a call (a callback keeps its class in `included do` and `with_options`) | the declaring class, method or generated method |
//!   | `Reference` | the constant path as written (`Trackable`, `::Base`; `::M` for `extend self` in `module M`); `""` for an expression (`include(plugin)`), whose `target` is its text; the arguments of one call last first, as Ruby inserts them | [`MIXIN`] + kind (`superclass`, `include`, `extend`, `prepend`, sent or not; prefixed `included:` or `prepended:` in a concern's `included do` or `prepended do`, `singleton:` for an `include` or `prepend` in `class << self` or sent to `singleton_class`, `hook:` in a hook such as `self.included(base)`); `extra` holds [`CONDITIONAL`] where it may not run | the declaring class or module; the hook method for `hook:` |
//!   | `Route` | the action (`create`) | [`ROUTE`] + the controller constant (`Admin::OrdersController`); in a file under `config/routes/`, [`ROUTE`] + `routes::DRAWN` + the controller's path in the module of the `draw` that reads it (`<draw>/orders`), unless absolute; `extra` = `[verb, path]` | the routes file |
//!   | `Association` | the class path (`Billing::Invoice`) | [`ASSOCIATION`] + the macro (`has_many`) | the declaring class |
//!   | `Import` | the literal spec (`foo/bar`) | the load method (`require`, `require_relative`, `require_dependency`, `load`); `extra` = `[path]` as `ruby_require_path` computes it (`[]` for `require "./x"`), then [`CONDITIONAL`] when the load may not run as the file loads | the file |
//!
//!   The source comes from the tree, never from line spans: a call in
//!   `class C; def a; b; end; end` is `C#a`'s, one after `end` on a class's
//!   last line is the scope's around it (`lang::FILE_OWNER` at the top
//!   level). `cref` is innermost first, each entry a fully qualified class
//!   or module name, or the singleton scope of one in its `class << self`;
//!   empty at top level and for a route. A superclass
//!   expression is read in the cref outside its class, as Ruby evaluates
//!   it, and its calls belong to that outer scope.
//! - **Facts.** Manifests carry their gems and load paths ([`Fact`]): a
//!   `Gemfile`, a `*.gemspec`, a `Gemfile.lock` (which has no symbol but
//!   its `File`). Any Ruby file carries what its symbols cannot say: a
//!   compact class path whose reading depends on other files
//!   ([`Fact::Compact`]), an owner's method that a lookup cannot know
//!   ([`Fact::Unknown`]), a method defined twice under a condition
//!   ([`Fact::Conditional`]), the names an unparsed region may define
//!   ([`Fact::Unread`]), a hook's statements the rules do not read
//!   ([`Fact::Hook`]), what code evaluated on a constant's class from
//!   outside its body may change there ([`Fact::Evaluated`]), a module
//!   included where the rules do not replay it ([`Fact::Mixed`]), one
//!   mixed in from outside that the rules cannot name ([`Fact::Opaque`]), a
//!   constant `const_set` or `X::Y =` may alias ([`Fact::Alias`]), the
//!   class a `def X.m` or `class << X` is read as ([`Fact::Definee`]),
//!   what a concern's `included do` defines on its includers
//!   ([`Fact::Block`]), what a class macro defines on its callers
//!   ([`Fact::Macro`]), a constant `||=` may leave unassigned
//!   ([`Fact::MaybeConstant`]), a method defined on an object the rules
//!   cannot name ([`Fact::Tainted`]), or on the singleton class of the
//!   object an owner's instance method runs on ([`Fact::Fronted`]), a
//!   module mixed into one
//!   ([`Fact::Spread`]), a method `private` or `protected` may hide from
//!   an explicit receiver ([`Fact::Hidden`]), or a visibility change the
//!   rules do not read may ([`Fact::Visibility`]),
//!   and a routes file's `draw`s ([`Fact::Draw`]).
//! - **Targets.** A resolved `Call`, `Reference`, `Route` or `Association`
//!   points at a `Method`, `Function`, `Class` or `Module` symbol; a
//!   resolved `Import` at the `File` symbol of the required file.
//!
//! # What counts as resolved
//!
//! An edge is resolved only when a rule proves its target (C-10: "a call
//! the rules cannot resolve gives no edge"; `CONTEXT.md`: avoid "guessed
//! edge"). v1's name-only tiers (the only file defining a name, a value
//! receiver's sole inherent method, a same-file definition of another
//! class) prove nothing about the receiver and stay unresolved here.
//! Candidates come from Ruby files only. A top-level function is reached
//! only from code of the file defining it or of a file requiring it,
//! transitively, and only when one file defines it.

mod dsl;
mod extract;
mod manifest;
mod require;
mod resolve;
mod routes;
mod unread;

#[cfg(test)]
mod tests;

use std::rc::Rc;

#[allow(unused_imports)]
use mvp_model::EdgeKind;
use mvp_model::Lang;

use crate::codec;
use crate::lang::{Extraction, Rules, World};

/// Bumped by any change to the grammar or these rules.
pub const VERSION: u32 = 6;

/// The nesting of an encoded [`Fact::Definee`] that has the one of the
/// definee before it: no constant's name.
const SAME_SCOPE: &str = "^";

/// The qualifier of a route reference: `":route Admin::OrdersController"`.
pub const ROUTE: &str = ":route ";
/// The qualifier of an association: `":association has_many"`.
pub const ASSOCIATION: &str = ":association ";
/// The qualifier of an ancestor declaration: `":mixin include"`.
pub const MIXIN: &str = ":mixin ";
/// The qualifier of an alias's reference to its original method.
pub const ALIAS: &str = ":alias";
/// The qualifier of a delegator's reference to its target method.
pub const DELEGATE: &str = ":delegate";
/// The `extra` of a call, `send` or callback written where `self` is not
/// what its source runs on (`extract::This`): a top-level block, a block a
/// macro or `instance_eval` runs elsewhere, a `class << self` body.
pub const REBOUND: &str = ":rebound";
/// The `extra` of an import written where it may not run when the file
/// loads: under a condition, in a block or in a method body; and of an
/// ancestor declaration written where it may not run when its body does
/// (R5-07).
pub const CONDITIONAL: &str = ":conditional";

/// The tree the Ruby rules read of `content`: tree-sitter's, past the
/// anonymous forwarding it cannot parse (#107); `None` past the parse
/// budget. C-10's oracle re-parses a file the same way.
#[cfg(test)]
pub(crate) fn tree(content: &[u8]) -> Option<tree_sitter::Tree> {
    extract::readable(content)
}

/// The cref entry of the body of `class << self` in `owner`, named as Ruby
/// inspects the singleton class (`#<Class:Owner>`): a constant assigned
/// there, or a class opened there, is the singleton class's, never
/// `owner`'s (`Owner::X` raises NameError), and Ruby looks it up first.
pub fn singleton_scope(owner: &str) -> String {
    format!("#<Class:{owner}>")
}

/// The owner whose `class << self` the cref entry `scope` is.
pub fn singleton_of(scope: &str) -> Option<&str> {
    scope.strip_prefix("#<Class:")?.strip_suffix('>')
}

pub struct Ruby;

impl Rules for Ruby {
    fn lang(&self) -> Lang {
        Lang::Ruby
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

    fn extract(&self, path: &str, content: &[u8]) -> Extraction {
        extract::extract(path, content)
    }

    fn resolve(&self, world: &World<'_>) -> Vec<Option<usize>> {
        resolve::resolve(world)
    }
}

/// What a Ruby site's `context` holds (see the table above).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Context {
    pub name: String,
    pub qualifier: String,
    /// Innermost first, fully qualified.
    pub cref: Vec<String>,
    /// Kind-specific fields: `[verb, path]` for a route, `[path]` for an
    /// import.
    pub extra: Vec<String>,
}

impl Context {
    pub fn encode(&self) -> String {
        let cref = codec::record(&self.cref);
        let extra = codec::record(&self.extra);
        codec::records(&[
            vec![self.name.as_str(), self.qualifier.as_str()],
            vec![cref.as_str()],
            vec![extra.as_str()],
        ])
    }

    pub fn decode(text: &str) -> Context {
        let records = codec::list(text);
        let field = |r: usize, f: usize| {
            records
                .get(r)
                .and_then(|record| record.get(f))
                .cloned()
                .unwrap_or_default()
        };
        let nested = |r: usize| -> Vec<String> {
            let raw = field(r, 0);
            if raw.is_empty() {
                Vec::new()
            } else {
                codec::fields(&raw)
            }
        };
        Context {
            name: field(0, 0),
            qualifier: field(0, 1),
            cref: nested(1),
            extra: nested(2),
        }
    }
}

/// A manifest fact, read by `require` resolution (v1's `Projects`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fact {
    /// `gem "name"` in a Gemfile, with its literal `require:` (`None` when
    /// absent or `require: false`).
    Gem {
        name: String,
        require: Option<String>,
    },
    /// A local gem a Gemfile declares: `gem "x", path: "dir"`, `gemspec
    /// path: "dir"`, or a gem inside a `path "dir" do` block, as written.
    PathGem { path: String },
    /// `spec.name = "x"` in a gemspec.
    GemspecName { name: String },
    /// `spec.require_paths = [...]` in a gemspec.
    RequirePaths { paths: Vec<String> },
    /// A gem a `Gemfile.lock` lists under `GEM` or `GIT`.
    LockGem { name: String },
    /// `class A::B` (or `module`, or `A::B = ...`) written inside a
    /// nesting: Ruby looks `A` up lexically, so the definition is
    /// `Scope::A::B` for the nearest scope where `A` exists then, else the
    /// top-level `A::B`. `name` is the name recorded, `written` the path as
    /// written; `nearer` holds the scopes, innermost first, that another
    /// file may give an `A` nearer than the one this file proves; `top` is
    /// true when the top-level reading was taken, which an ancestor of the
    /// innermost scope may contradict as well.
    Compact {
        name: String,
        written: String,
        top: bool,
        nearer: Vec<String>,
    },
    /// A lookup through `owner` on `side` (`#` instance, `.` the class or
    /// module itself) cannot know whether the owner holds `name` (`None`:
    /// any name): `remove_method`, `undef_method`, `undef`, or a
    /// `define_method` whose name is computed.
    Unknown {
        owner: String,
        side: char,
        name: Option<String>,
    },
    /// The method `name` of `owner` on `side` (`None`: any name) may be
    /// private or protected: defined while a bare `private` or `protected`
    /// is in force, or named by one (`private :a`, `protected def b`,
    /// `private_class_method :c`). A call on a constant receiver raises
    /// `NoMethodError` there, so it proves no target (R9-03).
    Hidden {
        owner: String,
        side: char,
        name: Option<String>,
    },
    /// Code the rules do not read may change the visibility of the method
    /// `name` (`None`: any name) of the class or module the constant
    /// `path` names in the nesting `scope`, on either side; of every class
    /// and module for an empty `path` (`send(:private, :a)` on another
    /// object, `K.private_class_method :s`, `K.class_eval { private :a }`):
    /// a call on a constant receiver reaching it proves no target
    /// (R11-03).
    Visibility {
        path: String,
        scope: Vec<String>,
        name: Option<String>,
    },
    /// The method or function `name` is defined more than once in this
    /// file, at least once under a condition (`if`, `case`, `rescue`, a
    /// loop, a block): which definition Ruby keeps depends on run time.
    Conditional { name: String },
    /// What a region the grammar could not parse may define
    /// ([`unread::scan`]).
    Unread(unread::Unread),
    /// `X ||= v` or `X &&= v` in a body: Ruby assigns `X` there only when
    /// its lookup finds none (or finds one), which another file may
    /// decide, so no constant is recorded, and a constant path through the
    /// last segment `segment` proves nothing (R5-12); the same for any
    /// assignment of `X` that may not run (under a condition, a loop, a
    /// `rescue`, in a block, R6-05).
    MaybeConstant { segment: String },
    /// A method `name` (`None`: any name) is defined on an object the
    /// rules cannot name exactly: outside every owner, on one object's
    /// singleton class, in a new anonymous class, in a block a method the
    /// rules do not model may run anywhere, on a receiver that is neither
    /// `self` nor a constant, or on a constant that names no class or
    /// module but another constant. A lookup of that name proves nothing,
    /// whatever its receiver (R5-03, R5-04); written in a test file, only
    /// the lookup of a caller in a test file does (`resolve`, "Test files
    /// are another program", #106).
    Tainted { name: Option<String> },
    /// An instance method of `owner` defines a method `name` (`None`: any
    /// name) on the singleton class of the object it runs on
    /// (`define_singleton_method(n)` in a `def`): that object's chain holds
    /// `owner`'s instance step, and the method comes before the whole
    /// chain. A lookup from an object whose chain holds that step proves
    /// nothing of the name; one from any other object is not touched (R5-04
    /// scoped, #106).
    Fronted { owner: String, name: Option<String> },
    /// The module the constant `path` names in the nesting `scope` is
    /// mixed into an object the rules cannot name (`o.extend(M)`,
    /// `klass.include(M)`, an `include` in a new anonymous class): its
    /// methods, and those of every module it brings along, may be any
    /// object's (R5-04).
    Spread { path: String, scope: Vec<String> },
    /// Code evaluated on the class or module the constant `path` names,
    /// written in the nesting `scope` (innermost first) outside its body
    /// (`X.class_eval do def m`, `X.remove_method :m`, `X.include M`), may
    /// define, remove or reorder `name` (`None`: any name) on `side` there:
    /// a lookup that reaches that class or module for it abstains (R9,
    /// R10).
    Evaluated {
        path: String,
        scope: Vec<String>,
        /// `#` its instance methods, `.` its own.
        side: char,
        name: Option<String>,
    },
    /// The method of `owner` on `method` (`#` or `.`), a class macro,
    /// calls a definer on `self` (`define_method`, `attr_*`, `include`, a
    /// `class_eval` block or string): it defines `name` (`None`: any name)
    /// on `side` of whatever object it runs on, an object whose singleton
    /// chain holds that method's step (B7).
    Macro {
        owner: String,
        method: char,
        side: char,
        name: Option<String>,
    },
    /// A concern's `included do` (`prepended`: `prepended do`) block
    /// defines `name` (`None`: any name) on `side` (`#` instance methods,
    /// `.` its own) of each class or module it runs on, as `class_eval`
    /// would: a method the rules give no owner (B6).
    Block {
        concern: String,
        prepended: bool,
        side: char,
        name: Option<String>,
    },
    /// `def X.m` or a `def m` in `class << X` (`name`: `N.m`), or the body
    /// of `class << X` (`name`: `#<Class:N>`), for the constant `X`
    /// (`written`) in the nesting `scope`: `N` is the class or module this
    /// file reads `X` as, which another file's nesting, or an ancestor of
    /// the innermost scope, may contradict (B2).
    Definee {
        name: String,
        written: String,
        scope: Rc<[String]>,
    },
    /// The module the constant `path` names, in the nesting `scope`, is
    /// mixed into an object whose own lookup the rules do not replay
    /// (`X.include M`, `X.prepend M`, `X.extend M` outside `X`'s body, in
    /// a block evaluated on `X`, by a class macro, or into `Object` by a
    /// top-level `include`): a call on `self` in its methods may run on
    /// that object, whose own methods may come first (R5-05, R6-06).
    Mixed { path: String, scope: Vec<String> },
    /// A mixin from outside the body of the class or module the constant
    /// `path` names in the nesting `scope` (`K.prepend(Cfg.mod)`, `K.include
    /// const_get(name)`, a `prepend` in `K.class_eval do`) whose argument
    /// is no constant path: an unknown step on `side` there (`#` its
    /// instance chain, `.` its singleton chain), in front of its own list
    /// for a `prepend` (`prepend`), after its own step otherwise, that may
    /// hold any module of the graph (R8-05).
    Opaque {
        path: String,
        scope: Vec<String>,
        side: char,
        prepend: bool,
    },
    /// `const_set`, on any receiver, sent or not, or a scoped constant
    /// assignment (`X::Y = v`), of a value that may be a class or module,
    /// assigns a constant whose last segment is `segment` in a namespace
    /// the rules do not follow: a class or module reopened under a path
    /// holding that segment may be another (R8-09).
    Alias { segment: String },
    /// The constant `name` is assigned the class or module a constructor
    /// makes (`X = Struct.new`, `Class.new`, `Module.new`, `Data.define`).
    /// Assigned twice, in one file or two, Ruby replaces the first with
    /// the second, which the graph would merge as if reopened: neither is
    /// then a class or module the rules can name (R10-06).
    Constructed { name: String },
    /// The hook `method` (qualified: `Tracked.included`) runs, at `line`, a
    /// statement that may change the ancestors of the object it runs for
    /// in a way the rules do not read; at the hook's own line, a hook
    /// whose parameters the rules do not read, or a definition of
    /// `append_features`, `prepend_features` or `extend_object` (D14).
    Hook { method: String, line: u32 },
    /// The hook `method` calls `super` at `line`: the next definition of
    /// the hook on the singleton chain it was found on runs there (a
    /// superclass's `inherited`).
    Super { method: String, line: u32 },
    /// `draw :name` in a routes file: it reads `config/routes/name.rb`
    /// (`name` empty when computed) in the scope of the draw, of the
    /// controller module `module` (prefixed `routes::DRAWN` in a drawn
    /// file); `None` in a resource or controller scope, which the rules do
    /// not carry (D04).
    Draw {
        name: String,
        module: Option<String>,
    },
}

impl Fact {
    pub fn encode_all(facts: &[Fact]) -> String {
        // A definee's nesting, written once for those that follow with the
        // same ([`SAME_SCOPE`], R11-02).
        let mut last: Option<&Rc<[String]>> = None;
        let records: Vec<Vec<String>> = facts
            .iter()
            .map(|fact| match fact {
                Fact::Gem { name, require } => vec![
                    "gem".to_owned(),
                    name.clone(),
                    require
                        .clone()
                        .map_or_else(String::new, |r| format!("={r}")),
                ],
                Fact::PathGem { path } => vec!["path_gem".to_owned(), path.clone()],
                Fact::GemspecName { name } => vec!["gemspec_name".to_owned(), name.clone()],
                Fact::RequirePaths { paths } => {
                    let mut record = vec!["require_paths".to_owned()];
                    record.extend(paths.iter().cloned());
                    record
                }
                Fact::LockGem { name } => vec!["lock_gem".to_owned(), name.clone()],
                Fact::Compact {
                    name,
                    written,
                    top,
                    nearer,
                } => {
                    let mut record = vec![
                        "compact".to_owned(),
                        name.clone(),
                        written.clone(),
                        if *top { "top" } else { "" }.to_owned(),
                    ];
                    record.extend(nearer.iter().cloned());
                    record
                }
                Fact::Unknown { owner, side, name } => vec![
                    "unknown".to_owned(),
                    owner.clone(),
                    side.to_string(),
                    name.clone().map_or_else(String::new, |n| format!("={n}")),
                ],
                Fact::Hidden { owner, side, name } => vec![
                    "hidden".to_owned(),
                    owner.clone(),
                    side.to_string(),
                    name.clone().map_or_else(String::new, |n| format!("={n}")),
                ],
                Fact::Conditional { name } => vec!["conditional".to_owned(), name.clone()],
                Fact::Unread(unread) => unread.record(),
                Fact::MaybeConstant { segment } => {
                    vec!["maybe_constant".to_owned(), segment.clone()]
                }
                Fact::Tainted { name } => vec![
                    "tainted".to_owned(),
                    name.clone().map_or_else(String::new, |n| format!("={n}")),
                ],
                Fact::Fronted { owner, name } => vec![
                    "fronted".to_owned(),
                    owner.clone(),
                    name.clone().map_or_else(String::new, |n| format!("={n}")),
                ],
                Fact::Spread { path, scope } => {
                    let mut record = vec!["spread".to_owned(), path.clone()];
                    record.extend(scope.iter().cloned());
                    record
                }
                Fact::Evaluated {
                    path,
                    scope,
                    side,
                    name,
                } => {
                    let mut record = vec![
                        "evaluated".to_owned(),
                        path.clone(),
                        side.to_string(),
                        name.clone().map_or_else(String::new, |n| format!("={n}")),
                    ];
                    record.extend(scope.iter().cloned());
                    record
                }
                Fact::Macro {
                    owner,
                    method,
                    side,
                    name,
                } => vec![
                    "macro".to_owned(),
                    owner.clone(),
                    method.to_string(),
                    side.to_string(),
                    name.clone().map_or_else(String::new, |n| format!("={n}")),
                ],
                Fact::Block {
                    concern,
                    prepended,
                    side,
                    name,
                } => vec![
                    "block".to_owned(),
                    concern.clone(),
                    if *prepended { "prepended" } else { "included" }.to_owned(),
                    side.to_string(),
                    name.clone().map_or_else(String::new, |n| format!("={n}")),
                ],
                Fact::Definee {
                    name,
                    written,
                    scope,
                } => {
                    let mut record = vec!["definee".to_owned(), name.clone(), written.clone()];
                    if !scope.is_empty()
                        && last.is_some_and(|last| Rc::ptr_eq(last, scope) || last == scope)
                    {
                        record.push(SAME_SCOPE.to_owned());
                    } else {
                        record.extend(scope.iter().cloned());
                    }
                    last = Some(scope);
                    record
                }
                Fact::Mixed { path, scope } => {
                    let mut record = vec!["mixed".to_owned(), path.clone()];
                    record.extend(scope.iter().cloned());
                    record
                }
                Fact::Visibility { path, scope, name } => {
                    let mut record = vec![
                        "visibility".to_owned(),
                        path.clone(),
                        name.clone().map_or_else(String::new, |n| format!("={n}")),
                    ];
                    record.extend(scope.iter().cloned());
                    record
                }
                Fact::Opaque {
                    path,
                    scope,
                    side,
                    prepend,
                } => {
                    let mut record = vec![
                        "opaque".to_owned(),
                        path.clone(),
                        side.to_string(),
                        if *prepend { "prepend" } else { "" }.to_owned(),
                    ];
                    record.extend(scope.iter().cloned());
                    record
                }
                Fact::Alias { segment } => vec!["alias".to_owned(), segment.clone()],
                Fact::Constructed { name } => vec!["constructed".to_owned(), name.clone()],
                Fact::Hook { method, line } => {
                    vec!["hook".to_owned(), method.clone(), line.to_string()]
                }
                Fact::Super { method, line } => {
                    vec!["super".to_owned(), method.clone(), line.to_string()]
                }
                Fact::Draw { name, module } => vec![
                    "draw".to_owned(),
                    name.clone(),
                    module
                        .as_ref()
                        .map_or_else(String::new, |m| format!("={m}")),
                ],
            })
            .collect();
        codec::records(&records)
    }

    pub fn decode_all(text: &str) -> Vec<Fact> {
        let mut last: Rc<[String]> = Rc::new([]);
        codec::list(text)
            .into_iter()
            .filter_map(|record| {
                let (tag, rest) = record.split_first()?;
                let first = rest.first().cloned().unwrap_or_default();
                Some(match tag.as_str() {
                    "gem" => Fact::Gem {
                        name: first,
                        require: rest
                            .get(1)
                            .and_then(|r| r.strip_prefix('='))
                            .map(str::to_owned),
                    },
                    "path_gem" => Fact::PathGem { path: first },
                    "gemspec_name" => Fact::GemspecName { name: first },
                    "require_paths" => Fact::RequirePaths {
                        paths: rest.to_vec(),
                    },
                    "lock_gem" => Fact::LockGem { name: first },
                    "compact" => Fact::Compact {
                        name: first,
                        written: rest.get(1).cloned().unwrap_or_default(),
                        top: rest.get(2).is_some_and(|t| t == "top"),
                        nearer: rest.get(3..).unwrap_or_default().to_vec(),
                    },
                    "unknown" => Fact::Unknown {
                        owner: first,
                        side: rest.get(1)?.chars().next()?,
                        name: rest
                            .get(2)
                            .and_then(|n| n.strip_prefix('='))
                            .map(str::to_owned),
                    },
                    "hidden" => Fact::Hidden {
                        owner: first,
                        side: rest.get(1)?.chars().next()?,
                        name: rest
                            .get(2)
                            .and_then(|n| n.strip_prefix('='))
                            .map(str::to_owned),
                    },
                    "conditional" => Fact::Conditional { name: first },
                    "unread" => Fact::Unread(unread::Unread::from_record(rest)),
                    "maybe_constant" => Fact::MaybeConstant { segment: first },
                    "tainted" => Fact::Tainted {
                        name: first.strip_prefix('=').map(str::to_owned),
                    },
                    "fronted" => Fact::Fronted {
                        owner: first,
                        name: rest
                            .get(1)
                            .and_then(|n| n.strip_prefix('='))
                            .map(str::to_owned),
                    },
                    "spread" => Fact::Spread {
                        path: first,
                        scope: rest.get(1..).unwrap_or_default().to_vec(),
                    },
                    "evaluated" => Fact::Evaluated {
                        path: first,
                        side: rest.get(1)?.chars().next()?,
                        name: rest
                            .get(2)
                            .and_then(|n| n.strip_prefix('='))
                            .map(str::to_owned),
                        scope: rest.get(3..).unwrap_or_default().to_vec(),
                    },
                    "macro" => Fact::Macro {
                        owner: first,
                        method: rest.get(1)?.chars().next()?,
                        side: rest.get(2)?.chars().next()?,
                        name: rest
                            .get(3)
                            .and_then(|n| n.strip_prefix('='))
                            .map(str::to_owned),
                    },
                    "block" => Fact::Block {
                        concern: first,
                        prepended: rest.get(1)? == "prepended",
                        side: rest.get(2)?.chars().next()?,
                        name: rest
                            .get(3)
                            .and_then(|n| n.strip_prefix('='))
                            .map(str::to_owned),
                    },
                    "definee" => {
                        let scope = rest.get(2..).unwrap_or_default();
                        if scope != [SAME_SCOPE] {
                            last = scope.into();
                        }
                        Fact::Definee {
                            name: first,
                            written: rest.get(1)?.clone(),
                            scope: Rc::clone(&last),
                        }
                    }
                    "mixed" => Fact::Mixed {
                        path: first,
                        scope: rest.get(1..).unwrap_or_default().to_vec(),
                    },
                    "visibility" => Fact::Visibility {
                        path: first,
                        name: rest
                            .get(1)
                            .and_then(|n| n.strip_prefix('='))
                            .map(str::to_owned),
                        scope: rest.get(2..).unwrap_or_default().to_vec(),
                    },
                    "opaque" => Fact::Opaque {
                        path: first,
                        side: rest.get(1)?.chars().next()?,
                        prepend: rest.get(2)? == "prepend",
                        scope: rest.get(3..).unwrap_or_default().to_vec(),
                    },
                    "alias" => Fact::Alias { segment: first },
                    "constructed" => Fact::Constructed { name: first },
                    "hook" => Fact::Hook {
                        method: first,
                        line: rest.get(1)?.parse().ok()?,
                    },
                    "super" => Fact::Super {
                        method: first,
                        line: rest.get(1)?.parse().ok()?,
                    },
                    "draw" => Fact::Draw {
                        name: first,
                        module: rest
                            .get(1)
                            .and_then(|m| m.strip_prefix('='))
                            .map(str::to_owned),
                    },
                    _ => return None,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    #[test]
    fn a_context_round_trips() {
        let context = Context {
            name: "perform_later".to_owned(),
            qualifier: "Job.set".to_owned(),
            cref: vec!["Admin::Jobs".to_owned(), "Admin".to_owned()],
            extra: vec![],
        };
        assert_eq!(Context::decode(&context.encode()), context);
        let top = Context {
            name: "x".to_owned(),
            ..Context::default()
        };
        assert_eq!(Context::decode(&top.encode()), top);
    }

    #[test]
    fn facts_round_trip() {
        let facts = vec![
            Fact::Gem {
                name: "rails".to_owned(),
                require: None,
            },
            Fact::Gem {
                name: "x".to_owned(),
                require: Some(String::new()),
            },
            Fact::PathGem {
                path: "engines/billing".to_owned(),
            },
            Fact::GemspecName {
                name: "billing".to_owned(),
            },
            Fact::RequirePaths {
                paths: vec!["lib".to_owned(), "ext".to_owned()],
            },
            Fact::LockGem {
                name: "rack".to_owned(),
            },
            Fact::Compact {
                name: "Foo::Bar".to_owned(),
                written: "Foo::Bar".to_owned(),
                top: true,
                nearer: vec!["Outer::Inner".to_owned(), "Outer".to_owned()],
            },
            Fact::Compact {
                name: "Outer::Foo::Bar".to_owned(),
                written: "Foo::Bar".to_owned(),
                top: false,
                nearer: vec![],
            },
            Fact::Unknown {
                owner: "Post".to_owned(),
                side: '#',
                name: Some("tag".to_owned()),
            },
            Fact::Unknown {
                owner: "Post".to_owned(),
                side: '.',
                name: None,
            },
            Fact::Hidden {
                owner: "Post".to_owned(),
                side: '#',
                name: Some("secret".to_owned()),
            },
            Fact::Hidden {
                owner: "Post".to_owned(),
                side: '.',
                name: None,
            },
            Fact::Conditional {
                name: "Cond#fast".to_owned(),
            },
            Fact::Hook {
                method: "Tracked.included".to_owned(),
                line: 4,
            },
            Fact::Super {
                method: "Model.inherited".to_owned(),
                line: 2,
            },
            Fact::Draw {
                name: "admin".to_owned(),
                module: Some("admin".to_owned()),
            },
            Fact::Draw {
                name: "".to_owned(),
                module: Some(String::new()),
            },
            Fact::Draw {
                name: "nested".to_owned(),
                module: None,
            },
            Fact::Unread(unread::scan(
                "module Admin\n  class User\n    def self.find(&); end\n    LIMIT = 1\n  end\nend\n",
            )),
            Fact::Evaluated {
                path: "Admin::User".to_owned(),
                scope: vec!["Admin::Panel".to_owned(), "Admin".to_owned()],
                side: '#',
                name: Some("tag".to_owned()),
            },
            Fact::Evaluated {
                path: "::Other".to_owned(),
                scope: vec![],
                side: '.',
                name: None,
            },
            Fact::Mixed {
                path: "Helpers".to_owned(),
                scope: vec!["Admin".to_owned()],
            },
            Fact::Visibility {
                path: "K".to_owned(),
                scope: vec!["Outer".to_owned()],
                name: Some("a".to_owned()),
            },
            Fact::Visibility {
                path: String::new(),
                scope: vec![],
                name: None,
            },
            Fact::Opaque {
                path: "Admin::User".to_owned(),
                scope: vec!["Admin".to_owned()],
                side: '#',
                prepend: true,
            },
            Fact::Opaque {
                path: "K".to_owned(),
                scope: vec![],
                side: '.',
                prepend: false,
            },
            Fact::Alias {
                segment: "Alias".to_owned(),
            },
            Fact::Constructed {
                name: "Admin::Point".to_owned(),
            },
            Fact::Macro {
                owner: "Post".to_owned(),
                method: '.',
                side: '#',
                name: Some("helper".to_owned()),
            },
            Fact::Macro {
                owner: "Macro".to_owned(),
                method: '#',
                side: '.',
                name: None,
            },
            Fact::Block {
                concern: "Tag".to_owned(),
                prepended: false,
                side: '#',
                name: Some("a".to_owned()),
            },
            Fact::Block {
                concern: "Tag".to_owned(),
                prepended: true,
                side: '.',
                name: None,
            },
            Fact::Definee {
                name: "Outer.s1".to_owned(),
                written: "Outer".to_owned(),
                scope: vec!["Outer".to_owned()].into(),
            },
            Fact::Definee {
                name: "Outer.s2".to_owned(),
                written: "Outer".to_owned(),
                scope: vec!["Outer".to_owned()].into(),
            },
            Fact::Definee {
                name: "#<Class:Config>".to_owned(),
                written: "Config".to_owned(),
                scope: Rc::new([]),
            },
            Fact::Definee {
                name: "Inner.s3".to_owned(),
                written: "Inner".to_owned(),
                scope: vec!["A::B".to_owned(), "A".to_owned()].into(),
            },
            Fact::MaybeConstant {
                segment: "Client".to_owned(),
            },
            Fact::Tainted {
                name: Some("hook".to_owned()),
            },
            Fact::Tainted { name: None },
            Fact::Fronted {
                owner: "Maker".to_owned(),
                name: Some("hook".to_owned()),
            },
            Fact::Fronted {
                owner: "Admin::Maker".to_owned(),
                name: None,
            },
            Fact::Spread {
                path: "Over".to_owned(),
                scope: vec!["Admin".to_owned()],
            },
        ];
        assert_eq!(Fact::decode_all(&Fact::encode_all(&facts)), facts);
    }
}
