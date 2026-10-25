// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! What the code says about a name where a site reads it: whether a local
//! or a parameter binds it, the type a binding states, and the generic
//! parameters in scope. Carried from v1's receiver typing
//! (`extract.rs:1459-1636`, rules Y-1 to Y-7), with three corrections:
//! a type keeps its whole path (`crate::git::Runner`, so resolution can
//! prove it), a pattern's binding positions are told from the paths in it
//! (`Some` in `Some(x)` binds nothing; `Foo { a }` binds `a`), and a
//! binding applies only where Rust puts it in scope (an `if let` binds in
//! its consequence, not its `else`; a `for` pattern in its body, not its
//! iterator; a match arm's pattern in its guard too).

use tree_sitter::Node;

use super::extract::Walker;
use super::types::{self, named_children, rooted_at, segments, text};
use super::{Receiver, TypeRef};

/// A local or parameter binds the name at the site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bound {
    /// Its binding states the type: an annotation, a constructor value;
    /// with the references an annotation writes around it, outermost first
    /// (`true` for `&mut`).
    Typed(Receiver, Vec<bool>),
    /// An annotation naming a generic parameter of the code around it, or
    /// a reference to one.
    Generic,
    /// A pattern, an unannotated parameter, a value of unknown type.
    Untyped,
}

/// The binding of `ident` that the code at `site` reads, searched outward
/// through each enclosing block (its `let`s that end before the site, the
/// last one winning), pattern and closure up to the function's parameters;
/// `None` when no local binds it, and it is an item's name.
pub fn local(w: &Walker, site: Node, ident: &str) -> Option<Bound> {
    let at = site.start_byte();
    let mut node = site;
    while let Some(parent) = node.parent() {
        match parent.kind() {
            "block" => {
                let mut bound = None;
                for child in named_children(parent) {
                    if child.kind() == "let_declaration"
                        && child.end_byte() <= at
                        && let Some(binding) = let_binding(w, child, ident)
                    {
                        bound = Some(binding);
                    }
                }
                if bound.is_some() {
                    return bound;
                }
            }
            "function_item" | "closure_expression" => {
                let params = parent.child_by_field_name("parameters");
                if let Some(binding) = params.and_then(|p| params_binding(w, p, ident)) {
                    return Some(binding);
                }
                if parent.kind() == "function_item" {
                    return None;
                }
            }
            "for_expression" => {
                if is_field(parent, "body", node) && field_binds(w, parent, "pattern", ident) {
                    return Some(Bound::Untyped);
                }
            }
            "match_arm" => {
                if field_binds(w, parent, "pattern", ident) {
                    return Some(Bound::Untyped);
                }
            }
            "if_expression" | "while_expression" => {
                if condition_binds(w, parent, node, site, ident) {
                    return Some(Bound::Untyped);
                }
            }
            kind if is_item(kind) => return None,
            _ => {}
        }
        node = parent;
    }
    None
}

/// Items end the search: a local of the function around an item is not in
/// scope inside it.
fn is_item(kind: &str) -> bool {
    matches!(
        kind,
        "source_file"
            | "mod_item"
            | "impl_item"
            | "trait_item"
            | "struct_item"
            | "enum_item"
            | "union_item"
            | "const_item"
            | "static_item"
            | "type_item"
            | "function_signature_item"
    )
}

fn is_field(parent: Node, field: &str, child: Node) -> bool {
    parent
        .child_by_field_name(field)
        .is_some_and(|c| c.id() == child.id())
}

fn field_binds(w: &Walker, parent: Node, field: &str, ident: &str) -> bool {
    parent
        .child_by_field_name(field)
        .is_some_and(|pattern| pattern_binds(w.src, pattern, ident))
}

/// `if let` and `while let`: the condition's patterns bind in the body, and
/// a `let` of a chain binds in the conditions after it.
fn condition_binds(w: &Walker, parent: Node, from: Node, site: Node, ident: &str) -> bool {
    let Some(condition) = parent.child_by_field_name("condition") else {
        return false;
    };
    let body = if parent.kind() == "if_expression" {
        "consequence"
    } else {
        "body"
    };
    let from_body = is_field(parent, body, from);
    let from_condition = condition.id() == from.id();
    let lets = match condition.kind() {
        "let_condition" => vec![condition],
        "let_chain" => named_children(condition)
            .into_iter()
            .filter(|c| c.kind() == "let_condition")
            .collect(),
        _ => Vec::new(),
    };
    lets.into_iter().any(|binding| {
        let before = binding.end_byte() <= site.start_byte();
        (from_body || (from_condition && before)) && field_binds(w, binding, "pattern", ident)
    })
}

