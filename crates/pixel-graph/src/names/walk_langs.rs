// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The syntax walkers of Python, Go, Java, C# and the generic table (PHP, C,
//! Swift, Elixir, Lua, Kotlin). Lifted from Pixel's `pixel-graph`
//! `extract.rs`; every site's line is the line of the callee's name, and an
//! `interface` is recorded as a `Trait`.

use tree_sitter::Node;

use pixel_model::SymbolKind;

use super::walk::{
    ImportBinding, MAX_DEPTH, Walker, each_child, field_text, line_start, strip_quotes,
    walk_call_arguments,
};

// --- Go ------------------------------------------------------------------

pub(super) fn walk_go(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    match node.kind() {
        "function_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                w.push_symbol(name, SymbolKind::Function, node);
            }
        }
        "method_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let receiver = node
                    .child_by_field_name("receiver")
                    .and_then(|r| first_descendant_of_kind(r, "type_identifier"))
                    .map(|n| w.text(n));
                let qualified = match &receiver {
                    Some(r) => format!("{r}.{name}"),
                    None => name,
                };
                w.push_symbol(qualified, SymbolKind::Method, node);
            }
        }
        "type_spec" => {
            if let (Some(name), Some(ty)) = (
                field_text(w, node, "name"),
                node.child_by_field_name("type"),
            ) {
                match ty.kind() {
                    "struct_type" => w.push_symbol(name, SymbolKind::Struct, node),
                    "interface_type" => w.push_symbol(name, SymbolKind::Trait, node),
                    _ => {}
                }
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
                    "selector_expression" => {
                        if let Some(field) = f.child_by_field_name("field") {
                            let name = w.text(field);
                            let recv = field_text(w, f, "operand");
                            callee_name = Some(name.clone());
                            w.push_call(name, recv, field);
                        }
                    }
                    _ => {}
                }
            }
            walk_call_arguments(w, node, callee_name);
        }
        "import_spec" => {
            if let Some(path) = node.child_by_field_name("path") {
                let spec = strip_quotes(&w.text(path));
                w.push_import(spec, node, Vec::new());
            }
        }
        _ => {}
    }
    for child in each_child(node) {
        walk_go(w, child, depth + 1);
    }
}

fn first_descendant_of_kind<'t>(n: Node<'t>, kind: &str) -> Option<Node<'t>> {
    if n.kind() == kind {
        return Some(n);
    }
    each_child(n)
        .into_iter()
        .find_map(|child| first_descendant_of_kind(child, kind))
}

// --- Java ----------------------------------------------------------------

pub(super) fn walk_java(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut pushed = false;
    match node.kind() {
        "class_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(q, SymbolKind::Class, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "interface_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(q, SymbolKind::Trait, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "enum_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(q, SymbolKind::Enum, node);
            }
        }
        "method_declaration" | "constructor_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(q, SymbolKind::Method, node);
            }
        }
        "method_invocation" => {
            let mut callee_name: Option<String> = None;
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = w.text(name_node);
                let recv = field_text(w, node, "object");
                callee_name = Some(name.clone());
                w.push_call(name, recv, name_node);
            }
            walk_call_arguments(w, node, callee_name);
        }
        "object_creation_expression" => {
            if let Some(ty) = node.child_by_field_name("type") {
                let text = w.text(ty);
                w.push_call(constructed_type(&text), None, ty);
            }
            // `new Foo(handler)`: constructor arguments can be callbacks too.
            walk_call_arguments(w, node, None);
        }
        "import_declaration" => {
            let mut spec = String::new();
            let mut star = false;
            for child in each_child(node) {
                match child.kind() {
                    "scoped_identifier" | "identifier" => spec = w.text(child),
                    "asterisk" => star = true,
                    _ => {}
                }
            }
            if star && !spec.is_empty() {
                spec.push_str(".*");
            }
            w.push_import(spec, node, Vec::new());
        }
        _ => {}
    }
    for child in each_child(node) {
        walk_java(w, child, depth + 1);
    }
    if pushed {
        w.stack.pop();
    }
}

/// The type a `new` expression constructs, without generic arguments or its
/// package path: `new java.util.ArrayList<String>()` constructs `ArrayList`.
fn constructed_type(text: &str) -> String {
    let base = text.split('<').next().unwrap_or(text);
    base.rsplit('.').next().unwrap_or(base).trim().to_owned()
}

