// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! What is defined on an object the rules cannot name exactly (R5-03,
//! R5-04): outside every owner, on one object's singleton class, in a new
//! anonymous class, in a block a method the rules do not model may run
//! anywhere, on a receiver that is neither `self` nor a constant. Modelling
//! these forms one by one did not converge, so each records the names it
//! may define ([`Fact::Tainted`]), or the modules it mixes in
//! ([`Fact::Spread`]), and no lookup of those names proves anything.

use tree_sitter::Node;

use super::{Defs, Landing, Walker};
use crate::ruby::{Fact, dsl};

impl Walker<'_> {
    /// Whether the innermost frame's `def`s land on an object no lookup of
    /// a named owner can tell: a block that may run anywhere, outside every
    /// owner or on one object's singleton class (`Landing::Nowhere`), in a
    /// body under a method the rules do not model, a class macro's caller,
    /// or an expression's singleton class. A block evaluated on a constant,
    /// `class << X` and a concern's `included do` are placed exactly, or
    /// left to resolution to tell.
    pub(in crate::ruby) fn unplaced(&self) -> bool {
        self.locals.current_defs() == Defs::Foreign
            && matches!(
                self.locals.landing(),
                Landing::Nowhere
                    | Landing::Owner(_)
                    | Landing::SingletonOfOwner(_)
                    | Landing::Macro { .. }
            )
    }

    /// The parameter of the innermost hook method around the walk, the
    /// object it runs for, when a replay reads the hook (D14).
    pub(in crate::ruby) fn hook_base(&self) -> Option<&str> {
        self.scopes
            .iter()
            .rev()
            .find_map(|def| self.hook_bases.get(def))
            .map(String::as_str)
    }

    /// Records that a method `name` (`None`: any name) is defined on an
    /// object the rules cannot name.
    pub(in crate::ruby) fn taint(&mut self, name: Option<String>) {
        self.facts.push(Fact::Tainted { name });
    }

    /// Records the names a definer may define (`None`: any).
    pub(in crate::ruby) fn taint_all(&mut self, names: Option<Vec<String>>) {
        match names {
            Some(names) => {
                for name in names {
                    self.taint(Some(name));
                }
            }
            None => self.taint(None),
        }
    }

    /// Records each module `args` names as mixed into an object the rules
    /// cannot name: `self` in a module body is that module, and any
    /// constant an argument that is no module constant writes (`(P1)`, `c ?
    /// nil : P1`) may be the one (R7-05). A local (one holding `Module.new
    /// do`, whose definitions are tainted where they are written) is not
    /// followed: a named module reached through a variable is a known
    /// residual, and so is a gem's module, whose names the graph does not
    /// hold.
    pub(in crate::ruby) fn spread(&mut self, args: &[Node]) {
        let scope = self.context("", "").cref;
        for &arg in args {
            let mut paths = dsl::mixed_paths(self, arg);
            if arg.kind() == "self"
                && let Some(owner) = self.owners.last()
            {
                paths.push((format!("::{}", owner.qualified), scope.clone()));
            }
            if paths.is_empty() {
                paths = dsl::written_constants(&self.text(arg))
                    .into_iter()
                    .map(|path| (path, scope.clone()))
                    .collect();
            }
            for (path, scope) in paths {
                self.facts.push(Fact::Spread { path, scope });
            }
        }
    }
}
