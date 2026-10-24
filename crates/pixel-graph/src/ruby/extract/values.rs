// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The value path of a receiver (ADR-0038): a chain of calls rooted at a
//! constant, written without its arguments but their shape, which
//! resolution types as a record or a relation of a class, or not at all.
//!
//! `Room.find(params[:id]).messages` is `C:Room.find(1).messages`: the
//! root `C:` and the constant as written, then each call's name and the
//! shape of its arguments ([`shape`]). A call given a block, an
//! argument the rules cannot read (`&blk`, `...`) or any other root
//! (a local, a method's return value, a literal) has none: such a
//! receiver keeps its text, which no rule types.

use rustc_hash::FxHashMap as HashMap;

use tree_sitter::Node;

use super::Walker;
use crate::ruby::{MAYBE_NIL, VALUE, VALUE_CONSTANT, VALUE_FACTORY, VALUE_FIXTURE, VALUE_LET};
use crate::text;

/// Most calls a path holds: a longer chain is left untyped.
const MAX_SEGMENTS: usize = 8;

/// The receiver of `call` as written; `Foo.new(args)` and `Job.set(args)`
/// on a constant keep the constant and the method only, which arguments
/// cannot change (v1 `ruby_receiver`).
pub(super) fn written(w: &Walker, call: Node) -> Option<String> {
    let receiver = call.child_by_field_name("receiver")?;
    if receiver.kind() == "call"
        && let Some(method) = w.field_text(receiver, "method")
        && matches!(method.as_str(), "new" | "set")
        && let Some(owner) = receiver.child_by_field_name("receiver")
        && matches!(owner.kind(), "constant" | "scope_resolution")
    {
        return Some(format!("{}.{method}", w.text(owner)));
    }
    Some(w.text(receiver))
}

/// The qualifier of `call` when its receiver is a chain the rules may
/// type ([`VALUE`] and its value path); `None` for any other receiver,
/// whose text is the qualifier. `Foo.new(..)` and `Job.set(..)` keep the
/// form the constant rules read.
pub(super) fn typed(w: &Walker, call: Node) -> Option<String> {
    let receiver = call.child_by_field_name("receiver")?;
    // `Foo.new` and `Job.set` on a constant keep the constant rules' form.
    let constant_form = receiver.kind() == "call"
        && w.field_text(receiver, "method")
            .is_some_and(|method| matches!(method.as_str(), "new" | "set"))
        && receiver
            .child_by_field_name("receiver")
            .is_some_and(|owner| matches!(owner.kind(), "constant" | "scope_resolution"));
    if !matches!(receiver.kind(), "call" | "identifier") || constant_form {
        return None;
    }
    Some(format!("{VALUE}{}", value_path(w, receiver, 0)?))
}

/// A call's target as written, on one line: `User.where`, `save`. A long
/// receiver loses its start, never the method's name: `…tail.first`.
pub(super) fn call_target(name: &str, receiver: Option<&str>) -> String {
    let Some(receiver) = receiver else {
        return name.to_owned();
    };
    let receiver = receiver.split_whitespace().collect::<Vec<_>>().join(" ");
    let whole = format!("{receiver}.{name}");
    if whole.chars().count() <= text::SIGNATURE_CAP {
        return whole;
    }
    let keep = text::SIGNATURE_CAP.saturating_sub(name.chars().count() + 2);
    let skip = receiver.chars().count().saturating_sub(keep);
    let tail: String = receiver.chars().skip(skip).collect();
    format!("…{tail}.{name}")
}

/// The value path of `node`, the last expression of a `let` block
/// (ADR-0041), or `None`.
pub(super) fn path_of(w: &Walker, node: Node) -> Option<String> {
    value_path(w, node, 0)
}

