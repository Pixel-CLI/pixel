// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The Ruby walker: symbols, call sites, references, imports, mixins.
//!
//! One pass over the tree-sitter-ruby tree, carried from v1's `walk_ruby`
//! (`crates/pixel-graph/src/extract.rs:2013-2447`), with the Rails DSL rules
//! in `dsl` and the routes in `routes`. Every site carries a [`Context`]
//! whose `cref` is the lexical class and module nesting read from the tree,
//! not from line spans, so a superclass expression and code after `end` on
//! a class's last line are placed where Ruby evaluates them.
//!
//! What this walker does differently from v1, each for C-10 ("everything in
//! the map is true of the tree"):
//!
//! - a call's line is the line of its method name, so `.compact` at the end
//!   of a chain opened two lines above is recorded where it is written (v1
//!   used the line where the chain starts);
//! - `self.a, self.b = 1, 2` calls the writers `a=` and `b=`, as a single
//!   assignment does (v1 recorded the readers);
//! - `class ::Top` defines `Top`, not `Outer::::Top`;
//! - `super` and `super(...)` are not recorded, and neither is an
//!   identifier passed as an argument: neither names a target the rules can
//!   prove (MVP decision 6);
//! - a signature is cut from the start of its line, so it is always a
//!   prefix of the trimmed source line (`private def b; end`);
//! - a site belongs to the definition the tree writes it in, not to the
//!   smallest definition spanning its line: `b` in `class C; def a; b;
//!   end; end` is `C#a`'s, and `run` in `end; run` the file's;
//! - a region the grammar could not parse (an `ERROR` node) yields its
//!   `require`s only: error recovery loses the class around a `def`, so
//!   neither the qualified name of a definition nor the `self` of a call
//!   written there can be read; a bare anonymous `&` or `**` ending the
//!   arguments of a call without parentheses (`link_to url, &`), which
//!   tree-sitter-ruby 0.23.1 cannot parse, is read as `&1` (`**1`) over
//!   the same bytes (#107, `forwarding`);
//! - a `def` in a block outside every class and module is a method of no
//!   owner, not a top-level function: the block may run on any object a
//!   DSL chooses (`Module.new do`, `describe do`);
//! - inside a class or module too, a `def` in a block belongs to the class
//!   only when the block is known to run there (plain iteration, a
//!   concern's `included`, `prepended`, `class_methods`, `concerning`); a
//!   `Struct.new`, `Class.new`, `Module.new` or `Data.define` assigned to a
//!   constant defines that class or module and owns its block's `def`s;
//!   any other block (`class_eval` on another class, `instance_eval`, an
//!   association's extension, `refine`) gives its `def`s no owner, as do
//!   `class << x` and `def x.m` for an `x` that is not the class (D01, D32);
//! - a constant assignment defines a constant, which shadows an outer one
//!   of the same name (D11), and a compact `class A::B` is named where Ruby
//!   finds `A` (D09);
//! - a `def` under a bare `module_function` is both the module's method and
//!   the private instance method Ruby copies (D15); `define_method` with a
//!   literal name defines a method (D16); `/(?<name>..)/ =~ s` binds `name`
//!   as a local (D33);
//! - a call, `send` or callback written where `self` is not what its source
//!   runs on is marked [`REBOUND`]: a top-level block, a class-body block a
//!   macro runs elsewhere, a block passed to `instance_eval` and its family
//!   in a method, a `class << self` body (D08, [`This`]);
//! - `class_eval` and its family sent to the class whose body they are in
//!   define its instance methods, `instance_eval` its own (R1); a block a
//!   method the rules do not model may yield to, with the class as `self`,
//!   and code evaluated on a constant's class from outside its body record
//!   what they may define there ([`Landing`], R1, R9, R10); a `def` nested
//!   in a method defines on the class the method was defined in (R14);
//! - `class << self` opens the singleton class's constant scope (R2), and
//!   `self::X = v` and `const_set(:X, v)` assign constants (R8);
//! - `singleton_class.include M` and `send(:include, M)` are mixins (R3,
//!   R12), and a load under a condition, in a block or in a method is
//!   marked [`CONDITIONAL`] (R13);
//! - `def X.m` and `class << X` define on the class or module the constant
//!   `X` names, as this file reads it, which a [`Fact::Definee`] lets
//!   resolution check; `class << expr` in a body may be the class itself
//!   (B2);
//! - `concerning :X do` makes the concern `Owner::X`, which the owner
//!   includes; what a concern's `included do` or `prepended do` defines is
//!   the includer's, no method of the concern ([`Fact::Block`], B6);
//! - a definer sent with `send` or called on `self` defines as written; one
//!   in a method body is a class macro's ([`Fact::Macro`]); a name with an
//!   interpolation is a pattern of the names it may be; a splat of a
//!   literal array of names reads those names while nothing read the
//!   constant since, and any other may be any name (B7, B8); an `each`
//!   loop including its parameter over such an array of constants
//!   includes each in turn;
//! - a compact `class A::B` the nesting does not resolve finds `A` through
//!   the ancestors of the innermost scope this file declares (B10);
//! - in a spec file, a `describe` block is an example group, whose
//!   `def`s, `let`s and `subject`s, and whose calls in an example, are
//!   recorded as the group's ([`Fact::Group`], `rspec`, ADR-0041); a `def`
//!   there taints its name only where RSpec's model does not read the
//!   program ([`Fact::GroupDef`]).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use pixel_model::{EdgeKind, SymbolKind};
use tree_sitter::{Node, Tree};

use self::flow::{is_branch, passes_block_on};
use self::locals::{Gate, Locals};
use super::unread::{self, Unread};
use super::{Context, Fact, MIXIN, REBOUND, dsl, manifest, routes};
use crate::lang::{Def, Extraction, FILE_OWNER, Site};
use crate::text;

pub use crate::guard::PARSE_BUDGET;

mod constants;
mod factories;
mod flow;
mod forwarding;
mod hidden;
mod imports;
mod locals;
mod rspec;
mod taint;
mod values;

/// Deepest node the walker visits: the root is depth 0, and a node past
/// the cap is skipped with its whole subtree (v1 `MAX_DEPTH`), which keeps
/// the recursive walk within a thread's stack.
pub const MAX_DEPTH: usize = 512;

/// `call` methods that load another file rather than invoke behaviour: their
/// first string argument is an import, never a call (v1 `extract.rs:2014`).
const REQUIRE_METHODS: &[&str] = &["require", "require_relative", "require_dependency", "load"];

/// One Ruby file's definitions, sites and manifest facts. A lockfile yields
/// facts only; a generated blob, a parse past [`PARSE_BUDGET`] or a panic
/// yields nothing but the driver's `File` symbol. A tree with syntax errors
/// is still walked: what could be read is kept.
pub fn extract(path: &str, content: &[u8]) -> Extraction {
    let facts = if manifest::is_lockfile(path) {
        Some(manifest::lock_gems(content))
    } else {
        manifest::fixture_set(path).map(|_| manifest::fixture_facts(content))
    };
    if let Some(facts) = facts {
        return Extraction {
            facts: Fact::encode_all(&facts),
            ..Extraction::default()
        };
    }
    if manifest::is_generated_blob(content) {
        return Extraction::default();
    }
    catch_unwind(AssertUnwindSafe(|| extract_tree(path, content))).unwrap_or_default()
}

fn extract_tree(path: &str, content: &[u8]) -> Extraction {
    let Some(tree) = readable(content) else {
        return Extraction::default();
    };
    let root = tree.root_node();
    let mut walker = Walker::new(content);
    walker.rebinders = flow::rebinders(&walker, root);
    walker.exits = flow::exits(&walker, root);
    walker.minitest = values::minitest(path);
    walker.rspec = rspec::spec_code(path);
    walker.spec_code = rspec::spec_code(path);
    walker.walk(root, None, None, 0);
    let unsettled = walker.unsettled();
    dsl::drop_overridden(&mut walker, &unsettled);
    let mut sites = walker.sites;
    let mut facts = manifest::facts(path, root, content);
    if routes::is_routes_file(path) {
        let (routes, draws) = routes::read(root, content, path);
        sites.extend(routes);
        facts.extend(draws);
    }
    facts.extend(walker.facts);
    facts.extend(unsettled.into_iter().map(|name| Fact::Conditional { name }));
    if !walker.unread.is_empty() {
        facts.push(Fact::Unread(walker.unread));
    }
    Extraction {
        defs: walker.defs,
        sites,
        facts: Fact::encode_all(&facts),
    }
}

/// The Ruby tree of `content`, or `None` past `budget` (`guard::parse`).
pub fn parse(content: &[u8], budget: Duration) -> Option<Tree> {
    crate::guard::parse(&tree_sitter_ruby::LANGUAGE.into(), content, budget)
}

/// The tree the walk reads of `content` ([`forwarding::parse`]).
pub(super) fn readable(content: &[u8]) -> Option<Tree> {
    forwarding::parse(content)
}

