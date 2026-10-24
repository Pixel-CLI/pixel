// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Receivers typed by Rails conventions (ADR-0038): a call whose receiver
//! is a value path (`extract::values`) reaches what the value's class
//! finds, when the path proves the value is a record or a relation of a
//! class.
//!
//! A record of `C` is an instance of `C` or of a subclass, or `nil` where
//! its source may return `nil`; a call on it reaches what `self.m` in an
//! instance method of `C` reaches, the same for every subclass (`Foo.new.m`'s
//! rule). A relation of `C` is an Active Record relation or association
//! collection of the model `C`; a call on it reaches a definition only
//! when it is a `scope` of `C`: Rails raises when a scope is named as a
//! method of `ActiveRecord::Relation`, and the relation's method it
//! generates calls the scope on `C`. Any other name on a relation is a
//! method of the relation's own, which the graph does not hold, or a class
//! method reached through `method_missing` after them: nothing is proven.
//!
//! What makes a typed value, each abstaining where the code may change it:
//! `C.new` (today's rule); on a model `C` (its superclass chain reaches
//! `ActiveRecord::Base`), the finders and query methods Rails defines,
//! unless `C`'s singleton chain defines the name; on a record, `reload`
//! and an association reader, unless a method of the graph overrides it,
//! a subclass redeclares it, or it is declared where the rules do not
//! place it; on a relation, the same finders and query methods, unless
//! the association has an extension.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::rc::Rc;

use pixel_model::{EdgeKind, SymbolKind};

use super::ancestors::{Lookup, MAX_ANCESTOR_DEPTH, Op, Superclass};
use super::{Decision, Index, Side, split_method};
use crate::lang::World;
use crate::ruby::manifest::fixture_set;
use crate::ruby::routes::{camelize_path, singularize};
use crate::ruby::{
    ASSOCIATION_EXTENDED, ASSOCIATION_THROUGH, Context, Fact, MAYBE_NIL, VALUE_CONSTANT,
    VALUE_FACTORY, VALUE_FIXTURE, VALUE_LET,
};

use super::rspec::Found;

/// Most `let`s a value goes through, one naming the next: past it, a cycle
/// or a chain too long to read, nothing is typed.
const MAX_LETS: usize = 8;

mod names;

/// The associations class bodies declare and the fixture sets, read once
/// per index.
#[derive(Default)]
pub(super) struct Rails<'a> {
    /// Fixture accessor (`users`, `admin_users`) → the model its set
    /// names (`User`, `Admin::User`); `None` where the rules cannot tell
    /// (the file names its class, two sets share the accessor). Empty
    /// unless the program declares `fixtures` and never calls
    /// `set_fixture_class`.
    fixtures: HashMap<String, Option<String>>,
    /// (Class, association name) → its declarations there.
    declared: HashMap<(&'a str, String), Vec<Association>>,
    /// Association name → the classes declaring it.
    owners: HashMap<String, Vec<&'a str>>,
    /// Every method an association of the graph generates, declared in a
    /// class body or not.
    generated: HashSet<String>,
}

/// One association declared in a class body.
struct Association {
    /// The declaring class's symbol.
    src: usize,
    /// `belongs_to`, `has_one`, `has_many`, `has_and_belongs_to_many`.
    macro_: String,
    /// Its site's context, which the association rule resolves.
    context: Context,
    flags: String,
}

/// A value the rules can type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Value<'a> {
    Record(Record<'a>),
    Relation(Relation<'a>),
}

/// A relation of `model`, which has an extension when `extended`, or
/// `nil` when `nil`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Relation<'a> {
    model: &'a str,
    extended: bool,
    nil: bool,
}