/// The value path of `node`, or `None`: a call on a constant or on a
/// value with a path, or a local whose one binding assigns one
/// ([`Gates`]); where `self` is an example of an RSpec group, a bare name
/// the group may answer (`purchase`, `subject`, `described_class`,
/// ADR-0041). `depth` counts the calls and locals read so far.
fn value_path(w: &Walker, node: Node, depth: usize) -> Option<String> {
    if depth >= MAX_SEGMENTS {
        return None;
    }
    match node.kind() {
        "call" => {
            let method = node.child_by_field_name("method")?;
            if !matches!(method.kind(), "identifier" | "constant") {
                return None;
            }
            let Some(inner) = node.child_by_field_name("receiver") else {
                return fixture(w, node)
                    .or_else(|| group_name(w, node))
                    .or_else(|| factory(w, node));
            };
            let segment = format!("{}{}", w.text(method), shape(w, node)?);
            let base = if matches!(inner.kind(), "constant" | "scope_resolution") {
                format!("{VALUE_CONSTANT}{}", super::one_line(&w.text(inner)))
            } else {
                value_path(w, inner, depth + 1)?
            };
            Some(format!("{base}.{segment}"))
        }
        "identifier" if w.locals.is_local(&w.text(node)) => local_value(w, node, depth),
        "identifier" => group_name(w, node),
        _ => None,
    }
}

/// The value path of the local `node` reads: the one its scope's single
/// binding assigns, parsed before `node`, followed by [`MAYBE_NIL`]: the
/// assignment may not have run (a branch), leaving `nil`.
fn local_value(w: &Walker, node: Node, depth: usize) -> Option<String> {
    let name = w.text(node);
    if !w.locals.is_local(&name) {
        return None;
    }
    let gate = w.values.stack.last()?;
    if gate.opaque {
        return None;
    }
    let value = (*gate.locals.get(&name)?)?;
    if value.end_byte() > node.start_byte() {
        return None;
    }
    let path = value_path(w, value, depth + 1)?;
    Some(format!("{path}.{MAYBE_NIL}"))
}

/// The root of a bare name an RSpec group may answer (`L:purchase`): a
/// receiverless call of no argument and no block, or an identifier that is
/// no local, where `self` is an example of a group.
fn group_name(w: &Walker, node: Node) -> Option<String> {
    w.example_group()?;
    let name = match node.kind() {
        "identifier" => w.text(node),
        _ if node.child_by_field_name("arguments").is_some()
            || node.child_by_field_name("block").is_some() =>
        {
            return None;
        }
        _ => {
            let method = node.child_by_field_name("method")?;
            if method.kind() != "identifier" {
                return None;
            }
            w.text(method)
        }
    };
    (name != "it").then(|| format!("{VALUE_LET}{name}"))
}

/// The root of a factory_bot call in an example (`Y:purchase` for
/// `create(:purchase, ...)` or `build(:purchase)`): its first argument a
/// literal name, no block.
fn factory(w: &Walker, call: Node) -> Option<String> {
    w.example_group()?;
    let method = w.field_text(call, "method")?;
    if !matches!(method.as_str(), "create" | "build") || call.child_by_field_name("block").is_some()
    {
        return None;
    }
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let first = args
        .named_children(&mut cursor)
        .find(|arg| arg.kind() != "comment")?;
    if matches!(first.kind(), "block_argument" | "forward_argument") {
        return None;
    }
    let name = super::rspec::literal(w, first)?;
    Some(format!("{VALUE_FACTORY}{name}"))
}

/// Whether `path` is a Minitest file: under a `test` directory, or named
/// `*_test.rb`.
pub(super) fn minitest(path: &str) -> bool {
    let (dirs, name) = path.rsplit_once('/').unwrap_or(("", path));
    dirs.split('/').any(|dir| dir == "test") || name.ends_with("_test.rb")
}

/// The blocks Active Support's test case runs on an instance of the test
/// case: `setup do`, `teardown do`, `test "..." do`.
const TEST_BLOCKS: &[&str] = &["setup", "teardown", "test"];

