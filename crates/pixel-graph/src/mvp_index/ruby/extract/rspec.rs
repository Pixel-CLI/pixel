// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! RSpec's example groups as the walk meets them (ADR-0041).
//!
//! In a `*_spec.rb` file, a `describe` (or a form of it) given a block is
//! an example group: a class whose body the block is. The walk keeps a
//! stack of what each block, `def` and body around it makes of `self`
//! ([`Frame`]): a group's class, an instance of a group (an example, a
//! hook, a `let` or `subject` block, a `def` of the group), or an object
//! the model does not name. A call written where `self` is an instance of
//! a group carries that group ([`GROUP`]); a group records its parent,
//! the constant it describes, whether it has metadata of its own, the
//! `def`s, `let`s and `subject`s its body defines, and whether a statement
//! there may define what the rules do not read ([`Fact::Group`],
//! [`Fact::Member`]). The last expression of a `let` or `subject` block is
//! read where the walk meets it, so its locals are bound as Ruby binds
//! them ([`Fact::LetValue`]).

use tree_sitter::Node;

use super::{Walker, flow, line, one_line, values};
use crate::ruby::{Fact, GROUP, SHARED_GROUP};

/// The calls that make an example group.
const GROUPS: &[&str] = &[
    "describe",
    "context",
    "feature",
    "example_group",
    "fdescribe",
    "fcontext",
    "ffeature",
    "xdescribe",
    "xcontext",
    "xfeature",
];

/// The calls whose block RSpec runs on an instance of the group: examples,
/// which run on that group alone, and hooks ([`HOOKS`]).
const EXAMPLES: &[&str] = &[
    "it",
    "specify",
    "example",
    "scenario",
    "fit",
    "fspecify",
    "fexample",
    "fscenario",
    "xit",
    "xspecify",
    "xexample",
    "xscenario",
    "pending",
    "skip",
];

/// The hooks, run on the examples of the group and of its nested groups.
const HOOKS: &[&str] = &[
    "before",
    "after",
    "around",
    "prepend_before",
    "append_before",
    "prepend_after",
    "append_after",
];

/// The calls that define a memoized method of the group.
const LETS: &[&str] = &["let", "let!"];
const SUBJECTS: &[&str] = &["subject", "subject!"];

/// The calls that declare a shared group or make a nested one from it:
/// their block runs on whatever group includes it.
const SHARED: &[&str] = &[
    "shared_examples",
    "shared_examples_for",
    "shared_context",
    "it_behaves_like",
    "it_should_behave_like",
    "include_examples",
    "include_context",
];

/// The methods whose block may run with what they build as `self`.
const BUILDERS: &[&str] = &["new", "define"];

/// RSpec's DSL: a method of one of these names defined in spec code
/// outside a group may change what the DSL does.
const DSL: &[&str] = &[
    "after",
    "around",
    "before",
    "context",
    "described_class",
    "describe",
    "example",
    "example_group",
    "feature",
    "include_context",
    "include_examples",
    "it",
    "it_behaves_like",
    "let",
    "let!",
    "scenario",
    "shared_context",
    "shared_examples",
    "specify",
    "subject",
    "subject!",
];

/// What `self` is where the walk stands, as far as RSpec goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Frame {
    /// The file's top level, or a class or module body: a `describe`
    /// there makes a top-level group.
    Top,
    /// The class of the group declared at that line.
    Group(u32),
    /// An instance of the group declared at that line, or of one nested
    /// in it (`open`: a hook, a `let`, a `def`).
    Example(u32, bool),
    /// What the frame around it is: a block that keeps `self`.
    Keep,
    /// An object the model does not name.
    Other,
}

/// A `let` or `subject` block the walk is in: what its last expression
/// types.
pub(super) struct LetBlock {
    group: u32,
    names: Vec<String>,
    line: u32,
    block: usize,
    last: usize,
}

/// Whether `path` is read for RSpec's groups.
pub(super) fn spec_file(path: &str) -> bool {
    path.ends_with("_spec.rb")
}

/// Whether `path` is RSpec's code: a spec file, or a file under `spec/`.
pub(super) fn spec_code(path: &str) -> bool {
    spec_file(path) || path.split('/').rev().skip(1).any(|dir| dir == "spec")
}

