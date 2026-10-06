// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Methods Ruby and Rails define without a `def`: `attr_reader`,
//! `attr_writer`, `attr_accessor`, `alias_method` (and the `alias` keyword),
//! `delegate` and the Active Record `scope`.
//!
//! Each literal declaration becomes a method symbol of the class or module
//! whose body holds it, spanning the declaration itself, so navigation to the
//! generated method lands on the line that creates it. Only literal names are
//! read: a splat, a variable or an interpolated string names nothing the
//! extractor can know, and the declaration then generates no symbol.
//!
//! An alias and a delegator also reference the method they forward to on
//! their own owner (`alias_method :full, :name` → `#name`, `delegate :email,
//! to: :user` → `#user`), resolved by the owner-relative rule of
//! [`super::ruby_callbacks::ReferenceKind::Alias`]. A delegator never links
//! to the delegate's own method (`User#email`): the type `user` returns is
//! not known to the graph.
//! <https://docs.ruby-lang.org/en/master/Module.html#method-i-attr_accessor>
//! <https://api.rubyonrails.org/classes/Module.html#method-i-delegate>
//! <https://api.rubyonrails.org/classes/ActiveRecord/Scoping/Named/ClassMethods.html#method-i-scope>

use tree_sitter::Node;

use super::{RubyDefs, Walker, each_child, field_text};
use crate::store::SymbolKind;

/// `arg_of` of an alias's reference to the method it copies. It starts with
/// `:`, which no Ruby method name does, so an ordinary argument passed to a
/// method called `alias_method` never takes the alias rule.
pub(crate) const ALIAS_REFERENCE: &str = ":alias";
/// `arg_of` of a delegator's reference to its `to:` method.
pub(crate) const DELEGATE_REFERENCE: &str = ":delegate";

