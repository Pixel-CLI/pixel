// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Ruby constants: the lexical lookup that turns a constant as written into
//! a class or module, and the calls whose receiver is one (`Foo.m`,
//! `Foo.new.m`, `Job.perform_later`), carried from v1's `pixel-graph`
//! (`src/resolve/ruby.rs`, `constant`, `decide`, `constant_call`).
//!
//! v1 placed a site in its scopes by line spans and left the lines where a
//! class opens or closes unresolved. Extraction now records each site's
//! nesting from the AST (`Context::cref`), so a superclass expression, a
//! statement after `end` and one-line classes are looked up where Ruby
//! evaluates them.

use super::ancestors::Lookup;

/// The scope [`Index::scopes_after`] lists where a walk was cut short:
/// whatever comes past it may hold the constant, so nothing is proven.
pub(super) const CUT: &str = "#<cut>";
use super::{Decision, Index, Side, split_method};
use crate::ruby::{singleton_of, singleton_scope};

/// Most aliases of aliases followed to the method they copy (R11-03).
const MAX_ALIASES: usize = 8;

impl<'a> Index<'a> {
    /// `found`, the class or module `path` lands on, when the rules can
    /// prove it: not a constant whose value they do not read (D11), not a
    /// definition another file may move (D09), and no segment of the path
    /// or of the name an unread region may define (D12), since such a
    /// region may give a nearer scope that name.
    pub(super) fn settled(&self, path: &str, found: &'a str) -> Option<&'a str> {
        let unread = path
            .trim_start_matches("::")
            .split("::")
            .chain(found.split("::"))
            .any(|segment| self.facts.unread_constant(segment));
        (!self.consts.contains(found) && !self.facts.doubtful(found) && !unread).then_some(found)
    }

    /// The class or module `path` names in code: the nesting, then the
    /// ancestors of the innermost scope in their lookup order, then the top
    /// level, as Ruby looks constants up. An ancestor the graph cannot name
    /// (a gem's base class) is taken not to define the constant: a library
    /// rarely defines an application's class name.
    pub(super) fn constant_in_code(&self, cref: &[String], path: &str) -> Option<&'a str> {
        let found = self.found_in_code(cref, path)?;
        self.settled(path, found)
    }