impl Walker<'_> {
    /// The frame that decides `self`: the innermost that is no block
    /// keeping the one around it.
    fn frame(&self) -> Frame {
        self.frames
            .iter()
            .rev()
            .find(|frame| **frame != Frame::Keep)
            .copied()
            .unwrap_or(Frame::Top)
    }

    /// The group whose instance `self` is where the walk stands, and
    /// whether it may be an instance of a group nested in it.
    pub(super) fn example_group(&self) -> Option<(u32, bool)> {
        match self.frame() {
            Frame::Example(group, open) if self.rspec => Some((group, open)),
            _ => None,
        }
    }

    /// The `extra` of a call written where `self` is an instance of a
    /// group.
    pub(super) fn group_marker(&self) -> Option<String> {
        self.example_group()
            .map(|(group, open)| format!("{GROUP}{group}{}", if open { "+" } else { "" }))
    }
}

/// Opens the frame of `node`, when it changes `self`; returns whether one
/// was pushed.
pub(super) fn enter<'a>(w: &mut Walker<'a>, node: Node<'a>, parent: Option<Node<'a>>) -> bool {
    let frame = match node.kind() {
        "block" | "do_block" => match parent.filter(|p| p.kind() == "call") {
            Some(call) => block_frame(w, call, node),
            None => Frame::Other,
        },
        "method" => match w.frame() {
            Frame::Group(group) => Frame::Example(group, true),
            _ => Frame::Other,
        },
        "class" | "module" => Frame::Top,
        "singleton_method" | "singleton_class" => Frame::Other,
        "lambda" => Frame::Keep,
        _ => return false,
    };
    w.frames.push(frame);
    true
}

/// Closes the frame [`enter`] opened, and the `let` block with it.
pub(super) fn leave(w: &mut Walker, node: Node) {
    w.frames.pop();
    if w.lets.last().is_some_and(|block| block.block == node.id()) {
        w.lets.pop();
    }
}

/// The value of a `let` or `subject` block, recorded when the walk meets
/// its last expression.
pub(super) fn let_value(w: &mut Walker, node: Node) {
    let Some(block) = w.lets.last().filter(|block| block.last == node.id()) else {
        return;
    };
    // A block that may leave early returns what it left with.
    if w.exits.contains(&block.block) {
        return;
    }
    let Some(value) = values::path_of(w, node) else {
        return;
    };
    let block = w.lets.last().expect("checked");
    let facts: Vec<Fact> = block
        .names
        .iter()
        .map(|name| Fact::LetValue {
            group: block.group,
            name: name.clone(),
            line: block.line,
            value: value.clone(),
        })
        .collect();
    w.facts.extend(facts);
}

/// What the block of `call` makes of `self`.
fn block_frame<'a>(w: &mut Walker<'a>, call: Node<'a>, block: Node<'a>) -> Frame {
    let method = w.field_text(call, "method").unwrap_or_default();
    let receiver = call.child_by_field_name("receiver");
    let on_rspec = receiver.is_some_and(|r| {
        matches!(r.kind(), "constant" | "scope_resolution")
            && w.text(r).trim_start_matches("::") == "RSpec"
    });
    let around = w.frame();
    if GROUPS.contains(&method.as_str()) && (receiver.is_none() || on_rspec) {
        let parent = match around {
            _ if on_rspec => Some(0),
            Frame::Top => Some(0),
            Frame::Group(parent) => Some(parent),
            _ => None,
        };
        return match parent {
            Some(parent) => {
                group(w, call, block, parent);
                Frame::Group(line(call))
            }
            None => Frame::Other,
        };
    }
    if receiver.is_none() || on_rspec {
        let exact = EXAMPLES.contains(&method.as_str());
        let example = exact
            || HOOKS.contains(&method.as_str())
            || LETS.contains(&method.as_str())
            || SUBJECTS.contains(&method.as_str());
        if let (true, Frame::Group(group)) = (example && receiver.is_none(), around) {
            if let Some(names) = member_names(w, call, &method).filter(|n| !n.is_empty())
                && let Some(last) = last_statement(block)
            {
                w.lets.push(LetBlock {
                    group,
                    names,
                    line: line(call),
                    block: block.id(),
                    last: last.id(),
                });
            }
            return Frame::Example(group, !exact);
        }
        if DECLARES.contains(&method.as_str()) {
            return shared(w, call, block, around, on_rspec);
        }
        if SHARED.contains(&method.as_str()) || method == "configure" {
            return Frame::Other;
        }
    }
    // A block given to a constructor (`Class.new`, `Struct.new`,
    // `Data.define`) runs with what it builds as `self`.
    match flow::runs(w, call) {
        Some(method) if !flow::rebinds(w, &method) && !BUILDERS.contains(&method.as_str()) => {
            Frame::Keep
        }
        _ => Frame::Other,
    }
}

