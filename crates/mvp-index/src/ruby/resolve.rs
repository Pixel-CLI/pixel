// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Deciding every Ruby site of a generation, carried from v1's
//! `pixel-graph` (`src/resolve.rs`, `src/resolve/ruby.rs`).
//!
//! The rows of the generation are read once into an [`Index`]; each site is
//! then decided from its own row and the index alone, so a run that
//! extracted one file decides exactly what a full run decides.
//!
//! An edge gets a target only when a rule proves it (C-10). The rules:
//!
//! - a call without receiver or on `self` follows Ruby's method lookup from
//!   the caller (`ancestors`): in a method, from every object `self` may be
//!   (an instance of the class or of a subclass, of a class or module
//!   including the module, in this project or one loading it as a path
//!   gem), which must all find the same definition (F1, R4, R6); in a core
//!   class or a module included where the rules do not replay it, objects
//!   the graph does not hold may come first, and nothing is proven (R5,
//!   R15). It reaches a
//!   top-level function only from an instance whose chain the graph knows
//!   to its end, and only one defined in the caller's own file or in a
//!   file it requires: a script's `def` exists only once that script is
//!   loaded, so the one other file defining the name proves nothing
//!   (`bin/release` calling the `gemfile` of `bundler/inline`, not the one
//!   `bin/bundle` defines). A class written `< Object` does not reach it,
//!   as on main; one with no superclass written still does where code the
//!   rules do not read (`send(:eval, ..)`, a computed `eval`, a top-level
//!   `alias` or `undef`) changed `Object`, a residual main shares (R8-01);
//! - a call on a constant follows Ruby's lexical constant lookup and the
//!   constructor, Active Job and Action Mailer rules (`constants`), and
//!   never reaches a method a bare `private`, `protected` or
//!   `module_function` precedes or one of them names, in its own class or
//!   module or in one the receiver's chain meets first, which raises
//!   there (R9-03, R10-04, R10-05); nor one a visibility change the rules
//!   do not read may touch, nor an alias whose original is not proven
//!   public where it runs (R11-03); nor, as on main, one past a class or
//!   module a constructor made or a module's own `extend self`, but the
//!   receiver's own method in a body a keyword opens (R12-03);
//! - a callback, `send`, alias or delegator names a method of its own owner
//!   or of an ancestor, a callback, `send` or delegator in a method the same
//!   for every object it may run on, an alias the body lookup finds when it
//!   runs (a module prepended above it first, B1); a route the action of
//!   its controller
//!   class; an
//!   association its model class as Active Record's `compute_type` finds it;
//!   an ancestor declaration the class or module it names;
//! - a call on a chain Rails' conventions type (ADR-0038, `values`): on a
//!   record of a class, what a call on `self` in its instance methods
//!   finds, the same for every subclass; on a relation of a model, a
//!   `scope` of the model and nothing else;
//! - a call in an example of an RSpec group (ADR-0041, `rspec`): the
//!   `def` the groups find from its own outwards, the same from every
//!   group a hook, a `let` or a `def` runs on, with nothing the rules do
//!   not read in between (a module the group includes is a step after its
//!   own methods, a shared context the repository declares once brings its
//!   block's definitions); `let`, `subject`, `described_class` and a
//!   repository factory's `create` and `build` type the receivers they
//!   make as the chains above;
//! - a `require` the file its project's load roots hold (`require`).
//!
//! v1's name-only tiers (the only file defining a name, the sole method of a
//! name for a value receiver, a same-file definition in another class) prove
//! nothing about the receiver: their sites stay unresolved. A method defined
//! twice in one file is its last definition, as Ruby loads it, unless one of
//! them is under a condition (D06); defined in two files it is unresolved,
//! since their load order is unknown. A constant whose value the rules do
//! not read (`X = Foo`) names no class (D11), nor does one assigned a
//! constructor's class or module twice (R10-06). The files' facts (`facts`)
//! make a decision abstain where another file may move a definition (D09),
//! an owner forgets or computes a method's name (F4, D16), an unread region
//! may define a name (D12; any class's, for a name it defines where it
//! cannot tell on what, B9), or code evaluated on a class from outside its
//! body may (R9, R10); a string a binding evaluates that can define
//! nothing on any `self` (a Redis script, `dsl::inert`, #111) changes no
//! class. A load under a condition proves no function (R13),
//! and neither does one whose file requires the caller's back (M1). A method
//! defined only where it may not run (under a condition, in a block, in
//! another method's body, in a body a `rescue` guards, past a statement that
//! may leave early at any depth outside a method body: a top-level `return`,
//! a `next` out of a block Ruby runs once, a `raise`) proves no target (M2,
//! R6-09, R6-10, R7-04, R8-04).
//!
//! Where modelling Ruby's dynamic forms one by one did not converge, the
//! rules are blunt, sound by construction: a method defined on an object
//! the rules cannot name exactly taints its name, which no lookup then
//! proves, whatever the receiver (but one an owner's instance method
//! defines on the singleton class of the object it runs on, which only
//! the lookups from objects whose chain holds that owner's step, and the
//! calls in a top-level function, may meet, #106; and one a test file
//! writes, below), and so does one a class reopened through
//! a constant alias defines (`class Alias` for `Alias = Foo`, R5-03,
//! R5-04); a module mixed in from outside a body, from a block or a macro,
//! by code the rules do not read, or into such an object, proves nothing
//! of its self calls, and neither does what it brings along (R5-05, R6-06,
//! R6-14), any constant an expression mixed in so writes, or any module
//! at all for a constant whose value the rules do not read (R7-05), but
//! none for a module `Module.new` builds where it is mixed in, which is
//! no module of the graph (`dsl::fresh`, #111); what code a string
//! evaluates on an object the rules cannot name mixes in is mixed into
//! such an object (a test file's, a stub, #106, #111); a
//! mixin that may not run, or that a step the rules cannot name may
//! already hold, is a step that may not be there (R5-06, R5-07); a mixin
//! of an expression from outside a class is an unknown step on its chain
//! (R8-05); a hook defined other than by `def`, on a singleton other than
//! by a `def self.` in its body, or on `Module` or `Class` themselves, is
//! unknown, and `method_added` anywhere is a macro that may define any
//! name (R5-08, R6-16, R8-06); a class reopened under a name `const_set`
//! or `X::Y =` may assign is an alias (R8-09); a body that passes a block
//! on to rebind `self`, a block given to a `send` of a name the rules
//! cannot read, and a lambda or proc passed, stored or returned keep no
//! `self` (R5-11, R6-12, R8-07, R8-08).
//!
//! A gem's module mixed into a module of the graph (`User::Mentionable`'s
//! `include ActionText::Attachable` in once-campfire) is a step of its
//! own, which the rules cannot read but can place (#120): it may define
//! any name, so a lookup through it proves only a definition before it,
//! but the modules a class mixes in beside the module stay known steps in
//! their places. What the gem's module adds to the module's singleton
//! class (a gem concern's `ClassMethods`, which Active Support's
//! `append_features` extends it with; what its own `included` hook
//! extends it with) is taken to define none of the module's hooks
//! (`append_features`, `included` and their family), so it neither keeps
//! the module out of a class including it nor mixes anything in ahead of
//! it there. The argument: Ruby removes no module from a chain and moves
//! none (there is no `uninclude`); what an `include` of a module inserts,
//! and where, is decided by that module's own hooks alone, looked up on
//! its singleton class. What a gem's module puts there is written for the
//! classes that include it, on which a module's hooks never run (a class
//! cannot be included), so a gem has no reason to define one there. The
//! residual, listed as a gem superclass's `inherited` is: a gem that gives
//! the modules including its own hooks of their own (a concern system
//! other than Active Support's). A concern of the graph already reads only
//! its own hooks (`Index::replaces`), and a module that `extend`s a gem's
//! (`extend Hooky`), the form that gives a module hooks, still may get any
//! (R7).
//!
//! Test files are another program (#106, decided 2026-10-14). What a test
//! file (under a `spec` or `test` directory, or named `*_spec.rb` or
//! `*_test.rb`) defines on an object the rules cannot name
//! (`product.define_singleton_method(m)`, `klass.class_eval(&block)`) is a
//! hand-written stub, as `allow(x).to receive(:m)` and `x.stubs(:m)` are,
//! which the rules do not read either: only a test runner loads the file,
//! never the application. Such a definition taints the lookups of callers
//! in test files alone, which prove nothing of its name (C-10 holds for
//! them); a caller elsewhere keeps its edge. A stub in an example file
//! reaches only its own callers and those of shared test code (`stubs`).
//! What RSpec scopes to one file does not make it shared test code: a
//! shared group declared without a receiver in a group's body, which
//! RSpec gives that group and its nested groups alone, all written in
//! the file (another file's group cannot include it, checked with
//! rspec-core 3.13.6); and, where RSpec's model reads the program, a
//! module a group includes, mixed into that group's examples, which the
//! model names and whose lookups read it as the group's step: its names
//! are no object's the rules cannot name, though a call on `self` in its
//! methods may run on the group, whose own `def`s come first, and proves
//! nothing (R5-05). So the `let` names such a file stubs (gumroad's
//! `user`, on a mock model beside a scoped shared group) stay provable in
//! every other example file.
//! What a test file defines on a class it names (`class Link` reopened in
//! `spec/support`) is read as any file's.