/// The walk's state: what it found, and where in the tree it stands.
pub(super) struct Walker<'a> {
    src: &'a [u8],
    pub(super) defs: Vec<Def>,
    /// The names of the classes, modules and constants of `defs` (R10-03).
    constants: rustc_hash::FxHashSet<String>,
    /// Parallel to `defs`: made by a declaration (`attr_reader`,
    /// `delegate`), not by a `def`.
    pub(super) generated: Vec<bool>,
    /// Parallel to `defs`: written where it may run or not, or more than
    /// once (a branch, a loop, a block).
    conditional: Vec<bool>,
    /// How many of the nodes around the walk make what they hold
    /// conditional.
    branches: u32,
    /// What the symbols cannot say ([`Fact::Compact`], [`Fact::Unknown`]).
    pub(super) facts: Vec<Fact>,
    /// What the regions the grammar could not parse may define.
    unread: Unread,
    pub(super) sites: Vec<Site>,
    /// The open classes, modules and `class_methods` modules, innermost
    /// last: what a `def` is qualified with.
    pub(super) owners: Vec<Owner>,
    /// The lexical class and module nesting (Ruby's cref), outermost first,
    /// each fully qualified. Unlike `owners`, a `class_methods` block opens
    /// no entry: its body is a block, evaluated in the module's scope.
    pub(super) cref: Vec<String>,
    /// What a definition on a constant keeps of `cref`, once read.
    kept: std::cell::OnceCell<std::rc::Rc<[String]>>,
    /// The enclosing class, module, `def` and `class <<` nodes, innermost
    /// last: the body a declaration is written in.
    bounds: Vec<Bound>,
    /// The indices in `defs` of the definitions the walk is in, innermost
    /// last: a class or module around its body, a `def` around its
    /// parameters and body, a `class_methods` module around its block, a
    /// declaration's first generated method around its arguments (the
    /// lambda of a `scope`). A site written here belongs to the last one,
    /// and to the file when there is none.
    scopes: Vec<usize>,
    pub(super) locals: Locals,
    /// The `concerning` calls the walk is in, innermost last.
    concerned: Vec<Concerned>,
    /// The constants assigned a literal array of names (`ATTRS = %i[a
    /// b]`), which a splat in a declaration reads (`attr_reader(*ATTRS)`).
    pub(super) arrays: rustc_hash::FxHashMap<String, Vec<String>>,
    /// The constants assigned a literal array of constants (`MODULES =
    /// [A, B]`), with the lexical scopes they were written in, which an
    /// `each` loop including them reads (`MODULES.each { |m| include m }`).
    pub(super) modules: rustc_hash::FxHashMap<String, (Vec<String>, Vec<String>)>,
    /// Each hook method (an index in `defs`) whose statements a replay
    /// reads, with its parameter, the object it runs for (D14).
    pub(super) hook_bases: rustc_hash::FxHashMap<usize, String>,
    /// The methods (indexes in `defs`) whose body passes a block on to one
    /// that runs it with another `self` (R5-11).
    rebinding: rustc_hash::FxHashSet<usize>,
    /// The names of this file's methods whose body does so: a block given
    /// to one may run with another `self` (R6-17).
    rebinders: rustc_hash::FxHashSet<String>,
    /// The nodes that hold an early exit outside a method they define
    /// ([`flow::exits`]).
    exits: rustc_hash::FxHashSet<usize>,
    /// The names each body reads uncalled ([`flow::Uncalled`], R9-01).
    uncalled: flow::Uncalled,
    /// The locals each open scope gate binds once (ADR-0038).
    values: values::Gates<'a>,
    /// The file is a Minitest one, whose test blocks run on a test case.
    minitest: bool,
    /// The file is a spec file, whose example groups are read (ADR-0041).
    rspec: bool,
    /// The file is RSpec's code: a spec file or a file under `spec/`.
    spec_code: bool,
    /// What each block, `def` and body around the walk makes of `self`, in
    /// a spec file ([`rspec::Frame`]).
    frames: Vec<rspec::Frame>,
    /// The `let` and `subject` blocks the walk is in.
    lets: Vec<rspec::LetBlock>,
    /// The `include` calls written in an example group's body that the
    /// group records as its steps ([`Fact::GroupInclude`]).
    group_mixins: rustc_hash::FxHashSet<usize>,
}

/// `concerning :Topic do` (B6): the module it makes, which the owner
/// around it includes once the block has run.
struct Concerned {
    /// The call's node.
    call: usize,
    /// The including class or module's index in `defs`.
    includer: usize,
    module: String,
    /// The topic as written (`:Topic`).
    written: String,
    prepend: bool,
}

/// An open class or module, fully qualified, with its index in `defs`.
pub(super) struct Owner {
    pub(super) qualified: String,
    pub(super) def: usize,
}

/// The nearest node that changes what `self` is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BoundKind {
    Class,
    Module,
    Method,
    SingletonMethod,
    SingletonClass,
    /// A block in a class or module body that may run on another object
    /// (`class_eval` on another class, `instance_eval`, an association's
    /// extension): nothing written there declares anything of the class.
    Foreign,
}

struct Bound {
    kind: BoundKind,
    /// How many `included do` blocks the walk is in, inside this body: an
    /// `include` there runs on the class that includes the concern.
    included: u32,
    /// How many `prepended do` blocks, which run on the class that
    /// prepends the concern.
    prepended: u32,
    /// The walk's branch count where this body starts, then where each
    /// `included do` or `prepended do` block in it starts: a mixin written
    /// under more may not run (R5-07).
    branches: Vec<u32>,
}

/// Where a receiver-less `include`, `prepend` or `extend` is written, and
/// so on which ancestors it acts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MixinBody {
    /// A class or module body, directly or through blocks that keep it.
    Owner,
    /// A concern's `included do` block, run on the class that includes it.
    Included,
    /// A concern's `prepended do` block, run on the class that prepends it.
    Prepended,
    /// `class << self` of the class or module: its singleton class (D35).
    Singleton,
}

/// What `self` is where a site is written, against the definition the
/// site belongs to (D08).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum This {
    /// What the site's source runs on: the file's `main`, the class or
    /// module of a body, an instance in an instance method, the class in a
    /// method of the class itself.
    Own,
    /// The class that includes or prepends the concern whose `included do`
    /// or `prepended do` block this is (Active Support evaluates it there).
    Includer,
    /// Rails' `with_options` option merger, which forwards calls to the
    /// class with its options merged in.
    Merger,
    /// An object the rules cannot name: a top-level block (`describe do`,
    /// Sinatra's `get do`), a class-body block a macro evaluates on an
    /// instance (`before_action do`, `scope :x, -> {}`), a block passed to
    /// `instance_eval` and its family, the singleton class in a `class <<
    /// self` body.
    Unknown,
}

fn bound_of(kind: &str) -> Option<BoundKind> {
    Some(match kind {
        "class" => BoundKind::Class,
        "module" => BoundKind::Module,
        "method" => BoundKind::Method,
        "singleton_method" => BoundKind::SingletonMethod,
        "singleton_class" => BoundKind::SingletonClass,
        _ => return None,
    })
}

impl<'a> Walker<'a> {
    fn new(src: &'a [u8]) -> Self {
        Walker {
            src,
            defs: Vec::new(),
            constants: rustc_hash::FxHashSet::default(),
            generated: Vec::new(),
            conditional: Vec::new(),
            branches: 0,
            facts: Vec::new(),
            unread: Unread::default(),
            sites: Vec::new(),
            owners: Vec::new(),
            cref: Vec::new(),
            kept: std::cell::OnceCell::new(),
            bounds: Vec::new(),
            scopes: Vec::new(),
            locals: Locals::default(),
            concerned: Vec::new(),
            arrays: rustc_hash::FxHashMap::default(),
            modules: rustc_hash::FxHashMap::default(),
            hook_bases: rustc_hash::FxHashMap::default(),
            rebinding: rustc_hash::FxHashSet::default(),
            rebinders: rustc_hash::FxHashSet::default(),
            exits: rustc_hash::FxHashSet::default(),
            uncalled: flow::Uncalled::default(),
            values: values::Gates::default(),
            minitest: false,
            rspec: false,
            spec_code: false,
            frames: Vec::new(),
            lets: Vec::new(),
            group_mixins: rustc_hash::FxHashSet::default(),
        }
    }

    pub(super) fn text(&self, node: Node) -> String {
        String::from_utf8_lossy(&self.src[node.byte_range()]).into_owned()
    }

    pub(super) fn field_text(&self, node: Node, field: &str) -> Option<String> {
        node.child_by_field_name(field)
            .map(|child| self.text(child))
    }

