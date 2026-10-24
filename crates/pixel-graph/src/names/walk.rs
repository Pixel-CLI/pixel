// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The syntax walkers of the name-resolved languages: a tree-sitter tree in,
//! definitions, call sites, references and imports out. Lifted from Pixel's
//! `pixel-graph` `extract.rs` (`walk_ts` and its helpers), with every site's
//! line taken from the name it targets, so that the truth oracle can find the
//! name at the line a site gives.

use tree_sitter::Node;

use pixel_model::SymbolKind;

/// The deepest node a walker visits; deeper nodes are skipped.
pub(super) const MAX_DEPTH: usize = 512;

/// A definition as a walker found it.
#[derive(Clone, Debug)]
pub(super) struct RawSymbol {
    /// The qualified name: the enclosing types, then the name.
    pub qualified: String,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub end_line: u32,
}

/// A call (or a constructor use, or a rendered JSX component).
#[derive(Clone, Debug)]
pub(super) struct RawCall {
    pub callee: String,
    pub receiver: Option<String>,
    /// The line of the callee's name.
    pub line: u32,
}

/// A symbol passed as an argument to a call: a callback, a plugin, a handler.
#[derive(Clone, Debug)]
pub(super) struct RawReference {
    pub name: String,
    pub line: u32,
    /// The callee that received the argument, when known.
    pub arg_of: Option<String>,
}

/// One name an import brings into scope: `local` is what the importing file
/// calls it, `source` what the imported file defines. They differ only under
/// an alias (`import { push as leased }`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ImportBinding {
    pub local: String,
    pub source: String,
}

impl ImportBinding {
    pub(super) fn named(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            local: name.clone(),
            source: name,
        }
    }

    pub(super) fn aliased(source: impl Into<String>, local: impl Into<String>) -> Self {
        Self {
            local: local.into(),
            source: source.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct RawImport {
    /// The statement's import text as written.
    pub spec: String,
    /// The line of the statement.
    pub line: u32,
    /// The inclusive line ranges where the bindings are in scope; empty means
    /// the whole file.
    pub scope: Vec<(u32, u32)>,
    /// Named bindings. Empty for a wildcard import or when bindings cannot
    /// be read; empty bindings never grant `Proved` confidence.
    pub bindings: Vec<ImportBinding>,
}

/// What a walk found in one file.
#[derive(Debug, Default)]
pub(super) struct Found {
    pub symbols: Vec<RawSymbol>,
    pub calls: Vec<RawCall>,
    pub references: Vec<RawReference>,
    pub imports: Vec<RawImport>,
}

pub(super) struct Walker<'a> {
    pub src: &'a [u8],
    pub found: Found,
    /// Enclosing type names, for qualification.
    pub stack: Vec<String>,
}

impl<'a> Walker<'a> {
    pub(super) fn new(src: &'a [u8]) -> Self {
        Walker {
            src,
            found: Found::default(),
            stack: Vec::new(),
        }
    }

    pub(super) fn text(&self, n: Node) -> String {
        String::from_utf8_lossy(&self.src[n.byte_range()]).into_owned()
    }

    pub(super) fn qualify(&self, name: &str, sep: &str) -> String {
        if self.stack.is_empty() {
            name.to_owned()
        } else {
            format!("{}{}{}", self.stack.join(sep), sep, name)
        }
    }

    pub(super) fn push_symbol(&mut self, qualified: String, kind: SymbolKind, node: Node) {
        if qualified.is_empty() {
            return;
        }
        self.found.symbols.push(RawSymbol {
            qualified,
            kind,
            start_line: line_start(node),
            end_line: line_end(node),
        });
    }

    pub(super) fn push_call(&mut self, callee: String, receiver: Option<String>, at: Node) {
        if callee.is_empty() {
            return;
        }
        // A receiver spread over lines (`a\n  .b()`) is one name without its
        // whitespace.
        let receiver = receiver.map(|r| r.split_whitespace().collect::<String>());
        self.found.calls.push(RawCall {
            callee,
            receiver,
            line: line_start(at),
        });
    }

    pub(super) fn push_reference(&mut self, name: String, at: Node, arg_of: Option<String>) {
        if name.is_empty() {
            return;
        }
        self.found.references.push(RawReference {
            name,
            line: line_start(at),
            arg_of,
        });
    }

    pub(super) fn push_import(&mut self, spec: String, at: Node, bindings: Vec<ImportBinding>) {
        if spec.is_empty() {
            return;
        }
        self.found.imports.push(RawImport {
            spec,
            line: line_start(at),
            scope: Vec::new(),
            bindings,
        });
    }
}

pub(super) fn line_start(n: Node) -> u32 {
    n.start_position().row as u32 + 1
}

pub(super) fn line_end(n: Node) -> u32 {
    n.end_position().row as u32 + 1
}

pub(super) fn field_text(w: &Walker, n: Node, field: &str) -> Option<String> {
    n.child_by_field_name(field).map(|c| w.text(c))
}

pub(super) fn strip_quotes(s: &str) -> String {
    s.trim_matches(['"', '\'', '`']).to_owned()
}

pub(super) fn each_child<'t>(n: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = n.walk();
    n.children(&mut cursor).collect()
}