/// The generated method names a declaring `call` defines, with the method it
/// forwards to on the same owner, if any.
struct Generated {
    names: Vec<String>,
    /// Separator of the generated names: `#` for an instance method, `.` for
    /// a method of the class itself (`scope`, or an accessor in `class <<
    /// self`).
    separator: char,
    /// `(arg_of, method)` of the reference every generated name carries.
    forwards_to: Option<(&'static str, String)>,
}

/// Record the methods a receiver-less declaration `call` of `method` defines
/// in the current class or module body. `defs` is how a `def` written at that
/// point is qualified, which an accessor follows. True iff it generated at
/// least one method.
pub(super) fn walk_declaration(w: &mut Walker, call: Node, method: &str, defs: RubyDefs) -> bool {
    if w.stack.is_empty() || !in_owner_body(call) {
        return false;
    }
    let Some(args) = call.child_by_field_name("arguments") else {
        return false;
    };
    let instance = match defs {
        RubyDefs::Instance => '#',
        RubyDefs::Singleton => '.',
    };
    let generated = match method {
        "attr_reader" => accessors(w, args, instance, &[""]),
        "attr_writer" => accessors(w, args, instance, &["="]),
        "attr_accessor" => accessors(w, args, instance, &["", "="]),
        "alias_method" => alias_method(w, args, instance),
        "delegate" => delegate(w, args, instance),
        // A scope is a class method of the model, whatever the frame.
        "scope" => scope(w, args),
        _ => None,
    };
    generated.is_some_and(|generated| push(w, call, generated))
}

/// `alias new old` (the keyword): `new` is a copy of `old` on the same owner.
/// Global-variable aliases (`alias $new $old`) define no method.
pub(super) fn walk_alias_keyword(w: &mut Walker, node: Node, defs: RubyDefs) {
    if w.stack.is_empty() || !in_owner_body(node) {
        return;
    }
    let (Some(new), Some(old)) = (
        node.child_by_field_name("name")
            .and_then(|n| method_name(w, n)),
        node.child_by_field_name("alias")
            .and_then(|n| method_name(w, n)),
    ) else {
        return;
    };
    let separator = match defs {
        RubyDefs::Instance => '#',
        RubyDefs::Singleton => '.',
    };
    push(
        w,
        node,
        Generated {
            names: vec![new],
            separator,
            forwards_to: Some((ALIAS_REFERENCE, old)),
        },
    );
}

/// True iff `node` sits in a class, module or `class << self` body, directly
/// or through blocks (`included do`), not in a method body.
fn in_owner_body(node: Node) -> bool {
    std::iter::successors(node.parent(), Node::parent)
        .find(|n| {
            matches!(
                n.kind(),
                "class" | "module" | "singleton_class" | "method" | "singleton_method"
            )
        })
        .is_some_and(|n| matches!(n.kind(), "class" | "module" | "singleton_class"))
}

fn accessors(w: &Walker, args: Node, separator: char, suffixes: &[&str]) -> Option<Generated> {
    let mut names = Vec::new();
    for arg in positional(args) {
        if let Some(name) = literal_name(w, arg) {
            for suffix in suffixes {
                names.push(format!("{name}{suffix}"));
            }
        }
    }
    Some(Generated {
        names,
        separator,
        forwards_to: None,
    })
}

fn alias_method(w: &Walker, args: Node, separator: char) -> Option<Generated> {
    let [new, old] = positional(args)[..] else {
        return None;
    };
    Some(Generated {
        names: vec![literal_name(w, new)?],
        separator,
        forwards_to: Some((ALIAS_REFERENCE, literal_name(w, old)?)),
    })
}

/// `delegate :a, :b, to: :target, prefix: true | :custom`. A dynamic `to:`
/// still names the delegators unless a `prefix: true` needs it; a dynamic
/// prefix names nothing. `to:` an instance variable, a constant or `:class`
/// forwards to no method of the owner, so no reference is recorded.
fn delegate(w: &Walker, args: Node, separator: char) -> Option<Generated> {
    let mut target: Option<Option<String>> = None;
    let mut prefix: Option<Prefix> = None;
    for arg in each_child(args).into_iter().filter(|a| a.kind() == "pair") {
        let Some(key) = field_text(w, arg, "key") else {
            continue;
        };
        let value = arg.child_by_field_name("value")?;
        match key.trim_start_matches(':').trim_end_matches(':') {
            "to" => target = Some(literal_name(w, value)),
            "prefix" => {
                prefix = Some(match value.kind() {
                    "true" => Prefix::Target,
                    "false" | "nil" => Prefix::None,
                    _ => Prefix::Custom(literal_name(w, value)?),
                });
            }
            _ => {}
        }
    }
    // Rails raises without `to:`; such a call defines nothing.
    let target = target?;
    let prefix = match prefix.unwrap_or(Prefix::None) {
        Prefix::None => String::new(),
        Prefix::Target => format!("{}_", target.clone()?),
        Prefix::Custom(custom) => format!("{custom}_"),
    };
    let names = positional(args)
        .into_iter()
        .filter_map(|arg| literal_name(w, arg))
        .map(|name| format!("{prefix}{name}"))
        .collect();
    let forwards_to = target
        .filter(|t| is_method_name(t) && t != "class")
        .map(|t| (DELEGATE_REFERENCE, t));
    Some(Generated {
        names,
        separator,
        forwards_to,
    })
}

enum Prefix {
    None,
    Target,
    Custom(String),
}

fn scope(w: &Walker, args: Node) -> Option<Generated> {
    let first = *positional(args).first()?;
    Some(Generated {
        names: vec![literal_name(w, first)?],
        separator: '.',
        forwards_to: None,
    })
}

/// The arguments that are not `key: value` pairs, block arguments or splats'
/// expansions; a splat itself is kept and later read as no name.
fn positional(args: Node) -> Vec<Node> {
    each_child(args)
        .into_iter()
        .filter(|a| a.is_named() && !matches!(a.kind(), "pair" | "block_argument" | "comment"))
        .collect()
}

/// The text of a literal method name: `:name`, `"name"` or `'name'`, without
/// interpolation. `None` for anything else (a variable, a splat, `:"a#{b}"`).
fn literal_name(w: &Walker, node: Node) -> Option<String> {
    let name = match node.kind() {
        "simple_symbol" => w.text(node).get(1..)?.to_string(),
        "string" => {
            let mut text = String::new();
            for part in each_child(node) {
                match part.kind() {
                    "string_content" => text.push_str(&w.text(part)),
                    "interpolation" | "escape_sequence" => return None,
                    _ => {}
                }
            }
            text
        }
        _ => return None,
    };
    (!name.is_empty()).then_some(name)
}

/// A method name as the `alias` keyword spells it: an identifier, a constant,
/// an operator, a setter or a symbol; `None` for a global variable.
fn method_name(w: &Walker, node: Node) -> Option<String> {
    match node.kind() {
        "simple_symbol" | "string" => literal_name(w, node),
        "global_variable" => None,
        _ => Some(w.text(node)),
    }
}

/// True iff `name` reads as a method of the owner: a plain identifier, maybe
/// ending in `?` or `!` (not `@ivar`, `@@cvar`, `$global` or `Constant`).
fn is_method_name(name: &str) -> bool {
    let body = name.trim_end_matches(['?', '!']);
    body.chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && body.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Push the generated methods, each with its own reference to the method it
/// forwards to. True iff there was at least one.
fn push(w: &mut Walker, node: Node, generated: Generated) -> bool {
    let owner = w.stack.join("::");
    let any = !generated.names.is_empty();
    for name in generated.names {
        let qualified = format!("{owner}{}{name}", generated.separator);
        let index = w.symbols.len();
        w.generated.push(index);
        w.push_symbol(name, qualified, SymbolKind::Method, node);
        if let Some((arg_of, target)) = &generated.forwards_to {
            w.push_reference(target.clone(), node, Some((*arg_of).to_string()));
            if let Some(reference) = w.references.last_mut() {
                reference.enclosing_index = Some(index);
            }
        }
    }
    any
}

/// Drop the generated symbols another definition of the same method in the
/// file overrides: a `def` of that name wins whatever the order, being the
/// body a reader navigates to, and of two generated ones the later wins, as
/// the later definition is the one Ruby keeps. The store keys a symbol by
/// its file, qualified name and kind, so two such rows would replace each
/// other there and leave the references of the first pointing at nothing.
pub(super) fn drop_overridden(w: &mut Walker) {
    if w.generated.is_empty() {
        return;
    }
    let generated: std::collections::HashSet<usize> = w.generated.iter().copied().collect();
    let key = |s: &super::RawSymbol| (s.qualified.clone(), s.kind);
    let mut drop = vec![false; w.symbols.len()];
    for (i, symbol) in w.symbols.iter().enumerate() {
        if !generated.contains(&i) {
            continue;
        }
        drop[i] = w.symbols.iter().enumerate().any(|(j, other)| {
            j != i && key(other) == key(symbol) && (!generated.contains(&j) || j > i)
        });
    }
    // Reindex the references placed on a generated symbol; one whose symbol
    // was dropped falls back to the smallest enclosing symbol.
    let mut new_index = Vec::with_capacity(drop.len());
    let mut next = 0;
    for dropped in &drop {
        new_index.push((!dropped).then_some(next));
        next += usize::from(!dropped);
    }
    for reference in &mut w.references {
        reference.enclosing_index = reference.enclosing_index.and_then(|i| new_index[i]);
    }
    let mut index = 0;
    w.symbols.retain(|_| {
        let keep = !drop[index];
        index += 1;
        keep
    });
    w.generated.clear();
}