    /// The first line of `node`, cut from the start of its line so that it
    /// is a prefix of the trimmed source line: `def b; end` alone is not
    /// one of `private def b; end`.
    fn signature(&self, node: Node) -> String {
        let start = self.src[..node.start_byte()]
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |i| i + 1);
        text::signature(&String::from_utf8_lossy(&self.src[start..node.end_byte()]))
    }

    /// The nearest enclosing class, module, `def` or `class <<`; blocks are
    /// transparent.
    pub(super) fn nearest(&self) -> Option<BoundKind> {
        self.bounds.last().map(|bound| bound.kind)
    }

    /// Where a receiver-less mixin written here acts: a class or module
    /// body, directly or through blocks, possibly a concern's `included do`
    /// or `prepended do`, or the body of the owner's `class << self`;
    /// `None` in a method body, in `class << x` or outside any owner (v1
    /// `ruby_mixins::owner_body`).
    pub(super) fn mixin_body(&self) -> Option<MixinBody> {
        let bound = self.bounds.last()?;
        match bound.kind {
            BoundKind::Class | BoundKind::Module if bound.included > 0 => Some(MixinBody::Included),
            BoundKind::Class | BoundKind::Module if bound.prepended > 0 => {
                Some(MixinBody::Prepended)
            }
            BoundKind::Class | BoundKind::Module => Some(MixinBody::Owner),
            BoundKind::SingletonClass if self.locals.current_defs() == Defs::Singleton => {
                Some(MixinBody::Singleton)
            }
            _ => None,
        }
    }

    /// How many branches the walk is under inside the body a receiver-less
    /// mixin written here acts in ([`Self::mixin_body`]): a condition, a
    /// loop, a `rescue`, a block that may run no time or more than once;
    /// and whether that body itself may not run, the class or module
    /// written under one (R6-07).
    pub(super) fn branches_in_body(&self) -> (u32, bool) {
        let Some(bound) = self.bounds.last() else {
            return (self.branches, false);
        };
        let start = bound.branches.last().copied().unwrap_or_default();
        let around = bound.branches.first().is_some_and(|&first| first > 0);
        (self.branches.saturating_sub(start), around)
    }

    /// What `self` is at the current point of the walk.
    pub(super) fn this(&self) -> This {
        self.locals.this()
    }

    /// Records a definition spanning `node`; `None` for an empty name.
    pub(super) fn push_def(
        &mut self,
        kind: SymbolKind,
        name: String,
        node: Node,
        generated: bool,
    ) -> Option<usize> {
        if name.is_empty() {
            return None;
        }
        if matches!(
            kind,
            SymbolKind::Class | SymbolKind::Module | SymbolKind::Const
        ) {
            self.constants.insert(name.clone());
        }
        // RSpec's own classes reopened may change its DSL (ADR-0041).
        if matches!(kind, SymbolKind::Class | SymbolKind::Module)
            && (name == "RSpec::Core" || name.starts_with("RSpec::Core::"))
        {
            self.facts.push(Fact::RspecOff);
        }
        self.defs.push(Def {
            kind,
            name,
            signature: self.signature(node),
            line: line(node),
            end_line: node.end_position().row as u32 + 1,
        });
        self.generated.push(generated);
        // A `def` nested in a method exists once that method has run.
        self.conditional
            .push(self.branches > 0 || (kind == SymbolKind::Method && self.locals.in_method()));
        Some(self.defs.len() - 1)
    }

    /// The methods and functions defined in this file at least once where
    /// the definition may not run (under a condition, in a block, in
    /// another method's body): whether, and which one, Ruby has depends on
    /// run time (D06, M2).
    fn unsettled(&self) -> Vec<String> {
        let mut seen: std::collections::BTreeSet<&str> = Default::default();
        for (def, conditional) in self.defs.iter().zip(&self.conditional) {
            if *conditional && matches!(def.kind, SymbolKind::Method | SymbolKind::Function) {
                seen.insert(def.name.as_str());
            }
        }
        seen.into_iter().map(str::to_owned).collect()
    }

    /// Records that a lookup through the open owner on the side of `defs`
    /// cannot know whether it holds `name` (`None`: any name).
    pub(super) fn unknown(&mut self, sides: &[char], name: Option<String>) {
        let Some(owner) = self.owners.last().map(|o| o.qualified.clone()) else {
            return;
        };
        for side in sides {
            self.facts.push(Fact::Unknown {
                owner: owner.clone(),
                side: *side,
                name: name.clone(),
            });
        }
    }

    /// A [`Context`] with the cref at the current point of the walk,
    /// innermost first.
    pub(super) fn context(&self, name: &str, qualifier: &str) -> Context {
        Context {
            name: name.to_owned(),
            qualifier: qualifier.to_owned(),
            cref: self.cref.iter().rev().cloned().collect(),
            extra: Vec::new(),
        }
    }

    /// A site of `owner` (an index in `defs`, or [`FILE_OWNER`]); `None`
    /// for the definition the walk is in.
    pub(super) fn push_site(
        &mut self,
        kind: EdgeKind,
        line: u32,
        owner: Option<usize>,
        target: String,
        context: &Context,
    ) {
        let owner = owner.unwrap_or_else(|| self.scopes.last().copied().unwrap_or(FILE_OWNER));
        self.sites.push(Site {
            kind,
            line,
            owner: Some(owner),
            target,
            context: context.encode(),
        });
    }

    /// A call of `name` on `receiver` as written (`None` for no receiver),
    /// at `line`, whose qualifier is the receiver's value path when it has
    /// one (`typed`, ADR-0038), else the receiver as written.
    fn push_call(&mut self, name: &str, receiver: Option<&str>, typed: Option<&str>, line: u32) {
        if name.is_empty() {
            return;
        }
        let qualifier = typed.or(receiver).unwrap_or_default();
        let mut context = self.context(name, qualifier);
        if self.this() != This::Own {
            context.extra = vec![REBOUND.to_owned()];
        }
        // Written where `self` is an example of a group (ADR-0041).
        if let Some(group) = self.group_marker() {
            context.extra.push(group);
        }
        let target = values::call_target(name, receiver);
        self.push_site(EdgeKind::Call, line, None, target, &context);
    }

    /// A reference to the method `name` that the DSL call `call` names
    /// (`before_action :load`), at the call's line.
    pub(super) fn push_reference(&mut self, name: &str, qualifier: &str, call: Node) {
        if name.is_empty() {
            return;
        }
        let mut context = self.context(name, qualifier);
        // A `send` goes to `self`; a callback is declared on the class,
        // which an `included do` block or `with_options` still reach.
        let rebound = match qualifier {
            "send" | "public_send" => self.this() != This::Own,
            _ => self.this() == This::Unknown,
        };
        if rebound {
            context.extra = vec![REBOUND.to_owned()];
        }
        self.push_site(
            EdgeKind::Reference,
            line(call),
            None,
            name.to_owned(),
            &context,
        );
    }

    fn walk(
        &mut self,
        node: Node<'a>,
        parent: Option<Node<'a>>,
        field: Option<&'a str>,
        depth: usize,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        if node.is_error() {
            self.imports_in(node, depth);
            self.unread.merge(unread::scan(&self.text(node)));
            return;
        }
        if !self.lets.is_empty() {
            rspec::let_value(self, node);
        }
        let framed = self.rspec && rspec::enter(self, node, parent);
        let kind = node.kind();
        let block =
            matches!(kind, "block" | "do_block" | "lambda").then(|| self.block_kind(kind, parent));
        let scope = scope_of(kind);
        if let Some(scope) = scope {
            self.open(scope, node, parent, block);
        }
        let branch = is_branch(self, node, block);
        if branch {
            self.branches += 1;
        }
        let mut owner_pushed = false;
        let mut body_cref = None;
        let mut included = false;
        let mut prepended = false;
        let mut captures = Vec::new();
        // The definition that owns the sites of every child (`opened`), or
        // of the body only (a class or module, whose name and superclass
        // are read outside it).
        let mut opened = None;
        let mut body_scope = None;
        match kind {
            "module" | "class" => {
                body_cref = self.container(node, kind);
                owner_pushed = body_cref.is_some();
                body_scope = self.owners.last().filter(|_| owner_pushed).map(|o| o.def);
            }
            // The body of the owner's `class << self` is a constant scope
            // of its own, the singleton class's (R2).
            "singleton_class" if self.locals.current_defs() == Defs::Singleton => {
                body_cref = self
                    .owners
                    .last()
                    .map(|owner| super::singleton_scope(&owner.qualified));
            }
            "singleton_class" => body_cref = self.singleton_body(),
            "method" | "singleton_method" => {
                opened = if kind == "method" {
                    self.method(node)
                } else {
                    self.singleton_method(node)
                };
                if let Some(def) = opened {
                    dsl::hook(self, node, def);
                    if passes_block_on(self, node) {
                        self.rebinding.insert(def);
                    }
                }
            }
            "call" => (owner_pushed, opened) = self.call(node, parent, field),
            "identifier" => self.identifier(node, role(parent, field)),
            // Not the keyword's own token, of the same kind.
            "alias" if node.is_named() => dsl::alias_keyword(self, node),
            "undef" if node.is_named() => dsl::undef_keyword(self, node),
            "assignment" | "operator_assignment" => self.constants(node),
            "constant" | "scope_resolution"
                if parent.is_none_or(|p| p.kind() != "scope_resolution") =>
            {
                self.read_array(node, parent, field)
            }
            "binary" => captures = self.named_captures(node),
            // `in {target:}` binds `target` although no identifier is
            // written.
            "keyword_pattern" if node.child_by_field_name("value").is_none() => {
                if let Some(key) = self.field_text(node, "key") {
                    self.locals.bind(key);
                }
            }
            // `render(target:)` and `{ target: }` read `target` like a bare
            // name: Ruby 3.1 shorthand for `target: target`.
            "pair" if node.child_by_field_name("value").is_none() => {
                if let Some(key) = self.field_text(node, "key")
                    && !self.locals.is_local(&key)
                {
                    self.push_call(&key, None, None, line(node));
                }
            }
            "block" | "do_block" => {
                included = parent.is_some_and(|p| dsl::is_concern_call(self, p, "included"));
                prepended = parent.is_some_and(|p| dsl::is_concern_call(self, p, "prepended"));
                // The block of `X = Class.new do`: its sites run with the
                // new class as `self`.
                if matches!(block, Some(BlockKind::Defines { .. })) {
                    opened = self.owners.last().map(|o| o.def);
                }
            }
            _ => {}
        }
        let bound = bound_of(kind).or_else(|| match block {
            Some(BlockKind::Foreign)
                if matches!(
                    self.nearest(),
                    Some(BoundKind::Class | BoundKind::Module | BoundKind::SingletonClass)
                ) =>
            {
                Some(BoundKind::Foreign)
            }
            Some(BlockKind::Defines { module: true }) => Some(BoundKind::Module),
            Some(BlockKind::Defines { module: false }) => Some(BoundKind::Class),
            _ => None,
        });
        if let Some(kind) = bound {
            self.bounds.push(Bound {
                kind,
                included: 0,
                prepended: 0,
                branches: vec![self.branches],
            });
        }
        if included && let Some(bound) = self.bounds.last_mut() {
            bound.included += 1;
            bound.branches.push(self.branches);
        }
        if prepended && let Some(bound) = self.bounds.last_mut() {
            bound.prepended += 1;
            bound.branches.push(self.branches);
        }
        if let Some(def) = opened {
            self.scopes.push(def);
        }
        let children = children(node);
        // `BEGIN { }` runs before the rest of the file (R8-04).
        let mut past = kind == "program"
            && children.iter().any(|(child, _)| {
                child.kind() == "begin_block" && flow::leaves_early(self, *child)
            });
        if past {
            self.branches += 1;
        }
        for (child, child_field) in children {
            let enter = child_field == Some("body") && body_cref.is_some();
            if enter && let Some(qualified) = &body_cref {
                self.cref.push(qualified.clone());
                self.kept.take();
                self.scopes.extend(body_scope);
            }
            self.walk(child, Some(node), child_field, depth + 1);
            if enter {
                self.cref.pop();
                self.kept.take();
                if body_scope.is_some() {
                    self.scopes.pop();
                }
            }
            // What follows a statement that may leave early, at any depth
            // but in a method's body, may not run: the rest of the file
            // past a `return` or an `abort`, of a class body past a
            // `raise`, of a block Ruby runs once past a `next`, wrapped in
            // `begin` or parentheses or not (R6-10, R7-04, R8-04).
            if !past && flow::leaves_early(self, child) {
                self.branches += 1;
                past = true;
            }
        }
        if past {
            self.branches -= 1;
        }
        // Ruby binds a named capture once the match is parsed.
        for name in captures {
            self.locals.bind(name);
        }
        if opened.is_some() {
            self.scopes.pop();
        }
        if included && let Some(bound) = self.bounds.last_mut() {
            bound.included -= 1;
            bound.branches.pop();
        }
        if prepended && let Some(bound) = self.bounds.last_mut() {
            bound.prepended -= 1;
            bound.branches.pop();
        }
        if bound.is_some() {
            self.bounds.pop();
        }
        if branch {
            self.branches -= 1;
        }
        if scope.is_some() {
            self.locals.close();
            if scope == Some(Scope::Gate) {
                self.values.pop();
            }
        }
        if owner_pushed {
            self.owners.pop();
        }
        // The module `concerning` made is included once its block has run.
        if self.concerned.last().is_some_and(|c| c.call == node.id()) {
            self.include_concerned(node);
        }
        if framed {
            rspec::leave(self, node);
        }
    }

    /// Opens the frame of `node`, a scope of kind `scope` (the walk's
    /// stack frame stays small: the walk recurses as deep as the tree).
    #[inline(never)]
    fn open(
        &mut self,
        scope: Scope,
        node: Node<'a>,
        parent: Option<Node<'a>>,
        block: Option<BlockKind>,
    ) {
        let kind = node.kind();
        let value_is_self = node
            .child_by_field_name("value")
            .is_some_and(|value| value.kind() == "self");
        let this = self.this_in(kind, node, parent, block);
        let landing = match (kind, block) {
            (_, Some(BlockKind::Foreign)) => Some(self.landing(kind, parent)),
            (_, Some(BlockKind::Included { prepended })) => {
                self.owners.last().map(|owner| Landing::Includers {
                    concern: owner.qualified.clone(),
                    prepended,
                })
            }
            ("singleton_class", _) if !value_is_self => Some(self.singleton_landing(node)),
            _ => None,
        };
        self.locals
            .open(scope, kind, value_is_self, block, this, landing);
        if scope == Scope::Gate {
            let gate = values::Gates::open(self, node);
            self.values.push(gate);
        }
    }

    /// The `include` of the module `concerning` made, by the owner around
    /// the call `node` (B6).
    #[inline(never)]
    fn include_concerned(&mut self, node: Node) {
        let Some(done) = self.concerned.pop() else {
            return;
        };
        let op = if done.prepend { "prepend" } else { "include" };
        let context = self.context(&format!("::{}", done.module), &format!("{MIXIN}{op}"));
        self.push_site(
            EdgeKind::Reference,
            line(node),
            Some(done.includer),
            done.written,
            &context,
        );
    }

    /// `concerning :Topic do ... end` in a class or module body (B6):
    /// Active Support's `concern` makes the module `Owner::Topic`, which
    /// extends `ActiveSupport::Concern` and whose block defines its
    /// methods; the owner then includes it (`prepend: true`, prepends).
    /// Returns whether it opened that module.
    fn concerning(&mut self, call: Node<'a>) -> bool {
        if self.field_text(call, "method").as_deref() != Some("concerning")
            || call.child_by_field_name("receiver").is_some()
            || call.child_by_field_name("block").is_none()
            || !matches!(self.nearest(), Some(BoundKind::Class | BoundKind::Module))
            || self.locals.gate() != Gate::Owner
            || self.locals.current_defs() == Defs::Foreign
            || self.this() != This::Own
        {
            return false;
        }
        let Some((args, includer)) = call
            .child_by_field_name("arguments")
            .zip(self.owners.last().map(|o| (o.qualified.clone(), o.def)))
        else {
            return false;
        };
        let Some((topic, name)) = dsl::positional(args)
            .first()
            .and_then(|&topic| Some((topic, dsl::literal_name(self, topic)?)))
        else {
            return false;
        };
        let mut cursor = args.walk();
        let prepend = args.named_children(&mut cursor).any(|arg| {
            arg.kind() == "pair"
                && self
                    .field_text(arg, "key")
                    .is_some_and(|k| k.trim_start_matches(':').trim_end_matches(':') == "prepend")
                && arg
                    .child_by_field_name("value")
                    .is_some_and(|v| v.kind() == "true")
        });
        let module = format!("{}::{name}", includer.0);
        let Some(def) = self.push_def(SymbolKind::Module, module.clone(), call, false) else {
            return false;
        };
        self.owners.push(Owner {
            qualified: module.clone(),
            def,
        });
        // The `extend ActiveSupport::Concern` that `concerning` writes.
        let context = self.context("ActiveSupport::Concern", &format!("{MIXIN}extend"));
        self.push_site(
            EdgeKind::Reference,
            line(call),
            Some(def),
            "concerning".to_owned(),
            &context,
        );
        let written = self.text(topic);
        self.concerned.push(Concerned {
            call: call.id(),
            includer: includer.1,
            module,
            written,
            prepend,
        });
        true
    }

    /// Where a block's `def`s go, from the call it is passed to (D01).
    fn block_kind(&self, kind: &str, parent: Option<Node>) -> BlockKind {
        let Some(call) = parent.filter(|p| kind != "lambda" && p.kind() == "call") else {
            return BlockKind::Foreign;
        };
        if let Some((symbol, _)) = self.constructor(call) {
            return BlockKind::Defines {
                module: symbol == SymbolKind::Module,
            };
        }
        let receiver = self.field_text(call, "receiver");
        let bare = receiver.is_none();
        let on_self = receiver.as_deref().is_none_or(|r| r == "self");
        // `self` is the class or module whose body this is, or its
        // singleton class in `class << self`.
        let defs = self.locals.current_defs();
        let in_body = defs != Defs::Foreign
            && match self.locals.gate() {
                Gate::Owner => !self.owners.is_empty(),
                Gate::SingletonClass => defs == Defs::Singleton,
                Gate::Top | Gate::Method => false,
            };
        match self.field_text(call, "method").as_deref() {
            Some(method) if ITERATORS.contains(&method) => BlockKind::Keeps,
            // A core collection's iterator yields to its block where it is
            // written: on a literal, or on a constant (`TYPES.each_value do
            // |t| define_method("#{t}?")`, #106), read as the core
            // collection it holds, as `each` on any receiver is.
            Some(method)
                if COLLECTION_ITERATORS.contains(&method)
                    && call
                        .child_by_field_name("receiver")
                        .is_some_and(|r| self.collection(r)) =>
            {
                BlockKind::Keeps
            }
            Some(method @ ("included" | "prepended")) if bare => BlockKind::Included {
                prepended: method == "prepended",
            },
            // The module `concerning` makes (B6).
            Some("concerning") if self.concerned.last().is_some_and(|c| c.call == call.id()) => {
                BlockKind::Defines { module: true }
            }
            Some("class_methods") if bare => BlockKind::Once,
            Some("with_options") if bare => BlockKind::Forwards,
            Some("class_eval" | "class_exec" | "module_eval" | "module_exec")
                if on_self && in_body =>
            {
                BlockKind::ClassEval
            }
            // In `class << self` the definee would be the singleton
            // class's own singleton class, which no lookup reads.
            Some("instance_eval" | "instance_exec")
                if on_self && in_body && self.locals.gate() == Gate::Owner =>
            {
                BlockKind::InstanceEval
            }
            _ => BlockKind::Foreign,
        }
    }

    /// Whether `receiver` is a literal collection or a constant, whose
    /// iterators are read as a core collection's: a constant holding an
    /// object of the application's whose `each_value` evaluates its block
    /// elsewhere is a known residual, as `each` on any receiver is.
    fn collection(&self, receiver: Node) -> bool {
        matches!(
            receiver.kind(),
            "array" | "hash" | "string_array" | "symbol_array"
        ) || dsl::constant_text(self, receiver).is_some()
    }

    /// Where the `def`s of a [`BlockKind::Foreign`] block written here may
    /// land: on the class or module a constant receiver of `class_eval` or
    /// its family names; nowhere a lookup reads for an association's
    /// extension, a new anonymous class or module, a refinement, or outside
    /// every class or module body; else, in a body, maybe on its owner,
    /// since a method the rules do not model may evaluate the block there.
    /// A block in a method body runs on whatever object calls the method (a
    /// class macro's `class_eval do`), which the rules do not model.
    fn landing(&self, kind: &str, parent: Option<Node>) -> Landing {
        let call = parent.filter(|p| kind != "lambda" && p.kind() == "call");
        let method = call
            .and_then(|call| self.field_text(call, "method"))
            .unwrap_or_default();
        let receiver = call.and_then(|call| call.child_by_field_name("receiver"));
        let constant = receiver.and_then(|r| dsl::constant_text(self, r));
        if let Some(path) = &constant
            && EVALUATORS.contains(&method.as_str())
        {
            return Landing::Constant {
                path: path.clone(),
                scope: self.context("", "").cref,
                singleton: method.starts_with("instance_"),
            };
        }
        let receiver_text = receiver.map(|r| self.text(r));
        // In a method body a block passed to a method on `self` runs when
        // the method does, maybe evaluated on whatever object calls it: a
        // class macro's (B7).
        if self.locals.gate() == Gate::Method {
            return match self.enclosing_method() {
                Some((owner, side))
                    if call.is_some()
                        && matches!(receiver_text.as_deref(), None | Some("self"))
                        && self.this() == This::Own =>
                {
                    Landing::Macro { owner, side }
                }
                _ => Landing::Nowhere,
            };
        }
        let Some(owner) = self.owners.last().map(|o| o.qualified.clone()) else {
            return Landing::Nowhere;
        };
        match (constant.as_deref(), method.as_str()) {
            (Some("Class" | "Module" | "Struct" | "::Class" | "::Module" | "::Struct"), "new")
            | (Some("Data" | "::Data"), "define") => Landing::Nowhere,
            // Evaluated on another object, which defines there: the owner
            // only as `self` or its singleton class.
            (None, method)
                if EVALUATORS.contains(&method)
                    && !matches!(
                        receiver_text.as_deref(),
                        None | Some("self" | "singleton_class" | "self.singleton_class")
                    ) =>
            {
                Landing::Nowhere
            }
            (None, "refine") if call.is_some() && receiver.is_none() => Landing::Nowhere,
            (None, method)
                if call.is_some() && receiver.is_none() && dsl::is_association_macro(method) =>
            {
                Landing::Nowhere
            }
            _ => Landing::Owner(owner),
        }
    }

    /// Whether the walk is in a block whose `def`s get no owner but may
    /// land on a class or module ([`Landing`]): what it declares is read
    /// as what may be defined there.
    pub(super) fn foreign(&self) -> bool {
        self.locals.current_defs() == Defs::Foreign && self.locals.landing() != Landing::Nowhere
    }

    /// Records that a method `name` (`None`: any name) the rules give no
    /// owner may land where the innermost frame's `def`s land, and taints
    /// it where they cannot name that object ([`Self::unplaced`]).
    pub(super) fn may_define(&mut self, defined: Defined, name: Option<String>) {
        if self.unplaced() {
            self.taint(name.clone());
        }
        self.landed(defined, name);
    }

    /// Records that a method `name` (`None`: any name) the rules give no
    /// owner may land where the innermost frame's `def`s land: on either
    /// side of an owner a method the rules do not model may evaluate the
    /// block on, on the side `defined` says of a constant's class.
    pub(super) fn landed(&mut self, defined: Defined, name: Option<String>) {
        match self.locals.landing() {
            Landing::Nowhere => {}
            // Evaluated there with `class_eval` or `instance_eval`, or
            // only yielded to: `self` is the owner either way.
            Landing::Owner(owner) => {
                let sides: &[char] = match defined {
                    Defined::OnSelf | Defined::Sent('.') => &['.'],
                    Defined::Sent(_) => &['#'],
                    Defined::Def | Defined::Any => &['#', '.'],
                };
                for side in sides {
                    self.facts.push(Fact::Unknown {
                        owner: owner.clone(),
                        side: *side,
                        name: name.clone(),
                    });
                }
            }
            Landing::Constant {
                path,
                scope,
                singleton,
            } => {
                let sides: &[char] = match defined {
                    Defined::Def if singleton => &['.'],
                    Defined::Def => &['#'],
                    Defined::OnSelf => &['.'],
                    Defined::Sent('.') => &['.'],
                    Defined::Sent(_) => &['#'],
                    Defined::Any => &['#', '.'],
                };
                for side in sides {
                    self.facts.push(Fact::Evaluated {
                        path: path.clone(),
                        scope: scope.clone(),
                        side: *side,
                        name: name.clone(),
                    });
                }
            }
            // `def self.m` in `class << x` defines on the singleton class's
            // own singleton, which no lookup of `x` reads; anything else on
            // `x` itself.
            Landing::SingletonOf { .. } | Landing::SingletonOfOwner(_)
                if defined == Defined::OnSelf => {}
            Landing::SingletonOf { path, scope, .. } => self.facts.push(Fact::Evaluated {
                path,
                scope: scope.to_vec(),
                side: '.',
                name,
            }),
            Landing::SingletonOfOwner(owner) => self.facts.push(Fact::Unknown {
                owner,
                side: '.',
                name,
            }),
            // Evaluated on the macro's caller, as `class_eval` would.
            Landing::Macro {
                owner,
                side: method,
            } => {
                let sides: &[char] = match defined {
                    Defined::Def | Defined::Sent('#') => &['#'],
                    Defined::OnSelf | Defined::Sent(_) => &['.'],
                    Defined::Any => &['#', '.'],
                };
                for side in sides {
                    self.facts.push(Fact::Macro {
                        owner: owner.clone(),
                        method,
                        side: *side,
                        name: name.clone(),
                    });
                }
            }
            // Evaluated on the includer, as `class_eval` would.
            Landing::Includers { concern, prepended } => {
                let sides: &[char] = match defined {
                    Defined::Def | Defined::Sent('#') => &['#'],
                    Defined::OnSelf | Defined::Sent(_) => &['.'],
                    Defined::Any => &['#', '.'],
                };
                for side in sides {
                    self.facts.push(Fact::Block {
                        concern: concern.clone(),
                        prepended,
                        side: *side,
                        name: name.clone(),
                    });
                }
            }
        }
    }

    /// What `self` is in the frame a node of `kind` opens (D08). A gate
    /// sets its own: the file's `main`, a class or module, a method's
    /// receiver; in a `class << self` body it is the singleton class, on
    /// which no method of the class is called. A block runs where it is
    /// written unless the method it is passed to evaluates it elsewhere:
    /// at the top level any block may be (RSpec's `describe`, Sinatra's
    /// `get`); in a class or module body only iteration keeps the class, a
    /// concern's `included` and `prepended` run on the includer,
    /// `with_options` on its option merger, and `define_method` or the
    /// block of `X = Class.new` are the method or class the site belongs
    /// to; in a method body a block keeps its `self` but one passed to
    /// `instance_eval`, `instance_exec`, `class_eval`, `class_exec`,
    /// `module_eval`, `module_exec`, `define_method` (run on an instance),
    /// `Class.new`, `Module.new`, `Struct.new` or `Data.define`, called or
    /// sent (R7-10), or sent by a name the rules cannot read (R8-07); a
    /// lambda or proc keeps it only called where it is written (R8-08).
    fn this_in(
        &self,
        kind: &str,
        node: Node,
        parent: Option<Node>,
        block: Option<BlockKind>,
    ) -> This {
        match kind {
            "singleton_class" => return This::Unknown,
            "block" | "do_block" | "lambda" => {}
            _ => return This::Own,
        }
        let call = parent.filter(|p| kind != "lambda" && p.kind() == "call");
        let method = call.and_then(|c| self.field_text(c, "method"));
        let receiver = call.and_then(|c| self.field_text(c, "receiver"));
        let inner = match (block, self.locals.gate()) {
            // The new class or the `ClassMethods` module is the site's
            // source, and `self` there.
            (Some(BlockKind::Defines { .. }), _) => return This::Own,
            (Some(BlockKind::Once), _) if method.as_deref() == Some("class_methods") => {
                return This::Own;
            }
            // `class_eval` and `instance_eval` sent to the body's own
            // class run with it as `self`, as the body does.
            (Some(BlockKind::ClassEval | BlockKind::InstanceEval), _) => This::Own,
            (_, Gate::Top | Gate::SingletonClass) => This::Unknown,
            (Some(BlockKind::Included { .. }), Gate::Owner) => This::Includer,
            (Some(BlockKind::Keeps), Gate::Owner) => This::Own,
            (Some(BlockKind::Forwards), Gate::Owner) => This::Merger,
            (_, Gate::Owner) => match (method.as_deref(), call) {
                (Some("define_method" | "define_singleton_method"), Some(call))
                    if receiver.is_none() && dsl::names_literally(self, call) =>
                {
                    This::Own
                }
                _ => This::Unknown,
            },
            (_, Gate::Method) => {
                let receiver = receiver.as_deref().map(|r| r.trim_start_matches("::"));
                // A block given to `send(:instance_exec)` and its family is
                // the sent method's (R7-10), and one given to `send(mode)`
                // may be any method's (R8-07).
                let method = call.and_then(|call| flow::runs(self, call));
                let rebinds = match (receiver, method.as_deref()) {
                    (Some(r), Some("define_singleton_method")) => r != "self",
                    (_, Some("define_singleton_method")) => false,
                    (_, Some(method)) if flow::rebinds(self, method) => true,
                    (Some("Class" | "Module" | "Struct"), Some("new")) => true,
                    (Some("Data"), Some("define")) => true,
                    _ => false,
                };
                // The body passes a block on to be run with another `self`,
                // which may be this one (R5-11); or this lambda or proc is
                // passed, stored or returned, where it may be rebound
                // (R7-12, R8-08).
                let passed = self
                    .scopes
                    .last()
                    .is_some_and(|def| self.rebinding.contains(def));
                if rebinds || passed || flow::escapes(self, kind, node, parent) {
                    This::Unknown
                } else {
                    This::Own
                }
            }
        };
        match (self.locals.this(), inner) {
            (This::Own, inner) => inner,
            (outer, This::Own) => outer,
            _ => This::Unknown,
        }
    }

    /// The names `/(?<name>..)/ =~ s` binds as locals: a regexp literal
    /// without interpolation, written to the left of `=~`, binds each named
    /// group whose name can be a local. `s =~ /(?<name>..)/`, a
    /// parenthesized regexp and `match` bind nothing (D33, Ruby 3.3).
    fn named_captures(&self, binary: Node) -> Vec<String> {
        let Some(left) = binary.child_by_field_name("left").filter(|left| {
            left.kind() == "regex"
                && binary
                    .child_by_field_name("operator")
                    .is_some_and(|op| self.text(op) == "=~")
        }) else {
            return Vec::new();
        };
        let mut source = String::new();
        for (part, _) in children(left) {
            match part.kind() {
                "string_content" | "escape_sequence" => source.push_str(&self.text(part)),
                "interpolation" => return Vec::new(),
                _ => {}
            }
        }
        group_names(&source)
    }

    /// `module A::B` or `class C < D`: the symbol, the owner its `def`s are
    /// qualified with, and for a class its superclass. Returns the
    /// qualified name, the cref entry of the body.
    fn container(&mut self, node: Node, kind: &str) -> Option<String> {
        let name = self.field_text(node, "name")?;
        let qualified = self.qualify(&name);
        let symbol = if kind == "class" {
            SymbolKind::Class
        } else {
            SymbolKind::Module
        };
        let def = self.push_def(symbol, qualified.clone(), node, false)?;
        self.owners.push(Owner {
            qualified: qualified.clone(),
            def,
        });
        if kind == "class" {
            self.superclass(node, def);
        }
        Some(qualified)
    }

    /// `class C < B`: an ancestor of `C`, read in the scope outside it.
    fn superclass(&mut self, class: Node, def: usize) {
        let Some(superclass) = class.child_by_field_name("superclass") else {
            return;
        };
        let mut cursor = superclass.walk();
        let Some(expr) = superclass.named_children(&mut cursor).next() else {
            return;
        };
        let target = dsl::constant_text(self, expr);
        let qualifier = format!("{}superclass", super::MIXIN);
        // `class Sub < self` in a class body: that class (B5).
        let outer = self
            .owners
            .len()
            .checked_sub(2)
            .map(|i| &self.owners[i].qualified);
        let own = (expr.kind() == "self")
            .then(|| outer.filter(|outer| self.cref.last() == Some(*outer)))
            .flatten()
            .map(|outer| format!("::{outer}"));
        // The class's own entry is pushed around its body only, so the cref
        // here is the one outside it.
        let named = own.as_deref().or(target.as_deref()).unwrap_or_default();
        let context = self.context(named, &qualifier);
        let written = target.unwrap_or_else(|| one_line(&self.text(expr)));
        self.push_site(
            EdgeKind::Reference,
            line(class),
            Some(def),
            written,
            &context,
        );
    }

    /// The owner and side (`#` or `.`) of the method whose body the walk
    /// is in, when it has an owner.
    pub(super) fn enclosing_method(&self) -> Option<(String, char)> {
        let def = &self.defs[*self.scopes.last()?];
        if def.kind != SymbolKind::Method {
            return None;
        }
        let at = def.name.rfind(['#', '.'])?;
        let side = def.name[at..].chars().next()?;
        Some((def.name[..at].to_owned(), side))
    }

    /// Records that the method whose body the walk is in, a class macro,
    /// may define `name` (`None`: any) on `side` of whatever object it runs
    /// on (B7).
    pub(super) fn macro_defines(&mut self, side: char, name: Option<String>) {
        if let Some((owner, method)) = self.enclosing_method() {
            // An instance method defines on the singleton class of the
            // object it runs on, one whose chain holds the owner's instance
            // step: lookups from such objects alone prove nothing of the
            // name (R5-04, #106).
            if method == '#' && side == '.' {
                self.facts.push(Fact::Fronted {
                    owner: owner.clone(),
                    name: name.clone(),
                });
            }
            self.facts.push(Fact::Macro {
                owner,
                method,
                side,
                name,
            });
        }
    }

    /// `def name`: `Owner#name`, `Owner.name` in a singleton frame, both
    /// under a bare `module_function`, or the function `name` at the top
    /// level. In a block or a `class << self` outside every owner it is the
    /// method `name` of an object the rules cannot name, as v1 names a
    /// top-level `def self.name`: the block may be evaluated on any object
    /// (`Module.new do`), and `class << self` defines on `main` alone, so
    /// neither defines a top-level function. Inside an owner, a frame that
    /// may run on another object gives it no owner either (D01, D32).
    fn method(&mut self, node: Node) -> Option<usize> {
        let name = self.field_text(node, "name")?;
        let defs = self.locals.enclosing_defs();
        if rspec::redefines_dsl(self, &name) {
            self.facts.push(Fact::RspecOff);
        }
        // A `def` in an example group is the group's, when RSpec reads the
        // file; else it is tainted as below (ADR-0041).
        let in_group = rspec::def_in_group(self);
        if in_group {
            self.facts.push(Fact::GroupDef { name: name.clone() });
        }
        if defs == Defs::Foreign {
            // In `class << X`, a method of the class or module `X` names
            // (B2).
            if let Landing::SingletonOf { path, scope, read } = self.locals.landing() {
                return self.defined_on(path, scope, &read, &name, node);
            }
            if in_group {
                self.landed(Defined::Def, Some(name.clone()));
            } else {
                self.may_define(Defined::Def, Some(name.clone()));
            }
        }
        let Some(owner) = self.owners.last().map(|owner| owner.qualified.clone()) else {
            let kind = if self.locals.in_block() || defs != Defs::Instance {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            // `main`'s singleton class (R5-04); a foreign frame's `def` is
            // tainted where it lands.
            if kind == SymbolKind::Method && defs != Defs::Foreign && !in_group {
                self.taint(Some(name.clone()));
            }
            return self.push_def(kind, name, node, false);
        };
        if defs == Defs::Foreign {
            return self.push_def(SymbolKind::Method, name, node, false);
        }
        // The first, a method of the module itself under `module_function`,
        // owns the body's sites.
        let mut first = None;
        for separator in defs.defined() {
            let def = self.push_def(
                SymbolKind::Method,
                format!("{owner}{separator}{name}"),
                node,
                false,
            );
            self.hide_defined(defs, 1, *separator, &name);
            first = first.or(def);
        }
        first
    }

    /// `def self.name`: a method of the enclosing class or module itself
    /// (v1 `extract.rs:2254-2265`); `def X.name`, of the class or module the
    /// constant `X` names (B2). On another object (`def obj.name`), or
    /// where `self` is not the class (a method body, a block that may run
    /// elsewhere), it has no owner the rules can name (D32); in a class or
    /// module body, `obj` may be the class itself.
    fn singleton_method(&mut self, node: Node) -> Option<usize> {
        let name = self.field_text(node, "name")?;
        let object = node.child_by_field_name("object");
        if let Some((path, scope, read)) = self.definee(object) {
            return self.defined_on(path, scope, &read, &name, node);
        }
        let on_self = object.is_some_and(|object| object.kind() == "self")
            && !self.locals.enclosing_instance();
        let on_class = on_self && self.locals.enclosing_defs() != Defs::Foreign;
        // `self` in a block that may run on the class is the class.
        if on_self && !on_class {
            self.may_define(Defined::OnSelf, Some(name.clone()));
        }
        if object.is_some_and(|object| object.kind() != "self")
            && self.locals.enclosing_gate() == Gate::Owner
        {
            self.unknown(&['.'], Some(name.clone()));
        }
        let qualified = match self.owners.last() {
            Some(owner) if on_class => format!("{}.{name}", owner.qualified),
            // On another object, or `main` (R5-04); `self` in a foreign
            // frame is tainted where it lands.
            _ if !on_self || on_class => {
                self.taint(Some(name.clone()));
                name
            }
            _ => name,
        };
        self.push_def(SymbolKind::Method, qualified, node, false)
    }

    /// A bare name: a binding, a local read, or a call of `self`.
    fn identifier(&mut self, node: Node, role: Ident) {
        match role {
            Ident::Binding => self.locals.bind(self.text(node)),
            Ident::Expr => {
                let name = self.text(node);
                if name == "it" && flow::may_be_block_parameter(self, node) {
                    return;
                }
                if !self.locals.is_local(&name) {
                    self.visibility_word(&name);
                    self.push_call(&name, None, None, line(node));
                }
            }
            Ident::Name => {}
        }
    }

    /// A `call` node: an import, a declaration that generates methods, or a
    /// call; then the DSL rules its arguments trigger. Returns whether it
    /// opened an owner for its block (a `class_methods` module, the class
    /// of `X = Struct.new`), and the definition that owns the sites of its
    /// arguments and block: that module, or the first method a declaration
    /// generates (a `scope`'s lambda belongs to the scope). A constructed
    /// class owns its block's sites only, never its arguments'.
    #[inline(never)]
    fn call(
        &mut self,
        node: Node<'a>,
        parent: Option<Node>,
        field: Option<&str>,
    ) -> (bool, Option<usize>) {
        let before = self.defs.len();
        if let Some(method) = node.child_by_field_name("method") {
            let name = self.text(method);
            let receiver = values::written(self, node);
            // `module_function()` and `public()` set the mode as the bare
            // words do; with arguments they only touch the methods named.
            if receiver.is_none()
                && node
                    .child_by_field_name("arguments")
                    .is_none_or(|args| args.named_child_count() == 0)
            {
                self.visibility_word(&name);
            }
            self.hide_named(node, &name);
            self.hide_unread(node, &name);
            dsl::mixin_call(self, node, &name);
            dsl::outside(self, node, &name);
            dsl::exec_mixin(self, node, &name);
            dsl::association(self, node, &name);
            if rspec::uses_rspec(self, node, &name) {
                self.facts.push(Fact::Rspec);
            }
            factories::factory(self, node, &name);
            if rspec::prepends_into_config(self, node, &name)
                || (receiver.is_none() && rspec::loads_minitest_spec(self, node, &name))
            {
                self.facts.push(Fact::RspecOff);
            }
            if receiver.is_none() && REQUIRE_METHODS.contains(&name.as_str()) {
                self.import(node, &name);
            } else {
                let declared = matches!(receiver.as_deref(), None | Some("self"))
                    && dsl::declaration(self, node, &name, self.locals.current_defs());
                // A declaration defines its methods, it is not a call of
                // the class body; `super` names no method the rules can
                // prove.
                if !declared && method.kind() != "super" {
                    let line = line(method);
                    let typed = values::typed(self, node);
                    let (receiver, typed) = (receiver.as_deref(), typed.as_deref());
                    let setter = format!("{name}=");
                    match assigned_through(node, parent, field) {
                        Some(Assigned::Plain) => self.push_call(&setter, receiver, typed, line),
                        Some(Assigned::Operator) => {
                            self.push_call(&setter, receiver, typed, line);
                            self.push_call(&name, receiver, typed, line);
                        }
                        None => self.push_call(&name, receiver, typed, line),
                    }
                }
            }
            if !REQUIRE_METHODS.contains(&name.as_str()) {
                dsl::symbol_arguments(self, node, &name);
            }
        }
        // `X = Struct.new do ... end` defines `X`, whose block holds its
        // methods; so does `concerning :X do`.
        if self.constructed(node) || self.concerning(node) {
            return (true, None);
        }
        // `class_methods do ... end` defines the concern's `ClassMethods`
        // module, which the includer extends.
        if let Some(owner) = self.owners.last()
            && dsl::is_class_methods_block(self, node)
        {
            let qualified = format!("{}::ClassMethods", owner.qualified);
            if let Some(def) = self.push_def(SymbolKind::Module, qualified.clone(), node, false) {
                self.owners.push(Owner { qualified, def });
                return (true, Some(def));
            }
        }
        let opened = (self.defs.len() > before).then_some(before);
        // `const_set(:X, v)`: a constant, whose value the class body
        // evaluates; sent too.
        if flow::runs(self, node).as_deref() == Some("const_set") {
            self.const_set(node);
        }
        (false, opened)
    }
}

