// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The Rust walker: one pre-order walk per file (v1's `walk_rust`,
//! `extract.rs:1092-1198`) that records definitions, call sites, argument
//! references, `use` leaves, and the facts resolution reads back.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use mvp_model::{EdgeKind, SymbolKind};
use tree_sitter::{Node, Tree};

use super::bindings::{self, Bound, generics_at};
use super::cfg;
use super::types::{
    self, MAX_NESTING, compact, line, named_children, outer_layer, rooted_at, segments, short,
    squash, text, unraw,
};
use super::uses;
use super::{
    Argument, Assoc, AssocKind, AttributeUse, Context, Declared, Facts, Form, Item, ItemKind,
    MacroCall, ModPath, ProcKind, ProcMacro, Receiver, Scope, ScopeKind, SelfKind, TypeRef,
    UNREAD_NAME, Use, Variant, Vis,
};
use crate::extract::uid;
use crate::lang::{Def, Extraction, Site};
use crate::text as rows_text;

/// Nodes deeper than this are skipped with their subtree; this depth itself
/// is visited (v1's `MAX_DEPTH`, W-1).
pub const MAX_DEPTH: usize = 512;

pub use crate::guard::PARSE_BUDGET;

/// Item kinds that make a block a scope of its own.
const BLOCK_ITEMS: [&str; 13] = [
    "function_item",
    "struct_item",
    "enum_item",
    "union_item",
    "trait_item",
    "mod_item",
    "const_item",
    "static_item",
    "type_item",
    "macro_definition",
    "use_declaration",
    "extern_crate_declaration",
    "foreign_mod_item",
];

/// One file's definitions, sites and facts. A file the grammar cannot
/// parse within [`PARSE_BUDGET`], or a walk that panics (v1's G-4), gives
/// nothing.
pub fn extract(content: &[u8]) -> Extraction {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let tree = parse_within(content, PARSE_BUDGET)?;
        Some(Walker::new(content).run(tree.root_node()))
    }))
    .ok()
    .flatten()
    .unwrap_or_default()
}

/// The Rust tree of `content`, or `None` past `budget` (`guard::parse`).
pub fn parse_within(content: &[u8], budget: Duration) -> Option<Tree> {
    crate::guard::parse(&tree_sitter_rust::LANGUAGE.into(), content, budget)
}

/// Where a function or constant is declared.
enum Container<'t> {
    Impl(Node<'t>),
    Trait(Node<'t>),
    Foreign,
    Free,
}

pub struct Walker<'a> {
    pub src: &'a [u8],
    defs: Vec<Def>,
    /// How many definitions of each name and kind so far: the `~n` of uids.
    repeats: HashMap<(String, SymbolKind), u32>,
    sites: Vec<Site>,
    facts: Facts,
    /// The scopes around the node being walked, innermost last.
    scopes: Vec<u32>,
    /// The scope a node opened, by node id.
    scope_of: HashMap<usize, u32>,
    /// The definitions around the node being walked, innermost last.
    owners: Vec<usize>,
    /// The item index of each enum node, for its variants.
    enum_items: HashMap<usize, u32>,
    /// What each impl or trait node qualifies its methods with.
    qualifiers: HashMap<usize, String>,
    /// The scopes an `ERROR` node sits in, already marked unreadable.
    poisoned: HashSet<u32>,
    /// The `#[cfg]`s under which code below the depth cap was found to hold
    /// an impl (D17).
    unread_under: HashSet<Vec<String>>,
    /// The macro calls recorded, each once: the same macro invoked in one
    /// scope under the same `#[cfg]`s counts as one (D23).
    calls: HashSet<MacroCall>,
    /// The attributes recorded, each once by name in a scope under the
    /// same `#[cfg]`s (D23).
    attributes: HashSet<AttributeUse>,
    /// The `#[cfg]` conjuncts of every node around the one being walked
    /// (F2), repeats included.
    cfg: Vec<String>,
}

