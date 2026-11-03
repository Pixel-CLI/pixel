// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The hooks Ruby calls on an `include`, `extend`, `prepend` or subclass
//! (`included`, `extended`, `prepended`, `inherited`, and Active
//! Support's concern rules), replayed for the owner being built: which
//! definition runs, and what its statements do to the object it runs for
//! (D14, B3, R7).

use super::super::corelib::defines_no_hook;
use super::super::{Def, Index, Side, facts, split_method};
use super::{Building, Chain, Decl, Lookup, Node, Object, Op, Stands};
use crate::mvp_index::ruby::dsl::{FEATURES, HOOKS};

/// A hook's definition, as lookup finds it.
pub(super) enum HookDef {
    None,
    Found(usize),
    /// Several definitions, or one past an ancestor the graph cannot name.
    Unknown,
}

/// What one statement of a hook does to the object it runs for.
pub(super) enum HookOp<'a> {
    Act(Op, Option<&'a str>, Stands<'a>),
    /// `super`: the next definition of the hook.
    Super,
    Unknown,
}

impl<'a> Index<'a> {
    /// The concern's own `self.included(base)` (`prepended`), not Active
    /// Support's, which Ruby calls after `append_features`
    /// (`prepend_features`) whatever it did.
    pub(super) fn concern_hook(
        &self,
        b: &mut Building<'a, '_>,
        object: Object,
        op: Op,
        module: &'a str,
        decl: Decl<'a>,
    ) {
        let hook = if op == Op::Prepend {
            "prepended"
        } else {
            "included"
        };
        match self.method(module, Side::Singleton, hook) {
            None => {}
            Some(Some(id)) => match self.defined_by(id, decl) {
                Some(true) => self.run_ops(b, object, module, id, decl),
                Some(false) => {}
                None => b.unknown_front(object, decl),
            },
            Some(None) => b.unknown_front(object, decl),
        }
    }

    /// Runs `module`'s hook `name` for `b`'s owner (or its singleton
    /// class).
    pub(super) fn run_hook(
        &self,
        b: &mut Building<'a, '_>,
        object: Object,
        module: &'a str,
        name: &str,
        decl: Decl<'a>,
    ) {
        match self.ran(module, self.hook_def(module, name), decl) {
            HookDef::None => {}
            HookDef::Unknown => b.unknown_front(object, decl),
            HookDef::Found(id) => self.run_ops(b, object, module, id, decl),
        }
    }

    /// The definition of a hook that runs for `decl`, given `found`, the
    /// first on `module`'s singleton chain: `found` when it is defined by
    /// then, else the next one there; unknown when the load order decides
    /// (B3), and so when a definition comes back (a chain holding its step
    /// twice). Ruby calls the hook lookup finds when the `include` or the
    /// subclass runs.
    pub(super) fn ran(&self, module: &'a str, mut found: HookDef, decl: Decl<'a>) -> HookDef {
        let mut seen = Vec::new();
        loop {
            let HookDef::Found(id) = found else {
                return found;
            };
            if seen.contains(&id) {
                return HookDef::Unknown;
            }
            seen.push(id);
            match self.defined_by(id, decl) {
                Some(true) => return found,
                Some(false) => found = self.hook_after(module, id),
                None => return HookDef::Unknown,
            }
        }
    }

    /// Whether the method `id` is defined when `decl` runs ([`Index::before`]).
    pub(super) fn defined_by(&self, id: usize, decl: Decl<'a>) -> Option<bool> {
        let symbol = &self.symbols[id];
        let by = split_method(&symbol.name).map_or("", |(owner, _, _)| owner);
        self.before((symbol.file.as_str(), symbol.line), by, decl.at)
    }

    /// Runs the statements of the hook method `hook`, found on `module`'s
    /// singleton chain. One run again inside itself (its `super` reaching
    /// it back) is cut.
    pub(super) fn run_ops(
        &self,
        b: &mut Building<'a, '_>,
        object: Object,
        module: &'a str,
        hook: usize,
        decl: Decl<'a>,
    ) {
        if b.running.contains(&hook) {
            return b.cut(object, module, decl);
        }
        b.running.push(hook);
        self.run_hook_ops(b, object, module, hook, decl);
        b.running.pop();
    }

