// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The frames of the walk: the locals Ruby's parser knows, and what a
//! `def`, a declaration and `self` mean in each body.

use super::{BlockKind, Defs, Landing, Scope, This};

/// The locals Ruby's parser knows at the current point of the walk, one
/// frame per open scope, innermost last. Ruby's rule: a name is a local
/// once an assignment to it (or a parameter of that name) has been parsed
/// earlier in a visible scope; otherwise the bare name is a method call
/// (v1 `RubyLocals`).
#[derive(Debug, Default)]
pub(in crate::ruby) struct Locals {
    frames: Vec<Frame>,
}

/// The kind of body a frame is in: the gate around it, for a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Gate {
    Top,
    Owner,
    SingletonClass,
    Method,
}

#[derive(Debug)]
struct Frame {
    scope: Scope,
    gate: Gate,
    /// What `self` is here.
    this: This,
    names: Vec<String>,
    defs: Defs,
    /// A `module` body, where a bare `module_function` applies.
    module: bool,
    /// `self` is an instance here (a `def` body, or a block in one), not
    /// the class or module being defined.
    instance_self: bool,
    /// Where a `def` here lands when `defs` is [`Defs::Foreign`].
    landing: Landing,
    /// Whether a bare `private` or `protected` is in force here; `None` in
    /// an iterator's block, which shares the body's (Ruby 3.3: `private`
    /// then `[1].each { def a; end }` makes `a` private, and a `private`
    /// in the block reaches the body).
    hidden: Option<bool>,
}

impl Locals {
    /// Opens the frame of a node of `kind`. A block that runs on the object
    /// around it keeps the enclosing frame's `def` qualification and
    /// `self`, as a `def` there defines a method of the enclosing class; a
    /// constructor's block defines on its new class; any other block on an
    /// object the rules cannot name. `class << self` names the class or
    /// module only where `self` is one: in a `def` body it is an instance,
    /// and in a block that runs elsewhere another object, whose singleton
    /// the rules cannot name, as for `class << x`.
    /// `landing` is where the `def`s of a [`BlockKind::Foreign`] block may
    /// land; any other frame takes its own or its enclosing frame's.
    pub(super) fn open(
        &mut self,
        scope: Scope,
        kind: &str,
        value_is_self: bool,
        block: Option<BlockKind>,
        this: This,
        landing: Option<Landing>,
    ) {
        let enclosing = self.frames.last();
        let gate = match kind {
            "program" => Gate::Top,
            "class" | "module" => Gate::Owner,
            "singleton_class" => Gate::SingletonClass,
            "method" | "singleton_method" => Gate::Method,
            _ => enclosing.map_or(Gate::Top, |f| f.gate),
        };
        let enclosing_defs = enclosing.map_or(Defs::Instance, |f| f.defs);
        let enclosing_instance = enclosing.is_some_and(|f| f.instance_self);
        let enclosing_module = enclosing.is_some_and(|f| f.module);
        let enclosing_landing = enclosing.map_or(Landing::Nowhere, |f| f.landing.clone());
        let (defs, instance_self, module, landing) = match kind {
            "block" | "do_block" | "lambda" => match block {
                Some(BlockKind::Keeps | BlockKind::Once) => {
                    (enclosing_defs, enclosing_instance, false, enclosing_landing)
                }
                // What a concern's block defines lands on its includers
                // (B6).
                Some(BlockKind::Included { .. }) => (
                    Defs::Foreign,
                    enclosing_instance,
                    false,
                    landing.unwrap_or(Landing::Nowhere),
                ),
                // `self` is the class: a `def` defines on it, and a bare
                // `module_function` written inside applies.
                Some(BlockKind::ClassEval) => {
                    let defs = match enclosing_defs {
                        Defs::Singleton => Defs::Singleton,
                        Defs::Foreign => Defs::Foreign,
                        _ => Defs::Instance,
                    };
                    (defs, false, enclosing_module, enclosing_landing)
                }
                Some(BlockKind::InstanceEval) => {
                    (Defs::InstanceEval, false, false, Landing::Nowhere)
                }
                // `X = Module.new do` is a module body, where
                // `module_function` applies.
                Some(BlockKind::Defines { module }) => {
                    (Defs::Instance, false, module, Landing::Nowhere)
                }
                // `with_options` evaluates a block without parameters on
                // its option merger, whose singleton class a `def` there
                // defines on.
                Some(BlockKind::Forwards) => {
                    (Defs::Foreign, enclosing_instance, false, Landing::Nowhere)
                }
                _ => (
                    Defs::Foreign,
                    enclosing_instance,
                    false,
                    landing.unwrap_or(enclosing_landing),
                ),
            },
            // A nested `def` runs when the method does, on the class the
            // method was defined in.
            "method" | "singleton_method" => (
                enclosing_defs.nested(),
                kind == "method",
                false,
                enclosing_landing,
            ),
            "singleton_class"
                if value_is_self && !enclosing_instance && enclosing_defs != Defs::Foreign =>
            {
                (Defs::Singleton, false, false, Landing::Nowhere)
            }
            // `class << self` in a block that may run on the class is its
            // singleton class too; `class << x` and `class << self` on an
            // instance are one object's.
            "singleton_class" if value_is_self && !enclosing_instance => {
                (Defs::Foreign, false, false, enclosing_landing)
            }
            // `class << x`: where the caller says (B2).
            "singleton_class" => (
                Defs::Foreign,
                false,
                false,
                landing.unwrap_or(Landing::Nowhere),
            ),
            _ => (Defs::Instance, false, kind == "module", Landing::Nowhere),
        };
        let hidden = (block != Some(BlockKind::Keeps) || !matches!(kind, "block" | "do_block"))
            .then_some(false);
        self.frames.push(Frame {
            scope,
            gate,
            this,
            names: Vec::new(),
            defs,
            module,
            instance_self,
            landing,
            hidden,
        });
    }

