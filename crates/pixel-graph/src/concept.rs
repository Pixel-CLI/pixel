// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Engine 1 — concept extraction + normalization.
//!
//! A second "concept pass" alongside `extract::extract_file`: it produces
//! [`RawConcept`] rows with a closed [`ConceptKind`] enum. Concepts are the
//! human-meaningful labels an agent actually asks for ("the form", a pasted
//! label, "I'm getting a 503") that the symbol graph cannot see — JSX text,
//! attribute strings, string literals, components, forms, routes, HTTP status
//! codes, and config keys.
//!
//! Design notes (per PLAN.md Engine 1):
//! - Normalization: lowercase, NFC, collapse whitespace, trim punctuation.
//! - Skip norms > 200 chars and files > 1MB.
//! - Concepts get their own extension gate adding `.svelte/.vue/.html/.json/
//!   .yaml/.css` on top of the existing graph languages.
//! - Svelte/Vue: `<script>` blocks run through the TS walker with a line
//!   offset; markup goes through a small hand-rolled scanner (heuristic v1,
//!   upgradeable to tree-sitter-html).
//! - No FTS5 — exact-norm is a point lookup, word AND-intersection handles
//!   partial phrases, fuzzier falls to the trigram index.

use tree_sitter::{Language, Node, Parser};

use crate::extract::parse_bounded;

/// The closed set of concept kinds. Mirrors PLAN.md's Engine 1 table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConceptKind {
    /// JSX/TSX text nodes; markup inner text in html/svelte/vue.
    UiText,
    /// `placeholder,label,aria-label,title,alt,name,data-testid` attr strings.
    AttrText,
    /// String literals + static template parts; ALL args to error-ish calls.
    String,
    /// Uppercase JSX element names (usage side).
    Component,
    /// `<form>`/`<Form>` elements, `useForm/useFormik/createForm`, zod schemas.
    Form,
    /// File-path-derived + call-derived HTTP routes (one row per method).
    Route,
    /// Integer literals 100–599 in status positions.
    Status,
    /// JSON/YAML dotted key paths + string leaf values.
    ConfigKey,
    /// The literal name of an environment variable the code reads
    /// (`env::var("NAME")`, `env!("NAME")`): the name only, never a value
    /// or a default. A computed name is not one.
    EnvRead,
}

impl ConceptKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConceptKind::UiText => "ui_text",
            ConceptKind::AttrText => "attr_text",
            ConceptKind::String => "string",
            ConceptKind::Component => "component",
            ConceptKind::Form => "form",
            ConceptKind::Route => "route",
            ConceptKind::Status => "status",
            ConceptKind::ConfigKey => "config_key",
            ConceptKind::EnvRead => "env_read",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "ui_text" => ConceptKind::UiText,
            "attr_text" => ConceptKind::AttrText,
            "string" => ConceptKind::String,
            "component" => ConceptKind::Component,
            "form" => ConceptKind::Form,
            "route" => ConceptKind::Route,
            "status" => ConceptKind::Status,
            "env_read" => ConceptKind::EnvRead,
            _ => ConceptKind::ConfigKey,
        }
    }
}

/// One extracted concept, pre-normalization. `owner_symbol_id` is filled at
/// store time (the smallest enclosing symbol's id, when one exists).
#[derive(Debug, Clone)]
pub struct RawConcept {
    pub kind: ConceptKind,
    pub raw: String,
    pub norm: String,
    pub detail: String,
    pub start_line: u32,
    pub end_line: u32,
    pub owner_symbol_id: Option<i64>,
}

// --- limits ---------------------------------------------------------------

const MAX_NORM_CHARS: usize = 200;
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MIN_STRING_WORDS: usize = 3;
const MIN_STRING_CHARS: usize = 12;
const MAX_DEPTH: usize = 512;

const ATTR_NAMES: &[&str] = &[
    "placeholder",
    "label",
    "aria-label",
    "title",
    "alt",
    "name",
    "data-testid",
];

const HTTP_METHODS: &[&str] = &["get", "post", "put", "patch", "delete", "head", "options"];

// --- normalization --------------------------------------------------------

/// Normalize a concept's text: lowercase, NFC, collapse whitespace, trim
/// leading/trailing punctuation. Returns the normalized form (may be empty).
pub fn normalize(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let nfc: String = text.nfc().collect();
    let lower = nfc.to_lowercase();
    let collapsed = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace())
        .to_string()
}

/// True when a plain string literal is worth indexing (≥3 words AND ≥12 chars).
fn string_worth_indexing(text: &str) -> bool {
    text.split_whitespace().count() >= MIN_STRING_WORDS && text.chars().count() >= MIN_STRING_CHARS
}

/// True when a repo-relative path looks like a test file. String concepts are
/// skipped in test files to cut index noise (assertion messages, fixture
/// literals, generated snapshots, etc.). Matches `/tests?/` path segments,
/// `__tests__` directories, and `*_test.*` / `*.spec.*` / `*.test.*` files.
pub(crate) fn is_test_path(path: &str) -> bool {
    let file = path.rsplit('/').next().unwrap_or(path);
    if file.contains(".spec.") || file.contains(".test.") {
        return true;
    }
    if let Some((stem, _ext)) = file.rsplit_once('.')
        && stem.ends_with("_test")
    {
        return true;
    }
    path.split('/')
        .any(|seg| seg == "tests" || seg == "test" || seg == "__tests__")
}

