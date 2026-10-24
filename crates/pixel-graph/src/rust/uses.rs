// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The leaves of a `use` declaration (v1's `rust_use_leaves`,
//! `extract.rs:1288-1391`): one per path it names, with the name it binds.
//!
//! One leaf per path, not one per statement: `use crate::{left::push,
//! right::publish};` names two items, and `use crate::push::{a::x, y};`
//! names `x` in `push::a`, not in `push`.

use tree_sitter::Node;

use super::Leaf;
use super::types::{MAX_NESTING, compact, line, named_children, segments, text};

/// One path a `use` names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UseLeaf {
    /// The path as v1 wrote it, whitespace removed: `crate::a::b`,
    /// `crate::a::self`, `std::collections::*`. The edge's target.
    pub target: String,
    /// The path resolution follows: without a trailing `self` or `*`.
    pub path: Vec<String>,
    pub leaf: Leaf,
    /// The name bound in the scope; only the alias for `a::b as c` (v1:
    /// "`use a::push as leased;` makes `leased()` a call to `push` and
    /// leaves `push()` unbound"); `None` for a glob or `as _`.
    pub local: Option<String>,
    pub line: u32,
}

/// The leaves of a `use` argument, in the order written; `None` when its
/// groups nest past [`MAX_NESTING`], which leaves the whole `use` unread
/// (D17). An explicit stack: no nesting costs the walker's stack.
pub fn leaves(src: &[u8], argument: Node) -> Option<Vec<UseLeaf>> {
    let mut out = Vec::new();
    let mut pending = vec![(argument, Vec::new(), String::new(), 0)];
    while let Some((node, prefix, prefix_text, depth)) = pending.pop() {
        match node.kind() {
            "use_as_clause" => {
                let inner = node
                    .child_by_field_name("path")
                    .and_then(|path| leaf(src, path, &prefix, &prefix_text));
                if let (Some(leaf), Some(alias)) = (inner, node.child_by_field_name("alias")) {
                    let alias = text(src, alias);
                    out.push(UseLeaf {
                        local: (alias != "_" && leaf.local.is_some()).then_some(alias),
                        ..leaf
                    });
                }
            }
            "scoped_use_list" => {
                let (prefix, prefix_text) = match node.child_by_field_name("path") {
                    Some(path) => (
                        joined(&prefix, &segments(src, path).unwrap_or_default()),
                        join_text(&prefix_text, &compact(src, path)),
                    ),
                    // `use ::{a::b}`: the leading `::` names an extern crate,
                    // never a local module (D40), as in `::a::b`.
                    None if text(src, node).trim_start().starts_with("::") => (
                        joined(&prefix, &[String::new()]),
                        format!("{prefix_text}::"),
                    ),
                    None => (prefix, prefix_text),
                };
                if let Some(list) = node.child_by_field_name("list") {
                    pending.push((list, prefix, prefix_text, depth));
                }
            }
            "use_list" => {
                if depth >= MAX_NESTING {
                    return None;
                }
                for child in named_children(node).into_iter().rev() {
                    pending.push((child, prefix.clone(), prefix_text.clone(), depth + 1));
                }
            }
            _ => out.extend(leaf(src, node, &prefix, &prefix_text)),
        }
    }
    Some(out)
}

fn join_text(prefix: &str, rest: &str) -> String {
    if prefix.is_empty() || prefix.ends_with("::") {
        format!("{prefix}{rest}")
    } else {
        format!("{prefix}::{rest}")
    }
}

fn joined(prefix: &[String], rest: &[String]) -> Vec<String> {
    prefix.iter().chain(rest).cloned().collect()
}

/// The leaf a path of a `use` names under `prefix`, or a glob; `None` for a
/// group or a shape the rules do not read.
fn leaf(src: &[u8], node: Node, prefix: &[String], prefix_text: &str) -> Option<UseLeaf> {
    let target = || join_text(prefix_text, &compact(src, node));
    match node.kind() {
        "identifier" | "type_identifier" => {
            let name = text(src, node);
            Some(UseLeaf {
                target: target(),
                path: joined(prefix, std::slice::from_ref(&name)),
                leaf: Leaf::Name,
                local: Some(name),
                line: line(node),
            })
        }
        "self" => Some(UseLeaf {
            target: target(),
            path: prefix.to_vec(),
            leaf: Leaf::SelfModule,
            local: prefix.last().cloned(),
            line: line(node),
        }),
        "crate" | "super" => Some(UseLeaf {
            target: target(),
            path: joined(prefix, &[text(src, node)]),
            leaf: Leaf::Name,
            local: None,
            line: line(node),
        }),
        "use_wildcard" => {
            let inner = named_children(node)
                .into_iter()
                .find_map(|child| segments(src, child))
                .unwrap_or_default();
            Some(UseLeaf {
                target: target(),
                path: joined(prefix, &inner),
                leaf: Leaf::Glob,
                local: None,
                line: line(node),
            })
        }
        "scoped_identifier" => {
            let path = segments(src, node)?;
            let full = joined(prefix, &path);
            let name = node.child_by_field_name("name");
            let (path, leaf, local) = match name.map(|n| n.kind()) {
                Some("self") => {
                    let module = full[..full.len() - 1].to_vec();
                    let local = module.last().cloned();
                    (module, Leaf::SelfModule, local)
                }
                Some("identifier" | "type_identifier") => {
                    let local = full.last().cloned();
                    (full, Leaf::Name, local)
                }
                _ => (full, Leaf::Name, None),
            };
            Some(UseLeaf {
                target: target(),
                path,
                leaf,
                local,
                line: name.map_or_else(|| line(node), line),
            })
        }
        _ => None,
    }
}
