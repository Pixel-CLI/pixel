// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Ruby lexical constants, the method owners their receivers identify, and
//! the ancestors a class or module declares (`ruby_mixins`), walked in Ruby's
//! method lookup order.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use super::Decision;
use crate::store::{GraphStore, StoreError};

#[derive(Debug)]
struct Scope {
    owner: String,
    start: u32,
    end: u32,
}

/// One declared ancestor, as `ruby_mixins` stores it.
#[derive(Debug)]
struct Mixin {
    file: i64,
    kind: String,
    target: Option<String>,
    line: u32,
}

#[derive(Debug, Default)]
pub(super) struct Index {
    constants: HashSet<String>,
    classes: HashSet<String>,
    methods: HashMap<String, Vec<i64>>,
    scopes: HashMap<i64, Vec<Scope>>,
    /// Declaring owner → its declared ancestors, in file then line order.
    mixins: HashMap<String, Vec<Mixin>>,
    /// Lookup chains already walked, by owner and side, with whether their
    /// order is uncertain: a build resolves many calls from one owner.
    chains: Mutex<HashMap<(String, char), Arc<Chain>>>,
}

/// What Ruby's method lookup from an owner finds for a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Lookup {
    /// The owner itself defines it before any ancestor does: the caller's
    /// own-method rules decide (reopened classes, same-file duplicates).
    Own,
    /// The one definition an ancestor provides, first in lookup order.
    Found(i64),
    /// A definition exists past an ancestor the graph cannot name (a
    /// dynamic `include`, an external module), or several ancestors define
    /// it in an order the graph cannot prove: no target.
    Abstain,
    /// No owner on the chain defines it.
    NotFound,
}

/// One step of a lookup chain: the methods of `owner` written with
/// `separator` (`#` instance, `.` singleton), or an ancestor the graph
/// cannot name.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Known(String, char),
    Unknown,
}

/// A walked lookup chain, and whether its order is uncertain (an owner's
/// declarations spread over several files).
type Chain = (Vec<Step>, bool);

/// Longest ancestor chain walked; past it the rest is unknown.
const MAX_ANCESTOR_DEPTH: usize = 32;

/// The literal `extend ActiveSupport::Concern` that makes a module a concern.
fn is_concern_marker(target: &str) -> bool {
    target.trim_start_matches("::") == "ActiveSupport::Concern"
}