impl<'a> Walker<'a> {
    fn new(src: &'a [u8]) -> Walker<'a> {
        Walker {
            src,
            defs: Vec::new(),
            repeats: HashMap::default(),
            sites: Vec::new(),
            facts: Facts::default(),
            scopes: Vec::new(),
            scope_of: HashMap::default(),
            owners: Vec::new(),
            enum_items: HashMap::default(),
            qualifiers: HashMap::default(),
            poisoned: HashSet::default(),
            unread_under: HashSet::default(),
            calls: HashSet::default(),
            attributes: HashSet::default(),
            cfg: Vec::new(),
        }
    }

    fn run(mut self, root: Node) -> Extraction {
        self.facts.scopes.push(Scope {
            parent: None,
            kind: ScopeKind::File,
            cfg: Vec::new(),
        });
        self.scope_of.insert(root.id(), 0);
        self.scopes.push(0);
        self.node(root, 0);
        Extraction {
            defs: self.defs,
            sites: self.sites,
            facts: self.facts.encode(),
        }
    }

    /// The innermost scope holding `node`, from the scopes its ancestors
    /// opened (all of them walked already).
    pub fn scope_at(&self, node: Node) -> u32 {
        let mut current = node.parent();
        while let Some(n) = current {
            if let Some(scope) = self.scope_of.get(&n.id()) {
                return *scope;
            }
            current = n.parent();
        }
        0
    }

    fn scope(&self) -> u32 {
        self.scopes.last().copied().unwrap_or(0)
    }

    fn node(&mut self, node: Node, depth: usize) {
        if depth > MAX_DEPTH {
            // An item skipped among a module's, an impl's or a trait's: what
            // it declares is unknown, so the scope proves no name it does not
            // bind otherwise (D17). Code below the cap is not read at all.
            if node
                .parent()
                .is_some_and(|p| matches!(p.kind(), "source_file" | "declaration_list"))
            {
                self.poison();
            }
            self.unread(node);
            return;
        }
        match node.kind() {
            "mod_item" => self.module(node, depth),
            "function_item" => self.function(node, depth),
            "function_signature_item" => self.signature(node),
            "impl_item" => self.implementation(node, depth),
            "trait_item" => self.trait_item(node, depth),
            "struct_item" => self.structure(node, depth),
            "union_item" => {
                if let Some(name) = self.field(node, "name") {
                    let vis = self.vis(node);
                    let (derives, _) = self.derives(node);
                    let item =
                        self.push_item(name, String::new(), vis, ItemKind::Union { derives });
                    self.note_bounds(node, item);
                }
            }
            "enum_item" => self.enumeration(node, depth),
            "enum_variant" => self.variant(node, depth),
            "const_item" | "static_item" => self.constant(node, depth),
            "type_item" => self.alias(node),
            "macro_definition" => self.macro_rules(node),
            "extern_crate_declaration" => self.extern_crate(node),
            "foreign_mod_item" => self.foreign(node),
            "use_declaration" => self.use_declaration(node),
            "call_expression" => {
                self.call(node);
                self.children(node, depth);
            }
            "block" => self.block(node, depth),
            "ERROR" => {
                self.poison();
                self.children(node, depth);
            }
            "macro_invocation" => {
                // A statics call in a block holding an attribute that is
                // not inert runs it as one among items does (D23); other
                // macros invoked in a block are not read.
                if among_items(node) || block_statics(self.src, node) {
                    self.item_macro(node);
                }
            }
            "attribute_item" | "inner_attribute_item" | "line_comment" | "block_comment" => {}
            _ => self.children(node, depth),
        }
    }

    /// Walks `node`'s children. An outer attribute's `#[cfg]` applies to
    /// the next child, an inner one's (`#![cfg]`) to the rest of them, and
    /// so does an outer one of a node whose attributes lead its own
    /// children (a match arm, a field initializer).
    fn children(&mut self, node: Node, depth: usize) {
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        let base = self.cfg.len();
        let leading = matches!(
            node.kind(),
            "match_arm"
                | "field_initializer"
                | "shorthand_field_initializer"
                | "array_expression"
                | "tuple_expression"
        );
        let mut pending = Vec::new();
        let items = matches!(node.kind(), "source_file" | "declaration_list" | "block");
        for child in children {
            if items && child.kind() == "attribute_item" {
                self.attribute_use(child);
            }
            match child.kind() {
                "attribute_item" if leading => self.cfg.extend(self.conjuncts(child)),
                "attribute_item" => pending.extend(self.conjuncts(child)),
                "inner_attribute_item" => self.cfg.extend(self.conjuncts(child)),
                "line_comment" | "block_comment" => {}
                _ => {
                    let mark = self.cfg.len();
                    self.cfg.append(&mut pending);
                    self.node(child, depth + 1);
                    self.cfg.truncate(mark);
                }
            }
        }
        self.cfg.truncate(base);
    }

    /// Records an attribute on an item that is not one of Rust's own,
    /// which may be an attribute macro of the workspace (D23), once per
    /// name in a scope: those a `cfg_attr` applies too, and
    /// [`UNREAD_NAME`] for an attribute the rules cannot read.
    fn attribute_use(&mut self, attribute: Node) {
        let names: Vec<String> = match applied(self.src, attribute) {
            Some(applied) => applied
                .into_iter()
                .filter(|a| !inert(&a.path))
                .filter_map(|mut a| a.path.pop())
                .collect(),
            None => vec![UNREAD_NAME.to_owned()],
        };
        for name in names {
            if name.is_empty() {
                continue;
            }
            let used = AttributeUse {
                scope: self.scope(),
                name,
                cfg: self.cfg_now(),
            };
            if self.attributes.insert(used.clone()) {
                self.facts.attributes.push(used);
            }
        }
    }

    /// The `#[cfg]` conjuncts an attribute item puts on what it applies to
    /// (F2), read from its text when that reads as its tokens do; else one
    /// conjunct the rules cannot read when it may apply a `cfg` (a comment
    /// in `cfg(all(unix /* ,false, */))` hides no `false`).
    fn conjuncts(&self, attribute: Node) -> Vec<String> {
        let mut written = compact(self.src, attribute);
        if let Some(inner) = written.strip_prefix("#![") {
            written = format!("#[{inner}");
        }
        if plain(self.src, attribute) {
            return cfg::conjuncts(&written);
        }
        match applied(self.src, attribute) {
            Some(applied) if applied.iter().all(|a| a.path != ["cfg"]) => Vec::new(),
            _ => {
                let inner = written.strip_prefix("#[").and_then(|w| w.strip_suffix(']'));
                vec![inner.unwrap_or(&written).to_owned()]
            }
        }
    }

    /// The conjuncts around the node being walked, sorted, without
    /// repeats; `cfg::FALSE` alone when one is false.
    fn cfg_now(&self) -> Vec<String> {
        cfg::union(&self.cfg, &[])
    }

    /// Walks `node`'s children with `def` as their owner.
    fn owned(&mut self, def: Option<usize>, node: Node, depth: usize) {
        if let Some(def) = def {
            self.owners.push(def);
        }
        self.children(node, depth);
        if def.is_some() {
            self.owners.pop();
        }
    }

    fn open(&mut self, node: Node, kind: ScopeKind) -> u32 {
        let id = self.facts.scopes.len() as u32;
        self.facts.scopes.push(Scope {
            parent: Some(self.scope()),
            kind,
            cfg: self.cfg_now(),
        });
        self.scope_of.insert(node.id(), id);
        self.scopes.push(id);
        id
    }

    fn close(&mut self) {
        self.scopes.pop();
    }

    fn field(&self, node: Node, field: &str) -> Option<String> {
        node.child_by_field_name(field)
            .map(|child| text(self.src, child))
            .filter(|name| !name.is_empty())
    }

    /// Records a definition (v1's `push_symbol`; an empty name is none,
    /// W-5) and returns its index and its uid without the path.
    fn def(&mut self, kind: SymbolKind, name: &str, node: Node) -> (Option<usize>, String) {
        if name.is_empty() {
            return (None, String::new());
        }
        let count = self.repeats.entry((name.to_owned(), kind)).or_insert(0);
        *count += 1;
        let key = uid("", name, kind, *count);
        let first = self.src[node.byte_range()]
            .split(|b| *b == b'\n')
            .next()
            .unwrap_or_default();
        self.defs.push(Def {
            kind,
            name: name.to_owned(),
            signature: rows_text::signature(&String::from_utf8_lossy(first)),
            line: line(node),
            end_line: node.end_position().row as u32 + 1,
        });
        (Some(self.defs.len() - 1), key)
    }

    fn push_item(&mut self, name: String, uid: String, vis: Vis, kind: ItemKind) -> u32 {
        let index = self.facts.items.len() as u32;
        self.facts.items.push(Item {
            scope: self.scope(),
            name,
            uid,
            vis,
            kind,
            cfg: self.cfg_now(),
        });
        index
    }

    /// Records the bounds a type declares on its parameters (D43).
    fn note_bounds(&mut self, node: Node, item: u32) {
        let bounds = bindings::declared_bounds(self.src, node);
        if !bounds.is_empty() {
            self.facts.bounds.push(Declared { item, bounds });
        }
    }

    fn site(&mut self, kind: EdgeKind, line: u32, target: String, form: Form) {
        self.sites.push(Site {
            kind,
            line,
            owner: self.owners.last().copied(),
            target,
            context: Context {
                scope: self.scope(),
                form,
                cfg: self.cfg_now(),
            }
            .encode(),
        });
    }

    // --- Items --------------------------------------------------------------

    fn module(&mut self, node: Node, depth: usize) {
        let Some(name) = self.field(node, "name") else {
            return;
        };
        let vis = self.vis(node);
        let path = self.path_attribute(node);
        match node.child_by_field_name("body") {
            Some(body) => {
                let (def, uid) = self.def(SymbolKind::Module, &name, node);
                let item = self.push_item(
                    name,
                    uid,
                    vis,
                    ItemKind::Module {
                        body: None,
                        path: path.clone(),
                    },
                );
                let scope = self.open(node, ScopeKind::Module { item });
                self.facts.items[item as usize].kind = ItemKind::Module {
                    body: Some(scope),
                    path,
                };
                self.owned(def, body, depth + 1);
                self.close();
            }
            None => {
                self.push_item(
                    name,
                    String::new(),
                    vis,
                    ItemKind::Module { body: None, path },
                );
            }
        }
    }

    /// A module's `#[path = "…"]`, from the attributes before it (D38):
    /// unknown when a `cfg_attr` may apply one, or an attribute the rules
    /// cannot read.
    fn path_attribute(&self, node: Node) -> ModPath {
        let mut found = ModPath::Layout;
        for item in attribute_items(node) {
            let Some(applied) = applied(self.src, item) else {
                return ModPath::Unknown;
            };
            for (i, attribute) in applied.iter().enumerate() {
                if attribute.path != ["path"] {
                    continue;
                }
                if i > 0 {
                    return ModPath::Unknown;
                }
                let value = named_children(item)
                    .into_iter()
                    .find(|c| c.kind() == "attribute")
                    .and_then(|a| a.child_by_field_name("value"));
                found = match value.map(|v| (v.kind(), named_children(v))) {
                    Some(("string_literal", parts))
                        if parts.iter().all(|p| p.kind() == "string_content") =>
                    {
                        ModPath::Literal(parts.iter().map(|p| text(self.src, *p)).collect())
                    }
                    _ => ModPath::Unknown,
                };
            }
        }
        found
    }

    /// Where `node` is declared: an `impl` or `trait` body, an `extern`
    /// block, or anywhere else.
    fn container<'t>(&self, node: Node<'t>) -> Container<'t> {
        let Some(list) = node.parent().filter(|p| p.kind() == "declaration_list") else {
            return Container::Free;
        };
        match list.parent() {
            Some(owner) if owner.kind() == "impl_item" => Container::Impl(owner),
            Some(owner) if owner.kind() == "trait_item" => Container::Trait(owner),
            Some(owner) if owner.kind() == "foreign_mod_item" => Container::Foreign,
            _ => Container::Free,
        }
    }

    fn method_name(&self, owner: Node, name: &str) -> String {
        let qualifier = self.qualifiers.get(&owner.id()).map_or("", String::as_str);
        format!("{qualifier}::{name}")
    }

    fn function(&mut self, node: Node, depth: usize) {
        let Some(name) = self.field(node, "name") else {
            return self.children(node, depth);
        };
        let def = match self.container(node) {
            Container::Impl(owner) | Container::Trait(owner) => {
                let qualified = self.method_name(owner, &name);
                let (def, uid) = self.def(SymbolKind::Method, &qualified, node);
                self.push_assoc(node, name, uid, true);
                def
            }
            Container::Foreign | Container::Free => {
                let (def, uid) = self.def(SymbolKind::Function, &name, node);
                let vis = self.vis(node);
                for item in attribute_items(node) {
                    self.facts
                        .proc_macros
                        .extend(proc_macros(self.src, item, &name));
                }
                self.push_item(name, uid, vis, ItemKind::Fn);
                def
            }
        };
        self.owned(def, node, depth);
    }

    /// A function without a body: a trait method's signature (a `Method`,
    /// decision 2), or a function of an `extern` block (a value, no symbol).
    fn signature(&mut self, node: Node) {
        let Some(name) = self.field(node, "name") else {
            return;
        };
        match self.container(node) {
            Container::Trait(owner) => {
                let qualified = self.method_name(owner, &name);
                let (_, uid) = self.def(SymbolKind::Method, &qualified, node);
                self.push_assoc(node, name, uid, false);
            }
            Container::Foreign => {
                let vis = self.vis(node);
                self.push_item(name, String::new(), vis, ItemKind::Foreign);
            }
            Container::Impl(_) | Container::Free => {}
        }
    }

    fn push_assoc(&mut self, node: Node, name: String, uid: String, body: bool) {
        // An `async fn` returns a future of its written return type.
        let asynchronous = named_children(node).into_iter().any(|child| {
            child.kind() == "function_modifiers"
                && text(self.src, child)
                    .split_whitespace()
                    .any(|w| w == "async")
        });
        let returns = node
            .child_by_field_name("return_type")
            .filter(|_| !asynchronous)
            .and_then(|ty| outer_layer(self.src, ty, &generics_at(self.src, ty)));
        let assoc = Assoc {
            owner: self.scope(),
            name,
            uid,
            vis: self.vis(node),
            kind: AssocKind::Fn {
                receiver: self.self_kind(node),
                returns,
                body,
            },
            cfg: self.cfg_now(),
        };
        self.facts.assocs.push(assoc);
    }

    /// How a function takes `self`, from its first parameter as the tree
    /// gives it (D42): `self_parameter`'s `&` and `mut` tokens, or the type
    /// of `self: T`.
    fn self_kind(&self, function: Node) -> SelfKind {
        let first = function
            .child_by_field_name("parameters")
            .and_then(|params| {
                named_children(params).into_iter().find(|p| {
                    !matches!(
                        p.kind(),
                        "attribute_item" | "line_comment" | "block_comment"
                    )
                })
            });
        match first {
            Some(param) if param.kind() == "self_parameter" => {
                let mut cursor = param.walk();
                let tokens: Vec<&str> = param.children(&mut cursor).map(|c| c.kind()).collect();
                match (tokens.contains(&"&"), tokens.contains(&"mutable_specifier")) {
                    (false, _) => SelfKind::Value,
                    (true, false) => SelfKind::Ref,
                    (true, true) => SelfKind::RefMut,
                }
            }
            Some(param)
                if param.kind() == "parameter"
                    && param
                        .child_by_field_name("pattern")
                        .is_some_and(|p| p.kind() == "self") =>
            {
                param
                    .child_by_field_name("type")
                    .map_or(SelfKind::Typed, |ty| self.typed_self(ty))
            }
            _ => SelfKind::None,
        }
    }

    /// `self: Self` is a value, `self: &Self` and `self: &'a mut Self` a
    /// reference; any other type (`Box<Self>`, `&&Self`) is typed.
    fn typed_self(&self, ty: Node) -> SelfKind {
        let is_self =
            |node: Node| node.kind() == "type_identifier" && text(self.src, node) == "Self";
        if is_self(ty) {
            return SelfKind::Value;
        }
        match ty.child_by_field_name("type") {
            Some(inner) if ty.kind() == "reference_type" && is_self(inner) => {
                if named_children(ty)
                    .iter()
                    .any(|c| c.kind() == "mutable_specifier")
                {
                    SelfKind::RefMut
                } else {
                    SelfKind::Ref
                }
            }
            _ => SelfKind::Typed,
        }
    }

    fn implementation(&mut self, node: Node, depth: usize) {
        let ty = node.child_by_field_name("type");
        let tr = node.child_by_field_name("trait");
        let own: Vec<String> = generics_at(self.src, node);
        let (by_ref, inner) = match ty {
            Some(ty) if ty.kind() == "reference_type" => (true, ty.child_by_field_name("type")),
            ty => (false, ty),
        };
        let mutable = by_ref && ty.is_some_and(|ty| mutable_reference(ty));
        let blanket = inner
            .and_then(|inner| segments(self.src, inner))
            .is_some_and(|path| rooted_at(&path, &own));
        // A parameter keeps what the impl requires of it (D43): `impl<T:
        // Copy> W<T>` is for the `W`s whose argument is `Copy`.
        let params = bindings::param_bounds(self.src, node, inner, None);
        let self_type = inner.and_then(|inner| {
            outer_layer(self.src, inner, &own).map(|mut layer| {
                if !layer.dynamic {
                    layer.args = types::impl_args(self.src, inner, &own, &params);
                }
                layer
            })
        });
        let trait_path = tr.and_then(|tr| segments(self.src, tr)).unwrap_or_default();
        // The trait's arguments (D20): a parameter the self type names too
        // is fixed by it.
        let trait_args = tr
            .map(|tr| {
                let params = bindings::param_bounds(self.src, node, Some(tr), ty);
                types::impl_args(self.src, tr, &own, &params)
            })
            .unwrap_or_default();
        let display = ty
            .map(|ty| types::impl_name(self.src, ty))
            .unwrap_or_default();
        let qualifier = match tr {
            Some(tr) => format!("<{display} as {}>", types::trait_name(self.src, tr)),
            None => display,
        };
        self.qualifiers.insert(node.id(), qualifier);
        self.open(
            node,
            ScopeKind::Impl {
                self_type,
                by_ref,
                mutable,
                blanket,
                trait_path,
                trait_args,
            },
        );
        self.children(node, depth);
        self.close();
    }

    fn trait_item(&mut self, node: Node, depth: usize) {
        let Some(name) = self.field(node, "name") else {
            return;
        };
        let (def, uid) = self.def(SymbolKind::Trait, &name, node);
        let vis = self.vis(node);
        self.qualifiers.insert(node.id(), name.clone());
        // Any parameter but a lifetime, read whatever its node's kind.
        let generic = node
            .child_by_field_name("type_parameters")
            .is_some_and(|list| {
                named_children(list)
                    .iter()
                    .any(|p| !matches!(p.kind(), "lifetime_parameter" | "lifetime"))
            });
        let item = self.push_item(name, uid, vis, ItemKind::Trait { generic });
        self.open(node, ScopeKind::Trait { item });
        self.owned(def, node, depth);
        self.close();
    }

    fn structure(&mut self, node: Node, depth: usize) {
        let Some(name) = self.field(node, "name") else {
            return;
        };
        let (def, uid) = self.def(SymbolKind::Struct, &name, node);
        let value = node
            .child_by_field_name("body")
            .is_none_or(|body| body.kind() == "ordered_field_declaration_list");
        let (derives, plain_default) = self.derives(node);
        let vis = self.vis(node);
        let kind = ItemKind::Struct {
            value,
            derives,
            plain_default,
        };
        let item = self.push_item(name, uid, vis, kind);
        self.note_bounds(node, item);
        self.owned(def, node, depth);
    }

    fn enumeration(&mut self, node: Node, depth: usize) {
        let Some(name) = self.field(node, "name") else {
            return;
        };
        let (def, uid) = self.def(SymbolKind::Enum, &name, node);
        let (derives, plain_default) = self.derives(node);
        let vis = self.vis(node);
        let kind = ItemKind::Enum {
            derives,
            plain_default,
        };
        let item = self.push_item(name, uid, vis, kind);
        self.note_bounds(node, item);
        self.enum_items.insert(node.id(), item);
        self.owned(def, node, depth);
    }

    /// `E::V` (v1's S-5).
    fn variant(&mut self, node: Node, depth: usize) {
        let Some(name) = self.field(node, "name") else {
            return;
        };
        let Some(owner) = node.parent().and_then(|list| list.parent()) else {
            return;
        };
        let (Some(item), Some(enum_name)) = (
            self.enum_items.get(&owner.id()).copied(),
            self.field(owner, "name"),
        ) else {
            return;
        };
        let (def, uid) = self.def(SymbolKind::Variant, &format!("{enum_name}::{name}"), node);
        let cfg = self.cfg_now();
        self.facts.variants.push(Variant {
            item,
            name,
            uid,
            cfg,
        });
        self.owned(def, node, depth);
    }

    /// `const` and `static`; an associated constant is a `Const` named
    /// unqualified, as in v1 (S-8).
    fn constant(&mut self, node: Node, depth: usize) {
        let Some(name) = self.field(node, "name") else {
            return;
        };
        let def = match self.container(node) {
            Container::Impl(_) | Container::Trait(_) => {
                let (def, uid) = self.def(SymbolKind::Const, &name, node);
                let vis = self.vis(node);
                self.facts.assocs.push(Assoc {
                    owner: self.scope(),
                    name,
                    uid,
                    vis,
                    kind: AssocKind::Const,
                    cfg: self.cfg_now(),
                });
                def
            }
            Container::Foreign => {
                let vis = self.vis(node);
                self.push_item(name, String::new(), vis, ItemKind::Foreign);
                None
            }
            Container::Free => {
                let (kind, item) = if node.kind() == "static_item" {
                    (SymbolKind::Static, ItemKind::Static)
                } else {
                    (SymbolKind::Const, ItemKind::Const)
                };
                let (def, uid) = self.def(kind, &name, node);
                let vis = self.vis(node);
                self.push_item(name, uid, vis, item);
                def
            }
        };
        self.owned(def, node, depth);
    }

    /// `type A = B;`, a `Type`; an associated type of an impl or a trait is
    /// not an alias and is not recorded.
    fn alias(&mut self, node: Node) {
        if !matches!(self.container(node), Container::Free) {
            return;
        }
        let Some(name) = self.field(node, "name") else {
            return;
        };
        let (_, uid) = self.def(SymbolKind::Type, &name, node);
        let params = bindings::own_parameter_names(self.src, node);
        let target = node.child_by_field_name("type").and_then(|ty| {
            let mut layer = outer_layer(self.src, ty, &generics_at(self.src, ty))?;
            if !layer.dynamic {
                layer.args = types::alias_args(self.src, ty, &params);
            }
            Some(layer)
        });
        let vis = self.vis(node);
        let params = params.len() as u32;
        self.push_item(name, uid, vis, ItemKind::Alias { target, params });
    }

    fn macro_rules(&mut self, node: Node) {
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let name = text(self.src, name);
        if name.is_empty() {
            return;
        }
        let (_, uid) = self.def(SymbolKind::Macro, &name, node);
        let exported = self
            .attributes(node)
            .iter()
            .any(|a| a == "#[macro_export]" || a.starts_with("#[macro_export("));
        self.push_item(name, uid, Vis::Public, ItemKind::Macro { exported });
    }

    fn extern_crate(&mut self, node: Node) {
        let Some(krate) = self.field(node, "name") else {
            return;
        };
        let local = self.field(node, "alias").unwrap_or_else(|| krate.clone());
        let vis = self.vis(node);
        self.push_item(local, String::new(), vis, ItemKind::ExternCrate { krate });
    }

    /// The functions and statics of an `extern` block are values of the
    /// scope around it, with no symbol.
    fn foreign(&mut self, node: Node) {
        let Some(body) = node.child_by_field_name("body") else {
            return;
        };
        let mut pending = Vec::new();
        for child in named_children(body) {
            if child.kind() == "attribute_item" {
                pending.extend(self.conjuncts(child));
                continue;
            }
            let mark = self.cfg.len();
            self.cfg.append(&mut pending);
            if matches!(child.kind(), "function_signature_item" | "static_item")
                && let Some(name) = self.field(child, "name")
            {
                let vis = self.vis(child);
                self.push_item(name, String::new(), vis, ItemKind::Foreign);
            }
            self.cfg.truncate(mark);
        }
    }

    fn block(&mut self, node: Node, depth: usize) {
        let declares = named_children(node)
            .iter()
            .any(|child| BLOCK_ITEMS.contains(&child.kind()));
        if declares {
            self.open(node, ScopeKind::Block);
            self.children(node, depth);
            self.close();
        } else {
            self.children(node, depth);
        }
    }

    /// The consecutive attributes before `node`, comments skipped, each
    /// without whitespace.
    fn attributes(&self, node: Node) -> Vec<String> {
        attribute_items(node)
            .into_iter()
            .map(|item| compact(self.src, item))
            .collect()
    }

    /// The traits an item derives, by their last path segment without
    /// `r#`: every `derive(…)` its attributes apply, through `cfg_attr`s
    /// too, with [`UNREAD_NAME`] for an attribute or a part of one the
    /// rules cannot read, which may derive anything (D23). And whether a
    /// plain `#[derive(…)]`, which no `cfg_attr` applies, names `Default`.
    fn derives(&self, node: Node) -> (Vec<String>, bool) {
        let mut out = Vec::new();
        let mut plain_default = false;
        for item in attribute_items(node) {
            let Some(applied) = applied(self.src, item) else {
                out.push(UNREAD_NAME.to_owned());
                continue;
            };
            // The attribute as written comes first, what a `cfg_attr`
            // applies after it.
            for (index, attribute) in applied.into_iter().enumerate() {
                if attribute.path.last().is_some_and(|last| last == "derive") {
                    let names = derive_list(self.src, attribute.arguments);
                    plain_default |= index == 0 && names.iter().any(|n| n == "Default");
                    out.extend(names);
                }
            }
        }
        (out, plain_default)
    }

    /// The visibility the tree gives `node` (D39): `pub(in path)` keeps
    /// its path.
    fn vis(&self, node: Node) -> Vis {
        let Some(modifier) = named_children(node)
            .into_iter()
            .find(|child| child.kind() == "visibility_modifier")
        else {
            return Vis::Private;
        };
        let mut cursor = modifier.walk();
        let tokens: Vec<Node> = modifier.children(&mut cursor).collect();
        let token = |kind: &str| tokens.iter().any(|t| t.kind() == kind);
        let named = tokens.iter().find(|t| t.is_named()).copied();
        if !token("pub") {
            // `crate fn f()`, the 2015 edition's `pub(crate)`.
            return Vis::Crate;
        }
        if token("in") {
            let path = named.and_then(|path| segments(self.src, path));
            return Vis::Restricted(path.unwrap_or_default());
        }
        match named.map(|n| n.kind()) {
            None => Vis::Public,
            Some("self") => Vis::Private,
            Some("super") => Vis::Super,
            Some("crate") => Vis::Crate,
            Some(_) => Vis::Restricted(Vec::new()),
        }
    }

    // --- Sites --------------------------------------------------------------

    /// One `Import` site and one binding per leaf (v1's U-1).
    fn use_declaration(&mut self, node: Node) {
        let Some(argument) = node.child_by_field_name("argument") else {
            return;
        };
        let leaves = if node.has_error() || misread(node) {
            None
        } else {
            uses::leaves(self.src, argument)
        };
        // Misread, or nested past what the rules follow: the names it binds
        // are unknown, so the scope proves none it does not bind otherwise.
        let Some(leaves) = leaves else {
            self.poison();
            let target = compact(self.src, argument);
            self.site(EdgeKind::Import, line(argument), target, Form::Opaque);
            return;
        };
        let vis = self.vis(node);
        if leaves.is_empty() {
            let target = compact(self.src, argument);
            self.site(EdgeKind::Import, line(argument), target, Form::Opaque);
        }
        for leaf in leaves {
            self.facts.uses.push(Use {
                scope: self.scope(),
                vis: vis.clone(),
                path: leaf.path.clone(),
                leaf: leaf.leaf,
                local: leaf.local,
                cfg: self.cfg_now(),
            });
            self.site(
                EdgeKind::Import,
                leaf.line,
                leaf.target,
                Form::Use {
                    path: leaf.path,
                    leaf: leaf.leaf,
                },
            );
        }
    }

    fn call(&mut self, call: Node) {
        let first = self.sites.len();
        if let Some(function) = call.child_by_field_name("function") {
            self.callee(call, function);
        }
        self.arguments(call);
        if misread(call) {
            for site in &mut self.sites[first..] {
                if let Some(mut context) = Context::decode(&site.context) {
                    context.form = Form::Opaque;
                    site.context = context.encode();
                }
            }
        }
    }

    /// A macro invoked where items go, a [`MacroCall`], which may write
    /// impls (D23). `thread_local!` and `lazy_static!` define the statics
    /// they name, which no symbol carries (values that shadow a glob's
    /// names), when they are std's and the `lazy_static` crate's, which
    /// resolution tells, and hold no attribute but inert ones: they put a
    /// static's attributes on the item they write for it, where a derive
    /// or an attribute macro may write anything. Any other macro may
    /// define anything, and leaves its scope marked.
    fn item_macro(&mut self, node: Node) {
        let path = node
            .child_by_field_name("macro")
            .and_then(|m| segments(self.src, m))
            .unwrap_or_default();
        let name = path.last().cloned().unwrap_or_default();
        let tree = named_children(node)
            .into_iter()
            .find(|child| child.kind() == "token_tree");
        let body = tree.map(|t| text(self.src, t)).unwrap_or_default();
        let statics = matches!(name.as_str(), "thread_local" | "lazy_static")
            && !tree.is_some_and(|t| holds_attribute(self.src, t));
        let call = MacroCall {
            scope: self.scope(),
            path,
            cfg: self.cfg_now(),
            statics,
        };
        if self.calls.insert(call.clone()) {
            self.facts.macro_calls.push(call);
        }
        if !statics {
            return self.poison();
        }
        // Words, a lifetime (`'static`) kept whole so it is no keyword.
        let words: Vec<&str> = body
            .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '\''))
            .filter(|word| !word.is_empty())
            .collect();
        for (i, word) in words.iter().enumerate() {
            if *word != "static" {
                continue;
            }
            if let Some(named) = words[i + 1..]
                .iter()
                .find(|w| !matches!(**w, "ref" | "mut"))
            {
                let named = (*named).to_owned();
                self.push_item(named, String::new(), Vis::Private, ItemKind::Static);
            }
        }
    }

    /// Code below the depth cap (D17): an impl in it gives a type of the
    /// crate items no row holds, wherever it sits, and so may an exported
    /// `macro_rules!` or a module's file. Such a region counts as a macro
    /// invoked among the items here that the rules cannot resolve (the
    /// empty path), which may write anything: once per set of `#[cfg]`s.
    fn unread(&mut self, node: Node) {
        let cfg = self.cfg_now();
        if self.unread_under.contains(&cfg) {
            return;
        }
        let mut pending = vec![node];
        while let Some(current) = pending.pop() {
            if matches!(
                current.kind(),
                "impl_item" | "macro_definition" | "mod_item"
            ) {
                self.facts.macro_calls.push(MacroCall {
                    scope: self.scope(),
                    path: Vec::new(),
                    cfg: cfg.clone(),
                    statics: false,
                });
                self.unread_under.insert(cfg);
                return;
            }
            pending.extend(named_children(current));
        }
    }

    /// Marks the current scope as holding names no row records: code the
    /// grammar misread, or a macro invoked where items go (module, impl or
    /// trait body), which can define any item. The mark is a glob of
    /// nothing: in a module it leaves every name the scope does not bind
    /// explicitly unresolved; an impl or trait so marked proves none of its
    /// associated items. It is `pub`, as what it stands for may be: a path
    /// or a glob into the module from another one, and a `use` naming it,
    /// read it too (D23: a trait a macro defines, imported elsewhere).
    fn poison(&mut self) {
        let scope = self.scope();
        if self.poisoned.insert(scope) {
            // Unconditional whatever it was found under: one mark per scope.
            self.facts.uses.push(Use {
                scope,
                vis: Vis::Public,
                path: Vec::new(),
                leaf: super::Leaf::Glob,
                local: None,
                cfg: Vec::new(),
            });
        }
    }

    /// The call's site, by the callee's shape (v1's K-2).
    fn callee(&mut self, call: Node, function: Node) {
        match function.kind() {
            "identifier" => {
                let name = text(self.src, function);
                let form = if bindings::local(self, call, &name).is_some() {
                    Form::Opaque
                } else {
                    Form::Path(vec![name.clone()])
                };
                self.site(EdgeKind::Call, line(function), name, form);
            }
            "scoped_identifier" => {
                let form = match self.path_form(function) {
                    Form::Path(path) => Form::PathCall {
                        path,
                        first: self.first_argument(call),
                    },
                    form => form,
                };
                let at = function
                    .child_by_field_name("name")
                    .map_or_else(|| line(function), line);
                let target = squash(self.src, function);
                self.site(EdgeKind::Call, at, target, form);
            }
            "field_expression" => {
                let Some(field) = function.child_by_field_name("field") else {
                    return;
                };
                let name = text(self.src, field);
                let value = function.child_by_field_name("value");
                let receiver = value.map_or(super::Receiver::Unknown, |value| {
                    bindings::receiver(self, call, value)
                });
                let shown = value
                    .map(|value| short(self.src, value))
                    .unwrap_or_default();
                self.site(
                    EdgeKind::Call,
                    line(field),
                    format!("{shown}.{name}"),
                    Form::Method { name, receiver },
                );
            }
            "generic_function" => {
                if let Some(inner) = function.child_by_field_name("function") {
                    self.callee(call, inner);
                }
            }
            _ => {}
        }
    }

    /// What the code states of a call's first argument: `x`, `&x`, `&mut
    /// x` (any number of references), with `x` a local or a parameter whose
    /// binding states its type or names a generic parameter, `self`, or a
    /// constructor or literal (D20).
    fn first_argument(&self, call: Node) -> Argument {
        let first = call.child_by_field_name("arguments").and_then(|arguments| {
            named_children(arguments).into_iter().find(|child| {
                !matches!(
                    child.kind(),
                    "attribute_item" | "line_comment" | "block_comment"
                )
            })
        });
        let Some(mut value) = first else {
            return Argument::Unknown;
        };
        let mut refs = Vec::new();
        while value.kind() == "reference_expression" {
            refs.push(
                named_children(value)
                    .iter()
                    .any(|c| c.kind() == "mutable_specifier"),
            );
            match value.child_by_field_name("value") {
                Some(inner) => value = inner,
                None => return Argument::Unknown,
            }
        }
        let (bound, receiver) = match value.kind() {
            "self" => {
                let function = std::iter::successors(call.parent(), |n| n.parent())
                    .find(|n| n.kind() == "function_item");
                let own = match function.map(|f| self.self_kind(f)) {
                    Some(SelfKind::Value) => Vec::new(),
                    Some(SelfKind::Ref) => vec![false],
                    Some(SelfKind::RefMut) => vec![true],
                    _ => return Argument::Unknown,
                };
                (own, Receiver::SelfValue)
            }
            "identifier" => match bindings::local(self, call, &text(self.src, value)) {
                Some(Bound::Typed(receiver, own)) => (own, receiver),
                Some(Bound::Generic) => return Argument::Generic,
                _ => return Argument::Unknown,
            },
            "call_expression" | "struct_expression" => match bindings::constructed(self, value) {
                Some(receiver) => (Vec::new(), receiver),
                None => return Argument::Unknown,
            },
            _ => return Argument::Unknown,
        };
        refs.extend(bound);
        Argument::Typed { refs, receiver }
    }

    /// A path in an expression: a path, `<T as Tr>::m`, or opaque when it is
    /// rooted at a generic parameter or not read.
    fn path_form(&self, node: Node) -> Form {
        if let Some(bracketed) = node
            .child_by_field_name("path")
            .filter(|path| path.kind() == "bracketed_type")
        {
            return self.qualified(node, bracketed);
        }
        match segments(self.src, node) {
            Some(path) if !rooted_at(&path, &generics_at(self.src, node)) => Form::Path(path),
            _ => Form::Opaque,
        }
    }

    fn qualified(&self, node: Node, bracketed: Node) -> Form {
        let Some(inner) = named_children(bracketed).into_iter().next() else {
            return Form::Opaque;
        };
        let Some(name) = self.field(node, "name") else {
            return Form::Opaque;
        };
        let (ty, tr) = if inner.kind() == "qualified_type" {
            (
                inner.child_by_field_name("type"),
                inner.child_by_field_name("alias"),
            )
        } else {
            (Some(inner), None)
        };
        let generics = generics_at(self.src, node);
        let (ty, by_ref) = match ty {
            Some(ty) => self.qualified_self(ty, &generics),
            None => (None, None),
        };
        let trait_path = match tr {
            Some(tr) => match segments(self.src, tr) {
                Some(path) if !rooted_at(&path, &generics) => path,
                _ => return Form::Opaque,
            },
            None => Vec::new(),
        };
        let trait_args = tr
            .map(|tr| types::type_args(self.src, tr, &generics))
            .unwrap_or_default();
        Form::Qualified {
            ty,
            by_ref,
            trait_path,
            trait_args,
            rest: vec![name],
        }
    }

    /// The `T` of `<T as Tr>` (see [`Form::Qualified`]): `None` for a
    /// generic parameter or a type built on one, else its outer path
    /// behind one `&` or `&mut` at most, with no layer when it is no path.
    fn qualified_self(&self, ty: Node, generics: &[String]) -> (Option<TypeRef>, Option<bool>) {
        let (by_ref, inner) = if ty.kind() == "reference_type" {
            (Some(mutable_reference(ty)), ty.child_by_field_name("type"))
        } else {
            (None, Some(ty))
        };
        let mut generic = inner;
        while let Some(node) = generic.filter(|n| n.kind() == "reference_type") {
            generic = node.child_by_field_name("type");
        }
        if generic
            .and_then(|node| segments(self.src, node))
            .is_some_and(|path| rooted_at(&path, generics))
        {
            return (None, None);
        }
        let layers = inner
            .and_then(|inner| outer_layer(self.src, inner, generics))
            .filter(|layer| !layer.dynamic)
            .into_iter()
            .collect();
        let ty = TypeRef {
            scope: self.scope(),
            layers,
        };
        (Some(ty), by_ref)
    }

    /// Argument references (v1's A-1): a name or a path passed as an
    /// argument. A name a local, a parameter or a generic parameter binds is
    /// a value and gives no site; `None` is a literal; a field (`self.x`) is
    /// data in Rust, where a method cannot be named without a call.
    fn arguments(&mut self, call: Node) {
        let Some(arguments) = call.child_by_field_name("arguments") else {
            return;
        };
        for argument in named_children(arguments) {
            match argument.kind() {
                "identifier" => {
                    let name = text(self.src, argument);
                    if name == "None"
                        || bindings::local(self, argument, &name).is_some()
                        || generics_at(self.src, argument).contains(&name)
                    {
                        continue;
                    }
                    self.site(
                        EdgeKind::Reference,
                        line(argument),
                        name.clone(),
                        Form::Path(vec![name]),
                    );
                }
                "scoped_identifier" => {
                    let form = self.path_form(argument);
                    let target = squash(self.src, argument);
                    self.site(EdgeKind::Reference, line(argument), target, form);
                }
                _ => {}
            }
        }
    }
}