impl<'a> Relation<'a> {
    fn of(model: &'a str, extended: bool) -> Relation<'a> {
        Relation {
            model,
            extended,
            nil: false,
        }
    }

    /// Whether a call of `name` on it reaches nothing the rules may name:
    /// an extension's method may come first, or `nil`'s may run.
    fn opaque(self, name: &str) -> bool {
        self.extended || (self.nil && names::NIL.binary_search(&name).is_ok())
    }
}

/// An instance of `class`, of exactly that class when `exact` (`C.new` of
/// a class whose chain the graph knows), else of it or a subclass; `nil`
/// when `nil`, an Array of them when `array` (`find(ids)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Record<'a> {
    class: &'a str,
    exact: bool,
    nil: bool,
    array: bool,
}

impl<'a> Record<'a> {
    fn of(class: &'a str) -> Record<'a> {
        Record {
            class,
            exact: false,
            nil: false,
            array: false,
        }
    }

    /// Whether a call of `name` may run on another object than the
    /// record: `nil`'s method, or an Array's.
    fn may_be_other(self, name: &str) -> bool {
        (self.nil && names::NIL.binary_search(&name).is_ok())
            || (self.array && names::ARRAY.binary_search(&name).is_ok())
    }
}

/// The Active Record query methods that return a relation of the same
/// model, with whether they need an argument (`where` alone is a
/// `WhereChain`). `merge` (an Array given an Array) and `extending` (it
/// adds methods) are not among them.
const QUERIES: &[(&str, bool)] = &[
    ("all", false),
    ("and", true),
    ("annotate", true),
    ("create_with", true),
    ("distinct", false),
    ("eager_load", true),
    ("excluding", true),
    ("from", true),
    ("group", true),
    ("having", true),
    ("in_order_of", true),
    ("includes", true),
    ("joins", true),
    ("left_joins", true),
    ("left_outer_joins", true),
    ("limit", true),
    ("lock", false),
    ("none", false),
    ("offset", true),
    ("or", true),
    ("order", true),
    ("preload", true),
    ("readonly", false),
    ("references", true),
    ("reorder", true),
    ("reverse_order", false),
    ("rewhere", true),
    ("select", true),
    ("strict_loading", false),
    ("unscope", true),
    ("where", true),
    ("without", true),
];

/// One call of a value path: its name and the shape of its arguments
/// (`""`, `(1)`, `(h)`, `(*)`, `extract::values`).
fn segment(text: &str) -> (&str, &str) {
    match text.find('(') {
        Some(at) => (&text[..at], &text[at..]),
        None => (text, ""),
    }
}

/// The methods an association `name` generates on its owner (Rails'
/// `Associations::Builder`): its reader and writer, `build_`, `create_`,
/// `create_..!`, `reload_`, `reset_`, the change queries, and for a
/// collection the ids reader and writer.
fn generated(name: &str, collection: bool) -> Vec<String> {
    let mut all = vec![
        name.to_owned(),
        format!("{name}="),
        format!("build_{name}"),
        format!("create_{name}"),
        format!("create_{name}!"),
        format!("reload_{name}"),
        format!("reset_{name}"),
        format!("{name}_changed?"),
        format!("{name}_previously_changed?"),
    ];
    if collection {
        let one = singularize(name);
        all.push(format!("{one}_ids"));
        all.push(format!("{one}_ids="));
    }
    all
}

impl<'a> Index<'a> {
    /// Reads the associations of the classes this index sees, what every
    /// association, wherever declared, generates, and the fixture sets.
    pub(super) fn read_rails(&mut self, world: &World<'a>, visible: &dyn Fn(&str) -> bool) {
        let mut rails = Rails::default();
        self.read_fixtures(&mut rails, world, visible);
        for edge in world
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Association)
        {
            let Some(&src) = self.by_uid.get(edge.src.as_str()) else {
                continue;
            };
            let context = Context::decode(&edge.context);
            let [name, flags, ..] = context.extra.as_slice() else {
                continue;
            };
            let (name, flags) = (name.clone(), flags.clone());
            let macro_ = context
                .qualifier
                .strip_prefix(crate::ruby::ASSOCIATION)
                .unwrap_or_default()
                .to_owned();
            let collection = matches!(macro_.as_str(), "has_many" | "has_and_belongs_to_many");
            rails.generated.extend(generated(&name, collection));
            let owner = &self.symbols[src];
            if owner.kind != SymbolKind::Class {
                continue;
            }
            let owner = owner.name.as_str();
            rails.owners.entry(name.clone()).or_default().push(owner);
            rails
                .declared
                .entry((owner, name))
                .or_default()
                .push(Association {
                    src,
                    macro_,
                    context,
                    flags,
                });
        }
        for name in self.facts.loose_associations() {
            rails.generated.extend(generated(name, true));
        }
        self.rails = rails;
    }