impl Index {
    pub(super) fn build(store: &GraphStore) -> Result<Self, StoreError> {
        let mut index = Self::default();
        let mut query = store.conn().prepare(
            "SELECT s.id, s.file_id, s.qualified, s.kind, s.start_line, s.end_line
             FROM symbols s JOIN files f ON f.id=s.file_id WHERE f.lang='ruby'",
        )?;
        let rows = query.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, u32>(4)?,
                r.get::<_, u32>(5)?,
            ))
        })?;
        for row in rows {
            let (id, file, qualified, kind, start, end) = row?;
            match kind.as_str() {
                "class" | "module" => {
                    // Each lookup owns its key; the scope retains the qualified name.
                    index.constants.insert(qualified.clone());
                    if kind == "class" {
                        index.classes.insert(qualified.clone());
                    }
                    index.scopes.entry(file).or_default().push(Scope {
                        owner: qualified,
                        start,
                        end,
                    });
                }
                "method" => {
                    index.methods.entry(qualified).or_default().push(id);
                }
                _ => {}
            }
        }
        for scopes in index.scopes.values_mut() {
            scopes.sort_by_key(|scope| (Reverse(scope.start), scope.end));
        }
        let mut query = store.conn().prepare(
            "SELECT file_id, owner, kind, target, site_line FROM ruby_mixins
              ORDER BY file_id, site_line, id",
        )?;
        let rows = query.query_map([], |r| {
            Ok((
                r.get::<_, String>(1)?,
                Mixin {
                    file: r.get(0)?,
                    kind: r.get(2)?,
                    target: r.get(3)?,
                    line: r.get(4)?,
                },
            ))
        })?;
        for row in rows {
            let (owner, mixin) = row?;
            index.mixins.entry(owner).or_default().push(mixin);
        }
        Ok(index)
    }

    /// The constant a declared ancestor names, looked up where Ruby evaluates
    /// it: a superclass outside the class it opens, a module argument in the
    /// body that declares it. `None` for a dynamic argument or a constant the
    /// graph does not define (a gem's module).
    fn ancestor(&self, mixin: &Mixin) -> Option<String> {
        let target = mixin.target.as_deref()?;
        if mixin.kind == "superclass" {
            self.constant_outside(mixin.file, target, mixin.line)
        } else {
            self.constant(mixin.file, target, Some(mixin.line))
        }
    }

    /// True iff `module` extends `ActiveSupport::Concern` literally.
    fn is_concern(&self, module: &str) -> bool {
        self.mixins.get(module).is_some_and(|mixins| {
            mixins
                .iter()
                .any(|m| m.kind == "extend" && m.target.as_deref().is_some_and(is_concern_marker))
        })
    }

    /// The ancestors `owner` declares of `kinds`, in declaration order,
    /// each named or unknown; the `bool` is true when they come from more
    /// than one file, whose load order the graph cannot tell.
    fn declared(&self, owner: &str, kinds: &[&str]) -> (Vec<Option<String>>, bool) {
        let Some(mixins) = self.mixins.get(owner) else {
            return (Vec::new(), false);
        };
        let chosen: Vec<&Mixin> = mixins
            .iter()
            .filter(|m| kinds.contains(&m.kind.as_str()))
            .collect();
        let files: HashSet<i64> = chosen.iter().map(|m| m.file).collect();
        let named = chosen
            .iter()
            .filter(|m| !(m.kind == "extend" && m.target.as_deref().is_some_and(is_concern_marker)))
            .map(|m| self.ancestor(m))
            .collect();
        (named, files.len() > 1)
    }

    /// The modules an `include` of `module` adds to the includer, in
    /// declaration order: the module, then what its `included do` block
    /// includes (included later, so looked up first).
    fn includes_of(&self, owner: &str, uncertain: &mut bool) -> Vec<Option<String>> {
        let (direct, multi) = self.declared(owner, &["include"]);
        *uncertain |= multi;
        let mut out = Vec::new();
        for module in direct {
            out.push(module.clone());
            if let Some(module) = module
                && self.is_concern(&module)
            {
                let (deferred, multi) = self.declared(&module, &["included:include"]);
                *uncertain |= multi;
                out.extend(deferred);
            }
        }
        out
    }

    /// Ruby's instance-method lookup chain from `owner`: prepended modules
    /// (last prepended first), the owner, included modules (last included
    /// first), each with its own chain, then the superclass's chain.
    fn instance_chain(
        &self,
        owner: &str,
        depth: usize,
        seen: &mut HashSet<String>,
        uncertain: &mut bool,
        out: &mut Vec<Step>,
    ) {
        // An owner already walked, or one past the depth cap, has no
        // superclass to follow either.
        if !self.own_chain(owner, depth, seen, uncertain, out) {
            return;
        }
        let (superclasses, _) = self.declared(owner, &["superclass"]);
        let distinct: HashSet<&Option<String>> = superclasses.iter().collect();
        match distinct.into_iter().collect::<Vec<_>>()[..] {
            [] => {}
            // `class A < B` with `class B < A` is a cycle Ruby itself
            // refuses: whatever follows is unknown.
            [Some(parent)] if seen.contains(parent) => out.push(Step::Unknown),
            [Some(parent)] => self.instance_chain(parent, depth + 1, seen, uncertain, out),
            // A dynamic or external superclass, or two reopenings that
            // disagree: whatever comes next is unknown.
            _ => out.push(Step::Unknown),
        }
    }

    /// [`Self::instance_chain`] up to, not including, the superclass. False
    /// when it walked nothing: the owner is past the depth cap (an unknown
    /// step is pushed) or already on the chain.
    fn own_chain(
        &self,
        owner: &str,
        depth: usize,
        seen: &mut HashSet<String>,
        uncertain: &mut bool,
        out: &mut Vec<Step>,
    ) -> bool {
        if depth > MAX_ANCESTOR_DEPTH {
            out.push(Step::Unknown);
            return false;
        }
        // A module already on the chain is skipped, as Ruby skips including
        // it twice; a cycle stops there too.
        if !seen.insert(owner.to_string()) {
            return false;
        }
        let (prepends, multi) = self.declared(owner, &["prepend"]);
        *uncertain |= multi;
        for module in prepends.into_iter().rev() {
            self.module_chain(module, depth, seen, uncertain, out);
        }
        out.push(Step::Known(owner.to_string(), '#'));
        for module in self.includes_of(owner, uncertain).into_iter().rev() {
            self.module_chain(module, depth, seen, uncertain, out);
        }
        true
    }

    fn module_chain(
        &self,
        module: Option<String>,
        depth: usize,
        seen: &mut HashSet<String>,
        uncertain: &mut bool,
        out: &mut Vec<Step>,
    ) {
        match module {
            Some(module) => self.instance_chain(&module, depth + 1, seen, uncertain, out),
            None => out.push(Step::Unknown),
        }
    }

    /// Ruby's lookup chain for a method of the class or module `owner`
    /// itself (`Owner.name`): its singleton methods, the instance methods of
    /// the modules it extends (last first), the `ClassMethods` of every
    /// concern on its instance chain, then its superclass's chain.
    fn singleton_chain(
        &self,
        owner: &str,
        depth: usize,
        seen: &mut HashSet<String>,
        uncertain: &mut bool,
        out: &mut Vec<Step>,
    ) {
        if depth > MAX_ANCESTOR_DEPTH {
            out.push(Step::Unknown);
            return;
        }
        if !seen.insert(format!("{owner}.")) {
            return;
        }
        out.push(Step::Known(owner.to_string(), '.'));
        let (mut extends, multi) = self.declared(owner, &["extend"]);
        *uncertain |= multi;
        // What a concern's `included do` extends lands on the includer; the
        // superclass's own concerns come with its singleton chain below.
        let mut instance = Vec::new();
        self.own_chain(owner, depth, &mut HashSet::new(), uncertain, &mut instance);
        let concerns: Vec<String> = instance
            .iter()
            .filter_map(|step| match step {
                Step::Known(module, _) if module != owner && self.is_concern(module) => {
                    Some(module.clone())
                }
                _ => None,
            })
            .collect();
        for concern in &concerns {
            let (deferred, multi) = self.declared(concern, &["included:extend"]);
            *uncertain |= multi;
            extends.extend(deferred);
        }
        let mut modules = HashSet::new();
        for module in extends.into_iter().rev() {
            self.module_chain(module, depth, &mut modules, uncertain, out);
        }
        for concern in concerns {
            let class_methods = format!("{concern}::ClassMethods");
            if self.constants.contains(&class_methods) {
                self.instance_chain(&class_methods, depth + 1, &mut modules, uncertain, out);
            }
        }
        let (superclasses, _) = self.declared(owner, &["superclass"]);
        let distinct: HashSet<&Option<String>> = superclasses.iter().collect();
        match distinct.into_iter().collect::<Vec<_>>()[..] {
            [] => {}
            [Some(parent)] => self.singleton_chain(parent, depth + 1, seen, uncertain, out),
            _ => out.push(Step::Unknown),
        }
    }

    /// The lookup chain from `owner` on the `separator` side, walked once.
    fn chain(&self, owner: &str, separator: char) -> Arc<Chain> {
        let key = (owner.to_string(), separator);
        if let Some(chain) = self.chains.lock().ok().and_then(|c| c.get(&key).cloned()) {
            return chain;
        }
        let mut chain = Vec::new();
        let mut uncertain = false;
        let mut seen = HashSet::new();
        if separator == '.' {
            self.singleton_chain(owner, 0, &mut seen, &mut uncertain, &mut chain);
        } else {
            self.instance_chain(owner, 0, &mut seen, &mut uncertain, &mut chain);
        }
        let chain = Arc::new((chain, uncertain));
        if let Ok(mut chains) = self.chains.lock() {
            chains.insert(key, Arc::clone(&chain));
        }
        chain
    }

    /// Look `name` up from a method of `owner` written with `separator`
    /// (`#` instance, `.` the class or module itself), in Ruby's order.
    pub(super) fn lookup(&self, owner: &str, separator: char, name: &str) -> Lookup {
        if self.mixins.is_empty() {
            // No declared ancestor anywhere: the chain is the owner alone.
            return if self
                .methods
                .contains_key(&format!("{owner}{separator}{name}"))
            {
                Lookup::Own
            } else {
                Lookup::NotFound
            };
        }
        let chain = self.chain(owner, separator);
        let (chain, uncertain) = (&chain.0, chain.1);
        let mut defining = HashSet::new();
        let hits: Vec<(usize, &str, char)> = chain
            .iter()
            .enumerate()
            .filter_map(|(i, step)| match step {
                Step::Known(o, s)
                    if self.methods.contains_key(&format!("{o}{s}{name}"))
                        && defining.insert((o.as_str(), *s)) =>
                {
                    Some((i, o.as_str(), *s))
                }
                _ => None,
            })
            .collect();
        let Some(&(first, hit_owner, hit_separator)) = hits.first() else {
            return Lookup::NotFound;
        };
        if chain[..first].contains(&Step::Unknown) || (uncertain && hits.len() > 1) {
            return Lookup::Abstain;
        }
        if hit_owner == owner && hit_separator == separator {
            return Lookup::Own;
        }
        match self.method(hit_owner, hit_separator, name) {
            Some(Decision::Exact(id)) => Lookup::Found(id),
            _ => Lookup::Abstain,
        }
    }

    /// Ruby uses actual lexical nesting, not prefixes of a qualified class name.
    /// `class A::B` sees A::B but not A; `module A; class B` sees both.
    /// A nearer first segment shadows outer paths even if its suffix is absent.
    /// <https://docs.ruby-lang.org/en/master/syntax/modules_and_classes_rdoc.html#label-Constants>
    fn constant(&self, file: i64, path: &str, line: Option<u32>) -> Option<String> {
        if let Some(rooted) = path.strip_prefix("::") {
            return self.constants.contains(rooted).then(|| rooted.to_string());
        }
        let line = line?;
        let visible: Vec<&Scope> = self
            .scopes
            .get(&file)
            .into_iter()
            .flatten()
            .filter(|s| s.start <= line && line <= s.end)
            .collect();
        // A superclass expression runs outside the new class, as does a
        // statement after `end` on the same line. Line-only spans cannot
        // place these sites, or prove the nesting of identical spans.
        if visible.iter().any(|s| s.start == line || s.end == line) {
            return None;
        }
        self.constant_in(&visible, path)
    }

    /// [`Self::constant`] for a superclass written on the line `line` that
    /// opens its class: Ruby evaluates it in the scopes around that class.
    /// The class must be the only scope opening on that line (`module A;
    /// class B < C` places `C` in `A`, which the line cannot tell), and no
    /// enclosing scope may close there.
    fn constant_outside(&self, file: i64, path: &str, line: u32) -> Option<String> {
        if let Some(rooted) = path.strip_prefix("::") {
            return self.constants.contains(rooted).then(|| rooted.to_string());
        }
        let visible: Vec<&Scope> = self
            .scopes
            .get(&file)
            .into_iter()
            .flatten()
            .filter(|s| s.start <= line && line <= s.end)
            .collect();
        let (opening, outer): (Vec<&Scope>, Vec<&Scope>) =
            visible.into_iter().partition(|s| s.start == line);
        if opening.len() != 1 || outer.iter().any(|s| s.end == line) {
            return None;
        }
        self.constant_in(&outer, path)
    }

    /// Resolve a relative `path` from the `visible` scopes, innermost first:
    /// the nearest scope defining its first segment owns it, else the top
    /// level.
    fn constant_in(&self, visible: &[&Scope], path: &str) -> Option<String> {
        let first = path.split("::").next()?;
        if visible
            .windows(2)
            .any(|pair| (pair[0].start, pair[0].end) == (pair[1].start, pair[1].end))
        {
            return None;
        }
        for scope in visible {
            if self
                .constants
                .contains(&format!("{}::{first}", scope.owner))
            {
                let qualified = format!("{}::{path}", scope.owner);
                return self.constants.contains(&qualified).then_some(qualified);
            }
        }
        self.constants.contains(path).then(|| path.to_string())
    }

    /// None means absent; Unresolved means competing definitions forbid fallback.
    fn method(&self, owner: &str, separator: char, name: &str) -> Option<Decision> {
        let candidates = self.methods.get(&format!("{owner}{separator}{name}"))?;
        Some(match candidates.as_slice() {
            [id] => Decision::Exact(*id),
            _ => Decision::Unresolved,
        })
    }

    pub(super) fn decide(
        &self,
        file: i64,
        receiver: &str,
        name: &str,
        line: Option<u32>,
    ) -> Option<Decision> {
        let receiver = receiver.trim();
        if !receiver.starts_with("::") && !receiver.chars().next().is_some_and(char::is_uppercase) {
            return None;
        }
        Some(
            self.constant_call(file, receiver, name, line)
                .unwrap_or(Decision::Unresolved),
        )
    }

    fn constant_call(
        &self,
        file: i64,
        receiver: &str,
        name: &str,
        line: Option<u32>,
    ) -> Option<Decision> {
        // Extraction strips arguments only from AST-confirmed constant.new/set calls.
        let (path, via) = receiver.rsplit_once('.').unwrap_or((receiver, ""));
        if !constant_path(path) {
            return None;
        }
        let owner = self.constant(file, path, line)?;
        match via {
            "new" => {
                if !self.classes.contains(&owner) || self.method(&owner, '.', "new").is_some() {
                    return None;
                }
                self.method(&owner, '#', name)
                    .or_else(|| self.inherited(&owner, '#', name))
            }
            "set" => {
                if !self.classes.contains(&owner)
                    || !matches!(name, "perform_later" | "perform_now")
                    || self.method(&owner, '.', "set").is_some()
                    || self.method(&owner, '.', name).is_some()
                {
                    return None;
                }
                self.method(&owner, '#', "perform").map(probable)
            }
            "" => {
                if let Some(explicit) = self.method(&owner, '.', name) {
                    return Some(explicit);
                }
                if let Some(inherited) = self.inherited(&owner, '.', name) {
                    return Some(inherited);
                }
                if !self.classes.contains(&owner) {
                    return None;
                }
                if name == "new" {
                    return self.method(&owner, '#', "initialize");
                }
                if matches!(
                    name,
                    "perform_later" | "perform_now" | "perform_async" | "perform_in" | "perform_at"
                ) {
                    return self.method(&owner, '#', "perform").map(probable);
                }
                if owner.ends_with("Mailer") {
                    return self.method(&owner, '#', name).map(probable);
                }
                None
            }
            _ => None,
        }
    }
}

