// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Per-file tree-sitter extraction: symbols, call sites, import specs.
//!
//! Pragmatic node-kind walks per language family. Any grammar failure
//! degrades gracefully to `None` — a file we cannot parse simply
//! contributes nothing to the graph.

use std::panic::AssertUnwindSafe;

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use tree_sitter::{Language, Node, ParseOptions, ParseState, Parser, Tree};

use crate::store::SymbolKind;

pub(crate) mod ruby_callbacks;
pub(crate) mod ruby_generated;
pub(crate) mod ruby_mixins;

pub use ruby_mixins::RawMixin;

#[derive(Debug, Clone)]
pub struct RawSymbol {
    pub name: String,
    pub qualified: String,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub end_line: u32,
    pub sig: String,
    /// A method of a trait implementation (`impl Display for X { fn fmt }`):
    /// called through the trait, so a missing direct caller proves nothing.
    pub trait_impl: bool,
    /// An external Rust module declaration (`mod foo;`): it names the file
    /// that holds the module's code instead of defining code in this one, so
    /// an exact-name probe of `foo` must not promote the declaring file (see
    /// `targets::symbol_hits`). Inline modules (`mod foo { … }`) are false.
    pub module_decl: bool,
}

#[derive(Debug, Clone)]
pub struct RawCall {
    pub callee_name: String,
    pub receiver: Option<String>,
    pub site_line: u32,
    /// Index into `FileExtraction::symbols` of the smallest enclosing symbol.
    pub enclosing_index: Option<usize>,
}

/// A symbol passed as an argument to a call (callback / plugin / handler
/// registration). The `arg_of` field is the callee that received the
/// argument (e.g. `"plugin"` in `schema.plugin(tenantScopePlugin)`).
#[derive(Debug, Clone)]
pub struct RawReference {
    pub name: String,
    /// Index into `FileExtraction::symbols` of the smallest enclosing symbol.
    pub enclosing_index: Option<usize>,
    pub site_line: u32,
    /// The callee that received this argument, when known.
    pub arg_of: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RawImport {
    /// The statement's import text as written; `rename` finds the statement
    /// by it, so it stays whole even when the statement names several paths.
    pub spec: String,
    /// What `resolve_import` resolves. Equal to `spec`, except for a Rust
    /// `use` naming several paths (`use crate::{left::push, right::publish};`),
    /// which yields one import per path (`crate::left::push`, …), each
    /// resolved to its own file.
    pub path: String,
    /// The inclusive line ranges where the bindings are in scope; empty means
    /// the whole file. A Rust `use` binds names for its enclosing module or
    /// block only, and not inside a nested module unless that module
    /// glob-imports its parent (`use super::*;`).
    pub scope: Vec<(u32, u32)>,
    /// Named bindings imported from this spec (`greet` and `farewell` for
    /// `import { greet, farewell } from "./a"`). Empty for wildcard imports
    /// (`import * as x`) or when bindings cannot be extracted. Empty bindings
    /// never grant Exact import-tier confidence.
    pub bindings: Vec<ImportBinding>,
}

/// One name an import brings into scope: `local` is what the importing file
/// calls it, `source` what the imported file defines. They differ only under
/// an alias (`use a::push as leased;`, `import { push as leased }`): the
/// resolver matches calls on `local` and candidates on `source`, and `rename`
/// rewrites the `source` occurrence at the import site.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ImportBinding {
    pub local: String,
    pub source: String,
}

impl ImportBinding {
    /// A binding imported under its own name.
    pub fn named(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            local: name.clone(),
            source: name,
        }
    }

    /// A binding imported as `source` and called `local` in the importer.
    pub fn aliased(source: impl Into<String>, local: impl Into<String>) -> Self {
        Self {
            local: local.into(),
            source: source.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RawJsxElement {
    pub tag: String,
    pub has_handler: bool,
    pub text_content: String,
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Debug)]
pub struct FileExtraction {
    pub lang: &'static str,
    pub symbols: Vec<RawSymbol>,
    pub calls: Vec<RawCall>,
    pub references: Vec<RawReference>,
    pub imports: Vec<RawImport>,
    pub jsx_elements: Vec<RawJsxElement>,
    /// Ruby only: the superclass and modules each class or module declares.
    pub mixins: Vec<RawMixin>,
}

/// Ruby files known by their whole name: Bundler's `Gemfile` and the
/// Ruby-DSL build files whose tools evaluate them as Ruby.
pub const RUBY_FILE_NAMES: &[&str] = &["Gemfile", "Rakefile", "Guardfile", "Capfile"];

/// Language tag for a repo-relative path, or `None` if unsupported.
pub fn lang_of(path: &str) -> Option<&'static str> {
    let file = path.rsplit('/').next().unwrap_or(path);
    if RUBY_FILE_NAMES.contains(&file) {
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
        "cs" => Some("csharp"),
        "rb" | "rake" | "gemspec" | "ru" => Some("ruby"),
        // Perfect-expansion languages driven by the generic node-kind walker.
        "php" => Some("php"),
        "c" | "h" => Some("c"),
        "swift" => Some("swift"),
        "ex" | "exs" => Some("elixir"),
        "lua" => Some("lua"),
        _ => None,
    }
}

/// True iff `path` may be a Ruby executable whose language only its shebang
/// tells: an extensionless file directly in a `bin/` or `exe/` directory
/// (`bin/rails`, `exe/mygem`), the places Rails and Bundler put binstubs.
/// The graph walks read it; [`lang_of_file`] decides from its first line.
pub fn is_binstub_candidate(path: &str) -> bool {
    let mut parts = path.rsplit('/');
    let file = parts.next().unwrap_or(path);
    !file.is_empty()
        && !file.contains('.')
        && matches!(parts.next(), Some("bin" | "exe"))
        && lang_of(path).is_none()
}

/// True iff the first line of `content` is a Ruby shebang: an interpreter
/// path ending in `ruby` (`#!/usr/bin/ruby`), or `env` naming `ruby`
/// (`#!/usr/bin/env ruby`, `#!/usr/bin/env -S ruby -w`).
pub fn has_ruby_shebang(content: &[u8]) -> bool {
    let line = content.split(|b| *b == b'\n').next().unwrap_or_default();
    let Some(rest) = line.strip_prefix(b"#!") else {
        return false;
    };
    let line = String::from_utf8_lossy(rest);
    let mut words = line.split_whitespace();
    let Some(interpreter) = words.next() else {
        return false;
    };
    let base = interpreter.rsplit('/').next().unwrap_or(interpreter);
    if base == "env" {
        words.find(|w| !w.starts_with('-')) == Some("ruby")
    } else {
        base == "ruby"
    }
}

/// [`lang_of`], plus a binstub ([`is_binstub_candidate`]) whose content opens
/// with a Ruby shebang. An extensionless executable is never Ruby by its
/// place alone: `bin/dev` is often a shell script.
pub fn lang_of_file(path: &str, content: &[u8]) -> Option<&'static str> {
    lang_of(path)
        .or_else(|| (is_binstub_candidate(path) && has_ruby_shebang(content)).then_some("ruby"))
}

/// Size floor for the generated-blob guard: below this, even a one-line file
/// parses in microseconds, so the guard would only add false-positive risk.
pub const GENERATED_MIN_BYTES: usize = 65_536; // 64 KiB

/// Mean bytes-per-line above which a file of at least [`GENERATED_MIN_BYTES`]
/// is treated as a generated/minified blob rather than source.
///
/// Calibrated against real trees, not guessed: the worst hand-written file
/// measured in a 400k-line Rails monolith averages 127 bytes/line (and its
/// largest file, a 400 KB `schema.rb`, averages 47), while this workspace's
/// worst averages 70. A single-line bundle averages its whole length. The
/// threshold therefore sits a factor of four above anything a human writes
/// and four orders of magnitude below a bundle.
///
/// Note the deliberate divergence from the per-line caps other tools use
/// (MeshMCP rejects any line over 1024 bytes): that would reject real source
/// here — `parking_id_extractor_service_spec.rb` has a 16 044-byte line, and
/// an `assets_controller.rb` a 5 276-byte one. Only the whole-file *mean*
/// separates the two populations cleanly.
pub const GENERATED_MAX_BYTES_PER_LINE: usize = 512;

/// Whether `content` looks like a generated/minified blob that tree-sitter
/// should not be pointed at.
///
/// Deterministic by construction — a pure function of the bytes, with no
/// clock and no machine-load dependency. That is the point: a wall-clock
/// parser timeout (MeshMCP's 15 ms C-FFI bound) would make the extracted
/// symbol set depend on how loaded the machine was during the build, and
/// `graph.db`'s freshness signature assumes the same bytes always yield the
/// same graph. A content-derived predicate keeps that invariant.
///
/// Both conditions must hold, so a large ordinary file (`schema.rb`) and a
/// small dense one (a fixture with one long string) both pass through.
pub fn is_generated_blob(content: &[u8]) -> bool {
    if content.len() < GENERATED_MIN_BYTES {
        return false;
    }
    // Count newlines rather than splitting: no allocation, and a trailing
    // fragment counts as its own line so a file with no newline at all is
    // treated as one line rather than zero.
    let newlines = bytecount(content, b'\n');
    let lines = if content.ends_with(b"\n") {
        newlines
    } else {
        newlines + 1
    };
    content.len() / lines.max(1) >= GENERATED_MAX_BYTES_PER_LINE
}

fn bytecount(haystack: &[u8], needle: u8) -> usize {
    haystack.iter().filter(|b| **b == needle).count()
}

/// Wall-clock cap on one tree-sitter parse. Error recovery on a few hundred
/// malformed bytes can run for minutes (#800); a parse past the cap yields no
/// tree, as a grammar failure does. A source file parses in milliseconds.
pub(crate) const PARSE_BUDGET: Duration = Duration::from_secs(3);

/// True once a parse has run longer than its budget; a parse that took
/// exactly the budget is still within it.
fn over_budget(elapsed: Duration, budget: Duration) -> bool {
    elapsed > budget
}

/// `parser.parse(content, None)`, cancelled once it has run past `budget`:
/// `None` on a cancelled parse as on any other failure. Every tree-sitter
/// parse in this crate goes through [`parse_bounded`], which sets the budget.
fn parse_within(parser: &mut Parser, content: &[u8], budget: Duration) -> Option<Tree> {
    let start = Instant::now();
    let mut stop_late = |_: &ParseState| {
        if over_budget(start.elapsed(), budget) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    parser.parse_with_options(
        &mut |offset, _| content.get(offset..).unwrap_or_default(),
        None,
        Some(ParseOptions::new().progress_callback(&mut stop_late)),
    )
}

/// [`parse_within`] at [`PARSE_BUDGET`]: the parse every extractor uses.
pub(crate) fn parse_bounded(parser: &mut Parser, content: &[u8]) -> Option<Tree> {
    parse_within(parser, content, PARSE_BUDGET)
}

/// Parse one file into a tree-sitter tree for the language its extension
/// maps to. `None` on unsupported language, a generated/minified blob, or
/// any parse/grammar failure.
/// Shared by extraction and the rename verifier, which re-parses a file to
/// confirm each candidate identifier's role before rewriting it.
pub fn parse_file(path_rel: &str, content: &[u8]) -> Option<tree_sitter::Tree> {
    let lang = lang_of_file(path_rel, content)?;
    if is_generated_blob(content) {
        return None;
    }
    let language = language_for(lang)?;
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut parser = Parser::new();
        parser.set_language(&language).ok()?;
        parse_bounded(&mut parser, content)
    }))
    .ok()
    .flatten()
}

/// Extract symbols/calls/imports from one file. `None` on unsupported
/// language, a generated/minified blob, or any parse/grammar failure.
///
/// The [`is_generated_blob`] guard lives here rather than at the call sites
/// so every path is covered by construction. It matters: `build_graph`
/// filters its inputs through `is_binary` beforehand, but
/// `update_files_unsigned` — the incremental path the daemon runs on every
/// save — does not, so a committed bundle was re-parsed on each touch.
pub fn extract_file(path_rel: &str, content: &[u8]) -> Option<FileExtraction> {
    let lang = lang_of_file(path_rel, content)?;
    if is_generated_blob(content) {
        return None;
    }
    let mut extraction =
        std::panic::catch_unwind(AssertUnwindSafe(|| extract_inner(lang, content)))
            .ok()
            .flatten()?;
    if lang == "ruby" {
        let end_line = u32::try_from(content.iter().filter(|byte| **byte == b'\n').count())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        extraction.symbols.push(RawSymbol {
            name: path_rel.to_string(),
            qualified: path_rel.to_string(),
            kind: SymbolKind::Script,
            start_line: 1,
            end_line,
            sig: path_rel.to_string(),
            trait_impl: false,
            module_decl: false,
        });
        assign_enclosing(&mut extraction);
    }
    Some(extraction)
}

fn language_for(lang: &str) -> Option<Language> {
    Some(match lang {
        "ts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "js" => tree_sitter_javascript::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "csharp" => tree_sitter_c_sharp::LANGUAGE.into(),
        "ruby" => tree_sitter_ruby::LANGUAGE.into(),
        "php" => tree_sitter_php::LANGUAGE_PHP.into(),
        "c" => tree_sitter_c::LANGUAGE.into(),
        "swift" => tree_sitter_swift::LANGUAGE.into(),
        "elixir" => tree_sitter_elixir::LANGUAGE.into(),
        "lua" => tree_sitter_lua::LANGUAGE.into(),
        _ => return None,
    })
}

fn extract_inner(lang: &'static str, content: &[u8]) -> Option<FileExtraction> {
    let language = language_for(lang)?;
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parse_bounded(&mut parser, content)?;
    let mut w = Walker {
        src: content,
        symbols: Vec::new(),
        calls: Vec::new(),
        references: Vec::new(),
        imports: Vec::new(),
        jsx_elements: Vec::new(),
        stack: Vec::new(),
        in_trait_impl: false,
        use_scopes: std::collections::HashMap::new(),
        generated: Vec::new(),
        mixins: Vec::new(),
    };
    let root = tree.root_node();
    match lang {
        "ts" | "tsx" | "js" => walk_ts(&mut w, lang, root, 0),
        "rust" => walk_rust(&mut w, root, 0),
        "go" => walk_go(&mut w, root, 0),
        "java" => walk_java(&mut w, root, 0),
        "python" => walk_python(&mut w, root, 0),
        "csharp" => walk_csharp(&mut w, root, 0),
        "ruby" => walk_ruby(&mut w, &mut RubyLocals::default(), root, RubyIdent::Expr, 0),
        // Any other wired language — or any future language added to the lang
        // map — falls back to the heuristic node-kind walker. Coarse but better
        // than absent.
        _ => walk_generic(&mut w, root, 0),
    }
    ruby_generated::drop_overridden(&mut w);
    let mut fx = FileExtraction {
        lang,
        symbols: w.symbols,
        calls: w.calls,
        references: w.references,
        imports: w.imports,
        jsx_elements: w.jsx_elements,
        mixins: w.mixins,
    };
    assign_enclosing(&mut fx);
    Some(fx)
}

/// Smallest symbol whose line range contains the call/reference site.
fn assign_enclosing(fx: &mut FileExtraction) {
    let best = |line: u32| -> Option<usize> {
        let mut best: Option<(usize, u32)> = None;
        for (i, s) in fx.symbols.iter().enumerate() {
            if s.start_line <= line && line <= s.end_line {
                let span = s.end_line - s.start_line;
                if best.is_none_or(|(_, b)| span < b) {
                    best = Some((i, span));
                }
            }
        }
        best.map(|(i, _)| i)
    };
    for call in &mut fx.calls {
        call.enclosing_index = best(call.site_line);
    }
    // A reference placed by its extractor keeps its symbol: the methods one
    // `delegate :a, :b` generates share a line, and each forwards on its own.
    for r#ref in fx
        .references
        .iter_mut()
        .filter(|r| r.enclosing_index.is_none())
    {
        r#ref.enclosing_index = best(r#ref.site_line);
    }
}

const MAX_DEPTH: usize = 512;
const SIG_CAP: usize = 200;