    /// The associations `class`'s body declares under `name`.
    fn associations_of(&self, class: &'a str, name: &str) -> Option<&[Association]> {
        self.rails
            .declared
            .get(&(class, name.to_owned()))
            .map(Vec::as_slice)
    }

    /// The classes declaring an association `name`.
    fn association_owners(&self, name: &str) -> impl Iterator<Item = &'a str> + '_ {
        self.rails.owners.get(name).into_iter().flatten().copied()
    }

    /// Whether an association of the graph generates the method `name`.
    fn association_generates(&self, name: &str) -> bool {
        self.rails.generated.contains(name)
    }

    /// Reads the fixture sets of Rails' default path (`test/fixtures/`),
    /// when the program declares `fixtures` (`fixtures :all`) and never
    /// renames a set's class (`set_fixture_class`).
    fn read_fixtures(
        &self,
        rails: &mut Rails<'a>,
        world: &World<'a>,
        visible: &dyn Fn(&str) -> bool,
    ) {
        // A call's target as written ends with its name: only those are
        // decoded.
        let called = |name: &str| {
            world.edges.iter().any(|edge| {
                edge.kind == EdgeKind::Call
                    && edge.target.ends_with(name)
                    && Context::decode(&edge.context).name == name
            })
        };
        if !called("fixtures") || called("set_fixture_class") {
            return;
        }
        for (path, facts) in world.files.iter().filter(|(path, _)| visible(path)) {
            let Some(set) = fixture_set(path) else {
                continue;
            };
            let named = Fact::decode_all(facts).contains(&Fact::FixtureClass);
            let model = (!named).then(|| camelize_path(&singularize(set)));
            rails
                .fixtures
                .entry(set.replace('/', "_"))
                .and_modify(|model| *model = None)
                .or_insert(model);
        }
        // Rails 8.1 reaches an accessor through `method_missing`: a method
        // of Rails', Minitest's or Capybara's test classes of that name
        // answers first.
        for (accessor, model) in &mut rails.fixtures {
            if names::TEST_DSL.binary_search(&accessor.as_str()).is_ok() {
                *model = None;
            }
        }
    }