/// Rust's own inert attributes, sorted: none is a macro of the workspace.
/// `derive`, `test` and `global_allocator` are macros of std's prelude,
/// which a `use` or a glob may shadow (`use test_log::test;`, `use
/// pm::add_get as derive;`), so they are not among them.
const BUILTIN_ATTRIBUTES: [&str; 50] = [
    "allow",
    "automatically_derived",
    "cfg",
    "cfg_attr",
    "cold",
    "collapse_debuginfo",
    "coverage",
    "crate_name",
    "crate_type",
    "debugger_visualizer",
    "deny",
    "deprecated",
    "doc",
    "expect",
    "export_name",
    "feature",
    "forbid",
    "ignore",
    "inline",
    "instruction_set",
    "link",
    "link_name",
    "link_ordinal",
    "link_section",
    "macro_export",
    "macro_use",
    "must_use",
    "naked",
    "no_builtins",
    "no_implicit_prelude",
    "no_link",
    "no_main",
    "no_mangle",
    "no_std",
    "non_exhaustive",
    "panic_handler",
    "path",
    "proc_macro",
    "proc_macro_attribute",
    "proc_macro_derive",
    "recursion_limit",
    "repr",
    "should_panic",
    "target_feature",
    "track_caller",
    "type_length_limit",
    "unsafe",
    "used",
    "warn",
    "windows_subsystem",
];