/// `Some(binding)` when the `let` binds `ident`: typed when its pattern is
/// the name alone and its annotation or its value states a type.
fn let_binding(w: &Walker, declaration: Node, ident: &str) -> Option<Bound> {
    let pattern = declaration.child_by_field_name("pattern")?;
    if !pattern_binds(w.src, pattern, ident) {
        return None;
    }
    if pattern.kind() != "identifier" {
        return Some(Bound::Untyped);
    }
    if let Some(ty) = declaration.child_by_field_name("type") {
        return Some(typed(w, ty, declaration));
    }
    Some(
        declaration
            .child_by_field_name("value")
            .and_then(|value| constructed(w, value))
            .map_or(Bound::Untyped, |receiver| {
                Bound::Typed(receiver, Vec::new())
            }),
    )
}

/// The binding a function's or a closure's parameters give `ident`.
fn params_binding(w: &Walker, params: Node, ident: &str) -> Option<Bound> {
    for param in named_children(params) {
        let (pattern, ty) = match param.kind() {
            "parameter" => (
                param.child_by_field_name("pattern"),
                param.child_by_field_name("type"),
            ),
            "self_parameter" | "variadic_parameter" | "attribute_item" | "line_comment"
            | "block_comment" => continue,
            _ => (Some(param), None),
        };
        let Some(pattern) = pattern else { continue };
        if !pattern_binds(w.src, pattern, ident) {
            continue;
        }
        if pattern.kind() != "identifier" {
            return Some(Bound::Untyped);
        }
        return Some(ty.map_or(Bound::Untyped, |ty| typed(w, ty, params)));
    }
    None
}

/// A type annotation as a binding, read in the scope of `at`.
fn typed(w: &Walker, ty: Node, at: Node) -> Bound {
    let generics = generics_at(w.src, ty);
    let (refs, inner) = types::references(ty);
    if matches!(
        inner.kind(),
        "type_identifier" | "scoped_type_identifier" | "generic_type"
    ) && segments(w.src, inner).is_some_and(|path| rooted_at(&path, &generics))
    {
        return Bound::Generic;
    }
    let layers = types::layers(w.src, ty, &generics);
    if layers.is_empty() {
        Bound::Untyped
    } else {
        let receiver = Receiver::Type(TypeRef {
            scope: w.scope_at(at),
            layers,
        });
        Bound::Typed(receiver, refs)
    }
}

/// True when `ident` is bound by `pattern`: an identifier in a binding
/// position, or a struct pattern's shorthand field. The path of a tuple
/// struct or struct pattern (`Some` in `Some(x)`) and a match guard bind
/// nothing. An explicit stack: a pattern nested deep cannot overflow it.
pub fn pattern_binds(src: &[u8], pattern: Node, ident: &str) -> bool {
    let mut pending = vec![pattern];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "identifier" | "shorthand_field_identifier" => {
                if text(src, node) == ident {
                    return true;
                }
            }
            "scoped_identifier" | "field_identifier" | "type_identifier" => {}
            "match_pattern" | "tuple_struct_pattern" | "struct_pattern" => {
                let skipped = ["condition", "type"]
                    .iter()
                    .filter_map(|field| node.child_by_field_name(field))
                    .map(|c| c.id())
                    .collect::<Vec<_>>();
                pending.extend(
                    named_children(node)
                        .into_iter()
                        .filter(|c| !skipped.contains(&c.id())),
                );
            }
            _ => pending.extend(named_children(node)),
        }
    }
    false
}

/// The type a method call's receiver is when the code states it (v1's
/// `rust_receiver_type`): `self`, a local whose binding states a type, or a
/// constructor written as the receiver itself (`Runner::new(root).run()`).
pub fn receiver(w: &Walker, call: Node, value: Node) -> Receiver {
    match value.kind() {
        "self" => self_receiver(w, call),
        "identifier" => match local(w, call, &text(w.src, value)) {
            Some(Bound::Typed(receiver, _)) => receiver,
            _ => Receiver::Unknown,
        },
        "call_expression" | "struct_expression" | "try_expression" => {
            constructed(w, value).unwrap_or(Receiver::Unknown)
        }
        _ => Receiver::Unknown,
    }
}

/// `self` in the function around `call`: the impl's type, or the type a
/// typed `self` states (`self: Box<Self>` is a `Box`, whose own methods
/// come first).
fn self_receiver(w: &Walker, call: Node) -> Receiver {
    let function =
        std::iter::successors(call.parent(), |n| n.parent()).find(|n| n.kind() == "function_item");
    let params = function.and_then(|f| f.child_by_field_name("parameters"));
    let typed_self = params.and_then(|params| {
        let first = named_children(params).into_iter().find(|p| {
            !matches!(
                p.kind(),
                "attribute_item" | "line_comment" | "block_comment"
            )
        })?;
        let is_self = first.kind() == "parameter"
            && first
                .child_by_field_name("pattern")
                .is_some_and(|p| p.kind() == "self");
        let ty = first.child_by_field_name("type").filter(|_| is_self)?;
        Some(typed(w, ty, params))
    });
    match typed_self {
        Some(Bound::Typed(receiver, _)) => receiver,
        Some(Bound::Untyped | Bound::Generic) => Receiver::Unknown,
        None => Receiver::SelfValue,
    }
}