// --- Python --------------------------------------------------------------

pub(super) fn walk_python(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut pushed = false;
    match node.kind() {
        "class_definition" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(q, SymbolKind::Class, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "function_definition" => {
            if let Some(name) = field_text(w, node, "name") {
                let (kind, q) = if w.stack.is_empty() {
                    (SymbolKind::Function, name)
                } else {
                    (SymbolKind::Method, w.qualify(&name, "."))
                };
                w.push_symbol(q, kind, node);
            }
        }
        "call" => {
            let mut callee_name: Option<String> = None;
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => {
                        let name = w.text(f);
                        callee_name = Some(name.clone());
                        w.push_call(name, None, f);
                    }
                    "attribute" => {
                        if let Some(attribute) = f.child_by_field_name("attribute") {
                            let name = w.text(attribute);
                            let recv = field_text(w, f, "object");
                            callee_name = Some(name.clone());
                            w.push_call(name, recv, attribute);
                        }
                    }
                    _ => {}
                }
            }
            walk_call_arguments(w, node, callee_name);
        }
        "import_statement" => {
            for child in each_child(node) {
                match child.kind() {
                    "dotted_name" => {
                        let spec = w.text(child);
                        w.push_import(spec, node, Vec::new());
                    }
                    "aliased_import" => {
                        if let Some(name) = child.child_by_field_name("name") {
                            let spec = w.text(name);
                            w.push_import(spec, node, Vec::new());
                        }
                    }
                    _ => {}
                }
            }
        }
        "import_from_statement" => {
            if let Some(m) = node.child_by_field_name("module_name") {
                let spec = w.text(m);
                let bindings = python_from_bindings(w, node);
                w.push_import(spec, node, bindings);
            }
        }
        _ => {}
    }
    for child in each_child(node) {
        walk_python(w, child, depth + 1);
    }
    if pushed {
        w.stack.pop();
    }
}

/// The names a `from m import a, b as c` binds: `a` and `c` (the latter
/// bound to the source `b`). A wildcard import binds nothing.
fn python_from_bindings(w: &Walker, node: Node) -> Vec<ImportBinding> {
    let module = node.child_by_field_name("module_name");
    let mut bindings = Vec::new();
    for child in each_child(node) {
        if Some(child) == module {
            continue;
        }
        match child.kind() {
            "wildcard_import" => return Vec::new(),
            "dotted_name" => {
                let name = w.text(child);
                if !name.contains('.') {
                    bindings.push(ImportBinding::named(name));
                }
            }
            "aliased_import" => {
                if let (Some(name), Some(alias)) = (
                    child.child_by_field_name("name"),
                    child.child_by_field_name("alias"),
                ) {
                    bindings.push(ImportBinding::aliased(w.text(name), w.text(alias)));
                }
            }
            _ => {}
        }
    }
    bindings
}

// --- C# -------------------------------------------------------------------

pub(super) fn walk_csharp(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut pushed = false;
    match node.kind() {
        "namespace_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(q, SymbolKind::Module, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "class_declaration" | "record_declaration" | "struct_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(q, SymbolKind::Class, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "interface_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(q, SymbolKind::Trait, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "enum_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(q, SymbolKind::Enum, node);
            }
        }
        "delegate_declaration"
        | "method_declaration"
        | "constructor_declaration"
        | "local_function_statement"
        | "property_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(q, SymbolKind::Method, node);
            }
        }
        "invocation_expression" => {
            let mut callee_name: Option<String> = None;
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => {
                        let name = w.text(f);
                        callee_name = Some(name.clone());
                        w.push_call(name, None, f);
                    }
                    "member_access_expression" => {
                        if let Some(name_node) = f.child_by_field_name("name") {
                            let name = csharp_simple_name(w, name_node);
                            let recv = field_text(w, f, "expression");
                            callee_name = Some(name.clone());
                            w.push_call(name, recv, name_node);
                        }
                    }
                    "generic_name" => {
                        let name = csharp_simple_name(w, f);
                        callee_name = Some(name.clone());
                        w.push_call(name, None, f);
                    }
                    _ => {}
                }
            }
            walk_call_arguments(w, node, callee_name);
        }
        "object_creation_expression" => {
            if let Some(ty) = node.child_by_field_name("type") {
                let text = w.text(ty);
                w.push_call(constructed_type(&text), None, ty);
            }
            walk_call_arguments(w, node, None);
        }
        "using_directive" => {
            // The `name` field exists for alias usings (`using Foo = X;`) and
            // holds the alias, not the imported namespace; the namespace is a
            // plain child. The alias is an `identifier` too, so it is skipped
            // by position.
            let alias = node.child_by_field_name("name");
            for child in each_child(node) {
                if Some(child) == alias {
                    continue;
                }
                if matches!(child.kind(), "qualified_name" | "identifier") {
                    let spec = w.text(child);
                    w.push_import(spec, node, Vec::new());
                }
            }
        }
        _ => {}
    }
    for child in each_child(node) {
        walk_csharp(w, child, depth + 1);
    }
    if pushed {
        w.stack.pop();
    }
}