/// The root of a fixture accessor's path (`users(:kevin)` → `F:users`):
/// a receiverless call of one symbol or string literal, in a Minitest
/// file, where `self` is an instance of the test case ([`in_test`]).
fn fixture(w: &Walker, call: Node) -> Option<String> {
    let method = call.child_by_field_name("method")?;
    if !w.minitest || method.kind() != "identifier" || call.child_by_field_name("block").is_some() {
        return None;
    }
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let named: Vec<Node> = args
        .named_children(&mut cursor)
        .filter(|arg| arg.kind() != "comment")
        .collect();
    let [name] = named.as_slice() else {
        return None;
    };
    let literal = name.kind() == "simple_symbol"
        || (name.kind() == "string" && {
            let mut cursor = name.walk();
            name.named_children(&mut cursor)
                .all(|part| part.kind() == "string_content")
        });
    (literal && in_test(w, call)).then(|| format!("{VALUE_FIXTURE}{}", w.text(method)))
}

/// Whether `self` at `node` is an instance of the test case: in a block
/// of [`TEST_BLOCKS`], or an instance method, written in a class body,
/// with only blocks that keep `self` in between (any block but one given
/// to a method that may run it with another `self`, `flow::rebinds`).
fn in_test(w: &Walker, node: Node) -> bool {
    let mut at = node;
    while let Some(parent) = at.parent() {
        match parent.kind() {
            "block" | "do_block" => {
                let Some(call) = parent.parent().filter(|call| call.kind() == "call") else {
                    return false;
                };
                let method = super::flow::runs(w, call).unwrap_or_default();
                if call.child_by_field_name("receiver").is_none()
                    && TEST_BLOCKS.contains(&method.as_str())
                {
                    return in_class_body(call);
                }
                if super::flow::rebinds(w, &method) {
                    return false;
                }
            }
            "method" => return in_class_body(parent),
            "singleton_method" | "class" | "module" | "singleton_class" | "program" => {
                return false;
            }
            _ => {}
        }
        at = parent;
    }
    false
}

/// Whether `node` is a statement of a class body.
fn in_class_body(node: Node) -> bool {
    node.parent()
        .filter(|body| body.kind() == "body_statement")
        .and_then(|body| body.parent())
        .is_some_and(|owner| owner.kind() == "class")
}

/// The locals each open scope gate (a `def`, a class or module body, the
/// file) binds once, to a value the rules may type, innermost last.
#[derive(Default)]
pub(super) struct Gates<'a> {
    stack: Vec<Gate<'a>>,
}

#[derive(Default)]
pub(super) struct Gate<'a> {
    /// Local → the right side of its one binding when it is a plain
    /// assignment (`x = expr`); `None` for a name bound otherwise or more
    /// than once anywhere in the gate (a parameter, `||=`, a block
    /// parameter, a rescue variable, a pattern, a named capture).
    locals: HashMap<String, Option<Node<'a>>>,
    /// The gate evaluates code or reaches its binding (`eval`, `binding`,
    /// `local_variable_set`, a string `instance_eval`): any local may be
    /// assigned where the rules do not read it.
    opaque: bool,
}

impl<'a> Gates<'a> {
    /// Opens the gate `node`, reading every binding in it but those of the
    /// gates nested in it.
    pub(super) fn open(w: &Walker<'a>, node: Node<'a>) -> Gate<'a> {
        let mut gate = Gate::default();
        scan(w, node, None, None, 0, &mut gate);
        gate
    }

    pub(super) fn push(&mut self, gate: Gate<'a>) {
        self.stack.push(gate);
    }

    pub(super) fn pop(&mut self) {
        self.stack.pop();
    }
}

/// The calls that evaluate code where the caller's locals are visible, or
/// hand them out.
const EVALUATORS: &[&str] = &["binding", "eval", "local_variable_set"];

/// The calls that evaluate a string where the caller's locals are visible.
const STRING_EVALUATORS: &[&str] = &["class_eval", "instance_eval", "module_eval"];

