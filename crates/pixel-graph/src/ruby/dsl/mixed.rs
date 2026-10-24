// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! What a mixin the rules do not replay records: the modules it may mix
//! in where no lookup replays it ([`Fact::Mixed`]), into an object the
//! rules cannot name ([`Fact::Spread`]), or an unknown step that may hold
//! any of the graph's ([`Fact::Opaque`]).

use tree_sitter::Node;

use super::super::Fact;
use super::super::extract::Walker;
use super::{constant_text, fresh, mixed_paths, written_constants};

/// Records each module `args` names as mixed into an object whose own
/// lookup the rules do not replay ([`Fact::Mixed`], R5-05): any constant
/// an argument that is no module constant writes (`(P1)`, `c ? nil : P1`,
/// `P1.itself`) may be the one (R7-05).
pub(super) fn mixed(w: &mut Walker, args: &[Node]) {
    let named: Vec<Node> = args
        .iter()
        .copied()
        .filter(|&arg| !fresh::fresh_module(w, arg))
        .collect();
    for arg in named {
        let mut paths = mixed_paths(w, arg);
        if paths.is_empty() {
            let scope = w.context("", "").cref;
            paths = written_constants(&w.text(arg))
                .into_iter()
                .map(|path| (path, scope.clone()))
                .collect();
        }
        for (path, scope) in paths {
            w.facts.push(Fact::Mixed { path, scope });
        }
    }
}

/// Records an unknown step on the class or module the constant `path`
/// names, in the nesting `scope`, for the mixin `op` there (on its
/// singleton class, `singleton`) of an argument that is no constant path
/// (`Cfg.mod`, `const_get(name)`, `*mods`): a module the rules cannot
/// name, which may be any of the graph's, on that chain ([`Fact::Opaque`],
/// R8-05). An `extend` of a singleton class acts on its own singleton,
/// which no lookup reads.
pub(super) fn opaque(
    w: &mut Walker,
    path: &str,
    scope: Vec<String>,
    op: &str,
    singleton: bool,
    args: &[Node],
) {
    let side = match (op, singleton) {
        ("extend", true) => return,
        ("extend", false) | (_, true) => '.',
        _ => '#',
    };
    if args
        .iter()
        .all(|&arg| constant_text(w, arg).is_some() || fresh::fresh_module(w, arg))
    {
        return;
    }
    w.facts.push(Fact::Opaque {
        path: path.to_owned(),
        scope,
        side,
        prepend: op == "prepend",
    });
}

/// Records as [`Fact::Spread`] every constant code evaluated on an object
/// the rules cannot name may mix into it ([`written_constants`]).
pub(super) fn spread_in_text(w: &mut Walker, text: &str) {
    let scope = w.context("", "").cref;
    for path in written_constants(text) {
        w.facts.push(Fact::Spread {
            path,
            scope: scope.clone(),
        });
    }
}

/// Records as [`Fact::Mixed`] every constant code the rules do not read
/// may name ([`written_constants`]): it may mix that module in where no
/// lookup replays it (R6-08, R6-14).
pub(super) fn mixed_in_text(w: &mut Walker, text: &str) {
    let scope = w.context("", "").cref;
    for path in written_constants(text) {
        w.facts.push(Fact::Mixed {
            path,
            scope: scope.clone(),
        });
    }
}
