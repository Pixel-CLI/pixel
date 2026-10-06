// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The ancestors a Ruby class or module declares literally: its superclass
//! (`class C < B`), and the modules it `include`s, `prepend`s and `extend`s,
//! each stored as written with its line so the resolver can look the
//! constant up in the lexical scope of the declaration.
//!
//! `ActiveSupport::Concern` adds two forms the resolver models: an
//! `included do ... end` block, whose `include`/`extend`/`prepend` run on
//! the class that includes the concern (stored with the `included:` kind
//! prefix), and `class_methods do ... end`, whose `def`s become methods of
//! the concern's `ClassMethods` module, which the includer extends.
//! A non-constant argument (`include mod`, `include Module.new`) is stored
//! without a target: the resolver cannot name it and abstains past it.
//! <https://docs.ruby-lang.org/en/master/Module.html#method-i-include>
//! <https://api.rubyonrails.org/classes/ActiveSupport/Concern.html>

use tree_sitter::Node;

use super::{Walker, each_child, field_text, line_start};

/// One declared ancestor of a Ruby class or module.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RawMixin {
    /// The qualified name of the declaring class or module (`Admin::Order`).
    pub owner: String,
    /// `superclass`, `include`, `prepend` or `extend`, prefixed with
    /// `included:` when declared in a concern's `included` block.
    pub kind: String,
    /// The constant path as written (`Trackable`, `::Billing::Base`), or
    /// `None` when the argument is not a literal constant.
    pub target: Option<String>,
    pub site_line: u32,
}

/// The kinds of ancestor a call can declare.
const MIXIN_METHODS: &[&str] = &["include", "prepend", "extend"];

/// Record the superclass of a `class` node, whose owner is already on the
/// walker's stack. `class C < Struct.new(:a)` is stored without a target.
pub(super) fn walk_superclass(w: &mut Walker, class: Node) {
    let Some(superclass) = class.child_by_field_name("superclass") else {
        return;
    };
    let Some(expr) = each_child(superclass).into_iter().find(Node::is_named) else {
        return;
    };
    let target = constant_text(w, expr);
    w.mixins.push(RawMixin {
        owner: w.stack.join("::"),
        kind: "superclass".to_string(),
        target,
        site_line: line_start(class),
    });
}

/// Record the modules a receiver-less (or `self.`) `include`, `prepend` or
/// `extend` call declares in a class or module body.
pub(super) fn walk_mixin_call(w: &mut Walker, call: Node, method: &str) {
    if !MIXIN_METHODS.contains(&method) || w.stack.is_empty() {
        return;
    }
    if field_text(w, call, "receiver").is_some_and(|r| r != "self") {
        return;
    }
    let Some(deferred) = owner_body(w, call) else {
        return;
    };
    let Some(args) = call.child_by_field_name("arguments") else {
        return;
    };
    let kind = if deferred {
        format!("included:{method}")
    } else {
        method.to_string()
    };
    for arg in each_child(args).into_iter().filter(Node::is_named) {
        if arg.kind() == "comment" {
            continue;
        }
        w.mixins.push(RawMixin {
            owner: w.stack.join("::"),
            kind: kind.clone(),
            target: constant_text(w, arg),
            site_line: line_start(call),
        });
    }
}

/// True iff `call` is a `class_methods do ... end` (or `{ }`) written in a
/// module body: its block defines the concern's `ClassMethods` module.
pub(super) fn is_class_methods_block(w: &Walker, call: Node) -> bool {
    call.child_by_field_name("receiver").is_none()
        && call.child_by_field_name("block").is_some()
        && field_text(w, call, "method").as_deref() == Some("class_methods")
        && nearest_owner(call).is_some_and(|n| n.kind() == "module")
}

