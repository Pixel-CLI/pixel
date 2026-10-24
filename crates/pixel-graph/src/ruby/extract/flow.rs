// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Whether code runs where it is written: a branch, a loop, a `rescue`, a
//! block an iterator may run no time or more than once (D06, M2), or one
//! that runs to its end once at least (R5-09), a body Ruby runs once
//! that holds an early exit (R6-10, R7-04, R8-04); whether a method's
//! blocks run on the object they are written for (R5-11); and what a bare
//! `it` reads.

use tree_sitter::Node;

use super::{BlockKind, Walker};

/// Whether what `node` holds may run or not, or more than once: a
/// branch, a loop, a `rescue` and the body it guards (any call there may
/// raise and skip the rest, R6-09), a boolean operator, a block (an
/// iterator may run it any number of times, `included do` once per
/// includer) but the blocks Ruby runs once, where they are (D06).
pub(super) fn is_branch(w: &Walker, node: Node, block: Option<BlockKind>) -> bool {
    match node.kind() {
        "if" | "unless" | "while" | "until" | "for" | "case" | "case_match" | "conditional"
        | "if_modifier" | "unless_modifier" | "while_modifier" | "until_modifier"
        | "rescue_modifier" | "rescue" => true,
        "begin" | "body_statement" => {
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .any(|child| child.kind() == "rescue")
        }
        "binary" => node
            .child_by_field_name("operator")
            .is_some_and(|op| matches!(w.text(op).as_str(), "and" | "or" | "&&" | "||")),
        // An iterator over a literal that is not empty runs its block.
        _ if block == Some(BlockKind::Keeps) => !iterates_once_at_least(w, node),
        _ => matches!(
            block,
            Some(BlockKind::Included { .. } | BlockKind::Forwards | BlockKind::Foreign)
        ),
    }
}

/// Whether the block `node` is passed to an iterator over a literal Ruby
/// runs it on at least once, to its end: `[1].each`, `%w[a].map`, `2.times`
/// (M2). Not with a splat among the elements, which may be empty, nor with
/// a jump or a raise in the block, which may leave it before a `def`
/// (R5-09).
fn iterates_once_at_least(w: &Walker, node: Node) -> bool {
    let Some(receiver) = node
        .parent()
        .filter(|call| call.kind() == "call")
        .and_then(|call| call.child_by_field_name("receiver"))
    else {
        return false;
    };
    let runs = match receiver.kind() {
        "array" | "string_array" | "symbol_array" => {
            let mut cursor = receiver.walk();
            let elements: Vec<Node> = receiver
                .named_children(&mut cursor)
                .filter(|element| element.kind() != "comment")
                .collect();
            !elements.is_empty()
                && elements.iter().all(|element| {
                    !matches!(
                        element.kind(),
                        "splat_argument" | "hash_splat_argument" | "block_argument"
                    )
                })
        }
        "integer" => w
            .text(receiver)
            .replace('_', "")
            .parse::<u64>()
            .is_ok_and(|n| n > 0),
        _ => false,
    };
    runs && !leaves_early(w, node)
}

/// Whether `node`, a block or a body Ruby runs once, holds a statement
/// that may leave it before its end ([`exits`]).
pub(super) fn leaves_early(w: &Walker, node: Node) -> bool {
    w.exits.contains(&node.id())
}