/// The tools whose attributes (`rustfmt::skip`) are no macro.
const TOOLS: [&str; 4] = ["clippy", "diagnostic", "rustdoc", "rustfmt"];

/// An attribute an attribute item applies (D23): its path, each segment
/// without `r#`, and its delimited arguments (a `derive`'s list).
struct Applied<'t> {
    path: Vec<String>,
    arguments: Option<Node<'t>>,
}

/// The attributes an attribute item applies, read from its tree: its own,
/// or those its `cfg_attr`s apply, nested ones too, comments skipped.
/// `None` when the rules cannot read it all, as it may then apply any
/// macro and derive anything: an error or a doc comment in it (a `#[doc]`
/// where it sits), a token where a path goes, or `cfg_attr`s nested past
/// [`MAX_NESTING`].
fn applied<'t>(src: &[u8], item: Node<'t>) -> Option<Vec<Applied<'t>>> {
    if item.has_error() {
        return None;
    }
    // `#[`, comments, the attribute, `]`; in the attribute, comments, its
    // path, then its arguments or its value.
    let read = |node: Node<'t>, kind: Option<&str>| {
        let mut found = None;
        for child in named_children(node) {
            if doc_comment(child) {
                return None;
            }
            if !matches!(child.kind(), "line_comment" | "block_comment")
                && kind.is_none_or(|kind| child.kind() == kind)
            {
                found = found.or(Some(child));
            }
        }
        found
    };
    let attribute = read(item, Some("attribute"))?;
    let path = segments(src, read(attribute, None)?)?;
    expand(
        src,
        Applied {
            path: path.iter().map(|s| unraw(s).to_owned()).collect(),
            arguments: attribute.child_by_field_name("arguments"),
        },
    )
}