/// The inverted-index words for a normalized concept: split on non-alphanumeric
/// boundaries, lowercased, deduped, keeping words of length ≥ 2. Used to build
/// the `concept_words` table and to match T1/T2 word-intersection queries.
pub fn concept_words(norm: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for chunk in norm.split(|c: char| !c.is_alphanumeric()) {
        let w = chunk.to_lowercase();
        if w.len() >= 2 && seen.insert(w.clone()) {
            out.push(w);
        }
    }
    out
}

// --- extension gate -------------------------------------------------------

/// Language tag for a repo-relative path under the *concept* gate: the graph
/// languages plus `.svelte/.vue/.html/.json/.yaml/.css`. `None` if unsupported.
pub fn concept_lang_of(path: &str) -> Option<&'static str> {
    let file = path.rsplit('/').next().unwrap_or(path);
    if crate::extract::RUBY_FILE_NAMES.contains(&file) {
        return Some("ruby");
    }
    let ext = file.rsplit_once('.')?.1;
    match ext {
        "ts" | "mts" | "cts" => Some("ts"),
        "tsx" => Some("tsx"),
        "js" | "jsx" | "mjs" | "cjs" => Some("js"),
        "rs" => Some("rust"),
        "go" => Some("go"),
        "java" => Some("java"),
        "py" => Some("python"),
        "rb" | "rake" | "gemspec" | "ru" => Some("ruby"),
        "svelte" => Some("svelte"),
        "vue" => Some("vue"),
        "html" => Some("html"),
        "json" => Some("json"),
        "yaml" | "yml" => Some("yaml"),
        "css" => Some("css"),
        _ => None,
    }
}

// --- top-level entry ------------------------------------------------------

/// Extract concepts from one file. Empty on unsupported language, oversized
/// files (>1MB), a generated/minified blob, or any parse/grammar failure
/// (graceful degradation).
pub fn extract_concepts(path_rel: &str, content: &[u8]) -> Vec<RawConcept> {
    if content.len() > MAX_FILE_BYTES {
        return Vec::new();
    }
    // The size cap alone does not cover this: a 900 KB single-line bundle
    // sits under it and still costs hundreds of milliseconds per parse, and
    // this path runs several. Same deterministic predicate as symbol
    // extraction, so both agree on what counts as source.
    if crate::extract::is_generated_blob(content) {
        return Vec::new();
    }
    let Some(lang) = concept_lang_of(path_rel) else {
        return Vec::new();
    };
    let mut out = match lang {
        "ts" | "tsx" | "js" => extract_ts(path_rel, content),
        "svelte" | "vue" => extract_svelte_vue(path_rel, content),
        "html" => extract_html(content),
        "json" => extract_json_config(content),
        "yaml" => extract_yaml_config(content),
        "css" => extract_css(content),
        "rust" => extract_rust(path_rel, content),
        // go/java/python/ruby have no concept sources defined in PLAN.md Engine 1.
        _ => Vec::new(),
    };
    out.extend(path_routes(path_rel, content));
    out
}

// --- shared tree-sitter walker -------------------------------------------

struct TsWalker<'a> {
    src: &'a [u8],
    concepts: Vec<RawConcept>,
    /// Added to every emitted line number (for `<script>` blocks in
    /// svelte/vue, whose content is parsed in isolation).
    line_offset: u32,
    /// True when the source file is a test file; String concepts are skipped.
    test_path: bool,
}

impl<'a> TsWalker<'a> {
    fn text(&self, n: Node) -> String {
        String::from_utf8_lossy(&self.src[n.byte_range()]).into_owned()
    }

    fn line_start(&self, n: Node) -> u32 {
        n.start_position().row as u32 + 1 + self.line_offset
    }

    fn line_end(&self, n: Node) -> u32 {
        n.end_position().row as u32 + 1 + self.line_offset
    }

