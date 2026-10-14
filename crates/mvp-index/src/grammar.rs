// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The node kinds and field names the rule files compare against exist in
//! the grammars they are pinned to. A rule file reads a tree by its kinds
//! (`node.kind() == "call"`) and its fields (`child_by_field_name("name")`);
//! a name the grammar does not have never matches, and the site it should
//! have decided gives no edge (C-10's abstention), with no test failing. A
//! grammar bump that renames a kind does exactly that.
//!
//! The names are read from the rule files' own source, parsed with the Rust
//! grammar, wherever a string literal takes the place of a kind or a field:
//!
//! - an operand of `==` or `!=` whose other side is one;
//! - a pattern of a `match` or `matches!` on one, position by position in
//!   a tuple, inside `Some(..)`;
//! - the argument of `child_by_field_name` or `children_by_field_name`;
//! - the elements of a constant whose `contains` is asked about one, and
//!   the argument of `contains` on a collection of them;
//! - an argument of a function or a closure bound by `let` whose
//!   parameter is one, at any call of it by name in the same language;
//! - each literal a `let` may hold (`if c { "a" } else { "b" }`) when the
//!   name is used as one.
//!
//! One is `x.kind()` or `x.field_name()`, what a closure or a method with
//! no argument makes of one (`.map(|n| n.kind())`, `.collect()`), a `let`
//! bound to one, or a parameter that is compared with one, passed where one
//! goes, or given one at a call: what a parameter is spreads from call to
//! call until nothing changes. What the scan cannot follow is stated in
//! [`SEEDS`]. A form it misses gives no name and no failure, so
//! `the_scan_reads_every_form_the_rule_files_use` holds one of each form.
//!
//! Comparisons of source text (`text == "def"`, an operator's text) are not
//! kinds and are not read.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use tree_sitter::{Language, Node, Parser, Tree};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Kind,
    Field,
}

/// What an expression holds: a kind or a field, or a tuple of them.
#[derive(Clone, Debug)]
enum Shape {
    None,
    Is(Role),
    Tuple(Vec<Shape>),
}

/// A parameter of a function or a closure, by the callable's node id.
type Slot = (usize, usize);

/// What a name is bound to in a function's body.
#[derive(Clone, Copy)]
enum Binding {
    Param(Slot),
    Fixed(Role),
    Closure(usize),
    /// A `let` of literals, by the node id of its value.
    Literals(usize),
}

/// Names the scan cannot follow, with what they hold: a function's
/// variable, by language. The Ruby walker gets each child's field from a
/// vector of `(node, cursor.field_name())` it builds.
const SEEDS: &[(&str, &str, &str, Role)] = &[
    ("ruby", "walk", "field", Role::Field),
    ("ruby", "walk", "child_field", Role::Field),
];

/// One source file of a language's rule files.
struct File {
    path: PathBuf,
    src: Vec<u8>,
    tree: Tree,
}

/// Every name read, each with the first place it is written.
#[derive(Default)]
struct Names {
    kinds: BTreeMap<String, String>,
    fields: BTreeMap<String, String>,
}

struct Scan<'a> {
    lang: &'a str,
    files: &'a [File],
    /// The literals of each `let` of literals, by its value's node id.
    lets: HashMap<usize, Vec<(String, String)>>,
    /// Functions by name and arity (`self` not counted): their node ids.
    functions: HashMap<(String, usize), Vec<usize>>,
    /// Constants by name: the string literals of their value, each with
    /// where it is written.
    constants: HashMap<String, Vec<(String, String)>>,
    roles: HashMap<Slot, Role>,
    changed: bool,
    names: Names,
}

fn parse(src: &[u8]) -> Tree {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .expect("the Rust grammar loads");
    parser.parse(src, None).expect("a parse with no deadline")
}

fn sources(dir: &Path, out: &mut Vec<File>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("the rule files' directory")
        .map(|entry| entry.expect("a directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let src = std::fs::read(&path).expect("a rule file");
            let tree = parse(&src);
            out.push(File { path, src, tree });
        }
    }
}

fn named(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn children(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.children(&mut cursor).collect()
}

/// A callable's parameter names, `self` left out.
fn params<'t>(file: &File, callable: Node<'t>) -> Vec<Option<String>> {
    let Some(list) = callable.child_by_field_name("parameters") else {
        return Vec::new();
    };
    named(list)
        .into_iter()
        .filter(|p| !matches!(p.kind(), "self_parameter" | "attribute_item"))
        .map(|p| {
            let name = if p.kind() == "identifier" {
                Some(p)
            } else {
                p.child_by_field_name("pattern")
            };
            name.filter(|n| n.kind() == "identifier")
                .map(|n| text(file, n).to_owned())
        })
        .collect()
}