/// Words that parse as identifiers in some grammars but never name a
/// function: literal keywords and the self pseudo-receivers.
const NON_REFERENCE_WORDS: &[&str] = &[
    "undefined",
    "null",
    "true",
    "false",
    "None",
    "nil",
    "self",
    "this",
];

/// The receiver texts a member argument may start from and still name a
/// function of the enclosing type (`this.onClick`, `self.handler`).
const SELF_RECEIVERS: &[&str] = &["this", "self", "Self"];

/// Walk the `arguments` field of a call node and record each argument that
/// may name a function as a reference; `arg_of` is the callee that received
/// the argument.
///
/// - a bare identifier (`schema.plugin(tenantScopePlugin)`), literal keywords
///   and self pseudo-receivers excepted;
/// - a path (`module::func`, `pkg.Func`) naming an item: its last segment;
/// - a member access only on a self receiver (`this.onClick`). A member of
///   any other value (`user.name`) is data: resolving its property name
///   against every function of that name linked unrelated code.
pub(super) fn walk_call_arguments(w: &mut Walker, call: Node, arg_of: Option<String>) {
    let Some(args) = call.child_by_field_name("arguments") else {
        return;
    };
    let mut cursor = args.walk();
    for arg in args.children(&mut cursor) {
        // C# and PHP wrap each argument in an `argument` node whose last
        // named child is the value: `OnDone` in `Handle(OnDone)`.
        let arg = if arg.kind() == "argument" {
            match each_child(arg).into_iter().rev().find(Node::is_named) {
                Some(value) => value,
                None => continue,
            }
        } else {
            arg
        };
        let name = match arg.kind() {
            "identifier" | "simple_identifier" | "variable" => Some(w.text(arg)),
            "scoped_identifier" => field_text(w, arg, "name"),
            "member_expression" | "field_expression" | "attribute" | "member_access_expression" => {
                self_member_name(w, arg)
            }
            _ => None,
        };
        if let Some(name) = name
            && !NON_REFERENCE_WORDS.contains(&name.as_str())
        {
            w.push_reference(name, arg, arg_of.clone());
        }
    }
}

/// The property of a member access whose receiver is `this`/`self`/`Self`,
/// or `None` for a member of any other value.
fn self_member_name(w: &Walker, member: Node) -> Option<String> {
    let receiver = ["object", "value", "expression"]
        .iter()
        .find_map(|f| member.child_by_field_name(f))
        .map(|c| w.text(c))?;
    if !SELF_RECEIVERS.contains(&receiver.as_str()) {
        return None;
    }
    ["property", "field", "attribute", "name"]
        .iter()
        .find_map(|f| member.child_by_field_name(f))
        .map(|c| w.text(c))
}

// --- TypeScript / TSX / JavaScript ---------------------------------------

/// Whether a `variable_declarator` or a class field holds a function value.
fn holds_function(node: Node) -> bool {
    node.child_by_field_name("value").is_some_and(|v| {
        matches!(
            v.kind(),
            "arrow_function" | "function_expression" | "function"
        )
    })
}

