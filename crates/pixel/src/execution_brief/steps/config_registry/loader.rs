// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! A Python configuration loader a question about environment files or
//! overrides asks for: a method that reads an environment variable named by
//! one of its parameters, forwards the result to a same-class helper, and
//! reaches a write to the object's mapping. The script followed it with
//! Python's `ast` and `tokenize`; the step follows the same shape on the
//! tree-sitter tree `pixel-graph` already parses Python with.
//!
//! The chain, its constructor and its conditions are quoted as source with
//! every literal, number, comment and docstring masked or left out: nothing
//! says which branch runs, what a file holds or what a value ends up as.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::LazyLock;

use regex::Regex;
use tree_sitter::Node;

use super::super::text::{splitlines, universal_newlines};
use super::repo::{MAX_FILE_BYTES, Repo};
use super::scan::SCAN_MAX_COLUMNS;

const LOADER_MAX_FILES: usize = 256;
const LOADER_MAX_LINES: usize = 80;
const LOADER_MAX_HELPERS: usize = 3;
const LOADER_MAX_CANDIDATES: usize = 2;
const LOADER_MAX_CHARS: usize = 6000;
const HEADER: &str = "### configuration loader — dynamic environment-name lookup, same-class helper candidates and mapping writes. Conditions and parameters are source evidence; runtime dispatch, active mode, file contents and effective values unknown. Literal values masked.";

static INTENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(env\w*|overrid\w*|load\w*|precedence)\b")
        .expect("the loader intent is a literal")
});
static TEST_DIR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(^|/)(tests?|fixtures?|vendor)/").expect("the test directory is a literal")
});
static RANGE_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-(\d+) —").expect("the range pattern is a literal"));

fn text<'s>(node: Node, src: &'s str) -> &'s str {
    &src[node.byte_range()]
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn strip_parens(mut node: Node<'_>) -> Node<'_> {
    while node.kind() == "parenthesized_expression" {
        match children(node).into_iter().find(|child| child.kind() != "comment") {
            Some(inner) => node = inner,
            None => break,
        }
    }
    node
}

/// The children of `node` that Python's AST has as its own children: lists
/// of statements, argument lists and parentheses are not nodes there.
fn ast_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    for child in children(node) {
        match child.kind() {
            "comment" | "string_start" | "string_end" | "string_content" | "escape_sequence"
            | "escape_interpolation" => {}
            "block" | "argument_list" | "parenthesized_expression" | "else_clause"
            | "finally_clause" | "with_clause" | "pair" => out.extend(ast_children(child)),
            _ => out.push(child),
        }
    }
    out
}

/// `ast.walk` over each statement of a function body in turn, or nothing when
/// the body defines a function, class or lambda.
fn body_nodes<'t>(function: Node<'t>) -> Vec<Node<'t>> {
    let Some(body) = function.child_by_field_name("body") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for statement in ast_children(body) {
        let mut queue = VecDeque::from([statement]);
        while let Some(node) = queue.pop_front() {
            out.push(node);
            queue.extend(ast_children(node));
        }
    }
    if out
        .iter()
        .any(|node| matches!(node.kind(), "function_definition" | "class_definition" | "lambda"))
    {
        return Vec::new();
    }
    out
}

fn dotted(node: Node<'_>, src: &str) -> Option<String> {
    let node = strip_parens(node);
    match node.kind() {
        "identifier" => Some(text(node, src).to_owned()),
        "attribute" => {
            let object = dotted(node.child_by_field_name("object")?, src)?;
            Some(format!("{object}.{}", text(node.child_by_field_name("attribute")?, src)))
        }
        _ => None,
    }
}

/// The positional arguments of a call: everything before the keywords.
fn positional<'t>(call: Node<'t>) -> Vec<Node<'t>> {
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return Vec::new();
    };
    if arguments.kind() != "argument_list" {
        return vec![arguments];
    }
    children(arguments)
        .into_iter()
        .filter(|arg| !matches!(arg.kind(), "comment" | "keyword_argument" | "dictionary_splat"))
        .collect()
}