/// The attributes `first` applies: itself, and those it applies when it is
/// a `cfg_attr`, level by level. Each level's tokens are read once, so the
/// cost is the attribute's length.
fn expand<'t>(src: &[u8], first: Applied<'t>) -> Option<Vec<Applied<'t>>> {
    let mut out = Vec::new();
    let mut pending = vec![(first, 0)];
    while let Some((applied, depth)) = pending.pop() {
        if applied.path.last().is_some_and(|last| last == "cfg_attr") {
            if depth >= MAX_NESTING {
                return None;
            }
            let tokens = tokens(applied.arguments?)?;
            // The predicate, then the attributes it applies, in order.
            let parts: Vec<Applied> = tokens
                .split(|t| t.kind() == ",")
                .skip(1)
                .filter(|part| !part.is_empty())
                .map(|part| token_attribute(src, part))
                .collect::<Option<_>>()?;
            pending.extend(parts.into_iter().rev().map(|part| (part, depth + 1)));
        }
        out.push(applied);
    }
    Some(out)
}

/// An attribute written as tokens (a `cfg_attr`'s part, the `[…]` of a
/// `#` in a macro's input): a path, then nothing, one delimited tree, or
/// `=` and a value. `None` for anything else.
fn token_attribute<'t>(src: &[u8], tokens: &[Node<'t>]) -> Option<Applied<'t>> {
    let (path, rest) = token_path(src, tokens)?;
    let arguments = match rest {
        [] => None,
        [tree] if tree.kind() == "token_tree" => Some(*tree),
        [equals, _, ..] if equals.kind() == "=" => None,
        _ => return None,
    };
    Some(Applied { path, arguments })
}