pub(super) fn walk_ts(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut pushed = false;
    match node.kind() {
        "function_declaration" | "generator_function_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(q, SymbolKind::Function, node);
            }
        }
        "class_declaration" | "abstract_class_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(q, SymbolKind::Class, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        // An interface is the closest thing to a trait the model has.
        "interface_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(q, SymbolKind::Trait, node);
            }
        }
        "enum_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(q, SymbolKind::Enum, node);
            }
        }
        "method_definition" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(q, SymbolKind::Method, node);
            }
        }
        // `class C { handle = async (e) => {...} }` (TS: `public_field_definition`,
        // JS: `field_definition`): a method of the class whose body is the
        // arrow, so the calls made inside it belong to it.
        "public_field_definition" | "field_definition" => {
            if holds_function(node)
                && let Some(name) = ["name", "property"]
                    .iter()
                    .find_map(|f| field_text(w, node, f))
                    .filter(|name| !name.contains(['{', '[', '(']))
            {
                let q = w.qualify(&name, ".");
                w.push_symbol(q, SymbolKind::Method, node);
            }
        }
        "variable_declarator" => {
            if holds_function(node)
                && let Some(name) = field_text(w, node, "name")
                && !name.contains(['{', '['])
            {
                let q = w.qualify(&name, ".");
                w.push_symbol(q, SymbolKind::Function, node);
            }
        }
        "call_expression" => {
            let mut callee_name: Option<String> = None;
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => {
                        let name = w.text(f);
                        callee_name = Some(name.clone());
                        w.push_call(name, None, f);
                    }
                    "member_expression" => {
                        if let Some(prop) = f.child_by_field_name("property") {
                            let name = w.text(prop);
                            let recv = field_text(w, f, "object");
                            callee_name = Some(name.clone());
                            w.push_call(name, recv, prop);
                        }
                    }
                    _ => {}
                }
            }
            // The callee's arguments may name callbacks, plugins, handlers.
            walk_call_arguments(w, node, callee_name);
        }
        "new_expression" => {
            if let Some(c) = node.child_by_field_name("constructor")
                && c.kind() == "identifier"
            {
                let name = w.text(c);
                w.push_call(name, None, c);
            }
            // `new Foo(handler)`: constructor arguments can be callbacks too.
            walk_call_arguments(w, node, None);
        }
        "import_statement" | "export_statement" => {
            if let Some(src) = node.child_by_field_name("source") {
                let spec = strip_quotes(&w.text(src));
                let bindings = ts_import_bindings(w, node);
                w.push_import(spec, node, bindings);
            }
        }
        // Only the tsx and js grammars produce these nodes; the ts grammar
        // has no JSX, so no language guard is needed here.
        "jsx_element" => {
            if let Some(opening) = node.child_by_field_name("open_tag")
                && let Some(tag_node) = opening.child_by_field_name("name")
            {
                let tag = w.text(tag_node);
                jsx_handler_refs(w, opening, &tag);
                if let Some((name, receiver)) = jsx_component_call(&tag) {
                    w.push_call(name, receiver, tag_node);
                }
            }
        }
        "jsx_self_closing_element" => {
            if let Some(tag_node) = node.child_by_field_name("name") {
                let tag = w.text(tag_node);
                jsx_handler_refs(w, node, &tag);
                if let Some((name, receiver)) = jsx_component_call(&tag) {
                    w.push_call(name, receiver, tag_node);
                }
            }
        }
        _ => {}
    }
    for child in each_child(node) {
        walk_ts(w, child, depth + 1);
    }
    if pushed {
        w.stack.pop();
    }
}