/// The identifier a node is, parentheses aside.
fn name_of<'s>(node: Node<'_>, src: &'s str) -> Option<&'s str> {
    let node = strip_parens(node);
    (node.kind() == "identifier").then(|| text(node, src))
}

/// The names of the positional-or-keyword parameters of a function.
fn positional_params(function: Node<'_>, src: &str) -> Vec<String> {
    let Some(parameters) = function.child_by_field_name("parameters") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for param in children(parameters) {
        match param.kind() {
            "comment" => {}
            // Everything before `/` is positional-only.
            "positional_separator" => out.clear(),
            "keyword_separator" | "list_splat_pattern" | "dictionary_splat_pattern" => break,
            "identifier" => out.push(text(param, src).to_owned()),
            "typed_parameter" => match children(param).first() {
                Some(first) if first.kind() == "identifier" => out.push(text(*first, src).to_owned()),
                _ => break,
            },
            "default_parameter" | "typed_default_parameter" => {
                if let Some(name) = param.child_by_field_name("name") {
                    out.push(text(name, src).to_owned());
                }
            }
            _ => {}
        }
    }
    out
}

/// Whether a subscript is assigned to (or iterated into): `self[key] = value`.
fn is_store(node: Node<'_>) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
                return parent.child_by_field_name("left").is_some_and(|left| left.id() == current.id());
            }
            "pattern_list" | "tuple_pattern" | "list_pattern" | "list_splat_pattern"
            | "expression_list" | "tuple" | "list" | "parenthesized_expression" => current = parent,
            _ => return false,
        }
    }
    false
}

/// A bare string statement that is a `str` constant: a docstring.
fn is_string_statement(statement: Node<'_>, src: &str) -> bool {
    let inner: Vec<Node> = children(statement)
        .into_iter()
        .filter(|child| child.kind() != "comment")
        .collect();
    let [only] = inner[..] else {
        return false;
    };
    let plain = |string: Node| {
        children(string)
            .iter()
            .find(|part| part.kind() == "string_start")
            .is_some_and(|start| !text(*start, src).chars().any(|ch| matches!(ch, 'f' | 'F' | 'b' | 'B' | 't' | 'T')))
    };
    match only.kind() {
        "string" => plain(only),
        "concatenated_string" => children(only).into_iter().all(plain),
        _ => false,
    }
}

/// The text of a file with the places Python's tokenizer finds.
struct Source<'s> {
    src: &'s str,
    /// `str.splitlines()`.
    lines: Vec<&'s str>,
    /// Byte offset where each `\n` row starts.
    row_starts: Vec<usize>,
    /// `(start line, start col, end line, end col, is a comment)`, columns in characters.
    tokens: Vec<(usize, usize, usize, usize, bool)>,
}

impl<'s> Source<'s> {
    fn new(src: &'s str, root: Node<'_>) -> Self {
        let mut row_starts = vec![0];
        row_starts.extend(src.match_indices('\n').map(|(at, _)| at + 1));
        let mut source = Self { src, lines: splitlines(src), row_starts, tokens: Vec::new() };
        source.visit(root);
        source
    }

    /// `(line, column in characters)` of a byte offset.
    fn position(&self, byte: usize) -> (usize, usize) {
        let row = self.row_starts.partition_point(|start| *start <= byte) - 1;
        (row + 1, self.src[self.row_starts[row]..byte].chars().count())
    }

    fn token(&mut self, start: usize, end: usize, comment: bool) {
        if start < end {
            let ((a, b), (c, d)) = (self.position(start), self.position(end));
            self.tokens.push((a, b, c, d, comment));
        }
    }

    /// Comments, numbers, strings, and the text between an f-string's braces.
    fn visit(&mut self, node: Node<'_>) {
        match node.kind() {
            "comment" => self.token(node.start_byte(), node.end_byte(), true),
            "integer" | "float" => self.token(node.start_byte(), node.end_byte(), false),
            "string" => {
                let fstring = children(node)
                    .iter()
                    .find(|part| part.kind() == "string_start")
                    .is_some_and(|start| text(*start, self.src).chars().any(|ch| matches!(ch, 'f' | 'F')));
                if !fstring {
                    self.token(node.start_byte(), node.end_byte(), false);
                    return;
                }
                for part in children(node) {
                    match part.kind() {
                        "string_content" => self.token(part.start_byte(), part.end_byte(), false),
                        "interpolation" => self.interpolation(part),
                        _ => {}
                    }
                }
            }
            _ => {
                for child in children(node) {
                    self.visit(child);
                }
            }
        }
    }

