// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Paths and types as written: the segments of a path, the layers of a
//! type, the name an impl gives its methods. Pure functions of the tree.

use tree_sitter::Node;

use super::Layer;

/// The pointers whose methods a value reaches through `Deref`: a local
/// typed `Box<T>`, `Rc<T>` or `Arc<T>` is read as a `T` (with the
/// pointer's own methods set apart at resolution).
pub const POINTERS: [&str; 3] = ["Box", "Rc", "Arc"];

/// Traits that add no method: a bound on one of them does not change which
/// trait an `impl Tr + Send` names.
const MARKERS: [&str; 5] = ["Send", "Sync", "Unpin", "Sized", "Copy"];

pub fn text(src: &[u8], node: Node) -> String {
    String::from_utf8_lossy(&src[node.byte_range()]).into_owned()
}

/// `text` without whitespace: a path broken across lines names the same
/// item (v1's `use_path_text`, `extract.rs:1321-1325`).
pub fn compact(src: &[u8], node: Node) -> String {
    text(src, node).split_whitespace().collect()
}

/// The identifier a raw one names: `r#match` is `match`.
pub fn unraw(name: &str) -> &str {
    name.strip_prefix("r#").unwrap_or(name)
}

pub fn line(node: Node) -> u32 {
    node.start_position().row as u32 + 1
}