fn csharp_simple_name(w: &Walker, node: Node) -> String {
    if node.kind() == "generic_name"
        && let Some(id) = each_child(node)
            .into_iter()
            .find(|c| c.kind() == "identifier")
    {
        return w.text(id);
    }
    w.text(node)
}

// --- Generic heuristic walker --------------------------------------------
//
// Languages without a hand-written walker (PHP, C, Swift, Elixir, Lua,
// Kotlin) get a node-kind heuristic pass: node kinds ending in
// `_declaration`/`_definition` for symbols, call/invocation node kinds for
// call sites, import/use/require node kinds for import specs. Deliberately
// coarse: a less precise graph beats an absent one. Kotlin through it:
// classes, `object`s and functions (members qualified by their class),
// imports by their qualified name, calls with their receiver; an `interface`
// or `enum class` is recorded as a class, `val`/`var` properties are not
// symbols, and an extension function is named without its receiver type.

pub(super) fn walk_generic(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let kind = node.kind();
    let mut pushed = false;

    // Symbols: declarations and definitions carry a name we can qualify.
    if (kind.ends_with("_declaration") || kind.ends_with("_definition"))
        && let Some((symbol_kind, is_container)) = generic_symbol_kind(kind)
        && let Some(name) = generic_name(w, node)
    {
        let q = w.qualify(&name, ".");
        w.push_symbol(q, symbol_kind, node);
        if is_container {
            w.stack.push(name);
            pushed = true;
        }
    }

    // Elixir defines through macros: `defmodule M do` and `def f(x) do` are
    // `call` nodes whose target is the definer.
    if let Some((name, symbol_kind, is_container)) = elixir_definition(w, node) {
        let q = w.qualify(&name, ".");
        w.push_symbol(q, symbol_kind, node);
        if is_container {
            w.stack.push(name);
            pushed = true;
        }
    }

    // Calls: any node kind mentioning call/invocation is a candidate site.
    if kind.contains("call") || kind.contains("invocation") {
        generic_call(w, node);
    }

    // Imports: import/use/require node kinds.
    if kind.contains("import") || kind.starts_with("use_") || kind.starts_with("require") {
        generic_import(w, node);
    }

    for child in each_child(node) {
        walk_generic(w, child, depth + 1);
    }
    if pushed {
        w.stack.pop();
    }
}