/// The path tokens start with (`a::b`, `::a::b`; each segment without
/// `r#`) and the tokens after it; `None` when they start with no path.
fn token_path<'a, 't>(src: &[u8], tokens: &'a [Node<'t>]) -> Option<(Vec<String>, &'a [Node<'t>])> {
    let mut path = Vec::new();
    let mut rest = tokens;
    if rest.first()?.kind() == "::" {
        path.push(String::new());
        rest = &rest[1..];
    }
    loop {
        let (segment, after) = rest.split_first()?;
        if !matches!(
            segment.kind(),
            "identifier" | "self" | "super" | "crate" | "unsafe"
        ) {
            return None;
        }
        path.push(unraw(&text(src, *segment)).to_owned());
        rest = after;
        match rest.split_first() {
            Some((separator, after)) if separator.kind() == "::" => rest = after,
            _ => return Some((path, rest)),
        }
    }
}

/// The tokens inside a delimited tree, comments skipped; `None` when one
/// is a doc comment, a `#[doc]` where it sits.
fn tokens(tree: Node) -> Option<Vec<Node>> {
    let mut cursor = tree.walk();
    let children: Vec<Node> = tree.children(&mut cursor).collect();
    let (open, rest) = children.split_first()?;
    let (close, inside) = rest.split_last()?;
    if !matches!(open.kind(), "(" | "[" | "{") || !matches!(close.kind(), ")" | "]" | "}") {
        return None;
    }
    let mut out = Vec::new();
    for token in inside {
        match token.kind() {
            _ if doc_comment(*token) => return None,
            "line_comment" | "block_comment" => {}
            _ => out.push(*token),
        }
    }
    Some(out)
}