    fn found_in_code(&self, cref: &[String], path: &str) -> Option<&'a str> {
        if let Some(found) = self.lexical(cref, path) {
            return found;
        }
        if let Some(innermost) = cref.first()
            && let Some(found) = self.through(self.scopes_after(innermost), path)
        {
            return found;
        }
        self.known(path)
    }

    /// The constant scopes Ruby searches after the nesting: the ancestors
    /// of the innermost scope, in lookup order. In `class << self`
    /// ([`singleton_scope`]) those of the singleton class: the modules it
    /// extends and the singleton classes of its superclasses, never the
    /// class's own ancestors (Ruby 3.3: `Retry` there is not
    /// `Base::Retry`, R2).
    pub(super) fn scopes_after(&self, innermost: &str) -> Vec<(String, bool)> {
        let mut scopes: (Vec<(String, bool)>, bool) = if let Some(owner) = singleton_of(innermost) {
            let Some(owner) = self.known(owner) else {
                return Vec::new();
            };
            let chain = self.chain(owner, Side::Singleton);
            let scopes = chain
                .owners_maybe()
                .filter(|(o, side, _)| (*o, *side) != (owner, Side::Singleton))
                .map(|(o, side, maybe)| match side {
                    Side::Singleton => (singleton_scope(o), maybe),
                    Side::Instance => (o.to_owned(), maybe),
                })
                .collect();
            (scopes, chain.is_cut())
        } else {
            let Some(scope) = self.known(innermost) else {
                return Vec::new();
            };
            let chain = self.chain(scope, Side::Instance);
            let scopes = chain
                .owners_maybe()
                .filter(|(o, _, _)| *o != scope)
                .map(|(o, _, maybe)| (o.to_owned(), maybe))
                .collect();
            (scopes, chain.is_cut())
        };
        // Past a walk cut short, any ancestor may hold the constant (R6-04).
        if scopes.1 {
            scopes.0.push((CUT.to_owned(), true));
        }
        scopes.0
    }

    /// The answer of the first of `ancestors` (in lookup order) that
    /// defines `path`'s first segment, which then owns the rest (`None`
    /// inside when it is absent there); `None` when none does. Each comes
    /// with whether it may not be there (B4, R5-07): then the answer is
    /// unproven, `None` inside.
    pub(super) fn through<S: AsRef<str>>(
        &self,
        ancestors: impl IntoIterator<Item = (S, bool)>,
        path: &str,
    ) -> Option<Option<&'a str>> {
        let first = path.split("::").next()?;
        for (ancestor, maybe) in ancestors {
            let ancestor = ancestor.as_ref();
            if ancestor == CUT {
                return Some(None);
            }
            if self.known(&format!("{ancestor}::{first}")).is_some() {
                return Some(
                    self.known(&format!("{ancestor}::{path}"))
                        .filter(|_| !maybe),
                );
            }
        }
        None
    }

    /// The nesting's answer: `Some(found)` when `path` is rooted or a scope
    /// of `cref` defines its first segment (`found` is `None` when the rest
    /// is absent there), `None` to look further. Ruby's lexical lookup, not
    /// prefixes of a qualified name, so `class A::B` sees `A::B` but not
    /// `A`; an absolute `::X` is looked up at the top level only. Only a
    /// class or module of the graph is found, so an expression (`user`,
    /// `Foo.bar`, `Struct.new(:x)`) names none.
    /// <https://docs.ruby-lang.org/en/master/syntax/modules_and_classes_rdoc.html#label-Constants>
    pub(super) fn lexical(&self, cref: &[String], path: &str) -> Option<Option<&'a str>> {
        if let Some(rooted) = path.strip_prefix("::") {
            return Some(self.known(rooted));
        }
        let first = path.split("::").next()?;
        cref.iter()
            .find(|scope| self.known(&format!("{scope}::{first}")).is_some())
            .map(|scope| self.known(&format!("{scope}::{path}")))
    }

    pub(super) fn known(&self, qualified: &str) -> Option<&'a str> {
        self.constants
            .get_key_value(qualified)
            .map(|(&name, _)| name)
    }

    fn is_class(&self, owner: &str) -> bool {
        self.classes.contains_key(owner)
    }

    /// A call whose receiver is not `self` (v1 `decide` and `constant_call`,
    /// K1 to K8 of `ruby-resolve.md`): only a constant names the receiver's
    /// class. Extraction keeps a receiver's arguments out only for
    /// `Const.new(..)` and `Const.set(..)`, so `Foo`, `Foo.new` and
    /// `Job.set` are the receivers these rules read; a local, an instance
    /// variable or any other chain (`Foo.bar.baz`) proves nothing.
    pub(super) fn constant_call(&self, cref: &[String], receiver: &str, name: &str) -> Decision {
        let (path, via) = receiver.rsplit_once('.').unwrap_or((receiver, ""));
        let owner = self.constant_in_code(cref, path)?;
        let decided = match via {
            // `Foo.new.m`: an instance of the class, unless a `new` on its
            // singleton chain, its own or a superclass's, makes the
            // factory's return value unknown (D07). Past a step the graph
            // cannot name, a gem's `new` may build a subclass (Active
            // Record's, from a `type` attribute): every subclass must
            // agree. The method is what `self.m` in an instance method of
            // Foo finds; v1 took Foo's own definition before its chain,
            // which a prepended module overriding it contradicts.
            "new" => {
                if !self.is_class(owner)
                    || self.lookup(owner, Side::Singleton, "new") != Lookup::NotFound
                {
                    return None;
                }
                let found = if self.chain(owner, Side::Singleton).is_known() {
                    self.chain_method(owner, Side::Instance, name)
                } else {
                    match self.dispatch(owner, Side::Instance, name) {
                        Lookup::Found(id) => Some(Some(id)),
                        Lookup::Abstain => Some(None),
                        Lookup::NotFound => None,
                    }
                };
                self.public(owner, Side::Instance, found)
            }
            // `Job.set(wait: 1).perform_later`: Active Job's configured job.
            "set" => {
                if !self.is_class(owner)
                    || !matches!(name, "perform_later" | "perform_now")
                    || self.method(owner, Side::Singleton, "set").is_some()
                    || self.method(owner, Side::Singleton, name).is_some()
                    || !self.own_first(owner)
                {
                    return None;
                }
                self.method(owner, Side::Instance, "perform")
            }
            "" => self.class_call(owner, name),
            _ => None,
        };
        decided.flatten()
    }

    /// `Foo.m`: Foo's own `Foo.m`, else an ancestor's on its singleton chain
    /// (an extended module, a concern's `ClassMethods`, a superclass); on a
    /// class, `new` is its own `initialize` (an inherited one is not
    /// followed), Active Job's `perform_*` its `perform`, and a mailer's
    /// action its instance method.
    pub(super) fn class_call(&self, owner: &'a str, name: &str) -> Option<Decision> {
        // Unless a module is prepended to its singleton class (D35).
        if self.own_first(owner)
            && let Some(explicit) = self.method(owner, Side::Singleton, name)
        {
            return self.public(owner, Side::Singleton, Some(explicit));
        }
        if let Some(inherited) = self.chain_method(owner, Side::Singleton, name) {
            return self.public(owner, Side::Singleton, Some(inherited));
        }
        if !self.is_class(owner) {
            return None;
        }
        if name == "new" {
            return self.method(owner, Side::Instance, "initialize");
        }
        if matches!(
            name,
            "perform_later" | "perform_now" | "perform_async" | "perform_in" | "perform_at"
        ) {
            return self.method(owner, Side::Instance, "perform");
        }
        if owner.ends_with("Mailer") {
            return self.method(owner, Side::Instance, name);
        }
        None
    }

    /// `found`, the method a call on `owner` on `side` reaches, unless it
    /// may be private or protected there
    /// ([`Fact::Hidden`](crate::ruby::Fact::Hidden)): Ruby raises
    /// `NoMethodError` for `Foo.m` and `Foo.new.m` there, whatever finds it
    /// (an `extend self`, a `define_method`, an `attr_reader *NAMES`, a
    /// Struct member made private), so the call proves nothing (R9-03).
    /// So does `private :m` for an inherited `m` in a step of the chain
    /// before the method's (`Sub = Class.new(Base) do private :m end`):
    /// it gives that step a private copy (R10-05).
    ///
    /// Nor is a method proven public when the lookup meets, up to the
    /// method's step, a class or module a constructor made, or the
    /// receiver's own list on its singleton chain (`extend self`): main
    /// holds neither, and the ways Ruby offers to change a method's
    /// visibility there (a computed or splatted `send`, `method(:private)`,
    /// a constant aliasing the class, a bare word under a condition) are
    /// more than the rules read, so such a call abstains as on main. The
    /// receiver's own method defined where a `class` or `module` keyword
    /// reopens it is main's too, and still found (R12-03).
    pub(super) fn public(
        &self,
        owner: &'a str,
        side: Side,
        found: Option<Decision>,
    ) -> Option<Decision> {
        let Some(Some(id)) = found else {
            return found;
        };
        let Some((hit, hit_side, name)) = split_method(&self.symbols[id].name) else {
            return found;
        };
        // Up to the method's own step, its last one where a step that may
        // not be there holds it first (B4): every step when none does.
        let steps: Vec<(&str, Side)> = self.chain(owner, side).steps().collect();
        let (mut hidden, mut before) = (false, None);
        for &(step, on) in &steps {
            if (step, on) == (hit, hit_side) {
                before = Some(hidden);
            }
            hidden = hidden || self.facts.private(step, on, name);
        }
        let upto = steps
            .iter()
            .rposition(|&step| step == (hit, hit_side))
            .map_or(steps.len(), |last| last + 1);
        let method = &self.symbols[id];
        let beyond = steps[..upto].iter().any(|&(step, on)| {
            (side == Side::Singleton && on == Side::Instance && step == owner)
                || self.facts.constructed(step).is_some_and(|bodies| {
                    // The receiver's own method, in a body its keyword
                    // opens, is one main holds.
                    upto > 1
                        || (step, on) != (hit, hit_side)
                        || !bodies.iter().any(|(file, first, last)| {
                            *file == method.file && (*first..=*last).contains(&method.line)
                        })
                })
        });
        if beyond
            || self.facts.private(hit, hit_side, name)
            || before.unwrap_or(hidden)
            || !self.alias_public(id, 0)
        {
            return Some(None);
        }
        found
    }

    /// Whether the method `id`, when an `alias` or `alias_method` made it,
    /// is public: Ruby copies the visibility its original has when the
    /// alias runs, which is proven only for an original defined above it
    /// in the same body, that nothing prepended by then may hide, and that
    /// no visibility change touches anywhere (R11-03).
    fn alias_public(&self, id: usize, depth: usize) -> bool {
        let Some(original) = self.aliases.get(&id) else {
            return true;
        };
        let alias = &self.symbols[id];
        let Some((owner, side, _)) = split_method(&alias.name) else {
            return false;
        };
        let separator = if side == Side::Instance { '#' } else { '.' };
        let qualified = format!("{owner}{separator}{original}");
        let above = self
            .by_name
            .get(qualified.as_str())
            .into_iter()
            .flatten()
            .copied()
            .filter(|&i| self.symbols[i].file == alias.file && self.symbols[i].line < alias.line)
            .max_by_key(|&i| self.symbols[i].line);
        depth < MAX_ALIASES
            && !self.facts.private(owner, side, original)
            && self
                .prepended(owner, side, original, (alias.file.as_str(), alias.line))
                .is_none()
            && above.is_some_and(|above| self.alias_public(above, depth + 1))
    }

    /// Whether lookup on `owner` itself starts at its own singleton
    /// methods: nothing is prepended to its singleton class (D35).
    fn own_first(&self, owner: &'a str) -> bool {
        self.chain(owner, Side::Singleton).first() == Some((owner, Side::Singleton))
    }

    /// What Ruby's lookup on `owner`'s chain finds for `name` on `side`: its
    /// one definition, `Some(None)` when the chain cannot prove one, `None`
    /// when no step defines it.
    fn chain_method(&self, owner: &'a str, side: Side, name: &str) -> Option<Decision> {
        match self.lookup(owner, side, name) {
            Lookup::Found(id) => Some(Some(id)),
            Lookup::Abstain => Some(None),
            Lookup::NotFound => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use mvp_model::SymbolKind::{Class, Method, Module};

    use super::super::fixture::Generation;

    #[test]
    fn c10_ruby_a_constant_receiver_resolves_through_the_lexical_nesting() {
        // targets.rb                      caller.rb
        //  1 module A                      1 module A
        //  2   class B                     2   class C
        //  3     def self.run; end         3     def invoke
        //  4   end                         4       B.run
        //  5 end                           5       ::B.run
        //  6 class B                       6       A::B.run
        //  7   def self.run; end
        //  8 end
        let mut g = Generation::default();
        g.def("targets.rb", Module, "A", 1, 5)
            .def("targets.rb", Class, "A::B", 2, 4)
            .def("targets.rb", Method, "A::B.run", 3, 3)
            .def("targets.rb", Class, "B", 6, 8)
            .def("targets.rb", Method, "B.run", 7, 7)
            .def("caller.rb", Module, "A", 1, 9)
            .def("caller.rb", Class, "A::C", 2, 8)
            .def("caller.rb", Method, "A::C#invoke", 3, 7);
        let nesting = ["A::C", "A"];
        let relative = g.call("caller.rb", "A::C#invoke", 4, "B", "run", &nesting);
        let rooted = g.call("caller.rb", "A::C#invoke", 5, "::B", "run", &nesting);
        let qualified = g.call("caller.rb", "A::C#invoke", 6, "A::B", "run", &nesting);
        let dst = g.resolve();
        assert_eq!(dst[relative].as_deref(), Some("targets.rb#A::B.run#method"));
        assert_eq!(dst[rooted].as_deref(), Some("targets.rb#B.run#method"));
        assert_eq!(
            dst[qualified].as_deref(),
            Some("targets.rb#A::B.run#method")
        );
    }

    #[test]
    fn c10_ruby_an_ancestors_constant_comes_before_the_top_level() {
        // base.rb               caller.rb
        // 1 class Base          1 class Job < Base
        // 2   class Retry       2   def run
        // 3     def self.go     3     Retry.go      # Base::Retry, not ::Retry
        // 4     end             4   end
        // 5   end               5 end
        // 6 end
        // 7 class Retry
        // 8   def self.go; end
        // 9 end
        let mut g = Generation::default();
        g.def("base.rb", Class, "Base", 1, 6)
            .def("base.rb", Class, "Base::Retry", 2, 5)
            .def("base.rb", Method, "Base::Retry.go", 3, 4)
            .def("base.rb", Class, "Retry", 7, 9)
            .def("base.rb", Method, "Retry.go", 8, 8)
            .def("caller.rb", Class, "Job", 1, 5)
            .def("caller.rb", Method, "Job#run", 2, 4);
        g.mixin("caller.rb", "Job", 1, "superclass", "Base", &[]);
        let call = g.call("caller.rb", "Job#run", 3, "Retry", "go", &["Job"]);
        let dst = g.resolve();
        assert_eq!(dst[call].as_deref(), Some("base.rb#Base::Retry.go#method"));
    }

    #[test]
    fn c10_ruby_class_a_colon_b_sees_its_own_name_but_not_a() {
        // targets.rb defines A::B.run and B.run; caller.rb:
        // 1 class A::D
        // 2   def invoke
        // 3     B.run          # Ruby's nesting is [A::D], not [A::D, A]
        let mut g = Generation::default();
        g.def("targets.rb", Module, "A", 1, 5)
            .def("targets.rb", Class, "A::B", 2, 4)
            .def("targets.rb", Method, "A::B.run", 3, 3)
            .def("targets.rb", Class, "B", 6, 8)
            .def("targets.rb", Method, "B.run", 7, 7)
            .def("caller.rb", Class, "A::D", 1, 5)
            .def("caller.rb", Method, "A::D#invoke", 2, 4);
        let call = g.call("caller.rb", "A::D#invoke", 3, "B", "run", &["A::D"]);
        assert_eq!(
            g.resolve()[call].as_deref(),
            Some("targets.rb#B.run#method")
        );
    }

    #[test]
    fn c10_ruby_a_nearer_first_segment_shadows_outer_scopes() {
        // remote.rb defines the classes B, A::B, A::C::B, B::Child and
        // A::B::Child; each but A::C::B has `def self.run`. caller.rb:
        //  1 module A
        //  2   class C
        //  3     def invoke; B.run; B::Child.run; end
        //  4   end
        //  5   def self.go; B::Child.run; end
        //  6 end
        //  7 B.run
        let mut g = Generation::default();
        for class in ["B", "A::B", "A::C::B", "B::Child", "A::B::Child"] {
            g.def("remote.rb", Class, class, 1, 1);
        }
        for method in ["B.run", "A::B.run", "B::Child.run", "A::B::Child.run"] {
            g.def("remote.rb", Method, method, 1, 1);
        }
        g.def("caller.rb", Module, "A", 1, 6)
            .def("caller.rb", Class, "A::C", 2, 4)
            .def("caller.rb", Method, "A::C#invoke", 3, 3)
            .def("caller.rb", Method, "A.go", 5, 5);
        let shadowed = g.call("caller.rb", "A::C#invoke", 3, "B", "run", &["A::C", "A"]);
        let no_fallback = g.call(
            "caller.rb",
            "A::C#invoke",
            3,
            "B::Child",
            "run",
            &["A::C", "A"],
        );
        let in_a = g.call("caller.rb", "A.go", 5, "B::Child", "run", &["A"]);
        let top = g.call("caller.rb", "caller.rb", 7, "B", "run", &[]);
        let dst = g.resolve();
        // A::C::B exists, so neither A::B.run nor A::B::Child.run is reached.
        assert_eq!(dst[shadowed], None);
        assert_eq!(dst[no_fallback], None);
        assert_eq!(
            dst[in_a].as_deref(),
            Some("remote.rb#A::B::Child.run#method")
        );
        assert_eq!(dst[top].as_deref(), Some("remote.rb#B.run#method"));
    }

    #[test]
    fn c10_ruby_a_superclass_and_a_statement_after_end_are_read_outside_the_class() {
        // targets.rb: `class C; class B; def self.run; end; end; end` and
        // `class B; def self.run; end; end`. Callers, each its own file:
        // header.rb `class C < B.run`, rooted.rb `class C < ::B.run`,
        // after.rb `class C\nend; B.run`, inline.rb `class C; def invoke;
        // B.run; end; end`. v1 left the first, third and fourth unresolved
        // (line spans cannot place them); decision 4 reads the nesting.
        let mut g = Generation::default();
        g.def("targets.rb", Class, "C", 1, 5)
            .def("targets.rb", Class, "C::B", 2, 4)
            .def("targets.rb", Method, "C::B.run", 3, 3)
            .def("targets.rb", Class, "B", 6, 8)
            .def("targets.rb", Method, "B.run", 7, 7)
            .def("header.rb", Class, "C", 1, 2)
            .def("rooted.rb", Class, "C", 1, 2)
            .def("after.rb", Class, "C", 1, 2)
            .def("inline.rb", Class, "C", 1, 1)
            .def("inline.rb", Method, "C#invoke", 1, 1);
        let header = g.call("header.rb", "C", 1, "B", "run", &[]);
        let rooted = g.call("rooted.rb", "C", 1, "::B", "run", &[]);
        let after = g.call("after.rb", "after.rb", 2, "B", "run", &[]);
        let inline = g.call("inline.rb", "C#invoke", 1, "B", "run", &["C"]);
        let dst = g.resolve();
        assert_eq!(dst[header].as_deref(), Some("targets.rb#B.run#method"));
        assert_eq!(dst[rooted].as_deref(), Some("targets.rb#B.run#method"));
        assert_eq!(dst[after].as_deref(), Some("targets.rb#B.run#method"));
        assert_eq!(dst[inline].as_deref(), Some("targets.rb#C::B.run#method"));
    }

    #[test]
    fn c10_ruby_scopes_sharing_lines_resolve_by_their_nesting() {
        // targets.rb defines A::B.run and B.run. Callers:
        // same.rb: `module A; class C\n def invoke\n B.run\n end\nend; end`
        // (A and A::C span the same lines); shared.rb: `class C\n def
        // invoke\n B.run; end; end; class Z\nend` (C and Z share a line).
        // v1 left both unresolved; decision 4 reads the nesting.
        let mut g = Generation::default();
        g.def("targets.rb", Module, "A", 1, 5)
            .def("targets.rb", Class, "A::B", 2, 4)
            .def("targets.rb", Method, "A::B.run", 3, 3)
            .def("targets.rb", Class, "B", 6, 8)
            .def("targets.rb", Method, "B.run", 7, 7)
            .def("same.rb", Module, "A", 1, 5)
            .def("same.rb", Class, "A::C", 1, 5)
            .def("same.rb", Method, "A::C#invoke", 2, 4)
            .def("shared.rb", Class, "C", 1, 3)
            .def("shared.rb", Method, "C#invoke", 2, 3)
            .def("shared.rb", Class, "Z", 3, 4);
        let same = g.call("same.rb", "A::C#invoke", 3, "B", "run", &["A::C", "A"]);
        let shared = g.call("shared.rb", "C#invoke", 3, "B", "run", &["C"]);
        let dst = g.resolve();
        assert_eq!(dst[same].as_deref(), Some("targets.rb#A::B.run#method"));
        assert_eq!(dst[shared].as_deref(), Some("targets.rb#B.run#method"));
    }

    #[test]
    fn c10_ruby_new_reaches_initialize_and_new_dot_m_an_instance_method() {
        // targets.rb: `class Widget; def initialize(v); end; def run; end;
        // end`. caller.rb, in `def invoke`: `Widget.new(1)` and
        // `Widget.new(1).run` (receiver `Widget.new`).
        let mut g = Generation::default();
        g.def("targets.rb", Class, "Widget", 1, 4)
            .def("targets.rb", Method, "Widget#initialize", 2, 2)
            .def("targets.rb", Method, "Widget#run", 3, 3)
            .function("caller.rb", "invoke", 1, 4);
        let new = g.call("caller.rb", "invoke", 2, "Widget", "new", &[]);
        let run = g.call("caller.rb", "invoke", 3, "Widget.new", "run", &[]);
        let dst = g.resolve();
        assert_eq!(
            dst[new].as_deref(),
            Some("targets.rb#Widget#initialize#method")
        );
        assert_eq!(dst[run].as_deref(), Some("targets.rb#Widget#run#method"));
    }

    #[test]
    fn c10_ruby_new_dot_m_follows_the_instance_chain_but_new_only_its_own_initialize() {
        // base.rb: `class Base; def initialize; end; def run; end; end`;
        // widget.rb: `class Widget < Base; end`; caller.rb: `Widget.new`
        // and `Widget.new.run` at the top level. v1 does not follow an
        // inherited `initialize` for `new`.
        let mut g = Generation::default();
        g.def("base.rb", Class, "Base", 1, 4)
            .def("base.rb", Method, "Base#initialize", 2, 2)
            .def("base.rb", Method, "Base#run", 3, 3)
            .def("widget.rb", Class, "Widget", 1, 2);
        g.mixin("widget.rb", "Widget", 1, "superclass", "Base", &[]);
        let new = g.call("caller.rb", "caller.rb", 1, "Widget", "new", &[]);
        let run = g.call("caller.rb", "caller.rb", 2, "Widget.new", "run", &[]);
        let dst = g.resolve();
        assert_eq!(dst[new], None);
        assert_eq!(dst[run].as_deref(), Some("base.rb#Base#run#method"));
    }

    #[test]
    fn c10_ruby_a_prepended_module_comes_before_the_class_for_new_dot_m() {
        // widget.rb: `class Widget; prepend Loud; def run; end; end`;
        // loud.rb: `module Loud; def run; end; end`; gadget.rb: `class
        // Gadget; prepend Auditing; def run; end; end` (a gem's module).
        // caller.rb: `Widget.new.run`, `Gadget.new.run`. v1 took each
        // class's own `run` first; Ruby looks a prepended module up before
        // the class.
        let mut g = Generation::default();
        g.def("loud.rb", Module, "Loud", 1, 3)
            .def("loud.rb", Method, "Loud#run", 2, 2)
            .def("widget.rb", Class, "Widget", 1, 4)
            .def("widget.rb", Method, "Widget#run", 3, 3)
            .def("gadget.rb", Class, "Gadget", 1, 4)
            .def("gadget.rb", Method, "Gadget#run", 3, 3);
        g.mixin("widget.rb", "Widget", 2, "prepend", "Loud", &["Widget"]);
        g.mixin("gadget.rb", "Gadget", 2, "prepend", "Auditing", &["Gadget"]);
        let widget = g.call("caller.rb", "caller.rb", 1, "Widget.new", "run", &[]);
        let gadget = g.call("caller.rb", "caller.rb", 2, "Gadget.new", "run", &[]);
        let dst = g.resolve();
        assert_eq!(dst[widget].as_deref(), Some("loud.rb#Loud#run#method"));
        assert_eq!(dst[gadget], None);
    }

    #[test]
    fn c10_ruby_a_custom_new_proves_no_instance_type() {
        // targets.rb: `class Widget; def initialize; end; def run; end;
        // end`; factory.rb: `class Widget; def self.new(v); Object.new;
        // end; end`. caller.rb: `Widget.new(1)` and `Widget.new(1).run`.
        let mut g = Generation::default();
        g.def("targets.rb", Class, "Widget", 1, 4)
            .def("targets.rb", Method, "Widget#initialize", 2, 2)
            .def("targets.rb", Method, "Widget#run", 3, 3)
            .def("factory.rb", Class, "Widget", 1, 3)
            .def("factory.rb", Method, "Widget.new", 2, 2);
        let new = g.call("caller.rb", "caller.rb", 1, "Widget", "new", &[]);
        let run = g.call("caller.rb", "caller.rb", 2, "Widget.new", "run", &[]);
        let dst = g.resolve();
        assert_eq!(dst[new].as_deref(), Some("factory.rb#Widget.new#method"));
        assert_eq!(dst[run], None);
    }

    #[test]
    fn c10_ruby_an_unknown_constant_or_an_instance_method_is_no_class_method() {
        // targets.rb: `module X; class Lost; end; end`, `module Y; class
        // Lost; end; end`, `class Known; def unique; end; end`.
        // caller.rb: `def invoke; Lost.unique; Missing.unique;
        // Known.unique; end`.
        let mut g = Generation::default();
        g.def("targets.rb", Class, "X::Lost", 2, 2)
            .def("targets.rb", Class, "Y::Lost", 5, 5)
            .def("targets.rb", Class, "Known", 7, 9)
            .def("targets.rb", Method, "Known#unique", 8, 8)
            .function("caller.rb", "invoke", 1, 5);
        let lost = g.call("caller.rb", "invoke", 2, "Lost", "unique", &[]);
        let missing = g.call("caller.rb", "invoke", 3, "Missing", "unique", &[]);
        let known = g.call("caller.rb", "invoke", 4, "Known", "unique", &[]);
        let dst = g.resolve();
        assert_eq!(dst[lost], None);
        assert_eq!(dst[missing], None);
        assert_eq!(dst[known], None);
    }

    #[test]
    fn c10_ruby_a_class_method_reopened_in_two_files_is_unresolved() {
        // targets.rb: `class Widget; def self.run; end; def initialize;
        // end; end`; reopened.rb: `class Widget; def self.run; end; end`.
        // caller.rb: `Widget.run` and `Widget.new`.
        let mut g = Generation::default();
        g.def("targets.rb", Class, "Widget", 1, 4)
            .def("targets.rb", Method, "Widget.run", 2, 2)
            .def("targets.rb", Method, "Widget#initialize", 3, 3)
            .def("reopened.rb", Class, "Widget", 1, 3)
            .def("reopened.rb", Method, "Widget.run", 2, 2);
        let run = g.call("caller.rb", "caller.rb", 1, "Widget", "run", &[]);
        let new = g.call("caller.rb", "caller.rb", 2, "Widget", "new", &[]);
        let dst = g.resolve();
        assert_eq!(dst[run], None);
        assert_eq!(
            dst[new].as_deref(),
            Some("targets.rb#Widget#initialize#method")
        );
    }

    #[test]
    fn c10_ruby_a_class_method_comes_from_an_extended_module_or_a_superclass() {
        // sortable.rb: `module Sortable; def sort_key; end; end`;
        // order.rb: `class Order; extend Sortable; end`; base.rb: `class
        // Base; def self.find; end; end`; post.rb: `class Post < Base;
        // end`. sales.rb: `class SalesReport; def build; Order.sort_key;
        // Post.find; end; end`.
        let mut g = Generation::default();
        g.def("sortable.rb", Module, "Sortable", 1, 3)
            .def("sortable.rb", Method, "Sortable#sort_key", 2, 2)
            .def("order.rb", Class, "Order", 1, 3)
            .def("base.rb", Class, "Base", 1, 3)
            .def("base.rb", Method, "Base.find", 2, 2)
            .def("post.rb", Class, "Post", 1, 2)
            .def("sales.rb", Class, "SalesReport", 1, 6)
            .def("sales.rb", Method, "SalesReport#build", 2, 5);
        g.mixin("order.rb", "Order", 2, "extend", "Sortable", &["Order"]);
        g.mixin("post.rb", "Post", 1, "superclass", "Base", &[]);
        let sort = g.call(
            "sales.rb",
            "SalesReport#build",
            3,
            "Order",
            "sort_key",
            &["SalesReport"],
        );
        let find = g.call(
            "sales.rb",
            "SalesReport#build",
            4,
            "Post",
            "find",
            &["SalesReport"],
        );
        let dst = g.resolve();
        assert_eq!(
            dst[sort].as_deref(),
            Some("sortable.rb#Sortable#sort_key#method")
        );
        assert_eq!(dst[find].as_deref(), Some("base.rb#Base.find#method"));
    }

    #[test]
    fn c10_ruby_extends_in_two_files_abstain_unless_the_class_defines_it() {
        // order.rb: `class Order; extend Finders; def self.find; end; end`;
        // search.rb reopens it: `class Order; extend Searchable; end`.
        // Finders and Searchable both define `find` and `search`, in an
        // order the graph cannot tell; a singleton method of Order comes
        // first whatever it is. caller.rb: `Order.find`, `Order.search`.
        let mut g = Generation::default();
        g.def("finders.rb", Module, "Finders", 1, 4)
            .def("finders.rb", Method, "Finders#find", 2, 2)
            .def("finders.rb", Method, "Finders#search", 3, 3)
            .def("searchable.rb", Module, "Searchable", 1, 4)
            .def("searchable.rb", Method, "Searchable#find", 2, 2)
            .def("searchable.rb", Method, "Searchable#search", 3, 3)
            .def("order.rb", Class, "Order", 1, 4)
            .def("order.rb", Method, "Order.find", 3, 3)
            .def("search.rb", Class, "Order", 1, 3);
        g.mixin("order.rb", "Order", 2, "extend", "Finders", &["Order"]);
        g.mixin("search.rb", "Order", 2, "extend", "Searchable", &["Order"]);
        let find = g.call("caller.rb", "caller.rb", 1, "Order", "find", &[]);
        let search = g.call("caller.rb", "caller.rb", 2, "Order", "search", &[]);
        let dst = g.resolve();
        assert_eq!(dst[find].as_deref(), Some("order.rb#Order.find#method"));
        assert_eq!(dst[search], None);
    }

    #[test]
    fn c10_ruby_active_job_dispatch_reaches_perform() {
        // targets.rb: `class Job; def perform; end; end`. caller.rb, in
        // `def invoke`: Job.perform_later, perform_now, perform_async,
        // perform_in, perform_at; `Job.set(wait: 1).perform_later` and
        // `.perform_now` (receiver `Job.set`), and the `set` call itself.
        let mut g = Generation::default();
        g.def("targets.rb", Class, "Job", 1, 3)
            .def("targets.rb", Method, "Job#perform", 2, 2)
            .function("caller.rb", "invoke", 1, 9);
        let direct: Vec<usize> = [
            "perform_later",
            "perform_now",
            "perform_async",
            "perform_in",
            "perform_at",
        ]
        .iter()
        .map(|name| g.call("caller.rb", "invoke", 2, "Job", name, &[]))
        .collect();
        let later = g.call("caller.rb", "invoke", 7, "Job.set", "perform_later", &[]);
        let now = g.call("caller.rb", "invoke", 8, "Job.set", "perform_now", &[]);
        let set = g.call("caller.rb", "invoke", 8, "Job", "set", &[]);
        let dst = g.resolve();
        for call in direct.into_iter().chain([later, now]) {
            assert_eq!(dst[call].as_deref(), Some("targets.rb#Job#perform#method"));
        }
        assert_eq!(dst[set], None);
    }

    #[test]
    fn c10_ruby_an_explicit_class_method_wins_over_dispatch() {
        // targets.rb: `class Job; def perform; end; end`; overrides.rb:
        // `class Job; def self.perform_later; end; end`. caller.rb:
        // `Job.perform_later`, `Job.perform_now`,
        // `Job.set(wait: 1).perform_later`.
        let mut g = Generation::default();
        g.def("targets.rb", Class, "Job", 1, 3)
            .def("targets.rb", Method, "Job#perform", 2, 2)
            .def("overrides.rb", Class, "Job", 1, 3)
            .def("overrides.rb", Method, "Job.perform_later", 2, 2);
        let later = g.call("caller.rb", "caller.rb", 1, "Job", "perform_later", &[]);
        let now = g.call("caller.rb", "caller.rb", 2, "Job", "perform_now", &[]);
        let configured = g.call("caller.rb", "caller.rb", 3, "Job.set", "perform_later", &[]);
        let dst = g.resolve();
        assert_eq!(
            dst[later].as_deref(),
            Some("overrides.rb#Job.perform_later#method")
        );
        assert_eq!(dst[now].as_deref(), Some("targets.rb#Job#perform#method"));
        assert_eq!(dst[configured], None);
    }

    #[test]
    fn c10_ruby_a_mailer_action_reaches_its_instance_method() {
        // targets.rb: `class NoticeMailer; def welcome; end; end` and
        // `class Notice; def welcome; end; end`. caller.rb:
        // `NoticeMailer.welcome(1).deliver_later` (two calls) and
        // `Notice.welcome`.
        let mut g = Generation::default();
        g.def("targets.rb", Class, "NoticeMailer", 1, 3)
            .def("targets.rb", Method, "NoticeMailer#welcome", 2, 2)
            .def("targets.rb", Class, "Notice", 4, 6)
            .def("targets.rb", Method, "Notice#welcome", 5, 5);
        let welcome = g.call("caller.rb", "caller.rb", 1, "NoticeMailer", "welcome", &[]);
        let deliver = g.call(
            "caller.rb",
            "caller.rb",
            1,
            "NoticeMailer.welcome(1)",
            "deliver_later",
            &[],
        );
        let not_a_mailer = g.call("caller.rb", "caller.rb", 2, "Notice", "welcome", &[]);
        let dst = g.resolve();
        assert_eq!(
            dst[welcome].as_deref(),
            Some("targets.rb#NoticeMailer#welcome#method")
        );
        assert_eq!(dst[deliver], None);
        assert_eq!(dst[not_a_mailer], None);
    }

    #[test]
    fn c10_ruby_modules_and_overridden_configurators_get_no_dispatch() {
        // targets.rb: `module Helpers` with initialize, run, perform;
        // `module HelpersMailer` with welcome; `class Job` with `def
        // self.set(options)` and `def perform`. caller.rb, in `def invoke`:
        // Helpers.new.run, Helpers.new, Helpers.perform_later,
        // HelpersMailer.welcome, Job.set(wait: 1).perform_now (two calls),
        // Helpers.set(wait: 1).perform_now.
        let mut g = Generation::default();
        g.def("targets.rb", Module, "Helpers", 1, 5)
            .def("targets.rb", Method, "Helpers#initialize", 2, 2)
            .def("targets.rb", Method, "Helpers#run", 3, 3)
            .def("targets.rb", Method, "Helpers#perform", 4, 4)
            .def("targets.rb", Module, "HelpersMailer", 6, 8)
            .def("targets.rb", Method, "HelpersMailer#welcome", 7, 7)
            .def("targets.rb", Class, "Job", 9, 12)
            .def("targets.rb", Method, "Job.set", 10, 10)
            .def("targets.rb", Method, "Job#perform", 11, 11)
            .function("caller.rb", "invoke", 1, 9);
        let refused = [
            g.call("caller.rb", "invoke", 2, "Helpers.new", "run", &[]),
            g.call("caller.rb", "invoke", 3, "Helpers", "new", &[]),
            g.call("caller.rb", "invoke", 4, "Helpers", "perform_later", &[]),
            g.call("caller.rb", "invoke", 5, "HelpersMailer", "welcome", &[]),
            g.call("caller.rb", "invoke", 6, "Job.set", "perform_now", &[]),
            g.call("caller.rb", "invoke", 7, "Helpers.set", "perform_now", &[]),
        ];
        let set = g.call("caller.rb", "invoke", 6, "Job", "set", &[]);
        let dst = g.resolve();
        for call in refused {
            assert_eq!(dst[call], None, "site {call}");
        }
        assert_eq!(dst[set].as_deref(), Some("targets.rb#Job.set#method"));
    }

    #[test]
    fn c10_ruby_a_chain_past_a_constant_proves_nothing() {
        // targets.rb: `class Job; def perform; end; def run; end; def
        // self.build; end; end`. caller.rb: `Job.build.perform_now`,
        // `Job.set(wait: 1).run`, `Job.set(wait: 1).perform_async`,
        // `Job::Missing.run`, `École.run` (no such class).
        let mut g = Generation::default();
        g.def("targets.rb", Class, "Job", 1, 5)
            .def("targets.rb", Method, "Job#perform", 2, 2)
            .def("targets.rb", Method, "Job#run", 3, 3)
            .def("targets.rb", Method, "Job.build", 4, 4);
        let receivers = [
            ("Job.build", "perform_now"),
            ("Job.set", "run"),
            ("Job.set", "perform_async"),
            ("Job::Missing", "run"),
            ("École", "run"),
        ];
        let calls: Vec<usize> = receivers
            .iter()
            .map(|(receiver, name)| g.call("caller.rb", "caller.rb", 1, receiver, name, &[]))
            .collect();
        let dst = g.resolve();
        for ((receiver, name), call) in receivers.iter().zip(calls) {
            assert_eq!(dst[call], None, "{receiver}.{name}");
        }
    }
}