    fn call_args<'b>(&self, node: Node<'b>) -> Vec<Node<'b>> {
        node.child_by_field_name("arguments")
            .map(each_child)
            .unwrap_or_default()
    }

    fn push(&mut self, kind: ConceptKind, raw: String, detail: String, node: Node) {
        let norm = normalize(&raw);
        if norm.is_empty() || norm.len() > MAX_NORM_CHARS {
            return;
        }
        self.concepts.push(RawConcept {
            kind,
            raw,
            norm,
            detail,
            start_line: self.line_start(node),
            end_line: self.line_end(node),
            owner_symbol_id: None,
        });
    }

    fn push_string(&mut self, text: String, node: Node, always: bool) {
        if text.is_empty() {
            return;
        }
        if self.test_path {
            // Skip String concepts in test files (assertion messages,
            // fixture literals, snapshots) to cut index noise.
            return;
        }
        if !always && !string_worth_indexing(&text) {
            return;
        }
        self.push(ConceptKind::String, text, String::new(), node);
    }

    fn push_ui_text(&mut self, text: String, node: Node) {
        self.push(ConceptKind::UiText, text, String::new(), node);
    }

    fn push_attr_text(&mut self, attr: String, text: String, node: Node) {
        self.push(ConceptKind::AttrText, text, attr, node);
    }

    fn push_component(&mut self, name: String, node: Node) {
        self.push(ConceptKind::Component, name, "component".into(), node);
    }

    fn push_form(&mut self, node: Node) {
        self.push(ConceptKind::Form, "form".into(), "form".into(), node);
    }

    /// Push the literal environment-variable name `arg` holds, with `detail`
    /// saying how it is read. Nothing for a computed or non-identifier name.
    fn push_env_read(&mut self, arg: Node, detail: &str, node: Node) {
        if arg.kind() != "string_literal" {
            return;
        }
        let name = strip_quotes(&self.text(arg));
        if !is_env_name(&name) {
            return;
        }
        self.push(ConceptKind::EnvRead, name, detail.to_string(), node);
    }

    fn push_status(&mut self, n: i64, node: Node) {
        let raw = n.to_string();
        self.push(ConceptKind::Status, raw, format!("status {n}"), node);
    }

    /// Push every string argument of an error-ish call (`Error()`, `throw`,
    /// `toast`, `alert`, `console.error`, `panic!`) — always, regardless of
    /// length.
    fn push_error_args(&mut self, node: Node) {
        for arg in self.call_args(node) {
            if arg.kind() == "string" || arg.kind() == "string_literal" {
                let text = strip_quotes(&self.text(arg));
                self.push_string(text, arg, true);
            }
        }
    }

    fn push_res_status(&mut self, node: Node) {
        for arg in self.call_args(node) {
            if let Some(n) = parse_int(&self.text(arg))
                && (100..=599).contains(&n)
            {
                self.push_status(n, node);
            }
        }
    }

    fn push_abort_status(&mut self, node: Node) {
        for arg in self.call_args(node) {
            if let Some(n) = parse_int(&self.text(arg))
                && (100..=599).contains(&n)
            {
                self.push_status(n, node);
            }
        }
    }

    fn push_app_route(&mut self, node: Node, method: &str) {
        // The first named argument that is not a comment: `arguments` opens
        // on its `(` token, and a comment (`/* note */`) is a named extra.
        let path = self
            .call_args(node)
            .into_iter()
            .find(|a| a.is_named() && !a.is_extra())
            .map(|a| strip_quotes(&self.text(a)))
            .unwrap_or_default();
        let raw = format!("{method} {path}");
        self.push(ConceptKind::Route, raw, format!("{method} {path}"), node);
    }

    fn push_fetch_route(&mut self, node: Node) {
        for arg in self.call_args(node) {
            if arg.kind() == "string" {
                let path = strip_quotes(&self.text(arg));
                if path.starts_with("/api/") {
                    let raw = format!("fetch {path}");
                    self.push(ConceptKind::Route, raw, format!("fetch {path}"), node);
                }
            }
        }
    }

    fn handle_call(&mut self, node: Node) {
        let Some(f) = node.child_by_field_name("function") else {
            return;
        };
        match f.kind() {
            "identifier" => match self.text(f).as_str() {
                "Error" | "toast" | "alert" => self.push_error_args(node),
                "useForm" | "useFormik" | "createForm" => self.push_form(node),
                "fetch" => self.push_fetch_route(node),
                "abort" => self.push_abort_status(node),
                _ => {}
            },
            "member_expression" => {
                let recv = field_text(self, f, "object").unwrap_or_default();
                let prop = field_text(self, f, "property").unwrap_or_default();
                match (recv.trim(), prop.trim()) {
                    ("console", "error") => self.push_error_args(node),
                    ("toast", _) => self.push_error_args(node),
                    ("res", "status") => self.push_res_status(node),
                    ("app", m) if HTTP_METHODS.contains(&m) => self.push_app_route(node, m),
                    ("z", "object") => self.push_form(node),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn walk_ts_concepts(w: &mut TsWalker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    match node.kind() {
        "jsx_text" => {
            let text = w.text(node);
            w.push_ui_text(text, node);
        }
        "jsx_attribute" => {
            // NOTE: this tree-sitter-typescript grammar does NOT expose
            // `name`/`value` fields on `jsx_attribute` (verified against the
            // actual parse tree) — the attribute name is always the first
            // child (a `property_identifier` or `jsx_namespace_name`), and
            // the value, when present, is the first `string` child (a
            // `jsx_expression_container` value like `placeholder={x}` is not
            // a static string and is intentionally skipped). Using
            // `child_by_field_name` here silently finds nothing, which is
            // why attr_text extraction previously never fired.
            if let Some(name_node) = node.child(0) {
                let name = w.text(name_node);
                if ATTR_NAMES.contains(&name.trim())
                    && let Some(value_node) =
                        each_child(node).into_iter().find(|c| c.kind() == "string")
                {
                    let text = strip_quotes(&w.text(value_node));
                    w.push_attr_text(name.trim().to_string(), text, node);
                }
            }
        }
        "jsx_opening_element" | "jsx_self_closing_element" => {
            if let Some(name) = field_text(w, node, "name") {
                let name = name.trim().to_string();
                if name == "form" || name == "Form" {
                    w.push_form(node);
                } else if is_uppercase_component(&name) {
                    w.push_component(name, node);
                }
            }
        }
        "string" => {
            let text = strip_quotes(&w.text(node));
            w.push_string(text, node, false);
        }
        "template_string" => {
            for child in each_child(node) {
                if child.kind() == "string_fragment" {
                    let text = w.text(child);
                    w.push_string(text, child, false);
                }
            }
        }
        "call_expression" => w.handle_call(node),
        "throw_statement" => {
            // `throw new Error("...")` — find the new_expression and its args.
            for child in each_child(node) {
                if child.kind() == "new_expression"
                    && let Some(c) = child.child_by_field_name("constructor")
                    && c.kind() == "identifier"
                    && w.text(c) == "Error"
                {
                    w.push_error_args(child);
                }
            }
        }
        "pair" => {
            // `{ status: 503 }`
            if let Some(key) = field_text(w, node, "key")
                && key.trim() == "status"
                && let Some(value) = node.child_by_field_name("value")
                && let Some(n) = parse_int(&w.text(value))
                && (100..=599).contains(&n)
            {
                w.push_status(n, node);
            }
        }
        _ => {}
    }
    for child in each_child(node) {
        walk_ts_concepts(w, child, depth + 1);
    }
}

fn walk_rust_concepts(w: &mut TsWalker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    match node.kind() {
        "string_literal" => {
            let text = strip_quotes(&w.text(node));
            w.push_string(text, node, false);
        }
        "call_expression" => {
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => match w.text(f).as_str() {
                        "panic" => w.push_error_args(node),
                        "abort" => w.push_abort_status(node),
                        _ => {}
                    },
                    "scoped_identifier" => {
                        // `env::var("NAME")`, `std::env::var_os("NAME")`
                        if let Some(detail) = rust_env_reader(w, f)
                            && let Some(arg) = w.call_args(node).into_iter().find(Node::is_named)
                        {
                            w.push_env_read(arg, detail, node);
                        }
                        // `StatusCode::from_u16(503)` / `StatusCode::N`
                        if let Some(path) = field_text(w, f, "path")
                            && path.trim() == "StatusCode"
                        {
                            if let Some(name) = field_text(w, f, "name")
                                && let Ok(n) = name.trim().parse::<i64>()
                                && (100..=599).contains(&n)
                            {
                                w.push_status(n, node);
                            }
                            for arg in w.call_args(node) {
                                if let Some(n) = parse_int(&w.text(arg))
                                    && (100..=599).contains(&n)
                                {
                                    w.push_status(n, node);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        "macro_invocation" => {
            // `env!("NAME")` / `option_env!("NAME")`: read at build time.
            if let Some(macro_name) = field_text(w, node, "macro")
                && let Some(detail) = env_macro_detail(macro_name.trim())
                && let Some(tokens) = each_child(node)
                    .into_iter()
                    .find(|c| c.kind() == "token_tree")
                && let Some(arg) = each_child(tokens).into_iter().find(Node::is_named)
            {
                w.push_env_read(arg, detail, node);
            }
        }
        "token_tree" => {
            // Inside another macro's tokens (`concat!(env!("NAME"), …)`) a
            // nested `env!` is no `macro_invocation`, only the tokens `env`,
            // `!` and `(…)`.
            for window in each_child(node).windows(3) {
                if window[1].kind() == "!"
                    && window[2].kind() == "token_tree"
                    && let Some(detail) = env_macro_detail(&w.text(window[0]))
                    && let Some(arg) = each_child(window[2]).into_iter().find(Node::is_named)
                {
                    w.push_env_read(arg, detail, window[0]);
                }
            }
        }
        _ => {}
    }
    for child in each_child(node) {
        walk_rust_concepts(w, child, depth + 1);
    }
}

/// How the macro named `name` reads the environment, when it does.
fn env_macro_detail(name: &str) -> Option<&'static str> {
    match name {
        "env" => Some("build-time read (env!)"),
        "option_env" => Some("build-time read (option_env!)"),
        _ => None,
    }
}

/// How `f`, the callee of a Rust call, reads the environment, when it is
/// `env::var` or `env::var_os` under any path ending in `env`.
fn rust_env_reader(w: &TsWalker, f: Node) -> Option<&'static str> {
    let path = field_text(w, f, "path")?;
    let path = path.trim();
    if path != "env" && !path.ends_with("::env") {
        return None;
    }
    match field_text(w, f, "name")?.trim() {
        "var" => Some("runtime read (env::var)"),
        "var_os" => Some("runtime read (env::var_os)"),
        _ => None,
    }
}

/// True for a name an environment variable can carry in practice: a letter
/// or `_`, then letters, digits or `_`.
fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// --- per-language entry points --------------------------------------------

fn extract_ts(path: &str, content: &[u8]) -> Vec<RawConcept> {
    let language: Language = if path.ends_with(".tsx") {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    } else if path.ends_with(".ts") || path.ends_with(".mts") || path.ends_with(".cts") {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    } else {
        tree_sitter_javascript::LANGUAGE.into()
    };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parse_bounded(&mut parser, content) else {
        return Vec::new();
    };
    let mut w = TsWalker {
        src: content,
        concepts: Vec::new(),
        line_offset: 0,
        test_path: is_test_path(path),
    };
    walk_ts_concepts(&mut w, tree.root_node(), 0);
    w.concepts
}

fn extract_rust(path: &str, content: &[u8]) -> Vec<RawConcept> {
    let language: Language = tree_sitter_rust::LANGUAGE.into();
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parse_bounded(&mut parser, content) else {
        return Vec::new();
    };
    let mut w = TsWalker {
        src: content,
        concepts: Vec::new(),
        line_offset: 0,
        test_path: is_test_path(path),
    };
    walk_rust_concepts(&mut w, tree.root_node(), 0);
    w.concepts
}

/// Run the TS concept walker over a `<script>` block's content, offsetting
/// line numbers so they point into the original file.
fn extract_ts_script(content: &str, line_offset: u32, test_path: bool) -> Vec<RawConcept> {
    let language: Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parse_bounded(&mut parser, content.as_bytes()) else {
        return Vec::new();
    };
    let mut w = TsWalker {
        src: content.as_bytes(),
        concepts: Vec::new(),
        line_offset,
        test_path,
    };
    walk_ts_concepts(&mut w, tree.root_node(), 0);
    w.concepts
}

/// Svelte/Vue: `<script>` blocks through the TS walker (with line offset);
/// markup through the hand-rolled scanner.
fn extract_svelte_vue(path: &str, content: &[u8]) -> Vec<RawConcept> {
    let text = String::from_utf8_lossy(content);
    let test_path = is_test_path(path);
    let mut out = Vec::new();
    let mut markup_start = 0usize;
    let mut pos = 0usize;
    while let Some(rel) = text[pos..].find("<script") {
        let start = pos + rel;
        // Each markup stretch is scanned from its own first line: one buffer
        // for all of them numbered markup before a `<script>` from where the
        // last script ended (#770).
        scan_markup(
            &text[markup_start..start],
            line_of(&text, markup_start),
            &mut out,
        );
        let (open_end, close) = script_block_bounds(&text, start);
        let script_content = &text[open_end..close];
        let line_offset = line_of(&text, open_end).saturating_sub(1);
        out.extend(extract_ts_script(script_content, line_offset, test_path));
        // Always past this tag, whatever the bounds say: the scan must
        // terminate on any input. An unclosed `<script>` runs to the end of
        // the text, so stop there rather than slice past it (#767).
        pos = (close.max(start) + "</script>".len()).min(text.len());
        markup_start = pos;
    }
    scan_markup(
        &text[markup_start..],
        line_of(&text, markup_start),
        &mut out,
    );
    out
}

/// Byte range of the code inside a `<script …>` block whose tag starts at
/// `start`: just past the opening tag's `>` up to `</script>` (or the end
/// of the text when either is missing).
fn script_block_bounds(text: &str, start: usize) -> (usize, usize) {
    let open_end = text[start..]
        .find('>')
        .map_or(text.len(), |i| start + i + 1);
    let close = text[open_end..]
        .find("</script>")
        .map_or(text.len(), |i| open_end + i);
    (open_end, close)
}

fn extract_html(content: &[u8]) -> Vec<RawConcept> {
    let text = String::from_utf8_lossy(content);
    let mut out = Vec::new();
    scan_markup(&text, 1, &mut out);
    out
}

// --- markup scanner (heuristic v1) ----------------------------------------

/// Hand-rolled markup scanner for html/svelte/vue. Emits `ui_text` from inner
/// text, `form`/`component` from element names, and `attr_text` from the
/// known attribute list. Line numbers are tracked as the markup is walked,
/// from `first_line`, the 1-based file line `text` starts on.
fn scan_markup(text: &str, first_line: u32, out: &mut Vec<RawConcept>) {
    let chars: Vec<char> = text.chars().collect();
    let mut line = first_line;
    let mut i = 0usize;
    let mut text_buf = String::new();
    let mut text_start_line = line;
    while i < chars.len() {
        if chars[i] == '<' {
            flush_ui_text(&mut text_buf, text_start_line, out);
            let mut j = i + 1;
            let mut tag = String::new();
            let tag_start_line = line;
            while j < chars.len() && chars[j] != '>' {
                if chars[j] == '\n' {
                    line += 1;
                }
                tag.push(chars[j]);
                j += 1;
            }
            if j < chars.len() {
                j += 1; // consume '>'
            }
            handle_tag(&tag, tag_start_line, out);
            i = j;
        } else {
            if text_buf.is_empty() {
                text_start_line = line;
            }
            if chars[i] == '\n' {
                line += 1;
            }
            text_buf.push(chars[i]);
            i += 1;
        }
    }
    flush_ui_text(&mut text_buf, text_start_line, out);
}

fn flush_ui_text(buf: &mut String, start_line: u32, out: &mut Vec<RawConcept>) {
    let text = buf.trim();
    if !text.is_empty() {
        let norm = normalize(text);
        if !norm.is_empty() && norm.len() <= MAX_NORM_CHARS {
            out.push(RawConcept {
                kind: ConceptKind::UiText,
                raw: text.to_string(),
                norm,
                detail: String::new(),
                start_line,
                end_line: start_line,
                owner_symbol_id: None,
            });
        }
    }
    buf.clear();
}

fn handle_tag(tag: &str, line: u32, out: &mut Vec<RawConcept>) {
    let inner = tag.trim_start_matches('<').trim_end_matches('>');
    if inner.starts_with('/') || inner.starts_with('!') || inner.starts_with('?') {
        return;
    }
    let name = inner.split_whitespace().next().unwrap_or("").to_string();
    if name.is_empty() {
        return;
    }
    if name == "form" || name == "Form" {
        out.push(RawConcept {
            kind: ConceptKind::Form,
            raw: "form".into(),
            norm: "form".into(),
            detail: "form element".into(),
            start_line: line,
            end_line: line,
            owner_symbol_id: None,
        });
    } else if name.chars().next().is_some_and(char::is_uppercase) {
        out.push(RawConcept {
            kind: ConceptKind::Component,
            raw: name.clone(),
            norm: normalize(&name),
            detail: "component".into(),
            start_line: line,
            end_line: line,
            owner_symbol_id: None,
        });
    }
    for attr in ATTR_NAMES {
        let pat = format!("{attr}=\"");
        if let Some(pos) = inner.find(&pat) {
            let rest = &inner[pos + pat.len()..];
            if let Some(end) = rest.find('"') {
                let value = &rest[..end];
                if !value.is_empty() {
                    out.push(RawConcept {
                        kind: ConceptKind::AttrText,
                        raw: value.to_string(),
                        norm: normalize(value),
                        detail: attr.to_string(),
                        start_line: line,
                        end_line: line,
                        owner_symbol_id: None,
                    });
                }
            }
        }
    }
}

// --- config files ---------------------------------------------------------

fn extract_json_config(content: &[u8]) -> Vec<RawConcept> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(content) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk_json(&value, "", &mut out);
    out
}

fn walk_json(value: &serde_json::Value, prefix: &str, out: &mut Vec<RawConcept>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                out.push(RawConcept {
                    kind: ConceptKind::ConfigKey,
                    raw: path.clone(),
                    norm: normalize(&path),
                    detail: "key".into(),
                    start_line: 0,
                    end_line: 0,
                    owner_symbol_id: None,
                });
                walk_json(v, &path, out);
            }
        }
        serde_json::Value::String(s) if !prefix.is_empty() => {
            out.push(RawConcept {
                kind: ConceptKind::ConfigKey,
                raw: s.clone(),
                norm: normalize(s),
                detail: format!("value of {prefix}"),
                start_line: 0,
                end_line: 0,
                owner_symbol_id: None,
            });
        }
        _ => {}
    }
}

fn extract_yaml_config(content: &[u8]) -> Vec<RawConcept> {
    let text = String::from_utf8_lossy(content);
    let mut out = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line_no = i as u32 + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
            continue;
        }
        let content = trimmed.split(" #").next().unwrap_or(trimmed);
        let Some((key, value)) = content.split_once(':') else {
            continue;
        };
        let indent = line.len() - line.trim_start().len();
        let key = key.trim().trim_matches('"').trim_matches('\'').to_string();
        while let Some(&(si, _)) = stack.last() {
            if si >= indent {
                stack.pop();
            } else {
                break;
            }
        }
        let path = if stack.is_empty() {
            key.clone()
        } else {
            let parent: Vec<&str> = stack.iter().map(|(_, k)| k.as_str()).collect();
            format!("{}.{}", parent.join("."), key)
        };
        out.push(RawConcept {
            kind: ConceptKind::ConfigKey,
            raw: path.clone(),
            norm: normalize(&path),
            detail: "key".into(),
            start_line: line_no,
            end_line: line_no,
            owner_symbol_id: None,
        });
        let value = value.trim();
        if !value.is_empty() && !value.starts_with('[') && !value.starts_with('{') {
            out.push(RawConcept {
                kind: ConceptKind::ConfigKey,
                raw: value.to_string(),
                norm: normalize(value),
                detail: format!("value of {path}"),
                start_line: line_no,
                end_line: line_no,
                owner_symbol_id: None,
            });
        }
        stack.push((indent, key));
    }
    out
}

fn extract_css(content: &[u8]) -> Vec<RawConcept> {
    let text = String::from_utf8_lossy(content);
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line_no = i as u32 + 1;
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("--")
            && let Some((name, _)) = rest.split_once(':')
        {
            let name = name.trim().to_string();
            let path = format!("--{name}");
            out.push(RawConcept {
                kind: ConceptKind::ConfigKey,
                raw: path.clone(),
                norm: normalize(&path),
                detail: "css custom property".into(),
                start_line: line_no,
                end_line: line_no,
                owner_symbol_id: None,
            });
        }
    }
    out
}