fn scan<'a>(
    w: &Walker<'a>,
    node: Node<'a>,
    parent: Option<Node<'a>>,
    field: Option<&str>,
    depth: usize,
    gate: &mut Gate<'a>,
) {
    if depth > super::MAX_DEPTH {
        gate.opaque = true;
        return;
    }
    let kind = node.kind();
    if depth > 0 && super::scope_of(kind) == Some(super::Scope::Gate) {
        return;
    }
    let mut bind = |name: String, value: Option<Node<'a>>| {
        gate.locals
            .entry(name)
            .and_modify(|bound| *bound = None)
            .or_insert(value);
    };
    match kind {
        "identifier" => match super::role(parent, field) {
            super::Ident::Binding => {
                let value = parent
                    .filter(|p| p.kind() == "assignment" && field == Some("left"))
                    .and_then(|p| p.child_by_field_name("right"));
                bind(w.text(node), value);
            }
            super::Ident::Expr if EVALUATORS.contains(&w.text(node).as_str()) => {
                gate.opaque = true;
            }
            _ => {}
        },
        "keyword_pattern" if node.child_by_field_name("value").is_none() => {
            if let Some(key) = w.field_text(node, "key") {
                bind(key, None);
            }
        }
        "binary" => {
            for name in w.named_captures(node) {
                bind(name, None);
            }
        }
        "call" => {
            let method = w.field_text(node, "method").unwrap_or_default();
            if EVALUATORS.contains(&method.as_str())
                || (STRING_EVALUATORS.contains(&method.as_str())
                    && node.child_by_field_name("block").is_none())
            {
                gate.opaque = true;
            }
        }
        _ => {}
    }
    for (child, field) in super::children(node) {
        scan(w, child, Some(node), field, depth + 1, gate);
    }
}

/// The shape of a call's arguments, all resolution reads of them: `""`
/// for none, `(1)` for one positional argument that is neither a splat
/// nor an array or range literal, `(h)` for keywords or one hash literal
/// only, `(*)` for anything else; `None` for a call given a block or
/// arguments the rules cannot read (`&blk`, `...`).
fn shape(w: &Walker, call: Node) -> Option<&'static str> {
    if call.child_by_field_name("block").is_some() {
        return None;
    }
    let Some(args) = call.child_by_field_name("arguments") else {
        return Some("");
    };
    let mut cursor = args.walk();
    let named: Vec<Node> = args
        .named_children(&mut cursor)
        .filter(|arg| arg.kind() != "comment")
        .collect();
    if named
        .iter()
        .any(|arg| matches!(arg.kind(), "block_argument" | "forward_argument"))
        || w.text(args).trim_end().ends_with('&')
    {
        return None;
    }
    let keyword = |arg: &Node| matches!(arg.kind(), "pair" | "hash_splat_argument");
    Some(match named.as_slice() {
        [] => "",
        [one] if one.kind() == "hash" => "(h)",
        all if all.iter().all(keyword) => "(h)",
        [one]
            if !matches!(
                one.kind(),
                "splat_argument" | "array" | "string_array" | "symbol_array" | "range"
            ) =>
        {
            "(1)"
        }
        _ => "(*)",
    })
}

#[cfg(test)]
mod audited_contract_tests {
    use super::super::{PARSE_BUDGET, parse, rspec::Frame};
    use super::*;

    fn expression(source: &str, check: impl FnOnce(&Walker<'_>, Node<'_>)) {
        let tree = parse(source.as_bytes(), PARSE_BUDGET).unwrap();
        let walker = Walker::new(source.as_bytes());
        check(&walker, tree.root_node().named_child(0).unwrap());
    }

    #[test]
    fn argument_shapes_should_preserve_hashes_arrays_splats_and_blocks() {
        for (source, expected) in [
            ("Room.find", Some("")),
            ("Room.find()", Some("")),
            ("Room.find(7)", Some("(1)")),
            ("Room.find(a: 1)", Some("(h)")),
            ("Room.find({a: 1})", Some("(h)")),
            ("Room.find(**attrs)", Some("(h)")),
            ("Room.find(1, 2)", Some("(*)")),
            ("Room.find([1])", Some("(*)")),
            ("Room.find(%w[a])", Some("(*)")),
            ("Room.find(%i[a])", Some("(*)")),
            ("Room.find(1..2)", Some("(*)")),
            ("Room.find(*ids)", Some("(*)")),
            ("Room.find(&block)", None),
            ("Room.find { work }", None),
        ] {
            expression(source, |walker, node| {
                assert_eq!(shape(walker, node), expected, "{source}")
            });
        }
    }