/// `text` with every run of white space, line breaks included, made one
/// space, cut as a signature is: a target fit for one line of a map.
pub(super) fn one_line(text: &str) -> String {
    text::signature(&text.split_whitespace().collect::<Vec<_>>().join(" "))
}

pub(super) fn line(node: Node) -> u32 {
    node.start_position().row as u32 + 1
}

/// The children of `node`, each with the field it fills.
fn children<'t>(node: Node<'t>) -> Vec<(Node<'t>, Option<&'t str>)> {
    let mut cursor = node.walk();
    let mut out = Vec::new();
    if cursor.goto_first_child() {
        loop {
            out.push((cursor.node(), cursor.field_name()));
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    out
}

/// How a call is the target of an assignment: `recv.name = v` calls the
/// writer `name=`; `recv.name += v` reads with `name` and writes with
/// `name=`. A bare `name = v` assigns a local, so only a call with a
/// receiver counts.
enum Assigned {
    Plain,
    Operator,
}

fn assigned_through(call: Node, parent: Option<Node>, field: Option<&str>) -> Option<Assigned> {
    call.child_by_field_name("receiver")?;
    match (parent?.kind(), field) {
        ("assignment", Some("left")) => Some(Assigned::Plain),
        ("operator_assignment", Some("left")) => Some(Assigned::Operator),
        // `self.a, self.b = 1, 2` writes each target, as a single
        // assignment does.
        ("left_assignment_list" | "destructured_left_assignment" | "rest_assignment", _) => {
            Some(Assigned::Plain)
        }
        _ => None,
    }
}

/// How an `identifier` reads, from the slot it fills in its parent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ident {
    /// An expression: a call of `self` unless a local of that name is in
    /// scope.
    Expr,
    /// A name that makes a local: an assignment target, a parameter, a
    /// `for` or `rescue` variable, a pattern binding.
    Binding,
    /// A method name being defined, called through `call`, or aliased.
    Name,
}

/// v1 `ruby_identifier_role`, verbatim.
fn role(parent: Option<Node>, field: Option<&str>) -> Ident {
    let Some(parent) = parent else {
        return Ident::Expr;
    };
    match (parent.kind(), field) {
        ("assignment" | "operator_assignment", Some("left"))
        | ("left_assignment_list" | "rest_assignment" | "destructured_left_assignment", _)
        | (
            "method_parameters"
            | "lambda_parameters"
            | "block_parameters"
            | "destructured_parameter",
            _,
        )
        | (
            "optional_parameter"
            | "keyword_parameter"
            | "splat_parameter"
            | "hash_splat_parameter"
            | "block_parameter",
            Some("name"),
        )
        | ("for", Some("pattern"))
        | ("exception_variable" | "array_pattern" | "find_pattern" | "as_pattern", _)
        | ("in_clause" | "match_pattern" | "test_pattern", Some("pattern"))
        | ("parenthesized_pattern", _)
        | ("keyword_pattern", Some("value")) => Ident::Binding,
        ("method" | "singleton_method", Some("name" | "object"))
        | ("call", Some("method"))
        | ("setter" | "alias" | "undef", _) => Ident::Name,
        _ => Ident::Expr,
    }
}

/// Whether a node opens a local-variable scope, and which kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    /// `def`, `class`, `module`, the file: the enclosing locals are hidden.
    Gate,
    /// A block or a lambda: the enclosing locals stay visible.
    Block,
}

