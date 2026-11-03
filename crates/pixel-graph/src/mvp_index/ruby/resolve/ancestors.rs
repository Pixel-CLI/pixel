// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Ruby's method lookup through the ancestors a class or module declares,
//! carried from v1's `pixel-graph` (`src/resolve/ruby.rs`, `lookup` and the
//! chains it walks), and rebuilt as Ruby 3.3 builds them.
//!
//! An owner's ancestors are built by replaying its declarations in the
//! order Ruby runs them (file, then line; the arguments of one call last
//! first, as `rb_mod_include` inserts them, D10), each an insertion into
//! one of two lists: its own (prepended modules, the owner, included
//! modules), and its singleton class's (singleton methods, extended
//! modules, the `include` and `prepend` of its `class << self`, D35), each
//! followed by its superclass's. An `include` inserts the module's
//! ancestors right after the owner, skipping each one already there, in
//! the owner's list or in the superclass's, and moving the insertion point
//! past one met after it; a `prepend` inserts them at the front, skipping
//! those already prepended (F3; `include_modules_at` in Ruby's `class.c`).
//! What the module's own declarations bring counts once they have run, as
//! a superclass's do below; one that may run after the owner's is carried
//! there later by Ruby 3.3, after the module, with no regard for what the
//! owner included in between: a step that may not be there (R12-02).
//! A superclass's declaration counts once it has run: above in the same
//! file, in a file required above, or in the one file defining that
//! superclass; one in a file loaded in an order the graph cannot tell
//! leaves the module a step that may not be there, which a lookup proves
//! nothing through unless the same module comes further on (B4).
//! An `extend` is an `include` into the singleton class; `extend self` in
//! a module puts the module's own list there; when an `include` or
//! `prepend` into the module may come later (Ruby 3.3 carries it there
//! too), or the module has its own `extended`, an unknown step stands for
//! that list.
//!
//! A module that `extend`s `ActiveSupport::Concern` follows its
//! `append_features`: included into a class, it first includes the
//! concerns it included itself (deferred until then), then itself, then
//! the class extends its `ClassMethods`, then its `included do` block runs
//! on the class (`include`, `extend` and `prepend` there act on the class,
//! D36); nothing at all when the class already has it. A `prepend` of one
//! is the mirror, with `prepended do`; what the block defines lands on the
//! class, unknown from the `include` on unless the class defines it again
//! below (B6). A module's `self.included(base)` hook, and `extended`,
//! `prepended`, and a superclass's `inherited`, run after the insertion
//! when they are defined by then (above in the same file, or in the one
//! file defining their module; B3), their `base.include(X)` and family
//! acting on the includer; any statement the rules do not read makes the
//! includer's lists unknown from their front (D14), and so does a module
//! hook that may come from a module outside the graph the hooked module
//! extends (R7; `ActiveSupport::Concern` defines no `extended`, and a
//! superclass's gem `inherited` is taken to change no chain), but not one
//! from what mixing a gem's module into it put on its singleton class,
//! which is taken to define none (#120, argued in `resolve`'s rules). An
//! object whose list such code left unknown may hold the module there, its
//! ancestors and the modules under it: a call on `self` in them counts it
//! (F1).
//! <https://docs.ruby-lang.org/en/master/Module.html#method-i-include>,
//! <https://api.rubyonrails.org/classes/ActiveSupport/Concern.html>.
//!
//! A constant an ancestor declaration names is looked up where Ruby
//! evaluates it (D37): the lexical nesting, then the ancestors of the
//! innermost scope as they are when the declaration runs (for the owner's
//! own declarations, the modules it has inserted so far and its
//! superclass's chain; in `class << self`, the singleton class's: the
//! modules the class extends so far, then its superclass's singleton
//! chain), then the top level.
//!
//! A class none of whose definitions declares a superclass inherits from
//! `Object`, unless it may be a reopened core or standard-library class
//! (`corelib`): one of their names, or one nested in their namespaces
//! (`URI::Generic`, `Net::HTTP`), whose own methods, superclass and
//! subclasses the graph does not hold (D31, R5-10). A gem's class reopened
//! without its superclass is read as a subclass of `Object`: a known
//! residual.
//!
//! A step the rules cannot name but that may be one of the graph's (a
//! superclass or mixin written as an expression, or as a constant whose
//! value the rules do not read) may hold any class or module: a call on
//! `self` resolves only when what such an object defines before it cannot
//! come first (B5, [`Index::opaque_agree`]). Since Ruby skips a module
//! already there, a module mixed in past such a step, past code the rules
//! do not play, or past a gem's class or module that the application
//! mixes it into elsewhere, may not be where it is inserted: a step that
//! may not be there (B4, R5-06). So is what a mixin under a condition, a
//! loop or a block that may run no time inserts (R5-07), and one in a
//! class body written under a condition, in a body a `rescue` guards or
//! past a statement that may leave early (R6-07, R6-09, R6-10, R8-04).
//!
//! Not modelled, each making the chain lack a step: `method_missing`,
//! refinements (whose definitions taint their names, R5-04), a module
//! included by a gem's own module. `remove_method`,
//! `undef`, a computed `define_method` in an owner's body, a block a
//! method may yield to there, code evaluated on the owner from outside
//! its body, a concern's `included do` (B6), and a class macro (a definer
//! in a method body, run on whatever class calls it, B7) make a lookup
//! that reaches the owner for the names they touch abstain
//! (`Fact::Unknown`, `Fact::Evaluated`, `Fact::Block`, `Fact::Macro`).

use rustc_hash::FxHashMap as HashMap;
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::VecDeque;
use std::hash::Hash;
use std::rc::Rc;

use self::held::{Behind, Contained, Hidden, Holders, Opaque, Shares};
use super::constants;
use super::corelib::{in_core_namespace, is_core};
use super::{Decision, Def, Index, Side};
use crate::mvp_index::ruby::unread::fits;
use crate::mvp_index::ruby::{singleton_of, singleton_scope};

/// Deepest nesting of ancestor walks (an include or a superclass is one
/// level); past it the rest is unknown, and so is the whole of each walk
/// nested in the one that met it ([`Walks`], R6-04).
pub(super) const MAX_ANCESTOR_DEPTH: usize = 32;

/// Most owners whose lists are built, or chains walked, inside one another,
/// whatever led from one to the next (a lookup a hook needs, a macro's
/// chain): past it the rest is unknown, so no input can exhaust the stack
/// (R5-01).
const MAX_NESTED_WALKS: usize = 2 * MAX_ANCESTOR_DEPTH;

/// One ancestor declaration, as extraction recorded it.
#[derive(Debug)]
pub(super) struct Mixin<'a> {
    /// The site's index in the generation's edges.
    pub(super) edge: usize,
    /// The declaring class or module, or the hook method.
    pub(super) src: usize,
    pub(super) file: &'a str,
    pub(super) line: u32,
    pub(super) kind: MixinKind,
    /// The constant as written; empty for an expression.
    pub(super) name: String,
    /// The declaration's target as written (an expression's text).
    pub(super) text: &'a str,
    pub(super) cref: Vec<String>,
    /// Written where it may not run when its body does: under a
    /// condition, a loop, a block that may run no time (R5-07).
    pub(super) maybe: bool,
}

impl Mixin<'_> {
    /// The literal `extend ActiveSupport::Concern`, which makes a concern.
    pub(super) fn makes_concern(&self) -> bool {
        self.kind.op == Op::Extend
            && self.kind.on == On::Body
            && self.name.trim_start_matches("::") == "ActiveSupport::Concern"
    }
}

/// What a declaration does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Op {
    Superclass,
    Include,
    Prepend,
    Extend,
}

/// Where a declaration is written, and so on what it acts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum On {
    /// The class or module body: on the owner.
    Body,
    /// The owner's `class << self`: on its singleton class.
    Singleton,
    /// A concern's `included do`: on the class including it.
    Included,
    /// A concern's `prepended do`: on the class prepending it.
    Prepended,
    /// A hook method (`self.included(base)`): on `base`.
    Hook,
}

/// What an ancestor declaration says (`MIXIN` + this, in a site's context).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MixinKind {
    pub(super) op: Op,
    pub(super) on: On,
}