mod ancestors;
mod constants;
mod corelib;
mod facts;
#[cfg(test)]
pub(super) mod fixture;
mod rspec;
mod stubs;
mod values;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use mvp_model::{Edge, EdgeKind, Symbol, SymbolKind};

use self::ancestors::{Lookup, Mixin, MixinKind, On, Op, Walks};
use self::facts::Facts;
use self::stubs::Tests;
use super::require::Projects;
use super::routes::{DRAWN, app_of, camelize_path, drawn_name, join_module};
use super::{ALIAS, CONDITIONAL, Context, DELEGATE, MIXIN, REBOUND, ROUTE, VALUE, singleton_of};
use crate::lang::World;

/// For `world.edges[i]`, the index in `world.symbols` of its target, or
/// `None` when no rule proves one. Each Ruby project is a program of its
/// own (D13): a site is decided from the rows of its project and of the
/// local path gems it declares only.
pub fn resolve(world: &World<'_>) -> Vec<Option<usize>> {
    let projects = Projects::build(world.files);
    let file_of: HashMap<&str, &str> = world
        .symbols
        .iter()
        .map(|s| (s.uid.as_str(), s.file.as_str()))
        .collect();
    let mut groups: BTreeMap<Option<&str>, Vec<usize>> = BTreeMap::new();
    for (position, edge) in world.edges.iter().enumerate() {
        let root = file_of
            .get(edge.src.as_str())
            .and_then(|file| projects.root_of(file));
        groups.entry(root).or_default().push(position);
    }
    let mut decided = vec![None; world.edges.len()];
    for (root, positions) in groups {
        let index_of = |root: Option<&str>, tests: &Tests| {
            let visible = projects.visible_from(root);
            Index::build(
                world,
                &|file| visible.contains(&projects.root_of(file)),
                tests,
            )
        };
        // The projects that load this one as a path gem hold subclasses
        // and includers of its classes and modules (R6).
        let with_loaders = |tests: &Tests| {
            let mut index = index_of(root, tests);
            index.loaders = projects
                .loaders_of(root)
                .into_iter()
                .map(|loader| index_of(loader, tests))
                .collect();
            index
        };
        // A test file's callers are decided with what test files define on
        // objects the rules cannot name, as far as it reaches them; the
        // others without (#106, ADR-0039).
        let (in_tests, elsewhere): (Vec<usize>, Vec<usize>) =
            positions.into_iter().partition(|&position| {
                file_of
                    .get(world.edges[position].src.as_str())
                    .is_some_and(|file| test_file(file))
            });
        let index = with_loaders(&Tests::Without);
        for position in elsewhere {
            decided[position] = index.decide(position, &world.edges[position]);
        }
        if !in_tests.is_empty() {
            stubs::decide(
                world,
                &file_of,
                in_tests,
                index,
                &with_loaders,
                &mut decided,
            );
        }
    }
    decided
}

/// Whether `path` is a test file: under a `spec` or `test` directory, or
/// named `*_spec.rb` or `*_test.rb`. Only a test runner loads one, never
/// the application, so what it defines on an object the rules cannot name
/// is a stub, which only its callers may meet (#106).
pub(super) fn test_file(path: &str) -> bool {
    let (dirs, name) = path.rsplit_once('/').unwrap_or(("", path));
    dirs.split('/').any(|dir| dir == "spec" || dir == "test")
        || name.ends_with("_spec.rb")
        || name.ends_with("_test.rb")
}

/// What a rule decides for a site: the index of its target symbol, or `None`
/// when the rule cannot prove one.
type Decision = Option<usize>;

/// Which methods a lookup reads: those of an instance (`Owner#m`) or those
/// of the class or module itself (`Owner.m`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Side {
    Instance,
    Singleton,
}

impl Side {
    fn other(self) -> Side {
        match self {
            Side::Instance => Side::Singleton,
            Side::Singleton => Side::Instance,
        }
    }
}

/// The owner, side and name of a method's qualified name, split at its last
/// `#` or `.` (v1 `ruby_owner`): `Svc#run` is `(Svc, Instance, run)`,
/// `Svc.build` `(Svc, Singleton, build)`. `None` for a name with neither.
fn split_method(qualified: &str) -> Option<(&str, Side, &str)> {
    let at = qualified.rfind(['#', '.'])?;
    let side = if qualified[at..].starts_with('#') {
        Side::Instance
    } else {
        Side::Singleton
    };
    Some((&qualified[..at], side, &qualified[at + 1..]))
}

