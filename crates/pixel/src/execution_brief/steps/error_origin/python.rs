// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The Python syntax `error-origin` reads: what kind of statement holds a
//! line, and the `except` clauses that convert or rethrow a caught type. The
//! script used `ast`; the step reads the same facts from tree-sitter's Python
//! grammar, and a file that does not parse cleanly (or is Python 2) is, as for
//! `ast.parse`, not read at all.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use tree_sitter::{Node, Parser, Tree};

use super::super::walk::Walk;
use super::super::text::subwords;
use super::{MIN_SHARED, is_test_path, stem};

/// The role of a line the script gives when no syntax says better.
pub(super) const UNVERIFIED: &str = "occurrence candidate; emission unverified";

static EXCEPT_OR_RAISE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(except|raise)\b").expect("the pattern is a literal"));

/// A parsed file: `ast.parse('\n'.join(lines))`.
struct Parsed {
    tree: Tree,
    text: String,
}

fn parse(lines: &[String]) -> Option<Parsed> {
    let text = lines.join("\n");
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(&text, None)?;
    let root = tree.root_node();
    if root.has_error() || any_node(root, &mut |node| is_python_two(node.kind())) {
        return None;
    }
    Some(Parsed { tree, text })
}

/// The Python 2 statements the grammar still reads and `ast.parse` refuses.
fn is_python_two(kind: &str) -> bool {
    matches!(kind, "print_statement" | "exec_statement")
}

/// Whether any node under `root` is `wanted`.
fn any_node<'t>(root: Node<'t>, wanted: &mut dyn FnMut(Node<'t>) -> bool) -> bool {
    let mut cursor = root.walk();
    loop {
        if wanted(cursor.node()) {
            return true;
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return false;
            }
        }
    }
}

/// Visits, in source order, the nodes whose rows hold `row` (0-based) and
/// stops when `visit` says so.
fn visit_holding(root: Node, row: usize, visit: &mut dyn FnMut(Node) -> bool) {
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        let holds = node.start_position().row <= row && row <= node.end_position().row;
        if holds && visit(node) {
            return;
        }
        if holds && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return;
            }
        }
    }
}

/// Whether the node is a constant of Python's `ast`: `Some(true)` for a text
/// one, `Some(false)` for bytes, a number, `None`, a boolean or `...`.
fn constant(node: Node, text: &str) -> Option<bool> {
    match node.kind() {
        "parenthesized_expression" => {
            let mut cursor = node.walk();
            let mut inner = node.named_children(&mut cursor);
            let only = inner.next()?;
            inner.next().is_none().then_some(())?;
            constant(only, text)
        }
        "string" => {
            let start = node.child(0)?;
            let prefix = text.get(start.byte_range())?.to_lowercase();
            let prefix = prefix.trim_end_matches(['"', '\'']);
            if prefix.contains(['f', 't']) {
                return None;
            }
            Some(!prefix.contains('b'))
        }
        "concatenated_string" => {
            let mut all_text = true;
            let mut cursor = node.walk();
            for part in node.named_children(&mut cursor) {
                all_text &= constant(part, text)?;
            }
            Some(all_text)
        }
        "integer" | "float" | "true" | "false" | "none" | "ellipsis" => Some(false),
        _ => None,
    }
}

/// An assignment whose value is a constant (`x = "a"`, `x: str = "a"`, `a = b = 1`).
fn assigns_constant(node: Node, text: &str) -> bool {
    let mut value = node.child_by_field_name("right");
    while let Some(inner) = value.filter(|inner| inner.kind() == "assignment") {
        value = inner.child_by_field_name("right");
    }
    value.is_some_and(|value| constant(value, text).is_some())
}

/// `(role, function name, 0-based line of its def)` of `line` (1-based): a
/// raise, a message definition, a docstring or a bare candidate.
pub(super) fn role(lines: &[String], line: usize) -> (String, Option<String>, usize) {
    let bare = (UNVERIFIED.to_owned(), None, 0);
    let Some(parsed) = parse(lines) else {
        return bare;
    };
    let row = line.saturating_sub(1);
    let root = parsed.tree.root_node();
    let mut scope: Option<(usize, String)> = None;
    let mut found: Option<&str> = None;
    visit_holding(root, row, &mut |node| {
        match node.kind() {
            "function_definition" => {
                let start = node.start_position().row;
                if scope.as_ref().is_none_or(|(best, _)| start > *best)
                    && let Some(name) = node.child_by_field_name("name")
                {
                    scope = Some((start, parsed.text[name.byte_range()].to_owned()));
                }
            }
            "raise_statement" if found.is_none() => {
                found = Some("local raise syntax; message origin candidate");
            }
            "expression_statement" if found.is_none() => {
                if let Some(inner) = node.named_child(0) {
                    if inner.kind() == "assignment" {
                        if assigns_constant(inner, &parsed.text) {
                            found = Some("message definition; emission unverified");
                        }
                    } else if constant(inner, &parsed.text) == Some(true) {
                        found = Some("documentation occurrence; no emission proved");
                    }
                }
            }
            _ => {}
        }
        false
    });
    (
        found.unwrap_or(UNVERIFIED).to_owned(),
        scope.as_ref().map(|(_, name)| name.clone()),
        scope.map_or(0, |(start, _)| start),
    )
}