fn text<'s>(file: &'s File, node: Node) -> &'s str {
    node.utf8_text(&file.src).expect("UTF-8 source")
}

/// The content of a plain string literal.
fn literal(file: &File, node: Node) -> Option<String> {
    (node.kind() == "string_literal").then(|| {
        let written = text(file, node);
        written[1..written.len() - 1].to_owned()
    })
}

/// `Some("x")`'s literal, or the literal itself.
fn unwrap_some<'t>(file: &File, node: Node<'t>) -> Node<'t> {
    if node.kind() == "call_expression"
        && node
            .child_by_field_name("function")
            .is_some_and(|f| text(file, f) == "Some")
        && let Some(args) = node.child_by_field_name("arguments")
        && let [arg] = named(args)[..]
    {
        return arg;
    }
    node
}

fn method_name<'f>(file: &'f File, call: Node<'f>) -> Option<(&'f str, Option<Node<'f>>)> {
    let function = call.child_by_field_name("function")?;
    match function.kind() {
        "field_expression" => {
            let field = function.child_by_field_name("field")?;
            Some((text(file, field), function.child_by_field_name("value")))
        }
        "identifier" => Some((text(file, function), None)),
        "scoped_identifier" => Some((text(file, function.child_by_field_name("name")?), None)),
        _ => None,
    }
}