/// The definitions of one name, as Ruby keeps them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Def {
    /// Defined in one file: the last definition there, the one Ruby keeps
    /// after loading the file.
    One(usize),
    /// Defined in several files, whose load order the graph cannot tell.
    Ambiguous,
}

impl Def {
    fn add<'a>(map: &mut HashMap<&'a str, Def>, key: &'a str, symbols: &[Symbol], i: usize) {
        map.entry(key)
            .and_modify(|def| {
                *def = match *def {
                    Def::One(j) if symbols[j].file != symbols[i].file => Def::Ambiguous,
                    Def::One(j) if symbols[j].line > symbols[i].line => Def::One(j),
                    Def::One(_) => Def::One(i),
                    Def::Ambiguous => Def::Ambiguous,
                }
            })
            .or_insert(Def::One(i));
    }

    fn target(self) -> Decision {
        match self {
            Def::One(i) => Some(i),
            Def::Ambiguous => None,
        }
    }
}

/// The generation's Ruby rows, read once for every decision.
struct Index<'a> {
    symbols: &'a [Symbol],
    by_uid: HashMap<&'a str, usize>,
    /// Class, module and constant names, with their definitions.
    constants: HashMap<&'a str, Def>,
    /// The names of constants whose value the rules do not read: a
    /// constant path that lands on one names no class (D11).
    consts: HashSet<&'a str>,
    /// The last segment of a constant name → the names ending in it: a
    /// path read in a nesting is looked up there, not in every constant
    /// (R7-02).
    nested: HashMap<&'a str, Vec<&'a str>>,
    /// Class names only, with their class definitions.
    classes: HashMap<&'a str, Def>,
    /// Owner → method name → definitions, instance methods then the
    /// methods of the class or module itself.
    methods: [HashMap<&'a str, HashMap<&'a str, Def>>; 2],
    /// Top-level functions (`def m` outside any class, module or block):
    /// name → file → its last definition there.
    functions: HashMap<&'a str, HashMap<&'a str, usize>>,
    /// Path → its `File` symbol, the target of an import.
    files: HashMap<&'a str, usize>,
    /// Path → the files its resolved `require`s load when the file does,
    /// each with the line of its `require`; a load under a condition, in
    /// a block or in a method body is not one of them (R13).
    requires: HashMap<&'a str, Vec<(u32, &'a str)>>,
    /// Path → the files loaded once it is: itself and, transitively, what
    /// it requires.
    loaded: RefCell<HashMap<&'a str, Rc<HashSet<&'a str>>>>,
    /// Every ancestor declaration.
    mixins: Vec<Mixin<'a>>,
    /// Declaring class or module → its declarations, in the order Ruby
    /// runs them (file, line, then edge, which puts the arguments of one
    /// call last first).
    mixins_of: HashMap<&'a str, Vec<usize>>,
    /// Hook method → the declarations of its body, in that order.
    hooks_of: HashMap<usize, Vec<usize>>,
    /// Edge → its declaration.
    mixin_at: HashMap<usize, usize>,
    /// Modules that `extend ActiveSupport::Concern`.
    concerns: HashSet<&'a str>,
    /// The modules that may be mixed into an object whose lookup the rules
    /// do not replay ([`Fact::Mixed`](super::Fact::Mixed)), with every one
    /// they may bring along (R5-05).
    spread: HashSet<&'a str>,
    /// Module names.
    modules: Vec<&'a str>,
    /// Lookup chains and own lists already walked: a generation decides
    /// many sites from one owner.
    walks: Walks<'a>,
    projects: Projects<'a>,
    /// Methods by qualified name, every definition (D05).
    by_name: HashMap<&'a str, Vec<usize>>,
    /// An `alias` or `alias_method` → the method it copies, as written
    /// (R11-03).
    aliases: HashMap<usize, String>,
    facts: Facts,
    /// The associations a class body declares, and the methods they
    /// generate (ADR-0038).
    rails: values::Rails<'a>,
    /// The example groups of the program's spec files (ADR-0041).
    rspec: rspec::Rspec,
    /// The methods of no owner, by file and line: an example group's
    /// `def`s among them.
    unowned: HashMap<(&'a str, u32), Vec<usize>>,
    /// Spec file → the modules it mixes into objects the rules cannot
    /// name (`stubs::spread_by`), read once.
    spread_by: RefCell<HashMap<&'a str, Option<Rc<HashSet<&'a str>>>>>,
    /// The indexes of the projects that load this one as a path gem: a
    /// call on `self` must find the same definition from their objects too
    /// (F1 across projects, R6).
    loaders: Vec<Index<'a>>,
}