/// Whether an attribute's text reads as its tokens do: it holds no comment,
/// raw identifier, raw string or character, which a reading of the text
/// would take for tokens, quotes or names.
fn plain(src: &[u8], attribute: Node) -> bool {
    let mut pending = vec![attribute];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "line_comment" | "block_comment" | "raw_string_literal" | "char_literal" => {
                return false;
            }
            "identifier" if src[node.byte_range()].starts_with(b"r#") => return false,
            _ => {}
        }
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor));
    }
    true
}

/// Whether `node` is a doc comment (`///`, `//!`, `/** */`, `/*! */`).
fn doc_comment(node: Node) -> bool {
    matches!(node.kind(), "line_comment" | "block_comment") && node.named_child_count() > 0
}

/// The derives a `derive`'s list names, by their last path segment
/// without `r#`; [`UNREAD_NAME`] for a part that is no path, or for the
/// whole list when it holds a doc comment.
fn derive_list(src: &[u8], arguments: Option<Node>) -> Vec<String> {
    let Some(arguments) = arguments else {
        return Vec::new();
    };
    let Some(tokens) = tokens(arguments) else {
        return vec![UNREAD_NAME.to_owned()];
    };
    tokens
        .split(|t| t.kind() == ",")
        .filter(|part| !part.is_empty())
        .map(|part| match token_path(src, part) {
            Some((path, [])) => path.last().cloned().unwrap_or_default(),
            _ => UNREAD_NAME.to_owned(),
        })
        .collect()
}