    /// Where a `def` written in the innermost frame lands when it has no
    /// owner the rules can name.
    pub(in crate::ruby) fn landing(&self) -> Landing {
        self.frames
            .last()
            .map_or(Landing::Nowhere, |f| f.landing.clone())
    }

    /// Whether the innermost frame is nested in a method body: a `def`
    /// there runs only when that method does.
    pub(super) fn in_method(&self) -> bool {
        self.frames
            .len()
            .checked_sub(2)
            .is_some_and(|i| self.frames[i].gate == Gate::Method)
    }

    pub(super) fn close(&mut self) {
        self.frames.pop();
    }

    /// The gate of the innermost frame.
    pub(super) fn gate(&self) -> Gate {
        self.frames.last().map_or(Gate::Top, |f| f.gate)
    }

    /// What `self` is in the innermost frame.
    pub(super) fn this(&self) -> This {
        self.frames.last().map_or(This::Own, |f| f.this)
    }

    pub(super) fn bind(&mut self, name: String) {
        if let Some(frame) = self.frames.last_mut() {
            frame.names.push(name);
        }
    }

    pub(in crate::ruby) fn is_local(&self, name: &str) -> bool {
        for frame in self.frames.iter().rev() {
            if frame.names.iter().any(|known| known == name) {
                return true;
            }
            if frame.scope == Scope::Gate {
                return false;
            }
        }
        false
    }

    /// What a declaration written in the innermost frame's body generates
    /// (`attr_reader` in `class << self` makes class methods).
    pub(in crate::ruby) fn current_defs(&self) -> Defs {
        self.frames.last().map_or(Defs::Instance, |f| f.defs)
    }

    /// Whether the frame around the innermost one is a block or a lambda:
    /// a `def` whose own frame is the innermost one is written in a block.
    pub(super) fn in_block(&self) -> bool {
        self.frames
            .len()
            .checked_sub(2)
            .is_some_and(|i| self.frames[i].scope == Scope::Block)
    }

    /// The qualification of a `def` whose own frame is the innermost one:
    /// the frame around it decides.
    pub(super) fn enclosing_defs(&self) -> Defs {
        self.frames
            .len()
            .checked_sub(2)
            .map_or(Defs::Instance, |i| self.frames[i].defs)
    }

    /// The gate of the frame around the innermost one: where a `def` whose
    /// own frame is the innermost one is written.
    pub(super) fn enclosing_gate(&self) -> Gate {
        self.frames
            .len()
            .checked_sub(2)
            .map_or(Gate::Top, |i| self.frames[i].gate)
    }

    /// Whether `self` is an instance in the frame around the innermost one:
    /// `def self.m` written in a method body defines on that instance.
    pub(super) fn enclosing_instance(&self) -> bool {
        self.frames
            .len()
            .checked_sub(2)
            .is_some_and(|i| self.frames[i].instance_self)
    }

    /// Whether a bare `private` or `protected` is in force in the frame
    /// `at` places from the innermost one: what a `def` or a declaration
    /// defines there may be hidden from an explicit receiver (R9-03).
    pub(super) fn hidden(&self, at: usize) -> bool {
        self.frames
            .iter()
            .rev()
            .skip(at)
            .find_map(|f| f.hidden)
            .unwrap_or(false)
    }

    /// A bare visibility word: `private` and `protected` hide what the body
    /// defines next, `public` no longer ([`Self::hidden`]). Written
    /// directly in a `module` body, `module_function` makes the following
    /// `def`s module functions, both the module's method and a private
    /// instance method, and `public`, `private` or `protected` ends that
    /// mode.
    pub(super) fn visibility(&mut self, word: &str) {
        let hidden = match word {
            "private" | "protected" => Some(true),
            "public" => Some(false),
            _ => None,
        };
        if hidden.is_some()
            && let Some(frame) = self.frames.iter_mut().rev().find(|f| f.hidden.is_some())
        {
            frame.hidden = hidden;
        }
        let Some(frame) = self.frames.last_mut().filter(|f| f.module) else {
            return;
        };
        match word {
            // The instance copies are private, and so is what a
            // declaration defines there (R10-04).
            "module_function" => {
                frame.defs = Defs::ModuleFunction;
                frame.hidden = Some(true);
            }
            "public" | "private" | "protected" => frame.defs = Defs::Instance,
            _ => {}
        }
    }
}