// --- file-path-derived routes --------------------------------------------

/// True when `path` has `dir` as a path segment, whether at the start of the
/// (repo-relative, no leading slash) path or nested under a prefix. Plain
/// `path.contains("/dir/")` misses the very common case where `dir` IS the
/// top-level segment (e.g. a repo-root `app/api/orders/route.ts`, which
/// contains no leading slash before `app/` at all).
fn has_dir_segment(path: &str, dir: &str) -> bool {
    path.starts_with(&format!("{dir}/")) || path.contains(&format!("/{dir}/"))
}

/// Derive the URL-ish route path from a file-path-derived route file, e.g.
/// `app/api/orders/route.ts` -> `/api/orders`, `routes/orders/+server.ts` ->
/// `/orders`. Falls back to the whole path when the anchor segment isn't
/// found (should not happen given the caller already matched on it).
fn derive_route_path(path: &str, anchor: &str) -> String {
    let after = path
        .rsplit_once(&format!("/{anchor}/"))
        .map(|(_, rest)| rest)
        .or_else(|| path.strip_prefix(&format!("{anchor}/")))
        .unwrap_or(path);
    let dir = after.rsplit_once('/').map_or("", |(d, _)| d);
    if dir.is_empty() {
        "/".to_string()
    } else {
        format!("/{dir}")
    }
}