struct Walker<'a> {
    src: &'a [u8],
    symbols: Vec<RawSymbol>,
    calls: Vec<RawCall>,
    references: Vec<RawReference>,
    imports: Vec<RawImport>,
    jsx_elements: Vec<RawJsxElement>,
    /// Enclosing type names (class/impl/trait) for qualification.
    stack: Vec<String>,
    /// Inside the body of a trait implementation (`impl Trait for Type`).
    in_trait_impl: bool,
    /// Rust `use` scopes already computed, by scope node id: every `use` of a
    /// file's top level shares one walk of the file.
    use_scopes: std::collections::HashMap<usize, Vec<(u32, u32)>>,
    /// Indices in `symbols` of the Ruby methods a declaration generated
    /// (`attr_reader`, `delegate`, ...), for `ruby_generated::drop_overridden`.
    generated: Vec<usize>,
    /// Ruby ancestors declared so far (`ruby_mixins`).
    mixins: Vec<RawMixin>,
}

impl<'a> Walker<'a> {
    fn text(&self, n: Node) -> String {
        String::from_utf8_lossy(&self.src[n.byte_range()]).into_owned()
    }

    fn sig(&self, n: Node) -> String {
        let raw = &self.src[n.byte_range()];
        let first = raw.split(|&b| b == b'\n').next().unwrap_or(raw);
        let s = String::from_utf8_lossy(first);
        let t = s.trim();
        if t.len() > SIG_CAP {
            let mut cut = SIG_CAP;
            while cut > 0 && !t.is_char_boundary(cut) {
                cut -= 1;
            }
            t[..cut].to_string()
        } else {
            t.to_string()
        }
    }

    fn qualify(&self, name: &str, sep: &str) -> String {
        if self.stack.is_empty() {
            name.to_string()
        } else {
            format!("{}{}{}", self.stack.join(sep), sep, name)
        }
    }

    fn push_symbol(&mut self, name: String, qualified: String, kind: SymbolKind, node: Node) {
        self.push_symbol_full(name, qualified, kind, node, false);
    }

    /// An external `mod foo;` declaration: unlike every other symbol, it names
    /// another file instead of defining code in this one.
    fn push_module_decl(&mut self, name: String, node: Node) {
        self.push_symbol_full(name.clone(), name, SymbolKind::Module, node, true);
    }

    fn push_symbol_full(
        &mut self,
        name: String,
        qualified: String,
        kind: SymbolKind,
        node: Node,
        module_decl: bool,
    ) {
        if name.is_empty() {
            return;
        }
        self.symbols.push(RawSymbol {
            trait_impl: self.in_trait_impl && kind == SymbolKind::Method,
            module_decl,
            sig: self.sig(node),
            start_line: line_start(node),
            end_line: line_end(node),
            name,
            qualified,
            kind,
        });
    }

    fn push_call(&mut self, callee: String, receiver: Option<String>, node: Node) {
        if callee.is_empty() {
            return;
        }
        self.calls.push(RawCall {
            callee_name: callee,
            receiver,
            site_line: line_start(node),
            enclosing_index: None,
        });
    }

    /// Record a symbol passed as an argument to a call (a callback / plugin /
    /// handler reference). `call_node` is the enclosing call expression; its
    /// start line becomes the reference site line. `arg_of` is the callee
    /// that received the argument, when known.
    fn push_reference(&mut self, name: String, call_node: Node, arg_of: Option<String>) {
        if name.is_empty() {
            return;
        }
        self.references.push(RawReference {
            name,
            enclosing_index: None,
            site_line: line_start(call_node),
            arg_of,
        });
    }

    fn push_import(&mut self, spec: String, bindings: Vec<ImportBinding>) {
        self.push_import_at(spec.clone(), spec, bindings, Vec::new());
    }

    fn push_import_at(
        &mut self,
        spec: String,
        path: String,
        bindings: Vec<ImportBinding>,
        scope: Vec<(u32, u32)>,
    ) {
        if !spec.is_empty() {
            self.imports.push(RawImport {
                spec,
                path,
                scope,
                bindings,
            });
        }
    }

    fn push_jsx_element(
        &mut self,
        tag: String,
        has_handler: bool,
        text_content: String,
        start_line: u32,
        end_line: u32,
    ) {
        if tag.is_empty() {
            return;
        }
        self.jsx_elements.push(RawJsxElement {
            tag,
            has_handler,
            text_content,
            start_line,
            end_line,
        });
    }
}

fn line_start(n: Node) -> u32 {
    n.start_position().row as u32 + 1
}
fn line_end(n: Node) -> u32 {
    n.end_position().row as u32 + 1
}

fn field_text(w: &Walker, n: Node, field: &str) -> Option<String> {
    n.child_by_field_name(field).map(|c| w.text(c))
}

fn strip_quotes(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .to_string()
}

fn each_child<'t>(n: Node<'t>) -> Vec<Node<'t>> {
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

/// Walk the `arguments` field of a call/invocation node and record each
/// argument that may name a function as a `RawReference`. The callee name
/// (`arg_of`) is the method/function that received the argument.
///
/// - a bare identifier (`schema.plugin(tenantScopePlugin)`), literal
///   keywords and self pseudo-receivers excepted;
/// - a path (`Self::helper`, `module::func`), which names an item;
/// - a member access only on a self receiver (`this.onClick`). A member of
///   any other value (`user.name`) is data: resolving its property name
///   against every function of that name linked unrelated code.
fn walk_call_arguments(w: &mut Walker, call: Node, arg_of: Option<String>) {
    let Some(args) = call.child_by_field_name("arguments") else {
        return;
    };
    let mut cursor = args.walk();
    for arg in args.children(&mut cursor) {
        // C# (and PHP) wrap each argument in an `argument` node whose last
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
            w.push_reference(name, call, arg_of.clone());
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

fn walk_ts(w: &mut Walker, lang: &'static str, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut pushed = false;
    match node.kind() {
        "function_declaration" | "generator_function_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Function, node);
            }
        }
        "class_declaration" | "abstract_class_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name.clone(), q, SymbolKind::Class, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "interface_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Interface, node);
            }
        }
        "enum_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Enum, node);
            }
        }
        "method_definition" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Method, node);
            }
        }
        "variable_declarator" => {
            let is_fn = node.child_by_field_name("value").is_some_and(|v| {
                matches!(
                    v.kind(),
                    "arrow_function" | "function_expression" | "function"
                )
            });
            if is_fn
                && let Some(name) = field_text(w, node, "name")
                && !name.contains(['{', '['])
            {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Function, node);
            }
        }
        "call_expression" => {
            let mut callee_name: Option<String> = None;
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => {
                        let name = w.text(f);
                        callee_name = Some(name.clone());
                        w.push_call(name, None, node);
                    }
                    "member_expression" => {
                        if let Some(prop) = field_text(w, f, "property") {
                            let recv = field_text(w, f, "object");
                            callee_name = Some(prop.clone());
                            w.push_call(prop, recv, node);
                        }
                    }
                    _ => {}
                }
            }
            // After extracting the callee, check arguments for identifier
            // references (callbacks / plugins / handlers passed as args).
            walk_call_arguments(w, node, callee_name);
        }
        "new_expression" => {
            if let Some(c) = node.child_by_field_name("constructor")
                && c.kind() == "identifier"
            {
                let name = w.text(c);
                w.push_call(name, None, node);
            }
            // `new Foo(handler)` — constructor args can also be callbacks.
            walk_call_arguments(w, node, None);
        }
        "import_statement" | "export_statement" => {
            if let Some(src) = node.child_by_field_name("source") {
                let spec = strip_quotes(&w.text(src));
                let bindings = ts_import_bindings(w, node);
                w.push_import(spec, bindings);
            }
        }
        // Only the tsx and js grammars produce this node; the ts grammar
        // has no JSX, so no language guard is needed here.
        "jsx_element" => {
            if let Some(opening) = node.child_by_field_name("open_tag")
                && let Some(tag_node) = opening.child_by_field_name("name")
            {
                let tag = w.text(tag_node);
                let has_handler = jsx_has_handler(w, opening);
                let text_content = jsx_text_content(w, node, opening, &tag);
                jsx_handler_refs(w, opening, &tag);
                if let Some((name, receiver)) = jsx_component_call(&tag) {
                    w.push_call(name, receiver, node);
                }
                w.push_jsx_element(
                    tag,
                    has_handler,
                    text_content,
                    line_start(node),
                    line_end(node),
                );
            }
        }
        "jsx_self_closing_element" if matches!(lang, "tsx" | "js") => {
            if let Some(tag_node) = node.child_by_field_name("name") {
                let tag = w.text(tag_node);
                let has_handler = jsx_has_handler(w, node);
                let text_content = jsx_attr_text(w, node);
                jsx_handler_refs(w, node, &tag);
                if let Some((name, receiver)) = jsx_component_call(&tag) {
                    w.push_call(name, receiver, node);
                }
                w.push_jsx_element(
                    tag,
                    has_handler,
                    text_content,
                    line_start(node),
                    line_end(node),
                );
            }
        }
        _ => {}
    }
    for child in each_child(node) {
        walk_ts(w, lang, child, depth + 1);
    }
    if pushed {
        w.stack.pop();
    }
}

/// Extract named import bindings from a TS/JS `import_statement` or
/// `export_statement ... from "..."`. Handles:
/// - `import { greet, farewell } from "./a"` → `greet`, `farewell`
/// - `import { greet as hi } from "./a"` → `hi` bound to the source `greet`
/// - `import greet from "./a"` → `greet` (default import)
/// - `import * as ns from "./a"` → nothing (wildcard — no tracked bindings)
/// - `import greet, { helper } from "./a"` → `greet`, `helper`
/// - `export { greet } from "./a"` → `greet` (a re-export keeps the source
///   name on both sides: `rename` rewrites it there)
///
/// Returns empty for wildcard imports and unparseable forms; T1 then falls
/// back to file-level matching (the safe, pre-fix behavior).
fn ts_import_bindings(w: &Walker, node: Node) -> Vec<ImportBinding> {
    let mut bindings = Vec::new();
    for child in each_child(node) {
        match child.kind() {
            // Named imports: `import { greet, farewell as f } from "./a"`
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
                        // Default import: `import greet from "./a"`
                        "identifier" => {
                            let name = w.text(sub);
                            if !name.is_empty() {
                                bindings.push(ImportBinding::named(name));
                            }
                        }
                        // Wildcard: `import * as ns` — no tracked bindings.
                        "namespace_import" | "import_namespace_clause" => {
                            return Vec::new();
                        }
                        _ => {}
                    }
                }
            }
            // Re-export: `export { greet } from "./a"`
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
    let child = node.child_by_field_name(field)?;
    let text = w.text(child);
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
            Some((name.to_string(), Some(receiver.to_string())))
        }
        Some(_) => None,
        None => tag
            .starts_with(|c: char| c.is_ascii_uppercase())
            .then(|| (tag.to_string(), None)),
    }
}

fn jsx_attr_name(w: &Walker, attr: Node) -> Option<String> {
    // jsx_attribute has no named fields and names like `aria-label` are parsed
    // as jsx_namespace_name. Use the raw attribute text and split on the first `=`.
    let raw = w.text(attr);
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    raw.split('=').next().map(|s| s.trim().to_string())
}

fn jsx_attr_value(w: &Walker, attr: Node) -> Option<String> {
    let raw = w.text(attr);
    let raw = raw.trim();
    if let Some((_, value)) = raw.split_once('=') {
        let value = strip_quotes(value.trim());
        if value.is_empty() { None } else { Some(value) }
    } else {
        None
    }
}

fn jsx_has_handler(w: &Walker, element: Node) -> bool {
    for child in each_child(element) {
        if child.kind() == "jsx_attribute"
            && let Some(name) = jsx_attr_name(w, child)
        {
            let n = name.to_lowercase();
            if n.starts_with("on") || n == "href" || n == "to" {
                return true;
            }
        }
    }
    false
}