    #[test]
    fn call_targets_should_keep_the_method_and_exact_unicode_character_budget() {
        assert_eq!(call_target("save", None), "save");
        assert_eq!(call_target("save", Some(" a\n  b ")), "a b.save");
        let receiver = "é".repeat(text::SIGNATURE_CAP);
        let result = call_target("save", Some(&receiver));
        assert_eq!(
            result,
            format!("…{}.save", "é".repeat(text::SIGNATURE_CAP - 6))
        );
        assert_eq!(result.chars().count(), text::SIGNATURE_CAP);
    }

    #[test]
    fn minitest_paths_should_match_a_directory_or_a_test_suffix_independently() {
        for (path, expected) in [
            ("test/unit/x.rb", true),
            ("lib/x_test.rb", true),
            ("x_test.rb", true),
            ("tests/x.rb", false),
            ("testish/x.rb", false),
            ("lib/x.rb", false),
        ] {
            assert_eq!(minitest(path), expected, "{path}");
        }
    }

    #[test]
    fn scanning_should_keep_all_siblings_but_not_locals_of_nested_scopes() {
        let source = "first = Room.new\nsecond = Room.find(1)\ndef nested\n hidden = User.new\nend\nthird = first\n";
        let tree = parse(source.as_bytes(), PARSE_BUDGET).unwrap();
        let walker = Walker::new(source.as_bytes());
        let gate = Gates::open(&walker, tree.root_node());
        let mut names: Vec<_> = gate.locals.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["first", "second", "third"]);
        assert_eq!(walker.text(gate.locals["second"].unwrap()), "Room.find(1)");
        assert!(!gate.opaque);
    }

    #[test]
    fn scanning_should_accept_the_depth_cap_and_reject_the_next_depth() {
        expression("harmless", |walker, node| {
            for (depth, opaque) in [
                (super::super::MAX_DEPTH, false),
                (super::super::MAX_DEPTH + 1, true),
            ] {
                let mut gate = Gate::default();
                scan(walker, node, None, None, depth, &mut gate);
                assert_eq!(gate.opaque, opaque, "depth {depth}");
            }
        });
    }

    #[test]
    fn scanning_should_invalidate_reassigned_or_dynamically_evaluated_locals() {
        for (source, opaque, bound) in [
            ("x = Room.new\nx = User.new", false, false),
            ("x = Room.new\neval('x = nil')", true, true),
            ("x = Room.new\nbinding", true, true),
            ("x = Room.new\nRoom.class_eval('x')", true, true),
            ("x = Room.new\nRoom.class_eval { work }", false, true),
        ] {
            let tree = parse(source.as_bytes(), PARSE_BUDGET).unwrap();
            let walker = Walker::new(source.as_bytes());
            let gate = Gates::open(&walker, tree.root_node());
            assert_eq!(gate.opaque, opaque, "{source}");
            assert_eq!(gate.locals["x"].is_some(), bound, "{source}");
        }
    }

    #[test]
    fn group_roots_should_require_an_example_and_no_arguments_or_block() {
        for (source, expected) in [
            ("purchase", Some("L:purchase")),
            ("purchase()", None),
            ("purchase(1)", None),
            ("purchase { work }", None),
            ("it", None),
        ] {
            let tree = parse(source.as_bytes(), PARSE_BUDGET).unwrap();
            let node = tree.root_node().named_child(0).unwrap();
            let mut walker = Walker::new(source.as_bytes());
            assert_eq!(group_name(&walker, node), None);
            walker.rspec = true;
            walker.frames.push(Frame::Example(1, false));
            assert_eq!(group_name(&walker, node).as_deref(), expected, "{source}");
        }
    }

    #[test]
    fn factory_roots_should_require_a_supported_method_without_a_block() {
        for (source, expected) in [
            ("create(:purchase)", Some("Y:purchase")),
            ("build(:purchase)", Some("Y:purchase")),
            ("destroy(:purchase)", None),
            ("create(:purchase) { work }", None),
        ] {
            let tree = parse(source.as_bytes(), PARSE_BUDGET).unwrap();
            let node = tree.root_node().named_child(0).unwrap();
            let mut walker = Walker::new(source.as_bytes());
            walker.rspec = true;
            walker.frames.push(Frame::Example(1, false));
            assert_eq!(factory(&walker, node).as_deref(), expected, "{source}");
        }
    }

    #[test]
    fn value_paths_should_stop_at_the_segment_budget() {
        expression("Room.where(id: 1).first", |walker, node| {
            assert_eq!(
                value_path(walker, node, 0).as_deref(),
                Some("C:Room.where(h).first")
            );
            assert_eq!(value_path(walker, node, MAX_SEGMENTS - 1), None);
            assert_eq!(value_path(walker, node, MAX_SEGMENTS), None);
        });
    }

    #[test]
    fn fixture_roots_should_require_literal_names_and_instance_test_context() {
        use crate::ruby::Context;
        use crate::ruby::extract::testing::rows;
        for (source, expected) in [
            (
                "class Check; def test_run; users(:one).label; end; end",
                true,
            ),
            ("class Check; setup do; users(:one).label; end; end", true),
            (
                "class Check; def test_run; [1].each { users(:one).label }; end; end",
                true,
            ),
            ("users(:one).label", false),
            ("class Check; users(:one).label; end", false),
            (
                "class Check; def self.test_run; users(:one).label; end; end",
                false,
            ),
            (
                "class Check; def test_run; other.instance_eval { users(:one).label }; end; end",
                false,
            ),
            (
                "class Check; def test_run; users(key).label; end; end",
                false,
            ),
            (
                "class Check; def test_run; users(:one,:two).label; end; end",
                false,
            ),
            (
                "class Check; def test_run; users(:one) { work }.label; end; end",
                false,
            ),
        ] {
            let found = rows("test/check_test.rb", source);
            let label = found
                .edges
                .iter()
                .map(|edge| Context::decode(&edge.context))
                .find(|c| c.name == "label")
                .unwrap();
            assert_eq!(
                label.qualifier == ":value F:users",
                expected,
                "{source}: {}",
                label.qualifier
            );
        }
    }

    #[test]
    fn pattern_scanning_should_bind_shorthand_keys_and_named_captures_without_inventing_explicit_keys()
     {
        for (source, expected) in [
            ("case input; in { name: }; end", vec!["name"]),
            ("case input; in { name: other }; end", vec!["other"]),
            ("/(?<name>.+)/ =~ input", vec!["name"]),
        ] {
            let tree = parse(source.as_bytes(), PARSE_BUDGET).unwrap();
            let walker = Walker::new(source.as_bytes());
            let gate = Gates::open(&walker, tree.root_node());
            let mut names = gate.locals.keys().map(String::as_str).collect::<Vec<_>>();
            names.sort_unstable();
            assert_eq!(names, expected, "{source}");
        }
    }

    #[test]
    fn long_local_alias_chains_should_abstain_instead_of_retyping_forever() {
        use crate::ruby::Context;
        use crate::ruby::extract::testing::rows;
        let mut source = "class Check; def run; a0=Room.new;".to_string();
        for i in 1..12 {
            source.push_str(&format!("a{i}=a{};", i - 1));
        }
        source.push_str("a11.label; end; end");
        let found = rows("check.rb", &source);
        let label = found
            .edges
            .iter()
            .map(|edge| Context::decode(&edge.context))
            .find(|c| c.name == "label")
            .unwrap();
        assert_eq!(label.qualifier, "a11");
    }
}