impl<'a> Scan<'a> {
    fn new(lang: &'a str, files: &'a [File]) -> Scan<'a> {
        let mut scan = Scan {
            lang,
            files,
            lets: HashMap::new(),
            functions: HashMap::new(),
            constants: HashMap::new(),
            roles: HashMap::new(),
            changed: false,
            names: Names::default(),
        };
        for file in files {
            scan.index(file, file.tree.root_node());
        }
        scan
    }

    fn index(&mut self, file: &'a File, node: Node<'a>) {
        match node.kind() {
            "function_item" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let key = (text(file, name).to_owned(), params(file, node).len());
                    self.functions.entry(key).or_default().push(node.id());
                }
            }
            "const_item" | "static_item" => {
                if let (Some(name), Some(value)) = (
                    node.child_by_field_name("name"),
                    node.child_by_field_name("value"),
                ) {
                    let mut literals = Vec::new();
                    strings(value, &mut literals);
                    let literals = literals
                        .into_iter()
                        .filter_map(|l| Some((literal(file, l)?, at(file, l))))
                        .collect();
                    self.constants.insert(text(file, name).to_owned(), literals);
                }
            }
            _ => {}
        }
        for child in named(node) {
            self.index(file, child);
        }
    }

    /// Walks every file until no parameter learns a role, then once more
    /// to collect with every role known.
    fn run(mut self) -> Names {
        loop {
            self.changed = false;
            for file in self.files {
                self.walk(file, file.tree.root_node(), &mut Vec::new());
            }
            if !self.changed {
                return self.names;
            }
        }
    }

    fn learn(&mut self, slot: Slot, role: Role) {
        if let Entry::Vacant(vacant) = self.roles.entry(slot) {
            vacant.insert(role);
            self.changed = true;
        }
    }

    fn collect(&mut self, file: &File, node: Node, role: Role) {
        if let Some(name) = literal(file, node) {
            self.record(role, name, at(file, node));
        }
    }

    fn record(&mut self, role: Role, name: String, at: String) {
        let names = match role {
            Role::Kind => &mut self.names.kinds,
            Role::Field => &mut self.names.fields,
        };
        names.entry(name).or_insert(at);
    }

    fn lookup(env: &[(String, Binding)], name: &str) -> Option<Binding> {
        env.iter().rev().find(|(n, _)| n == name).map(|(_, b)| *b)
    }

    fn slot_of(&self, file: &File, env: &[(String, Binding)], node: Node) -> Option<Slot> {
        match (node.kind(), Self::lookup(env, text(file, node))) {
            ("identifier", Some(Binding::Param(slot))) => Some(slot),
            _ => None,
        }
    }

    /// `node` stands where a kind or a field goes: a literal is one, a
    /// parameter becomes one, a `let` of literals holds some.
    fn sink(&mut self, file: &File, env: &[(String, Binding)], node: Node, role: Role) {
        let node = unwrap_some(file, node);
        self.collect(file, node, role);
        if node.kind() != "identifier" {
            return;
        }
        match Self::lookup(env, text(file, node)) {
            Some(Binding::Param(slot)) => self.learn(slot, role),
            Some(Binding::Literals(id)) => {
                for (name, at) in self.lets.get(&id).cloned().unwrap_or_default() {
                    self.record(role, name, at);
                }
            }
            _ => {}
        }
    }

    fn shape(&self, file: &File, env: &[(String, Binding)], node: Node) -> Shape {
        match node.kind() {
            "identifier" => match Self::lookup(env, text(file, node)) {
                Some(Binding::Param(slot)) => {
                    self.roles.get(&slot).map_or(Shape::None, |r| Shape::Is(*r))
                }
                Some(Binding::Fixed(role)) => Shape::Is(role),
                _ => Shape::None,
            },
            "reference_expression" | "parenthesized_expression" => named(node)
                .last()
                .map_or(Shape::None, |inner| self.shape(file, env, *inner)),
            "tuple_expression" => Shape::Tuple(
                named(node)
                    .into_iter()
                    .map(|e| self.shape(file, env, e))
                    .collect(),
            ),
            "call_expression" => {
                let args = node
                    .child_by_field_name("arguments")
                    .map(named)
                    .unwrap_or_default();
                match (method_name(file, node), &args[..]) {
                    (Some(("kind", Some(_))), []) => Shape::Is(Role::Kind),
                    (Some(("field_name", Some(_))), []) => Shape::Is(Role::Field),
                    // `.collect()`, `.as_deref()`: what the receiver is.
                    (Some((_, Some(receiver))), []) => self.shape(file, env, receiver),
                    // `.map(|n| n.kind())`
                    (Some((_, Some(_))), [closure]) if closure.kind() == "closure_expression" => {
                        let mut inner = env.to_vec();
                        self.bind_params(file, *closure, &mut inner);
                        closure
                            .child_by_field_name("body")
                            .map_or(Shape::None, |body| self.shape(file, &inner, body))
                    }
                    _ => Shape::None,
                }
            }
            _ => Shape::None,
        }
    }

    fn bind_params(&self, file: &File, callable: Node, env: &mut Vec<(String, Binding)>) {
        for (i, name) in params(file, callable).into_iter().enumerate() {
            if let Some(name) = name {
                env.push((name, Binding::Param((callable.id(), i))));
            }
        }
    }

    fn walk(&mut self, file: &'a File, node: Node<'a>, env: &mut Vec<(String, Binding)>) {
        let depth = env.len();
        match node.kind() {
            "function_item" | "closure_expression" => {
                self.bind_params(file, node, env);
                let name = node.child_by_field_name("name").map(|n| text(file, n));
                for (lang, function, variable, role) in SEEDS {
                    if *lang == self.lang && name == Some(*function) {
                        env.push(((*variable).to_owned(), Binding::Fixed(*role)));
                    }
                }
            }
            "let_declaration" => {
                if let (Some(pattern), Some(value)) = (
                    node.child_by_field_name("pattern"),
                    node.child_by_field_name("value"),
                ) && pattern.kind() == "identifier"
                {
                    self.walk(file, value, env);
                    let name = text(file, pattern).to_owned();
                    let mut tails = Vec::new();
                    literal_tails(value, &mut tails);
                    if value.kind() == "closure_expression" {
                        env.push((name, Binding::Closure(value.id())));
                    } else if let Shape::Is(role) = self.shape(file, env, value) {
                        env.push((name, Binding::Fixed(role)));
                    } else if !tails.is_empty() {
                        let tails = tails
                            .into_iter()
                            .filter_map(|t| Some((literal(file, t)?, at(file, t))))
                            .collect();
                        self.lets.insert(value.id(), tails);
                        env.push((name, Binding::Literals(value.id())));
                    }
                    // Bound for the rest of the block: not truncated here.
                    return;
                }
            }
            "binary_expression" => self.compare(file, node, env),
            "match_expression" => {
                if let Some(value) = node.child_by_field_name("value") {
                    let shape = self.shape(file, env, value);
                    let arms = node
                        .child_by_field_name("body")
                        .map(named)
                        .unwrap_or_default();
                    for arm in arms.into_iter().filter(|a| a.kind() == "match_arm") {
                        if let Some(pattern) = arm
                            .child_by_field_name("pattern")
                            .and_then(|p| named(p).first().copied())
                        {
                            self.patterns(file, pattern, &shape);
                        }
                    }
                }
            }
            "macro_invocation" => self.matches(file, node, env),
            "call_expression" => self.call(file, node, env),
            _ => {}
        }
        for child in named(node) {
            self.walk(file, child, env);
        }
        if node.kind() != "let_declaration" {
            env.truncate(depth);
        }
    }

    fn compare(&mut self, file: &File, node: Node, env: &[(String, Binding)]) {
        let operator = node.child_by_field_name("operator").map(|o| text(file, o));
        if !matches!(operator, Some("==" | "!=")) {
            return;
        }
        let (Some(left), Some(right)) = (
            node.child_by_field_name("left"),
            node.child_by_field_name("right"),
        ) else {
            return;
        };
        for (one, other) in [(left, right), (right, left)] {
            if let Shape::Is(role) = self.shape(file, env, one) {
                self.sink(file, env, other, role);
            }
        }
    }

    fn patterns(&mut self, file: &File, pattern: Node, shape: &Shape) {
        match (pattern.kind(), shape) {
            ("string_literal", Shape::Is(role)) => self.collect(file, pattern, *role),
            ("tuple_pattern", Shape::Tuple(shapes)) => {
                for (element, shape) in named(pattern).into_iter().zip(shapes) {
                    self.patterns(file, element, shape);
                }
            }
            ("tuple_struct_pattern", _) => {
                let ty = pattern.child_by_field_name("type").map(|t| t.id());
                for inner in named(pattern).into_iter().filter(|p| Some(p.id()) != ty) {
                    self.patterns(file, inner, shape);
                }
            }
            ("or_pattern" | "captured_pattern", _) => {
                for inner in named(pattern) {
                    self.patterns(file, inner, shape);
                }
            }
            _ => {}
        }
    }

    /// `matches!(x.kind(), "a" | "b")`: the macro's tokens, not an
    /// expression, so the scrutinee is read from them.
    fn matches(&mut self, file: &File, node: Node, env: &[(String, Binding)]) {
        let is_matches = node
            .child_by_field_name("macro")
            .is_some_and(|m| text(file, m) == "matches");
        let Some(tokens) = named(node).into_iter().find(|n| n.kind() == "token_tree") else {
            return;
        };
        if !is_matches {
            return;
        }
        let tokens = children(tokens);
        let tokens = &tokens[1..tokens.len().saturating_sub(1)];
        let Some(comma) = tokens.iter().position(|t| t.kind() == ",") else {
            return;
        };
        let scrutinee = &tokens[..comma];
        let role = match scrutinee {
            [.., method, call]
                if call.kind() == "token_tree"
                    && call.named_child_count() == 0
                    && method.kind() == "identifier"
                    && scrutinee.len() > 2 =>
            {
                match text(file, *method) {
                    "kind" => Some(Role::Kind),
                    "field_name" => Some(Role::Field),
                    _ => None,
                }
            }
            [one] => match self.shape(file, env, *one) {
                Shape::Is(role) => Some(role),
                _ => None,
            },
            _ => None,
        };
        let Some(role) = role else {
            return;
        };
        for token in &tokens[comma + 1..] {
            if token.kind() == "identifier" && text(file, *token) == "if" {
                break;
            }
            let mut literals = Vec::new();
            strings(*token, &mut literals);
            for literal in literals {
                self.collect(file, literal, role);
            }
        }
    }

    fn call(&mut self, file: &'a File, node: Node<'a>, env: &[(String, Binding)]) {
        let Some((name, receiver)) = method_name(file, node) else {
            return;
        };
        let args = node
            .child_by_field_name("arguments")
            .map(named)
            .unwrap_or_default();
        if matches!(name, "child_by_field_name" | "children_by_field_name") {
            if let Some(arg) = args.first() {
                self.sink(file, env, *arg, Role::Field);
            }
            return;
        }
        // `tokens.contains(&"&")` where `tokens` holds kinds.
        if name == "contains"
            && let (Some(receiver), [arg]) = (receiver, &args[..])
            && let Shape::Is(role) = self.shape(file, env, receiver)
        {
            let arg = named(*arg)
                .last()
                .copied()
                .filter(|_| arg.kind() == "reference_expression");
            if let Some(arg) = arg {
                self.collect(file, arg, role);
            }
            return;
        }
        if name == "contains"
            && let (Some(receiver), [arg]) = (receiver, &args[..])
            && let Shape::Is(role) = self.shape(file, env, *arg)
        {
            if receiver.kind() == "identifier" {
                let literals = self.constants.get(text(file, receiver)).cloned();
                for (name, at) in literals.unwrap_or_default() {
                    self.record(role, name, at);
                }
            } else {
                let mut literals = Vec::new();
                strings(receiver, &mut literals);
                for literal in literals {
                    self.collect(file, literal, role);
                }
            }
            return;
        }
        // `kind.is_none_or(|kind| child.kind() == kind)`: the receiver is
        // what the closure's parameter is.
        if let (Some(receiver), [closure]) = (receiver, &args[..])
            && closure.kind() == "closure_expression"
            && let Some(slot) = self.slot_of(file, env, receiver)
            && let Some(role) = self.roles.get(&(closure.id(), 0)).copied()
        {
            self.learn(slot, role);
        }
        let callables: Vec<usize> = match (receiver, Self::lookup(env, name)) {
            (None, Some(Binding::Closure(id))) => vec![id],
            _ => self
                .functions
                .get(&(name.to_owned(), args.len()))
                .cloned()
                .unwrap_or_default(),
        };
        for callable in callables {
            for (i, arg) in args.iter().enumerate() {
                let slot = (callable, i);
                let arg = unwrap_some(file, *arg);
                if let Shape::Is(role) = self.shape(file, env, arg) {
                    self.learn(slot, role);
                }
                if let Some(role) = self.roles.get(&slot).copied() {
                    self.sink(file, env, arg, role);
                }
            }
        }
    }
}

