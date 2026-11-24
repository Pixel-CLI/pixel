// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The graph's same-file tier can bind a call on a shadowing parameter to
//! the outer callable. Intersect its edges with bare calls in the lexical
//! scope of the indexed declaration, using the very bytes the graph indexed.

use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;

use pixel_graph::store::{FileRow, SymbolRow};
use pixel_index::index::{MAX_FILE_BYTES, read_regular_bounded};
use tree_sitter::Node;

pub(super) fn sites(
    root: &Path,
    file: &FileRow,
    target: &SymbolRow,
    deadline: Instant,
) -> Option<HashSet<u32>> {
    let bytes = read_regular_bounded(&root.join(&file.path), MAX_FILE_BYTES).ok()?;
    if pixel_graph::build::content_oid(&bytes) != file.blob_oid || Instant::now() >= deadline {
        return None;
    }
    let tree = pixel_graph::extract::parse_file(&file.path, &bytes)?;
    let mut declaration = None;
    let mut shadows = Vec::new();
    let mut calls = Vec::new();
    for (at, name) in std::str::from_utf8(&bytes)
        .ok()?
        .match_indices(&target.name)
    {
        if Instant::now() >= deadline {
            return None;
        }
        let end = at + name.len();
        let node = tree.root_node().named_descendant_for_byte_range(at, end)?;
        if node.byte_range() != (at..end)
            || !matches!(
                node.kind(),
                "identifier" | "shorthand_property_identifier_pattern"
            )
        {
            continue;
        }
        let parent = node.parent()?;
        let callee = match parent.kind() {
            "call_expression" => parent.child_by_field_name("function"),
            "new_expression" => parent.child_by_field_name("constructor"),
            _ => None,
        };
        if callee == Some(node) {
            calls.push((at, u32::try_from(node.start_position().row + 1).ok()?));
        } else if let Some((binder, scope)) = binding(node) {
            if binder.child_by_field_name("name") == Some(node)
                && matches!(
                    binder.kind(),
                    "variable_declarator"
                        | "function_declaration"
                        | "generator_function_declaration"
                )
                && binder.start_position().row + 1 == target.start_line as usize
            {
                if declaration.replace(scope.byte_range()).is_some() {
                    return None;
                }
            } else {
                shadows.push(scope.byte_range());
            }
        }
    }
    let scope = declaration?;
    if shadows.contains(&scope) {
        return None;
    }
    shadows.retain(|shadow| scope.start <= shadow.start && shadow.end <= scope.end);
    // Calls arrive in source order. Sweep the shadows once, including
    // nested or overlapping ranges, without a shadow scan per call.
    shadows.sort_unstable_by_key(|shadow| shadow.start);
    let mut shadows = shadows.into_iter().peekable();
    let mut shadow_end = 0;
    Some(
        calls
            .into_iter()
            .filter_map(|(byte, line)| {
                while let Some(shadow) = shadows.next_if(|shadow| shadow.start <= byte) {
                    shadow_end = shadow_end.max(shadow.end);
                }
                (scope.contains(&byte) && byte >= shadow_end).then_some(line)
            })
            .collect(),
    )
}

/// The binding node and the extent where a spelling in its pattern shadows
/// an outer declaration. References on the right of a parameter default
/// read the enclosing binding.
fn binding(node: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    fn ancestors(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
        std::iter::successors(Some(node), Node::parent)
    }
    for binder in ancestors(node.parent()?) {
        let field = match binder.kind() {
            "formal_parameters" => return Some((binder, binder.parent()?)),
            "assignment_pattern" | "object_assignment_pattern" => {
                if !contains(binder.child_by_field_name("left")?, node) {
                    return None;
                }
                continue;
            }
            "required_parameter" | "optional_parameter" => {
                let pattern = binder
                    .child_by_field_name("pattern")
                    .or_else(|| binder.child_by_field_name("name"))?;
                if !contains(pattern, node) {
                    return None;
                }
                continue;
            }
            "arrow_function" | "catch_clause" => "parameter",
            "for_in_statement" if binder.child_by_field_name("kind").is_some() => "left",
            "variable_declarator"
            | "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "generator_function"
            | "class_declaration"
            | "class" => "name",
            "statement_block" | "program" => break,
            _ => continue,
        };
        let Some(pattern) = binder.child_by_field_name(field) else {
            continue;
        };
        if !contains(pattern, node) {
            continue;
        }
        if matches!(
            binder.kind(),
            "arrow_function"
                | "catch_clause"
                | "function_expression"
                | "generator_function"
                | "class"
        ) {
            return Some((binder, binder));
        }
        let var = binder.parent()?.kind() == "variable_declaration"
            || binder
                .child_by_field_name("kind")
                .is_some_and(|kind| kind.kind() == "var");
        if binder.kind() == "for_in_statement" && !var {
            return Some((binder, binder));
        }
        let scope = ancestors(binder.parent()?).find(|a| {
            matches!(
                a.kind(),
                "program"
                    | "function_declaration"
                    | "function_expression"
                    | "generator_function"
                    | "generator_function_declaration"
                    | "arrow_function"
                    | "method_definition"
            ) || (!var
                && matches!(
                    a.kind(),
                    "statement_block" | "switch_body" | "for_statement" | "for_in_statement"
                ))
        })?;
        // A body-local var is hoisted within the body. Default parameters
        // are evaluated in the surrounding environment before that body.
        let scope = if var {
            scope.child_by_field_name("body").unwrap_or(scope)
        } else {
            scope
        };
        return Some((binder, scope));
    }
    None
}

fn contains(outer: Node<'_>, inner: Node<'_>) -> bool {
    outer.start_byte() <= inner.start_byte() && inner.end_byte() <= outer.end_byte()
}