/// Emit `references` edges for JSX event-handler props: `onClick={handler}`,
/// `onSubmit={this.save}`. Without this, a handler like `handleSubmit` has
/// zero edges and reads as dead code even though the element wires it.
/// Only bare identifiers and member expressions produce edges — inline
/// arrows (`onClick={() => f()}`) are walked as ordinary code, so their
/// inner calls are already extracted.
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
        let mut cursor = child.walk();
        for part in child.children(&mut cursor) {
            if part.kind() != "jsx_expression" {
                continue;
            }
            let mut inner = part.walk();
            for expr in part.children(&mut inner) {
                match expr.kind() {
                    "identifier" => {
                        let name = w.text(expr);
                        w.push_reference(name, child, arg_of.clone());
                    }
                    "member_expression" => {
                        if let Some(prop) = expr.child_by_field_name("property") {
                            let name = w.text(prop);
                            w.push_reference(name, child, arg_of.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn jsx_attr_text(w: &Walker, element: Node) -> String {
    for child in each_child(element) {
        if child.kind() == "jsx_attribute"
            && let Some(name) = jsx_attr_name(w, child)
        {
            let n = name.to_lowercase();
            if (n == "aria-label" || n == "title")
                && let Some(v) = jsx_attr_value(w, child)
                && !v.is_empty()
            {
                return v;
            }
        }
    }
    String::new()
}

fn jsx_text_content(w: &Walker, element: Node, opening: Node, tag: &str) -> String {
    let mut parts = Vec::new();
    for child in each_child(element) {
        if child.kind() == "jsx_text" {
            let t = w.text(child);
            if !t.trim().is_empty() {
                parts.push(t.trim().to_string());
            }
        }
    }
    if !parts.is_empty() {
        return parts.join(" ").trim().to_string();
    }
    // Fallback to aria-label or title attribute for interactive tags.
    if matches!(tag.to_lowercase().as_str(), "button" | "a" | "link") {
        let fallback = jsx_attr_text(w, opening);
        if !fallback.is_empty() {
            return fallback;
        }
    }
    String::new()
}

// --- Rust ----------------------------------------------------------------

fn walk_rust(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    if rust_is_test_container(w, node) {
        return;
    }
    let mut pushed = false;
    let outer_trait_impl = w.in_trait_impl;
    match node.kind() {
        "function_item" => {
            if let Some(name) = field_text(w, node, "name") {
                let (kind, q) = if w.stack.is_empty() {
                    (SymbolKind::Function, name.clone())
                } else {
                    (SymbolKind::Method, w.qualify(&name, "::"))
                };
                w.push_symbol(name, q, kind, node);
            }
        }
        "impl_item" => {
            if let Some(ty) = field_text(w, node, "type") {
                let base = ty.split('<').next().unwrap_or(&ty).trim().to_string();
                w.stack.push(base);
                pushed = true;
            }
            w.in_trait_impl = node.child_by_field_name("trait").is_some();
        }
        "struct_item" => {
            if let Some(name) = field_text(w, node, "name") {
                w.push_symbol(name.clone(), name, SymbolKind::Struct, node);
            }
        }
        "enum_item" => {
            if let Some(name) = field_text(w, node, "name") {
                w.push_symbol(name.clone(), name.clone(), SymbolKind::Enum, node);
                // Qualify each variant as `Enum::Variant` for the subtree.
                w.stack.push(name);
                pushed = true;
            }
        }
        "enum_variant" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, "::");
                w.push_symbol(name, q, SymbolKind::Variant, node);
            }
        }
        "trait_item" => {
            if let Some(name) = field_text(w, node, "name") {
                w.push_symbol(name.clone(), name.clone(), SymbolKind::Trait, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "mod_item" => {
            if let Some(name) = field_text(w, node, "name") {
                // `mod foo;` names the file that holds the module's code;
                // `mod foo { … }` defines it in this file.
                if node.child_by_field_name("body").is_some() {
                    w.push_symbol(name.clone(), name, SymbolKind::Module, node);
                } else {
                    w.push_module_decl(name, node);
                }
            }
        }
        "const_item" | "static_item" => {
            if let Some(name) = field_text(w, node, "name") {
                w.push_symbol(name.clone(), name, SymbolKind::Const, node);
            }
        }
        "call_expression" => {
            let mut callee_name: Option<String> = None;
            if let Some(f) = node.child_by_field_name("function") {
                callee_name = rust_callee(w, node, f);
            }
            // After extracting the callee, check arguments for identifier
            // references (callbacks / closures passed as args).
            walk_call_arguments(w, node, callee_name);
        }
        "use_declaration" => {
            if let Some(arg) = node.child_by_field_name("argument") {
                let spec = w.text(arg);
                let leaves = rust_use_leaves(w.src, arg);
                let scope = rust_use_scope(w, node);
                if leaves.is_empty() {
                    w.push_import_at(spec.clone(), spec.clone(), Vec::new(), scope.clone());
                }
                for leaf in leaves {
                    w.push_import_at(
                        spec.clone(),
                        leaf.path,
                        leaf.binding.into_iter().collect(),
                        scope.clone(),
                    );
                }
            }
        }
        _ => {}
    }
    for child in each_child(node) {
        walk_rust(w, child, depth + 1);
    }
    if pushed {
        w.stack.pop();
    }
    w.in_trait_impl = outer_trait_impl;
}

/// The lines where the names a Rust `use` binds are in scope (see
/// `RawImport::scope`). The scope is the innermost enclosing block, inline
/// module or file, minus the inline modules nested in it: a module does not
/// see its parent's names, unless it glob-imports them with `use super::*;`
/// — and then only a module's names, since `super` names a module, never a
/// block. A file-level `use` with no such module to exclude is in scope
/// everywhere: empty.
fn rust_use_scope(w: &mut Walker, use_node: Node) -> Vec<(u32, u32)> {
    // Climb to the enclosing block or inline module; the walk ends on the
    // file's root otherwise. A `use` never sits under a `mod foo;`, which
    // has no body, so any `mod_item` ancestor is an inline module.
    let mut scope = use_node;
    let mut is_module = true;
    while let Some(parent) = scope.parent() {
        scope = parent;
        match parent.kind() {
            "block" => {
                is_module = false;
                break;
            }
            "mod_item" => break,
            _ => {}
        }
    }
    if let Some(cached) = w.use_scopes.get(&scope.id()) {
        return cached.clone();
    }
    let excluded = hidden_modules(w, scope, is_module);
    let whole_file = scope.kind() == "source_file";
    let ranges = if whole_file && excluded.is_empty() {
        Vec::new()
    } else {
        subtract_line_ranges((line_start(scope), line_end(scope)), &excluded)
    };
    w.use_scopes.insert(scope.id(), ranges.clone());
    ranges
}

/// The line ranges of the inline modules under `scope` that do not see its
/// names, sorted. With `through_glob`, a module that glob-imports its parent
/// sees them, and only the modules nested in it are examined. An explicit
/// stack rather than recursion: a deeply nested file cannot overflow it.
fn hidden_modules(w: &Walker, scope: Node, through_glob: bool) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut pending = each_child(scope);
    while let Some(node) = pending.pop() {
        if node.kind() == "mod_item"
            && let Some(body) = node.child_by_field_name("body")
        {
            if through_glob && glob_imports_parent(w, body) {
                pending.extend(each_child(body));
            } else {
                out.push((line_start(node), line_end(node)));
            }
        } else {
            pending.extend(each_child(node));
        }
    }
    out.sort_unstable();
    out
}

/// True iff the module body holds `use super::*;` at its own level.
fn glob_imports_parent(w: &Walker, body: Node) -> bool {
    each_child(body).into_iter().any(|item| {
        item.kind() == "use_declaration"
            && item
                .child_by_field_name("argument")
                .is_some_and(|arg| use_path_text(w.src, arg) == "super::*")
    })
}

/// `span` minus the sorted, disjoint `holes`, as inclusive line ranges.
fn subtract_line_ranges(span: (u32, u32), holes: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut start = span.0;
    for &(hole_start, hole_end) in holes {
        if hole_start > start {
            out.push((start, hole_start - 1));
        }
        start = start.max(hole_end.saturating_add(1));
    }
    if start <= span.1 {
        out.push((start, span.1));
    }
    out
}

/// One path a Rust `use` names, with the item name it brings into scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UseLeaf {
    /// The full path, prefixes of enclosing groups included
    /// (`crate::left::push` in `use crate::{left::push, right::publish};`).
    pub path: String,
    /// `None` for `self`, a wildcard, or anything else that names no item.
    pub binding: Option<ImportBinding>,
    /// Byte range of the source item's name, the text `rename` rewrites.
    pub source_bytes: Option<std::ops::Range<usize>>,
}

/// The paths a Rust `use` argument names, the way `ts_import_bindings`
/// reads a TS import:
/// - `use crate::push::push;` → `crate::push::push` binding `push`
/// - `use crate::push::{PushOptions, push};` → `crate::push::PushOptions`,
///   `crate::push::push`
/// - `use a::{b::{c, d}, e};` → `a::b::c`, `a::b::d`, `a::e`
/// - `use a::b as c;` → `a::b`, binding `c` to the source item `b`
/// - `use a::*;`, `use a::{self};` → `a::*`, `a::self`, binding nothing
///
/// One path per leaf, not one per statement: `use crate::{left::push,
/// right::publish};` names two files, and resolving the statement as a whole
/// found neither (the text before `{` is `crate`), while `use
/// crate::push::{a::x, y};` sent `x` to `push.rs` instead of `push/a.rs`.
pub(crate) fn rust_use_leaves(src: &[u8], node: Node) -> Vec<UseLeaf> {
    let mut out = Vec::new();
    collect_use_leaves(src, node, "", &mut out);
    out
}

/// `node`'s text with whitespace removed: a path split across lines
/// (`crate::\n    push`) still names `crate::push`.
fn use_path_text(src: &[u8], node: Node) -> String {
    String::from_utf8_lossy(&src[node.byte_range()])
        .split_whitespace()
        .collect()
}

fn join_use_path(prefix: &str, rest: &str) -> String {
    if prefix.is_empty() {
        rest.to_string()
    } else {
        format!("{prefix}::{rest}")
    }
}

fn collect_use_leaves(src: &[u8], node: Node, prefix: &str, out: &mut Vec<UseLeaf>) {
    let text = || use_path_text(src, node);
    match node.kind() {
        "identifier" | "type_identifier" => out.push(UseLeaf {
            path: join_use_path(prefix, &text()),
            binding: Some(ImportBinding::named(text())),
            source_bytes: Some(node.byte_range()),
        }),
        "self" | "crate" | "super" | "use_wildcard" => out.push(UseLeaf {
            path: join_use_path(prefix, &text()),
            binding: None,
            source_bytes: None,
        }),
        "scoped_identifier" => {
            let name = node
                .child_by_field_name("name")
                .filter(|n| matches!(n.kind(), "identifier" | "type_identifier"));
            out.push(UseLeaf {
                path: join_use_path(prefix, &text()),
                binding: name.map(|n| ImportBinding::named(use_path_text(src, n))),
                source_bytes: name.map(|n| n.byte_range()),
            });
        }
        "use_as_clause" => {
            let mut inner = Vec::new();
            if let Some(path) = node.child_by_field_name("path") {
                collect_use_leaves(src, path, prefix, &mut inner);
            }
            // Only the alias is in scope: `use a::push as leased;` makes
            // `leased()` a call to `push` and leaves `push()` unbound.
            if let ([leaf], Some(alias)) = (inner.as_slice(), node.child_by_field_name("alias")) {
                out.push(UseLeaf {
                    path: leaf.path.clone(),
                    binding: leaf.binding.as_ref().map(|b| {
                        ImportBinding::aliased(b.source.clone(), use_path_text(src, alias))
                    }),
                    source_bytes: leaf.source_bytes.clone(),
                });
            }
        }
        "scoped_use_list" => {
            let prefix = node.child_by_field_name("path").map_or_else(
                || prefix.to_string(),
                |p| join_use_path(prefix, &use_path_text(src, p)),
            );
            if let Some(list) = node.child_by_field_name("list") {
                collect_use_leaves(src, list, &prefix, out);
            }
        }
        "use_list" => {
            for child in each_child(node) {
                collect_use_leaves(src, child, prefix, out);
            }
        }
        _ => {}
    }
}

fn rust_is_test_container(w: &Walker, node: Node) -> bool {
    if !matches!(node.kind(), "function_item" | "mod_item") {
        return false;
    }
    let range = node.byte_range();
    let end = range.end.min(range.start.saturating_add(512));
    let prefix = String::from_utf8_lossy(&w.src[range.start..end]);
    let header = prefix.split('{').next().unwrap_or(&prefix);
    let mut attributes = String::new();
    let mut sibling = node.prev_named_sibling();
    while let Some(previous) = sibling {
        if previous.kind() != "attribute_item" {
            break;
        }
        attributes.push_str(&w.text(previous));
        sibling = previous.prev_named_sibling();
    }
    let markers = format!("{attributes}{header}");
    markers.contains("#[test]")
        || markers.contains("::test]")
        || markers.contains("::test(")
        || (node.kind() == "mod_item" && markers.contains("#[cfg(test)]"))
}

fn rust_callee(w: &mut Walker, call: Node, f: Node) -> Option<String> {
    match f.kind() {
        "identifier" => {
            let name = w.text(f);
            w.push_call(name.clone(), None, call);
            Some(name)
        }
        "scoped_identifier" => {
            if let Some(name) = field_text(w, f, "name") {
                let recv = field_text(w, f, "path");
                w.push_call(name.clone(), recv, call);
                Some(name)
            } else {
                None
            }
        }
        "field_expression" => {
            if let Some(name) = field_text(w, f, "field") {
                // `.` marks a method call (`split_method_receiver`): the
                // receiver is a value, never a module path.
                let recv = f.child_by_field_name("value").map(|value| {
                    let receiver =
                        rust_receiver_type(w, call, value).unwrap_or_else(|| w.text(value));
                    format!(".{receiver}")
                });
                w.push_call(name.clone(), recv, call);
                Some(name)
            } else {
                None
            }
        }
        "generic_function" => {
            if let Some(inner) = f.child_by_field_name("function") {
                rust_callee(w, call, inner)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The type of a method call's receiver `value`, when the code states it,
/// recorded as the call's receiver instead of the expression so the
/// resolver's receiver-type tiebreak (`qualified_match`) can pick the
/// method of that type among same-name definitions:
///
/// - a local read where it is bound: a parameter annotated with a path type
///   (`runner: &GitRunner`, `mut idx: pixel_graph::Index<'_>`), or a `let`
///   whose annotation or value names one (`let runner: GitRunner = …`,
///   `let runner = GitRunner::new(root);`, `let s = Store { … };`);
/// - a constructor call as the receiver itself (`GitRunner::new(root).x()`).
///
/// Only `new` and `default` count as constructors: they return `Self` by
/// convention, where `open()?` or `from_env()` may return anything. `None`
/// (the expression is kept) when the binding the call reads states no type,
/// is shadowed by a pattern (`for`, `match`, `if let`, a closure parameter,
/// a destructuring `let`), or is not found before the function's own
/// parameters.
fn rust_receiver_type(w: &Walker, call: Node, value: Node) -> Option<String> {
    match value.kind() {
        "identifier" => rust_local_type(w, call, &w.text(value)),
        "call_expression" => rust_constructed_type(w, value),
        _ => None,
    }
}

/// The type stated for the local `ident` that `call` reads: the nearest
/// binding before it, searched outward from the call through each enclosing
/// block, pattern and closure up to the function's parameters.
fn rust_local_type(w: &Walker, call: Node, ident: &str) -> Option<String> {
    let site = call.start_byte();
    let mut node = call;
    while let Some(parent) = node.parent() {
        match parent.kind() {
            "block" => {
                let lets = each_child(parent)
                    .into_iter()
                    .filter(|c| c.kind() == "let_declaration" && c.end_byte() <= site);
                let mut bound = None;
                for binding in lets {
                    if let Some(ty) = rust_let_binding(w, binding, ident) {
                        bound = Some(ty);
                    }
                }
                if let Some(ty) = bound {
                    return ty;
                }
            }
            "function_item" => {
                return rust_param_binding(w, parent.child_by_field_name("parameters")?, ident)?;
            }
            "closure_expression" => {
                if let Some(params) = parent.child_by_field_name("parameters")
                    && let Some(ty) = rust_param_binding(w, params, ident)
                {
                    return ty;
                }
            }
            "for_expression" | "match_arm" | "if_expression" | "while_expression" => {
                let patterns = ["pattern", "condition"]
                    .iter()
                    .filter_map(|field| parent.child_by_field_name(field))
                    .filter(|p| p.id() != node.id());
                for pattern in patterns {
                    if rust_binds(w, pattern, ident) {
                        return None;
                    }
                }
            }
            _ => {}
        }
        node = parent;
    }
    None
}

/// `Some(type)` when the `let` binds `ident`: the stated type when its
/// pattern is `ident` alone (`None` when neither its annotation nor its value
/// states one), `None` inside the outer `Some` for a destructuring pattern
/// that binds it. `None` when the `let` does not bind `ident`.
#[allow(clippy::option_option)]
fn rust_let_binding(w: &Walker, binding: Node, ident: &str) -> Option<Option<String>> {
    let pattern = binding.child_by_field_name("pattern")?;
    if !rust_binds(w, pattern, ident) {
        return None;
    }
    // Bound, so a bare identifier pattern is `ident` itself; anything else
    // destructures. A `mut` sits beside the pattern, not in it.
    if pattern.kind() != "identifier" {
        return Some(None);
    }
    Some(
        binding
            .child_by_field_name("type")
            .and_then(|ty| rust_type_name(w, ty))
            .or_else(|| {
                binding
                    .child_by_field_name("value")
                    .and_then(|value| rust_constructed_type(w, value))
            }),
    )
}

/// `Some(type)` when a parameter of `params` binds `ident` (a function's
/// `parameters` or a closure's `closure_parameters`): its annotated path
/// type, or `None` inside for an unannotated or destructured one.
#[allow(clippy::option_option)]
fn rust_param_binding(w: &Walker, params: Node, ident: &str) -> Option<Option<String>> {
    for param in each_child(params) {
        let (pattern, ty) = match param.kind() {
            "parameter" => (
                param.child_by_field_name("pattern"),
                param.child_by_field_name("type"),
            ),
            _ => (Some(param), None),
        };
        let Some(pattern) = pattern else { continue };
        if !rust_binds(w, pattern, ident) {
            continue;
        }
        if pattern.kind() != "identifier" {
            return Some(None);
        }
        return Some(ty.and_then(|ty| rust_type_name(w, ty)));
    }
    None
}

/// True iff an identifier `ident` appears in `pattern`: a pattern holding it
/// rebinds the name for the code it scopes.
fn rust_binds(w: &Walker, pattern: Node, ident: &str) -> bool {
    (pattern.kind() == "identifier" && w.text(pattern) == ident)
        || each_child(pattern)
            .into_iter()
            .any(|c| rust_binds(w, c, ident))
}

/// The last segment of a path type (`&mut pixel_git::GitRunner<'_>` →
/// `GitRunner`); `None` for any other type (`impl Trait`, `dyn`, tuples,
/// slices).
fn rust_type_name(w: &Walker, ty: Node) -> Option<String> {
    match ty.kind() {
        "type_identifier" => Some(w.text(ty)),
        "scoped_type_identifier" => field_text(w, ty, "name"),
        "generic_type" | "reference_type" => rust_type_name(w, ty.child_by_field_name("type")?),
        _ => None,
    }
}

/// The type a constructor expression builds: `T::new(…)`, `T::default()`
/// (`T` a path whose last segment is a type name), or a struct literal
/// `T { … }`. `Self::new()` names the enclosing impl's type.
fn rust_constructed_type(w: &Walker, value: Node) -> Option<String> {
    let segment = match value.kind() {
        "struct_expression" => rust_type_name(w, value.child_by_field_name("name")?)?,
        "call_expression" => {
            let f = value.child_by_field_name("function")?;
            if f.kind() != "scoped_identifier"
                || !matches!(field_text(w, f, "name").as_deref(), Some("new" | "default"))
            {
                return None;
            }
            let path = w.text(f.child_by_field_name("path")?);
            path.split("::<")
                .next()?
                .rsplit("::")
                .next()?
                .trim()
                .to_string()
        }
        _ => return None,
    };
    if segment == "Self" {
        return w.stack.last().cloned();
    }
    segment
        .starts_with(|c: char| c.is_ascii_uppercase())
        .then_some(segment)
}

// --- Go ------------------------------------------------------------------

fn walk_go(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    match node.kind() {
        "function_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                w.push_symbol(name.clone(), name, SymbolKind::Function, node);
            }
        }
        "method_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let recv = node
                    .child_by_field_name("receiver")
                    .and_then(|r| first_descendant_of_kind(r, "type_identifier"))
                    .map(|n| w.text(n));
                let q = match &recv {
                    Some(r) => format!("{r}.{name}"),
                    None => name.clone(),
                };
                w.push_symbol(name, q, SymbolKind::Method, node);
            }
        }
        "type_spec" => {
            if let (Some(name), Some(ty)) = (
                field_text(w, node, "name"),
                node.child_by_field_name("type"),
            ) {
                match ty.kind() {
                    "struct_type" => w.push_symbol(name.clone(), name, SymbolKind::Struct, node),
                    "interface_type" => {
                        w.push_symbol(name.clone(), name, SymbolKind::Interface, node)
                    }
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
                        w.push_call(name, None, node);
                    }
                    "selector_expression" => {
                        if let Some(name) = field_text(w, f, "field") {
                            let recv = field_text(w, f, "operand");
                            callee_name = Some(name.clone());
                            w.push_call(name, recv, node);
                        }
                    }
                    _ => {}
                }
            }
            // After extracting the callee, check arguments for identifier
            // references (callbacks / handlers passed as args).
            walk_call_arguments(w, node, callee_name);
        }
        "import_spec" => {
            if let Some(path) = node.child_by_field_name("path") {
                let spec = strip_quotes(&w.text(path));
                w.push_import(spec, Vec::new());
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
    for child in each_child(n) {
        if let Some(found) = first_descendant_of_kind(child, kind) {
            return Some(found);
        }
    }
    None
}

// --- Java ----------------------------------------------------------------

fn walk_java(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut pushed = false;
    match node.kind() {
        "class_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name.clone(), q, SymbolKind::Class, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "interface_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name.clone(), q, SymbolKind::Interface, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "enum_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Enum, node);
            }
        }
        "method_declaration" | "constructor_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Method, node);
            }
        }
        "method_invocation" => {
            let mut callee_name: Option<String> = None;
            if let Some(name) = field_text(w, node, "name") {
                let recv = field_text(w, node, "object");
                callee_name = Some(name.clone());
                w.push_call(name, recv, node);
            }
            // After extracting the callee, check arguments for identifier
            // references (callbacks / handlers passed as args).
            walk_call_arguments(w, node, callee_name);
        }
        "object_creation_expression" => {
            if let Some(ty) = field_text(w, node, "type") {
                let base = ty.split('<').next().unwrap_or(&ty);
                let name = base.rsplit('.').next().unwrap_or(base).trim().to_string();
                w.push_call(name, None, node);
            }
            // `new Foo(handler)` — constructor args can also be callbacks.
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
            w.push_import(spec, Vec::new());
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

// --- Python --------------------------------------------------------------

fn walk_python(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut pushed = false;
    match node.kind() {
        "class_definition" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name.clone(), q, SymbolKind::Class, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "function_definition" => {
            if let Some(name) = field_text(w, node, "name") {
                let (kind, q) = if w.stack.is_empty() {
                    (SymbolKind::Function, name.clone())
                } else {
                    (SymbolKind::Method, w.qualify(&name, "."))
                };
                w.push_symbol(name, q, kind, node);
            }
        }
        "call" => {
            let mut callee_name: Option<String> = None;
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => {
                        let name = w.text(f);
                        callee_name = Some(name.clone());
                        w.push_call(name, None, node);
                    }
                    "attribute" => {
                        if let Some(name) = field_text(w, f, "attribute") {
                            let recv = field_text(w, f, "object");
                            callee_name = Some(name.clone());
                            w.push_call(name, recv, node);
                        }
                    }
                    _ => {}
                }
            }
            // After extracting the callee, check arguments for identifier
            // references (callbacks / handlers passed as args).
            walk_call_arguments(w, node, callee_name);
        }
        "import_statement" => {
            for child in each_child(node) {
                match child.kind() {
                    "dotted_name" => {
                        let spec = w.text(child);
                        w.push_import(spec, Vec::new());
                    }
                    "aliased_import" => {
                        if let Some(name) = child.child_by_field_name("name") {
                            let spec = w.text(name);
                            w.push_import(spec, Vec::new());
                        }
                    }
                    _ => {}
                }
            }
        }
        "import_from_statement" => {
            if let Some(m) = node.child_by_field_name("module_name") {
                let spec = w.text(m);
                w.push_import(spec, Vec::new());
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

// --- C# -------------------------------------------------------------------

fn walk_csharp(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut pushed = false;
    match node.kind() {
        "namespace_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name.clone(), q, SymbolKind::Module, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "class_declaration" | "record_declaration" | "struct_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name.clone(), q, SymbolKind::Class, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "interface_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name.clone(), q, SymbolKind::Interface, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "enum_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Enum, node);
            }
        }
        "delegate_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Method, node);
            }
        }
        "method_declaration" | "constructor_declaration" | "local_function_statement" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Method, node);
            }
        }
        "property_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Method, node);
            }
        }
        "invocation_expression" => {
            let mut callee_name: Option<String> = None;
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => {
                        let name = w.text(f);
                        callee_name = Some(name.clone());
                        w.push_call(name, None, node);
                    }
                    "member_access_expression" => {
                        if let Some(name) = f
                            .child_by_field_name("name")
                            .map(|n| csharp_simple_name(w, n))
                        {
                            let recv = field_text(w, f, "expression");
                            callee_name = Some(name.clone());
                            w.push_call(name, recv, node);
                        }
                    }
                    "generic_name" => {
                        let name = csharp_simple_name(w, f);
                        callee_name = Some(name.clone());
                        w.push_call(name, None, node);
                    }
                    _ => {}
                }
            }
            // After extracting the callee, check arguments for identifier
            // references (callbacks / handlers passed as args).
            walk_call_arguments(w, node, callee_name);
        }
        "object_creation_expression" => {
            if let Some(ty) = field_text(w, node, "type") {
                let base = ty.split('<').next().unwrap_or(&ty);
                let name = base.rsplit('.').next().unwrap_or(base).trim().to_string();
                w.push_call(name, None, node);
            }
            // `new Foo(handler)` — constructor args can also be callbacks.
            walk_call_arguments(w, node, None);
        }
        "using_directive" => {
            // `name` field only exists for alias usings (`using Foo = X;`)
            // and holds the alias — not the imported namespace. The qualified
            // namespace is a plain child (`qualified_name` / `identifier`);
            // the alias is an `identifier` too, so it is skipped by position.
            let alias = node.child_by_field_name("name");
            for child in each_child(node) {
                if Some(child) == alias {
                    continue;
                }
                match child.kind() {
                    "qualified_name" | "identifier" => {
                        let spec = w.text(child);
                        w.push_import(spec, Vec::new());
                    }
                    _ => {}
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

// --- Ruby -----------------------------------------------------------------

/// `call` methods that load another file rather than invoke behaviour. Their
/// first string argument becomes an import spec instead of a call edge.
const RUBY_REQUIRE_METHODS: &[&str] =
    &["require", "require_relative", "require_dependency", "load"];

/// How an `identifier` reads in Ruby, decided by the slot it fills in its
/// parent node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RubyIdent {
    /// An expression: a method call unless a local of that name is in scope.
    Expr,
    /// A name that makes a local variable: assignment target, parameter,
    /// `for`/`rescue` variable, pattern binding.
    Binding,
    /// A method name being defined, called through `call`, or aliased.
    Name,
}

/// Role of an `identifier` child sitting in field `field` of a `parent` node.
fn ruby_identifier_role(parent: &str, field: Option<&str>) -> RubyIdent {
    match (parent, field) {
        ("assignment" | "operator_assignment", Some("left"))
        | ("left_assignment_list" | "rest_assignment" | "destructured_left_assignment", _)
        | (
            "method_parameters"
            | "lambda_parameters"
            | "block_parameters"
            | "destructured_parameter",
            _,
        )
        | (
            "optional_parameter"
            | "keyword_parameter"
            | "splat_parameter"
            | "hash_splat_parameter"
            | "block_parameter",
            Some("name"),
        )
        | ("for", Some("pattern"))
        | ("exception_variable" | "array_pattern" | "find_pattern" | "as_pattern", _)
        | ("in_clause" | "match_pattern" | "test_pattern", Some("pattern"))
        | ("parenthesized_pattern", _)
        | ("keyword_pattern", Some("value")) => RubyIdent::Binding,
        ("method" | "singleton_method", Some("name" | "object"))
        | ("call", Some("method"))
        | ("setter" | "alias" | "undef", _) => RubyIdent::Name,
        _ => RubyIdent::Expr,
    }
}

/// Whether a Ruby node kind opens a local-variable scope, and which kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RubyScope {
    /// `def`, `class`, `module`, the file: the enclosing locals are hidden.
    Gate,
    /// A block or lambda: the enclosing locals stay visible.
    Block,
}

fn ruby_scope_of(kind: &str) -> Option<RubyScope> {
    match kind {
        "program" | "method" | "singleton_method" | "class" | "module" | "singleton_class" => {
            Some(RubyScope::Gate)
        }
        "block" | "do_block" | "lambda" => Some(RubyScope::Block),
        _ => None,
    }
}

/// The local variables Ruby's parser knows at the current point of the walk,
/// one frame per open scope, innermost last.
///
/// Ruby's own rule: a name is a local once an assignment to it (or a
/// parameter of that name) has been parsed earlier in a visible scope;
/// otherwise the bare name is a method call.
#[derive(Debug, Default)]
struct RubyLocals {
    frames: Vec<RubyFrame>,
}

/// One open Ruby scope: the locals it binds and how a `def` in its body is
/// qualified.
#[derive(Debug)]
struct RubyFrame {
    scope: RubyScope,
    names: Vec<String>,
    defs: RubyDefs,
    /// The frame is a `module` body, where a bare `module_function` applies.
    module: bool,
    /// `self` is an instance here (a `def` body, or a block inside one), not
    /// the class or module being defined.
    instance_self: bool,
}

/// What a `def name` written in a frame's body defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RubyDefs {
    /// An instance method, `Klass#name`.
    Instance,
    /// A method of the class or module object itself, `Klass.name`: the body
    /// of `class << self`, or a module after a bare `module_function`, which
    /// Ruby applies to every following `def` until a `public`, `private` or
    /// `protected`. The private instance copy `module_function` also makes is
    /// not recorded: only an `include` of the module reaches it.
    /// <https://docs.ruby-lang.org/en/master/Module.html#method-i-module_function>
    Singleton,
}