/// `Some(deferred)` when `node` sits in a class or module body, directly or
/// through blocks, where `deferred` says one of those blocks is a concern's
/// `included do` (its body runs on the includer). `None` inside a method
/// body or outside any owner.
fn owner_body(w: &Walker, node: Node) -> Option<bool> {
    let mut deferred = false;
    let mut current = node.parent();
    while let Some(n) = current {
        match n.kind() {
            "class" | "module" => return Some(deferred),
            "method" | "singleton_method" | "singleton_class" => return None,
            "block" | "do_block" => {
                if let Some(call) = n.parent().filter(|p| p.kind() == "call")
                    && call.child_by_field_name("receiver").is_none()
                    && field_text(w, call, "method").as_deref() == Some("included")
                {
                    deferred = true;
                }
            }
            _ => {}
        }
        current = n.parent();
    }
    None
}

fn nearest_owner(node: Node) -> Option<Node> {
    std::iter::successors(node.parent(), Node::parent).find(|n| {
        matches!(
            n.kind(),
            "class" | "module" | "method" | "singleton_method" | "singleton_class"
        )
    })
}

/// The text of a literal constant path (`A`, `A::B`, `::A::B`), or `None`
/// for any other expression.
fn constant_text(w: &Walker, node: Node) -> Option<String> {
    match node.kind() {
        "constant" => Some(w.text(node)),
        "scope_resolution" => {
            let scope_ok = node
                .child_by_field_name("scope")
                .is_none_or(|scope| constant_text(w, scope).is_some());
            let name_ok = node
                .child_by_field_name("name")
                .is_some_and(|name| name.kind() == "constant");
            (scope_ok && name_ok).then(|| w.text(node))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::RawMixin;
    use crate::extract::extract_file;

    fn mixins(source: &str) -> Vec<(String, String, Option<String>, u32)> {
        extract_file("app/models/a.rb", source.as_bytes())
            .unwrap()
            .mixins
            .into_iter()
            .map(
                |RawMixin {
                     owner,
                     kind,
                     target,
                     site_line,
                 }| (owner, kind, target, site_line),
            )
            .collect()
    }

    fn row(
        owner: &str,
        kind: &str,
        target: Option<&str>,
        line: u32,
    ) -> (String, String, Option<String>, u32) {
        (owner.into(), kind.into(), target.map(Into::into), line)
    }

    #[test]
    fn ruby_mixins_should_record_each_literal_ancestor_with_its_owner_and_line() {
        let source = "\
module Admin
  class Order < ::Base
    include Trackable, Admin::Auditable
    prepend Loud
    extend Sortable
    self.include Kernelish
    include(plugin)
    include Module.new
    other.include Ignored
    def run
      include NotAnAncestor
    end
  end
  class Point < Struct.new(:x)
  end
  module Tracked
    extend ActiveSupport::Concern
    included do
      include Audited
      extend Finders
    end
    class_methods do
      def since; end
    end
  end
end
include TopLevel
";
        assert_eq!(
            mixins(source),
            [
                row("Admin::Order", "superclass", Some("::Base"), 2),
                row("Admin::Order", "include", Some("Trackable"), 3),
                row("Admin::Order", "include", Some("Admin::Auditable"), 3),
                row("Admin::Order", "prepend", Some("Loud"), 4),
                row("Admin::Order", "extend", Some("Sortable"), 5),
                row("Admin::Order", "include", Some("Kernelish"), 6),
                row("Admin::Order", "include", None, 7),
                row("Admin::Order", "include", None, 8),
                row("Admin::Point", "superclass", None, 14),
                row(
                    "Admin::Tracked",
                    "extend",
                    Some("ActiveSupport::Concern"),
                    17
                ),
                row("Admin::Tracked", "included:include", Some("Audited"), 19),
                row("Admin::Tracked", "included:extend", Some("Finders"), 20),
            ]
        );
        let fx = extract_file("app/models/a.rb", source.as_bytes()).unwrap();
        let qualified: Vec<&str> = fx.symbols.iter().map(|s| s.qualified.as_str()).collect();
        assert!(
            qualified.contains(&"Admin::Tracked::ClassMethods")
                && qualified.contains(&"Admin::Tracked::ClassMethods#since"),
            "`class_methods do` defines the concern's ClassMethods module: {qualified:?}"
        );
        assert!(!qualified.contains(&"Admin::Tracked#since"));
    }
}