/// The calls that declare a shared group.
const DECLARES: &[&str] = &["shared_examples", "shared_examples_for", "shared_context"];

/// Records the shared group `call` declares: at the top level, a group of
/// its own, whose block runs in whatever group includes it (parent
/// [`SHARED_GROUP`]); in a group, a name that group alone sees, which
/// hides a top-level one there. A name that is no literal may be any.
/// Declared without a receiver where `self` is a group, it is that
/// group's and its nested groups' alone (`scoped`); `RSpec.shared_examples`
/// there, or one declared where `self` is no group, is the program's
/// (checked with rspec-core 3.13.6: another file's group cannot include
/// the first, and can the second).
fn shared<'a>(
    w: &mut Walker<'a>,
    call: Node<'a>,
    block: Node<'a>,
    around: Frame,
    on_rspec: bool,
) -> Frame {
    let args = arguments(call);
    let name = args
        .first()
        .and_then(|arg| literal(w, *arg))
        .unwrap_or_default();
    let metadata = args.iter().skip(1).any(|arg| arg.kind() != "string");
    let top = around == Frame::Top && !name.is_empty();
    w.facts.push(Fact::Shared {
        name,
        line: line(call),
        scoped: !on_rspec && matches!(around, Frame::Group(_)),
        metadata,
    });
    if !top {
        return Frame::Other;
    }
    group(w, call, block, SHARED_GROUP);
    Frame::Group(line(call))
}

/// Records the group `call` declares, nested in `parent` (0: none).
fn group(w: &mut Walker, call: Node, block: Node, parent: u32) {
    let at = line(call);
    let args = arguments(call);
    let described = match args.first() {
        Some(arg) if matches!(arg.kind(), "constant" | "scope_resolution") => {
            one_line(&w.text(*arg))
        }
        None => String::new(),
        Some(arg) if matches!(arg.kind(), "string" | "simple_symbol") => String::new(),
        Some(_) => "?".to_owned(),
    };
    // The first argument is what the group describes, whatever it is;
    // one after it that is no description, or a first one that is a
    // keyword, is metadata.
    let metadata = args.iter().enumerate().any(|(i, arg)| {
        matches!(arg.kind(), "pair" | "hash")
            || i > 0 && !matches!(arg.kind(), "string" | "constant" | "scope_resolution")
    });
    let mut scan = Scan::default();
    for statement in statements(block) {
        scan.statement(w, statement, false);
    }
    w.facts.push(Fact::Group {
        line: at,
        parent,
        described,
        scope: w.cref.iter().rev().cloned().collect(),
        metadata,
        opaque: scan.opaque,
        nests: scan.nests,
    });
    for (name, line, def) in scan.members {
        w.facts.push(Fact::Member {
            group: at,
            name,
            line,
            def,
        });
    }
    for module in scan.mixins {
        w.facts.push(Fact::GroupInclude { group: at, module });
    }
    w.group_mixins.extend(scan.mixin_calls);
    for (name, line) in scan.includes {
        w.facts.push(Fact::Include {
            group: at,
            name,
            line,
        });
    }
}

/// What a group's body defines, and whether it holds a statement the
/// rules do not read.
#[derive(Default)]
struct Scan {
    members: Vec<(String, u32, bool)>,
    opaque: bool,
    /// It makes a nested group the rules do not read (`it_behaves_like`).
    nests: bool,
    /// The shared groups it includes, by name, with the line.
    includes: Vec<(String, u32)>,
    /// The modules it includes, as written.
    mixins: Vec<String>,
    /// The `include` calls that include them.
    mixin_calls: Vec<usize>,
}

impl Scan {
    /// Reads one statement of a group's body; `nested` under an iterator,
    /// where a definition may run any number of times.
    fn statement(&mut self, w: &Walker, node: Node, nested: bool) {
        match node.kind() {
            "comment" | "empty_statement" | "singleton_method" => {}
            "method" if !nested => {
                if let Some(name) = w.field_text(node, "name") {
                    self.members.push((name, line(node), true));
                }
            }
            "assignment" | "operator_assignment" => {
                let left = node.child_by_field_name("left");
                if !left.is_some_and(|l| {
                    matches!(
                        l.kind(),
                        "identifier" | "constant" | "instance_variable" | "class_variable"
                    )
                }) {
                    self.opaque = true;
                }
            }
            "call" => self.call(w, node, nested),
            _ => self.opaque = true,
        }
    }