    fn interpolation(&mut self, node: Node<'_>) {
        for child in children(node) {
            if child.kind() != "format_specifier" {
                self.visit(child);
                continue;
            }
            // The literal text of a format spec, around nested replacement fields.
            let mut at = child.start_byte() + usize::from(text(child, self.src).starts_with(':'));
            for nested in children(child) {
                self.token(at, nested.start_byte(), false);
                self.interpolation(nested);
                at = nested.end_byte();
            }
            self.token(at, child.end_byte(), false);
        }
    }
}

/// The 1-based first and last line of a node (a function ends at its last statement).
fn line_span(node: Node<'_>) -> (usize, usize) {
    let last = node
        .child_by_field_name("body")
        .and_then(|body| children(body).into_iter().rfind(|child| child.kind() != "comment"))
        .map_or(node.end_position().row, |child| child.end_position().row);
    (node.start_position().row + 1, last + 1)
}

/// `(line, text)` of the lines of a method that carry evidence: docstrings
/// omitted, literals and numbers masked, comments cut.
fn source_rows(file: &Source<'_>, method: Node<'_>) -> Vec<(usize, String)> {
    let (first, last) = line_span(method);
    let mut omitted: HashSet<usize> = HashSet::new();
    for node in body_nodes(method) {
        if node.kind() == "expression_statement" && is_string_statement(node, file.src) {
            omitted.extend(node.start_position().row + 1..=node.end_position().row + 1);
        }
    }
    let mut replacements: BTreeMap<usize, Vec<(usize, usize, &str)>> = BTreeMap::new();
    for (start_line, start_col, end_line, end_col, comment) in &file.tokens {
        if *start_line < first || *start_line > last || omitted.contains(start_line) {
            continue;
        }
        for line in *start_line..=*end_line {
            let low = if line == *start_line { *start_col } else { 0 };
            let high = if line == *end_line {
                *end_col
            } else {
                file.lines.get(line - 1).map_or(0, |text| text.chars().count())
            };
            replacements
                .entry(line)
                .or_default()
                .push((low, high, if *comment { "" } else { "<masked>" }));
        }
    }
    let mut rows = Vec::new();
    for number in first..=last {
        if omitted.contains(&number) {
            continue;
        }
        let Some(line) = file.lines.get(number - 1) else {
            continue;
        };
        let mut chars: Vec<char> = line.chars().collect();
        let mut marks = replacements.remove(&number).unwrap_or_default();
        marks.sort_by(|a, b| b.cmp(a));
        for (low, high, value) in marks {
            let head: String = chars.iter().take(low).collect();
            let tail: String = chars.iter().skip(high).collect();
            chars = format!("{head}{value}{tail}").chars().collect();
        }
        let row: String = chars.into_iter().collect();
        if !row.trim().is_empty() {
            rows.push((number, row));
        }
    }
    rows
}

/// A method of a class: its name and tree node.
type Method<'t> = (String, Node<'t>);

/// The undecorated, synchronous methods of a class, by name; a later method
/// of the same name replaces an earlier one in place.
fn methods_of<'t>(class: Node<'t>, src: &str) -> Vec<Method<'t>> {
    let mut methods: Vec<Method> = Vec::new();
    let Some(body) = class.child_by_field_name("body") else {
        return methods;
    };
    for member in children(body) {
        let plain = member.kind() == "function_definition"
            && member.child(0).is_none_or(|first| first.kind() != "async");
        let Some(name) = member.child_by_field_name("name").filter(|_| plain) else {
            continue;
        };
        let name = text(name, src).to_owned();
        match methods.iter_mut().find(|(existing, _)| *existing == name) {
            Some(slot) => slot.1 = member,
            None => methods.push((name, member)),
        }
    }
    methods
}