impl<'a> Index<'a> {
    /// The index of the rows of the files `visible` accepts; every file's
    /// `File` symbol, which a `require` may name across projects.
    /// With `tests`, what test files define on objects the rules cannot
    /// name taints the lookups as any such definition does; without, it
    /// taints none ([`test_file`]).
    fn build(world: &World<'a>, visible: &dyn Fn(&str) -> bool, tests: &Tests) -> Index<'a> {
        let symbols = world.symbols;
        let mut index = Index {
            symbols,
            by_uid: HashMap::with_capacity(symbols.len()),
            constants: HashMap::new(),
            classes: HashMap::new(),
            methods: [HashMap::new(), HashMap::new()],
            functions: HashMap::new(),
            files: HashMap::new(),
            requires: HashMap::new(),
            loaded: RefCell::new(HashMap::new()),
            mixins: Vec::new(),
            mixins_of: HashMap::new(),
            hooks_of: HashMap::new(),
            mixin_at: HashMap::new(),
            concerns: HashSet::new(),
            spread: HashSet::new(),
            modules: Vec::new(),
            walks: Walks::default(),
            projects: Projects::build(world.files),
            consts: HashSet::new(),
            nested: HashMap::new(),
            by_name: HashMap::new(),
            aliases: HashMap::new(),
            facts: Facts::read(world.files, symbols, visible),
            rails: values::Rails::default(),
            rspec: rspec::Rspec::read(world.files, visible),
            unowned: HashMap::new(),
            spread_by: RefCell::new(HashMap::new()),
            loaders: Vec::new(),
        };
        // A group's `def` is a method of an object the rules cannot name
        // unless RSpec's model reads the program.
        if !index.rspec.on() {
            for (file, name) in std::mem::take(&mut index.rspec.defs) {
                index.facts.taint_test(&file, &name);
            }
        }
        index.facts.group_mixins(index.rspec.on());
        index.facts.with_tests(tests);
        for (i, symbol) in symbols.iter().enumerate() {
            if symbol.kind != SymbolKind::File && !visible(&symbol.file) {
                continue;
            }
            index.by_uid.insert(symbol.uid.as_str(), i);
            let name = symbol.name.as_str();
            match symbol.kind {
                SymbolKind::Class => {
                    Def::add(&mut index.constants, name, symbols, i);
                    Def::add(&mut index.classes, name, symbols, i);
                }
                SymbolKind::Module => {
                    Def::add(&mut index.constants, name, symbols, i);
                    index.modules.push(name);
                }
                SymbolKind::Const => {
                    Def::add(&mut index.constants, name, symbols, i);
                    index.consts.insert(name);
                }
                SymbolKind::Method => {
                    if let Some((owner, side, method)) = split_method(name) {
                        let owned = index.methods[side as usize].entry(owner).or_default();
                        Def::add(owned, method, symbols, i);
                        // Ruby keeps whichever definition runs last, which
                        // a condition decides (D06).
                        if index.facts.unsettled(&symbol.file, name) {
                            owned.insert(method, Def::Ambiguous);
                        }
                        index.by_name.entry(name).or_default().push(i);
                    } else {
                        let at = (symbol.file.as_str(), symbol.line);
                        index.unowned.entry(at).or_default().push(i);
                    }
                }
                SymbolKind::Function => {
                    let in_file = index.functions.entry(name).or_default();
                    let last = in_file.entry(symbol.file.as_str()).or_insert(i);
                    if symbols[*last].line <= symbol.line {
                        *last = i;
                    }
                }
                SymbolKind::File => {
                    index.files.insert(name, i);
                }
                _ => {}
            }
        }
        for &name in index.constants.keys() {
            if let Some((_, last)) = name.rsplit_once("::") {
                index.nested.entry(last).or_default().push(name);
            }
        }
        let constants: Vec<&str> = index.constants.keys().copied().collect();
        index.facts.settle(&constants);
        let placed: HashMap<(String, Vec<String>), Vec<String>> = index
            .facts
            .written()
            .into_iter()
            .map(|(path, scope)| {
                let key = (path.to_owned(), scope.to_vec());
                (key, index.may_name(path, scope))
            })
            .collect();
        index.facts.place(&|path, scope| {
            placed
                .get(&(path.to_owned(), scope.to_vec()))
                .cloned()
                .unwrap_or_default()
        });
        // A method defined on a constant that may name another constant
        // (`ALIAS = Foo`) lands on a class the rules cannot name exactly
        // (R5-03).
        let tainted: Vec<Option<String>> = index
            .facts
            .defined_on_constants()
            .into_iter()
            .filter(|(path, scope, _)| {
                placed
                    .get(&((*path).to_owned(), scope.to_vec()))
                    .is_some_and(|named| named.iter().any(|n| index.consts.contains(n.as_str())))
            })
            .map(|(_, _, name)| name.map(str::to_owned))
            .collect();
        for name in &tainted {
            index.facts.taint(name.as_deref());
        }
        index.read_mixins(world.edges);
        index.read_requires(world.edges);
        index.read_rails(world, visible);
        // `class Alias` for a constant `Alias = Foo` reopens whatever
        // `Alias` holds, and so does `class Alias::Bar` (`Foo::Bar`): what
        // their bodies define, and what they mix in, lands on a class the
        // rules cannot name, and nothing written there knows its `self`
        // (R5-03, R7-11); so for a segment `const_set` or `X::Y =` may
        // assign a class, whatever the namespace (R8-09).
        let modules: HashSet<&str> = index.modules.iter().copied().collect();
        let through_const = |name: &str| {
            index.consts.contains(name)
                || name
                    .match_indices("::")
                    .any(|(at, _)| index.consts.contains(&name[..at]))
                || name.split("::").any(|segment| index.facts.aliased(segment))
        };
        let aliased: Vec<&'a str> = index
            .constants
            .keys()
            .copied()
            .filter(|name| index.classes.contains_key(name) || modules.contains(name))
            .filter(|name| through_const(name))
            .collect();
        let mut reopened: Vec<&'a str> = Vec::new();
        for owner in aliased {
            index.facts.doubt(owner);
            let names: Vec<&str> = index
                .methods
                .iter()
                .filter_map(|owned| owned.get(owner))
                .flat_map(|methods| methods.keys().copied())
                .collect();
            for name in names {
                index.facts.taint(Some(name));
            }
            for &i in index.mixins_of.get(owner).into_iter().flatten() {
                let mixin = &index.mixins[i];
                if mixin.kind.op != Op::Superclass && !mixin.name.is_empty() {
                    let named = index.may_name(&mixin.name, &mixin.cref);
                    reopened.extend(named.iter().filter_map(|name| index.known(name)));
                }
            }
        }
        // A module mixed in through a constant whose value the rules do not
        // read (`PA = P1`, then `K2.prepend(PA)`), or one that may not be
        // assigned, may be any module of the graph, as such a step may be
        // (B5, R7-05); so may an expression mixed into a gem's class from
        // outside, whose objects the graph holds behind a step it cannot
        // name (R8-05).
        let aliased = |(path, scope): &(String, Vec<String>)| {
            index.facts.unread_constant(facts::last_segment(path))
                || placed
                    .get(&(path.clone(), scope.clone()))
                    .is_some_and(|named| named.iter().any(|n| index.consts.contains(n.as_str())))
        };
        let (mixed, spread) = index.facts.mixins_written();
        let spread_any = spread.iter().any(aliased);
        let mixed_any = spread_any || mixed.iter().any(aliased) || index.facts.opaque_elsewhere();
        let any = |yes: bool| {
            if yes {
                index.modules.clone()
            } else {
                Vec::new()
            }
        };
        let mixed: Vec<&'a str> = index
            .facts
            .mixed()
            .chain(index.facts.spread())
            .filter_map(|m| index.known(m))
            .chain(reopened.iter().copied())
            .chain(any(mixed_any))
            .collect();
        index.spread = index.brought(mixed);
        // What a module mixed into an object the rules cannot name defines
        // may be that object's, and so any object's (R5-04).
        let spread: Vec<&'a str> = index
            .facts
            .spread()
            .filter_map(|m| index.known(m))
            .chain(reopened)
            .chain(any(spread_any))
            .collect();
        for module in index.brought(spread) {
            let (any, touched) = index.facts.touched(module);
            let mut names: Vec<String> = touched.into_iter().map(str::to_owned).collect();
            names.extend(
                index.methods[Side::Instance as usize]
                    .get(module)
                    .into_iter()
                    .flat_map(|methods| methods.keys().map(|name| (*name).to_owned())),
            );
            if any {
                index.facts.taint(None);
            }
            for name in names {
                index.facts.taint(Some(&name));
            }
        }
        index
    }

    /// The modules `mixed`, and what each brings along: the modules its own
    /// declarations, its concern blocks and its hooks may name (any, for a
    /// hook the rules do not read, R8-06), any module code written in its
    /// body mixes in where the rules do not replay it (a hook's unread
    /// statement, which acts on the object the hook runs for, R7-09), and
    /// those nested in it (`ClassMethods`), read from the declarations and
    /// facts alone, so before any chain is walked.
    fn brought(&self, mixed: Vec<&'a str>) -> HashSet<&'a str> {
        // What each module nests and declares, read once: a scan of every
        // module and declaration per module brought would be quadratic.
        let mut nested: HashMap<&str, Vec<&'a str>> = HashMap::new();
        for &module in &self.modules {
            let mut end = module.len();
            while let Some(at) = module[..end].rfind("::") {
                nested.entry(&module[..at]).or_default().push(module);
                end = at;
            }
        }
        let mut declared: HashMap<&str, Vec<&Mixin<'a>>> = HashMap::new();
        for mixin in self.mixins.iter().filter(|mixin| !mixin.name.is_empty()) {
            let declarer = self.symbols[mixin.src].name.as_str();
            let by = match mixin.kind.on {
                On::Hook => split_method(declarer).map_or("", |(owner, _, _)| owner),
                _ => declarer,
            };
            declared.entry(by).or_default().push(mixin);
        }
        let mut written: HashMap<&str, Vec<(&str, &[String])>> = HashMap::new();
        for (path, scope) in self.facts.mixins_written().0 {
            if let Some(innermost) = scope.first() {
                let paths = written.entry(innermost.as_str()).or_default();
                paths.push((path.as_str(), scope.as_slice()));
            }
        }
        let mut spread = HashSet::new();
        let mut next = mixed;
        let mut any = false;
        while let Some(module) = next.pop() {
            if !spread.insert(module) {
                continue;
            }
            // A hook the rules do not read may mix any module into the
            // object the module is mixed into (R8-06).
            if !any && self.unknown_hook(module) {
                any = true;
                next.extend(self.modules.iter().copied());
            }
            next.extend(nested.get(module).into_iter().flatten());
            for mixin in declared.get(module).into_iter().flatten() {
                let named = self.may_name(&mixin.name, &mixin.cref);
                next.extend(named.iter().filter_map(|name| self.known(name)));
            }
            for (path, scope) in written.get(module).into_iter().flatten() {
                let named = self.may_name(path, scope);
                next.extend(named.iter().filter_map(|name| self.known(name)));
            }
        }
        spread
    }

    /// The file each resolved `require` loads, per requiring file.
    fn read_requires(&mut self, edges: &'a [Edge]) {
        for edge in edges.iter().filter(|e| e.kind == EdgeKind::Import) {
            let Some(&src) = self.by_uid.get(edge.src.as_str()) else {
                continue;
            };
            let context = Context::decode(&edge.context);
            if context
                .extra
                .get(1)
                .is_some_and(|extra| extra == CONDITIONAL)
            {
                continue;
            }
            if let Some(target) = self.import(src, &context) {
                let from = self.symbols[src].file.as_str();
                let to = self.symbols[target].file.as_str();
                self.requires.entry(from).or_default().push((edge.line, to));
            }
        }
    }

    /// The ancestor declarations among `edges`, by owner and by hook, in
    /// the order Ruby runs them; and the aliases, each with the method it
    /// copies.
    fn read_mixins(&mut self, edges: &'a [Edge]) {
        for (position, edge) in edges.iter().enumerate() {
            if edge.kind != EdgeKind::Reference {
                continue;
            }
            let context = Context::decode(&edge.context);
            if context.qualifier == ALIAS
                && let Some(&src) = self.by_uid.get(edge.src.as_str())
            {
                self.aliases.insert(src, context.name);
                continue;
            }
            let Some(kind) = context
                .qualifier
                .strip_prefix(MIXIN)
                .and_then(MixinKind::parse)
            else {
                continue;
            };
            let Some(&src) = self.by_uid.get(edge.src.as_str()) else {
                continue;
            };
            let owner = &self.symbols[src];
            let mixin = Mixin {
                edge: position,
                src,
                file: owner.file.as_str(),
                line: edge.line,
                kind,
                name: context.name,
                text: edge.target.as_str(),
                maybe: context.extra.iter().any(|extra| extra == CONDITIONAL),
                cref: context.cref,
            };
            if mixin.makes_concern() {
                self.concerns.insert(owner.name.as_str());
            }
            self.mixin_at.insert(position, self.mixins.len());
            self.mixins.push(mixin);
        }
        let mut order: Vec<usize> = (0..self.mixins.len()).collect();
        order.sort_by_key(|&i| {
            let mixin = &self.mixins[i];
            (mixin.file, mixin.line, mixin.edge)
        });
        for i in order {
            let mixin = &self.mixins[i];
            if mixin.kind.on == On::Hook {
                self.hooks_of.entry(mixin.src).or_default().push(i);
            } else {
                let owner = self.symbols[mixin.src].name.as_str();
                self.mixins_of.entry(owner).or_default().push(i);
            }
        }
    }

    /// The class or module the ancestor declaration at `edge` names, where
    /// Ruby evaluates it (D37).
    fn mixin_target(&self, edge: usize) -> Option<&'a str> {
        let mixin = &self.mixins[*self.mixin_at.get(&edge)?];
        match (mixin.kind.on, mixin.kind.op) {
            (On::Body | On::Singleton, op) if op != Op::Superclass => {
                let owner = self.symbols[mixin.src].name.as_str();
                self.resolved(owner, edge)
            }
            _ => self.named(mixin),
        }
    }

    fn decide(&self, position: usize, edge: &Edge) -> Decision {
        let src = *self.by_uid.get(edge.src.as_str())?;
        if self.unsure_caller(src) {
            return None;
        }
        let context = Context::decode(&edge.context);
        // Written in the body of `class << X` for an `X` this file may read
        // as the wrong class (B2): neither its nesting nor its `self` is
        // known.
        if context
            .cref
            .iter()
            .any(|scope| singleton_of(scope).is_some() && self.facts.doubtful(scope))
        {
            return None;
        }
        let decided = match edge.kind {
            EdgeKind::Call => self.call(src, edge.line, &context),
            // An ancestor declaration names a class or module where Ruby
            // evaluates it (extraction gives a superclass the nesting outside
            // its class); an argument that is not a constant
            // (`include(plugin)`) or a gem's constant names none.
            EdgeKind::Reference if context.qualifier.starts_with(MIXIN) => self
                .mixin_target(position)
                .and_then(|q| self.constants[q].target()),
            EdgeKind::Reference => self.method_reference(src, &context),
            EdgeKind::Route => self.route(src, &context),
            EdgeKind::Association => self.association(src, &context),
            EdgeKind::Import => self.import(src, &context),
        };
        decided.filter(|&target| {
            let symbol = &self.symbols[target];
            let method = split_method(&symbol.name).map_or(symbol.name.as_str(), |m| m.2);
            !self.facts.doubtful(&symbol.name)
                // An unread region may define a method of that name on a
                // class or module it does not name (B9), and so may code on
                // an object the rules cannot name (R5-03, R5-04).
                && !(matches!(symbol.kind, SymbolKind::Method | SymbolKind::Function)
                    && (self.facts.loose(method) || self.facts.tainted(method)))
        })
    }

    /// Whether every site of `src` is beyond proof: its class or module is
    /// one another file may move (D09), or one an unread region may reopen
    /// (D12), so neither its `self` nor its nesting is known.
    fn unsure_caller(&self, src: usize) -> bool {
        let caller = &self.symbols[src];
        let owner = match caller.kind {
            SymbolKind::Class | SymbolKind::Module => caller.name.as_str(),
            // A method recorded on a class another file may contradict
            // (B2).
            SymbolKind::Method if self.facts.doubtful(&caller.name) => return true,
            SymbolKind::Method => match split_method(&caller.name) {
                Some((owner, _, _)) => owner,
                None => return false,
            },
            _ => return false,
        };
        self.facts.doubtful(owner) || self.facts.unread_owner(owner)
    }

    /// A call: without receiver or on `self`, Ruby's lookup from the
    /// caller, unless `self` there is not the caller's (a block run
    /// elsewhere, D08); on a constant, the constant rules; on a value path,
    /// what the value's class finds when Rails' conventions type it
    /// (`values`, ADR-0038); on anything else nothing, since no rule knows
    /// the receiver's class.
    fn call(&self, src: usize, line: u32, context: &Context) -> Decision {
        let receiver = context.qualifier.trim();
        let name = context.name.as_str();
        let group = rspec::group_of(context);
        if receiver.is_empty() || receiver == "self" {
            // In an example of an RSpec group: what the groups find.
            if let Some(group) = group {
                return self.example_call(src, group, name);
            }
            if rebound(context) {
                return None;
            }
            return self.self_call(src, line, name);
        }
        if let Some(path) = receiver.strip_prefix(VALUE) {
            return self.value_call(src, group, &context.cref, path, name);
        }
        self.constant_call(&context.cref, receiver, name)
    }

    /// A call sent to `self` (v1 R1, with the MVP's own-method and
    /// top-level rules): the method Ruby's lookup from the caller finds
    /// first. A class or module body runs once, on the class or module
    /// itself. A method runs on whatever object `self` may be there, so
    /// the lookup from each must agree ([`Self::dispatch`], F1). When no
    /// step of a chain known to its end defines the name, Ruby reaches
    /// `Object`, where a top-level `def` lives.
    fn self_call(&self, src: usize, line: u32, name: &str) -> Decision {
        let caller = &self.symbols[src];
        let file = caller.file.as_str();
        let (owner, side, exact) = match caller.kind {
            SymbolKind::Class | SymbolKind::Module => (caller.name.as_str(), Side::Singleton, true),
            // A method of no owner (a `def` in a top-level block) runs on
            // an object the rules cannot name.
            SymbolKind::Method => {
                let (owner, side, _) = split_method(&caller.name)?;
                (owner, side, false)
            }
            // Top-level code and top-level functions run with `main`, an
            // `Object`, as `self`; top-level code runs as the file loads.
            SymbolKind::File => return self.function(name, file, Some(line)),
            // A top-level function runs on whatever object calls it, which
            // may be one an owner's instance method defined `name` on
            // (#106).
            _ if self.facts.front_named(name) => return None,
            _ => return self.function(name, file, None),
        };
        let found = if exact {
            self.lookup(owner, side, name)
        } else {
            self.dispatch(owner, side, name)
        };
        match found {
            Lookup::Found(id) => Some(id),
            Lookup::Abstain => None,
            // `self` in an instance method never reaches a method of the
            // class itself, nor the reverse: a name only the other side's
            // chain defines is not this call's target, however unique.
            Lookup::NotFound => {
                if self.lookup(owner, side.other(), name) != Lookup::NotFound
                    || !self.reaches_object(owner, side)
                    || !self.reach(owner, side)
                    || !self.loaders.iter().all(|loader| {
                        loader.reaches_object(owner, side) && loader.reach(owner, side)
                    })
                {
                    return None;
                }
                self.function(name, file, None)
            }
        }
    }

    /// What a call on `self` in a method of `owner` on `side` reaches (F1).
    /// `self` there is the object the method runs on: an instance of the
    /// class or of any subclass, of any class that includes or prepends the
    /// module (or of their subclasses), the class itself or any subclass, a
    /// class or module extending the module. Lookup from `owner` and from
    /// each of them must find the same definition, or none at all; a
    /// subclass overriding the name (once-campfire's
    /// `Rooms::ClosedsController#room_scope`) leaves it unresolved.
    ///
    /// A core class or module has subclasses and includers the graph does
    /// not hold, whose own methods come first (every class for `Object`,
    /// `Integer` for `Numeric`, Active Support's `SafeBuffer` for `String`):
    /// a call on `self` there proves nothing (R5); nor in a module included
    /// where the rules do not replay it (`X.include M`, R15).
    fn dispatch(&self, owner: &'a str, side: Side, name: &str) -> Lookup {
        let own = self.lookup(owner, side, name);
        if own == Lookup::Abstain {
            return own;
        }
        // A module's own methods run on the module alone.
        let shared = side == Side::Instance || self.classes.contains_key(owner);
        if (shared && self.maybe_core(owner)) || self.spread.contains(owner) {
            return Lookup::Abstain;
        }
        // An object whose chain was cut may hold `owner` past the cut
        // (R5-01).
        if std::iter::once(self)
            .chain(&self.loaders)
            .any(|index| index.held_past_a_cut(owner, side))
        {
            return Lookup::Abstain;
        }
        // Every object whose lookup chain holds `owner`'s step on `side`,
        // with the index that knows its chain: this project's, and in each
        // project loading this one, the owner itself and every such object
        // there.
        let agree = self.agree(owner, side, name, own)
            && self.loaders.iter().all(|loader| {
                loader.lookup(owner, side, name) == own && loader.agree(owner, side, name, own)
            })
            && std::iter::once(self)
                .chain(&self.loaders)
                .all(|index| index.opaque_agree(owner, side, name, own));
        if agree { own } else { Lookup::Abstain }
    }

    /// Whether Ruby's lookup from `owner` on `side` reaches `Object`, where
    /// top-level functions live, through steps the graph knows: an
    /// instance of a class whose chain is known to its end. An instance
    /// method of a module runs on an instance of whatever includes it,
    /// whose own chain (an Active Record model's gem superclass) comes
    /// between the module and `Object`; a class or module itself reaches
    /// `Object` only past `Module` (and `Class`), whose methods, Ruby's and
    /// those a library adds (Active Support's `Module#delegate`), the graph
    /// does not hold (Ruby 3.3: a top-level `def name` is not what `name`
    /// in `def self.x` calls).
    fn reaches_object(&self, owner: &'a str, side: Side) -> bool {
        side == Side::Instance
            && self.classes.contains_key(owner)
            && self.chain(owner, side).is_known()
    }

    /// The top-level function `name` reachable from code of `file`: a
    /// private method of `Object` once the file defining it is loaded, so
    /// one defined in `file` itself or in a file it requires, transitively.
    /// Defined in two files, it is the one loaded last, which the graph
    /// cannot tell (decision 5), whether or not `file` loads both. From
    /// top-level code at line `before`, which runs as the file loads, only
    /// one defined above it in `file`, or in a file a `require` above it
    /// loads (D34) and that does not require `file` back (M1): Ruby raises
    /// NameError on the others.
    fn function(&self, name: &str, file: &'a str, before: Option<u32>) -> Decision {
        // An unread region may define it too (D12), and so may a reopened
        // `Object`: a method of `Object` either way, which the one loaded
        // last defines (R5-15); `main`, or any object, may hold one of its
        // own (R5-04).
        if self.facts.unread_method(name) || self.object_defines(name) || self.facts.tainted(name) {
            return None;
        }
        let defined = self.functions.get(name)?;
        let [(path, &i)] = defined.iter().collect::<Vec<_>>()[..] else {
            return None;
        };
        if self.facts.unsettled(path, name) {
            return None;
        }
        let reached = match before {
            None => self.loaded(file).contains(path),
            Some(line) if *path == file => self.symbols[i].line < line,
            // A file that requires this one back may be what loaded it,
            // still loading where it does (M1).
            Some(line) => self
                .requires
                .get(file)
                .into_iter()
                .flatten()
                .filter(|(at, _)| *at < line)
                .any(|(_, required)| {
                    let loaded = self.loaded(required);
                    loaded.contains(path) && !loaded.contains(file)
                }),
        };
        reached.then_some(i)
    }

    /// Whether a reopened `Object`, or a module prepended to it, defines
    /// `name` as an instance method, or may: where a top-level `def` puts
    /// it, or before. `Kernel` and what `Object` includes come after. Code
    /// evaluated on `Object` from outside its body (`Object.prepend(P)`,
    /// `Object.class_eval`, a `remove_method` sent to it) may, whether or
    /// not the graph reopens it (R6-01).
    fn object_defines(&self, name: &str) -> bool {
        if self.facts.changed(facts::OBJECT, Side::Instance, name) {
            return true;
        }
        let Some(object) = self.known(facts::OBJECT) else {
            return false;
        };
        for (owner, side) in self.chain(object, Side::Instance).steps() {
            if self.defines(owner, side, name) || self.unsure(owner, side, name) {
                return true;
            }
            if owner == object {
                break;
            }
        }
        false
    }

    /// Whether the code at `earlier`, written in a body of the class or
    /// module `by`, has run when the code at `later` runs. `Some(false)`
    /// below it in the same file; `Some(true)` above it, in a file that
    /// `later`'s file requires above it (directly or not), or in the one
    /// file defining `by`, which code elsewhere names only once that file
    /// is loaded (a file is taken to finish loading before another uses
    /// what it defines; a cyclic load is a known residual); `None` when the
    /// load order decides, which the graph cannot tell.
    fn before(&self, earlier: (&str, u32), by: &str, later: (&'a str, u32)) -> Option<bool> {
        if earlier.0 == later.0 {
            return Some(earlier.1 < later.1);
        }
        let required = self
            .requires
            .get(later.0)
            .into_iter()
            .flatten()
            .filter(|(line, _)| *line < later.1)
            .any(|(_, required)| self.loaded(required).contains(earlier.0));
        let only_there = match self.constants.get(by) {
            Some(Def::One(i)) => self.symbols[*i].file == earlier.0,
            _ => false,
        };
        (required || only_there).then_some(true)
    }

    /// `file` and every file its resolved `require`s load, transitively.
    fn loaded(&self, file: &'a str) -> Rc<HashSet<&'a str>> {
        if let Some(loaded) = self.loaded.borrow().get(file) {
            return Rc::clone(loaded);
        }
        let mut loaded = HashSet::from([file]);
        let mut next = vec![file];
        while let Some(path) = next.pop() {
            for &(_, required) in self.requires.get(path).into_iter().flatten() {
                if loaded.insert(required) {
                    next.push(required);
                }
            }
        }
        let loaded = Rc::new(loaded);
        self.loaded.borrow_mut().insert(file, Rc::clone(&loaded));
        loaded
    }

    /// `Some` when `owner` defines `name` on `side` itself: its one
    /// definition, or `None` inside when two files define it, a condition
    /// picks one, or the owner may have removed it, hold it from an unread
    /// region or from code evaluated outside its body. `None` when the
    /// owner does not define it.
    fn method(&self, owner: &str, side: Side, name: &str) -> Option<Decision> {
        let def = self.methods[side as usize]
            .get(owner)
            .and_then(|methods| methods.get(name))?;
        if self.unsure(owner, side, name) {
            return Some(None);
        }
        Some(def.target())
    }

    /// The classes and modules the constant `path`, written in the nesting
    /// `scope`, may name: the nesting's answer when it has one; else the
    /// top-level one, and in a nesting, any `S::path`, which an ancestor of
    /// the innermost scope may hold (read before the chains are).
    fn may_name(&self, path: &str, scope: &[String]) -> Vec<String> {
        if let Some(rooted) = path.strip_prefix("::") {
            return self.known(rooted).into_iter().map(str::to_owned).collect();
        }
        if let Some(found) = self.lexical(scope, path) {
            return found.into_iter().map(str::to_owned).collect();
        }
        let suffix = format!("::{path}");
        let mut named: Vec<String> = self.known(path).into_iter().map(str::to_owned).collect();
        if !scope.is_empty() {
            named.extend(
                self.nested
                    .get(facts::last_segment(path))
                    .into_iter()
                    .flatten()
                    .filter(|c| c.ends_with(&suffix))
                    .map(|c| (*c).to_owned()),
            );
        }
        named
    }

    /// A method named by a callback, `send`, `alias_method` or `delegate`
    /// (v1 `decide_reference`): a callback or alias in a class or module
    /// body names an instance method of that owner, a `send` there a method
    /// of the owner itself; in a method body `send` and an alias take the
    /// method's side. The owner's own definition or an ancestor's, never an
    /// unrelated class's. A callback declared in a method is not one.
    fn method_reference(&self, src: usize, context: &Context) -> Decision {
        // A `send` or callback written where `self` is not the source's
        // (D08).
        if rebound(context) {
            return None;
        }
        let caller = &self.symbols[src];
        let qualifier = context.qualifier.as_str();
        let reference = match qualifier {
            "send" | "public_send" => Reference::Send,
            ALIAS => return self.aliased(src, context),
            DELEGATE => Reference::Alias,
            // Any other qualifier is one of v1's closed list of callbacks
            // (`before_action`, `validate`, ...): extraction records no other.
            _ => Reference::Callback,
        };
        let (owner, side) = match caller.kind {
            SymbolKind::Class | SymbolKind::Module => (
                caller.name.as_str(),
                if reference == Reference::Send {
                    Side::Singleton
                } else {
                    Side::Instance
                },
            ),
            SymbolKind::Method if reference != Reference::Callback => {
                split_method(&caller.name).map(|(o, side, _)| (o, side))?
            }
            _ => return None,
        };
        // A `send` in a class or module body runs once, on it; anything
        // else runs on whatever object the method or callback runs on.
        let found = if matches!(caller.kind, SymbolKind::Class | SymbolKind::Module)
            && reference == Reference::Send
        {
            self.lookup(owner, side, &context.name)
        } else {
            self.dispatch(owner, side, &context.name)
        };
        match found {
            Lookup::Found(id) => Some(id),
            Lookup::Abstain | Lookup::NotFound => None,
        }
    }

    /// The method `alias_method :new, :old` or `alias new old` copies
    /// (D05): Ruby copies the body `old` has when the alias runs, so the
    /// owner's last definition of `old` written above it in the same file
    /// (a later one is another body, `alias_method :old_save, :save` before
    /// a new `save`). With none above it, `old` comes from another file or
    /// an ancestor, loaded before or after in an order the graph cannot
    /// tell, unless the owner's lookup chain holds one definition of `old`:
    /// then the lookup's answer, never a definition below the alias.
    fn aliased(&self, src: usize, context: &Context) -> Decision {
        let alias = &self.symbols[src];
        let (owner, side, _) = split_method(&alias.name)?;
        let original = context.name.as_str();
        let separator = if side == Side::Instance { '#' } else { '.' };
        let qualified = format!("{owner}{separator}{original}");
        if self.facts.unsettled(&alias.file, &qualified) || self.unsure(owner, side, original) {
            return None;
        }
        let above = self
            .by_name
            .get(qualified.as_str())
            .into_iter()
            .flatten()
            .copied()
            .filter(|&i| self.symbols[i].file == alias.file && self.symbols[i].line < alias.line)
            .max_by_key(|&i| self.symbols[i].line);
        // A module prepended to the owner before the alias runs comes
        // first (B1).
        if above.is_some() {
            return self
                .prepended(owner, side, original, (alias.file.as_str(), alias.line))
                .unwrap_or(above);
        }
        let separator_of = |side: Side| if side == Side::Instance { '#' } else { '.' };
        let on_chain: usize = self
            .chain(owner, side)
            .steps()
            .collect::<HashSet<_>>()
            .into_iter()
            .map(|(o, s)| {
                let name = format!("{o}{}{original}", separator_of(s));
                self.by_name.get(name.as_str()).map_or(0, Vec::len)
            })
            .sum();
        if on_chain != 1 {
            return None;
        }
        match self.lookup(owner, side, original) {
            Lookup::Found(id)
                if !(self.symbols[id].file == alias.file && self.symbols[id].name == qualified) =>
            {
                Some(id)
            }
            _ => None,
        }
    }

    /// The action of a route's controller (v1 `route_action`): the
    /// controller, an absolute constant as Rails constantizes it, must be a
    /// class; the action is its own definition or an inherited one, never a
    /// same-name method of another class.
    fn route(&self, src: usize, context: &Context) -> Decision {
        let written = context.qualifier.strip_prefix(ROUTE)?;
        let controller = match written.strip_prefix(DRAWN) {
            None => written.to_owned(),
            Some(relative) => {
                let module = self.drawn_module(&self.symbols[src].file, 0)?;
                let path = join_module(&module, relative);
                format!("{}Controller", camelize_path(&path))
            }
        };
        let (&controller, _) = self.classes.get_key_value(controller.as_str())?;
        self.settled(controller, controller)?;
        match self.lookup(controller, Side::Instance, &context.name) {
            Lookup::Found(id) => Some(id),
            Lookup::Abstain | Lookup::NotFound => None,
        }
    }

    /// The controller module a drawn routes file is read in (D04): the one
    /// module of every `draw` of its application naming it, a draw in a
    /// drawn file in the module that file is read in. `None` when nothing
    /// draws it (Rails never reads it), when the draws disagree, or when
    /// one is computed or written in a scope the rules do not carry.
    fn drawn_module(&self, file: &str, depth: usize) -> Option<String> {
        let (app, name) = drawn_name(file)?;
        if depth > MAX_DRAW_DEPTH {
            return None;
        }
        let mut found: Option<String> = None;
        for (drawing, drawn, module) in self.facts.draws() {
            if app_of(drawing) != Some(app) || !(drawn.is_empty() || drawn == name) {
                continue;
            }
            if drawn.is_empty() {
                return None;
            }
            let module = match module.as_ref()?.strip_prefix(DRAWN) {
                None => module.clone()?,
                Some(relative) => join_module(&self.drawn_module(drawing, depth + 1)?, relative),
            };
            match &found {
                Some(other) if *other != module => return None,
                _ => found = Some(module),
            }
        }
        found
    }

    /// The model class an association of the class `src` names, looked up
    /// as Active Record's `compute_type` does (v1 `association_target`): in
    /// the owner's own namespace and each one around it (`Admin::Order`
    /// tries `Admin::Order::Item`, `Admin::Item`, then `Item`), an absolute
    /// `::Item` at the top only. The first name that is a class decides; a
    /// class reopened in two files has no one definition.
    fn association(&self, src: usize, context: &Context) -> Decision {
        let owner = &self.symbols[src];
        if !matches!(owner.kind, SymbolKind::Class | SymbolKind::Module) {
            return None;
        }
        let path = context.name.as_str();
        let candidates: Vec<String> = match path.strip_prefix("::") {
            Some(rooted) => vec![rooted.to_owned()],
            None => {
                let segments: Vec<&str> = owner.name.split("::").collect();
                (0..=segments.len())
                    .rev()
                    .map(|i| match segments[..i].join("::") {
                        prefix if prefix.is_empty() => path.to_owned(),
                        prefix => format!("{prefix}::{path}"),
                    })
                    .collect()
            }
        };
        let (&found, def) = candidates
            .iter()
            .find_map(|candidate| self.classes.get_key_value(candidate.as_str()))?;
        self.settled(path, found)?;
        def.target()
    }

    /// A `require` (or `require_relative`, `require_dependency`, `load`):
    /// the `File` symbol of the file its project's load roots hold.
    fn import(&self, src: usize, context: &Context) -> Decision {
        let path = context.extra.first()?;
        let importer = self.symbols[src].file.as_str();
        let target = self.projects.resolve(path, importer)?;
        self.files.get(target).copied()
    }
}

/// Whether the site is written where `self` is not what its source runs
/// on ([`REBOUND`]).
fn rebound(context: &Context) -> bool {
    context.extra.first().is_some_and(|extra| extra == REBOUND)
}

/// Most nested draws followed: a file drawn from itself is no route.
const MAX_DRAW_DEPTH: usize = 16;

/// The rule a method reference follows (v1 `ReferenceKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reference {
    Callback,
    Send,
    /// `alias_method`, `alias`, `delegate`: a method of the generated
    /// method's own owner and side.
    Alias,
}

#[cfg(test)]
mod tests;