    fn call(&mut self, w: &Walker, call: Node, nested: bool) {
        let method = w.field_text(call, "method").unwrap_or_default();
        let method = method.as_str();
        let receiver = call.child_by_field_name("receiver");
        let has_block = call.child_by_field_name("block").is_some();
        if receiver.is_none() {
            if GROUPS.contains(&method)
                || EXAMPLES.contains(&method)
                || HOOKS.contains(&method)
                || matches!(
                    method,
                    "shared_examples" | "shared_examples_for" | "shared_context"
                )
            {
                return;
            }
            if matches!(method, "it_behaves_like" | "it_should_behave_like") {
                self.nests = true;
                return;
            }
            // `include M`: a step after the group's own methods.
            if method == "include" && !nested && !has_block {
                let args = arguments(call);
                if !args.is_empty()
                    && args
                        .iter()
                        .all(|arg| matches!(arg.kind(), "constant" | "scope_resolution"))
                {
                    self.mixins
                        .extend(args.iter().map(|arg| super::one_line(&w.text(*arg))));
                    self.mixin_calls.push(call.id());
                    return;
                }
            }
            // `include_context "n"`: the shared block runs here, in this
            // group's body.
            if matches!(method, "include_context" | "include_examples")
                && !nested
                && !has_block
                && let Some(name) = arguments(call).first().and_then(|arg| literal(w, *arg))
            {
                self.includes.push((name, line(call)));
                return;
            }
            if !nested
                && has_block
                && let Some(names) = member_names(w, call, method)
            {
                let at = line(call);
                self.members
                    .extend(names.into_iter().map(|name| (name, at, false)));
                return;
            }
            self.opaque = true;
            return;
        }
        // `%w[a b].each do |x| context x do`: the groups and examples of
        // an iterator's block are the group's; a definition there may run
        // any number of times.
        if receiver.is_some_and(|r| {
            w.text(r).trim_start_matches("::") == "RSpec" && GROUPS.contains(&method)
        }) {
            return;
        }
        if has_block
            && super::ITERATORS.contains(&method)
            && let Some(block) = call.child_by_field_name("block")
        {
            for statement in statements(block) {
                self.statement(w, statement, true);
            }
            return;
        }
        self.opaque = true;
    }
}

/// The names a `let`, `let!`, `subject` or `subject!` call defines: its
/// literal name (and `subject` for a named subject), or `subject` for an
/// unnamed one; `None` for a name the rules cannot read.
fn member_names(w: &Walker, call: Node, method: &str) -> Option<Vec<String>> {
    if !(LETS.contains(&method) || SUBJECTS.contains(&method)) {
        return None;
    }
    let args = arguments(call);
    let name = match args.as_slice() {
        [] if SUBJECTS.contains(&method) => return Some(vec!["subject".to_owned()]),
        [one] => literal(w, *one)?,
        _ => return None,
    };
    let mut names = vec![name];
    if SUBJECTS.contains(&method) {
        names.push("subject".to_owned());
    }
    Some(names)
}

/// A literal name: `:x` or `"x"`.
pub(super) fn literal(w: &Walker, node: Node) -> Option<String> {
    match node.kind() {
        "simple_symbol" => Some(w.text(node).trim_start_matches(':').to_owned()),
        "string" => {
            let mut cursor = node.walk();
            let parts: Vec<Node> = node.named_children(&mut cursor).collect();
            match parts.as_slice() {
                [part] if part.kind() == "string_content" => Some(w.text(*part)),
                _ => None,
            }
        }
        _ => None,
    }
    .filter(|name| !name.is_empty())
}

/// The arguments of `call`, comments left out.
pub(super) fn arguments(call: Node) -> Vec<Node> {
    let Some(args) = call.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let mut cursor = args.walk();
    args.named_children(&mut cursor)
        .filter(|arg| arg.kind() != "comment")
        .collect()
}

/// The statements of a block's body, comments included.
fn statements(block: Node) -> Vec<Node> {
    let mut cursor = block.walk();
    let body = block
        .named_children(&mut cursor)
        .find(|child| matches!(child.kind(), "body_statement" | "block_body"));
    let Some(body) = body else {
        return Vec::new();
    };
    let mut cursor = body.walk();
    body.named_children(&mut cursor).collect()
}

/// The last statement of a block's body, not a comment.
fn last_statement(block: Node) -> Option<Node> {
    statements(block)
        .into_iter()
        .rev()
        .find(|statement| statement.kind() != "comment")
}