/// Next.js `app/**/route.ts` + method exports, `pages/api/**`, and SvelteKit
/// `+server.ts` — one route concept per HTTP method. Call-derived routes
/// (`app.get('/x')`, `fetch('/api/…')`) come from the TS walker instead.
fn path_routes(path: &str, content: &[u8]) -> Vec<RawConcept> {
    let mut out = Vec::new();
    let is_next_route = has_dir_segment(path, "app")
        && (path.ends_with("/route.ts")
            || path.ends_with("/route.js")
            || path.ends_with("/route.tsx")
            || path.ends_with("/route.jsx"));
    let is_pages_api = has_dir_segment(path, "pages/api");
    let is_sveltekit = has_dir_segment(path, "routes")
        && (path.ends_with("+server.ts") || path.ends_with("+server.js"));
    if is_next_route || is_sveltekit {
        let anchor = if is_sveltekit { "routes" } else { "app" };
        let route_path = derive_route_path(path, anchor);
        let text = String::from_utf8_lossy(content);
        for method in ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"] {
            if text.contains(&format!("function {method}"))
                || text.contains(&format!("const {method}"))
            {
                // Include the derived URL path in raw/norm (not just the
                // method name) so the endpoint's path text is actually
                // indexed into concept_words — otherwise a phrase like
                // "orders endpoint" could never resolve to this route, since
                // only `detail` carried the path and `detail` is never
                // normalized or indexed.
                let raw = format!("{method} {route_path}");
                out.push(RawConcept {
                    kind: ConceptKind::Route,
                    raw: raw.clone(),
                    norm: normalize(&raw),
                    detail: format!("{method} {path}"),
                    start_line: 0,
                    end_line: 0,
                    owner_symbol_id: None,
                });
            }
        }
    } else if is_pages_api {
        out.push(RawConcept {
            kind: ConceptKind::Route,
            raw: path.to_string(),
            norm: normalize(path),
            detail: format!("api {path}"),
            start_line: 0,
            end_line: 0,
            owner_symbol_id: None,
        });
    }
    out
}