fn scope_of(kind: &str) -> Option<Scope> {
    match kind {
        "program" | "method" | "singleton_method" | "class" | "module" | "singleton_class" => {
            Some(Scope::Gate)
        }
        "block" | "do_block" | "lambda" => Some(Scope::Block),
        _ => None,
    }
}

/// What a `def name` written in a frame's body defines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Defs {
    /// An instance method, `Klass#name`.
    Instance,
    /// A method of the class or module itself, `Klass.name`: the body of
    /// `class << self`.
    Singleton,
    /// Both: a module after a bare `module_function`, which Ruby applies
    /// to every following `def` until a `public`, `private` or
    /// `protected`, copying it as a private instance method (D15).
    ModuleFunction,
    /// The block of `instance_eval` in a class or module body: a `def` (and
    /// the `alias` and `undef` keywords) defines on the singleton class, a
    /// declaration sent to `self` on the class (Ruby 3.3).
    InstanceEval,
    /// A method of no owner the rules can name: a block that may run on
    /// another object, `class << x` for an `x` that is not the class.
    Foreign,
}

impl Defs {
    /// The separators of the methods a `def` defines here: none when there
    /// is no owner to name.
    pub(super) fn defined(self) -> &'static [char] {
        match self {
            Defs::Instance => &['#'],
            Defs::Singleton | Defs::InstanceEval => &['.'],
            Defs::ModuleFunction => &['.', '#'],
            Defs::Foreign => &[],
        }
    }

    /// The separator of the methods a declaration (`attr_reader`,
    /// `alias_method`) defines here: `module_function` does not touch
    /// them, Ruby 3.3 makes them instance methods only.
    pub(super) fn declared(self) -> Option<char> {
        match self {
            Defs::Instance | Defs::ModuleFunction | Defs::InstanceEval => Some('#'),
            Defs::Singleton => Some('.'),
            Defs::Foreign => None,
        }
    }

    /// The separator of the methods the `alias` and `undef` keywords touch
    /// here: the definee's, like a `def`'s, without `module_function`'s
    /// copy.
    pub(super) fn keyword(self) -> Option<char> {
        match self {
            Defs::InstanceEval => Some('.'),
            _ => self.declared(),
        }
    }

    /// What a `def` nested in a method written here defines when the method
    /// runs: a method of the class the outer one was defined in (its
    /// lexical class), never `module_function`'s copy (Ruby 3.3: a `def`
    /// in `def self.m` is an instance method, in a method of `class <<
    /// self` a method of the class itself).
    fn nested(self) -> Defs {
        match self {
            Defs::Instance | Defs::ModuleFunction => Defs::Instance,
            Defs::Singleton | Defs::InstanceEval => Defs::Singleton,
            Defs::Foreign => Defs::Foreign,
        }
    }
}

