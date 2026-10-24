// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The loads a file makes: `require` and its family, read where they are
//! written, and in a region the grammar could not parse.

use pixel_model::EdgeKind;
use tree_sitter::Node;

use super::locals::Gate;
use super::{MAX_DEPTH, REQUIRE_METHODS, Walker, children, line};
use crate::lang::FILE_OWNER;
use crate::ruby::CONDITIONAL;

impl Walker<'_> {
    /// The `require`s of a region the grammar could not parse: a load
    /// needs no enclosing structure to be read, unlike a definition's name
    /// or a call's `self`.
    pub(super) fn imports_in(&mut self, node: Node, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        if node.kind() == "call"
            && node.child_by_field_name("receiver").is_none()
            && let Some(method) = self.field_text(node, "method")
            && REQUIRE_METHODS.contains(&method.as_str())
        {
            self.import(node, &method);
        }
        for (child, _) in children(node) {
            self.imports_in(child, depth + 1);
        }
    }

    /// `require "x"` and its family: an import of the literal spec, with the
    /// path `require` resolution reads.
    pub(super) fn import(&mut self, call: Node, method: &str) {
        let Some(spec) = self.first_string_argument(call) else {
            return;
        };
        let mut context = self.context(&spec, method);
        context.extra = vec![require_path(method, &spec)];
        // Under a condition, in a block or in a method body, the load may
        // not run when the file does (R13).
        if self.branches > 0 || self.locals.gate() == Gate::Method {
            context.extra.push(CONDITIONAL.to_owned());
        }
        // A load is the file's, wherever it is written (the contract in
        // `ruby/mod.rs`).
        let file = Some(FILE_OWNER);
        self.push_site(EdgeKind::Import, line(call), file, spec, &context);
    }

    /// The literal text of the first argument when it is a string without
    /// interpolation; an escape sequence is skipped (v1
    /// `ruby_first_string_argument`).
    fn first_string_argument(&self, call: Node) -> Option<String> {
        let args = call.child_by_field_name("arguments")?;
        let mut cursor = args.walk();
        let first = args.named_children(&mut cursor).next()?;
        if first.kind() != "string" {
            return None;
        }
        let mut spec = String::new();
        let mut cursor = first.walk();
        for part in first.children(&mut cursor) {
            match part.kind() {
                "string_content" => spec.push_str(&self.text(part)),
                "interpolation" => return None,
                _ => {}
            }
        }
        (!spec.is_empty()).then_some(spec)
    }
}

/// What `require` resolution resolves for `spec` loaded through `method`:
/// `require_relative` names a file relative to the requiring one, kept as an
/// explicit `./` or `../` path; `require`, `require_dependency` and `load`
/// search the load path, kept bare. A load-path spec written as a relative
/// or absolute path (`require "./x"`) is relative to the process's working
/// directory, which the rules cannot know: `""`, which resolves to nothing
/// (v1 `ruby_require_path`).
pub(super) fn require_path(method: &str, spec: &str) -> String {
    let explicit = spec.starts_with("./") || spec.starts_with("../") || spec.starts_with('/');
    match (method, explicit) {
        ("require_relative", false) => format!("./{spec}"),
        ("require_relative", true) => spec.to_owned(),
        (_, false) => spec.to_owned(),
        (_, true) => String::new(),
    }
}

#[cfg(test)]
mod audited_contract_tests {
    use super::super::{PARSE_BUDGET, parse};
    use super::*;

    #[test]
    fn unread_imports_should_include_the_depth_boundary_but_not_its_children() {
        let source = b"require 'one'\nrequire 'two'";
        let tree = parse(source, PARSE_BUDGET).unwrap();
        for (depth, count) in [(MAX_DEPTH, 0), (MAX_DEPTH - 1, 2), (MAX_DEPTH + 1, 0)] {
            let mut walker = Walker::new(source);
            walker.imports_in(tree.root_node(), depth);
            assert_eq!(walker.sites.len(), count, "depth={depth}");
        }
        let mut walker = Walker::new(source);
        walker.imports_in(tree.root_node().named_child(0).unwrap(), MAX_DEPTH);
        assert_eq!(walker.sites.len(), 1);
    }
}