impl MixinKind {
    pub(super) fn parse(text: &str) -> Option<MixinKind> {
        let (on, op) = match text.split_once(':') {
            None => (On::Body, text),
            Some(("singleton", op)) => (On::Singleton, op),
            Some(("included", op)) => (On::Included, op),
            Some(("prepended", op)) => (On::Prepended, op),
            Some(("hook", op)) => (On::Hook, op),
            Some(_) => return None,
        };
        let op = match op {
            "superclass" if on == On::Body => Op::Superclass,
            "include" => Op::Include,
            "prepend" => Op::Prepend,
            "extend" => Op::Extend,
            _ => return None,
        };
        Some(MixinKind { op, on })
    }
}

/// What Ruby's method lookup from an owner finds for a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Lookup {
    /// The one definition first in lookup order: the owner's own or an
    /// ancestor's.
    Found(usize),
    /// A definition exists past an ancestor the graph cannot name (a dynamic
    /// `include`, an external superclass), several ancestors define it in an
    /// order the graph cannot prove, or the first one defines it in two
    /// files: no target.
    Abstain,
    /// No owner on the chain defines it.
    NotFound,
}

/// The methods of an owner on one side, or an ancestor the graph cannot
/// name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Node<'a> {
    Known(&'a str, Side),
    Unknown,
}

/// The steps one owner's declarations, spread over several files, insert
/// on one side of it (`true`: before it, prepended): Ruby loads those files
/// in an order the graph cannot tell, so their order among themselves is
/// unknown.
type Region<'a> = (&'a str, Side, bool);

/// One step of a lookup chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Step<'a> {
    node: Node<'a>,
    region: Option<Region<'a>>,
    /// The declaration that inserted it, when an owner's own declaration
    /// did: whether it ran before a subclass's, or an alias.
    at: Option<At<'a>>,
    /// What an unknown step stands for.
    stands: Stands<'a>,
    /// The step may not be there: its module is skipped when the
    /// superclass already has it, which a declaration in a file loaded in
    /// an order the graph cannot tell may decide (B4).
    maybe: bool,
}

impl<'a> Step<'a> {
    /// An unknown step that stands for code the rules do not play.
    fn unread() -> Step<'a> {
        Step {
            stands: Stands::Unread,
            ..Step::new(Node::Unknown)
        }
    }

    /// An unknown step that stands for the rest of a walk cut short.
    fn cut() -> Step<'a> {
        Step {
            stands: Stands::Cut,
            ..Step::new(Node::Unknown)
        }
    }

    fn new(node: Node<'a>) -> Step<'a> {
        Step {
            node,
            region: None,
            at: None,
            stands: Stands::Any,
            maybe: false,
        }
    }
}

/// Where a declaration is written: its file and line, and the class or
/// module whose declaration it is (the owner being built).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct At<'a> {
    pub(super) file: &'a str,
    pub(super) line: u32,
    pub(super) by: &'a str,
}

/// What an unknown step stands for, as far as hooks go (R7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Stands<'a> {
    /// A gem's class or module, or a core one: anything of its own, but
    /// of the graph's modules only one the application mixes in where the
    /// rules do not replay it (R5-06).
    Any,
    /// What code the rules do not play did: anything, any module of the
    /// graph included (R5-06).
    Unread,
    /// The rest of a walk cut short (the depth cap, a cycle): anything, a
    /// hook, a constant or a definition of the graph's included, so no
    /// lookup through it proves anything (R5-01, R6-04).
    Cut,
    /// `ActiveSupport::Concern` itself, a gem's module: its methods are
    /// the concern rules, none of them `extended` or `extend_object`.
    Concern,
    /// What mixing a gem's module into a class or module of the graph may
    /// have put on its singleton class (a gem concern's `ClassMethods`):
    /// methods of its own and, of the graph's modules, only one mixed in
    /// where the rules do not replay it, as [`Stands::Any`]; but no hook
    /// of the module's, so the gem cannot change what mixing the module
    /// into the application's classes inserts (#120).
    Gem,
    /// The module's own list after `extend self`, with what an `include`
    /// the rules cannot place adds to it.
    Itself(&'a str),
    /// What an expression or a constant the rules cannot follow names,
    /// which may be one of the graph's modules, or, for a superclass
    /// (`classes`), one of its classes too (B5).
    Opaque { classes: bool },
}

/// A walked lookup chain.
#[derive(Debug, Default)]
pub(super) struct Chain<'a> {
    steps: Vec<Step<'a>>,
}

impl<'a> Chain<'a> {
    /// A chain of one step that stands for a walk cut short.
    fn cut() -> Chain<'a> {
        Chain {
            steps: vec![Step::cut()],
        }
    }

    /// Whether a step stands for a walk cut short ([`Stands::Cut`]).
    pub(super) fn is_cut(&self) -> bool {
        self.steps
            .iter()
            .any(|step| step.node == Node::Unknown && step.stands == Stands::Cut)
    }

    /// No step is an ancestor the graph cannot name: past the chain, Ruby
    /// reaches `Object`.
    pub(super) fn is_known(&self) -> bool {
        self.steps.iter().all(|step| step.node != Node::Unknown)
    }

    /// The owners of the chain the graph names, in lookup order.
    pub(super) fn owners(&self) -> impl Iterator<Item = &'a str> + '_ {
        self.steps().map(|(owner, _)| owner)
    }

    /// The first step, when the graph names it.
    pub(super) fn first(&self) -> Option<(&'a str, Side)> {
        match self.steps.first()?.node {
            Node::Known(owner, side) => Some((owner, side)),
            Node::Unknown => None,
        }
    }

    /// The owners of the chain the graph names, in lookup order, each with
    /// whether its step may not be there (B4, R5-07).
    pub(super) fn owners_maybe(&self) -> impl Iterator<Item = (&'a str, Side, bool)> + '_ {
        self.steps.iter().filter_map(|step| match step.node {
            Node::Known(owner, side) => Some((owner, side, step.maybe)),
            Node::Unknown => None,
        })
    }

    /// The steps the graph names, each an owner and a side, in lookup
    /// order.
    pub(super) fn steps(&self) -> impl Iterator<Item = (&'a str, Side)> + '_ {
        self.steps.iter().filter_map(|step| match step.node {
            Node::Known(owner, side) => Some((owner, side)),
            Node::Unknown => None,
        })
    }
}

/// An owner's own list on one side: the steps before it (prepended), its
/// own (`origin`; a core class's own step is followed by an unknown one),
/// and those after it (included).
#[derive(Clone, Debug)]
struct Part<'a> {
    steps: Vec<Step<'a>>,
    origin: usize,
}

impl<'a> Part<'a> {
    fn new(owner: &'a str, side: Side, core: bool) -> Part<'a> {
        let mut steps = vec![Step::new(Node::Known(owner, side))];
        if core {
            steps.push(Step::new(Node::Unknown));
        }
        Part {
            origin: steps.len() - 1,
            steps,
        }
    }

    fn cut() -> Part<'a> {
        Part {
            steps: vec![Step::cut()],
            origin: 0,
        }
    }
}

/// An owner's own lists, with the class or module each of its own
/// declarations names.
#[derive(Debug)]
pub(super) struct Parts<'a> {
    instance: Part<'a>,
    singleton: Part<'a>,
    /// Edge → the ancestor its declaration names (D37).
    resolved: HashMap<usize, Option<&'a str>>,
    /// For a concern, the concerns it included or prepended, deferred to
    /// its includers (Active Support's `@_dependencies`), in their order,
    /// each with whether its declaration may not run (R5-07).
    dependencies: Vec<(&'a str, bool)>,
    /// The modules a declaration mixed in through code the rules do not
    /// play (R7), by the side of the owner where it acted.
    replaced: Vec<(Side, &'a str)>,
    /// What concerns' `included do` blocks defined on the owner, by side
    /// (`None`: any name), each with the `include` that ran it (B6).
    evaluated: Vec<(Side, Option<String>, At<'a>)>,
}

impl Parts<'_> {
    /// Own lists of one step each that stands for a walk cut short.
    fn cut() -> Self {
        Parts {
            instance: Part::cut(),
            singleton: Part::cut(),
            resolved: HashMap::default(),
            dependencies: Vec::new(),
            replaced: Vec::new(),
            evaluated: Vec::new(),
        }
    }
}

/// The superclass an owner declares.
#[derive(Clone, Copy, Debug)]
pub(super) enum Superclass<'a> {
    /// None declared: the class inherits from `Object`, where top-level
    /// functions live.
    None,
    Known(&'a str),
    /// One the graph cannot name, or reopenings that disagree.
    Unknown,
    /// One the rules cannot follow, which may be a class of the graph
    /// (B5).
    Opaque,
}

/// The object a declaration acts on: the owner, or its singleton class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Object {
    Owner,
    SingletonClass,
}