fn at(file: &File, node: Node) -> String {
    let path = file.path.strip_prefix(env!("CARGO_MANIFEST_DIR"));
    let path = path.unwrap_or(&file.path).display();
    format!("{path}:{}", node.start_position().row + 1)
}

/// The literals a `let`'s value can be: itself, or the last expression of
/// each branch of an `if` or a `match`.
fn literal_tails<'t>(node: Node<'t>, out: &mut Vec<Node<'t>>) {
    match node.kind() {
        "string_literal" => out.push(node),
        "if_expression" => {
            for branch in ["consequence", "alternative"] {
                if let Some(branch) = node.child_by_field_name(branch) {
                    literal_tails(branch, out);
                }
            }
        }
        "else_clause" | "match_block" => {
            for child in named(node) {
                literal_tails(child, out);
            }
        }
        "match_expression" => {
            if let Some(body) = node.child_by_field_name("body") {
                literal_tails(body, out);
            }
        }
        "match_arm" => {
            if let Some(value) = node.child_by_field_name("value") {
                literal_tails(value, out);
            }
        }
        "block" => {
            if let Some(last) = named(node).last() {
                literal_tails(*last, out);
            }
        }
        _ => {}
    }
}

fn strings<'t>(node: Node<'t>, out: &mut Vec<Node<'t>>) {
    if node.kind() == "string_literal" {
        out.push(node);
        return;
    }
    for child in named(node) {
        strings(child, out);
    }
}

