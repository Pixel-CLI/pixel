// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! A mixin argument that builds a new module where it is written (#111).
//!
//! `K.singleton_class.prepend(Module.new do ... end)` mixes in a module
//! that exists nowhere else: `Module.new` returns a new, anonymous
//! module, never one of the graph's, and nothing it holds is a constant's.
//! What its block defines is defined on an object the rules cannot name
//! and is tainted where it is written; what its block mixes in is mixed
//! into that object, recorded there as for any such object (R5-03,
//! R5-04). So the argument names no module of the graph: it is no step
//! that may hold one (`Fact::Opaque`, R8-05), and the constants written
//! in its block are not mixed in by the call (`Fact::Mixed`, R7-05).
//!
//! The argument must be `Module.new` with no argument but a block written
//! there (a block passed from elsewhere, `Module.new(&body)`, may mix in
//! anything), sent to `::Module`, or to `Module` where no nesting can
//! hold another constant of that name: at the top level of the file, and
//! in blocks there.

use tree_sitter::Node;

use super::super::extract::Walker;

/// Whether `arg` is `Module.new` (or `Module.new do … end`), which
/// builds a module no other code names.
pub(in crate::mvp_index::ruby) fn fresh_module(w: &Walker, arg: Node) -> bool {
    if arg.kind() != "call" || w.field_text(arg, "method").as_deref() != Some("new") {
        return false;
    }
    let Some(receiver) = arg.child_by_field_name("receiver") else {
        return false;
    };
    let rooted = match w.text(receiver).as_str() {
        "::Module" => true,
        "Module" => false,
        _ => return false,
    };
    if !rooted && !w.context("", "").cref.is_empty() {
        return false;
    }
    let mut walk = arg.walk();
    arg.child_by_field_name("arguments")
        .is_none_or(|args| args.named_children(&mut walk).next().is_none())
}

#[cfg(test)]
mod tests {
    use crate::mvp_index::ruby::Fact;
    use crate::mvp_index::ruby::tests::facts_of;

    fn opaque_or_mixed(facts: &[Fact]) -> Vec<String> {
        facts
            .iter()
            .filter_map(|fact| match fact {
                Fact::Opaque { path, .. } => Some(format!("opaque {path}")),
                Fact::Mixed { path, .. } => Some(format!("mixed {path}")),
                _ => None,
            })
            .collect()
    }

    /// A module `Module.new` builds where the mixin is written is none of
    /// the graph's: no unknown step, no constant of its block mixed in.
    /// Ruby 3.3: `Alterity.singleton_class.prepend(Module.new do def x;
    /// InstantDdlFirst.go; end end)` puts an anonymous module first in
    /// `Alterity.singleton_class.ancestors`; `InstantDdlFirst` is not there.
    #[test]
    fn c10_ruby_a_mixin_of_a_new_module_names_no_module_of_the_graph() {
        // #111
        for code in [
            "Alterity.singleton_class.prepend(Module.new do\n  def execute_alter(t)\n    InstantDdlFirst.go(t)\n  end\nend)\n",
            "Alterity.prepend(::Module.new { def x; Other.y; end })\n",
        ] {
            assert_eq!(
                opaque_or_mixed(&facts_of(code)),
                Vec::<String>::new(),
                "{code}"
            );
        }
    }

    /// Any other argument may be one of the graph's modules: a module a
    /// method returns, `Module.new` given a block from elsewhere, or a
    /// `Module` a nesting may hold.
    #[test]
    fn c10_ruby_a_mixin_of_another_expression_stays_unknown() {
        // R8-05, R7-05
        for code in [
            "Alterity.prepend(Cfg.mod)\n",
            "Alterity.prepend(Module.new(&body))\n",
            "module App\n  Alterity.prepend(Module.new { def x; end })\nend\n",
        ] {
            assert!(
                opaque_or_mixed(&facts_of(code))
                    .iter()
                    .any(|f| f.starts_with("opaque")),
                "{code}"
            );
        }
    }
}