/// A value whose type resolution can prove from what makes it: a call to
/// a type's associated function `T::f(…)` (v1 took `new` and `default`
/// only, by convention; resolution checks that the function returns `Self`
/// or `T`, whatever its name), what `?`, `.unwrap()` or `.expect(…)` takes
/// out of one (resolution checks that it returns std's `Result` or
/// `Option` of `T`), or a struct literal `T { … }`.
pub fn constructed(w: &Walker, value: Node) -> Option<Receiver> {
    match value.kind() {
        "struct_expression" => {
            let path = segments(w.src, value.child_by_field_name("name")?)?;
            (!rooted_at(&path, &generics_at(w.src, value))).then(|| Receiver::Literal {
                scope: w.scope_at(value),
                path,
            })
        }
        "call_expression" => match unwrapped(w, value) {
            Some(call) => associated_call(w, call, true),
            None => associated_call(w, value, false),
        },
        "try_expression" => {
            let operand = named_children(value).into_iter().next()?;
            (operand.kind() == "call_expression")
                .then(|| associated_call(w, operand, true))
                .flatten()
        }
        _ => None,
    }
}

/// `T::f(…)` as a [`Receiver::Ctor`], `unwrapped` when the value is what
/// `?`, `.unwrap()` or `.expect(…)` takes out of what it returns. A path
/// rooted at a generic parameter names whatever the parameter is.
fn associated_call(w: &Walker, call: Node, unwrapped: bool) -> Option<Receiver> {
    let function = call.child_by_field_name("function")?;
    if function.kind() != "scoped_identifier" {
        return None;
    }
    let method = text(w.src, function.child_by_field_name("name")?);
    let path = segments(w.src, function.child_by_field_name("path")?)?;
    (!rooted_at(&path, &generics_at(w.src, call))).then(|| Receiver::Ctor {
        scope: w.scope_at(call),
        path,
        method,
        unwrapped,
    })
}

/// The call `.unwrap()` or `.expect(msg)` is made on, when it is one: on
/// std's `Result` or `Option` (which resolution proves of the call's
/// return type), Rust's method probe takes their inherent method, by value,
/// before any trait's, and it gives the `Ok` or `Some` value.
fn unwrapped<'t>(w: &Walker, call: Node<'t>) -> Option<Node<'t>> {
    let function = call.child_by_field_name("function")?;
    if function.kind() != "field_expression" {
        return None;
    }
    let field = text(w.src, function.child_by_field_name("field")?);
    let value = function.child_by_field_name("value")?;
    (matches!(field.as_str(), "unwrap" | "expect") && value.kind() == "call_expression")
        .then_some(value)
}

/// The names of the generic type and const parameters of every item
/// around `node`.
pub fn generics_at(src: &[u8], node: Node) -> Vec<String> {
    let mut names = Vec::new();
    let mut current = Some(node);
    while let Some(n) = current {
        if matches!(
            n.kind(),
            "function_item"
                | "function_signature_item"
                | "impl_item"
                | "trait_item"
                | "struct_item"
                | "enum_item"
                | "union_item"
                | "type_item"
        ) && let Some(params) = n.child_by_field_name("type_parameters")
        {
            for param in named_children(params) {
                let name = match param.kind() {
                    "type_parameter" | "const_parameter" => param.child_by_field_name("name"),
                    _ => None,
                };
                names.extend(name.map(|name| text(src, name)));
            }
        }
        current = n.parent();
    }
    names
}

/// The names of the type and const parameters an item declares itself, in
/// order: what the arguments written on it stand for.
pub fn own_parameter_names(src: &[u8], item: Node) -> Vec<String> {
    own_parameters(item)
        .0
        .into_iter()
        .map(|(name, _)| text(src, name))
        .collect()
}

/// The type parameters an item declares itself, each with its bounds as
/// written in its list (`T: Copy`), and its `where` predicates.
fn own_parameters(item: Node) -> (Vec<(Node, Option<Node>)>, Vec<Node>) {
    let mut params = Vec::new();
    if let Some(list) = item.child_by_field_name("type_parameters") {
        for param in named_children(list) {
            let name = match param.kind() {
                "type_parameter" | "const_parameter" => param.child_by_field_name("name"),
                _ => None,
            };
            if let Some(name) = name {
                params.push((name, param.child_by_field_name("bounds")));
            }
        }
    }
    let predicates = named_children(item)
        .into_iter()
        .filter(|child| child.kind() == "where_clause")
        .flat_map(named_children)
        .filter(|predicate| predicate.kind() == "where_predicate")
        .collect();
    (params, predicates)
}

