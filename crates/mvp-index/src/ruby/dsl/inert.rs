// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Literal code a binding evaluates that can define nothing (#111).
//!
//! `x.eval(code)` with a receiver is `Binding#eval` or `Kernel.eval` as
//! far as the rules can tell: the code runs on a `self` they cannot name,
//! so what it defines, mixes in or reopens may land anywhere, `Object`
//! included (R7-03, [`super::eval_elsewhere`]). On gumroad a Redis
//! script (`$redis.eval(<<~LUA, ...)`) was read so, and every lookup of
//! the repository went through an `Object` that might hold any method.
//!
//! Code is inert when Ruby running it, on any `self`, defines no method,
//! constant, class or module and changes no ancestor itself:
//!
//! - it is one literal, with no interpolation, so the text read is the
//!   text run;
//! - the grammar reads all of it (no `ERROR` or missing node): a region
//!   it cannot read is not judged;
//! - it holds no definition (`def`, `def x.m`, `class`, `module`, `class <<
//!   x`, `alias`, `undef`) and no constant assignment;
//! - no word of it, as a method name, a local or a constant, is one that
//!   defines, evaluates, sends by name, loads a file or reaches a binding
//!   ([`DENIED`]), nor one [`unread::scan`] reads as a definition.
//!
//! What remains are calls of other names, on any receiver, and plain
//! expressions: the rules read the same in a block evaluated on an object
//! they cannot name (`o.instance_eval { local(1) }`), where a call proves
//! no definition either. A method such a call reaches that defines on its
//! caller is a class macro, recorded where it is written (B7), as it is
//! for every caller. Inert code is therefore recorded as nothing; any
//! other code keeps the blunt reading.

use std::time::Duration;

use tree_sitter::Node;

use super::super::unread;

/// Longest a literal's parse may take: past it the code is not judged.
const BUDGET: Duration = Duration::from_millis(200);

/// Node kinds that define or reopen something wherever they run.
const DEFINITIONS: &[&str] = &[
    "method",
    "singleton_method",
    "class",
    "singleton_class",
    "module",
    "alias",
    "undef",
];

/// Names that define, remove, evaluate, send by name, load or reach a
/// binding: one of them anywhere in the code, called or not, makes it not
/// inert.
const DENIED: &[&str] = &[
    "include",
    "prepend",
    "extend",
    "attr_reader",
    "attr_writer",
    "attr_accessor",
    "attr",
    "alias_method",
    "define_method",
    "define_singleton_method",
    "delegate",
    "scope",
    "module_function",
    "private",
    "protected",
    "public",
    "private_class_method",
    "public_class_method",
    "private_constant",
    "remove_method",
    "undef_method",
    "remove_const",
    "const_set",
    "class_variable_set",
    "eval",
    "class_eval",
    "class_exec",
    "module_eval",
    "module_exec",
    "instance_eval",
    "instance_exec",
    "send",
    "__send__",
    "public_send",
    "method_missing",
    "respond_to_missing?",
    "binding",
    "require",
    "require_relative",
    "require_dependency",
    "load",
    "autoload",
    "refine",
    "using",
    "extend_object",
    "append_features",
    "prepend_features",
    "included",
    "extended",
    "prepended",
    "inherited",
    "method_added",
    "singleton_method_added",
    "Class",
    "Module",
    "Struct",
    "Data",
    "Object",
    "BasicObject",
    "Kernel",
    "ObjectSpace",
    "TOPLEVEL_BINDING",
];

/// Whether `code`, the text of one literal with each interpolation
/// replaced by [`unread::INTERPOLATED`], defines nothing when any `self`
/// runs it.
pub(super) fn inert(code: &str) -> bool {
    if code.contains(unread::INTERPOLATED) || !unread::scan(code).is_empty() {
        return false;
    }
    let Some(tree) = super::super::extract::parse(code.as_bytes(), BUDGET) else {
        return false;
    };
    let root = tree.root_node();
    !root.has_error() && all_inert(root, code.as_bytes())
}

/// Whether no node under `node` is a definition, a constant assignment or
/// a denied word; iterative, so no nesting exhausts the stack.
fn all_inert(node: Node, src: &[u8]) -> bool {
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if DEFINITIONS.contains(&kind) || node.is_missing() {
            return false;
        }
        if matches!(kind, "assignment" | "operator_assignment")
            && node
                .child_by_field_name("left")
                .is_some_and(|left| matches!(left.kind(), "constant" | "scope_resolution"))
        {
            return false;
        }
        if matches!(kind, "identifier" | "constant" | "simple_symbol")
            && let Ok(text) = node.utf8_text(src)
            && DENIED.contains(&text.trim_start_matches(':'))
        {
            return false;
        }
        let mut walk = node.walk();
        stack.extend(node.children(&mut walk));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::inert;

    /// A Redis script is Lua, which Ruby's grammar reads as calls: it
    /// defines nothing, so its `eval` changes no class.
    #[test]
    fn c10_ruby_a_literal_that_only_calls_is_inert() {
        // #111
        for code in [
            "local version = redis.call('INCR', KEYS[2])\nfor _, id in ipairs(ARGV) do\n  redis.call('ZADD', KEYS[1], 'NX', version, id)\nend\n",
            "if redis.call('GET', KEYS[1]) == ARGV[1] then return redis.call('DEL', KEYS[1]) end",
            "1 + 2",
        ] {
            assert!(inert(code), "{code}");
        }
    }

    /// Any definition, constant, mixin, evaluation, send, load or
    /// interpolation, and text the grammar cannot read, leaves the code
    /// not inert: each may change a class the rules do not name.
    #[test]
    fn c10_ruby_a_literal_that_may_define_is_not_inert() {
        // #111, R7-03
        for code in [
            "def helper; end",
            "def self.helper; end",
            "class Foo; end",
            "module Foo; end",
            "class << self; end",
            "Foo = 1",
            "Foo::Bar ||= 1",
            "Foo &&= 1",
            "Foo += 1",
            "alias a b",
            "undef a",
            "Object.prepend(P)",
            "self.class.include(P)",
            "attr_reader :a",
            "x.define_method(:a) { }",
            "eval(code)",
            "x.instance_eval { 1 }",
            "send(:define_method, :a)",
            "x.public_send(name)",
            "require 'foo'",
            "Class.new(Base)",
            "Struct.new(:a)",
            "binding.local_variable_get(:a)",
            "private :a",
            "x.const_set(:A, 1)",
            "call(\u{1})",
            "redis.call(",
            "x = (1 + ]",
        ] {
            assert!(!inert(code), "{code}");
        }
    }
}