impl RubyLocals {
    /// Open the frame of a node of `kind`. A block keeps the enclosing
    /// frame's `def` qualification and `self`, as a `def` in a block defines
    /// a method of the enclosing class.
    fn open(&mut self, scope: RubyScope, kind: &str, value_is_self: bool) {
        let enclosing = self.frames.last();
        let enclosing_defs = enclosing.map_or(RubyDefs::Instance, |f| f.defs);
        let enclosing_instance = enclosing.is_some_and(|f| f.instance_self);
        let (defs, instance_self) = match kind {
            "block" | "do_block" | "lambda" => (enclosing_defs, enclosing_instance),
            "method" => (RubyDefs::Instance, true),
            // `class << self` names the class or module only where `self` is
            // one: in a `def` body it is an instance, whose own singleton the
            // graph cannot name, so its methods keep instance qualification.
            "singleton_class" if value_is_self && !enclosing_instance => {
                (RubyDefs::Singleton, false)
            }
            _ => (RubyDefs::Instance, false),
        };
        self.frames.push(RubyFrame {
            scope,
            names: Vec::new(),
            defs,
            module: kind == "module",
            instance_self,
        });
    }

    fn close(&mut self) {
        self.frames.pop();
    }

    fn bind(&mut self, name: String) {
        if let Some(frame) = self.frames.last_mut() {
            frame.names.push(name);
        }
    }

    fn is_local(&self, name: &str) -> bool {
        for frame in self.frames.iter().rev() {
            if frame.names.iter().any(|known| known == name) {
                return true;
            }
            if frame.scope == RubyScope::Gate {
                return false;
            }
        }
        false
    }

    /// The qualification of a `def` written in the innermost frame's body:
    /// what a declaration there (`attr_reader`) generates.
    fn current_defs(&self) -> RubyDefs {
        self.frames.last().map_or(RubyDefs::Instance, |f| f.defs)
    }

    /// The qualification of a `def` whose own frame is the innermost one:
    /// the frame around it decides.
    fn enclosing_defs(&self) -> RubyDefs {
        self.frames
            .len()
            .checked_sub(2)
            .map_or(RubyDefs::Instance, |i| self.frames[i].defs)
    }

    /// A bare visibility word written directly in a `module` body:
    /// `module_function` makes the following `def`s module functions, and
    /// `public`/`private`/`protected` end that mode.
    fn visibility(&mut self, word: &str) {
        let Some(frame) = self.frames.last_mut().filter(|f| f.module) else {
            return;
        };
        match word {
            "module_function" => frame.defs = RubyDefs::Singleton,
            "public" | "private" | "protected" => frame.defs = RubyDefs::Instance,
            _ => {}
        }
    }
}

fn walk_ruby(w: &mut Walker, locals: &mut RubyLocals, node: Node, role: RubyIdent, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut pushed = false;
    let scope = ruby_scope_of(node.kind());
    if let Some(scope) = scope {
        let value_is_self = node
            .child_by_field_name("value")
            .is_some_and(|value| value.kind() == "self");
        locals.open(scope, node.kind(), value_is_self);
    }
    match node.kind() {
        "module" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, "::");
                w.push_symbol(name.clone(), q, SymbolKind::Module, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "class" => {
            // `name` may be a `scope_resolution` (`Admin::User`); keep the
            // full text so reopened namespaced classes qualify consistently.
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, "::");
                w.push_symbol(name.clone(), q, SymbolKind::Class, node);
                w.stack.push(name);
                pushed = true;
                ruby_mixins::walk_superclass(w, node);
            }
        }
        "method" => {
            if let Some(name) = field_text(w, node, "name") {
                let (kind, q) = if w.stack.is_empty() {
                    (SymbolKind::Function, name.clone())
                } else {
                    // Ruby convention: `Klass#instance_method`, and
                    // `Klass.class_method` for a singleton frame.
                    let separator = match locals.enclosing_defs() {
                        RubyDefs::Instance => '#',
                        RubyDefs::Singleton => '.',
                    };
                    (
                        SymbolKind::Method,
                        format!("{}{separator}{name}", w.stack.join("::")),
                    )
                };
                w.push_symbol(name, q, kind, node);
            }
        }
        "singleton_method" => {
            // `def self.foo` — Ruby convention: `Klass.class_method`. The
            // `object` is not pushed on the stack; the enclosing type is.
            if let Some(name) = field_text(w, node, "name") {
                let q = if w.stack.is_empty() {
                    name.clone()
                } else {
                    format!("{}.{}", w.stack.join("::"), name)
                };
                w.push_symbol(name, q, SymbolKind::Method, node);
            }
        }
        "call" => {
            walk_ruby_call(w, locals, node);
            // `class_methods do ... end` defines the concern's `ClassMethods`
            // module, which the includer extends.
            if !w.stack.is_empty() && ruby_mixins::is_class_methods_block(w, node) {
                let q = w.qualify("ClassMethods", "::");
                w.push_symbol("ClassMethods".to_string(), q, SymbolKind::Module, node);
                w.stack.push("ClassMethods".to_string());
                pushed = true;
            }
        }
        // A name read without receiver or parentheses (`target`, or the
        // receiver of `target.to_set`) calls the method unless a local of
        // that name is in scope; tree-sitter cannot tell the two apart.
        "identifier" => match role {
            RubyIdent::Binding => locals.bind(w.text(node)),
            RubyIdent::Expr => {
                let name = w.text(node);
                if !locals.is_local(&name) {
                    locals.visibility(&name);
                    w.push_call(name, None, node);
                }
            }
            RubyIdent::Name => {}
        },
        "alias" => ruby_generated::walk_alias_keyword(w, node, locals.current_defs()),
        // `in {target:}` binds `target` although no identifier is written.
        "keyword_pattern" if node.child_by_field_name("value").is_none() => {
            if let Some(key) = field_text(w, node, "key") {
                locals.bind(key);
            }
        }
        // `render(target:)` and `{ target: }` read `target` like a bare name.
        "pair" if node.child_by_field_name("value").is_none() => {
            if let Some(key) = field_text(w, node, "key")
                && !locals.is_local(&key)
            {
                w.push_call(key, None, node);
            }
        }
        _ => {}
    }
    for (index, child) in each_child(node).into_iter().enumerate() {
        let field = u32::try_from(index)
            .ok()
            .and_then(|index| node.field_name_for_child(index));
        let child_role = ruby_identifier_role(node.kind(), field);
        walk_ruby(w, locals, child, child_role, depth + 1);
    }
    if scope.is_some() {
        locals.close();
    }
    if pushed {
        w.stack.pop();
    }
}