/// Classifies a `_declaration`/`_definition` node kind into a `SymbolKind`,
/// plus whether the declared item nests further symbols (a container: class,
/// struct, interface, trait, protocol, namespace, module, package). Bare
/// declaration kinds (import, use, attribute, parameter, preproc, deinit,
/// typealias) inject neither symbols nor qualification.
fn generic_symbol_kind(kind: &str) -> Option<(SymbolKind, bool)> {
    if !(kind.ends_with("_declaration") || kind.ends_with("_definition")) {
        return None;
    }
    if kind.contains("import")
        || kind.contains("use")
        || kind.starts_with("namespace_use")
        || kind.contains("attribute")
        || kind.contains("parameter")
        || kind.contains("argument")
        || kind.starts_with("preproc")
        || kind.contains("deinit")
        || kind.contains("typealias")
        || kind.contains("associatedtype")
        || kind.contains("operator")
    {
        return None;
    }
    if kind.contains("function")
        || kind.contains("method")
        || kind.contains("lambda")
        || kind.contains("closure")
    {
        return Some((SymbolKind::Function, false));
    }
    if kind.contains("namespace")
        || kind.contains("module")
        || kind.contains("package")
        || kind.contains("library")
    {
        return Some((SymbolKind::Module, true));
    }
    // Kotlin's `object Foo` (`object_declaration`) is a singleton class.
    if kind.contains("class")
        || kind.contains("struct")
        || kind.contains("record")
        || kind.contains("actor")
        || kind.contains("object")
    {
        return Some((SymbolKind::Class, true));
    }
    if kind.contains("interface") || kind.contains("trait") || kind.contains("protocol") {
        return Some((SymbolKind::Trait, true));
    }
    if kind.contains("enum") {
        return Some((SymbolKind::Enum, false));
    }
    None
}

/// Best-effort symbol name for a declaration node: a `name` field, then a
/// `declarator` field (a C function definition's nested declarator), then
/// the first identifier-like child (Kotlin's `simple_identifier`).
fn generic_name(w: &Walker, node: Node) -> Option<String> {
    if let Some(name) = node.child_by_field_name("name") {
        let t = w.text(name);
        if !t.is_empty() && !t.contains(['(', ')', ',']) {
            return Some(t);
        }
    }
    // C: `function_definition` → `function_declarator` (or a pointer
    // declarator around it) → `identifier`, a child of the declarator.
    if let Some(decl) = node.child_by_field_name("declarator")
        && let Some(n) = generic_name(w, decl)
    {
        return Some(n);
    }
    for child in each_child(node) {
        if matches!(
            child.kind(),
            "simple_identifier" | "identifier" | "type_identifier" | "name"
        ) {
            let t = w.text(child);
            if !t.is_empty() && !t.contains(['(', ')', ',']) {
                return Some(t);
            }
        }
    }
    None
}

/// Extracts a callee and an optional receiver from a generic call node:
/// direct identifiers (`function`/`callee`/`name`/`method`/`target` fields)
/// and member accesses (PHP `member_call_expression`, C `field_expression`).
fn generic_call(w: &mut Walker, node: Node) {
    // Elixir `def`/`defmodule …` and the head a definer defines (`total(x)`
    // in `def total(x) do`) are definitions, not invocation sites.
    if elixir_definer(w, node) || elixir_definition_head(w, node) {
        return;
    }
    let mut callee: Option<String> = None;
    let mut receiver: Option<String> = None;
    // The node whose line the site takes: the callee's name.
    let mut at = node;
    // Swift's `call_expression` names no field: the callee is its first
    // named child, before the `call_suffix`.
    let expr = ["function", "callee", "name", "method", "target"]
        .iter()
        .find_map(|f| node.child_by_field_name(f))
        .or_else(|| {
            (node.kind() == "call_expression")
                .then(|| node.named_child(0))
                .flatten()
        });
    if let Some(e) = expr {
        match e.kind() {
            "identifier" | "simple_identifier" | "name" | "type_identifier" | "dotted_name"
            | "qualified_name" | "namespace_name" | "escaped_identifier" | "variable" => {
                callee = Some(w.text(e));
                at = e;
                // The callee itself is no receiver: Elixir's `round(x)` names
                // its callee in `target`, a field this list also reads.
                receiver = ["receiver", "object", "scope", "target"]
                    .iter()
                    .find_map(|f| node.child_by_field_name(f))
                    .filter(|r| *r != e)
                    .map(|r| w.text(r));
            }
            k if k.ends_with("_expression")
                || k.ends_with("_selector")
                || k.contains("member")
                || k.contains("attribute")
                || k.contains("index")
                || k.contains("access") =>
            {
                // Swift: `navigation_expression` → `suffix` field
                // (`navigation_suffix`) → its own `suffix` identifier.
                let pos_name = ["property", "field", "name", "attribute", "member"]
                    .iter()
                    .find_map(|f| e.child_by_field_name(f))
                    .or_else(|| {
                        e.child_by_field_name("suffix")
                            .and_then(|s| s.child_by_field_name("suffix"))
                    });
                // C's `field_expression` holds its operand in `argument`.
                let pos_recv = [
                    "object",
                    "operand",
                    "scope",
                    "expression",
                    "value",
                    "target",
                    "argument",
                ]
                .iter()
                .find_map(|f| e.child_by_field_name(f));
                callee = pos_name.map(|n| w.text(n));
                if let Some(n) = pos_name {
                    at = n;
                }
                receiver = pos_recv.map(|r| w.text(r));
                // Kotlin's `navigation_expression` names no field: the
                // receiver is its first named child, the member its last.
                if callee.is_none()
                    && e.named_child_count() == 2
                    && let (Some(recv), Some(member)) = (e.named_child(0), e.named_child(1))
                    && member.kind() == "identifier"
                {
                    callee = Some(w.text(member));
                    at = member;
                    receiver = Some(w.text(recv));
                }
            }
            _ => {}
        }
    }
    // A PHP `scoped_call_expression` names a qualified path; fall back to the
    // `target` field's text.
    if callee.is_none()
        && let Some(t) = node.child_by_field_name("target")
    {
        callee = Some(w.text(t));
        at = t;
    }
    if let Some(callee) = callee {
        // A call spread over lines is on the line of its callee's name; a
        // callee that is no node of its own is on the call's first line.
        let at = if line_start(at) >= line_start(node) {
            at
        } else {
            node
        };
        w.push_call(callee.clone(), receiver, at);
        walk_call_arguments(w, node, Some(callee));
    }
}

