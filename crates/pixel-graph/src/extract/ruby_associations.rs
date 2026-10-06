// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Active Record associations as relationships: a literal `belongs_to`,
//! `has_one`, `has_many` or `has_and_belongs_to_many` in a class body
//! references the model class it names, never a method. The class is the
//! literal `class_name:`, else the association name Rails classifies
//! (`has_many :line_items` → `LineItem`, `belongs_to :author` → `Author`).
//! A `polymorphic: true` association, or a `through:` one without
//! `class_name:`, has no single target and references nothing; a computed
//! name or `class_name:` neither.
//! <https://api.rubyonrails.org/classes/ActiveRecord/Associations/ClassMethods.html>

use tree_sitter::Node;

use super::ruby_routes::{camelize_path, singularize};
use super::{Walker, each_child, field_text};

/// `arg_of` prefix of an association's reference; the macro and the class
/// path follow (`:association has_many Billing::Invoice`).
pub(crate) const ASSOCIATION_REFERENCE: &str = ":association ";

const MACROS: &[&str] = &[
    "belongs_to",
    "has_one",
    "has_many",
    "has_and_belongs_to_many",
];

/// Record the model class a receiver-less association `call` references.
pub(super) fn walk_association(w: &mut Walker, call: Node, method: &str) {
    if !MACROS.contains(&method)
        || call.child_by_field_name("receiver").is_some()
        || !in_class_body(call)
    {
        return;
    }
    let Some(args) = call.child_by_field_name("arguments") else {
        return;
    };
    let children: Vec<Node> = each_child(args)
        .into_iter()
        .filter(Node::is_named)
        .collect();
    let Some(name) = children.first().and_then(|n| symbol(w, *n)) else {
        return;
    };
    let option = |key: &str| {
        children.iter().find_map(|arg| {
            (arg.kind() == "pair"
                && field_text(w, *arg, "key")
                    .is_some_and(|k| k.trim_start_matches(':').trim_end_matches(':') == key))
            .then(|| arg.child_by_field_name("value"))
            .flatten()
        })
    };
    if option("polymorphic").is_some_and(|v| v.kind() == "true") {
        return;
    }
    let class_name = match option("class_name") {
        Some(value) => match symbol(w, value) {
            Some(class_name) => class_name,
            None => return,
        },
        None if option("through").is_some() => return,
        None if matches!(method, "has_many" | "has_and_belongs_to_many") => {
            camelize_path(&singularize(&name))
        }
        None => camelize_path(&name),
    };
    let last = class_name
        .rsplit("::")
        .next()
        .unwrap_or(&class_name)
        .to_string();
    if last.is_empty() {
        return;
    }
    w.push_reference(
        last,
        call,
        Some(format!("{ASSOCIATION_REFERENCE}{method} {class_name}")),
    );
}

/// True iff `node` sits in a class body, directly or through blocks.
fn in_class_body(node: Node) -> bool {
    std::iter::successors(node.parent(), Node::parent)
        .find(|n| {
            matches!(
                n.kind(),
                "class" | "module" | "singleton_class" | "method" | "singleton_method"
            )
        })
        .is_some_and(|n| n.kind() == "class")
}

/// A literal symbol (`:a`, `:"a"`) or string without interpolation.
fn symbol(w: &Walker, node: Node) -> Option<String> {
    let text = match node.kind() {
        "simple_symbol" => w.text(node).get(1..)?.to_string(),
        "string" | "delimited_symbol" => {
            let mut out = String::new();
            for part in each_child(node) {
                match part.kind() {
                    "string_content" => out.push_str(&w.text(part)),
                    "interpolation" => return None,
                    _ => {}
                }
            }
            out
        }
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use crate::extract::extract_file;

    #[test]
    fn associations_should_record_their_class_path_from_plain_and_quoted_names() {
        let source = "class Order\n  has_many :\"line_items\"\n  belongs_to :customer\n  has_one :invoice, class_name: :\"Billing::Invoice\"\n  has_many :\"x#{y}\"\n  def build\n    has_many :nested\n  end\nend\nmodule Concern\n  has_many :in_module\nend\n";
        let fx = extract_file("app/models/order.rb", source.as_bytes()).unwrap();
        let refs: Vec<(String, Option<String>)> = fx
            .references
            .into_iter()
            .map(|r| (r.name, r.arg_of))
            .collect();
        let row = |name: &str, arg: &str| (name.to_string(), Some(arg.to_string()));
        assert_eq!(
            refs,
            [
                row("LineItem", ":association has_many LineItem"),
                row("Customer", ":association belongs_to Customer"),
                row("Invoice", ":association has_one Billing::Invoice"),
            ],
            "interpolated names, method bodies and modules reference nothing"
        );
    }
}