/// The nodes of the tree `root` that hold a statement that may leave them
/// before their end: `break`, `next`, `redo`, `retry`, `return`, or a call
/// of `raise`, `fail`, `throw`, `exit`, `exit!` or `abort`, at any depth
/// but in the body of a method they define, which runs when called
/// (R5-09, R8-04). Each such statement marks the nodes around it as far as
/// it may leave: `next`, `redo` and `retry` their block, lambda or loop;
/// `break` its lambda or loop (out of a block it may raise
/// `LocalJumpError`); `return` its lambda; the others the method. A loop
/// left is no node left early: it is a branch already. One pass over the
/// tree, each node marked once per reach.
pub(super) fn exits(w: &Walker, root: Node) -> rustc_hash::FxHashSet<usize> {
    // Node → how far the statements marking it leave, the farthest kept:
    // each reach stops wherever a nearer one does.
    let mut reach: rustc_hash::FxHashMap<usize, u8> = rustc_hash::FxHashMap::default();
    // The nodes from the root to the one being read, with their kinds.
    let mut path: Vec<(usize, &str)> = Vec::new();
    let mut stack = vec![(root, 0)];
    while let Some((at, depth)) = stack.pop() {
        path.truncate(depth);
        let kind = at.kind();
        path.push((at.id(), kind));
        let leaves = match kind {
            "next" | "redo" | "retry" => Some(0),
            "break" => Some(1),
            "return" => Some(2),
            "identifier"
                if at.byte_range().len() <= "raise".len()
                    && matches!(
                        w.text(at).as_str(),
                        "raise" | "fail" | "throw" | "exit" | "exit!" | "abort"
                    ) =>
            {
                Some(3)
            }
            _ => None,
        };
        if let Some(leaves) = leaves {
            for &(id, around) in path.iter().rev() {
                let ends = match around {
                    "method" | "singleton_method" => break,
                    "while" | "until" | "for" | "while_modifier" | "until_modifier"
                        if leaves <= 1 =>
                    {
                        break;
                    }
                    "block" | "do_block" => leaves == 0,
                    "lambda" => leaves <= 2,
                    _ => false,
                };
                // Marked as far already: the rest is too.
                if reach.get(&id).is_some_and(|&marked| marked >= leaves) {
                    break;
                }
                reach.insert(id, leaves);
                if ends {
                    break;
                }
            }
        }
        let mut cursor = at.walk();
        stack.extend(at.children(&mut cursor).map(|child| (child, depth + 1)));
    }
    reach.into_keys().collect()
}

/// The methods that run a block with another `self` (D08). A method
/// outside this list that evaluates its block elsewhere (a gem's builder,
/// Prawn's `generate`, Nokogiri's `Builder.new`, or one of the
/// application's in another file) leaves the caller's `self` where Ruby
/// changes it: an accepted risk (R5-13); one of the same file that passes
/// its block on to these is read ([`rebinders`], R6-17).
pub(super) const REBINDERS: &[&str] = &[
    "instance_exec",
    "instance_eval",
    "class_exec",
    "class_eval",
    "module_exec",
    "module_eval",
    "define_method",
    "define_singleton_method",
];

/// Whether the body of the method `node` passes a block on (`&header`,
/// `&block`, `&@cb`, `&`, `&-> { }`, `&proc { }`, `&parts[:h]`, `...`) to
/// a method that runs it with another `self` ([`REBINDERS`], or one of the
/// file's [`rebinders`]), called or sent (`send(:instance_exec,
/// &header)`): any block, lambda or proc written in that body may be the
/// one, so none of them runs on the method's object for sure (R5-11,
/// R6-12, R7-12). A symbol's or a method object's block (`&:name`,
/// `&method(:helper)`) is no block of the body.
pub(super) fn passes_block_on(w: &Walker, node: Node) -> bool {
    passed_to(w, node).iter().any(|method| rebinds(w, method))
}

/// The methods that run another by its name: what they run, when the name
/// is no literal one, may be any method, one that rebinds `self` among
/// them (R8-07).
const SENDERS: &[&str] = &["send", "__send__", "public_send"];

/// Whether the method `method`, as [`runs`] gives it, may run a block
/// with another `self`: one of [`REBINDERS`], one of the file's
/// [`rebinders`], or a sender whose name the rules cannot read (R8-07).
pub(super) fn rebinds(w: &Walker, method: &str) -> bool {
    REBINDERS.contains(&method) || SENDERS.contains(&method) || w.rebinders.contains(method)
}

/// The methods the body of the method `node` passes a block on to
/// ([`passes_block_on`]), as each call runs them ([`runs`]).
fn passed_to(w: &Walker, node: Node) -> Vec<String> {
    let mut methods = Vec::new();
    let mut stack: Vec<Node> = node.child_by_field_name("body").into_iter().collect();
    while let Some(at) = stack.pop() {
        if matches!(
            at.kind(),
            "method" | "singleton_method" | "class" | "module"
        ) {
            continue;
        }
        if at.kind() == "call"
            && at.child_by_field_name("arguments").is_some_and(|args| {
                let mut cursor = args.walk();
                args.named_children(&mut cursor).any(|arg| {
                    arg.kind() == "forward_argument"
                        || arg.kind() == "block_argument"
                            && arg.named_child(0).is_none_or(|held| {
                                held.kind() != "simple_symbol"
                                    && !(held.kind() == "call"
                                        && w.field_text(held, "method").as_deref()
                                            == Some("method"))
                            })
                })
            })
            && let Some(method) = runs(w, at)
        {
            methods.push(method);
        }
        let mut cursor = at.walk();
        stack.extend(at.children(&mut cursor));
    }
    methods
}