/// Where a `def` the rules give no owner may still land, and so which
/// owner's lookup must not take its absence for proof (R1, R9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Landing {
    /// On an object no lookup of a named owner reads: outside every owner,
    /// an association's extension module, a new anonymous class or module,
    /// a refinement, the singleton class of one object.
    Nowhere,
    /// Maybe on the enclosing owner itself or its singleton class: a block
    /// passed to a method the rules do not model, which may `class_eval`
    /// or `instance_eval` it there.
    Owner(String),
    /// On the class or module the constant `path` names in the nesting
    /// `scope` (`X.class_eval do`), or on its singleton class for
    /// `instance_eval` and `instance_exec` (`singleton`).
    Constant {
        path: String,
        scope: Vec<String>,
        singleton: bool,
    },
    /// On the singleton class of the class or module the constant `path`
    /// names in the nesting `scope`, which this file reads as `read`: the
    /// body of `class << X` (B2). A `def` there is `read.m`; anything else
    /// it declares is a method of `X` itself.
    SingletonOf {
        path: String,
        scope: std::rc::Rc<[String]>,
        read: String,
    },
    /// Maybe on the singleton class of the enclosing owner: the body of
    /// `class << x` for an expression `x` written in its body, which may be
    /// the class itself (`class << (c ? self : Other)`).
    SingletonOfOwner(String),
    /// On each class or module including (`prepended`: prepending) the
    /// concern: its `included do` or `prepended do` block (B6).
    Includers { concern: String, prepended: bool },
    /// On whatever object the method of `owner` on `side` (`#` or `.`)
    /// runs on, a class macro's caller: a block passed to a method on
    /// `self` there, which may `class_eval` it (B7).
    Macro { owner: String, side: char },
}