    /// The record a fixture accessor returns in a method or block of the
    /// test case `test`: one of the model its set names, or of a subclass
    /// (a `type` column), unless a method of that name on the chain of
    /// `test` or of a subclass answers first (Rails 8.1 reaches the
    /// accessor through `method_missing`).
    fn fixture(&self, test: &'a str, accessor: &str) -> Option<Value<'a>> {
        let model = self.rails.fixtures.get(accessor)?.as_deref()?;
        let class = self.known(model)?;
        let answered = !self.classes.contains_key(test) || !self.absent(test, accessor);
        (!answered && self.classes.contains_key(class) && self.model(class))
            .then_some(Value::Record(Record::of(class)))
    }

    /// Whether no method of the graph answers `name` on an instance of
    /// `class` or of a subclass: what `dispatch` proves when it finds none,
    /// the objects behind a step the rules cannot name read as for a
    /// definition (`opaque_absent`), since here a name found nowhere is
    /// what lets Rails' own method run (a fixture accessor, a reader,
    /// `reload`).
    fn absent(&self, class: &'a str, name: &str) -> bool {
        let none = Lookup::NotFound;
        !self.maybe_core(class)
            && !self.spread.contains(class)
            && !self.facts.loose(name)
            && std::iter::once(self).chain(&self.loaders).all(|index| {
                !index.held_past_a_cut(class, Side::Instance)
                    && index.lookup(class, Side::Instance, name) == none
                    && index.agree(class, Side::Instance, name, none)
                    && index.opaque_absent(class, name)
            })
    }

    /// Whether no method of the graph answers `name` on a record: on its
    /// chain for exactly its class, else as [`Self::absent`] proves.
    fn record_absent(&self, record: Record<'a>, name: &str) -> bool {
        if record.exact {
            self.lookup(record.class, Side::Instance, name) == Lookup::NotFound
                && !self.facts.loose(name)
        } else {
            self.absent(record.class, name)
        }
    }

    /// The class of the graph `owner` declares as its superclass, its
    /// reopenings agreeing.
    fn parent_class(&self, owner: &'a str) -> Option<&'a str> {
        match self.superclass(owner) {
            Superclass::Known(parent) => Some(parent),
            _ => None,
        }
    }

    /// Whether `class` is `ancestor` or one of its subclasses, through
    /// superclasses the graph names.
    fn inherits(&self, class: &'a str, ancestor: &str) -> bool {
        let mut current = class;
        for _ in 0..MAX_ANCESTOR_DEPTH {
            if current == ancestor {
                return true;
            }
            match self.parent_class(current) {
                Some(parent) => current = parent,
                None => return false,
            }
        }
        false
    }

    /// Whether `class` is an Active Record model: its superclass chain, as
    /// the rules read it, reaches `ActiveRecord::Base`, written so where
    /// it leaves the graph.
    fn model(&self, class: &'a str) -> bool {
        let mut current = class;
        for _ in 0..MAX_ANCESTOR_DEPTH {
            if current == "ActiveRecord::Base" {
                return true;
            }
            match self.superclass(current) {
                Superclass::Known(parent) => current = parent,
                Superclass::Unknown => {
                    let mut written = self
                        .declarations(current, |kind| kind.op == Op::Superclass)
                        .map(|mixin| mixin.name.trim_start_matches("::"))
                        .peekable();
                    return written.peek().is_some()
                        && written.all(|name| name == "ActiveRecord::Base");
                }
                Superclass::None | Superclass::Opaque => return false,
            }
        }
        false
    }

    /// A call of `name` on the receiver whose value path is `path`,
    /// written in the nesting `cref`, in the definition `src`, in an
    /// example of the RSpec group at `group` if any.
    pub(super) fn value_call(
        &self,
        src: usize,
        group: Option<(u32, bool)>,
        cref: &[String],
        path: &str,
        name: &str,
    ) -> Decision {
        // `described_class.m`: the constant call `K.m` where the group's
        // argument `K` is written (ADR-0041).
        if path == format!("{VALUE_LET}described_class") {
            let (written, scope) = self.described(src, group?)?;
            return self.constant_call(scope, written, name);
        }
        match self.value_of(src, group, cref, path, 0)? {
            Value::Record(record) => self.record_call(record, name),
            Value::Relation(relation) if !relation.opaque(name) => {
                self.scope_call(relation.model, name)
            }
            Value::Relation(_) => None,
        }
    }

    /// The type of the value `path` names, when the rules can prove one;
    /// `depth` counts the `let`s read so far.
    fn value_of(
        &self,
        src: usize,
        group: Option<(u32, bool)>,
        cref: &[String],
        path: &str,
        depth: usize,
    ) -> Option<Value<'a>> {
        let mut value;
        let mut parts;
        if let Some(path) = path.strip_prefix(VALUE_LET) {
            parts = path.split('.');
            let root = parts.next()?;
            value = if root == "described_class" {
                let (written, scope) = self.described(src, group?)?;
                let class = self.constant_in_code(scope, written)?;
                if !self.classes.contains_key(class) {
                    return None;
                }
                self.on_class(class, segment(parts.next()?))?
            } else {
                self.group_value(src, group?, root, depth)?
            };
        } else if let Some(path) = path.strip_prefix(VALUE_FACTORY) {
            parts = path.split('.');
            value = self.factory_value(src, group?, parts.next()?)?;
        } else if let Some(path) = path.strip_prefix(VALUE_FIXTURE) {
            // Extraction roots a fixture accessor only where `self` is an
            // instance of the class around: a block of its body, or one of
            // its instance methods.
            let caller = &self.symbols[src];
            let test = match caller.kind {
                SymbolKind::Class => caller.name.as_str(),
                SymbolKind::Method => match split_method(&caller.name)? {
                    (owner, Side::Instance, _) => owner,
                    _ => return None,
                },
                _ => return None,
            };
            parts = path.split('.');
            value = self.fixture(test, parts.next()?)?;
        } else {
            parts = path.strip_prefix(VALUE_CONSTANT)?.split('.');
            let class = self.constant_in_code(cref, parts.next()?)?;
            if !self.classes.contains_key(class) {
                return None;
            }
            value = self.on_class(class, segment(parts.next()?))?;
        }
        for part in parts {
            value = self.step(value, segment(part))?;
        }
        Some(value)
    }

    /// The value of `name` in an example of the group at `group`: the
    /// last expression of the nearest `let` or `subject` of that name,
    /// read from the same example; the implicit subject, an instance of the
    /// described class or of a subclass, where no group defines one
    /// (ADR-0041).
    fn group_value(
        &self,
        src: usize,
        (group, open): (u32, bool),
        name: &str,
        depth: usize,
    ) -> Option<Value<'a>> {
        if depth >= MAX_LETS || self.facts.tainted(name) || self.facts.loose(name) {
            return None;
        }
        let file = self.symbols[src].file.as_str();
        // In a hook, a `let` or a `def`, the same from every group whose
        // examples it may run on.
        if open {
            let mut values = self
                .rspec
                .groups(file, group, true)?
                .into_iter()
                .map(|group| self.group_value(src, (group, false), name, depth));
            let first = values.next()??;
            return values.all(|value| value == Some(first)).then_some(first);
        }
        // What the caller's own file stubs may change any name the value
        // goes through (ADR-0040): the `let`'s, and those its value calls;
        // so may a module it mixes into an object the rules cannot name.
        let stubbed = self.own_stubs(file)?;
        if stubbed(name) {
            return None;
        }
        match self.groups_find(file, &[group], name) {
            Found::Member(member, scope) if !member.def => {
                let value = member.value.as_deref()?;
                if value
                    .split('.')
                    .skip(1)
                    .any(|part| stubbed(segment(part).0))
                {
                    return None;
                }
                self.value_of(src, Some((group, false)), scope, value, depth + 1)
            }
            Found::Nowhere if name == "subject" => {
                let (written, scope) = self.described(src, (group, false))?;
                let class = self.constant_in_code(scope, written)?;
                self.classes
                    .contains_key(class)
                    .then_some(Value::Record(Record::of(class)))
            }
            _ => None,
        }
    }

