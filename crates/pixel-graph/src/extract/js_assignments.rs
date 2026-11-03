// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! JavaScript assignment syntax, isolated from callable symbols and resolution.
//!
//! Coordinates are half-open UTF-8 byte ranges in the file's recorded blob.
//! Context indices describe AST containment, not runtime environments. Declaration
//! patterns cover variables and parameters only: absence never proves an unbound
//! name. Property reads/chained assignments describe possible aliases, never
//! object identity, alias lifetime, execution order or the receiver of `this`.

use serde::{Deserialize, Serialize};
use tree_sitter::Node;

/// A half-open byte range in one source version.
pub type Span = [usize; 2];

/// Syntax evidence that no call resolver or symbol lookup consumes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    pub parsed: bool,
    pub truncated: bool,
    pub contexts: Vec<Context>,
    pub bindings: Vec<Binding>,
    pub assignments: Vec<Assignment>,
}

/// An AST container; its vector index is its identity, even with coincident spans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    pub kind: String,
    pub span: Span,
    pub parent: Option<usize>,
}

/// A declaration pattern, without resolving references or interpreting destructuring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub kind: String,
    pub pattern: Expression,
    pub context: usize,
}

/// An expression's coordinates and, where literal, its written property access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expression {
    pub kind: String,
    pub span: Span,
    /// Written `(receiver, property)` for `identifier.name` / `this.name` only.
    pub member: Option<(String, String)>,
}

/// A write expression, including updates/deletes that invalidate a lasting-alias assumption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub kind: String,
    pub span: Span,
    pub context: usize,
    pub target: Expression,
    /// A chained assignment points to its nested assignment's span, not a resolved callee.
    pub value: Option<Expression>,
}

fn span(node: Node<'_>) -> Span {
    [node.start_byte(), node.end_byte()]
}

fn expression(node: Node<'_>, src: &[u8]) -> Expression {
    let member = (|| {
        if node.kind() != "member_expression" {
            return None;
        }
        let object = node.child_by_field_name("object")?;
        let property = node.child_by_field_name("property")?;
        if !matches!(object.kind(), "identifier" | "this")
            || property.kind() != "property_identifier"
        {
            return None;
        }
        Some((
            object.utf8_text(src).ok()?.into(),
            property.utf8_text(src).ok()?.into(),
        ))
    })();
    Expression {
        kind: node.kind().into(),
        span: span(node),
        member,
    }
}

fn is_context(kind: &str) -> bool {
    matches!(
        kind,
        "program"
            | "statement_block"
            | "function_declaration"
            | "function_expression"
            | "generator_function"
            | "generator_function_declaration"
            | "arrow_function"
            | "method_definition"
            | "for_statement"
            | "for_in_statement"
            | "catch_clause"
            | "class_body"
            | "switch_body"
    )
}

pub(super) fn collect(root: Node<'_>, src: &[u8]) -> Facts {
    if root.has_error() {
        return Facts::default();
    }
    let mut facts = Facts {
        parsed: true,
        ..Facts::default()
    };
    let mut pending = vec![(root, None, 0)];
    while let Some((node, mut context, depth)) = pending.pop() {
        if super::too_deep(depth) {
            facts.truncated = true;
            continue;
        }
        if is_context(node.kind()) {
            let index = facts.contexts.len();
            facts.contexts.push(Context {
                kind: node.kind().into(),
                span: span(node),
                parent: context,
            });
            context = Some(index);
        }
        let context_id = context.expect("JavaScript syntax has a program context");
        if node.kind() == "variable_declarator"
            && let Some(pattern) = node.child_by_field_name("name")
            && let Some(keyword) = node.parent().and_then(|p| p.child(0))
        {
            facts.bindings.push(Binding {
                kind: keyword.kind().into(),
                pattern: expression(pattern, src),
                context: context_id,
            });
        }
        if node.kind() == "formal_parameters" {
            let mut cursor = node.walk();
            for parameter in node
                .named_children(&mut cursor)
                .filter(|n| n.kind() != "comment")
            {
                facts.bindings.push(Binding {
                    kind: "parameter".into(),
                    pattern: expression(parameter, src),
                    context: context_id,
                });
            }
        }
        if node.kind() == "arrow_function"
            && let Some(parameter) = node.child_by_field_name("parameter")
        {
            facts.bindings.push(Binding {
                kind: "parameter".into(),
                pattern: expression(parameter, src),
                context: context_id,
            });
        }
        let writes = matches!(
            node.kind(),
            "assignment_expression" | "augmented_assignment_expression" | "update_expression"
        ) || (node.kind() == "unary_expression"
            && node
                .child_by_field_name("operator")
                .is_some_and(|op| op.kind() == "delete"));
        if writes
            && let Some(target) = node
                .child_by_field_name("left")
                .or_else(|| node.child_by_field_name("argument"))
        {
            facts.assignments.push(Assignment {
                kind: node.kind().into(),
                span: span(node),
                context: context_id,
                target: expression(target, src),
                value: node
                    .child_by_field_name("right")
                    .map(|n| expression(n, src)),
            });
        }
        // Reverse indexing keeps source order without allocating a child vector.
        for index in (0..node.named_child_count()).rev() {
            let child = node
                .named_child(u32::try_from(index).expect("tree-sitter child indices fit u32"))
                .expect("index is within the named child count");
            pending.push((child, context, depth + 1));
        }
    }
    facts
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_depth_cut_should_be_explicit_in_the_facts() {
        // Exercise the iterative collector independently of the legacy recursive symbol walker.
        let source = format!("{}a.read=b.read;{}", "{\n".repeat(520), "}\n".repeat(520));
        let tree = crate::extract::parse_file("app.js", source.as_bytes()).unwrap();
        let facts = super::collect(tree.root_node(), source.as_bytes());
        assert!(facts.parsed);
        assert!(facts.truncated);
        assert!(
            facts.assignments.is_empty(),
            "the only write is beyond the traversal bound"
        );
    }
}
