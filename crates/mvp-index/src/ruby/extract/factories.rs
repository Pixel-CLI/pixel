// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! factory_bot's factories as the walk meets them (ADR-0041 point 7,
//! ADR-0038's factory row).
//!
//! In a `FactoryBot.define` block, `factory :name` (nested in another
//! factory or not) records its name, its class as written (`class:`), its
//! parent (the factory around it, or `parent:`), its aliases, the nesting
//! it is written in, and whether its block holds an `initialize_with`,
//! which builds anything ([`Fact::Factory`]). A factory in a
//! `FactoryBot.modify` block is recorded as changed: nothing it builds is
//! typed.

use tree_sitter::Node;

use super::{Walker, rspec};
use crate::ruby::Fact;

/// Records the factory `call` declares, when it is one.
pub(super) fn factory(w: &mut Walker, call: Node, method: &str) {
    if method != "factory" || call.child_by_field_name("receiver").is_some() {
        return;
    }
    let Some((defined, parent)) = around(w, call) else {
        return;
    };
    let args = rspec::arguments(call);
    let Some(name) = args.first().and_then(|arg| rspec::literal(w, *arg)) else {
        return;
    };
    let mut class = String::new();
    let mut parent = parent.unwrap_or_default();
    let mut aliases = Vec::new();
    let mut dynamic = !defined;
    for arg in &args[1..] {
        let pairs: Vec<Node> = match arg.kind() {
            "pair" => vec![*arg],
            "hash" => {
                let mut cursor = arg.walk();
                arg.named_children(&mut cursor).collect()
            }
            _ => {
                dynamic = true;
                continue;
            }
        };
        for pair in pairs {
            let key = w
                .field_text(pair, "key")
                .unwrap_or_default()
                .trim_end_matches(':')
                .trim_start_matches(':')
                .to_owned();
            let Some(value) = pair.child_by_field_name("value") else {
                dynamic = true;
                continue;
            };
            match key.as_str() {
                "class" => match value.kind() {
                    "constant" | "scope_resolution" => class = super::one_line(&w.text(value)),
                    _ => match rspec::literal(w, value) {
                        // A string is constantized at the top level.
                        Some(text) => class = format!("::{}", text.trim_start_matches("::")),
                        None => dynamic = true,
                    },
                },
                "parent" => match rspec::literal(w, value) {
                    Some(text) => parent = text,
                    None => dynamic = true,
                },
                "aliases" => {
                    let mut cursor = value.walk();
                    for element in value.named_children(&mut cursor) {
                        match rspec::literal(w, element) {
                            Some(text) => aliases.push(text),
                            None => dynamic = true,
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let initialized = dynamic
        || call
            .child_by_field_name("block")
            .is_some_and(|block| holds_initialize_with(w, block));
    let scope: Vec<String> = w.cref.iter().rev().cloned().collect();
    for alias in aliases {
        w.facts.push(Fact::Factory {
            name: alias,
            class: String::new(),
            parent: name.clone(),
            scope: scope.clone(),
            initialized: false,
        });
    }
    w.facts.push(Fact::Factory {
        name,
        class,
        parent,
        scope,
        initialized,
    });
}

/// Whether `call` is in a `FactoryBot.define` block (`Some(true)`) or a
/// `FactoryBot.modify` one (`Some(false)`), with the factory around it.
fn around(w: &Walker, call: Node) -> Option<(bool, Option<String>)> {
    let mut parent = None;
    let mut at = call;
    while let Some(up) = at.parent() {
        if matches!(up.kind(), "block" | "do_block")
            && let Some(outer) = up.parent().filter(|p| p.kind() == "call")
        {
            let method = w.field_text(outer, "method").unwrap_or_default();
            let receiver = outer.child_by_field_name("receiver");
            match (method.as_str(), receiver) {
                ("factory", None) if parent.is_none() => {
                    parent = rspec::arguments(outer)
                        .first()
                        .and_then(|arg| rspec::literal(w, *arg));
                    parent.as_ref()?;
                }
                ("define" | "modify", Some(r))
                    if matches!(
                        w.text(r).trim_start_matches("::"),
                        "FactoryBot" | "FactoryGirl"
                    ) =>
                {
                    return Some((method == "define", parent));
                }
                _ => {}
            }
        }
        if matches!(up.kind(), "method" | "class" | "module") {
            return None;
        }
        at = up;
    }
    None
}

/// Whether the subtree `node` calls `initialize_with` (in the factory, a
/// trait or a nested factory, which inherits it).
fn holds_initialize_with(w: &Walker, node: Node) -> bool {
    let mut stack = vec![node];
    while let Some(at) = stack.pop() {
        if at.kind() == "call" && w.field_text(at, "method").as_deref() == Some("initialize_with") {
            return true;
        }
        if at.kind() == "identifier" && w.text(at) == "initialize_with" {
            return true;
        }
        let mut cursor = at.walk();
        stack.extend(at.named_children(&mut cursor));
    }
    false
}