/// The declaration of the owner being built that an insertion comes from.
#[derive(Clone, Copy, Debug)]
struct Decl<'a> {
    at: (&'a str, u32),
    /// The owner's declarations come from several files.
    uncertain: bool,
}

/// The owner being built.
struct Building<'a, 'i> {
    /// Whether one declaration ran before another (`Index::before`).
    index: &'i Index<'a>,
    owner: &'a str,
    concern: bool,
    /// The concerns this concern defers (`@_dependencies`).
    dependencies: VecDeque<(&'a str, bool)>,
    /// The declaration being played may not run: what it inserts, its
    /// hooks' and its concern's included, may not be there (R5-07).
    maybe: bool,
    /// [`Parts::replaced`].
    replaced: Vec<(Side, &'a str)>,
    /// [`Parts::evaluated`].
    evaluated: Vec<(Side, Option<String>, At<'a>)>,
    /// The declarations being played, innermost last: one played again
    /// inside itself is a hook or a concern dependency that mixes its own
    /// module back in, which Ruby repeats until its stack overflows (R5-01).
    acting: Vec<(Object, Op, &'a str)>,
    /// The hook methods being run, innermost last.
    running: Vec<usize>,
    instance: Part<'a>,
    singleton: Part<'a>,
    tail_instance: Vec<Step<'a>>,
    tail_singleton: Vec<Step<'a>>,
}

/// Owners, read once and shared.
type Owners<'a> = Rc<[&'a str]>;

/// The walks of one generation. What a walk builds depends only on the
/// owner, unless the walk met the depth cap (then on the depth it started
/// at too) or a cycle (then on the walks around it): each is kept as far as
/// it holds, so no decision depends on the order sites are decided in. A
/// walk nested in another that met the cap is kept as wholly unknown, not
/// as the steps it reached: keeping those at every depth an owner is met
/// cost each owner past the cap the square of the cap in steps (R6-04).
/// What it hid (a hook, a definition, a constant) may be anywhere past the
/// cut, so no lookup, hook or constant is proven through it
/// ([`Stands::Cut`]).
#[derive(Default)]
pub(super) struct Walks<'a> {
    parts: Kept<&'a str, Rc<Parts<'a>>>,
    chains: Kept<(&'a str, Side), Rc<Chain<'a>>>,
    superclasses: Kept<&'a str, Superclass<'a>>,
    /// The owners whose own lists are being built, innermost last.
    building: RefCell<Vec<&'a str>>,
    /// The chains being walked.
    walking: RefCell<Vec<(&'a str, Side)>>,
    /// How deep the current walk is nested.
    depth: Cell<usize>,
    /// The deepest level the current walk reached.
    reached: Cell<usize>,
    /// The current walk met the depth cap.
    capped: Cell<bool>,
    /// The current walk met an owner being built (a cycle).
    cyclic: Cell<bool>,
    /// (owner, side) → every object whose lookup chain holds that step.
    containers: RefCell<Option<Rc<Contained<'a>>>>,
    /// The steps before each step the rules cannot name, indexed.
    behind: RefCell<Option<Rc<Opaque<'a>>>>,
    /// The objects whose chains are the same past their own step, and
    /// (owner, side) → the objects holding that step, so grouped (R10-01).
    shares: OnceCell<Rc<Shares<'a>>>,
    held: RefCell<Holders<'a>>,
    /// (owner, side) → whether every object holding that step reaches
    /// `Object`.
    reach: RefCell<HashMap<(&'a str, Side), bool>>,
    /// (owner, side) → the owners of its chain whose instance methods
    /// define on the singleton class of the object they run on
    /// ([`Index::fronted`]), for chains read outside any walk.
    fronts: RefCell<HashMap<(&'a str, Side), Owners<'a>>>,
    /// The wholly unknown chain and own lists, shared by every walk kept
    /// as such.
    unread_chain: OnceCell<Rc<Chain<'a>>>,
    unread_parts: OnceCell<Rc<Parts<'a>>>,
}

/// What walks built: those that met neither the cap nor a cycle, with how
/// many levels deep they went; those that met the cap only, by the depth
/// they started at.
struct Kept<K, V> {
    whole: RefCell<HashMap<K, (V, usize)>>,
    capped: RefCell<HashMap<(K, usize), V>>,
}

impl<K, V> Default for Kept<K, V> {
    fn default() -> Self {
        Kept {
            whole: RefCell::new(HashMap::default()),
            capped: RefCell::new(HashMap::default()),
        }
    }
}

impl<'a> Walks<'a> {
    /// A kept value valid at the current depth: a whole one whose walk
    /// went `height` levels deep and stays within the cap from here, or a
    /// capped one started at this depth.
    fn kept<K: Eq + Hash + Clone, V: Clone>(&self, kept: &Kept<K, V>, key: &K) -> Option<V> {
        let depth = self.depth.get();
        if let Some((value, height)) = kept.whole.borrow().get(key)
            && depth + height <= MAX_ANCESTOR_DEPTH
        {
            self.reached.set(self.reached.get().max(depth + height));
            return Some(value.clone());
        }
        let value = kept.capped.borrow().get(&(key.clone(), depth)).cloned()?;
        self.capped.set(true);
        self.reached
            .set(self.reached.get().max(MAX_ANCESTOR_DEPTH + 1));
        Some(value)
    }

    /// Runs `walk` at the current depth and keeps what it built as far as
    /// it holds; a walk below the top that met the cap is `unknown` (R6-04).
    fn keep<K: Eq + Hash + Clone, V: Clone>(
        &self,
        kept: &Kept<K, V>,
        key: K,
        walk: impl FnOnce() -> V,
        unknown: impl FnOnce() -> V,
    ) -> V {
        let depth = self.depth.get();
        let outer = (
            self.capped.replace(false),
            self.cyclic.replace(false),
            self.reached.replace(depth),
        );
        let mut value = walk();
        let (capped, cyclic, reached) = (self.capped.get(), self.cyclic.get(), self.reached.get());
        self.capped.set(outer.0 || capped);
        self.cyclic.set(outer.1 || cyclic);
        self.reached.set(outer.2.max(reached));
        if capped && depth > 0 {
            value = unknown();
        }
        if !cyclic {
            if capped {
                kept.capped.borrow_mut().insert((key, depth), value.clone());
            } else {
                kept.whole
                    .borrow_mut()
                    .insert(key, (value.clone(), reached - depth));
            }
        }
        value
    }
}

impl<'a> Index<'a> {
    /// Looks `name` up from a method of `owner` on `side`, in Ruby's order.
    pub(super) fn lookup(&self, owner: &'a str, side: Side, name: &str) -> Lookup {
        let chain = self.chain(owner, side);
        // A step another file may move (D09), or a walk cut short, which
        // may hold anything past the cut (R6-04), leaves the whole lookup
        // unproven.
        if chain.is_cut() || chain.owners().any(|o| self.facts.doubtful(o)) {
            return Lookup::Abstain;
        }
        self.lookup_in(&chain.steps, name)
    }

    /// Looks `name` up along `steps`, in their order, whatever their
    /// owners' names.
    fn lookup_in(&self, steps: &[Step<'a>], name: &str) -> Lookup {
        let defines =
            |step: &Step| matches!(step.node, Node::Known(o, s) if self.defines(o, s, name));
        // The first step that may hold the name or not: `remove_method`,
        // `undef`, a computed `define_method` (F4, D16), code evaluated
        // from outside the owner's body (R9), an unread region (D12): from
        // the front of that owner's own list, where what it prepends goes.
        let unsure =
            |step: &Step| matches!(step.node, Node::Known(o, s) if self.unsure(o, s, name));
        let Some(first) = steps.iter().position(defines) else {
            return if steps.iter().any(unsure) {
                Lookup::Abstain
            } else {
                Lookup::NotFound
            };
        };
        // An unsure step whose own list starts at or before the first
        // definition: every step up to it, and a later one only when it
        // sits in the same own list. A step past both cannot matter, so
        // its facts are not read (most of a long chain).
        if steps[..first].iter().any(|step| step.node == Node::Unknown)
            || steps
                .iter()
                .enumerate()
                .any(|(j, step)| (j <= first || front(steps, j) <= first) && unsure(step))
        {
            return Lookup::Abstain;
        }
        // A step that may not be there: without it, lookup must find the
        // same module further on, at a step that is there (B4, R5-07).
        let mut at = first;
        while steps[at].maybe {
            let next = steps[at + 1..]
                .iter()
                .position(|step| step.node == Node::Unknown || defines(step) || unsure(step));
            match next.map(|k| at + 1 + k) {
                Some(k) if steps[k].node == steps[first].node => at = k,
                _ => return Lookup::Abstain,
            }
        }
        // Another step of the same unordered region that defines the name,
        // or may, may come first.
        if let Some(region) = steps[first].region
            && steps.iter().enumerate().any(|(j, step)| {
                j != first
                    && step.region == Some(region)
                    && (step.node == Node::Unknown || defines(step))
            })
        {
            return Lookup::Abstain;
        }
        let Node::Known(hit_owner, hit_side) = steps[first].node else {
            return Lookup::Abstain;
        };
        self.hit(hit_owner, hit_side, name)
    }

    /// What lookup finds at `owner`'s step on `side`, the first that
    /// defines `name`: its one definition.
    fn hit(&self, owner: &str, side: Side, name: &str) -> Lookup {
        match self.method(owner, side, name) {
            Some(Some(id)) => Lookup::Found(id),
            _ => Lookup::Abstain,
        }
    }

    /// Whether a lookup through `owner` on `side` cannot know whether it
    /// holds `name`: a method of that name defined on an object the rules
    /// cannot name, which may be any (R5-03, R5-04), `remove_method`,
    /// `undef`, a computed `define_method` (F4, D16), code evaluated from
    /// outside the owner's body (R9), an unread region that may reopen it
    /// (D12), a concern's block or a class macro (B6, B7).
    pub(super) fn unsure(&self, owner: &str, side: Side, name: &str) -> bool {
        self.facts.tainted(name)
            || self.facts.unknown(owner, side, name)
            || self.facts.changed(owner, side, name)
            || self.facts.hidden(owner, name)
            || self.block_defined(owner, side, name)
            || self.macro_defined(owner, side, name)
            || self.fronted(owner, side, name)
    }

    /// Whether the chain of `owner` on `side` holds the instance step of an
    /// owner whose instance methods may define `name` on the singleton
    /// class of the object they run on ([`Fact::Fronted`]): such an object
    /// is `owner` itself (its instances, or the class or module), or one
    /// whose chain holds `owner`'s step, which then holds that step too,
    /// so the step is unsure from the front of every chain that may meet
    /// the method (#106). Every other chain is left alone.
    ///
    /// [`Fact::Fronted`]: crate::mvp_index::ruby::Fact::Fronted
    fn fronted(&self, owner: &str, side: Side, name: &str) -> bool {
        if !self.facts.front_named(name) {
            return false;
        }
        if side == Side::Instance && self.facts.fronts(owner, name) {
            return true;
        }
        match self.fronts_held(owner, side) {
            Some(held) => held.iter().any(|front| self.facts.fronts(front, name)),
            None => true,
        }
    }

    /// Whether the chain of `owner` on `side` holds the instance step of
    /// an owner whose instance methods may define some name on the
    /// singleton class of the object they run on ([`Self::fronted`]).
    pub(super) fn fronting(&self, owner: &str, side: Side) -> bool {
        self.facts.any_front()
            && ((side == Side::Instance && self.facts.fronting(owner))
                || self
                    .fronts_held(owner, side)
                    .is_none_or(|held| !held.is_empty()))
    }

    /// The owners of the chain of `owner` on `side` whose instance methods
    /// define on the singleton class of the object they run on; `None`
    /// when the chain is not known yet (being walked, or cut), which may
    /// hold any.
    fn fronts_held(&self, owner: &str, side: Side) -> Option<Rc<[&'a str]>> {
        let Some((&owner, _)) = self.constants.get_key_value(owner) else {
            return Some(Rc::from([]));
        };
        if let Some(held) = self.walks.fronts.borrow().get(&(owner, side)) {
            return Some(Rc::clone(held));
        }
        let walks = &self.walks;
        let outside = walks.building.borrow().is_empty() && walks.walking.borrow().is_empty();
        if walks.building.borrow().contains(&owner)
            || walks.walking.borrow().contains(&(owner, side))
        {
            return None;
        }
        let chain = self.chain(owner, side);
        if chain.is_cut() {
            return None;
        }
        let mut held: Vec<&'a str> = chain
            .steps()
            .filter(|&(o, s)| s == Side::Instance && self.facts.fronting(o))
            .map(|(o, _)| o)
            .collect();
        held.dedup();
        let held: Rc<[&'a str]> = held.into();
        if outside {
            walks
                .fronts
                .borrow_mut()
                .insert((owner, side), Rc::clone(&held));
        }
        Some(held)
    }

    /// Whether [`Self::unsure`] holds through `owner` on `side` for no name
    /// but a tainted one, which holds everywhere: no fact touches that step
    /// (R6-03). Conservative: a step being walked is not quiet.
    pub(super) fn quiet(&self, owner: &str, side: Side) -> bool {
        if self.touched(owner, side) {
            return false;
        }
        let Some((&owner, _)) = self.constants.get_key_value(owner) else {
            return true;
        };
        let walks = &self.walks;
        let walked = walks.building.borrow().contains(&owner)
            || walks.walking.borrow().contains(&(owner, Side::Singleton));
        if self.facts.any_block()
            && (walked
                || self
                    .parts(owner)
                    .is_some_and(|parts| parts.evaluated.iter().any(|(on, _, _)| *on == side)))
        {
            return false;
        }
        !self.facts.macro_named(side, None)
            || !walked
                && !self
                    .chain(owner, Side::Singleton)
                    .steps()
                    .any(|(o, s)| self.facts.macro_defines(o, s, side, None))
    }

    /// Whether a fact may leave lookup through `owner` on `side` unsure of
    /// some name, whatever the name: one of [`Facts::touches`], or an
    /// owner of its chain whose instance methods define on the singleton
    /// class of the object they run on ([`Self::fronted`]).
    ///
    /// [`Facts::touches`]: super::facts::Facts::touches
    pub(super) fn touched(&self, owner: &str, side: Side) -> bool {
        self.facts.touches(owner, side) || self.fronting(owner, side)
    }

    /// Whether a class macro may have defined `name` on `side` of `owner`:
    /// one whose method `owner` runs (B7), a method of a class `owner` is or
    /// inherits from, or of a module it extends, found on its singleton
    /// chain.
    fn macro_defined(&self, owner: &str, side: Side, name: &str) -> bool {
        if !self.facts.macro_named(side, Some(name)) {
            return false;
        }
        let Some((&owner, _)) = self.constants.get_key_value(owner) else {
            return false;
        };
        // Being walked: nothing proves the chain yet.
        let walks = &self.walks;
        if walks.building.borrow().contains(&owner)
            || walks.walking.borrow().contains(&(owner, Side::Singleton))
        {
            return true;
        }
        let chain = self.chain(owner, Side::Singleton);
        chain.is_cut()
            || chain
                .steps()
                .any(|(o, s)| self.facts.macro_defines(o, s, side, Some(name)))
    }

    /// Whether a concern's `included do` block, run on `owner`, defined
    /// `name` on `side` there, unless the owner's own definition comes
    /// later in the file of that `include` (B6).
    fn block_defined(&self, owner: &str, side: Side, name: &str) -> bool {
        if !self.facts.block_named(name) {
            return false;
        }
        let Some((&owner, _)) = self.constants.get_key_value(owner) else {
            return false;
        };
        if self.walks.building.borrow().contains(&owner) {
            return false;
        }
        let Some(parts) = self.parts(owner) else {
            return false;
        };
        let own = self.methods[side as usize]
            .get(owner)
            .and_then(|methods| methods.get(name));
        parts.evaluated.iter().any(|(on, defined, at)| {
            *on == side
                && defined.as_deref().is_none_or(|d| fits(d, name))
                && !matches!(own, Some(Def::One(id))
                    if self.symbols[*id].file == at.file && self.symbols[*id].line > at.line)
        })
    }

    /// Whether every object that may hold `owner`'s step on `side` behind
    /// a step the rules cannot name, but that may be one of the graph's
    /// classes or modules (B5: `class Child < B2` for `B2 = Base`, `class
    /// Child < pick`, `include Mixin` for `Mixin = M`), finds `own` for
    /// `name`: its own steps before that one must not define it, nor hold
    /// a step the rules cannot name where a class or module defining it
    /// may be (R6-08, R6-15), nor may lookup run past `owner`'s chain
    /// (`own` not found). Only the steps before such a step that define
    /// `name`, or that a fact may leave unsure of it, are looked at
    /// ([`Index::behind`], R6-03).
    pub(super) fn opaque_agree(&self, owner: &'a str, side: Side, name: &str, own: Lookup) -> bool {
        let module = !self.classes.contains_key(owner);
        // A module's own methods run on the module alone.
        if side == Side::Singleton && module {
            return true;
        }
        let behind = self.behind();
        // An instance chain holds instance steps; a singleton chain a
        // class's own, or a module's instance step (extended). An
        // `include` of a class raises: only a superclass's chain holds one.
        let holds = |at: &Behind<'a>| {
            let on = match at.on {
                Side::Instance => side == Side::Instance,
                Side::Singleton => side == Side::Singleton || module,
            };
            on && (at.classes || module)
        };
        if own == Lookup::NotFound {
            return !behind.steps.iter().any(holds);
        }
        // Nothing before the opaque step defines `name`, unless an unknown
        // step there may hold a class or module that does.
        let hidden = |at: &Behind<'a>| match at.hidden {
            Hidden::Nothing => false,
            Hidden::Spread => behind.spread_defined.contains(name),
            Hidden::Anything => behind.defined.contains(name),
        };
        let agrees = |&i: &usize| {
            let at = &behind.steps[i];
            !holds(at) || {
                let found = self.lookup_in(&at.chain.steps[..at.at], name);
                (found == Lookup::NotFound && !hidden(at)) || found == own
            }
        };
        // A tainted name leaves every step unsure.
        if self.facts.tainted(name) {
            return (0..behind.steps.len()).all(|i| agrees(&i));
        }
        let defining = behind.defining.get(name).into_iter().flatten();
        defining.chain(&behind.unsure).all(agrees)
    }

    /// Whether no object that may hold `owner`'s instance step behind a
    /// step the rules cannot name defines `name` before that step, or may
    /// hold a class or module that does: what a lookup that finds `name`
    /// nowhere on `owner`'s chain needs to prove no method of the graph
    /// answers it there (a fixture accessor, ADR-0038).
    pub(super) fn opaque_absent(&self, owner: &'a str, name: &str) -> bool {
        let module = !self.classes.contains_key(owner);
        let behind = self.behind();
        behind
            .steps
            .iter()
            .filter(|at| at.on == Side::Instance && (at.classes || module))
            .all(|at| {
                let hidden = match at.hidden {
                    Hidden::Nothing => false,
                    Hidden::Spread => behind.spread_defined.contains(name),
                    Hidden::Anything => behind.defined.contains(name),
                };
                !hidden && self.lookup_in(&at.chain.steps[..at.at], name) == Lookup::NotFound
            })
    }

    /// Whether the ancestor `mixin` declares, which the rules cannot name,
    /// may be a class or module of the graph (B5): a constant whose value
    /// the rules do not read (`B2 = Base`), one another class may hold or
    /// an unread region may define, or an expression (`pick`, `(c ? Base :
    /// Object)`, `Class.new(Base)`) unless rooted in a constant outside the
    /// graph and naming none of it (`Struct.new(:x)`,
    /// `ActiveRecord::Migration[7.1]`, which a gem's code makes). Such an
    /// expression is taken to yield no module of the application, even one
    /// the application stored where it reads (`include SomeGem.mixin`
    /// after `SomeGem.mixin = M`, `Thread.current[:mixin]`): a known
    /// residual (R7-14), since taking every expression for any module
    /// would cost `include Rails.application.routes.url_helpers`.
    fn opaque(&self, mixin: &Mixin<'a>) -> bool {
        let names = |path: &str| {
            // `Object::M` is the top-level `M` (R6-13).
            let top = path
                .trim_start_matches("::")
                .strip_prefix("Object::")
                .map(|rest| format!("::{rest}"));
            let path = top.as_deref().unwrap_or(path);
            !self.may_name(path, &mixin.cref).is_empty()
                || self
                    .facts
                    .unread_constant(path.rsplit("::").next().unwrap_or(path))
        };
        if !mixin.name.is_empty() {
            return names(&mixin.name);
        }
        // `Object.const_get(:M)`, `"M".constantize`: a constant named by a
        // string or a symbol may be any of the graph's, whatever the
        // expression is rooted in (R6-13).
        if names_a_constant(mixin.text) {
            return true;
        }
        // `DelegateClass(File)` makes a new subclass of `Delegator`, no
        // class the graph names.
        if mixin.text.starts_with("DelegateClass(") {
            return false;
        }
        let paths = constant_paths(mixin.text);
        let Some((at, root)) = paths.first() else {
            return true;
        };
        let root = root.trim_start_matches("::");
        if *at != 0 || matches!(root, "Class" | "Module") {
            return true;
        }
        // Core code (`Struct.new(:x)`, an empty-name `Struct` of `X =
        // Struct.new`) makes no class of the graph but a subclass of a
        // core class, which a call on `self` there proves nothing in
        // anyway (R5); but what it is given may come back (`Array(M).first`,
        // `Hash.new(M)[:x]`), so any other constant it names may be the
        // step (R7-06).
        let rest = if is_core(root) {
            &paths[1..]
        } else {
            &paths[..]
        };
        rest.iter().any(|(_, path)| names(path))
    }

    /// What an unknown step `mixin` makes stands for.
    fn stands_for(&self, mixin: &Mixin<'a>, ancestor: Option<&'a str>) -> Stands<'a> {
        if ancestor.is_none() && self.opaque(mixin) {
            Stands::Opaque {
                classes: mixin.kind.op == Op::Superclass,
            }
        } else {
            Stands::Any
        }
    }

    /// What lookup from `owner` on `side` finds for `name` before the
    /// owner's own step when the declaration at `at` runs (an alias, B1):
    /// `None` when no step prepended by then defines it; `Some(Some(id))`
    /// the first that does; `Some(None)` when one may, unproven (a module
    /// the graph cannot name, one prepended in a file loaded in an order
    /// the graph cannot tell).
    pub(super) fn prepended(
        &self,
        owner: &'a str,
        side: Side,
        name: &str,
        at: (&'a str, u32),
    ) -> Option<Decision> {
        let chain = self.chain(owner, side);
        let Some(own) = chain
            .steps
            .iter()
            .position(|step| step.node == Node::Known(owner, side))
        else {
            return Some(None);
        };
        for step in &chain.steps[..own] {
            let there = match step.at {
                Some(inserted) => self.before((inserted.file, inserted.line), inserted.by, at),
                None => Some(true),
            };
            if there == Some(false) {
                continue;
            }
            let Node::Known(o, s) = step.node else {
                return Some(None);
            };
            if !self.defines(o, s, name) && !self.unsure(o, s, name) {
                continue;
            }
            return Some(match there {
                Some(_) if !step.maybe => self.method(o, s, name).flatten(),
                _ => None,
            });
        }
        None
    }

    pub(super) fn defines(&self, owner: &str, side: Side, name: &str) -> bool {
        self.methods[side as usize]
            .get(owner)
            .is_some_and(|methods| methods.contains_key(name))
    }

    /// The lookup chain from `owner` on `side`: its own list, then its
    /// superclass's chain.
    pub(super) fn chain(&self, owner: &'a str, side: Side) -> Rc<Chain<'a>> {
        self.chain_or_cycle(owner, side)
            .unwrap_or_else(|| Rc::new(Chain::default()))
    }

    /// [`Self::chain`]; `None` when it is being walked or `owner` built (a
    /// cycle).
    fn chain_or_cycle(&self, owner: &'a str, side: Side) -> Option<Rc<Chain<'a>>> {
        let walks = &self.walks;
        if let Some(chain) = walks.kept(&walks.chains, &(owner, side)) {
            return Some(chain);
        }
        if walks.building.borrow().contains(&owner)
            || walks.walking.borrow().contains(&(owner, side))
        {
            walks.cyclic.set(true);
            return None;
        }
        let unknown = || Rc::clone(walks.unread_chain.get_or_init(|| Rc::new(Chain::cut())));
        let walk = || {
            if walks.depth.get() > MAX_ANCESTOR_DEPTH
                || walks.walking.borrow().len() >= MAX_NESTED_WALKS
            {
                walks.capped.set(true);
                return unknown();
            }
            walks.walking.borrow_mut().push((owner, side));
            let chain = match self.parts(owner) {
                Some(parts) => {
                    let mut steps = match side {
                        Side::Instance => parts.instance.steps.clone(),
                        Side::Singleton => parts.singleton.steps.clone(),
                    };
                    steps.extend(self.tail(owner, side));
                    Rc::new(Chain { steps })
                }
                None => unknown(),
            };
            walks.walking.borrow_mut().pop();
            chain
        };
        Some(walks.keep(&walks.chains, (owner, side), walk, unknown))
    }

    /// What comes after `owner`'s own list on `side`: its superclass's
    /// chain, nothing past a module (or a class of `Object`), an unknown
    /// step past a core class or a superclass the graph cannot name.
    fn tail(&self, owner: &'a str, side: Side) -> Vec<Step<'a>> {
        if !self.classes.contains_key(owner) {
            return Vec::new();
        }
        match self.superclass(owner) {
            Superclass::None if self.maybe_core(owner) => vec![Step::new(Node::Unknown)],
            Superclass::None => Vec::new(),
            // `class A < B` with `class B < A` is a cycle Ruby itself
            // refuses: whatever follows is unknown.
            Superclass::Known(parent) => match self.deeper(|| self.chain_or_cycle(parent, side)) {
                Some(chain) => chain.steps.clone(),
                None => vec![Step::cut()],
            },
            // A dynamic or external superclass, or two reopenings that
            // disagree: whatever comes next is unknown.
            Superclass::Unknown => vec![Step::new(Node::Unknown)],
            Superclass::Opaque => vec![Step {
                stands: Stands::Opaque { classes: true },
                ..Step::new(Node::Unknown)
            }],
        }
    }

    /// Runs `walk` one level deeper.
    fn deeper<T>(&self, walk: impl FnOnce() -> T) -> T {
        let depth = &self.walks.depth;
        depth.set(depth.get() + 1);
        let value = walk();
        depth.set(depth.get() - 1);
        value
    }

    /// Whether `step` of a module's own list is one a declaration of the
    /// module may make only after `decl` mixed the module in: below it in
    /// the same file, or in a file loaded in an order the graph cannot
    /// tell. Ruby 3.3 then carries it to the includer's list after the
    /// module, whatever that list holds by then, and a later `include` of
    /// the includer's own does not skip it (`rb_include_module`): where it
    /// lands is not read, so it is a step that may not be there (R12-02).
    fn late(&self, step: &Step<'a>, decl: Decl<'a>) -> bool {
        step.at
            .is_some_and(|by| self.before((by.file, by.line), by.by, decl.at) != Some(true))
    }

    /// Whether the step `step`, one the graph cannot name, may hold the
    /// module `module` (R5-06): code the rules do not play, a superclass or
    /// mixin they cannot follow, or a module's own list may hold any; a
    /// gem's class or module only one the application mixes in where the
    /// rules do not replay it.
    fn may_hold(&self, step: Step<'a>, module: &str) -> bool {
        if step.node != Node::Unknown {
            return false;
        }
        match step.stands {
            Stands::Concern => false,
            Stands::Any | Stands::Gem => self.spread.contains(module),
            Stands::Unread | Stands::Cut | Stands::Opaque { .. } | Stands::Itself(_) => true,
        }
    }

    /// Whether including `module` into an object inserts only its own
    /// chain, every step of which the graph names: a module, no concern,
    /// with no `included` or `append_features` of its own or of a module
    /// it extends, nor one the rules do not read (an RSpec group's
    /// `include M`, ADR-0041).
    pub(super) fn plain_include(&self, module: &'a str) -> bool {
        !self.concerns.contains(module)
            && !self.classes.contains_key(module)
            && matches!(self.hook_def(module, "included"), hooks::HookDef::None)
            && !self.replaces(module, "append_features")
            && !self.unknown_hook(module)
            && self.chain(module, Side::Instance).is_known()
    }

    /// Whether `owner` may be a core or standard-library class or module
    /// the graph reopens: one of their names, or a name nested in theirs
    /// that declares no superclass of the graph's (`class URI::Generic`;
    /// `class Net::A < Base` cannot be Ruby's), whose own methods,
    /// superclass and subclasses the graph does not hold (D31, R5-10).
    pub(super) fn maybe_core(&self, owner: &'a str) -> bool {
        is_core(owner)
            || in_core_namespace(owner)
                && !(self.classes.contains_key(owner)
                    && matches!(self.superclass(owner), Superclass::Known(_)))
    }

    /// The one superclass `owner` declares, its reopenings agreeing.
    pub(super) fn superclass(&self, owner: &'a str) -> Superclass<'a> {
        let walks = &self.walks;
        if let Some(superclass) = walks.kept(&walks.superclasses, &owner) {
            return superclass;
        }
        let walk = || {
            let mut distinct: Vec<Option<&'a str>> = Vec::new();
            let mut opaque = false;
            for mixin in self.declarations(owner, |kind| kind.op == Op::Superclass) {
                // `class X < Object`, `Object` not reopened, has an unknown
                // superclass, as `X` had before R5-15: code the rules do not
                // read may have changed `Object` (R8-01).
                let parent = self
                    .ancestor_of(mixin, None)
                    .filter(|parent| self.classes.contains_key(parent));
                opaque |= parent.is_none() && self.opaque(mixin);
                if !distinct.contains(&parent) {
                    distinct.push(parent);
                }
            }
            match distinct[..] {
                [] => Superclass::None,
                [Some(parent)] => Superclass::Known(parent),
                _ if opaque => Superclass::Opaque,
                _ => Superclass::Unknown,
            }
        };
        walks.keep(&walks.superclasses, owner, walk, || Superclass::Unknown)
    }

    /// `owner`'s declarations of the kinds `kinds` accepts, in the order
    /// Ruby runs them.
    pub(super) fn declarations(
        &self,
        owner: &str,
        kinds: impl Fn(MixinKind) -> bool,
    ) -> impl Iterator<Item = &Mixin<'a>> {
        self.mixins_of
            .get(owner)
            .into_iter()
            .flatten()
            .map(|&i| &self.mixins[i])
            .filter(move |mixin| kinds(mixin.kind))
    }

    /// `owner`'s own lists, built once while they hold; `None` when it is
    /// being built (a cycle).
    fn parts(&self, owner: &'a str) -> Option<Rc<Parts<'a>>> {
        let walks = &self.walks;
        if let Some(parts) = walks.kept(&walks.parts, &owner) {
            return Some(parts);
        }
        if walks.building.borrow().contains(&owner) {
            walks.cyclic.set(true);
            return None;
        }
        let unknown = || Rc::clone(walks.unread_parts.get_or_init(|| Rc::new(Parts::cut())));
        let walk = || {
            if walks.depth.get() > MAX_ANCESTOR_DEPTH
                || walks.building.borrow().len() >= MAX_NESTED_WALKS
            {
                walks.capped.set(true);
                return unknown();
            }
            walks.building.borrow_mut().push(owner);
            let parts = self.replay(owner);
            walks.building.borrow_mut().pop();
            Rc::new(parts)
        };
        Some(walks.keep(&walks.parts, owner, walk, unknown))
    }

    /// Replays `owner`'s declarations.
    fn replay(&self, owner: &'a str) -> Parts<'a> {
        let core = self.maybe_core(owner);
        let mut b = Building {
            index: self,
            owner,
            concern: self.concerns.contains(owner),
            dependencies: VecDeque::new(),
            maybe: false,
            replaced: Vec::new(),
            evaluated: Vec::new(),
            acting: Vec::new(),
            running: Vec::new(),
            instance: Part::new(owner, Side::Instance, core),
            singleton: Part::new(owner, Side::Singleton, core),
            tail_instance: self.tail(owner, Side::Instance),
            tail_singleton: self.tail(owner, Side::Singleton),
        };
        let mut resolved = HashMap::default();
        // `Superclass.inherited(owner)` runs when the class is created.
        let first = self
            .declarations(owner, |kind| kind.op == Op::Superclass)
            .next();
        if let (Superclass::Known(parent), Some(first)) = (self.superclass(owner), first) {
            let decl = Decl {
                at: (first.file, first.line),
                uncertain: false,
            };
            self.run_hook(&mut b, Object::Owner, parent, "inherited", decl);
        } else if !core && self.classes.contains_key(owner) && self.core_hook("inherited") {
            // `Object.inherited(owner)`, which the graph may define, runs for
            // a class that declares no superclass, or one the rules cannot
            // follow, which may reach it (R7-07).
            let at = match (first, self.classes.get(owner)) {
                (Some(first), _) => (first.file, first.line),
                (None, Some(Def::One(i))) => {
                    (self.symbols[*i].file.as_str(), self.symbols[*i].line)
                }
                (None, _) => ("", 0),
            };
            let decl = Decl {
                at,
                uncertain: false,
            };
            b.unknown_front(Object::Owner, decl);
        }
        let own = |kind: MixinKind| {
            kind.op != Op::Superclass && matches!(kind.on, On::Body | On::Singleton)
        };
        let mut files: Vec<&str> = self.declarations(owner, own).map(|m| m.file).collect();
        files.dedup();
        let uncertain = files.len() > 1;
        let declarations: Vec<&Mixin<'a>> = self.declarations(owner, own).collect();
        for (at, mixin) in declarations.iter().enumerate() {
            let decl = Decl {
                at: (mixin.file, mixin.line),
                uncertain,
            };
            b.maybe = mixin.maybe;
            let (object, ancestor) = if mixin.kind.on == On::Singleton {
                // In `class << self`, the singleton class's ancestors: the
                // modules the class extends so far, then its superclass's.
                let partial = (&b.singleton, b.tail_singleton.as_slice());
                let ancestor = self.ancestor_of(mixin, Some((owner, partial)));
                (Object::SingletonClass, ancestor)
            } else {
                let partial = (&b.instance, b.tail_instance.as_slice());
                let ancestor = self.ancestor_of(mixin, Some((owner, partial)));
                (Object::Owner, ancestor)
            };
            resolved.insert(mixin.edge, ancestor);
            // `extend ActiveSupport::Concern` of a gem: the concern rules
            // play its `append_features` and `included`; `extended` and
            // `extend_object` it does not define.
            if ancestor.is_none() && mixin.makes_concern() {
                let concern = Step {
                    stands: Stands::Concern,
                    ..Step::new(Node::Unknown)
                };
                b.include(Side::Singleton, &[concern], decl);
                continue;
            }
            // `extend self`: the module's own list, as built so far, on its
            // singleton class (R7). Ruby 3.3 carries a later `include` or
            // `prepend` into the module there too (one in another file may
            // load later): an unknown step that is that list. Its own
            // `extended` hook may add anything besides.
            if mixin.kind.op == Op::Extend && mixin.kind.on == On::Body && ancestor == Some(owner) {
                let mixes = |m: &&Mixin| {
                    m.kind.on == On::Body && matches!(m.kind.op, Op::Include | Op::Prepend)
                };
                let later = declarations[at + 1..].iter().any(mixes)
                    || declarations[..at]
                        .iter()
                        .any(|m| mixes(m) && m.file != mixin.file);
                let extended = self.method(owner, Side::Singleton, "extended").is_some();
                if later || extended {
                    let itself = Step {
                        stands: Stands::Itself(owner),
                        ..Step::new(Node::Unknown)
                    };
                    b.include(Side::Singleton, &[itself], decl);
                }
                if extended {
                    self.act(&mut b, object, Op::Extend, None, Stands::Any, decl);
                } else if !later {
                    let own = b.instance.steps.clone();
                    b.include(Side::Singleton, &own, decl);
                }
                continue;
            }
            let stands = self.stands_for(mixin, ancestor);
            self.act(&mut b, object, mixin.kind.op, ancestor, stands, decl);
        }
        // A mixin from outside the body of a module the rules cannot name
        // (`K.prepend(Cfg.mod)`): an unknown step that may be any module
        // of the graph, where Ruby inserts it, whenever that ran (R8-05).
        for &(side, prepend) in self.facts.opaque_on(owner) {
            let step = Step {
                stands: Stands::Opaque { classes: false },
                ..Step::new(Node::Unknown)
            };
            let decl = Decl {
                at: ("", 0),
                uncertain: true,
            };
            b.maybe = false;
            if prepend {
                b.prepend(side, &[step], decl);
            } else {
                b.include(side, &[step], decl);
            }
        }
        Parts {
            instance: b.instance,
            singleton: b.singleton,
            resolved,
            dependencies: b.dependencies.into(),
            replaced: b.replaced,
            evaluated: b.evaluated,
        }
    }

    /// The ancestors a module brings: its own list (a module has no
    /// superclass). `None` for a class (`include` of one raises) and for a
    /// module being built (a cycle, which Ruby refuses; skipped).
    fn ancestry(&self, module: &'a str) -> Option<Vec<Step<'a>>> {
        if self.classes.contains_key(module) {
            return Some(vec![Step::unread()]);
        }
        let chain = self.deeper(|| self.chain_or_cycle(module, Side::Instance))?;
        Some(chain.steps.clone())
    }

    /// One declaration: `op` of `module` (`None`: one the graph cannot
    /// name, for which an unknown step that `stands` stands) on `object`.
    /// One played again inside itself, or nested past
    /// [`MAX_ANCESTOR_DEPTH`], is cut ([`Building::cut`]).
    fn act(
        &self,
        b: &mut Building<'a, '_>,
        object: Object,
        op: Op,
        module: Option<&'a str>,
        stands: Stands<'a>,
        decl: Decl<'a>,
    ) {
        let Some(named) = module else {
            return self.play(b, object, op, module, stands, decl);
        };
        if b.acting.len() >= MAX_ANCESTOR_DEPTH || b.acting.contains(&(object, op, named)) {
            return b.cut(object, named, decl);
        }
        b.acting.push((object, op, named));
        self.play(b, object, op, module, stands, decl);
        b.acting.pop();
    }

    /// [`Self::act`], once its guard let it through.
    fn play(
        &self,
        b: &mut Building<'a, '_>,
        object: Object,
        op: Op,
        module: Option<&'a str>,
        stands: Stands<'a>,
        decl: Decl<'a>,
    ) {
        let side = match object {
            Object::Owner if op == Op::Extend => Side::Singleton,
            Object::Owner => Side::Instance,
            // An `extend` of the singleton class acts on its own singleton,
            // which no lookup of the owner reads; its `extended` hook runs
            // on the singleton class all the same.
            Object::SingletonClass if op == Op::Extend => {
                if let Some(module) = module {
                    self.run_hook(b, object, module, "extended", decl);
                }
                return;
            }
            Object::SingletonClass => Side::Singleton,
        };
        let features = match op {
            Op::Prepend => "prepend_features",
            Op::Extend => "extend_object",
            _ => "append_features",
        };
        // A module that replaces what the declaration does may do anything.
        let custom = module.is_some_and(|m| self.replaces(m, features));
        let Some(module) = module.filter(|_| !custom) else {
            // A module the graph cannot name may be a gem's concern, whose
            // `ClassMethods` and `included do` reach the singleton class too.
            let unknown = [Step {
                stands: if custom { Stands::Unread } else { stands },
                ..Step::new(Node::Unknown)
            }];
            // What a gem's module adds to the singleton class defines no
            // hook there (#120).
            let extension = [Step {
                stands: match unknown[0].stands {
                    Stands::Any => Stands::Gem,
                    other => other,
                },
                ..unknown[0]
            }];
            match op {
                Op::Prepend => {
                    b.prepend(side, &unknown, decl);
                    if object == Object::Owner {
                        b.prepend(Side::Singleton, &extension, decl);
                    }
                }
                _ => {
                    b.include(side, &unknown, decl);
                    if object == Object::Owner && op == Op::Include {
                        b.include(Side::Singleton, &extension, decl);
                    }
                }
            }
            if let Some(module) = module.filter(|_| custom) {
                b.unknown_front(object, decl);
                if object == Object::Owner {
                    b.replaced.push((Side::Instance, module));
                }
                b.replaced.push((Side::Singleton, module));
            }
            return;
        };
        if op != Op::Extend && self.concerns.contains(module) {
            return self.concern(b, object, op, module, decl);
        }
        let Some(ancestry) = self.ancestry(module) else {
            return;
        };
        let hook = match op {
            Op::Prepend => {
                b.prepend(side, &ancestry, decl);
                "prepended"
            }
            Op::Extend => {
                b.include(side, &ancestry, decl);
                "extended"
            }
            _ => {
                b.include(side, &ancestry, decl);
                "included"
            }
        };
        if module != "ActiveSupport::Concern" {
            self.run_hook(b, object, module, hook, decl);
        }
    }

    /// `include` (or `prepend`) of the concern `module`, as Active
    /// Support's `append_features` (`prepend_features`) does it.
    fn concern(
        &self,
        b: &mut Building<'a, '_>,
        object: Object,
        op: Op,
        module: &'a str,
        decl: Decl<'a>,
    ) {
        let side = if object == Object::Owner {
            Side::Instance
        } else {
            Side::Singleton
        };
        // A concern including or prepending a concern defers it to its own
        // includers (`@_dependencies`, an `include` appended, a `prepend`
        // put first); a class that already has it gets nothing (`base <
        // self`). Ruby still calls the hook after either.
        if b.concern && object == Object::Owner {
            match op {
                Op::Prepend => b.dependencies.push_front((module, b.maybe)),
                _ => b.dependencies.push_back((module, b.maybe)),
            }
            self.concern_hook(b, object, op, module, decl);
            return;
        }
        match b.has(side, module, decl) {
            Some(true) => {
                self.concern_hook(b, object, op, module, decl);
                return;
            }
            // Whether it is there depends on the load order: so does all
            // it would do (B4).
            None => {
                b.unknown_front(object, decl);
                return;
            }
            Some(false) => {}
        }
        let Some(parts) = self.deeper(|| self.parts(module)) else {
            return;
        };
        // Each of its dependencies included (or prepended) now as it is.
        let played = b.maybe;
        for (dependency, maybe) in parts.dependencies.clone() {
            b.maybe = played || maybe;
            self.act(b, object, op, Some(dependency), Stands::Any, decl);
        }
        b.maybe = played;
        let Some(ancestry) = self.ancestry(module) else {
            return;
        };
        let block = match op {
            Op::Prepend => {
                b.prepend(side, &ancestry, decl);
                On::Prepended
            }
            _ => {
                b.include(side, &ancestry, decl);
                On::Included
            }
        };
        // `const_get(:ClassMethods)`: the concern's own, else one of its
        // other ancestors' in their order, else a top-level one (Ruby 3.3
        // reads a module's own constants before its prepended modules').
        let ancestors = self.deeper(|| self.chain(module, Side::Instance));
        // Past a walk cut short, any ancestor may hold it (R6-04).
        let cut = ancestors.is_cut().then_some((constants::CUT, true));
        let class_methods = self
            .through(
                std::iter::once((module, false))
                    .chain(
                        ancestors
                            .owners_maybe()
                            .filter(|(o, _, _)| *o != module)
                            .map(|(o, _, maybe)| (o, maybe)),
                    )
                    .chain(cut),
                "ClassMethods",
            )
            .unwrap_or_else(|| self.known("ClassMethods"))
            .filter(|name| !self.classes.contains_key(name));
        if let Some(class_methods) = class_methods
            && object == Object::Owner
        {
            match op {
                Op::Prepend => self.act(
                    b,
                    Object::SingletonClass,
                    Op::Prepend,
                    Some(class_methods),
                    Stands::Any,
                    decl,
                ),
                _ => self.act(
                    b,
                    Object::Owner,
                    Op::Extend,
                    Some(class_methods),
                    Stands::Any,
                    decl,
                ),
            }
        }
        let deferred: Vec<&Mixin<'a>> =
            self.declarations(module, |kind| kind.on == block).collect();
        let mut files: Vec<&str> = deferred.iter().map(|m| m.file).collect();
        files.dedup();
        let decl = Decl {
            uncertain: decl.uncertain || files.len() > 1,
            ..decl
        };
        for mixin in deferred {
            let ancestor = self.ancestor_of(mixin, None);
            let stands = self.stands_for(mixin, ancestor);
            b.maybe = played || mixin.maybe;
            self.act(b, object, mixin.kind.op, ancestor, stands, decl);
        }
        b.maybe = played;
        // What the block defines lands on the object it runs on (B6): on a
        // singleton class, its instance methods are the owner's own.
        let at = b.at(decl);
        for (on, name) in self.facts.block_defs(module, block == On::Prepended) {
            let side = match (object, on) {
                (Object::Owner, on) => on,
                (Object::SingletonClass, Side::Instance) => Side::Singleton,
                (Object::SingletonClass, Side::Singleton) => continue,
            };
            b.evaluated.push((side, name, at));
        }
        self.concern_hook(b, object, op, module, decl);
    }

    /// The class or module `mixin`'s constant names where Ruby evaluates
    /// it (D37): the lexical nesting; then the ancestors of the innermost
    /// scope, the owner's as built so far when `partial` gives them (in
    /// `class << self`, its singleton class's: the modules it extends);
    /// then the top level.
    fn ancestor_of(
        &self,
        mixin: &Mixin<'a>,
        partial: Option<(&'a str, (&Part<'a>, &[Step<'a>]))>,
    ) -> Option<&'a str> {
        let path = mixin.name.as_str();
        if path.is_empty() {
            return None;
        }
        let found = match self.lexical(&mixin.cref, path) {
            Some(found) => found,
            None => {
                let innermost = mixin.cref.first().map(String::as_str);
                let through = match (innermost, partial) {
                    // The owner's own declaration, in its body or its
                    // `class << self`: the list it acts on, as built so
                    // far; a singleton class's step holds the constants of
                    // that class's `class << self`.
                    (Some(scope), Some((owner, (part, tail))))
                        if scope == owner || singleton_of(scope) == Some(owner) =>
                    {
                        let steps = part.steps.iter().chain(tail);
                        let scopes = steps.filter_map(|step| match step.node {
                            Node::Known(o, Side::Instance) if o != owner => {
                                Some((o.to_owned(), step.maybe))
                            }
                            Node::Known(o, Side::Singleton) if o != owner => {
                                Some((singleton_scope(o), step.maybe))
                            }
                            // Past a walk cut short, any ancestor may hold
                            // it (R6-04).
                            Node::Unknown if step.stands == Stands::Cut => {
                                Some((constants::CUT.to_owned(), true))
                            }
                            _ => None,
                        });
                        self.through(scopes, path)
                    }
                    (Some(scope), _) => {
                        self.through(self.deeper(|| self.scopes_after(scope)), path)
                    }
                    (None, _) => None,
                };
                through.unwrap_or_else(|| self.known(path))
            }
        }?;
        self.settled(path, found)
    }

    /// The ancestor a declaration that is no owner's own (a superclass, a
    /// concern's block, a hook) names, where Ruby evaluates it.
    pub(super) fn named(&self, mixin: &Mixin<'a>) -> Option<&'a str> {
        self.ancestor_of(mixin, None)
    }

    /// The ancestor `owner`'s own declaration at `edge` names, as its
    /// lists were built.
    pub(super) fn resolved(&self, owner: &'a str, edge: usize) -> Option<&'a str> {
        self.parts(owner)?.resolved.get(&edge).copied().flatten()
    }
}

/// The first step of the own list whose owner's step is `steps[j]`: the
/// modules its own declarations prepended, which come right before it.
fn front(steps: &[Step], j: usize) -> usize {
    let Node::Known(owner, _) = steps[j].node else {
        return j;
    };
    let mut i = j;
    while i > 0 && steps[i - 1].at.is_some_and(|at| at.by == owner) {
        i -= 1;
    }
    i
}

/// Whether the expression `text` may compute a constant from a name:
/// `const_get`, `constantize`, or a string or symbol that reads as a
/// constant (`:M`, `"Admin::M"`).
fn names_a_constant(text: &str) -> bool {
    text.contains("const_get")
        || text.contains("constantize")
        || text
            .as_bytes()
            .windows(2)
            .any(|pair| matches!(pair[0], b':' | b'"' | b'\'') && pair[1].is_ascii_uppercase())
}

/// The constant paths written in `text`, each with its offset: `A`,
/// `A::B`, `::A` (a symbol's `:A`, an instance variable's `@A`, a global's
/// `$A` are none).
fn constant_paths(text: &str) -> Vec<(usize, &str)> {
    let bytes = text.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut paths = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let rooted = bytes[i..].starts_with(b"::")
            && bytes.get(i + 2).is_some_and(u8::is_ascii_uppercase)
            && (i == 0 || !word(bytes[i - 1]));
        let starts = rooted
            || bytes[i].is_ascii_uppercase()
                && (i == 0 || !(word(bytes[i - 1]) || matches!(bytes[i - 1], b':' | b'@' | b'$')));
        if !starts {
            i += 1;
            continue;
        }
        let start = i;
        if rooted {
            i += 2;
        }
        loop {
            while i < bytes.len() && word(bytes[i]) {
                i += 1;
            }
            if bytes[i..].starts_with(b"::") && bytes.get(i + 2).is_some_and(u8::is_ascii_uppercase)
            {
                i += 2;
            } else {
                break;
            }
        }
        paths.push((start, &text[start..i]));
    }
    paths
}

mod building;
mod held;
mod hooks;
#[cfg(test)]
mod tests;