fn imports_os(root: Node<'_>, src: &str) -> bool {
    children(root).into_iter().any(|statement| {
        statement.kind() == "import_statement" && {
            let mut cursor = statement.walk();
            statement
                .children_by_field_name("name", &mut cursor)
                .any(|name| name.kind() == "dotted_name" && text(name, src) == "os")
        }
    })
}

/// The method of the class that a call on the receiver names, and the first
/// positional argument of the call.
fn on_receiver<'t>(
    call: Node<'t>,
    src: &str,
    receiver: &str,
    methods: &[Method<'_>],
) -> Option<(usize, Option<Node<'t>>)> {
    let func = strip_parens(call.child_by_field_name("function")?);
    if func.kind() != "attribute" || name_of(func.child_by_field_name("object")?, src)? != receiver {
        return None;
    }
    let attr = text(func.child_by_field_name("attribute")?, src);
    let target = methods.iter().position(|(existing, _)| existing == attr)?;
    Some((target, positional(call).first().copied()))
}

/// The loader candidates of one tracked Python file.
fn file_candidates(repo: &Repo, rel: &str) -> Vec<String> {
    let full = repo.root.join(rel);
    let Ok(meta) = std::fs::symlink_metadata(&full) else {
        return Vec::new();
    };
    let base = rel.rsplit('/').next().unwrap_or(rel);
    if meta.file_type().is_symlink() || base.starts_with(".env") || !full.is_file() {
        return Vec::new();
    }
    if meta.len() > MAX_FILE_BYTES || TEST_DIR.is_match(rel) {
        return Vec::new();
    }
    let Ok(raw) = std::fs::read(&full) else {
        return Vec::new();
    };
    let Ok(decoded) = String::from_utf8(raw) else {
        return Vec::new();
    };
    let source = universal_newlines(&decoded);
    if !source.contains("os.environ") && !source.contains("os.getenv") {
        return Vec::new();
    }
    let Some(tree) = pixel_graph::extract::parse_file(rel, source.as_bytes()) else {
        return Vec::new();
    };
    let root = tree.root_node();
    if root.has_error() || !imports_os(root, &source) {
        return Vec::new();
    }
    let file = Source::new(&source, root);
    let mut out = Vec::new();
    for top in children(root) {
        let class = match top.kind() {
            "class_definition" => Some(top),
            "decorated_definition" => top.child_by_field_name("definition").filter(|d| d.kind() == "class_definition"),
            _ => None,
        };
        let Some(class) = class else {
            continue;
        };
        let class_name = class.child_by_field_name("name").map_or("", |name| text(name, &source));
        let methods = methods_of(class, &source);
        for index in 0..methods.len() {
            if let Some(candidate) = method_candidate(&file, rel, class_name, &methods, index) {
                out.push(candidate);
            }
        }
    }
    out
}