/// Elixir macros that define rather than call: a `call` whose `target` is one
/// of them is a definition site.
const ELIXIR_DEFINERS: &[&str] = &[
    "def",
    "defp",
    "defmacro",
    "defmacrop",
    "defguard",
    "defguardp",
    "defdelegate",
    "defmodule",
    "defprotocol",
    "defimpl",
];

/// True for an Elixir `call` whose target is a definer (`def`, `defmodule`…).
fn elixir_definer(w: &Walker, node: Node) -> bool {
    node.kind() == "call"
        && node
            .child_by_field_name("target")
            .is_some_and(|t| ELIXIR_DEFINERS.contains(&w.text(t).as_str()))
}

/// What an Elixir definer defines: a module (a container) or a function
/// named by the head in its first argument (`total(x)`, `total(x) when …`,
/// or a bare `total`).
fn elixir_definition(w: &Walker, node: Node) -> Option<(String, SymbolKind, bool)> {
    if !elixir_definer(w, node) {
        return None;
    }
    let definer = w.text(node.child_by_field_name("target")?);
    let args = each_child(node)
        .into_iter()
        .find(|c| c.kind() == "arguments")?;
    let mut head = args.named_child(0)?;
    if head.kind() == "binary_operator" {
        head = head.child_by_field_name("left")?;
    }
    match definer.as_str() {
        "defmodule" | "defprotocol" | "defimpl" => Some((w.text(head), SymbolKind::Module, true)),
        _ => {
            let name = match head.kind() {
                "call" => w.text(head.child_by_field_name("target")?),
                "identifier" => w.text(head),
                _ => return None,
            };
            Some((name, SymbolKind::Function, false))
        }
    }
}

/// True for the head a definer defines: `total(x)` in `def total(x) do`, also
/// behind a guard (`def total(x) when is_integer(x)`, where the head is the
/// left operand of `when`). The guard's own calls stay call sites.
fn elixir_definition_head(w: &Walker, node: Node) -> bool {
    let mut up = node.parent();
    if let Some(guard) = up.filter(|p| p.kind() == "binary_operator")
        && guard.child_by_field_name("left") == Some(node)
    {
        up = guard.parent();
    }
    up.filter(|args| args.kind() == "arguments")
        .and_then(|args| args.parent())
        .is_some_and(|def| elixir_definer(w, def))
}