/// A Ruby `call` node: an import, a generated-method declaration or a call,
/// and the method symbols its arguments name. Kept out of [`walk_ruby`] so
/// the recursive walker's stack frame stays small: the depth cap must be
/// reachable on a test thread's stack.
#[inline(never)]
fn walk_ruby_call(w: &mut Walker, locals: &mut RubyLocals, node: Node) {
    let mut callee_name: Option<String> = None;
    if let Some(name) = field_text(w, node, "method") {
        let recv = ruby_receiver(w, node);
        callee_name = Some(name.clone());
        // `module_function()` and `public()` set the mode as the
        // bare words do; with arguments they only touch the methods
        // they name.
        if recv.is_none()
            && node
                .child_by_field_name("arguments")
                .is_none_or(|args| args.named_child_count() == 0)
        {
            locals.visibility(&name);
        }
        ruby_mixins::walk_mixin_call(w, node, &name);
        if recv.is_none() && RUBY_REQUIRE_METHODS.contains(&name.as_str()) {
            if let Some(spec) = ruby_first_string_argument(w, node) {
                let path = ruby_require_path(&name, &spec);
                w.push_import_at(spec, path, Vec::new(), Vec::new());
            }
        } else {
            // A declaration that generated methods defines them; it is not
            // a call of the class body.
            let declared = recv.is_none()
                && ruby_generated::walk_declaration(w, node, &name, locals.current_defs());
            // Covers paren-less Rails DSL (`has_many :spots`,
            // `before_action :auth`) and receiver calls (`user.save`).
            // An assignment to `recv.name` calls the writer `name=`;
            // `recv.name += 1` reads with `name` and writes with `name=`.
            match ruby_assigned_through(node) {
                _ if declared => {}
                Some("assignment") => w.push_call(format!("{name}="), recv, node),
                Some(_) => {
                    w.push_call(format!("{name}="), recv.clone(), node);
                    w.push_call(name, recv, node);
                }
                None => w.push_call(name, recv, node),
            }
        }
    }
    // After extracting the callee, check arguments for identifier
    // references (callbacks / handlers passed as args). Skip require
    // methods — their string args are imports, not references.
    if !matches!(callee_name.as_deref(), Some(n) if RUBY_REQUIRE_METHODS.contains(&n)) {
        if let Some(method) = callee_name.as_deref() {
            ruby_callbacks::walk_symbol_arguments(w, node, method);
        }
        walk_call_arguments(w, node, callee_name);
    }
}

/// The kind of the assignment whose target `call` is (`self.name = v` is an
/// `assignment`, `self.count += 1` an `operator_assignment`), or `None` when
/// the call is not the left side of one. Only a call with a receiver can be:
/// a bare `name = v` assigns a local.
fn ruby_assigned_through(call: Node) -> Option<&'static str> {
    let parent = call.parent()?;
    let kind = match parent.kind() {
        "assignment" => "assignment",
        "operator_assignment" => "operator_assignment",
        _ => return None,
    };
    (parent.child_by_field_name("left")? == call && call.child_by_field_name("receiver").is_some())
        .then_some(kind)
}

/// Preserve receiver text except AST-confirmed constant factories/configurators.
/// Arguments cannot change which constant `Foo.new(args)` or `Job.set(args)`
/// names; keeping `Foo.new` / `Job.set` also survives stored-call replay.
fn ruby_receiver(w: &Walker, call: Node) -> Option<String> {
    let receiver = call.child_by_field_name("receiver")?;
    if receiver.kind() == "call"
        && let Some(method) = field_text(w, receiver, "method")
        && matches!(method.as_str(), "new" | "set")
        && let Some(owner) = receiver.child_by_field_name("receiver")
        && matches!(owner.kind(), "constant" | "scope_resolution")
    {
        return Some(format!("{}.{method}", w.text(owner)));
    }
    Some(w.text(receiver))
}

/// What `resolve_import` resolves for a Ruby load of `spec` through
/// `method`: `require_relative` names a file relative to the requiring one,
/// kept as an explicit `./`/`../` path; `require`, `require_dependency` and
/// `load` search the load path, kept bare. A load path spec written as a
/// relative or absolute path (`require "./x"`) is relative to the process's
/// working directory, which the graph cannot know: it resolves to nothing.
fn ruby_require_path(method: &str, spec: &str) -> String {
    let explicit = spec.starts_with("./") || spec.starts_with("../") || spec.starts_with('/');
    match (method, explicit) {
        ("require_relative", false) => format!("./{spec}"),
        ("require_relative", true) => spec.to_string(),
        (_, false) => spec.to_string(),
        (_, true) => String::new(),
    }
}

/// Literal text of the first `string` argument of a Ruby `call`, or `None`
/// when the first argument is missing, non-literal, or interpolated.
fn ruby_first_string_argument(w: &Walker, call: Node) -> Option<String> {
    let args = call.child_by_field_name("arguments")?;
    let first = each_child(args)
        .into_iter()
        .find(tree_sitter::Node::is_named)?;
    if first.kind() != "string" {
        return None;
    }
    let mut spec = String::new();
    for part in each_child(first) {
        match part.kind() {
            "string_content" => spec.push_str(&w.text(part)),
            "interpolation" => return None,
            _ => {}
        }
    }
    if spec.is_empty() { None } else { Some(spec) }
}

/// The plain name of a C# `generic_name` (`Create` for `Create<int>`), or any
/// other node's own text. The grammar gives `generic_name` no `name` field:
/// its identifier is a child beside the `type_argument_list`.
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

// --- Generic heuristic walker -------------------------------------------------
//
// Languages without a hand-written walker (php, c, swift, elixir, lua,
// and any future grammar wired into `language_for`) fall back to a node-kind
// heuristic pass. We match node kinds ending in `_declaration`/`_definition`
// for symbols、 call/invocation node kinds for call sites, and import/use/require
// node kinds for import specs. It is deliberately coarse:s a less precise graph
// beats an absent one. Field names and node kinds degrade gracefully to `None`.