/// The `Name` nodes of an expression: its identifiers, but not an attribute's
/// own name or a keyword argument's.
fn names_in(root: Node, text: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "identifier" => {
                names.insert(text[node.byte_range()].to_owned());
            }
            "attribute" => stack.extend(node.child_by_field_name("object")),
            "keyword_argument" => stack.extend(node.child_by_field_name("value")),
            _ => {
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
            }
        }
    }
    names
}

/// The raise statements of one handler, not those of the functions and classes nested in it.
fn raises_of(clause: Node) -> Vec<Node> {
    let mut found = Vec::new();
    let mut stack: Vec<Node> = Vec::new();
    let mut cursor = clause.walk();
    stack.extend(clause.named_children(&mut cursor).collect::<Vec<_>>().into_iter().rev());
    while let Some(node) = stack.pop() {
        match node.kind() {
            "function_definition" | "class_definition" | "decorated_definition" | "lambda" => {}
            "raise_statement" => found.push(node),
            _ => {
                let mut cursor = node.walk();
                let children: Vec<Node> = node.named_children(&mut cursor).collect();
                stack.extend(children.into_iter().rev());
            }
        }
    }
    found
}

/// The `except` clause the question's words name, and what to quote of it.
pub(super) struct Handler {
    pub(super) path: String,
    /// 1-based lines of the clause to show.
    pub(super) first: usize,
    pub(super) last: usize,
    /// 1-based lines of the module-level imports of a caught type.
    pub(super) imports: Vec<(usize, usize)>,
    /// 1-based lines of the raise statements of the clause, at most six.
    pub(super) raises: Vec<(usize, usize)>,
}

/// The typed handler that re-raises and whose caught type shares the most of
/// the question's words (`meaningful`, two at least).
pub(super) fn handlers(walk: &mut Walk, meaningful: &HashSet<String>) -> Option<Option<Handler>> {
    let mut best: Option<((usize, String, usize), Handler)> = None;
    let files = walk.files_matching(&[".py"], 80, &|row| EXCEPT_OR_RAISE.is_match(row))?;
    for path in files {
        if is_test_path(&path) {
            continue;
        }
        let lines = walk.lines(&path);
        let Some(parsed) = parse(&lines) else {
            continue;
        };
        let root = parsed.tree.root_node();
        let mut clauses: Vec<Node> = Vec::new();
        any_node(root, &mut |node| {
            if node.kind() == "except_clause" {
                clauses.push(node);
            }
            false
        });
        for clause in clauses {
            let Some(value) = clause.child_by_field_name("value") else {
                continue;
            };
            let kind = if value.kind() == "as_pattern" {
                value.named_child(0)
            } else {
                Some(value)
            };
            let Some(kind) = kind else {
                continue;
            };
            let words: HashSet<String> = subwords(&parsed.text[kind.byte_range()])
                .iter()
                .map(|token| stem(token))
                .collect();
            let shared = meaningful.intersection(&words).count();
            if shared < MIN_SHARED {
                continue;
            }
            let raises = raises_of(clause);
            if raises.is_empty() {
                continue;
            }
            let first = clause.start_position().row + 1;
            let key = (usize::MAX - shared, path.clone(), first);
            if best.as_ref().is_some_and(|(held, _)| *held <= key) {
                continue;
            }
            let owner = clause
                .parent()
                .map_or(first, |parent| parent.start_position().row + 1);
            let lo = first.saturating_sub(20).max(owner);
            let types = names_in(kind, &parsed.text);
            let mut imports = Vec::new();
            let mut cursor = root.walk();
            for statement in root.named_children(&mut cursor) {
                if statement.kind() != "import_from_statement" {
                    continue;
                }
                let mut names = statement.walk();
                let imported = statement.children_by_field_name("name", &mut names).any(|item| {
                    let bound = if item.kind() == "aliased_import" {
                        item.child_by_field_name("alias")
                            .or_else(|| item.child_by_field_name("name"))
                    } else {
                        Some(item)
                    };
                    bound.is_some_and(|bound| types.contains(&parsed.text[bound.byte_range()]))
                });
                if imported {
                    imports.push((
                        statement.start_position().row + 1,
                        statement.end_position().row + 1,
                    ));
                }
            }
            let handler = Handler {
                path: path.clone(),
                first: lo,
                last: (clause.end_position().row + 1).min(first + 55),
                imports,
                raises: raises
                    .iter()
                    .take(6)
                    .map(|node| (node.start_position().row + 1, node.end_position().row + 1))
                    .collect(),
            };
            best = Some((key, handler));
        }
    }
    Some(best.map(|(_, handler)| handler))
}