/// The names of the methods of the file `root` whose body passes a block
/// on to rebind `self` ([`passes_block_on`]), directly or through another
/// such method of the file (`def outer(&b); on_pdf(&b); end`): a block
/// given to one of them (`def on_pdf(&b); pdf.instance_exec(&b); end`,
/// then `on_pdf { title }`) may run with another `self` too (R6-17,
/// R7-12). A method `define_method` defines with a literal name is read
/// as a `def` is (R8-08). One defined in another file is not seen: an
/// accepted risk, as for a gem's (R5-13).
pub(super) fn rebinders(w: &Walker, root: Node) -> rustc_hash::FxHashSet<String> {
    let mut names = rustc_hash::FxHashSet::default();
    // Each method a body passes a block on to → the methods whose body
    // does: read once, then followed back from the rebinding ones.
    let mut callers: rustc_hash::FxHashMap<String, Vec<String>> = rustc_hash::FxHashMap::default();
    let mut next = Vec::new();
    let mut stack = vec![root];
    while let Some(at) = stack.pop() {
        // A method `define_method(:name) { |&b| }` defines reads as a
        // `def` does.
        let defined = || {
            let method = w.field_text(at, "method")?;
            let first = at.child_by_field_name("arguments")?.named_child(0)?;
            matches!(method.as_str(), "define_method" | "define_singleton_method")
                .then(|| super::dsl::literal_name(w, first))?
        };
        let name = match at.kind() {
            "method" | "singleton_method" => w.field_text(at, "name"),
            "call" => defined(),
            _ => None,
        };
        let body = match at.kind() {
            "call" => at.child_by_field_name("block"),
            _ => Some(at),
        };
        if let (Some(name), Some(body)) = (name, body) {
            for method in passed_to(w, body) {
                if REBINDERS.contains(&method.as_str()) || SENDERS.contains(&method.as_str()) {
                    next.push(name.clone());
                }
                callers.entry(method).or_default().push(name.clone());
            }
        }
        let mut cursor = at.walk();
        stack.extend(at.children(&mut cursor));
    }
    while let Some(name) = next.pop() {
        if let Some(those) = callers.get(&name).filter(|_| !names.contains(&name)) {
            next.extend(those.iter().cloned());
        }
        names.insert(name);
    }
    names
}

/// Whether the lambda `node` (`kind`), or the block of `proc`, `lambda`,
/// `Proc.new`, `Kernel.proc` or `Kernel.lambda` whose parent is `parent`,
/// may be run with another `self`: whatever is given that value may
/// `instance_exec` it. It keeps the method's `self` only called right away
/// (`-> { }.call`, `.()`), assigned to a local that is only called, or
/// left unused before the end of a body; passed as an argument at any
/// depth (in an array, a hash, parentheses, a splat), stored anywhere
/// else, or returned, it does not (R7-12, R8-08).
pub(super) fn escapes(w: &Walker, kind: &str, node: Node, parent: Option<Node>) -> bool {
    let value = if kind == "lambda" {
        node
    } else {
        let Some(call) = parent.filter(|call| call.kind() == "call") else {
            return false;
        };
        let receiver = w.field_text(call, "receiver");
        let receiver = receiver.as_deref().map(|r| r.trim_start_matches("::"));
        let maker = matches!(
            (receiver, w.field_text(call, "method").as_deref()),
            (None | Some("Kernel"), Some("proc" | "lambda")) | (Some("Proc"), Some("new"))
        );
        if !maker {
            return false;
        }
        call
    };
    let mut at = value;
    while let Some(outer) = at
        .parent()
        .filter(|p| p.kind() == "parenthesized_statements")
    {
        at = outer;
    }
    let Some(around) = at.parent() else {
        return false;
    };
    match around.kind() {
        // Called right away.
        "call" => {
            around.child_by_field_name("receiver").map(|r| r.id()) != Some(at.id())
                || !matches!(
                    w.field_text(around, "method").as_deref(),
                    None | Some("call")
                )
        }
        "assignment" => {
            let local = around
                .child_by_field_name("left")
                .filter(|left| left.kind() == "identifier");
            !local.is_some_and(|local| only_called(w, local))
        }
        // A statement, unused unless it is the body's value.
        "body_statement" | "block_body" | "program" => {
            let mut cursor = around.walk();
            let last = around
                .named_children(&mut cursor)
                .filter(|n| n.kind() != "comment")
                .last();
            last.is_some_and(|last| last.id() == at.id())
        }
        _ => true,
    }
}