fn scan(lang: &str, files: &[File]) -> Names {
    Scan::new(lang, files).run()
}

fn rule_files(lang: &str) -> Vec<File> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(lang);
    let mut files = Vec::new();
    sources(&dir, &mut files);
    files
}

fn missing(language: &Language, names: &Names) -> Vec<String> {
    let kinds = names.kinds.iter().filter(|(kind, _)| {
        language.id_for_node_kind(kind, true) == 0 && language.id_for_node_kind(kind, false) == 0
    });
    let fields = names
        .fields
        .iter()
        .filter(|(field, _)| language.field_id_for_name(field).is_none());
    kinds
        .map(|(kind, at)| format!("kind {kind:?} at {at}"))
        .chain(fields.map(|(field, at)| format!("field {field:?} at {at}")))
        .collect()
}

/// Whether `function` is defined in `files` and names `variable`.
fn seeded(files: &[File], function: &str, variable: &str) -> bool {
    fn find(file: &File, node: Node, function: &str, variable: &str) -> bool {
        let defines = node.kind() == "function_item"
            && node
                .child_by_field_name("name")
                .is_some_and(|name| text(file, name) == function);
        if defines {
            let mut cursor = node.walk();
            let mut stack = vec![node];
            while let Some(n) = stack.pop() {
                if n.kind() == "identifier" && text(file, n) == variable {
                    return true;
                }
                stack.extend(n.named_children(&mut cursor));
            }
            return false;
        }
        named(node)
            .into_iter()
            .any(|child| find(file, child, function, variable))
    }
    files
        .iter()
        .any(|file| find(file, file.tree.root_node(), function, variable))
}