fn walk_generic(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let kind = node.kind();
    let mut pushed = false;

    // Symbols: declarations and definitions carry a name we can qualify.
    if (kind.ends_with("_declaration") || kind.ends_with("_definition"))
        && let Some((sym_kind, is_container)) = generic_symbol_kind(kind)
        && let Some(name) = generic_name(w, node)
    {
        let q = w.qualify(&name, ".");
        if is_container {
            w.push_symbol(name.clone(), q, sym_kind, node);
            w.stack.push(name);
            pushed = true;
        } else {
            w.push_symbol(name, q, sym_kind, node);
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

/// Classify a `_declaration`/`_definition` node kind into a `SymbolKind`,
/// plus whether the declared item nests further symbols (containers: class,
/// struct, interface, trait, protocol, namespace, module, package). Bare
/// declaration kinds (import/use/attribute/parameter/preproc/deinit/typealias…)
/// are filtered out —— they inject neither symbols nor qualification.
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
    if kind.contains("class")
        || kind.contains("struct")
        || kind.contains("record")
        || kind.contains("actor")
    {
        return Some((SymbolKind::Class, true));
    }
    if kind.contains("interface") {
        return Some((SymbolKind::Interface, true));
    }
    if kind.contains("trait") || kind.contains("protocol") {
        return Some((SymbolKind::Trait, true));
    }
    if kind.contains("enum") {
        return Some((SymbolKind::Enum, false));
    }
    None
}

/// Best-effort symbol name for a declaration node. Prefers a `name` field,
/// then a `declarator` field (C function definitions nested type declarator),
/// then the first identifier-like named child (kotlin simple_identifier etc.).
fn generic_name(w: &Walker, node: Node) -> Option<String> {
    if let Some(name) = node.child_by_field_name("name") {
        let t = w.text(name);
        if !t.is_empty() && !t.contains(['(', ')', ',']) {
            return Some(t);
        }
    }
    // C: `function_definition` → `function_declarator` (or a pointer
    // declarator around it) → `identifier`. The identifier is a child of the
    // declarator, so recurse into the declarator itself, not its children.
    if let Some(decl) = node.child_by_field_name("declarator")
        && let Some(n) = generic_name(w, decl)
    {
        return Some(n);
    }
    for child in each_child(node) {
        match child.kind() {
            "simple_identifier" | "identifier" | "type_identifier" | "name" => {
                let t = w.text(child);
                if !t.is_empty() && !t.contains(['(', ')', ',']) {
                    return Some(t);
                }
            }
            _ => {}
        }
    }
    None
}

/// Extract a callee + optional receiver from a generic call/invocation node.
/// Handles direct identifiers (`function`/`callee`/`name`/`method`/`target`
/// fieldsh and member accesses (php `member_call_expression`, C `field_expression`
/// member of a join, etc.).
fn generic_call(w: &mut Walker, node: Node) {
    // Elixir `def`/`defmodule …` and the head a definer defines (`total(x)`
    // in `def total(x) do`) are definitions, not invocation sites.
    if elixir_definer(w, node) || elixir_definition_head(w, node) {
        return;
    }
    let mut callee: Option<String> = None;
    let mut receiver: Option<String> = None;
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
                receiver = pos_recv.map(|r| w.text(r));
            }
            _ => {}
        }
    }
    // php `scoped_call_expression` members a namelessqualified path; fall back
    // to dots in member name.
    if callee.is_none()
        && let Some(t) = node.child_by_field_name("target")
    {
        callee = Some(w.text(t));
    }
    if let (Some(c), r) = (callee, receiver) {
        w.push_call(c.clone(), r, node);
        // After extracting the callee, check arguments for identifier
        // references (callbacks / handlers passed as args).
        walk_call_arguments(w, node, Some(c));
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

/// True for the head a definer defines: `total(x)` in `def total(x) do`,
/// also behind a guard (`def total(x) when is_integer(x)`, where the head is
/// the left operand of `when`). The guard's own calls stay call sites.
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

/// Best-effort import spec from import/use/require node kinds. Prefers source-like
/// fields, then string/identifier children (php `require_expression`, kotlin
/// `import_header`, swift `import_declaration`).
fn generic_import(w: &mut Walker, node: Node) {
    for field in ["source", "path", "module_name", "import_string", "name"] {
        if let Some(src) = node.child_by_field_name(field) {
            let spec = strip_quotes(&w.text(src));
            if !spec.is_empty() {
                w.push_import(spec, Vec::new());
                return;
            }
        }
    }
    for child in each_child(node) {
        match child.kind() {
            // PHP parses a double-quoted path as `encapsed_string`.
            // An interpolated one (`"lib/$name.php"`) is chosen at run time:
            // only a double-quoted path made of plain text is an import.
            "string" | "encapsed_string"
                if each_child(child)
                    .into_iter()
                    .filter(Node::is_named)
                    .all(|c| c.kind() == "string_content") =>
            {
                let spec = strip_quotes(&w.text(child));
                if !spec.is_empty() {
                    w.push_import(spec, Vec::new());
                    return;
                }
            }
            "identifier" | "dotted_name" | "qualified_name" | "namespace_name" => {
                let spec = w.text(child);
                if !spec.is_empty() {
                    w.push_import(spec, Vec::new());
                    return;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FileExtraction, GENERATED_MAX_BYTES_PER_LINE, GENERATED_MIN_BYTES, ImportBinding, RawCall,
        RawSymbol, assign_enclosing, extract_file, is_generated_blob, jsx_component_call,
        parse_file, subtract_line_ranges,
    };
    use crate::store::SymbolKind;

    /// One line of `width` bytes plus its newline.
    fn line_of(width: usize) -> String {
        let mut s = "x".repeat(width);
        s.push('\n');
        s
    }

    /// A single-line minified bundle is skipped, and skipped for the reason
    /// that matters: pointing tree-sitter at it costs seconds of CPU and
    /// injects one junk symbol per minified function into the graph, which
    /// then competes with real code in every symbol lookup and ranking.
    ///
    /// Measured before the guard, release build: 3.5 MB of one-line JS took
    /// 5.97 s and produced 36 922 symbols; 1 MB took 768 ms for 10 880.
    #[test]
    fn minified_bundle_contributes_nothing_to_the_graph() {
        let mut bundle = String::new();
        let mut i = 0;
        while bundle.len() < 1_000_000 {
            bundle.push_str(&format!(
                "function f{i}(a,b){{return a?{{k:[b,{i}]}}:((c)=>{{let d={i};return d+c}})(b)}};"
            ));
            i += 1;
        }
        assert!(i > 5_000, "fixture should hold thousands of functions");

        assert!(is_generated_blob(bundle.as_bytes()));
        assert!(extract_file("public/assets/app.js", bundle.as_bytes()).is_none());
        // The rename verifier re-parses through `parse_file`: it must refuse
        // the same blob, or a rename could rewrite identifiers inside a
        // generated artifact that is regenerated from a source elsewhere.
        assert!(parse_file("public/assets/app.js", bundle.as_bytes()).is_none());
    }

    /// The guard must not cost real source its symbols. These two shapes are
    /// the ones that actually occur and that a naive per-line cap (MeshMCP
    /// rejects any line over 1024 bytes) would wrongly reject.
    #[test]
    fn dense_real_source_still_extracts() {
        // A file with one very long line — measured in the wild at 16 044
        // bytes — but an ordinary mean, as source always has.
        let mut ruby = String::from("class ParkingIdExtractor\n  def call\n    plates = \"");
        ruby.push_str(&"AB-123-CD ".repeat(1_700));
        ruby.push_str("\"\n    plates.split\n  end\nend\n");
        assert!(
            ruby.lines().map(str::len).max().unwrap() > 16_000,
            "fixture must carry a line longer than a per-line cap would allow"
        );
        assert!(!is_generated_blob(ruby.as_bytes()));
        let extracted = extract_file("app/services/parking_id_extractor.rb", ruby.as_bytes())
            .expect("dense but hand-written source must still extract");
        let names: Vec<&str> = extracted.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"call"), "got {names:?}");

        // A large ordinary file, well over the size floor: the biggest real
        // file measured in that monolith is a 400 KB `schema.rb` averaging
        // 47 bytes/line. Size alone must never trip the guard.
        let mut schema = String::from("# frozen_string_literal: true\n");
        while schema.len() < GENERATED_MIN_BYTES * 6 {
            schema.push_str("  create_table \"parkings\", force: :cascade do |t|\n  end\n");
        }
        assert!(!is_generated_blob(schema.as_bytes()));
    }

    /// Both conditions are load-bearing and the comparison is inclusive.
    /// Pinning the boundary keeps a widened threshold (or a flipped
    /// comparison) from silently swallowing real files.
    #[test]
    fn guard_boundary_is_exact_on_both_conditions() {
        // Exactly at the mean threshold and exactly at the size floor: in.
        let wide = line_of(GENERATED_MAX_BYTES_PER_LINE - 1);
        let mut at_threshold = String::new();
        while at_threshold.len() < GENERATED_MIN_BYTES {
            at_threshold.push_str(&wide);
        }
        assert_eq!(at_threshold.len() % GENERATED_MAX_BYTES_PER_LINE, 0);
        assert!(at_threshold.len() >= GENERATED_MIN_BYTES);
        assert!(
            is_generated_blob(at_threshold.as_bytes()),
            "mean of exactly {GENERATED_MAX_BYTES_PER_LINE} bytes/line must be caught"
        );

        // One byte narrower per line: mean falls below the threshold, out —
        // while still comfortably over the size floor, so it is the mean and
        // not the size that decides.
        let narrow = line_of(GENERATED_MAX_BYTES_PER_LINE - 2);
        let mut narrower = String::new();
        while narrower.len() < GENERATED_MIN_BYTES * 2 {
            narrower.push_str(&narrow);
        }
        assert!(narrower.len() >= GENERATED_MIN_BYTES);
        assert!(!is_generated_blob(narrower.as_bytes()));

        // Same dense shape, one byte under the size floor: out. Small files
        // parse in microseconds, so the guard buys nothing and only risks
        // false positives.
        let small = &at_threshold[..GENERATED_MIN_BYTES - 1];
        assert!(!is_generated_blob(small.as_bytes()));

        // A file with no trailing newline counts its last fragment as a
        // line, so a single unterminated line is one line, not zero.
        let unterminated = "y".repeat(GENERATED_MIN_BYTES);
        assert!(is_generated_blob(unterminated.as_bytes()));
        assert!(!is_generated_blob(b""));

        // …and the same off-by-one must not bite a multi-line file that
        // happens to lack its final newline. Sized so that counting the
        // unterminated tail (the right answer) puts the mean one byte under
        // the threshold, while dropping it would push the mean over and
        // condemn the file. Editors that strip the trailing newline are
        // common enough that this is a real shape, not a contrived one.
        let body_lines = 128;
        let mut no_final_newline = line_of(GENERATED_MAX_BYTES_PER_LINE - 2).repeat(body_lines);
        let counted_lines = body_lines + 1;
        let target_len = (GENERATED_MAX_BYTES_PER_LINE - 1) * counted_lines;
        no_final_newline.push_str(&"z".repeat(target_len - no_final_newline.len()));
        assert!(!no_final_newline.ends_with('\n'));
        assert!(no_final_newline.len() >= GENERATED_MIN_BYTES);
        assert!(
            no_final_newline.len() / body_lines >= GENERATED_MAX_BYTES_PER_LINE,
            "fixture must be one the wrong line count would condemn"
        );
        assert!(!is_generated_blob(no_final_newline.as_bytes()));
    }

    fn sym(name: &str, start_line: u32, end_line: u32) -> RawSymbol {
        RawSymbol {
            name: name.to_string(),
            qualified: name.to_string(),
            kind: SymbolKind::Function,
            start_line,
            end_line,
            sig: String::new(),
            trait_impl: false,
            module_decl: false,
        }
    }

    fn call(site_line: u32) -> RawCall {
        RawCall {
            callee_name: "f".to_string(),
            receiver: None,
            site_line,
            enclosing_index: None,
        }
    }

    /// A call site belongs to the smallest symbol whose line range holds it;
    /// among equal spans the first declared wins; outside every symbol it
    /// has no owner.
    #[test]
    fn assign_enclosing_picks_the_smallest_containing_symbol() {
        let mut fx = FileExtraction {
            lang: "rs",
            symbols: vec![
                sym("outer", 90, 100),
                sym("inner", 95, 96),
                sym("a", 1, 10),
                sym("b", 5, 14),
            ],
            calls: vec![call(95), call(98), call(7), call(50)],
            imports: vec![],
            jsx_elements: vec![],
            references: vec![],
            mixins: vec![],
        };
        assign_enclosing(&mut fx);
        let owners: Vec<Option<usize>> = fx.calls.iter().map(|c| c.enclosing_index).collect();
        assert_eq!(owners, vec![Some(1), Some(0), Some(2), None]);
    }

    #[test]
    fn typescript_arrow_and_function_expression_declarators_are_symbols() {
        let source =
            b"const arrow = () => 1;\nconst expr = function () { return 2; };\nconst value = 3;\n";
        let extraction = extract_file("src/a.ts", source).unwrap();
        let names: Vec<&str> = extraction.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"arrow"), "{names:?}");
        assert!(names.contains(&"expr"), "{names:?}");
        assert!(
            !names.contains(&"value"),
            "a plain value is not a symbol: {names:?}"
        );
    }

    #[test]
    fn rust_test_containers_do_not_enter_runtime_graph() {
        let source = br#"
fn production_entry() { production_helper(); }
fn production_helper() {}

#[test]
fn top_level_test() { production_entry(); }

#[cfg(test)]
mod tests {
    #[test]
    fn nested_test() { super::production_entry(); }
}
"#;
        let extraction = extract_file("src/lib.rs", source).unwrap();
        let names: Vec<_> = extraction
            .symbols
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect();
        assert!(names.contains(&"production_entry"));
        assert!(names.contains(&"production_helper"));
        assert!(!names.contains(&"top_level_test"));
        assert!(!names.contains(&"nested_test"));
        assert!(!names.contains(&"tests"));
    }

    /// `mod foo;` names the file that holds the module's code; `mod foo { … }`
    /// defines it here. Only the first must not grant this file an exact-name
    /// probe (see `targets::symbol_hits`).
    #[test]
    fn rust_module_declaration_distinguishes_external_from_inline() {
        let source = br#"
pub mod external;
mod inline {
    pub fn body() {}
}
"#;
        let extraction = extract_file("src/lib.rs", source).unwrap();
        let find = |name: &str| {
            extraction
                .symbols
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("module `{name}` is a symbol"))
        };
        assert!(
            find("external").module_decl,
            "`mod foo;` must be marked as naming another file"
        );
        assert!(
            !find("inline").module_decl,
            "`mod foo {{ … }}` defines code in this file"
        );
    }

    /// Enum variants are definitions the ident tier must find. Before
    /// `EXTRACTOR_VERSION` 4, `walk_rust` emitted only the enum, so an exact
    /// query for `SelfUpdate` fell through to string-concept noise.
    #[test]
    fn rust_enum_variants_are_symbols() {
        let source = br#"
enum Command {
    SelfUpdate { force: bool },
    RepoState,
    DryRun,
    ListErrors(usize),
}
"#;
        let extraction = extract_file("src/main.rs", source).unwrap();
        let variants: Vec<(&str, &str)> = extraction
            .symbols
            .iter()
            .filter(|s| s.kind.as_str() == "variant")
            .map(|s| (s.name.as_str(), s.qualified.as_str()))
            .collect();
        assert_eq!(
            variants,
            [
                ("SelfUpdate", "Command::SelfUpdate"),
                ("RepoState", "Command::RepoState"),
                ("DryRun", "Command::DryRun"),
                ("ListErrors", "Command::ListErrors"),
            ],
            "every enum variant is a symbol qualified by its enum"
        );
        assert!(
            extraction
                .symbols
                .iter()
                .any(|s| s.kind == SymbolKind::Enum && s.qualified == "Command"),
            "the enum itself stays a symbol"
        );
    }

    #[test]
    fn csharp_extracts_symbols_calls_and_imports() {
        let source = br#"
using System.Collections.Generic;

namespace MyApp.Services {
    interface IGreeter {
        string Greet(string who);
    }

    enum Status { Open, Closed }

    public struct Point { public int X; }

    public class Greeter : IGreeter {
        public Greeter() { }

        public string Greet(string who) {
            var list = new List<string>();
            list.Add(who);
            return $"Hello {who}";
        }

        public int Add(int a, int b) => a + b;
    }

    public delegate bool Predicate(int x);
}
"#;
        let extraction = extract_file("Greeter.cs", source).unwrap();
        assert_eq!(extraction.lang, "csharp");
        assert_eq!(extraction.imports.len(), 1);
        assert_eq!(extraction.imports[0].spec, "System.Collections.Generic");

        let names: Vec<_> = extraction.symbols.iter().map(|s| s.name.as_str()).collect();
        for expected in [
            "MyApp.Services",
            "IGreeter",
            "Status",
            "Point",
            "Greeter",
            "Greet",
            "Add",
            "Predicate",
        ] {
            assert!(
                names.contains(&expected),
                "missing symbol {expected}: {names:?}"
            );
        }

        // Call into List<string>.Add via member access: receiver `list`.
        let member_calls: Vec<_> = extraction
            .calls
            .iter()
            .filter(|c| c.receiver.is_some())
            .collect();
        assert!(
            member_calls
                .iter()
                .any(|c| c.callee_name == "Add" && c.receiver.as_deref() == Some("list")),
            "expected member call Add() on receiver `list`: {:?}",
            extraction.calls
        );
        // Constructor invocation of List.
        assert!(
            extraction.calls.iter().any(|c| c.callee_name == "List"),
            "expected object creation of List: {:?}",
            extraction.calls
        );

        // Symbols inside the class carry qualified names (both the
        // interface method and the class method have the same simple name).
        let greet_in_greeter = extraction
            .symbols
            .iter()
            .find(|s| s.qualified == "MyApp.Services.Greeter.Greet")
            .expect("class method Greet should exist with full qualification");
        assert_eq!(greet_in_greeter.name, "Greet");
        let greet_in_interface = extraction
            .symbols
            .iter()
            .find(|s| s.qualified == "MyApp.Services.IGreeter.Greet")
            .expect("interface method Greet should exist with full qualification");
        assert_eq!(greet_in_interface.name, "Greet");
    }

    /// `class << self` and a bare `module_function` define methods of the
    /// class or module object (`Klass.name`); the mode ends where Ruby ends
    /// it, and never leaks out of a `def` body or into `class << other`.
    #[test]
    fn ruby_singleton_defs_should_be_qualified_as_class_methods() {
        let source = br"
class User
  def self.direct; end
  class << self
    def importable; end
    private
    def hidden; end
    [1].each do
      def in_block; end
    end
  end
  def after; end
  class << other
    def elsewhere; end
  end
end

module Util
  def before; end
  module_function
  def slug(s); end
  def with_private_inside
    private
  end
  def still_function; end
  public
  def back_to_instance; end
end

module Plain
  def helper
    module_function
  end
  def instance_too; end
end

class Widget
  def build
    class << self
      def per_instance; end
    end
  end
  def self.setup
    class << self
      def meta; end
    end
  end
end

module Parens
  module_function()
  def a; end
  private :a
  def b; end
  public()
  def c; end
end
";
        let extraction = extract_file("lib/user.rb", source).unwrap();
        let methods: Vec<&str> = extraction
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .map(|s| s.qualified.as_str())
            .collect();
        assert_eq!(
            methods,
            [
                "User.direct",
                "User.importable",
                "User.hidden",
                "User.in_block",
                "User#after",
                "User#elsewhere",
                "Util#before",
                "Util.slug",
                "Util.with_private_inside",
                "Util.still_function",
                "Util#back_to_instance",
                "Plain#helper",
                "Plain#instance_too",
                "Widget#build",
                "Widget#per_instance",
                "Widget.setup",
                "Widget.meta",
                "Parens.a",
                "Parens.b",
                "Parens#c",
            ]
        );
    }

    #[test]
    fn ruby_extracts_rails_symbols_calls_and_imports() {
        let source = br#"
require "json"
require_relative "../lib/pricing"

module Admin
  class UsersController < ApplicationController
    before_action :authenticate!

    def index
      @users = User.where(active: true)
      render json: @users
    end

    def self.permitted_params
    end

    private

    def authenticate!
    end
  end
end

def helper
end
"#;
        let extraction = extract_file("app/controllers/admin/users_controller.rb", source).unwrap();
        assert_eq!(extraction.lang, "ruby");

        // `require` loads files: they must land in imports, in source order,
        // so import-tier resolution can link the spec to a repo file.
        let specs: Vec<_> = extraction.imports.iter().map(|i| i.spec.as_str()).collect();
        assert_eq!(
            specs,
            vec!["json", "../lib/pricing"],
            "require strings become import specs in order"
        );
        assert!(
            !extraction
                .calls
                .iter()
                .any(|c| c.callee_name == "require" || c.callee_name == "require_relative"),
            "require must not double as a call edge — it would resolve to nothing and pollute impact: {:?}",
            extraction.calls
        );

        let find = |q: &str| extraction.symbols.iter().find(|s| s.qualified == q);
        let module =
            find("Admin").expect("module Admin is a symbol so namespaced targets qualify under it");
        assert_eq!(module.kind, SymbolKind::Module);
        let class =
            find("Admin::UsersController").expect("class qualifies under its module with `::`");
        assert_eq!(class.kind, SymbolKind::Class);
        assert_eq!(class.name, "UsersController");
        // `#` vs `.` distinguishes instance from class methods so impact
        // lookups never merge `User#save` with `User.save`.
        let index =
            find("Admin::UsersController#index").expect("instance method qualifies with `#`");
        assert_eq!(index.kind, SymbolKind::Method);
        let params = find("Admin::UsersController.permitted_params")
            .expect("`def self.` qualifies with `.`");
        assert_eq!(params.kind, SymbolKind::Method);
        assert!(
            find("Admin::UsersController#authenticate!").is_some(),
            "bang methods keep their `!` — it is part of the Ruby name: {:?}",
            extraction
                .symbols
                .iter()
                .map(|s| &s.qualified)
                .collect::<Vec<_>>()
        );
        let helper =
            find("helper").expect("top-level def is a bare Function, not a method of anything");
        assert_eq!(helper.kind, SymbolKind::Function);

        let call = |name: &str| extraction.calls.iter().find(|c| c.callee_name == name);
        let before = call("before_action")
            .expect("paren-less Rails DSL is a call — hooks are how Rails wires behaviour");
        assert_eq!(before.receiver, None);
        let wher = call("where").expect("receiver call `User.where` is a call");
        assert_eq!(
            wher.receiver.as_deref(),
            Some("User"),
            "receiver is kept so class-method calls can resolve to `User.where`"
        );
        let render = call("render").expect("keyword-arg call without parens is still a call");
        assert_eq!(render.receiver, None);
        assert_eq!(
            wher.enclosing_index
                .map(|i| extraction.symbols[i].qualified.as_str()),
            Some("Admin::UsersController#index"),
            "call sites attach to the smallest enclosing method so impact walks method-to-method"
        );
    }

    #[test]
    fn ruby_root_calls_attach_to_script_while_method_calls_keep_method_owner() {
        let source = b"root_call()\n\ndef worker\n  nested_call()\nend\n";
        let extraction = extract_file("scripts/run.rb", source).unwrap();
        let script_index = extraction
            .symbols
            .iter()
            .position(|symbol| symbol.kind == SymbolKind::Script)
            .unwrap();
        let script = &extraction.symbols[script_index];
        assert_eq!(script.name, "scripts/run.rb");
        assert_eq!(script.qualified, "scripts/run.rb");
        assert_eq!((script.start_line, script.end_line), (1, 6));
        assert_eq!(script.sig, "scripts/run.rb");
        assert!(!script.trait_impl);
        assert!(!script.module_decl);
        let root_call = extraction
            .calls
            .iter()
            .find(|call| call.callee_name == "root_call")
            .unwrap();
        assert_eq!(root_call.enclosing_index, Some(script_index));
        let nested_call = extraction
            .calls
            .iter()
            .find(|call| call.callee_name == "nested_call")
            .unwrap();
        assert_eq!(
            nested_call
                .enclosing_index
                .map(|index| extraction.symbols[index].qualified.as_str()),
            Some("worker")
        );

        let non_ruby = extract_file("scripts/run.ts", b"function worker() {}\n").unwrap();
        assert!(
            non_ruby
                .symbols
                .iter()
                .all(|symbol| symbol.kind != SymbolKind::Script)
        );
    }

    /// Qualified name of the enclosing symbol of every call to `callee` in a
    /// Ruby source, in source order.
    fn ruby_callers_of(source: &str, callee: &str) -> Vec<String> {
        let extraction = extract_file("app/svc.rb", source.as_bytes()).unwrap();
        extraction
            .calls
            .iter()
            .filter(|call| call.callee_name == callee)
            .map(|call| {
                call.enclosing_index
                    .map_or_else(String::new, |i| extraction.symbols[i].qualified.clone())
            })
            .collect()
    }

    #[test]
    fn ruby_bare_identifier_should_be_a_call_when_no_local_of_that_name_is_in_scope() {
        // Ruby parses a name that no earlier assignment made local as a method
        // call. tree-sitter gives it an `identifier` node, not a `call`, and a
        // caller written `target` or `target.to_set` was invisible to impact.
        let source = [
            "class Svc",
            "  def chained",
            "    @ids ||= target.to_set",
            "  end",
            "  def bare",
            "    target",
            "  end",
            "  def parens",
            "    target()",
            "  end",
            "  def with_self",
            "    self.target",
            "  end",
            "  def as_argument",
            "    render(target, *target, &target, key: target)",
            "  end",
            "  def target",
            "    [1]",
            "  end",
            "end",
        ]
        .join("\n");
        assert_eq!(
            ruby_callers_of(&source, "target"),
            [
                "Svc#chained",
                "Svc#bare",
                "Svc#parens",
                "Svc#with_self",
                "Svc#as_argument",
                "Svc#as_argument",
                "Svc#as_argument",
                "Svc#as_argument",
            ],
            "every form of a call to `target` is an edge, and `def target` itself is not"
        );
        let extraction = extract_file("app/svc.rb", source.as_bytes()).unwrap();
        let to_set: Vec<_> = extraction
            .calls
            .iter()
            .filter(|call| call.callee_name == "to_set")
            .collect();
        assert_eq!(
            to_set.len(),
            1,
            "the method of a `call` stays one call, it is not recorded again as a bare name"
        );
        assert_eq!(to_set[0].receiver.as_deref(), Some("target"));
    }

    #[test]
    fn ruby_identifier_should_not_be_a_call_when_it_names_a_local() {
        // Every method below reads a local named `target`: an edge from any of
        // them to `def target` would be a caller that does not exist.
        let source = [
            "class Svc",
            "  def assigned",
            "    target = 1",
            "    target",
            "  end",
            "  def op_assigned",
            "    target ||= 1",
            "    target.succ",
            "  end",
            "  def multiple",
            "    first, target = 1, 2",
            "    target",
            "  end",
            "  def splat_assigned",
            "    first, *target = 1, 2",
            "    target",
            "  end",
            "  def nested_assigned",
            "    first, (second, target) = 1, [2, 3]",
            "    target",
            "  end",
            "  def positional(target)",
            "    target",
            "  end",
            "  def optional(target = 1)",
            "    target",
            "  end",
            "  def keyword(target: 1)",
            "    target",
            "  end",
            "  def splat(*target)",
            "    target",
            "  end",
            "  def double_splat(**target)",
            "    target",
            "  end",
            "  def block_arg(&target)",
            "    target",
            "  end",
            "  def block_param",
            "    [1].each { |target| target }",
            "  end",
            "  def block_local",
            "    [1].each { |x; target| target }",
            "  end",
            "  def destructured",
            "    [[1, 2]].each { |(x, target)| target }",
            "  end",
            "  def lambda_param",
            "    ->(target) { target }",
            "  end",
            "  def outer_local_in_block",
            "    target = 1",
            "    [1].each do target end",
            "  end",
            "  def looped",
            "    for target in [1]; end",
            "    target",
            "  end",
            "  def rescued",
            "    raise 'x'",
            "  rescue => target",
            "    target",
            "  end",
            "  def matched_array(v)",
            "    case v",
            "    in [target] then target",
            "    end",
            "  end",
            "  def matched_hash(v)",
            "    case v",
            "    in {k: target} then target",
            "    end",
            "  end",
            "  def matched_shorthand(v)",
            "    case v",
            "    in {target:} then target",
            "    end",
            "  end",
            "  def matched_as(v)",
            "    case v",
            "    in Integer => target then target",
            "    end",
            "  end",
            "  def matched_pin(v, target)",
            "    case v",
            "    in ^target then 1",
            "    end",
            "  end",
            "  alias other target",
            "  def target",
            "    [1]",
            "  end",
            "end",
        ]
        .join("\n");
        assert_eq!(
            ruby_callers_of(&source, "target"),
            Vec::<String>::new(),
            "a local, a parameter or a pattern binding named `target` is not a call"
        );
    }

    #[test]
    fn ruby_local_should_be_visible_only_after_its_assignment_and_inside_its_scope() {
        // `def`, `class` and `module` start a fresh local table; a block sees
        // the locals around it but its own die with it; a name read before
        // its assignment is still a method call.
        let source = [
            "target = 1",
            "module Ns",
            "  target",
            "end",
            "class Svc",
            "  target = 2",
            "  def gated",
            "    target",
            "  end",
            "  def not_leaked",
            "    [1].each { |target| target }",
            "    [1].each { target = 1 }",
            "    target",
            "  end",
            "  def before_assignment",
            "    target",
            "    target = 1",
            "  end",
            "  def self.class_side",
            "    target",
            "  end",
            "  def matched_key_with_value(v)",
            "    case v",
            "    in {target: 1} then target",
            "    end",
            "  end",
            "  class << self",
            "    target",
            "  end",
            "  def target",
            "  end",
            "end",
        ]
        .join("\n");
        assert_eq!(
            ruby_callers_of(&source, "target"),
            [
                "Ns",
                "Svc#gated",
                "Svc#not_leaked",
                "Svc#before_assignment",
                "Svc.class_side",
                "Svc#matched_key_with_value",
                "Svc",
            ],
            "only the reads no visible assignment precedes are calls"
        );
    }

    #[test]
    fn ruby_standalone_pattern_should_bind_its_names_like_a_case_in() {
        // `v => target`, `v in target` and `in (target)` bind `target`: the
        // pattern itself is no call, and neither is any later read.
        let source = [
            "class Svc",
            "  def rightward(v)",
            "    v => target",
            "    target",
            "  end",
            "  def boolean(v)",
            "    v in target",
            "    target",
            "  end",
            "  def parenthesized(v)",
            "    case v",
            "    in (target) then target",
            "    end",
            "  end",
            "  def target",
            "  end",
            "end",
        ]
        .join("\n");
        assert_eq!(ruby_callers_of(&source, "target"), Vec::<String>::new());
    }

    #[test]
    fn ruby_hash_shorthand_should_call_the_method_unless_a_local_has_that_name() {
        // Ruby 3.1 `render(target:)` is `render(target: target)`: a method call
        // when no local `target` exists, a local read otherwise. A key with a
        // value (`other: 1`) is only a key.
        let source = [
            "class Svc",
            "  def as_argument",
            "    render(target:, other: 1)",
            "  end",
            "  def as_literal",
            "    { target: }",
            "  end",
            "  def shadowed(target)",
            "    render(target:)",
            "  end",
            "  def target",
            "  end",
            "end",
        ]
        .join("\n");
        assert_eq!(
            ruby_callers_of(&source, "target"),
            ["Svc#as_argument", "Svc#as_literal"]
        );
        assert_eq!(
            ruby_callers_of(&source, "other"),
            Vec::<String>::new(),
            "a key written with its value calls nothing"
        );
    }

    #[test]
    fn ruby_walk_should_stop_at_the_depth_cap() {
        // The walker recurses once per tree level; the cap is what keeps a
        // pathological nesting from overflowing the stack.
        let deep = format!("{}deep_call(){}", "[".repeat(600), "]".repeat(600));
        let source = format!("shallow_call()\n{deep}\n");
        let extraction = extract_file("gen.rb", source.as_bytes()).unwrap();
        let names: Vec<_> = extraction
            .calls
            .iter()
            .map(|call| call.callee_name.as_str())
            .collect();
        assert_eq!(names, ["shallow_call"]);
    }

    #[test]
    fn jsx_button_with_onclick_is_wired() {
        let source = br#"function App() { return <button onClick={() => {}}>Save</button>; }"#;
        let extraction = extract_file("App.tsx", source).unwrap();
        assert_eq!(extraction.jsx_elements.len(), 1);
        let e = &extraction.jsx_elements[0];
        assert_eq!(e.tag, "button");
        assert!(e.has_handler);
        assert_eq!(e.text_content, "Save");
    }

    #[test]
    fn jsx_element_without_a_handler_prop_is_not_wired() {
        // `className` is an attribute but not a handler; `href` alone is.
        // (A wrong comparison used to let any non-href attribute count.)
        let source = br#"function App() { return <button className="x">Save</button>; }"#;
        let extraction = extract_file("App.tsx", source).unwrap();
        assert_eq!(extraction.jsx_elements.len(), 1);
        assert!(
            !extraction.jsx_elements[0].has_handler,
            "{:?}",
            extraction.jsx_elements
        );
        let source = br#"function App() { return <a href="/x">link</a>; }"#;
        let extraction = extract_file("App.tsx", source).unwrap();
        assert!(extraction.jsx_elements[0].has_handler);
        let source = br#"function App() { return <button>Save</button>; }"#;
        let extraction = extract_file("App.tsx", source).unwrap();
        assert!(
            !extraction.jsx_elements[0].has_handler,
            "no attributes at all"
        );
    }

    #[test]
    fn plain_ts_source_yields_no_jsx_elements() {
        let source = br#"function App() { return <button onClick={f}>Save</button>; }"#;
        let extraction = extract_file("App.ts", source).unwrap();
        assert!(
            extraction.jsx_elements.is_empty(),
            "{:?}",
            extraction.jsx_elements
        );
    }

    #[test]
    fn jsx_link_href_and_to_are_handlers() {
        let source = br#"
function App() {
  return (
    <>
      <a href="/home" className="x">Home</a>
      <Link to="/profile">Profile</Link>
    </>
  );
}
"#;
        let extraction = extract_file("App.tsx", source).unwrap();
        let tags: Vec<_> = extraction
            .jsx_elements
            .iter()
            .map(|e| (e.tag.as_str(), e.has_handler))
            .collect();
        assert!(tags.contains(&("a", true)));
        assert!(tags.contains(&("Link", true)));
    }

    #[test]
    fn jsx_aria_label_and_title_fallback() {
        let source = br#"
function App() {
  return (
    <>
      <button aria-label="Close" />
      <button title="Submit form" />
      <button aria-label="Dismiss" title="X">x</button>
    </>
  );
}
"#;
        let extraction = extract_file("App.tsx", source).unwrap();
        let texts: Vec<_> = extraction
            .jsx_elements
            .iter()
            .map(|e| e.text_content.as_str())
            .collect();
        assert!(texts.contains(&"Close"));
        assert!(texts.contains(&"Submit form"));
        // Visible text wins over title/aria-label when present.
        assert!(texts.contains(&"x"));
    }

    #[test]
    fn jsx_self_closing_button_is_extracted() {
        let source = br#"function App() { return <button onClick={handle} className="ok" aria-label="OK" />; }"#;
        let extraction = extract_file("App.tsx", source).unwrap();
        let e = extraction
            .jsx_elements
            .iter()
            .find(|e| e.tag == "button")
            .expect("button");
        assert!(e.has_handler);
        assert_eq!(e.text_content, "OK");
    }

    #[test]
    fn jsx_malformed_does_not_crash() {
        let source = br#"function App() { return <button onClick={}>  ; }"#;
        let extraction = extract_file("App.tsx", source);
        // Extraction should return a result even if the JSX is broken.
        assert!(extraction.is_some());
    }

    #[test]
    fn ts_callback_arg_extracted_as_reference() {
        // `schema.plugin(tenantScopePlugin)` — tenantScopePlugin is passed
        // as an argument to the `plugin` call, so it should be a RawReference
        // with arg_of = "plugin".
        let source = br#"
export function tenantScopePlugin(schema: any) { return schema; }
export function setup(schema: any) {
  schema.plugin(tenantScopePlugin);
}
"#;
        let extraction = extract_file("src/schema.ts", source).unwrap();
        let refs: Vec<_> = extraction
            .references
            .iter()
            .filter(|r| r.name == "tenantScopePlugin")
            .collect();
        assert_eq!(
            refs.len(),
            1,
            "tenantScopePlugin passed as arg: {:?}",
            extraction.references
        );
        assert_eq!(refs[0].name, "tenantScopePlugin");
        assert_eq!(refs[0].arg_of.as_deref(), Some("plugin"));
        // The reference is enclosed by `setup`, not `tenantScopePlugin`.
        let enclosing = refs[0]
            .enclosing_index
            .map(|i| extraction.symbols[i].name.as_str());
        assert_eq!(enclosing, Some("setup"));
    }

    #[test]
    fn ts_emitter_on_handler_extracted_as_reference() {
        // `emitter.on('event', handler)` — handler is passed as an argument
        // to the `on` call, so it should be a RawReference with arg_of = "on".
        let source = br#"
export function handler() {}
export function wire(emitter: any) {
  emitter.on('event', handler);
}
"#;
        let extraction = extract_file("src/emitter.ts", source).unwrap();
        let refs: Vec<_> = extraction
            .references
            .iter()
            .filter(|r| r.name == "handler")
            .collect();
        assert_eq!(
            refs.len(),
            1,
            "handler passed as arg: {:?}",
            extraction.references
        );
        assert_eq!(refs[0].arg_of.as_deref(), Some("on"));
    }

    fn reference_names(path: &str, source: &[u8]) -> Vec<String> {
        let extraction = extract_file(path, source).unwrap();
        extraction.references.into_iter().map(|r| r.name).collect()
    }

    #[test]
    fn ruby_callbacks_should_reference_methods_but_not_option_values_or_foreign_receivers() {
        let source = b"class Record\n  before_action :load, :authorize, only: :show\n  validate :check, if: :ready?, unless: :blocked?\n  after_commit :sync, on: :create\n  foo.before_action :foreign\n  self.before_action :explicit\n  before_action()\n  scope :active, -> { true }\nend\n";
        let fx = extract_file("record.rb", source).unwrap();
        assert_eq!(
            fx.references
                .iter()
                .map(|r| (
                    r.name.as_str(),
                    r.arg_of.as_deref(),
                    r.site_line,
                    r.enclosing_index
                ))
                .collect::<Vec<_>>(),
            [
                ("load", Some("before_action"), 2, Some(0)),
                ("authorize", Some("before_action"), 2, Some(0)),
                ("check", Some("validate"), 3, Some(0)),
                ("ready?", Some("validate"), 3, Some(0)),
                ("blocked?", Some("validate"), 3, Some(0)),
                ("sync", Some("after_commit"), 4, Some(0)),
            ],
            "only method symbols in supported callback declarations are references"
        );
    }

    #[test]
    fn ruby_callbacks_should_cover_the_documented_dsl_and_only_class_or_module_bodies() {
        let methods = [
            "before_action",
            "after_action",
            "around_action",
            "prepend_before_action",
            "prepend_after_action",
            "prepend_around_action",
            "append_before_action",
            "append_after_action",
            "append_around_action",
            "skip_before_action",
            "skip_after_action",
            "skip_around_action",
            "validate",
            "before_validation",
            "after_validation",
            "before_save",
            "around_save",
            "after_save",
            "before_create",
            "around_create",
            "after_create",
            "before_update",
            "around_update",
            "after_update",
            "before_destroy",
            "around_destroy",
            "after_destroy",
            "after_initialize",
            "after_find",
            "after_touch",
            "before_commit",
            "after_commit",
            "after_rollback",
            "after_create_commit",
            "after_update_commit",
            "after_destroy_commit",
            "after_save_commit",
            "helper_method",
            "before_enqueue",
            "around_enqueue",
            "after_enqueue",
            "before_perform",
            "around_perform",
            "after_perform",
        ];
        for method in methods {
            let source = format!(
                "{method} :top\nmodule Rules\n  {method} :check\nend\nclass Record\n  def run\n    {method} :inside\n  end\n  def self.configure\n    {method} :singleton_method\n  end\n  class << self\n    {method} :singleton\n  end\nend\n"
            );
            assert_eq!(
                reference_names("record.rb", source.as_bytes()),
                ["check"],
                "{method}"
            );
        }
    }

    #[test]
    fn ruby_send_should_reference_only_the_first_literal_symbol_on_self() {
        let source = b"class Record\n  def run\n    send(:check, :data)\n    self.public_send(:ready?, :other)\n    object.send(:foreign)\n    Record.send(:constant)\n    send(name, :dynamic)\n    send()\n    send(\"string\")\n  end\n  class << self\n    send(:unknown_owner)\n    def singleton_run\n      send(:class_target)\n    end\n  end\nend\nsend(:top_level)\n";
        assert_eq!(
            reference_names("record.rb", source),
            ["check", "ready?", "name", "class_target"]
        );
    }

    /// A member argument names a function only on a self receiver:
    /// `user.name` is data, and resolving `name` against every function of
    /// that name linked unrelated code.
    #[test]
    fn member_arguments_are_references_only_on_a_self_receiver() {
        assert_eq!(
            reference_names(
                "src/a.ts",
                b"export class C { go(user: any) { consume(user.name, this.onClick, handler); } }\n",
            ),
            ["onClick", "handler"]
        );
        assert_eq!(
            reference_names(
                "src/a.py",
                b"class C:\n    def go(self, obj):\n        register(self, self.handler, obj.attr, None)\n",
            ),
            ["handler"]
        );
        assert_eq!(
            reference_names(
                "src/a.rs",
                b"impl C { fn go(&self, cfg: Cfg) { run(cfg.field, self.handler, Self::helper, self); } }\n",
            ),
            ["handler", "helper"]
        );
    }

    #[test]
    fn jsx_component_call_names_rendered_components_only() {
        assert_eq!(
            jsx_component_call("Button"),
            Some(("Button".to_string(), None))
        );
        assert_eq!(
            jsx_component_call("Menu.Item"),
            Some(("Item".to_string(), Some("Menu".to_string())))
        );
        assert_eq!(
            jsx_component_call("ui.menu.item"),
            Some(("item".to_string(), Some("ui.menu".to_string())))
        );
        for intrinsic in ["div", "button", "svg:rect", "Svg:Rect", ".x", "x.", ""] {
            assert_eq!(jsx_component_call(intrinsic), None, "{intrinsic:?}");
        }
    }

    #[test]
    fn rendered_jsx_components_are_calls_and_intrinsic_tags_are_not() {
        let source = b"export function App() { return <div><Button label=\"x\" /><Menu.Item>go</Menu.Item></div>; }\n";
        let extraction = extract_file("src/App.tsx", source).unwrap();
        let calls: Vec<(&str, Option<&str>)> = extraction
            .calls
            .iter()
            .map(|c| (c.callee_name.as_str(), c.receiver.as_deref()))
            .collect();
        assert_eq!(calls, [("Button", None), ("Item", Some("Menu"))]);
        let app = extraction
            .symbols
            .iter()
            .position(|s| s.name == "App")
            .unwrap();
        assert!(
            extraction
                .calls
                .iter()
                .all(|c| c.enclosing_index == Some(app)),
            "the renderer encloses every component call: {:?}",
            extraction.calls
        );
    }

    /// `impl Display for X { fn fmt }` is called through the trait: its
    /// methods are marked, inherent and trait-definition methods are not,
    /// and the mark ends with the impl block, nested impls included.
    /// The receiver each `current_branch()` call records, by site line.
    fn rust_receivers(source: &str, callee: &str) -> Vec<(u32, Option<String>)> {
        let fx = extract_file("src/lib.rs", source.as_bytes()).expect("rust extracts");
        fx.calls
            .into_iter()
            .filter(|c| c.callee_name == callee)
            .map(|c| (c.site_line, c.receiver))
            .collect()
    }

    /// A method call on a local records the type the code states for it, so
    /// the resolver can pick that type's method among same-name definitions;
    /// every binding form that states one is read, and the nearest binding
    /// before the call wins.
    #[test]
    fn rust_receiver_records_the_stated_type_of_a_local() {
        let source = [
            "fn param(runner: &pixel_git::GitRunner) { runner.current_branch(); }",
            "fn param_mut(mut runner: GitRunner<'_>) { runner.current_branch(); }",
            "fn ctor(root: &Path) { let runner = GitRunner::new(root); runner.current_branch(); }",
            "fn annotated() { let runner: Box<GitRunner> = make(); runner.current_branch(); }",
            "fn literal() { let mut runner = Runner { root }; runner.current_branch(); }",
            "fn chained(root: &Path) { GitRunner::new(root).current_branch(); }",
            "fn default_ctor() { git::GitRunner::default().current_branch(); }",
            "fn closure(runner: &GitRunner) { let f = || runner.current_branch(); }",
            "fn nearest(runner: &Old) { let runner = GitRunner::new(root); runner.current_branch(); }",
            "fn other_names(runner: &GitRunner) { let branch = 1; for x in xs { runner.current_branch(); } }",
            "struct Store;",
            "impl Store { fn open() { let s = Self::new(); s.current_branch(); let t = Self {}; t.current_branch(); } }",
        ]
        .join("\n");
        let got = rust_receivers(&source, "current_branch");
        let want = [
            (1, "GitRunner"),
            (2, "GitRunner"),
            (3, "GitRunner"),
            (4, "Box"),
            (5, "Runner"),
            (6, "GitRunner"),
            (7, "GitRunner"),
            (8, "GitRunner"),
            (9, "GitRunner"),
            (10, "GitRunner"),
            (12, "Store"),
            (12, "Store"),
        ]
        .map(|(line, ty)| (line, Some(format!(".{ty}"))));
        assert_eq!(got, want);
    }

    /// Where the code does not state the local's type, the receiver stays the
    /// expression as written: an untyped or non-constructor `let`, a pattern
    /// that rebinds the name (`for`, `match`, `if let`, a closure parameter,
    /// a destructuring `let`), a `let` in a block that closed before the
    /// call, a binding after the call, a constructor other than `new` or
    /// `default`, and a type that is not a path.
    #[test]
    fn rust_receiver_keeps_the_expression_without_a_stated_type() {
        let source = [
            "fn untyped(runner: &GitRunner) { let runner = open(); runner.current_branch(); }",
            "fn fallible(root: &Path) { let runner = GitRunner::open(root)?; runner.current_branch(); }",
            "fn looped(runner: &GitRunner) { for runner in all() { runner.current_branch(); } }",
            "fn matched(runner: &GitRunner) { match x { Some(runner) => runner.current_branch(), _ => {} } }",
            "fn if_let(runner: &GitRunner) { if let Some(runner) = x { runner.current_branch(); } }",
            "fn closure_param(runner: &GitRunner) { let f = |runner| runner.current_branch(); }",
            "fn destructured(runner: &GitRunner) { let (runner, _) = pair(); runner.current_branch(); }",
            "fn closed_block() { { let runner = GitRunner::new(r); } runner.current_branch(); }",
            "fn later() { runner.current_branch(); let runner = GitRunner::new(r); }",
            "fn opaque(runner: impl Branch) { runner.current_branch(); }",
            "fn lower(runner: &GitRunner) { GitRunner::new(r).current_branch(); git::new(r).current_branch(); }",
            "fn opener(r: &Path) { GitRunner::open(r).current_branch(); }",
            "fn wrapped(r: &Path) { let Wrapper(runner) = Wrapper::new(r); runner.current_branch(); }",
            "fn wrapped_param(Wrapper(runner): Wrapper) { runner.current_branch(); }",
        ]
        .join("\n");
        let got = rust_receivers(&source, "current_branch");
        let want = [
            (1, "runner"),
            (2, "runner"),
            (3, "runner"),
            (4, "runner"),
            (5, "runner"),
            (6, "runner"),
            (7, "runner"),
            (8, "runner"),
            (9, "runner"),
            (10, "runner"),
            (11, "GitRunner"),
            (11, "git::new(r)"),
            (12, "GitRunner::open(r)"),
            (13, "runner"),
            (14, "runner"),
        ]
        .map(|(line, ty)| (line, Some(format!(".{ty}"))));
        assert_eq!(got, want);
    }

    #[test]
    fn rust_trait_impl_methods_are_marked() {
        let source = br#"
struct X;
impl std::fmt::Display for X {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { Ok(()) }
}
impl X {
    fn own(&self) {
        struct Y;
        impl Default for Y { fn default() -> Self { Y } }
    }
    fn after(&self) {}
}
trait Greet { fn hello(&self) {} }
fn free() {}
"#;
        let extraction = extract_file("src/lib.rs", source).unwrap();
        let marked: Vec<(&str, bool)> = extraction
            .symbols
            .iter()
            .filter(|s| matches!(s.kind, SymbolKind::Method | SymbolKind::Function))
            .map(|s| (s.name.as_str(), s.trait_impl))
            .collect();
        assert_eq!(
            marked,
            [
                ("fmt", true),
                ("own", false),
                ("default", true),
                ("after", false),
                ("hello", false),
                ("free", false),
            ]
        );
    }

    #[test]
    fn ts_literal_args_not_extracted_as_references() {
        // `foo(undefined, null, true, false, "x", 42)` — none of the literal
        // args should become references.
        let source = b"export function f() { foo(undefined, null, true, false, \"x\", 42); }\n";
        let extraction = extract_file("src/f.ts", source).unwrap();
        assert!(
            extraction.references.is_empty(),
            "literal args must not be references: {:?}",
            extraction.references
        );
    }

    #[test]
    fn rust_use_binds_the_item_names_it_brings_into_scope() {
        type Row<'a> = (&'a str, &'a str, Vec<(&'a str, &'a str)>);
        let source = [
            "use crate::push::push;",
            "use crate::push::{PushOptions, push as leased};",
            "use crate::a::{b::{c, d}, e, self};",
            "use std::collections::*;",
            "use super::Walker;",
        ]
        .join("\n");
        let extraction = extract_file("src/ship.rs", source.as_bytes()).unwrap();
        // One import per path, keeping the statement's spec for `rename`:
        // (spec, path, [(local, source)]). The resolver matches calls on the
        // local name, so an aliased item is in scope as its alias only.
        let imports: Vec<Row> = extraction
            .imports
            .iter()
            .map(|i| {
                (
                    i.spec.as_str(),
                    i.path.as_str(),
                    i.bindings
                        .iter()
                        .map(|b| (b.local.as_str(), b.source.as_str()))
                        .collect(),
                )
            })
            .collect();
        let grouped = "crate::push::{PushOptions, push as leased}";
        let nested = "crate::a::{b::{c, d}, e, self}";
        assert_eq!(
            imports,
            [
                (
                    "crate::push::push",
                    "crate::push::push",
                    vec![("push", "push")]
                ),
                (
                    grouped,
                    "crate::push::PushOptions",
                    vec![("PushOptions", "PushOptions")]
                ),
                (grouped, "crate::push::push", vec![("leased", "push")]),
                (nested, "crate::a::b::c", vec![("c", "c")]),
                (nested, "crate::a::b::d", vec![("d", "d")]),
                (nested, "crate::a::e", vec![("e", "e")]),
                (nested, "crate::a::self", vec![]),
                ("std::collections::*", "std::collections::*", vec![]),
                ("super::Walker", "super::Walker", vec![("Walker", "Walker")]),
            ]
        );
    }

    /// Where a Rust `use` binds names: its enclosing module or block, minus
    /// the inline modules nested in it — except a module that glob-imports
    /// its parent (`use super::*;`), and then only for a module's names,
    /// since `super` never names a block.
    #[test]
    fn rust_use_scope_is_its_module_or_block_minus_the_modules_that_do_not_see_it() {
        let source = [
            "use crate::left::push;", // 1: file level
            "mod a {",                // 2
            "    use crate::right::publish;",
            "    pub fn f() { publish(); }",
            "}",                          // 5
            "mod tests {",                // 6
            "    use super::*;",          // 7: sees the file's names
            "    mod deep { fn g() {} }", // 8: its parent is tests, no glob
            "}",                          // 9
            "fn h() {",                   // 10
            "    use crate::inner::x;",   // 11: block level
            "    x();",
            "    mod m { use super::*; }", // 13: super is the file, not h's block
            "}",                           // 14
        ]
        .join("\n");
        let extraction = extract_file("src/ship.rs", source.as_bytes()).unwrap();
        let scopes: Vec<(&str, &[(u32, u32)])> = extraction
            .imports
            .iter()
            .map(|i| (i.path.as_str(), i.scope.as_slice()))
            .collect();
        assert_eq!(
            scopes,
            [
                ("crate::left::push", &[(1, 1), (6, 7), (9, 14)][..]),
                ("crate::right::publish", &[(2, 5)][..]),
                ("super::*", &[(6, 7), (9, 9)][..]),
                ("crate::inner::x", &[(10, 12), (14, 14)][..]),
                ("super::*", &[(13, 13)][..]),
            ]
        );
    }

    /// A file-level `use` with no module to exclude is in scope everywhere,
    /// which the column stores as nothing. An external `mod foo;` holds no
    /// code here and excludes nothing.
    #[test]
    fn rust_file_level_use_without_hidden_modules_is_in_scope_everywhere() {
        let source = b"use crate::left::push;\nmod other;\npub fn f() { push(); }\n";
        let extraction = extract_file("src/ship.rs", source).unwrap();
        assert!(extraction.imports[0].scope.is_empty());
    }

    #[test]
    fn subtract_line_ranges_keeps_the_lines_between_the_holes() {
        assert_eq!(subtract_line_ranges((1, 10), &[]), [(1, 10)]);
        assert_eq!(subtract_line_ranges((1, 5), &[(2, 2)]), [(1, 1), (3, 5)]);
        assert_eq!(subtract_line_ranges((1, 10), &[(1, 3), (8, 10)]), [(4, 7)]);
        assert_eq!(subtract_line_ranges((1, 5), &[(1, 4)]), [(5, 5)]);
        assert!(subtract_line_ranges((1, 5), &[(1, 5)]).is_empty());
    }

    /// A path broken across lines names the same module as on one line.
    #[test]
    fn rust_use_path_ignores_the_whitespace_inside_it() {
        let extraction =
            extract_file("src/ship.rs", b"use crate::{\n    left ::push,\n};\n").unwrap();
        assert_eq!(extraction.imports[0].path, "crate::left::push");
    }

    #[test]
    fn rust_use_alias_of_a_path_binds_the_alias_to_the_last_segment() {
        let extraction =
            extract_file("src/ship.rs", b"use crate::push::push as leased;\n").unwrap();
        assert_eq!(
            extraction.imports[0].bindings,
            [ImportBinding::aliased("push", "leased")]
        );
    }

    /// A default import binds its local name beside the named ones.
    #[test]
    fn ts_default_import_binds_its_name_beside_the_named_ones() {
        let extraction =
            extract_file("src/ship.ts", b"import greet, { helper } from \"./a\";\n").unwrap();
        assert_eq!(
            extraction.imports[0].bindings,
            [
                ImportBinding::named("greet"),
                ImportBinding::named("helper")
            ]
        );
    }

    #[test]
    fn ts_import_alias_binds_the_alias_to_the_exported_name() {
        let source = b"import { push as leased, publish } from \"./push\";\nexport { open } from \"./store\";\n";
        let extraction = extract_file("src/ship.ts", source).unwrap();
        let bindings: Vec<&[ImportBinding]> = extraction
            .imports
            .iter()
            .map(|i| i.bindings.as_slice())
            .collect();
        assert_eq!(
            bindings,
            [
                &[
                    ImportBinding::aliased("push", "leased"),
                    ImportBinding::named("publish")
                ][..],
                &[ImportBinding::named("open")][..],
            ]
        );
    }
}

#[cfg(test)]
mod contract_tests;