/// Best-effort import spec from import/use/require node kinds: the first
/// plain string or identifier child (PHP `require_expression`, Kotlin
/// `import_header`, Swift `import_declaration`).
fn generic_import(w: &mut Walker, node: Node) {
    for child in each_child(node) {
        match child.kind() {
            // PHP parses a double-quoted path as `encapsed_string`. An
            // interpolated one (`"lib/$name.php"`) is chosen at run time:
            // only a double-quoted path made of plain text is an import.
            "string" | "encapsed_string"
                if each_child(child)
                    .into_iter()
                    .filter(Node::is_named)
                    .all(|c| c.kind() == "string_content") =>
            {
                let spec = strip_quotes(&w.text(child));
                if !spec.is_empty() {
                    w.push_import(spec, node, Vec::new());
                    return;
                }
            }
            // Kotlin's `import a.b.C` holds a `qualified_identifier`.
            "identifier"
            | "dotted_name"
            | "qualified_name"
            | "namespace_name"
            | "qualified_identifier" => {
                let spec = w.text(child);
                if !spec.is_empty() {
                    w.push_import(spec, node, Vec::new());
                    return;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use pixel_model::EdgeKind;

    use crate::codec;
    use crate::lang::Rules;
    use crate::names::{self, Names};

    /// `(name, kind, line)` of every definition.
    fn defs(rules: &Names, path: &str, source: &str) -> Vec<String> {
        rules
            .extract(path, source.as_bytes())
            .defs
            .iter()
            .map(|d| format!("{}:{:?}@{}", d.name, d.kind, d.line))
            .collect()
    }

    /// `kind target @line` of every site, sorted by line then text.
    fn sites(rules: &Names, path: &str, source: &str) -> Vec<String> {
        let mut sites: Vec<(u32, String)> = rules
            .extract(path, source.as_bytes())
            .sites
            .iter()
            .map(|s| (s.line, format!("{:?} {}", s.kind, s.target)))
            .collect();
        sites.sort();
        sites
            .into_iter()
            .map(|(l, t)| format!("{t} @{l}"))
            .collect()
    }

    #[test]
    fn python_defines_classes_and_qualified_methods_and_reads_its_imports() {
        let src = "import os.path\nfrom .a import b as c, d\nclass K:\n    def m(self):\n        self.x(c)\n        helper()\ndef helper():\n    pass\n";
        assert_eq!(
            defs(&names::PYTHON, "a.py", src),
            ["K:Class@3", "K.m:Method@4", "helper:Function@7"]
        );
        assert_eq!(
            sites(&names::PYTHON, "a.py", src),
            [
                "Import os.path @1",
                "Import .a @2",
                "Call self.x @5",
                "Reference c @5",
                "Call helper @6",
            ]
        );
        let facts = names::PYTHON.extract("a.py", src.as_bytes()).facts;
        let imports = crate::names::resolve::decode_facts(&facts);
        assert_eq!(imports[1].spec, ".a");
        assert_eq!(
            imports[1].bindings,
            [
                ("c".to_owned(), "b".to_owned()),
                ("d".to_owned(), "d".to_owned())
            ]
        );
    }

    #[test]
    fn go_qualifies_methods_by_receiver_and_types_structs_and_interfaces() {
        let src = "package a\nimport \"fmt\"\ntype S struct{}\ntype I interface{}\nfunc (s *S) M() {\n\tfmt.Println(1)\n\thelper()\n}\nfunc helper() {}\n";
        assert_eq!(
            defs(&names::GO, "a.go", src),
            [
                "S:Struct@3",
                "I:Trait@4",
                "S.M:Method@5",
                "helper:Function@9"
            ]
        );
        assert_eq!(
            sites(&names::GO, "a.go", src),
            ["Import fmt @2", "Call fmt.Println @6", "Call helper @7"]
        );
    }

    #[test]
    fn java_records_constructions_wildcard_imports_and_callback_arguments() {
        let src = "import java.util.List;\nimport a.b.*;\nclass A {\n  void m() {\n    new Foo<X>(h);\n    this.n();\n    f();\n  }\n  void n() {}\n}\ninterface I {}\n";
        assert_eq!(
            defs(&names::JAVA, "A.java", src),
            ["A:Class@3", "A.m:Method@4", "A.n:Method@9", "I:Trait@11"]
        );
        assert_eq!(
            sites(&names::JAVA, "A.java", src),
            [
                "Import java.util.List @1",
                "Import a.b.* @2",
                "Call Foo @5",
                "Reference h @5",
                "Call this.n @6",
                "Call f @7",
            ]
        );
    }

    #[test]
    fn csharp_qualifies_by_namespace_and_names_generic_calls_without_arguments() {
        let src = "using System;\nusing X = Y;\nnamespace N {\nclass A {\n  void M() {\n    var l = new List<int>();\n    this.N2();\n    Helper<int>();\n  }\n}\n}\n";
        assert_eq!(
            defs(&names::CSHARP, "A.cs", src),
            ["N:Module@3", "N.A:Class@4", "N.A.M:Method@5"]
        );
        assert_eq!(
            sites(&names::CSHARP, "A.cs", src),
            [
                "Import System @1",
                "Import Y @2",
                "Call List @6",
                "Call this.N2 @7",
                "Call Helper @8",
            ]
        );
    }

    #[test]
    fn the_generic_table_reads_definitions_calls_and_imports() {
        assert_eq!(
            defs(
                &names::PHP,
                "a.php",
                "<?php\nfunction f() { g(); $o->m(); }\nclass C { function m() {} }\nrequire 'x.php';\n"
            ),
            ["f:Function@2", "C:Class@3", "C.m:Function@3"]
        );
        assert_eq!(
            sites(
                &names::PHP,
                "a.php",
                "<?php\nfunction f() { g(); $o->m(); }\nrequire 'x.php';\n"
            ),
            ["Call $o.m @2", "Call g @2", "Import x.php @3"]
        );
        assert_eq!(
            sites(
                &names::C,
                "a.c",
                "#include <x.h>\nint f(int a) {\n  g(a);\n  s.m();\n  return 0;\n}\n"
            ),
            ["Call g @3", "Reference a @3", "Call s.m @4"]
        );
        assert_eq!(
            defs(
                &names::SWIFT,
                "a.swift",
                "import Foundation\nclass A {\n  func m() {\n    helper()\n  }\n}\nfunc helper() {}\n"
            ),
            ["A:Class@2", "A.m:Function@3", "helper:Function@7"]
        );
        assert_eq!(
            defs(
                &names::LUA,
                "a.lua",
                "local function f()\n  g()\nend\nfunction g() end\n"
            ),
            ["f:Function@1", "g:Function@4"]
        );
        assert_eq!(
            defs(
                &names::KOTLIN,
                "a.kt",
                "import a.b.C\nclass A {\n  fun m() {\n    helper()\n  }\n}\nfun helper() {}\nobject O\n"
            ),
            [
                "A:Class@2",
                "A.m:Function@3",
                "helper:Function@7",
                "O:Class@8"
            ]
        );
    }

    #[test]
    fn elixir_definers_define_and_are_not_calls() {
        let src = "defmodule M do\n  def total(x) when is_integer(x) do\n    round(x)\n  end\n  defp bare, do: 1\nend\n";
        assert_eq!(
            defs(&names::ELIXIR, "a.ex", src),
            ["M:Module@1", "M.total:Function@2", "M.bare:Function@5"]
        );
        assert_eq!(
            sites(&names::ELIXIR, "a.ex", src),
            ["Call is_integer @2", "Call round @3"]
        );
    }

    #[test]
    fn a_file_that_is_not_the_languages_is_not_wanted() {
        assert!(names::PYTHON.wants("pkg/a.py") && !names::PYTHON.wants("a.pyc"));
        assert!(names::C.wants("a.h") && !names::C.wants("a.hpp"));
        assert!(names::KOTLIN.wants("build.gradle.kts"));
        assert!(!names::GO.wants("go.mod") && !names::JAVA.wants("A.class"));
    }

    #[test]
    fn a_site_context_names_its_form_in_every_language() {
        for (rules, path, src) in [
            (&names::PYTHON, "a.py", "def f():\n    a.b()\n"),
            (&names::GO, "a.go", "package p\nfunc f() { a.b() }\n"),
            (&names::JAVA, "A.java", "class A { void f() { a.b(); } }\n"),
            (&names::CSHARP, "A.cs", "class A { void F() { a.b(); } }\n"),
        ] {
            let x = rules.extract(path, src.as_bytes());
            let call = x
                .sites
                .iter()
                .find(|s| s.kind == EdgeKind::Call)
                .unwrap_or_else(|| panic!("{path} has a call"));
            assert_eq!(codec::fields(&call.context), ["c", "b", "a"], "{path}");
        }
    }
}