fn grammar(lang: &str) -> Language {
    match lang {
        "ruby" => tree_sitter_ruby::LANGUAGE.into(),
        _ => tree_sitter_rust::LANGUAGE.into(),
    }
}

#[test]
fn c10_every_kind_and_field_the_rule_files_name_is_in_their_grammar() {
    for lang in ["ruby", "rust"] {
        let files = rule_files(lang);
        let names = scan(lang, &files);
        // A scan gone blind would pass with nothing to check.
        assert!(
            names.kinds.len() > 50 && names.fields.len() > 10,
            "{lang}: {} kinds and {} fields read",
            names.kinds.len(),
            names.fields.len()
        );
        assert_eq!(
            missing(&grammar(lang), &names),
            Vec::<String>::new(),
            "{lang}"
        );
    }
}

#[test]
fn every_seed_names_a_variable_of_its_function() {
    for (lang, function, variable, _) in SEEDS {
        assert!(
            seeded(&rule_files(lang), function, variable),
            "{lang}: no `{variable}` in `fn {function}`"
        );
    }
}

/// One source per form the rule files write a kind or a field in, each
/// name unique to its form, beside literals that are neither.
const FORMS: &str = r#"
const ITEMS: &[&str] = &["k_const"];
const WORDS: &[&str] = &["not_const"];

fn is(node: Node, kind: &str) -> bool {
    node.kind() == kind
}

fn spread(kind: &str) -> bool {
    matches!(kind, "k_spread")
}

fn role(parent: Node, field: Option<&str>) -> bool {
    match (parent.kind(), field) {
        ("k_tuple", Some("f_tuple")) => true,
        _ => false,
    }
}

fn mixin(owner: usize, kind: &str) {}

fn forms(node: Node, cursor: Walk, text: &str, x: bool) {
    let _ = node.kind() == "k_eq";
    let _ = "k_rev" != node.kind();
    match node.kind() {
        "k_match" | "k_or" => {}
        _ => {}
    }
    let _ = matches!(node.kind(), "k_macro" | "k_macro2" if x);
    let _ = node.child_by_field_name("f_child");
    let _ = node.children_by_field_name("f_children", &mut cursor);
    let _ = ITEMS.contains(&node.kind());
    let _ = WORDS.contains(&text);
    let _ = is(node, "k_helper");
    let _ = spread(node.kind());
    let _ = role(node, cursor.field_name());
    let token = |kind: &str| node.children().any(|t| t.kind() == kind);
    let _ = token("k_closure");
    let read = |kind: Option<&str>| kind.is_none_or(|kind| node.kind() == kind);
    let _ = read(Some("k_option"));
    let kind = node.kind();
    let _ = kind == "k_let";
    let body = if x { "f_if" } else { "f_else" };
    let _ = node.child_by_field_name(body);
    match node.parent().map(|n| n.kind()) {
        Some("k_map") => {}
        _ => {}
    }
    let kinds: Vec<&str> = node.children().map(|c| c.kind()).collect();
    let _ = kinds.contains(&"k_vec");
    let _ = text == "not_text";
    mixin(0, "not_mixin");
}
"#;

#[test]
fn the_scan_reads_every_form_the_rule_files_use() {
    let src = FORMS.as_bytes().to_vec();
    let tree = parse(&src);
    let files = [File {
        path: PathBuf::from("forms.rs"),
        src,
        tree,
    }];
    let names = scan("forms", &files);
    let kinds: Vec<&str> = names.kinds.keys().map(String::as_str).collect();
    let fields: Vec<&str> = names.fields.keys().map(String::as_str).collect();
    assert_eq!(
        kinds,
        [
            "k_closure",
            "k_const",
            "k_eq",
            "k_helper",
            "k_let",
            "k_macro",
            "k_macro2",
            "k_map",
            "k_match",
            "k_option",
            "k_or",
            "k_rev",
            "k_spread",
            "k_tuple",
            "k_vec",
        ]
    );
    assert_eq!(
        fields,
        ["f_child", "f_children", "f_else", "f_if", "f_tuple"]
    );
}