pub fn named_children(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

/// How deep the rules follow syntax that nests without bound (a type's
/// `+` bounds, `use` groups, `cfg` predicates): past it, what the nesting
/// names is unresolved (D17). Real code nests a few levels.
pub const MAX_NESTING: usize = 64;

/// The segments of a path, generic arguments dropped: `a::B::<T>::f` →
/// `["a", "B", "f"]`, `::std::mem` → `["", "std", "mem"]`. `None` for a
/// path the rules do not read (`<T as Tr>::m`, a macro's `$x`). A loop, not
/// a recursion: a path of any length costs no stack (D17).
pub fn segments(src: &[u8], node: Node) -> Option<Vec<String>> {
    let mut reversed = Vec::new();
    let mut current = node;
    loop {
        match current.kind() {
            "identifier" | "type_identifier" | "self" | "super" | "crate" | "primitive_type" => {
                reversed.push(text(src, current));
                break;
            }
            "scoped_identifier" | "scoped_type_identifier" => {
                reversed.push(text(src, current.child_by_field_name("name")?));
                match current.child_by_field_name("path") {
                    Some(path) => current = path,
                    None => {
                        if text(src, current).trim_start().starts_with("::") {
                            reversed.push(String::new());
                        }
                        break;
                    }
                }
            }
            "generic_type" | "generic_type_with_turbofish" => {
                current = current.child_by_field_name("type")?;
            }
            _ => return None,
        }
    }
    reversed.reverse();
    Some(reversed)
}

/// True when `path` starts with one of `generics`: `T::new()` in
/// `fn f<T: Default>()` names whatever `T` is, never an item called `T`.
pub fn rooted_at(path: &[String], generics: &[String]) -> bool {
    path.first().is_some_and(|first| generics.contains(first))
}

/// The references written around a type, outermost first (`true` for
/// `&mut`), and the type inside them. A loop: `&&…&T` costs no stack.
pub fn references(ty: Node) -> (Vec<bool>, Node) {
    let mut refs = Vec::new();
    let mut current = ty;
    while current.kind() == "reference_type" {
        refs.push(
            named_children(current)
                .iter()
                .any(|c| c.kind() == "mutable_specifier"),
        );
        match current.child_by_field_name("type") {
            Some(inner) => current = inner,
            None => break,
        }
    }
    (refs, current)
}

/// The layers of a type (see [`super::TypeRef`]): `&mut a::B<'_>` →
/// `[a::B]`, `Arc<dyn Tr>` → `[Arc, dyn Tr]`, `impl Tr + Send` →
/// `[dyn Tr]`; empty for a type that names no item (a tuple, a slice, a
/// primitive's methods are std's, a generic parameter). A loop over the
/// references and pointers, so `&&…&T` and `Box<Box<…>>` cost no stack.
pub fn layers(src: &[u8], ty: Node, generics: &[String]) -> Vec<Layer> {
    let mut out = Vec::new();
    let mut current = ty;
    loop {
        match current.kind() {
            "reference_type" => match current.child_by_field_name("type") {
                Some(inner) => current = inner,
                None => return out,
            },
            "type_identifier" | "scoped_type_identifier" | "generic_type" => {
                let Some(path) = segments(src, current).filter(|p| !rooted_at(p, generics)) else {
                    return out;
                };
                let pointer = current.kind() == "generic_type"
                    && path
                        .last()
                        .is_some_and(|last| POINTERS.contains(&last.as_str()));
                out.push(Layer {
                    path,
                    dynamic: false,
                    args: type_args(src, current, generics),
                });
                match first_type_argument(current).filter(|_| pointer) {
                    Some(pointee) => current = pointee,
                    None => return out,
                }
            }
            "dynamic_type" | "abstract_type" => {
                out.extend(
                    current
                        .child_by_field_name("trait")
                        .and_then(|tr| trait_layer(src, tr, generics, 0)),
                );
                return out;
            }
            "bounded_type" => {
                out.extend(bounded(src, current, generics, 0));
                return out;
            }
            _ => return out,
        }
    }
}

/// The one trait of `dyn Tr`, `impl Tr`, `Tr + Send`; `None` past
/// [`MAX_NESTING`] nested bounds.
fn trait_layer(src: &[u8], tr: Node, generics: &[String], depth: usize) -> Option<Layer> {
    match tr.kind() {
        "type_identifier" | "scoped_type_identifier" | "generic_type" => {
            let path = segments(src, tr).filter(|p| !rooted_at(p, generics))?;
            Some(Layer {
                path,
                dynamic: true,
                args: Vec::new(),
            })
        }
        "bounded_type" => bounded(src, tr, generics, depth + 1),
        _ => None,
    }
}

/// `A + Send + 'a`: the single bound that is not a marker or a lifetime.
/// `A + B + C` nests one `bounded_type` per `+`, so the recursion stops at
/// [`MAX_NESTING`] with no layer.
fn bounded(src: &[u8], ty: Node, generics: &[String], depth: usize) -> Option<Layer> {
    if depth > MAX_NESTING {
        return None;
    }
    let mut found = None;
    for part in named_children(ty) {
        let layer = match part.kind() {
            "lifetime" | "removed_trait_bound" => continue,
            "type_identifier" if MARKERS.contains(&text(src, part).as_str()) => continue,
            "dynamic_type" | "abstract_type" => part
                .child_by_field_name("trait")
                .and_then(|tr| trait_layer(src, tr, generics, depth)),
            _ => trait_layer(src, part, generics, depth),
        };
        if found.is_some() || layer.is_none() {
            return None;
        }
        found = layer;
    }
    found
}

/// Past this many bytes a generic argument is not read (`?`): reading
/// each layer of `Box<Box<…>>` whole would cost the square of its length.
const MAX_ARGUMENT: usize = 256;

/// An impl's generic parameter that the rules do not read as ranging over
/// every type its bounds allow, as the arguments of its self type or trait
/// show it (D43, D20).
pub const CONSTRAINED: &str = "_+";

/// What starts an impl's generic parameter held to bounds: `_:` and the
/// bounds as one record.
pub const BOUNDED: &str = "_:";

/// Whether an argument stands for a generic parameter: `_`, [`BOUNDED`],
/// [`CONSTRAINED`].
pub fn is_param(arg: &str) -> bool {
    arg == "_" || arg == CONSTRAINED || arg.starts_with(BOUNDED)
}

/// The generic arguments written on a path type's last segment (D43):
/// `G<'a, u8, T>` → `["u8", "_"]` when `T` is among `generics`. Each is its
/// text on one line ([`squash`]), `_` for a generic parameter, `?` for a
/// type that names one, is left to inference (`G<_>`) or is longer than
/// [`MAX_ARGUMENT`].
pub fn type_args(src: &[u8], ty: Node, generics: &[String]) -> Vec<String> {
    impl_args(src, ty, generics, &[])
}

/// [`type_args`] of an impl's self type or trait, each parameter as
/// `params` holds it (`bindings::param_bounds`): `_` with no bound,
/// [`BOUNDED`] and its bounds, [`CONSTRAINED`].
pub fn impl_args(
    src: &[u8],
    ty: Node,
    generics: &[String],
    params: &[(String, Option<Vec<String>>)],
) -> Vec<String> {
    let Some(arguments) = ty
        .child_by_field_name("type_arguments")
        .filter(|_| ty.kind() == "generic_type")
    else {
        return Vec::new();
    };
    named_children(arguments)
        .into_iter()
        .filter(|arg| !matches!(arg.kind(), "lifetime" | "line_comment" | "block_comment"))
        .map(|arg| {
            if arg.byte_range().len() > MAX_ARGUMENT {
                return "?".to_owned();
            }
            let written = squash(src, arg);
            match params.iter().find(|(name, _)| *name == written) {
                Some((_, None)) => return CONSTRAINED.to_owned(),
                Some((_, Some(bounds))) if !bounds.is_empty() => {
                    return format!("{BOUNDED}{}", crate::codec::record(bounds));
                }
                _ => {}
            }
            if generics.contains(&written) {
                "_".to_owned()
            } else if written == "_" || names_one_of(src, arg, generics) {
                "?".to_owned()
            } else {
                written
            }
        })
        .collect()
}

/// The generic arguments written on an alias's target, `ty` (D43): as
/// [`type_args`], with the alias's own parameter `k` of `params` written
/// `$k`, and a type built on one `?`. What an impl or a value writes on the
/// alias then takes the place of `$k`.
pub fn alias_args(src: &[u8], ty: Node, params: &[String]) -> Vec<String> {
    let Some(arguments) = ty
        .child_by_field_name("type_arguments")
        .filter(|_| ty.kind() == "generic_type")
    else {
        return Vec::new();
    };
    named_children(arguments)
        .into_iter()
        .filter(|arg| !matches!(arg.kind(), "lifetime" | "line_comment" | "block_comment"))
        .map(|arg| {
            if arg.byte_range().len() > MAX_ARGUMENT {
                return "?".to_owned();
            }
            let written = squash(src, arg);
            match params.iter().position(|p| *p == written) {
                Some(k) => format!("${k}"),
                None if written == "_" || names_one_of(src, arg, params) => "?".to_owned(),
                None => written,
            }
        })
        .collect()
}

/// How many times each of `names` appears in `node`'s subtree. An explicit
/// stack: a type nested deep costs no stack.
pub fn count_names(src: &[u8], node: Node, names: &[String]) -> Vec<usize> {
    let mut counts = vec![0; names.len()];
    if names.is_empty() {
        return counts;
    }
    let mut pending = vec![node];
    while let Some(current) = pending.pop() {
        if matches!(current.kind(), "type_identifier" | "identifier")
            && let Some(at) = names.iter().position(|n| *n == text(src, current))
        {
            counts[at] += 1;
        }
        pending.extend(named_children(current));
    }
    counts
}

/// Whether a list of bounds (`trait_bounds`) holds one that is no lifetime
/// and not `?Sized` or `Sized` (which every parameter has unless it says
/// `?Sized`): a bound only some types meet.
pub fn restricts(src: &[u8], bounds: Node) -> bool {
    named_children(bounds).into_iter().any(|bound| {
        !matches!(
            bound.kind(),
            "lifetime" | "removed_trait_bound" | "line_comment" | "block_comment"
        ) && text(src, bound) != "Sized"
    })
}

/// Whether a name of `names` appears in `node`'s subtree. An explicit
/// stack: a type nested deep costs no stack.
fn names_one_of(src: &[u8], node: Node, names: &[String]) -> bool {
    if names.is_empty() {
        return false;
    }
    let mut pending = vec![node];
    while let Some(current) = pending.pop() {
        if matches!(current.kind(), "type_identifier" | "identifier")
            && names.contains(&text(src, current))
        {
            return true;
        }
        pending.extend(named_children(current));
    }
    false
}

fn first_type_argument(ty: Node) -> Option<Node> {
    let arguments = ty.child_by_field_name("type_arguments")?;
    named_children(arguments)
        .into_iter()
        .find(|arg| !matches!(arg.kind(), "lifetime" | "line_comment" | "block_comment"))
}

/// A type's outer path, without looking through pointers: the self type of
/// an impl, the target of an alias, a function's return type. `dyn Tr` is
/// the trait, dynamic; anything else (a reference, a tuple, a generic
/// parameter) is `None`.
pub fn outer_layer(src: &[u8], ty: Node, generics: &[String]) -> Option<Layer> {
    match ty.kind() {
        "type_identifier" | "scoped_type_identifier" | "generic_type" => {
            let path = segments(src, ty).filter(|p| !rooted_at(p, generics))?;
            Some(Layer {
                path,
                dynamic: false,
                args: type_args(src, ty, generics),
            })
        }
        "dynamic_type" => ty
            .child_by_field_name("trait")
            .and_then(|tr| trait_layer(src, tr, generics, 0)),
        _ => None,
    }
}

/// What an impl's methods are qualified with: the self type's last path
/// segment without generics (`impl<T> a::B<T>` → `B`, v1's S-2 with the
/// intent of question 7); `&B` for a reference; for any other type its text
/// up to the first `<`, whitespace collapsed (`dyn Tr`, `[T]`). A loop over
/// the references, so `&&…&B` costs no stack.
pub fn impl_name(src: &[u8], ty: Node) -> String {
    let mut prefix = String::new();
    let mut current = ty;
    while current.kind() == "reference_type" {
        let mutable = named_children(current)
            .iter()
            .any(|c| c.kind() == "mutable_specifier");
        prefix.push_str(if mutable { "&mut " } else { "&" });
        match current.child_by_field_name("type") {
            Some(inner) => current = inner,
            None => return prefix,
        }
    }
    let name = match current.kind() {
        "type_identifier" => text(src, current),
        "scoped_type_identifier" => current
            .child_by_field_name("name")
            .map(|name| text(src, name))
            .unwrap_or_default(),
        "generic_type" => match current.child_by_field_name("type") {
            Some(inner) if inner.kind() == "type_identifier" => text(src, inner),
            Some(inner) if inner.kind() == "scoped_type_identifier" => inner
                .child_by_field_name("name")
                .map(|name| text(src, name))
                .unwrap_or_default(),
            Some(inner) => head(src, inner),
            None => String::new(),
        },
        _ => head(src, current),
    };
    prefix + &name
}

/// A type's text up to its first `<`, whitespace collapsed.
fn head(src: &[u8], ty: Node) -> String {
    let whole = text(src, ty);
    let head = whole.split('<').next().unwrap_or("");
    head.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A trait's last path segment without generics (`fmt::Display` →
/// `Display`, `From<u8>` → `From`).
pub fn trait_name(src: &[u8], tr: Node) -> String {
    segments(src, tr)
        .and_then(|path| path.last().cloned())
        .unwrap_or_else(|| impl_name(src, tr))
}

/// A node's text on one line, as an edge's target shows it: whitespace
/// dropped next to punctuation, one space between two words
/// (`<A as Tr>::m`, `foo.bar()` from `foo\n    .bar()`).
pub fn squash(src: &[u8], node: Node) -> String {
    let raw = text(src, node);
    let mut out = String::new();
    let mut pending_space = false;
    for c in raw.chars() {
        if c.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space {
            let previous = out.chars().last();
            if previous.is_some_and(word) && word(c) {
                out.push(' ');
            }
            pending_space = false;
        }
        out.push(c);
    }
    out
}

/// A receiver's text for a method call's target: [`squash`]ed, and cut to
/// its last 47 characters behind `…` past 48.
pub fn short(src: &[u8], node: Node) -> String {
    let out = squash(src, node);
    let count = out.chars().count();
    if count > 48 {
        let tail: String = out.chars().skip(count - 47).collect();
        format!("…{tail}")
    } else {
        out
    }
}

fn word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}