/// Whether the `def` the walk is reading is written in a group's body,
/// directly or through blocks that keep `self`: [`enter`] made its frame
/// an example of that group.
pub(super) fn def_in_group(w: &Walker) -> bool {
    w.rspec && matches!(w.frames.last(), Some(Frame::Example(..)))
}

/// Whether a `def` of `name`, the one the walk is reading, may change
/// RSpec's DSL: one of its names, defined in spec code outside a group.
pub(super) fn redefines_dsl(w: &Walker, name: &str) -> bool {
    w.spec_code && DSL.contains(&name) && !def_in_group(w)
}

/// Whether `call`, a receiverless `require`, loads Minitest's spec DSL
/// in spec code, whose `describe` and `let` follow other rules.
pub(super) fn loads_minitest_spec(w: &Walker, call: Node, method: &str) -> bool {
    w.spec_code
        && method == "require"
        && arguments(call)
            .first()
            .and_then(|arg| literal(w, *arg))
            .is_some_and(|spec| matches!(spec.as_str(), "minitest/spec" | "minitest/autorun"))
}

/// Whether `call` turns the model off in spec code: a `prepend` or an
/// `include_context` sent to an object the rules cannot name
/// (`config.prepend M`, `config.include_context "n"`), which lands in
/// every group's own list.
pub(super) fn prepends_into_config(w: &Walker, call: Node, method: &str) -> bool {
    w.spec_code
        && matches!(method, "prepend" | "include_context")
        && call
            .child_by_field_name("receiver")
            .is_some_and(|r| r.kind() == "identifier")
}

/// Whether `call` is `RSpec.configure` or `RSpec.describe`.
pub(super) fn uses_rspec(w: &Walker, call: Node, method: &str) -> bool {
    matches!(method, "configure" | "describe")
        && call.child_by_field_name("receiver").is_some_and(|r| {
            matches!(r.kind(), "constant" | "scope_resolution")
                && w.text(r).trim_start_matches("::") == "RSpec"
        })
}

#[cfg(test)]
mod audited_contract_tests {
    use super::super::{PARSE_BUDGET, parse};
    use super::*;

    fn call(source: &str, check: impl for<'a> FnOnce(&mut Walker<'a>, Node<'a>)) {
        let tree = parse(source.as_bytes(), PARSE_BUDGET).unwrap();
        let mut walker = Walker::new(source.as_bytes());
        check(&mut walker, tree.root_node().named_child(0).unwrap());
    }

    #[test]
    fn spec_detection_should_distinguish_spec_files_from_support_and_normal_code() {
        for (path, file, code) in [
            ("x_spec.rb", true, true),
            ("spec/support/x.rb", false, true),
            ("lib/x.rb", false, false),
            ("specification/x.rb", false, false),
        ] {
            assert_eq!(spec_file(path), file, "{path}");
            assert_eq!(spec_code(path), code, "{path}");
        }
    }

    #[test]
    fn example_markers_should_require_the_rspec_model_and_the_nearest_example_frame() {
        let mut walker = Walker::new(b"");
        walker.frames.push(Frame::Example(12, true));
        assert_eq!(walker.example_group(), None);
        walker.rspec = true;
        assert_eq!(walker.example_group(), Some((12, true)));
        walker.frames.push(Frame::Other);
        assert_eq!(walker.example_group(), None);
    }

    #[test]
    fn frame_boundaries_should_preserve_lambda_self_but_stop_at_classes_and_singletons() {
        for (source, expected) in [
            ("class X; end", Frame::Top),
            ("module X; end", Frame::Top),
            ("class << self; end", Frame::Other),
            ("def self.run; end", Frame::Other),
            ("-> { work }", Frame::Keep),
        ] {
            call(source, |walker, node| {
                walker.frames.push(Frame::Example(1, false));
                assert!(enter(walker, node, node.parent()), "{source}");
                assert_eq!(walker.frames.last(), Some(&expected), "{source}");
                leave(walker, node);
                assert_eq!(walker.frames, [Frame::Example(1, false)]);
            });
        }
    }

    #[test]
    fn group_members_should_distinguish_subject_defaults_named_subjects_and_lets() {
        for (source, method, expected) in [
            ("subject", "subject", Some(vec!["subject"])),
            ("let", "let", None),
            ("let(:item)", "let", Some(vec!["item"])),
            ("subject(:item)", "subject", Some(vec!["item", "subject"])),
            ("subject(dynamic)", "subject", None),
        ] {
            call(source, |walker, node| {
                let names = member_names(walker, node, method);
                assert_eq!(
                    names
                        .as_ref()
                        .map(|v| v.iter().map(String::as_str).collect::<Vec<_>>()),
                    expected,
                    "{source}"
                );
            });
        }
    }