    /// [`Self::run_ops`], once its guard let it through.
    fn run_hook_ops(
        &self,
        b: &mut Building<'a, '_>,
        object: Object,
        module: &'a str,
        hook: usize,
        decl: Decl<'a>,
    ) {
        for op in self.hook_ops(hook) {
            match op {
                HookOp::Act(op, ancestor, stands) => {
                    self.act(b, object, op, ancestor, stands, decl)
                }
                HookOp::Super => match self.ran(module, self.hook_after(module, hook), decl) {
                    HookDef::None => {}
                    HookDef::Unknown => b.unknown_front(object, decl),
                    HookDef::Found(next) => self.run_ops(b, object, module, next, decl),
                },
                HookOp::Unknown => b.unknown_front(object, decl),
            }
        }
    }

    /// The definition `super` in the hook method `hook` runs: the next one
    /// of its name on `module`'s singleton chain after `hook`'s own step.
    /// One past a step the graph cannot name may or may not run.
    pub(super) fn hook_after(&self, module: &'a str, hook: usize) -> HookDef {
        let Some((owner, side, name)) = split_method(&self.symbols[hook].name) else {
            return HookDef::Unknown;
        };
        let chain = self.deeper(|| self.chain(module, Side::Singleton));
        let Some(at) = chain
            .steps
            .iter()
            .position(|step| step.node == Node::Known(owner, side))
        else {
            return HookDef::Unknown;
        };
        let mut past_unknown = false;
        for step in &chain.steps[at + 1..] {
            match step.node {
                // Past a walk cut short, a next definition may be there.
                Node::Unknown if step.stands == Stands::Cut => return HookDef::Unknown,
                Node::Unknown => past_unknown = true,
                Node::Known(o, s) if self.defines(o, s, name) => {
                    return match self.method(o, s, name) {
                        Some(Some(id)) if !past_unknown && !step.maybe => HookDef::Found(id),
                        _ => HookDef::Unknown,
                    };
                }
                Node::Known(..) => {}
            }
        }
        if self.core_hook(name) {
            return HookDef::Unknown;
        }
        HookDef::None
    }

    /// The definition of `module`'s hook `name` lookup finds on its
    /// singleton chain (a method of the module itself, or of a module it
    /// extends); Active Support's `included` and `append_features` are the
    /// concern rules. A module that extends one the graph cannot name (a
    /// gem's `extend Hooky`) may get any hook there (R7); a superclass's
    /// gem step is taken to define no `inherited` that changes a chain (a
    /// known residual: Active Record's adds its generated modules).
    pub(super) fn hook_def(&self, module: &'a str, name: &str) -> HookDef {
        match self.deeper(|| self.lookup(module, Side::Singleton, name)) {
            Lookup::Found(id) => HookDef::Found(id),
            Lookup::NotFound if name != "inherited" && self.extends_unknown(module, name) => {
                HookDef::Unknown
            }
            Lookup::NotFound if self.core_hook(name) => HookDef::Unknown,
            Lookup::NotFound => HookDef::None,
            Lookup::Abstain => HookDef::Unknown,
        }
    }

    /// Whether a reopened `Module` or `Class`, a module prepended to one,
    /// or code evaluated on one from outside its body, may define the hook
    /// `name`, which Ruby then calls for every module or class past their
    /// own definitions (R6-16); for `inherited`, the same on the own side
    /// of `Object` and `BasicObject`, which a class's singleton chain
    /// reaches before `Class` (R7-07).
    pub(super) fn core_hook(&self, name: &str) -> bool {
        let instance = [facts::MODULE, facts::CLASS].map(|core| (core, Side::Instance));
        let own = [facts::OBJECT, facts::BASIC_OBJECT].map(|core| (core, Side::Singleton));
        let own = own.into_iter().filter(|_| name == "inherited");
        instance.into_iter().chain(own).any(|(core, side)| {
            self.facts.changed(core, side, name)
                || self.defines(core, side, name)
                || self.known(core).is_some_and(|owner| {
                    self.deeper(|| self.chain(owner, side))
                        .steps()
                        .any(|(o, s)| self.defines(o, s, name) || self.unsure(o, s, name))
                })
        })
    }