/// What a type parameter is held to: the bounds its list and the `where`
/// predicates on it alone give it, each on one line, sorted; `?Sized`
/// among them when `keep_unsized`, and those every parameter meets (a
/// lifetime, `Sized`) left out.
///
/// A bound naming a parameter of the item or `Self` (`T: PartialEq<U>`)
/// means something else in another item: it is left out, and the second
/// value is then true.
fn bound_texts(
    src: &[u8],
    name: &str,
    own: Option<Node>,
    predicates: &[Node],
    keep_unsized: bool,
    params: &[String],
) -> (Vec<String>, bool) {
    let mut lists: Vec<Node> = own.into_iter().collect();
    lists.extend(predicates.iter().filter_map(|predicate| {
        let left = predicate.child_by_field_name("left")?;
        (text(src, left) == name)
            .then(|| predicate.child_by_field_name("bounds"))
            .flatten()
    }));
    let mut named = false;
    let mut out: Vec<String> = Vec::new();
    for bound in lists.into_iter().flat_map(named_children) {
        match bound.kind() {
            "removed_trait_bound" if keep_unsized => out.push("?Sized".to_owned()),
            "removed_trait_bound" | "lifetime" | "line_comment" | "block_comment" => {}
            _ => {
                let mut names = params.to_vec();
                names.push("Self".to_owned());
                if types::count_names(src, bound, &names)
                    .iter()
                    .any(|n| *n > 0)
                {
                    named = true;
                    continue;
                }
                let written = types::squash(src, bound);
                if written != "Sized" {
                    out.push(written);
                }
            }
        }
    }
    out.sort();
    out.dedup();
    (out, named)
}

/// What an impl requires of each of its type parameters (D43, D20), by
/// name: the bounds it gives it alone ([`bound_texts`]), or `None` when
/// the rules do not read the parameter as ranging over every type its
/// bounds allow: a bound naming a parameter, a `where` predicate on a type
/// built on it or on `Self` with a bound only some types meet, a second
/// use in `args` (the self type's or the trait's), or a use in `elsewhere`
/// (the self type, for the trait's arguments).
pub fn param_bounds(
    src: &[u8],
    imp: Node,
    args: Option<Node>,
    elsewhere: Option<Node>,
) -> Vec<(String, Option<Vec<String>>)> {
    let (params, predicates) = own_parameters(imp);
    let names: Vec<String> = params.iter().map(|(name, _)| text(src, *name)).collect();
    let mut out: Vec<(String, Option<Vec<String>>)> = params
        .iter()
        .zip(&names)
        .map(|((_, own), name)| {
            let (bounds, named) = bound_texts(src, name, *own, &predicates, false, &names);
            (name.clone(), (!named).then_some(bounds))
        })
        .collect();
    let mut tangled = vec![false; names.len()];
    for predicate in &predicates {
        let (Some(left), Some(bounds)) = (
            predicate.child_by_field_name("left"),
            predicate.child_by_field_name("bounds"),
        ) else {
            continue;
        };
        if !types::restricts(src, bounds) || names.contains(&text(src, left)) {
            continue;
        }
        let on_self = types::count_names(src, left, &["Self".to_owned()])[0] > 0;
        for (at, count) in types::count_names(src, left, &names)
            .into_iter()
            .enumerate()
        {
            tangled[at] |= on_self || count > 0;
        }
    }
    if let Some(args) = args.and_then(|ty| ty.child_by_field_name("type_arguments")) {
        for (at, count) in types::count_names(src, args, &names)
            .into_iter()
            .enumerate()
        {
            tangled[at] |= count > 1;
        }
    }
    if let Some(elsewhere) = elsewhere {
        for (at, count) in types::count_names(src, elsewhere, &names)
            .into_iter()
            .enumerate()
        {
            tangled[at] |= count > 0;
        }
    }
    for (entry, tangled) in out.iter_mut().zip(tangled) {
        if tangled {
            entry.1 = None;
        }
    }
    out
}

/// The bounds a struct, enum or union declares on each of its type and
/// const parameters, in order ([`bound_texts`], `?Sized` kept, one naming
/// a parameter left out): every value of the type meets them (D43). Empty
/// when none has one.
pub fn declared_bounds(src: &[u8], item: Node) -> Vec<Vec<String>> {
    let (params, predicates) = own_parameters(item);
    let names: Vec<String> = params.iter().map(|(name, _)| text(src, *name)).collect();
    let out: Vec<Vec<String>> = params
        .iter()
        .zip(&names)
        .map(|((_, own), name)| bound_texts(src, name, *own, &predicates, true, &names).0)
        .collect();
    if out.iter().all(Vec::is_empty) {
        Vec::new()
    } else {
        out
    }
}