    /// What `create(:name)` or `build(:name)` makes in an example of the
    /// group at `group`: a record of the factory's class, where the
    /// configuration includes factory_bot's syntax methods and nothing
    /// else may answer `create` or `build` (ADR-0041).
    fn factory_value(
        &self,
        src: usize,
        (group, open): (u32, bool),
        name: &str,
    ) -> Option<Value<'a>> {
        let file = self.symbols[src].file.as_str();
        let groups = self.rspec.groups(file, group, open)?;
        let stubbed = self.own_stubs(file)?;
        let answered = ["create", "build"].iter().any(|method| {
            !matches!(self.groups_find(file, &groups, method), Found::Nowhere)
                || self.facts.tainted(method)
                || self.facts.loose(method)
                || stubbed(method)
        });
        if !self.rspec.factory_syntax() || answered {
            return None;
        }
        let (written, scope) = self.rspec.factory_class(name)?;
        let class = self.constant_in_code(&scope, &written)?;
        self.classes
            .contains_key(class)
            .then_some(Value::Record(Record::of(class)))
    }

    /// Whether what the test file `file` stubs may answer a name (ADR-0040):
    /// it defines it on an object the rules cannot name, or mixes into one
    /// a module that defines it; `None` when such a module may be any.
    fn own_stubs(&self, file: &'a str) -> Option<impl Fn(&str) -> bool + '_> {
        let modules = if self.facts.spreads_in(file) {
            let known = self.spread_by.borrow().get(file).cloned();
            match known {
                Some(modules) => modules?,
                None => {
                    let modules = super::stubs::spread_by(self, file).map(Rc::new);
                    self.spread_by.borrow_mut().insert(file, modules.clone());
                    modules?
                }
            }
        } else {
            Rc::default()
        };
        Some(move |name: &str| {
            self.facts.stubbed_in(file, name)
                || modules
                    .iter()
                    .any(|module| self.defines(module, Side::Instance, name))
        })
    }

    /// The constant `described_class` is in an example of the group at
    /// `group`, as written, with the nesting it is written in: RSpec's
    /// method, which no group defines (a module the configuration mixes in
    /// that defines it taints its name, R5-04; ADR-0041).
    fn described(&self, src: usize, (group, open): (u32, bool)) -> Option<(&str, &[String])> {
        const DESCRIBED: &str = "described_class";
        let file = self.symbols[src].file.as_str();
        let groups = self.rspec.groups(file, group, open)?;
        if !matches!(self.groups_find(file, &groups, DESCRIBED), Found::Nowhere)
            || self.facts.tainted(DESCRIBED)
            || self.facts.loose(DESCRIBED)
        {
            return None;
        }
        // The same constant, written in the same place, for every group.
        let mut described = groups
            .iter()
            .map(|&group| self.rspec.described(file, group));
        let first = described.next()??;
        described.all(|other| other == Some(first)).then_some(first)
    }

    /// What a call of the class `class` itself makes.
    fn on_class(&self, class: &'a str, (name, shape): (&str, &str)) -> Option<Value<'a>> {
        if self.lookup(class, Side::Singleton, name) != Lookup::NotFound {
            return None;
        }
        if name == "new" {
            // `Foo.new`: exactly `Foo` when its chain is known, else a
            // gem's `new` may build a subclass (today's rule).
            return Some(Value::Record(Record {
                exact: self.chain(class, Side::Singleton).is_known(),
                ..Record::of(class)
            }));
        }
        if !self.model(class) {
            return None;
        }
        self.model_value(class, name, shape)
    }

    /// What a finder or a query method of Active Record, sent to the model
    /// `model` or to one of its relations, returns.
    fn model_value(&self, model: &'a str, name: &str, shape: &str) -> Option<Value<'a>> {
        let record = |nil, array| {
            Some(Value::Record(Record {
                nil,
                array,
                ..Record::of(model)
            }))
        };
        let given = !shape.is_empty();
        match name {
            "find" if shape == "(1)" => record(false, true),
            "find_by" if given => record(true, false),
            "find_by!" | "find_sole_by" if given => record(false, false),
            "first" | "last" | "take" if !given => record(true, false),
            "first!" | "last!" | "take!" | "sole" if !given => record(false, false),
            "new" | "build" | "create" | "create!" if matches!(shape, "" | "(h)") => {
                record(false, false)
            }
            "find_or_create_by"
            | "find_or_create_by!"
            | "find_or_initialize_by"
            | "create_or_find_by"
            | "create_or_find_by!"
                if given =>
            {
                record(false, false)
            }
            _ => QUERIES
                .iter()
                .any(|&(query, needs)| query == name && (given || !needs))
                .then_some(Value::Relation(Relation::of(model, false))),
        }
    }

    /// What a call on a typed value makes; [`MAYBE_NIL`] makes the same
    /// value, which may also be `nil`.
    fn step(&self, value: Value<'a>, (name, shape): (&str, &str)) -> Option<Value<'a>> {
        match value {
            Value::Record(record) if name == MAYBE_NIL => Some(Value::Record(Record {
                nil: true,
                ..record
            })),
            Value::Relation(relation) if name == MAYBE_NIL => Some(Value::Relation(Relation {
                nil: true,
                ..relation
            })),
            Value::Record(record) if record.may_be_other(name) || !shape.is_empty() => None,
            Value::Record(record) if name == "reload" => {
                self.record_absent(record, name).then_some(value)
            }
            Value::Record(record) => self.reader(record, name),
            Value::Relation(relation) if !relation.opaque(name) => {
                self.model_value(relation.model, name, shape)
            }
            Value::Relation(_) => None,
        }
    }

    /// What lookup finds for `name` on a record: what `self.m` in an
    /// instance method of its class finds, for it and every subclass.
    fn record_lookup(&self, record: Record<'a>, name: &str) -> Lookup {
        if record.exact {
            self.lookup(record.class, Side::Instance, name)
        } else {
            self.dispatch(record.class, Side::Instance, name)
        }
    }

    /// A call of `name` on a record.
    fn record_call(&self, record: Record<'a>, name: &str) -> Decision {
        // Generated right after the model, an association's method comes
        // before its superclass's and before what it included first.
        if record.may_be_other(name) || self.association_generates(name) {
            return None;
        }
        let found = match self.record_lookup(record, name) {
            Lookup::Found(id) => Some(Some(id)),
            Lookup::Abstain => Some(None),
            Lookup::NotFound => None,
        };
        self.public(record.class, Side::Instance, found).flatten()
    }

    /// A call of `name` on a relation of `model`: the scope `model.name`
    /// reaches, if a scope it is.
    fn scope_call(&self, model: &'a str, name: &str) -> Decision {
        let found = self.class_call(model, name)??;
        let signature = self.symbols[found].signature.trim_start();
        let scope = signature
            .strip_prefix("scope")
            .is_some_and(|rest| rest.starts_with([' ', '(']));
        scope.then_some(found)
    }

    /// What the association reader `name` of a record returns: a record of
    /// the class a `belongs_to` or `has_one` names (or `nil`), a relation
    /// of the one a `has_many` or `has_and_belongs_to_many` names.
    fn reader(&self, record: Record<'a>, name: &str) -> Option<Value<'a>> {
        if self.facts.loose_association(name) || !self.record_absent(record, name) {
            return None;
        }
        // The nearest class of the record's that declares it, declared
        // there once; and no subclass redeclares it.
        let mut owner = record.class;
        let declared = loop {
            if let Some(found) = self.associations_of(owner, name) {
                break found;
            }
            owner = self.parent_class(owner)?;
        };
        let [association] = declared else {
            return None;
        };
        let redeclared = self
            .association_owners(name)
            .any(|other| other != owner && self.inherits(other, record.class));
        if redeclared || association.flags.contains(ASSOCIATION_THROUGH) {
            return None;
        }
        let target = self.association(association.src, &association.context)?;
        let class = self.symbols[target].name.as_str();
        if !self.classes.contains_key(class) {
            return None;
        }
        Some(match association.macro_.as_str() {
            "belongs_to" | "has_one" => Value::Record(Record {
                nil: true,
                ..Record::of(class)
            }),
            _ => Value::Relation(Relation::of(
                class,
                association.flags.contains(ASSOCIATION_EXTENDED),
            )),
        })
    }
}

#[cfg(test)]
mod audited_contract_tests {
    use super::*;
    use crate::ruby::resolve::{fixture::Generation, stubs::Tests};

    #[test]
    fn model_finders_should_preserve_argument_shape_nil_and_array_contracts() {
        let generation = Generation::default();
        let world = generation.world();
        let index = Index::build(&world, &|_| true, &Tests::Without);
        for (name, shape, expected) in [
            ("find", "(1)", Some((false, true))),
            ("find_by", "(h)", Some((true, false))),
            ("find_by", "", None),
            ("find_by!", "(h)", Some((false, false))),
            ("find_by!", "", None),
            ("first", "", Some((true, false))),
            ("first", "(1)", None),
            ("first!", "", Some((false, false))),
            ("first!", "(1)", None),
            ("create!", "", Some((false, false))),
            ("create!", "(h)", Some((false, false))),
            ("create!", "(1)", None),
            ("find_or_create_by", "(h)", Some((false, false))),
            ("find_or_create_by", "", None),
            ("where", "", None),
        ] {
            let expected = expected.map(|(nil, array)| {
                Value::Record(Record {
                    class: "Item",
                    exact: false,
                    nil,
                    array,
                })
            });
            assert_eq!(
                index.model_value("Item", name, shape),
                expected,
                "{name}{shape}"
            );
        }
        assert_eq!(
            index.model_value("Item", "where", "(h)"),
            Some(Value::Relation(Relation {
                model: "Item",
                extended: false,
                nil: false
            }))
        );
    }

    #[test]
    fn nullable_records_should_not_be_typed_as_relations_or_reloaded_with_arguments() {
        let generation = Generation::default();
        let world = generation.world();
        let index = Index::build(&world, &|_| true, &Tests::Without);
        let plain = Value::Record(Record {
            class: "Item",
            exact: true,
            nil: false,
            array: false,
        });
        let nullable = index.step(plain, (MAYBE_NIL, "")).unwrap();
        assert_eq!(
            nullable,
            Value::Record(Record {
                class: "Item",
                exact: true,
                nil: true,
                array: false
            })
        );
        assert_eq!(index.step(nullable, ("reload", "(1)")), None);
        assert_eq!(index.step(nullable, ("to_s", "")), None);
    }

    #[test]
    fn exact_records_should_abstain_when_their_own_chain_defines_the_reader_name() {
        use pixel_model::SymbolKind::{Class, Method};
        let mut generation = Generation::default();
        generation
            .def("item.rb", Class, "Item", 1, 5)
            .def("item.rb", Method, "Item#saved", 2, 2);
        let world = generation.world();
        let index = Index::build(&world, &|_| true, &Tests::Without);
        let record = Record {
            class: "Item",
            exact: true,
            nil: false,
            array: false,
        };
        assert!(index.record_absent(record, "missing"));
        assert!(!index.record_absent(record, "saved"));
        assert_eq!(index.step(Value::Record(record), ("reload", "(1)")), None);
        assert_eq!(
            index.step(Value::Record(record), ("reload", "")),
            Some(Value::Record(record))
        );
        let relation = Value::Relation(Relation {
            model: "Item",
            extended: false,
            nil: false,
        });
        assert_eq!(
            index.step(relation, (MAYBE_NIL, "")),
            Some(Value::Relation(Relation {
                model: "Item",
                extended: false,
                nil: true
            }))
        );
    }

    #[test]
    fn fixture_reading_should_require_a_real_fixtures_call_not_a_reference_or_suffix_match() {
        use pixel_model::SymbolKind::Class;
        for declaration in ["call", "reference", "suffix"] {
            let mut generation = Generation::default();
            generation
                .file("test/fixtures/users.yml")
                .file("test/check_test.rb")
                .def("user.rb", Class, "User", 1, 3);
            generation.mixin(
                "user.rb",
                "User",
                1,
                "superclass",
                "ActiveRecord::Base",
                &[],
            );
            match declaration {
                "call" => {
                    generation.call(
                        "test/check_test.rb",
                        "test/check_test.rb",
                        1,
                        "",
                        "fixtures",
                        &[],
                    );
                }
                "reference" => {
                    generation.reference(
                        "test/check_test.rb",
                        "test/check_test.rb",
                        1,
                        "",
                        "fixtures",
                        &[],
                    );
                }
                _ => {
                    generation.call(
                        "test/check_test.rb",
                        "test/check_test.rb",
                        1,
                        "",
                        "not_fixtures",
                        &[],
                    );
                }
            }
            let world = generation.world();
            let index = Index::build(&world, &|_| true, &Tests::Without);
            assert_eq!(
                index.rails.fixtures.get("users").and_then(|c| c.as_deref()),
                (declaration == "call").then_some("User"),
                "{declaration}"
            );
        }
    }
}