/// What a `def` or declaration the rules give no owner defines where it
/// lands ([`Landing`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Defined {
    /// `def m`, and the `alias` and `undef` keywords: on the definee.
    Def,
    /// `def self.m`: on the object `self` is.
    OnSelf,
    /// A declaration sent to `self`: methods on this side of it (`#` for
    /// `attr_reader`, `.` for `scope`).
    Sent(char),
    /// An ancestor or a computed name: anything, on either side.
    Any,
}

/// Where the `def`s of a block go, from the call it is passed to (D01).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockKind {
    /// Runs on the object around it, any number of times: plain iteration
    /// (Ruby 3.3 keeps the definee in `each`, `each_with_index`, `times`,
    /// `map`).
    Keeps,
    /// A concern's `included do` or `prepended do`, which Active Support
    /// evaluates on each class including (prepending) it: its `include`,
    /// `extend` and `prepend` are read as the includer's, and what it
    /// defines lands on the includer, which the rules give no owner (B6).
    Included { prepended: bool },
    /// Runs once on the object around it: `class_methods`.
    Once,
    /// `class_eval`, `class_exec`, `module_eval` or `module_exec` sent to
    /// the class or module whose body it is written in (no receiver, or
    /// `self`): runs once with that class as `self` and definee, so a `def`
    /// is its instance method (a method of the class itself in `class <<
    /// self`); a bare `module_function` around it does not reach in (Ruby
    /// 3.3).
    ClassEval,
    /// `instance_eval` or `instance_exec` sent to the class or module whose
    /// body it is written in: runs once with the class as `self` and its
    /// singleton class as definee, so a `def` is a method of the class
    /// itself, while `attr_reader` or `define_method`, sent to `self`,
    /// still make instance methods.
    InstanceEval,
    /// The block of `X = Struct.new do` and its family: `X` itself, a
    /// module for `Module.new`.
    Defines { module: bool },
    /// Rails' `with_options do`: run on an option merger, which forwards
    /// each call to the class (a callback, an association) but keeps a
    /// `def` for itself.
    Forwards,
    /// May run on another object: `class_eval` on another class,
    /// `instance_eval`, an association's extension, `refine`, a lambda,
    /// any DSL.
    Foreign,
}