// --- small helpers --------------------------------------------------------

fn line_of(text: &str, byte_offset: usize) -> u32 {
    text[..byte_offset.min(text.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count() as u32
        + 1
}

fn is_uppercase_component(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

fn parse_int(s: &str) -> Option<i64> {
    s.trim().parse::<i64>().ok()
}

fn strip_quotes(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .to_string()
}

fn each_child<'t>(n: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = n.walk();
    n.children(&mut cursor).collect()
}

fn field_text(w: &TsWalker, n: Node, field: &str) -> Option<String> {
    n.child_by_field_name(field).map(|c| w.text(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Concept extraction runs several tree-sitter passes over a file, so a
    /// bundle that slips under the 1 MB size cap costs more here than in
    /// symbol extraction. Both paths must agree on what counts as source, or
    /// a file contributes concepts while contributing no symbols.
    #[test]
    fn minified_bundle_yields_no_concepts() {
        let mut bundle = String::new();
        let mut i = 0;
        while bundle.len() < 900 * 1024 {
            bundle.push_str(&format!("const r{i}=useRouter();fetch('/api/x{i}');"));
            i += 1;
        }
        assert!(
            bundle.len() < MAX_FILE_BYTES,
            "fixture must sit under the size cap, so the blob guard is what rejects it"
        );
        assert!(crate::extract::is_generated_blob(bundle.as_bytes()));
        assert!(extract_concepts("public/assets/app.js", bundle.as_bytes()).is_empty());

        // The same calls in ordinary layout are still picked up: the guard
        // rejects the shape, never the content.
        let readable = bundle.replace(';', ";\n");
        assert!(!crate::extract::is_generated_blob(readable.as_bytes()));
        assert!(!extract_concepts("src/app.js", readable.as_bytes()).is_empty());
    }

    #[test]
    fn handle_tag_records_forms_and_components_and_skips_closing_or_html_tags() {
        let mut out = Vec::new();
        handle_tag("<form method=\"post\">", 3, &mut out);
        handle_tag("<Button primary>", 4, &mut out);
        handle_tag("</Button>", 5, &mut out);
        handle_tag("<!-- note -->", 6, &mut out);
        handle_tag("<div>", 7, &mut out);
        handle_tag("<>", 8, &mut out);
        let seen: Vec<(ConceptKind, &str, u32)> = out
            .iter()
            .map(|c| (c.kind, c.raw.as_str(), c.start_line))
            .collect();
        assert_eq!(
            seen,
            vec![
                (ConceptKind::Form, "form", 3),
                (ConceptKind::Component, "Button", 4)
            ]
        );
        assert_eq!(out[1].norm, normalize("Button"));
    }

    #[test]
    fn script_block_bounds_starts_after_the_opening_tag_and_stops_at_the_close() {
        let text = "<p/>\n<script lang=\"ts\">let a = 1;</script>\n<b/>";
        let start = text.find("<script").unwrap();
        let (open_end, close) = script_block_bounds(text, start);
        assert_eq!(&text[open_end..close], "let a = 1;");
        // No `>` at all: nothing to parse; no `</script>`: runs to the end.
        assert_eq!(script_block_bounds("<script", 0), (7, 7));
        let open = "<script>x = 1";
        assert_eq!(script_block_bounds(open, 0), (8, open.len()));
    }

    #[test]
    fn is_uppercase_component_checks_the_first_character() {
        assert!(is_uppercase_component("Button"));
        assert!(!is_uppercase_component("button"));
        assert!(!is_uppercase_component(""));
    }

    /// The `<script>` block goes through the TypeScript walker with the
    /// block's line offset, the rest through the markup scanner: a route
    /// called from the script and a component in the markup both carry
    /// their line in the .svelte file.
    #[test]
    fn extract_svelte_vue_offsets_script_concepts_and_scans_the_markup() {
        let content = b"<div class=\"page\">\n<script lang=\"ts\">\n  async function submit() {\n    await fetch(\"/api/contact\", { method: \"POST\" });\n  }\n  // tail-marker\n</script>\n<Button label=\"Press me\" />\n</div>\n";
        let concepts = extract_svelte_vue("src/Page.svelte", content);
        // Nothing from inside the script block may reach the markup scanner.
        assert!(
            concepts.iter().all(|c| c.kind != ConceptKind::UiText),
            "{concepts:?}"
        );
        let route = concepts
            .iter()
            .find(|c| c.kind == ConceptKind::Route && c.raw.contains("/api/contact"))
            .unwrap_or_else(|| panic!("route from the script block: {concepts:?}"));
        assert_eq!(route.start_line, 4, "{route:?}");
        let component = concepts
            .iter()
            .find(|c| c.kind == ConceptKind::Component && c.raw == "Button")
            .unwrap_or_else(|| panic!("component from the markup: {concepts:?}"));
        assert_eq!(component.start_line, 8, "{component:?}");
        let attr = concepts
            .iter()
            .find(|c| c.kind == ConceptKind::AttrText && c.raw == "Press me")
            .unwrap_or_else(|| panic!("attr text from the markup: {concepts:?}"));
        assert_eq!(attr.start_line, 8);
        assert!(extract_svelte_vue("src/Empty.svelte", b"").is_empty());
    }

    #[test]
    fn extract_svelte_vue_should_keep_the_script_concepts_when_the_script_is_never_closed() {
        // A repository controls this file: an unclosed <script> must not
        // panic the graph build (#767), and the code after the tag is
        // still the script's.
        let content = b"<h1>Title text here</h1>\n<script>\n  fetch(\"/api/unclosed\");\n";
        let concepts = extract_svelte_vue("src/Broken.svelte", content);
        let route = concepts
            .iter()
            .find(|c| c.kind == ConceptKind::Route && c.raw.contains("/api/unclosed"))
            .unwrap_or_else(|| panic!("route from the unclosed script: {concepts:?}"));
        assert_eq!(route.start_line, 3, "{route:?}");
        assert!(
            concepts
                .iter()
                .any(|c| c.kind == ConceptKind::UiText && c.raw == "Title text here"),
            "markup before the script is still scanned: {concepts:?}"
        );
    }

    #[test]
    fn extract_svelte_vue_should_not_panic_when_the_script_tag_itself_is_cut_off() {
        // No `>` and no `</script>`: the open tag runs to the end of file.
        for content in [&b"<script"[..], b"<p>x</p><script lang=\"ts\"", b"<script>"] {
            let _ = extract_svelte_vue("src/Cut.vue", content);
        }
    }
}

#[cfg(test)]
mod contract_tests;