/// Named import bindings of a TS/JS `import_statement` or
/// `export_statement ... from "..."`:
///
/// - `import { greet, farewell } from "./a"` binds `greet` and `farewell`;
/// - `import { greet as hi } from "./a"` binds `hi` to the source `greet`;
/// - `import greet from "./a"` binds `greet` (a default import);
/// - `import * as ns from "./a"` binds nothing (a wildcard);
/// - `import greet, { helper } from "./a"` binds both;
/// - `export { greet } from "./a"` binds `greet` (a re-export keeps the
///   source name on both sides).
///
/// A wildcard or an unparseable form gives no bindings, and resolution then
/// falls back to the repository-wide unique name.
fn ts_import_bindings(w: &Walker, node: Node) -> Vec<ImportBinding> {
    let mut bindings = Vec::new();
    for child in each_child(node) {
        match child.kind() {
            "import_clause" => {
                for sub in each_child(child) {
                    match sub.kind() {
                        "named_imports" => {
                            for spec in each_child(sub) {
                                if spec.kind() == "import_specifier"
                                    && let Some(name) = sub_field_text(w, spec, "name")
                                {
                                    bindings.push(match sub_field_text(w, spec, "alias") {
                                        Some(alias) => ImportBinding::aliased(name, alias),
                                        None => ImportBinding::named(name),
                                    });
                                }
                            }
                        }
                        "identifier" => {
                            let name = w.text(sub);
                            if !name.is_empty() {
                                bindings.push(ImportBinding::named(name));
                            }
                        }
                        "namespace_import" | "import_namespace_clause" => return Vec::new(),
                        _ => {}
                    }
                }
            }
            "export_clause" => {
                for spec in each_child(child) {
                    if spec.kind() == "export_specifier"
                        && let Some(name) = sub_field_text(w, spec, "name")
                    {
                        bindings.push(ImportBinding::named(name));
                    }
                }
            }
            _ => {}
        }
    }
    bindings
}

fn sub_field_text(w: &Walker, node: Node, field: &str) -> Option<String> {
    let text = w.text(node.child_by_field_name(field)?);
    if text.is_empty() { None } else { Some(text) }
}

// --- JSX helpers ---------------------------------------------------------

/// The call a JSX tag compiles to, as `(name, receiver)`: `<Button/>` renders
/// the `Button` component, `<Menu.Item>` the `Item` member of `Menu`. A
/// lowercase or namespaced tag (`div`, `svg:rect`) is an intrinsic element
/// and renders no symbol. Without this edge every component that is only
/// rendered, never called, had no callers.
fn jsx_component_call(tag: &str) -> Option<(String, Option<String>)> {
    if tag.contains(':') {
        return None;
    }
    match tag.rsplit_once('.') {
        Some((receiver, name)) if !receiver.is_empty() && !name.is_empty() => {
            Some((name.to_owned(), Some(receiver.to_owned())))
        }
        Some(_) => None,
        None => tag
            .starts_with(|c: char| c.is_ascii_uppercase())
            .then(|| (tag.to_owned(), None)),
    }
}

/// `jsx_attribute` has no named fields and names like `aria-label` parse as
/// namespace names: use the raw attribute text up to the first `=`.
fn jsx_attr_name(w: &Walker, attr: Node) -> Option<String> {
    let raw = w.text(attr);
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    raw.split('=').next().map(|s| s.trim().to_owned())
}

/// A reference for each JSX event-handler prop: `onClick={handler}`,
/// `onSubmit={this.save}`. Without it a handler like `handleSubmit` has no
/// edge and reads as dead code although the element wires it. Only bare
/// identifiers and member expressions count: an inline arrow
/// (`onClick={() => f()}`) is walked as ordinary code, so its calls are
/// already extracted.
fn jsx_handler_refs(w: &mut Walker, element: Node, tag: &str) {
    for child in each_child(element) {
        if child.kind() != "jsx_attribute" {
            continue;
        }
        let Some(attr) = jsx_attr_name(w, child) else {
            continue;
        };
        if !attr.to_lowercase().starts_with("on") {
            continue;
        }
        let arg_of = Some(format!("{tag}.{attr}"));
        for part in each_child(child) {
            if part.kind() != "jsx_expression" {
                continue;
            }
            for expr in each_child(part) {
                match expr.kind() {
                    "identifier" => {
                        let name = w.text(expr);
                        w.push_reference(name, expr, arg_of.clone());
                    }
                    "member_expression" => {
                        if let Some(prop) = expr.child_by_field_name("property") {
                            let name = w.text(prop);
                            w.push_reference(name, prop, arg_of.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