    /// Whether `module`'s singleton chain holds a step the graph cannot
    /// name that may define the hook `name`, or meets the depth cap, past
    /// which a hook may be too (R6-04).
    pub(super) fn extends_unknown(&self, module: &'a str, name: &str) -> bool {
        let walks = &self.walks;
        let outer = walks.capped.replace(false);
        let unknown = self.deeper(|| self.may_hook(&self.chain(module, Side::Singleton), name));
        let capped = walks.capped.get();
        walks.capped.set(outer || capped);
        unknown || capped
    }

    /// Whether an unknown step of `chain` may define the hook `name`: not
    /// the rest of a core module ([`defines_no_hook`]), not
    /// `ActiveSupport::Concern`, not a module's own list after `extend
    /// self` unless that list may.
    pub(super) fn may_hook(&self, chain: &Chain<'a>, name: &str) -> bool {
        chain.steps.iter().enumerate().any(|(i, step)| match step.stands {
            _ if step.node != Node::Unknown => false,
            Stands::Any | Stands::Unread | Stands::Cut | Stands::Opaque { .. } => !(i > 0
                && matches!(chain.steps[i - 1].node, Node::Known(o, _) if defines_no_hook(o))),
            Stands::Concern | Stands::Gem => false,
            Stands::Itself(module) => self.deeper(|| {
                self.lookup(module, Side::Instance, name) != Lookup::NotFound
                    || self.may_hook(&self.chain(module, Side::Instance), name)
            }),
        })
    }

    /// Whether `module` has a hook the rules do not read, on its own side
    /// or, for the objects it extends, on its instance side: one defined
    /// other than by a `def` they read (a [`Fact::Hook`] at its own line:
    /// in `class << self`, as `def M.included`, by `define_singleton_method`,
    /// with parameters they do not read, `append_features` and its
    /// family), defined twice, or one code from outside its body may
    /// define or remove (R8-06). Read from the definitions and facts alone,
    /// before any chain is walked.
    ///
    /// [`Fact::Hook`]: crate::mvp_index::ruby::Fact::Hook
    pub(in crate::mvp_index::ruby::resolve) fn unknown_hook(&self, module: &str) -> bool {
        HOOKS.iter().chain(FEATURES).any(|&name| {
            [Side::Singleton, Side::Instance].into_iter().any(|side| {
                let defined = self.methods[side as usize]
                    .get(module)
                    .and_then(|methods| methods.get(name));
                self.facts.changed(module, side, name)
                    || self.facts.unknown(module, side, name)
                    || match defined {
                        Some(Def::One(id)) => {
                            let symbol = &self.symbols[*id];
                            self.facts
                                .hook_lines(&symbol.file, &symbol.name)
                                .contains(&symbol.line)
                        }
                        Some(Def::Ambiguous) => true,
                        None => false,
                    }
            })
        })
    }

    /// Whether `module` defines `features` (`append_features` and its
    /// family) itself or through a module it extends; a concern's own
    /// definition only, Active Support's being the concern rules.
    pub(super) fn replaces(&self, module: &'a str, features: &str) -> bool {
        if self.concerns.contains(module) && features != "extend_object" {
            return self.method(module, Side::Singleton, features).is_some();
        }
        !matches!(self.hook_def(module, features), HookDef::None)
    }

    /// What the hook method `hook` does, statement by statement.
    pub(super) fn hook_ops(&self, hook: usize) -> Vec<HookOp<'a>> {
        let symbol = &self.symbols[hook];
        let mut ops: Vec<(u32, usize, HookOp<'a>)> = self
            .hooks_of
            .get(&hook)
            .into_iter()
            .flatten()
            .map(|&i| {
                let mixin = &self.mixins[i];
                let module = self.ancestor_of(mixin, None);
                let stands = self.stands_for(mixin, module);
                (
                    mixin.line,
                    mixin.edge,
                    HookOp::Act(mixin.kind.op, module, stands),
                )
            })
            .collect();
        for line in self.facts.super_lines(&symbol.file, &symbol.name) {
            ops.push((line, 0, HookOp::Super));
        }
        for line in self.facts.hook_lines(&symbol.file, &symbol.name) {
            // Last among the statements of its line: the unknown one may
            // have come after.
            ops.push((line, usize::MAX, HookOp::Unknown));
        }
        ops.sort_by_key(|&(line, edge, _)| (line, edge));
        ops.into_iter().map(|(_, _, op)| op).collect()
    }
}