impl Index {
    /// The decision an ancestor of `owner` gives `name` when the owner does
    /// not define it itself: `Probable` for the one definition first in
    /// lookup order, `Unresolved` when the chain cannot prove it, `None` when
    /// no ancestor defines it.
    fn inherited(&self, owner: &str, separator: char, name: &str) -> Option<Decision> {
        match self.lookup(owner, separator, name) {
            Lookup::Found(id) => Some(Decision::Probable(id)),
            Lookup::Abstain => Some(Decision::Unresolved),
            Lookup::Own | Lookup::NotFound => None,
        }
    }
}

fn probable(decision: Decision) -> Decision {
    match decision {
        Decision::Exact(id) => Decision::Probable(id),
        other => other,
    }
}

fn constant_path(path: &str) -> bool {
    path.strip_prefix("::")
        .unwrap_or(path)
        .split("::")
        .all(|part| {
            part.chars().next().is_some_and(char::is_uppercase)
                && part.chars().all(|c| c.is_alphanumeric() || c == '_')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SymbolKind;

    #[test]
    fn constant_lookup_should_honor_scope_boundaries_and_shadow_the_first_segment() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let local = store.replace_file("local.rb", "local", "ruby").unwrap();
        let remote = store.replace_file("remote.rb", "remote", "ruby").unwrap();
        store
            .insert_symbol(local, "outer", "A", "A", SymbolKind::Module, 1, 20, "")
            .unwrap();
        store
            .insert_symbol(local, "inner", "C", "A::C", SymbolKind::Class, 2, 10, "")
            .unwrap();
        for qualified in ["B", "A::B", "A::C::B", "B::Child", "A::B::Child"] {
            store
                .insert_symbol(
                    remote,
                    qualified,
                    "B",
                    qualified,
                    SymbolKind::Class,
                    1,
                    1,
                    "",
                )
                .unwrap();
        }
        let index = Index::build(&store).unwrap();
        for (line, target) in [
            (0, Some("B")),
            (1, None),
            (2, None),
            (3, Some("A::C::B")),
            (9, Some("A::C::B")),
            (10, None),
            (11, Some("A::B")),
            (19, Some("A::B")),
            (20, None),
            (21, Some("B")),
        ] {
            assert_eq!(
                index.constant(local, "B", Some(line)),
                target.map(str::to_string),
                "line {line}"
            );
        }
        assert_eq!(
            index.constant(local, "B::Child", Some(5)),
            None,
            "A::C::B exists, so its missing Child cannot fall back to A::B::Child"
        );
        assert_eq!(
            index.constant(local, "B::Child", Some(11)),
            Some("A::B::Child".into())
        );
        assert_eq!(index.constant(local, "::B", None), Some("B".into()));
        assert_eq!(index.constant(local, "::Missing", Some(5)), None);
        assert_eq!(index.constant(local, "B", None), None);
    }

    #[test]
    fn constant_receivers_should_reject_expressions_without_guessing_from_the_method_name() {
        for valid in ["Foo", "A::B", "::A::B", "Some_Job", "École"] {
            assert!(constant_path(valid), "{valid}");
        }
        for invalid in [
            "",
            "::",
            "a",
            "Foo::",
            "Foo::::Bar",
            "Foo::bar",
            "Foo!",
            "Foo.new()",
        ] {
            assert!(!constant_path(invalid), "{invalid}");
        }
        let index = Index::default();
        for receiver in ["Foo", "::Foo", "Foo.new(1)", "Foo.tap", "École"] {
            assert_eq!(
                index.decide(1, receiver, "run", Some(1)),
                Some(Decision::Unresolved)
            );
        }
        for receiver in ["self", "variable", "@thing", "", "123"] {
            assert_eq!(index.decide(1, receiver, "run", Some(1)), None);
        }
    }
}