/// Whether every read of the local `local` assigns, in the method or file
/// around it, is the receiver of `call` or `.()`. The names read otherwise
/// in that body are counted in one walk, kept for its other locals
/// (R9-01); the assignment's own left side is one of them.
fn only_called(w: &Walker, local: Node) -> bool {
    let mut scope = local;
    while let Some(outer) = scope.parent() {
        scope = outer;
        if matches!(scope.kind(), "method" | "singleton_method" | "program") {
            break;
        }
    }
    let mut bodies = w.uncalled.borrow_mut();
    let uncalled = bodies.entry(scope.id()).or_insert_with(|| {
        let mut uncalled = rustc_hash::FxHashMap::default();
        // The receivers of `call` and `.()`, met before they are.
        let mut called = rustc_hash::FxHashSet::default();
        let mut stack = vec![scope];
        while let Some(at) = stack.pop() {
            match at.kind() {
                "call" if matches!(w.field_text(at, "method").as_deref(), None | Some("call")) => {
                    called.extend(at.child_by_field_name("receiver").map(|r| r.id()));
                }
                "identifier" if !called.contains(&at.id()) => {
                    *uncalled.entry(w.text(at)).or_insert(0) += 1;
                }
                _ => {}
            }
            let mut cursor = at.walk();
            stack.extend(at.children(&mut cursor));
        }
        uncalled
    });
    uncalled.get(&w.text(local)).is_none_or(|&reads| reads <= 1)
}

/// The names read in a method or file body other than as the receiver of
/// `call` or `.()`, each with how often, by the body's node ([`escapes`]):
/// filled the first time one of its locals is asked about.
pub(super) type Uncalled =
    std::cell::RefCell<rustc_hash::FxHashMap<usize, rustc_hash::FxHashMap<String, usize>>>;

/// The method `call` runs: its own, or the one it sends by a literal name
/// (`send(:instance_exec)`, `__send__`, `public_send`), or the one a
/// method object it calls names (`method(:instance_exec).call`); the
/// sender itself for a name the rules cannot read (`send(mode)`,
/// `send(*names)`), which may be any method ([`rebinds`], R8-07).
pub(super) fn runs(w: &Walker, call: Node) -> Option<String> {
    let mut method = w.field_text(call, "method")?;
    let mut sender = call;
    if method == "call"
        && let Some(object) = call
            .child_by_field_name("receiver")
            .filter(|object| object.kind() == "call")
        && matches!(
            w.field_text(object, "method").as_deref(),
            Some("method" | "public_method")
        )
    {
        method = "send".to_owned();
        sender = object;
    }
    if !SENDERS.contains(&method.as_str()) {
        return Some(method);
    }
    let sent = sender
        .child_by_field_name("arguments")
        .and_then(|args| args.named_child(0))
        .and_then(|first| super::dsl::literal_name(w, first));
    Some(sent.unwrap_or(method))
}

/// Whether the bare name `it` at `node` may be the parameter of the block
/// around it: Ruby 3.4 reads `it`, in a block that declares no parameter
/// and uses no numbered one (`_1`), as the block's first argument, where
/// Ruby 3.3 calls a method `it`. Which Ruby runs the file is unknown, so
/// such an `it` is never a call the rules may resolve.
pub(super) fn may_be_block_parameter(w: &Walker, node: Node) -> bool {
    let mut at = node;
    let block = loop {
        let Some(parent) = at.parent() else {
            return false;
        };
        match parent.kind() {
            "block" | "do_block" | "lambda" => break parent,
            "method" | "singleton_method" | "class" | "module" | "singleton_class" => return false,
            _ => at = parent,
        }
    };
    if block.child_by_field_name("parameters").is_some() {
        return false;
    }
    let mut stack: Vec<Node> = block.child_by_field_name("body").into_iter().collect();
    while let Some(at) = stack.pop() {
        if matches!(at.kind(), "block" | "do_block" | "lambda") {
            continue;
        }
        if at.kind() == "identifier" && is_numbered(&w.text(at)) {
            return false;
        }
        let mut cursor = at.walk();
        stack.extend(at.children(&mut cursor));
    }
    true
}

/// Whether `name` is a numbered block parameter, `_1` to `_9`.
fn is_numbered(name: &str) -> bool {
    matches!(name.as_bytes(), [b'_', b'1'..=b'9'])
}