/// Whether an attribute's path is Rust's own (`inline`, `cfg_attr`) or a
/// tool's (`rustfmt::skip`): no attribute macro.
fn inert(path: &[String]) -> bool {
    match path {
        [name] => BUILTIN_ATTRIBUTES.binary_search(&name.as_str()).is_ok(),
        [first, _, ..] => TOOLS.contains(&first.as_str()),
        [] => false,
    }
}

/// Whether a macro's token tree holds an attribute that is no inert one:
/// a `derive`, any other attribute, through `cfg_attr`s too, or a `#` the
/// rules cannot read as one. `thread_local!` and `lazy_static!` put a
/// static's attributes on the item they write for it (D23).
fn holds_attribute(src: &[u8], tree: Node) -> bool {
    let mut pending = vec![tree];
    while let Some(current) = pending.pop() {
        let mut cursor = current.walk();
        let tokens: Vec<Node> = current.children(&mut cursor).collect();
        for (i, token) in tokens.iter().enumerate() {
            match token.kind() {
                "#" => {
                    let bang = tokens.get(i + 1).is_some_and(|t| t.kind() == "!");
                    let applied = tokens
                        .get(i + 1 + usize::from(bang))
                        .filter(|t| t.kind() == "token_tree")
                        .filter(|t| t.child(0).is_some_and(|open| open.kind() == "["))
                        .and_then(|t| self::tokens(*t))
                        .and_then(|inside| token_attribute(src, &inside))
                        .and_then(|first| expand(src, first));
                    match applied {
                        Some(applied) if applied.iter().all(|a| inert(&a.path)) => {}
                        _ => return true,
                    }
                }
                "token_tree" => pending.push(*token),
                _ => {}
            }
        }
    }
    false
}

/// Whether a macro invoked in a block is `thread_local!` or
/// `lazy_static!` by name and holds an attribute that is not inert, which
/// may write anything (D23).
fn block_statics(src: &[u8], node: Node) -> bool {
    let named = node
        .child_by_field_name("macro")
        .and_then(|m| segments(src, m))
        .and_then(|path| path.last().cloned())
        .is_some_and(|name| matches!(name.as_str(), "thread_local" | "lazy_static"));
    named
        && named_children(node)
            .into_iter()
            .find(|child| child.kind() == "token_tree")
            .is_some_and(|tree| holds_attribute(src, tree))
}

/// The procedural macros a function's attribute item defines, by the name
/// each is invoked by (a `cfg_attr` applying one counts). One the rules
/// cannot read may define one of each kind, a derive of a name they
/// cannot read among them ([`UNREAD_NAME`]).
fn proc_macros(src: &[u8], item: Node, function: &str) -> Vec<ProcMacro> {
    let defined = |kind, name: &str| ProcMacro {
        kind,
        name: unraw(name).to_owned(),
    };
    let Some(applied) = applied(src, item) else {
        return vec![
            defined(ProcKind::Bang, function),
            defined(ProcKind::Attribute, function),
            defined(ProcKind::Derive, UNREAD_NAME),
        ];
    };
    let mut out = Vec::new();
    for attribute in applied {
        match attribute.path.last().map(String::as_str) {
            Some("proc_macro") => out.push(defined(ProcKind::Bang, function)),
            Some("proc_macro_attribute") => out.push(defined(ProcKind::Attribute, function)),
            Some("proc_macro_derive") => {
                // `proc_macro_derive(Name, attributes(…))`.
                let tokens = attribute.arguments.and_then(tokens).unwrap_or_default();
                let name = match tokens.split(|t| t.kind() == ",").next() {
                    Some([name]) if name.kind() == "identifier" => text(src, *name),
                    _ => UNREAD_NAME.to_owned(),
                };
                out.push(defined(ProcKind::Derive, &name));
            }
            _ => {}
        }
    }
    out
}

/// The attribute items right before `node`, comments skipped.
fn attribute_items(node: Node) -> Vec<Node> {
    let mut out = Vec::new();
    let mut sibling = node.prev_named_sibling();
    while let Some(previous) = sibling {
        match previous.kind() {
            "attribute_item" => out.push(previous),
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        sibling = previous.prev_named_sibling();
    }
    out
}

/// Whether a macro is invoked where items go: among a module's, an impl's
/// or a trait's items. tree-sitter-rust reads `m!(…);` among a file's items
/// as a statement holding the invocation.
fn among_items(node: Node) -> bool {
    let item_list = |n: Node| matches!(n.kind(), "source_file" | "declaration_list");
    match node.parent() {
        Some(parent) if parent.kind() == "expression_statement" => {
            parent.parent().is_some_and(item_list)
        }
        Some(parent) => item_list(parent),
        None => false,
    }
}

/// Whether a reference type is `&mut`.
fn mutable_reference(ty: Node) -> bool {
    named_children(ty)
        .iter()
        .any(|c| c.kind() == "mutable_specifier")
}

/// Whether the grammar misread code around `node`: an `ERROR` node holds
/// it, or the function holding it has an error somewhere (a `let` or a
/// block lost in it would change what a name means). Such a site is
/// opaque.
fn misread(node: Node) -> bool {
    let mut current = Some(node);
    while let Some(n) = current {
        if n.is_error() || (n.kind() == "function_item" && n.has_error()) {
            return true;
        }
        current = n.parent();
    }
    false
}