    #[test]
    fn interpolated_literals_should_never_be_recorded_as_static_names() {
        for (source, expected) in [
            (":name", Some("name")),
            ("\"name\"", Some("name")),
            ("\"#{name}\"", None),
            ("\"\"", None),
            ("name", None),
        ] {
            call(source, |walker, node| {
                assert_eq!(literal(walker, node).as_deref(), expected, "{source}")
            });
        }
    }

    #[test]
    fn group_scanning_should_keep_safe_statements_and_reject_dynamic_definitions() {
        for (source, nested, opaque, members) in [
            ("# comment", false, false, 0),
            ("def self.extra; end", false, false, 0),
            ("x = 1", false, false, 0),
            ("@x ||= 1", false, false, 0),
            ("obj.x = 1", false, true, 0),
            ("def extra; end", false, false, 1),
            ("def extra; end", true, true, 0),
            ("include Known", false, false, 0),
            ("include Known", true, true, 0),
            ("include Known do; end", false, true, 0),
            ("Other.describe 'x' do; end", false, true, 0),
            ("RSpec.describe 'x' do; end", false, false, 0),
            ("RSpec.unread 'x'", false, true, 0),
        ] {
            call(source, |walker, node| {
                let mut scan = Scan::default();
                scan.statement(walker, node, nested);
                assert_eq!(
                    (scan.opaque, scan.members.len()),
                    (opaque, members),
                    "{source} nested={nested}"
                );
            });
        }
    }

    #[test]
    fn model_switches_should_require_the_exact_host_method_and_spec_context() {
        for (source, method, context, minitest, prepend, rspec) in [
            (
                "require 'minitest/spec'",
                "require",
                true,
                true,
                false,
                false,
            ),
            ("require 'minitest/spec'", "load", true, false, false, false),
            (
                "require 'minitest/spec'",
                "require",
                false,
                false,
                false,
                false,
            ),
            (
                "config.prepend Helpers",
                "prepend",
                true,
                false,
                true,
                false,
            ),
            (
                "config.prepend Helpers",
                "include",
                true,
                false,
                false,
                false,
            ),
            (
                "config.prepend Helpers",
                "prepend",
                false,
                false,
                false,
                false,
            ),
            ("RSpec.describe Item", "describe", true, false, false, true),
            ("Other.describe Item", "describe", true, false, false, false),
            ("RSpec.unread Item", "unread", true, false, false, false),
        ] {
            call(source, |walker, node| {
                walker.spec_code = context;
                assert_eq!(
                    loads_minitest_spec(walker, node, method),
                    minitest,
                    "{source}"
                );
                assert_eq!(
                    prepends_into_config(walker, node, method),
                    prepend,
                    "{source}"
                );
                assert_eq!(uses_rspec(walker, node, method), rspec, "{source}");
            });
        }
    }

    #[test]
    fn group_metadata_should_distinguish_descriptions_from_metadata_and_dynamic_described_classes()
    {
        for (source, described, metadata) in [
            ("describe Item do; end", "Item", false),
            ("describe 'item' do; end", "", false),
            ("describe :item do; end", "", false),
            ("describe item do; end", "?", false),
            ("describe :tag do; end", "", false),
            ("describe Item, :tag do; end", "Item", true),
            ("describe Item, 'description' do; end", "Item", false),
        ] {
            call(source, |walker, node| {
                group(walker, node, node.child_by_field_name("block").unwrap(), 0);
                assert!(
                    matches!(&walker.facts[0],Fact::Group {described:actual,metadata:actual_meta,..} if actual==described && *actual_meta==metadata),
                    "{source}: {:?}",
                    walker.facts
                );
            });
        }
    }

    #[test]
    fn shared_groups_should_be_read_only_with_a_literal_name_at_the_top_level() {
        for (source, around, expected) in [
            (
                "shared_examples 'shared' do; end",
                Frame::Top,
                Frame::Group(1),
            ),
            ("shared_examples dynamic do; end", Frame::Top, Frame::Other),
            (
                "shared_examples 'shared' do; end",
                Frame::Group(5),
                Frame::Other,
            ),
        ] {
            call(source, |walker, node| {
                assert_eq!(
                    shared(
                        walker,
                        node,
                        node.child_by_field_name("block").unwrap(),
                        around,
                        false
                    ),
                    expected
                );
            });
        }
    }
}