/// The loader a method starts, if it has the shape.
fn method_candidate(
    file: &Source<'_>,
    rel: &str,
    class_name: &str,
    methods: &[Method<'_>],
    index: usize,
) -> Option<String> {
    let src = file.src;
    let (name, function) = &methods[index];
    let params = positional_params(*function, src);
    let receiver = params.first()?.clone();
    let nodes = body_nodes(*function);
    let calls: Vec<Node> = nodes.iter().copied().filter(|node| node.kind() == "call").collect();
    let is_read = |call: &Node| {
        let func = call.child_by_field_name("function");
        let first = positional(*call).first().copied();
        func.and_then(|func| dotted(func, src))
            .is_some_and(|path| path == "os.environ.get" || path == "os.getenv")
            && first.and_then(|arg| name_of(arg, src)).is_some_and(|arg| params.iter().any(|p| p == arg))
    };
    let reads: HashSet<usize> = calls.iter().filter(|call| is_read(call)).map(tree_sitter::Node::id).collect();
    let names: HashSet<&str> = nodes
        .iter()
        .filter(|node| node.kind() == "assignment" && node.child_by_field_name("type").is_none())
        .filter(|node| node.parent().is_none_or(|parent| parent.kind() != "assignment"))
        .filter_map(|node| {
            let left = node.child_by_field_name("left").filter(|left| left.kind() == "identifier")?;
            let right = strip_parens(node.child_by_field_name("right")?);
            reads.contains(&right.id()).then(|| text(left, src))
        })
        .collect();
    let forwards: Vec<usize> = calls
        .iter()
        .filter_map(|call| on_receiver(*call, src, &receiver, methods))
        .filter(|(_, first)| first.and_then(|arg| name_of(arg, src)).is_some_and(|arg| names.contains(arg)))
        .map(|(target, _)| target)
        .collect();
    if forwards.is_empty() {
        return None;
    }
    let mut chain = vec![index];
    let mut pending: VecDeque<usize> = forwards.into();
    while !pending.is_empty() && chain.len() < LOADER_MAX_HELPERS {
        let helper = pending.pop_front()?;
        if chain.contains(&helper) {
            continue;
        }
        chain.push(helper);
        for call in body_nodes(methods[helper].1).into_iter().filter(|node| node.kind() == "call") {
            if let Some((target, _)) = on_receiver(call, src, &receiver, methods)
                && !chain.contains(&target)
            {
                pending.push_back(target);
            }
        }
    }
    let writes = chain.iter().any(|member| {
        body_nodes(methods[*member].1).iter().any(|node| {
            node.kind() == "subscript"
                && node
                    .child_by_field_name("value")
                    .and_then(|value| name_of(value, src))
                    .is_some_and(|value| value == receiver)
                && is_store(*node)
        })
    });
    if !writes {
        return None;
    }
    let init = methods.iter().position(|(existing, _)| existing == "__init__");
    let selected: Vec<usize> = init
        .filter(|init| !chain.contains(init))
        .into_iter()
        .chain(chain.iter().copied())
        .collect();
    let rows: Vec<(usize, String)> = selected
        .iter()
        .flat_map(|member| source_rows(file, methods[*member].1))
        .collect();
    if rows.len() > LOADER_MAX_LINES || rows.iter().any(|(_, row)| row.len() > SCAN_MAX_COLUMNS) {
        return None;
    }
    let mut out = vec![HEADER.to_owned()];
    if !pending.is_empty() {
        out.push("Additional helper candidates omitted; chain is incomplete.".to_owned());
    }
    let mut previous = None;
    for (number, row) in &rows {
        if previous.is_none_or(|before: usize| *number != before + 1) {
            out.push(format!("### {rel}:{number}-{number} — loader evidence"));
        } else if let Some(header) = out.iter().rposition(|line| line.starts_with("### ")) {
            out[header] = RANGE_END.replace_all(&out[header], regex::NoExpand(&format!("-{number} —"))).into_owned();
        }
        out.push(format!("{number}| {row}"));
        previous = Some(*number);
    }
    let (line, _) = line_span(*function);
    out[0].push_str(&format!(" Candidate {class_name}.{name} @ {rel}:{line}."));
    Some(out.join("\n") + "\n")
}

/// The loader evidence of the tracked Python files, or nothing.
pub(super) fn evidence(repo: &Repo, prompt: &str) -> String {
    if !INTENT.is_match(prompt) {
        return String::new();
    }
    let mut paths: Vec<&str> = repo
        .tracked()
        .iter()
        .map(String::as_str)
        .filter(|path| path.ends_with(".py"))
        .collect();
    paths.sort_unstable();
    let mut candidates: Vec<String> = Vec::new();
    for rel in paths.into_iter().take(LOADER_MAX_FILES) {
        if repo.timed_out() {
            break;
        }
        candidates.extend(file_candidates(repo, rel));
    }
    if candidates.is_empty() {
        return String::new();
    }
    let mut selected: Vec<&String> = Vec::new();
    let mut used = 0;
    for candidate in candidates.iter().take(LOADER_MAX_CANDIDATES) {
        let size = candidate.chars().count();
        if used + size <= LOADER_MAX_CHARS {
            used += size;
            selected.push(candidate);
        }
    }
    let mut result: String = selected.iter().map(|candidate| candidate.as_str()).collect();
    if !result.is_empty() && candidates.len() > selected.len() {
        result.push_str(&format!(
            "Additional loader candidates omitted: {}; no unique loader established.\n",
            candidates.len() - selected.len()
        ));
    }
    result
}