/// The iterators whose block Ruby runs with the definee around it.
const ITERATORS: &[&str] = &["each", "each_with_index", "times", "map"];

/// The iterators of a core collection (`Hash`, `Array`, `Enumerable`)
/// whose block Ruby runs with the definee around it, read so only on a
/// literal or a constant receiver ([`Walker::collection`]).
const COLLECTION_ITERATORS: &[&str] = &[
    "each_value",
    "each_key",
    "each_pair",
    "each_with_object",
    "each_slice",
    "each_cons",
    "reverse_each",
    "flat_map",
    "filter_map",
    "select",
    "reject",
];

/// The methods that evaluate a block or a string with their receiver as
/// `self`, and a class or module receiver (or its singleton class) as
/// definee.
pub(super) const EVALUATORS: &[&str] = &[
    "class_eval",
    "class_exec",
    "module_eval",
    "module_exec",
    "instance_eval",
    "instance_exec",
];

/// The names of the named groups of a regexp's source that can be locals:
/// `(?<name>..)` and `(?'name'..)`, not a lookbehind, an escaped
/// parenthesis or one in a character class.
fn group_names(source: &str) -> Vec<String> {
    let chars: Vec<char> = source.chars().collect();
    let mut names = Vec::new();
    let mut class = 0u32;
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '[' => class += 1,
            ']' if class > 0 => class -= 1,
            '(' if class == 0 && chars.get(i + 1) == Some(&'?') => {
                let close = match chars.get(i + 2) {
                    Some('<') if !matches!(chars.get(i + 3), Some('=' | '!')) => Some('>'),
                    Some('\'') => Some('\''),
                    _ => None,
                };
                if let Some(close) = close
                    && let Some(length) = chars[i + 3..].iter().position(|c| *c == close)
                {
                    let name: String = chars[i + 3..i + 3 + length].iter().collect();
                    let local = name
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_lowercase() || c == '_')
                        && name.chars().all(|c| c.is_alphanumeric() || c == '_');
                    if local {
                        names.push(name);
                    }
                    i += 3 + length;
                }
            }
            _ => {}
        }
        i += 1;
    }
    names
}

#[cfg(test)]
pub(super) mod testing;
#[cfg(test)]
mod tests;
