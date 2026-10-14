// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Rails DSL rules: callbacks, `send`, associations, mixins, generated methods.
//!
//! Carried from v1's `crates/pixel-graph/src/extract/ruby_callbacks.rs`,
//! `ruby_associations.rs`, `ruby_mixins.rs` and `ruby_generated.rs`. Each
//! rule reads literals only: a symbol, a string, a constant path. Anything
//! computed (a variable, an interpolation, a splat) names nothing, and the
//! rule records nothing rather than a guess.

use std::collections::HashMap;

use mvp_model::{EdgeKind, SymbolKind};
use tree_sitter::Node;

use super::extract::{BoundKind, Defined, Defs, Landing, MixinBody, This, Walker, line, one_line};
use super::routes::{camelize_path, singularize};
use super::unread;
use super::{ALIAS, ASSOCIATION, CONDITIONAL, DELEGATE, Fact, MIXIN};
use crate::lang::FILE_OWNER;

/// Closed Rails callback list: positional symbols name instance methods,
/// skip declarations included (a reference does not assert that the
/// callback runs). v1 `ruby_callbacks.rs:33-78`.
/// <https://api.rubyonrails.org/classes/AbstractController/Callbacks/ClassMethods.html>
/// <https://api.rubyonrails.org/classes/ActiveRecord/Callbacks.html>
/// <https://api.rubyonrails.org/classes/ActiveRecord/Transactions/ClassMethods.html>
/// <https://api.rubyonrails.org/classes/ActiveJob/Callbacks.html>
pub const CALLBACK_METHODS: &[&str] = &[
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

/// The Active Record association macros (v1 `ruby_associations.rs:23`).
const MACROS: &[&str] = &[
    "belongs_to",
    "has_one",
    "has_many",
    "has_and_belongs_to_many",
];

/// The calls that declare an ancestor (v1 `ruby_mixins.rs:37`).
const MIXIN_METHODS: &[&str] = &["include", "prepend", "extend"];

/// The literal method symbols a callback declaration or a `send` names
/// (v1 `walk_symbol_arguments`). A callback is read in a class or module
/// body only, receiver-less: its positional symbols and the symbols of its
/// `if:` and `unless:` options. A `send` or `public_send`, on no receiver or
/// `self`, in a class, module or method body, names its first argument
/// only: later symbols are data.
/// <https://docs.ruby-lang.org/en/master/Object.html#method-i-send>
pub(super) fn symbol_arguments(w: &mut Walker, call: Node, method: &str) {
    let callback = CALLBACK_METHODS.contains(&method);
    if !callback && !matches!(method, "send" | "public_send") {
        return;
    }
    let receiver = w.field_text(call, "receiver");
    let scope = w.nearest();
    let allowed = if callback {
        receiver.is_none() && matches!(scope, Some(BoundKind::Class | BoundKind::Module))
    } else {
        receiver.as_deref().is_none_or(|r| r == "self")
            && matches!(
                scope,
                Some(
                    BoundKind::Class
                        | BoundKind::Module
                        | BoundKind::Method
                        | BoundKind::SingletonMethod
                )
            )
    };
    let Some(args) = call.child_by_field_name("arguments").filter(|_| allowed) else {
        return;
    };
    if !callback {
        if let Some(first) = args.named_child(0) {
            push_symbol(w, first, method, call);
        }
        return;
    }
    let mut cursor = args.walk();
    let all: Vec<Node> = args.children(&mut cursor).collect();
    for arg in all {
        if arg.kind() != "pair" {
            push_symbol(w, arg, method, call);
        } else if let Some(key) = w.field_text(arg, "key")
            && matches!(key.trim_start_matches(':'), "if" | "unless")
            && let Some(value) = arg.child_by_field_name("value")
        {
            push_symbol(w, value, method, call);
        }
    }
}

/// A simple symbol names a method; a quoted or dynamic one is left
/// unguessed.
fn push_symbol(w: &mut Walker, symbol: Node, method: &str, call: Node) {
    if symbol.kind() == "simple_symbol" {
        let name = w.text(symbol)[1..].to_owned();
        w.push_reference(&name, method, call);
    }
}

/// An Active Record association in a class body references the model class
/// it names, never a method (v1 `walk_association`): the literal
/// `class_name:`, else the association name Rails classifies
/// (`has_many :line_items` → `LineItem`). A `polymorphic: true`
/// association, or a `through:` one without `class_name:`, has no single
/// target; a computed name or `class_name:` neither.
/// <https://api.rubyonrails.org/classes/ActiveRecord/Associations/ClassMethods.html>
pub(super) fn association(w: &mut Walker, call: Node, method: &str) {
    if !MACROS.contains(&method)
        || call.child_by_field_name("receiver").is_some()
        || w.nearest() != Some(BoundKind::Class)
    {
        return;
    }
    let Some(args) = call.child_by_field_name("arguments") else {
        return;
    };
    let mut cursor = args.walk();
    let children: Vec<Node> = args.named_children(&mut cursor).collect();
    let Some(name) = children.first().and_then(|n| association_name(w, *n)) else {
        return;
    };
    let option = |key: &str| {
        children.iter().find_map(|arg| {
            (arg.kind() == "pair"
                && w.field_text(*arg, "key")
                    .is_some_and(|k| k.trim_start_matches(':').trim_end_matches(':') == key))
            .then(|| arg.child_by_field_name("value"))
            .flatten()
        })
    };
    if option("polymorphic").is_some_and(|v| v.kind() == "true") {
        return;
    }
    let class_path = match option("class_name") {
        Some(value) => match association_name(w, value) {
            Some(class_name) => class_name,
            None => return,
        },
        None if option("through").is_some() => return,
        None if matches!(method, "has_many" | "has_and_belongs_to_many") => {
            camelize_path(&singularize(&name))
        }
        None => camelize_path(&name),
    };
    if class_path.rsplit("::").next().is_none_or(str::is_empty) {
        return;
    }
    let context = w.context(&class_path, &format!("{ASSOCIATION}{method}"));
    w.push_site(
        EdgeKind::Association,
        line(call),
        None,
        class_path,
        &context,
    );
}

/// A literal symbol (`:a`, `:"a"`) or string without interpolation, as an
/// association reads it: an escape sequence is skipped, as v1 does.
fn association_name(w: &Walker, node: Node) -> Option<String> {
    let text = match node.kind() {
        "simple_symbol" => w.text(node).get(1..)?.to_owned(),
        "string" | "delimited_symbol" => {
            let mut out = String::new();
            let mut cursor = node.walk();
            for part in node.children(&mut cursor) {
                match part.kind() {
                    "string_content" => out.push_str(&w.text(part)),
                    "interpolation" => return None,
                    _ => {}
                }
            }
            out
        }
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

/// A receiver-less (or `self.`) `include`, `prepend` or `extend` in a class
/// or module body: one ancestor per argument, kind prefixed `included:` in a
/// concern's `included do` block and `prepended:` in its `prepended do`,
/// whose bodies run on the includer (v1 `walk_mixin_call`), `singleton:` in
/// the owner's `class << self`, where `include` and `prepend` act on its
/// singleton class (D35; an `extend` there acts on the singleton class's
/// own singleton, which no lookup of the owner reads). The same sent
/// through `send`, `__send__` or `public_send` (R12), and sent to
/// `singleton_class` in the body, which acts on the singleton class as
/// `class << self` does (R3; in a concern's block, an `include` there is
/// the includer's `extend`). Ruby inserts the arguments of one call last
/// first (`include A, B` looks `A` up first), so they are recorded last
/// first (D10). A non-constant argument (`include(plugin)`) is recorded
/// with an empty name: an ancestor the resolver cannot name. In a block
/// that may run on the class, a mixin may give it any name.
/// <https://docs.ruby-lang.org/en/master/Module.html#method-i-include>
/// <https://api.rubyonrails.org/classes/ActiveSupport/Concern.html>
pub(super) fn mixin_call(w: &mut Walker, call: Node, method: &str) {
    let Some((op, args)) = sent(w, call, method).filter(|(op, _)| MIXIN_METHODS.contains(op))
    else {
        return;
    };
    let receiver = w.field_text(call, "receiver");
    let on_singleton = match receiver.as_deref() {
        None | Some("self") => false,
        Some("singleton_class" | "self.singleton_class") => true,
        Some(_) => return,
    };
    // A concern's block runs on the includer, where its mixins act. A
    // block evaluated elsewhere (`K.class_eval { prepend M }`, `class <<
    // K`) mixes into an object whose lookup the rules do not replay
    // (R6-06).
    if w.foreign() && !matches!(w.locals.landing(), Landing::Includers { .. }) {
        w.landed(Defined::Any, None);
        if w.unplaced() {
            w.spread(&args);
        } else {
            mixed(w, &args);
        }
        match w.locals.landing() {
            Landing::Constant { path, scope, .. } => {
                opaque(w, &path, scope, op, on_singleton, &args);
            }
            Landing::SingletonOf { path, scope, .. } => {
                opaque(w, &path, scope.to_vec(), op, true, &args);
            }
            _ => {}
        }
        return;
    }
    // On an object no lookup can name exactly (R5-04), but the hook's own
    // object, which its replay reads (D14).
    if w.unplaced() {
        if !w
            .hook_base()
            .is_some_and(|base| in_hook_eval(w, call, base))
        {
            w.spread(&args);
        }
        return;
    }
    // In a method body, a class macro: it mixes into whatever object the
    // method runs on (B7); an instance method's `extend`, into one
    // object's singleton class (R5-04).
    if in_method_body(w) {
        let side = if op == "extend" || on_singleton {
            '.'
        } else {
            '#'
        };
        // A hook the module runs (`extended` prepending to `base`, a
        // concern's `ClassMethods`) may act on the other side of a class
        // macro's caller too, whether the macro is a method of the class
        // or of a module it extends (`extend Setup`, then `K.setup!`,
        // R7-08); only a module has that other side, which no object's
        // singleton class is.
        let other = if side == '.' { '#' } else { '.' };
        if let Some((owner, method)) = w.enclosing_method() {
            w.facts.push(Fact::Macro {
                owner,
                method,
                side: other,
                name: None,
            });
        }
        if side == '.'
            && w.enclosing_method()
                .is_some_and(|(_, method)| method == '#')
        {
            w.spread(&args);
        } else {
            w.macro_defines(side, None);
            // Into whatever object calls the macro, whose lookup the rules
            // do not replay (R6-06).
            mixed(w, &args);
        }
        return;
    }
    // At the top level, `include` mixes into `Object`, so into every
    // object, and `extend` into `main` alone, one object the rules cannot
    // name (R6-06).
    let Some(owner) = w.owners.last().map(|owner| owner.def) else {
        if op == "include" && !on_singleton && w.nearest().is_none() {
            mixed(w, &args);
        } else {
            w.spread(&args);
        }
        return;
    };
    let (prefix, op) = match (w.mixin_body(), on_singleton) {
        (Some(MixinBody::Owner), false) => ("", op),
        (Some(MixinBody::Included), false) => ("included:", op),
        (Some(MixinBody::Prepended), false) => ("prepended:", op),
        (Some(MixinBody::Singleton), false) if op != "extend" => ("singleton:", op),
        (Some(MixinBody::Owner), true) if op != "extend" => ("singleton:", op),
        (Some(MixinBody::Included), true) if op == "include" => ("included:", "extend"),
        (Some(MixinBody::Prepended), true) if op == "include" => ("prepended:", "extend"),
        _ => return,
    };
    let kind = format!("{MIXIN}{prefix}{op}");
    // `extend self` in a module body extends the module with itself.
    let itself = (op == "extend" && prefix.is_empty() && w.nearest() == Some(BoundKind::Module))
        .then(|| w.owners.last().map(|o| format!("::{}", o.qualified)))
        .flatten();
    // Under a condition, a loop or a block that may run no time, the
    // mixin may not run (R5-07).
    let (branches, around) = w.branches_in_body();
    for arg in args {
        // `MODULES.each { |m| include m }`: each module in turn, the
        // parameter as written naming it; the `each` runs once per module.
        if let Some(paths) = looped(w, call, arg) {
            let written = w.text(arg);
            for path in paths {
                let mut context = w.context(&path, &kind);
                if branches > 1 || around {
                    context.extra.push(CONDITIONAL.to_owned());
                }
                w.push_site(
                    EdgeKind::Reference,
                    line(call),
                    Some(owner),
                    written.clone(),
                    &context,
                );
            }
            continue;
        }
        let name = itself.clone().filter(|_| arg.kind() == "self");
        push_named_ancestor(
            w,
            arg,
            line(call),
            owner,
            &kind,
            name,
            branches > 0 || around,
        );
    }
}

/// The constants `arg` takes in turn when it is the one parameter of an
/// `each` block whose only statement is `call`, over a constant holding a
/// literal array of constants written in the same lexical scopes and not
/// read since (`MODULES = [A, B]` then `MODULES.each do |m| include m
/// end`); `None` otherwise.
fn looped(w: &Walker, call: Node, arg: Node) -> Option<Vec<String>> {
    if arg.kind() != "identifier" {
        return None;
    }
    let body = call.parent()?;
    let block = body
        .parent()
        .filter(|b| matches!(b.kind(), "block" | "do_block"))?;
    let mut cursor = body.walk();
    let statements: Vec<Node> = body
        .named_children(&mut cursor)
        .filter(|n| n.kind() != "comment")
        .collect();
    if statements.len() != 1 || statements[0].id() != call.id() {
        return None;
    }
    let parameters = block.child_by_field_name("parameters")?;
    let mut cursor = parameters.walk();
    let parameters: Vec<Node> = parameters.named_children(&mut cursor).collect();
    match parameters[..] {
        [only] if only.kind() == "identifier" && w.text(only) == w.text(arg) => {}
        _ => return None,
    }
    let each = block.parent().filter(|c| c.kind() == "call")?;
    if w.field_text(each, "method").as_deref() != Some("each") {
        return None;
    }
    let constant = constant_text(w, each.child_by_field_name("receiver")?)?;
    let (cref, paths) = w.modules.get(&w.refer(&constant))?;
    (*cref == w.cref).then(|| paths.clone())
}

/// The method `call` runs and its positional arguments, last first: its
/// own, or the one it sends (`send(:include, M)`, `__send__`,
/// `public_send`) with the arguments after the name.
fn sent<'t>(w: &Walker, call: Node<'t>, method: &str) -> Option<(&'static str, Vec<Node<'t>>)> {
    let args = call.child_by_field_name("arguments");
    let all = args.map(mixin_arguments).unwrap_or_default();
    let (name, rest) = match method {
        "send" | "__send__" | "public_send" => {
            let mut all = all;
            let first = all.pop()?;
            (literal_name(w, first)?, all)
        }
        // `eval "code"` evaluates it where it is written, as `class_eval`
        // sent to `self` does in a body (R6-14); computed code is not read.
        "eval"
            if call.child_by_field_name("receiver").is_none()
                && all
                    .last()
                    .is_some_and(|code| eval_code(w, *code, call).is_some()) =>
        {
            ("class_eval".to_owned(), all)
        }
        _ => (method.to_owned(), all),
    };
    let name = DEFINERS.iter().find(|d| **d == name)?;
    Some((name, rest))
}

/// The side a definer sent to a class or module (`X.attr_reader :a`)
/// defines or removes methods on: its own for `scope`,
/// `define_singleton_method` and `module_function`, else its instances'.
fn sent_side(definer: &str) -> char {
    match definer {
        "scope" | "define_singleton_method" | "module_function" => '.',
        _ => '#',
    }
}

/// The methods that change what a class or module holds: its ancestors,
/// its methods by name, or by evaluating code there.
const DEFINERS: &[&str] = &[
    "include",
    "prepend",
    "extend",
    "attr_reader",
    "attr_writer",
    "attr_accessor",
    "attr",
    "alias_method",
    "define_method",
    "define_singleton_method",
    "delegate",
    "scope",
    "module_function",
    "remove_method",
    "undef_method",
    "class_eval",
    "module_eval",
    "instance_eval",
];

/// What a definer call touches, from its literal arguments (last first):
/// the method names, or `None` when one is computed or it may touch any
/// (an ancestor, a string of code with interpolation).
fn touched(w: &Walker, definer: &str, args: &[Node], call: Node) -> Option<Vec<String>> {
    let positional: Vec<Node> = args
        .iter()
        .rev()
        .copied()
        .filter(|a| !matches!(a.kind(), "pair" | "block_argument"))
        .collect();
    // A name with an interpolation is a pattern of the names it may be
    // (`define_method "#{name}?"` defines a name ending in `?`).
    let pattern = |arg: &Node| literal_name(w, *arg).or_else(|| name_pattern(w, *arg));
    match definer {
        "attr_reader" | "attr_writer" | "attr_accessor" | "attr" => positional
            .iter()
            .map(|arg| names_of(w, *arg).or_else(|| Some(vec![name_pattern(w, *arg)?])))
            .collect::<Option<Vec<_>>>()
            .map(|names| {
                names
                    .into_iter()
                    .flatten()
                    .flat_map(|n| [format!("{n}="), n])
                    .collect()
            }),
        "alias_method" => Some(vec![pattern(positional.first()?)?]),
        // A prefix makes each name a pattern (`target_name`, `x_name`).
        "delegate"
            if args.iter().any(|a| {
                a.kind() == "pair"
                    && w.field_text(*a, "key").is_some_and(|k| {
                        k.trim_start_matches(':').trim_end_matches(':') == "prefix"
                    })
            }) =>
        {
            positional
                .iter()
                .map(|arg| pattern(arg).map(|name| format!("{}{name}", unread::INTERPOLATED)))
                .collect()
        }
        // The name comes first; a body, a method or a lambda may follow.
        "define_method" | "define_singleton_method" | "scope" => {
            Some(vec![pattern(positional.first()?)?])
        }
        "remove_method" | "undef_method" | "module_function" | "delegate" => {
            positional.iter().map(pattern).collect()
        }
        "class_eval" | "module_eval" | "instance_eval" => {
            let code = eval_code(w, *positional.first()?, call)?;
            let found = unread::scan(&code);
            // An interpolation in a name makes a pattern of it (`def
            // _run_#{name}_callbacks`); one that is a whole statement may
            // define anything.
            (!found.any && !unread::runs_interpolated(&code)).then_some(found.methods)
        }
        _ => None,
    }
}

/// The code a string or heredoc argument holds, each interpolation
/// replaced by [`unread::INTERPOLATED`]; `None` for anything else (a
/// variable).
/// A heredoc's body follows the statement that opens it.
fn eval_code(w: &Walker, arg: Node, call: Node) -> Option<String> {
    let body = match arg.kind() {
        "string" => arg,
        "heredoc_beginning" => {
            let mut node = call;
            loop {
                if let Some(next) = node.next_sibling().filter(|n| n.kind() == "heredoc_body") {
                    break next;
                }
                node = node.parent()?;
            }
        }
        _ => return None,
    };
    let mut text = String::new();
    let mut cursor = body.walk();
    for part in body.children(&mut cursor) {
        match part.kind() {
            "string_content" | "heredoc_content" | "escape_sequence" => {
                text.push_str(&w.text(part))
            }
            "interpolation" => text.push(unread::INTERPOLATED),
            _ => {}
        }
    }
    Some(text)
}

/// Code that changes a class or module from outside its body, which the
/// rules do not replay (R9, R10): `X.remove_method :m`, `X.include M`,
/// `X.send(:define_method, :m)`, `X.class_eval "def m; end"` on a constant
/// (or on `X.singleton_class`), and in a class or module body a string
/// `class_eval` or `instance_eval` on `self`, or a definer sent to
/// `singleton_class`. A lookup through that class or module then abstains
/// for the names touched (any name for an ancestor or computed code); a
/// module included into a constant this way may run its methods on an
/// object whose own methods come first ([`Fact::Mixed`]).
pub(super) fn outside(w: &mut Walker, call: Node, method: &str) {
    if method == "eval" && call.child_by_field_name("receiver").is_some() {
        return eval_elsewhere(w, call);
    }
    let Some((definer, args)) = sent(w, call, method) else {
        return;
    };
    // A block evaluated on the receiver is read where its `def`s land.
    let string = matches!(definer, "class_eval" | "module_eval" | "instance_eval");
    if string && args.is_empty() {
        return;
    }
    // Code a string evaluates may mix in any module it names (R6-14).
    if string
        && let Some(code) = args.last().and_then(|code| eval_code(w, *code, call))
        && MIXIN_METHODS.iter().any(|verb| code.contains(verb))
    {
        mixed_in_text(w, &code);
    }
    let receiver = call.child_by_field_name("receiver");
    let text = receiver.map(|r| w.text(r));
    let constant = receiver.and_then(|r| {
        constant_text(w, r).or_else(|| {
            let object = r.child_by_field_name("receiver")?;
            (r.kind() == "call" && w.field_text(r, "method").as_deref() == Some("singleton_class"))
                .then(|| constant_text(w, object))
                .flatten()
        })
    });
    let names = touched(w, definer, &args, call);
    // The side of the code a string evaluates: `class_eval` code defines
    // instance methods, but where it names the singleton class or adds an
    // ancestor.
    let singleton_code = || {
        definer == "instance_eval"
            || args
                .last()
                .and_then(|code| eval_code(w, *code, call))
                .is_none_or(|code| {
                    ["self", "<<", "include", "extend", "prepend", "singleton"]
                        .iter()
                        .any(|word| code.contains(word))
                })
    };
    let on_singleton_class = receiver.is_some_and(|r| {
        r.kind() == "call" && w.field_text(r, "method").as_deref() == Some("singleton_class")
    });
    if let Some(path) = constant {
        let scope = w.context("", "").cref;
        let sides: &[char] = match definer {
            "include" | "prepend" | "extend" => &['#', '.'],
            _ if on_singleton_class => &['.'],
            "class_eval" | "module_eval" | "instance_eval" if singleton_code() => &['#', '.'],
            "class_eval" | "module_eval" | "instance_eval" => &['#'],
            _ => &[sent_side(definer)],
        };
        let names: Vec<Option<String>> = match names {
            Some(names) => names.into_iter().map(Some).collect(),
            None => vec![None],
        };
        for name in names {
            for side in sides {
                w.facts.push(Fact::Evaluated {
                    path: path.clone(),
                    scope: scope.clone(),
                    side: *side,
                    name: name.clone(),
                });
            }
        }
        if string && let Some(code) = args.last().and_then(|code| eval_code(w, *code, call)) {
            w.evaluated_visibility(&code, Some((&path, &scope)));
        }
        // Whichever the verb, the module's methods may run on an object
        // whose lookup the rules do not replay: a subclass's, whose own
        // methods come first, or the class itself (R5-05).
        if MIXIN_METHODS.contains(&definer) {
            mixed(w, &args);
            opaque(w, &path, scope, definer, on_singleton_class, &args);
        }
        return;
    }
    // In a method body, a class macro's: on whatever object it runs on
    // (B7).
    let in_method = in_method_body(w);
    let in_body = in_method || matches!(w.nearest(), Some(BoundKind::Class | BoundKind::Module));
    let singleton = singleton_code();
    if string
        && !in_method
        && in_body
        && matches!(text.as_deref(), None | Some("self"))
        && let Some(code) = args.last().and_then(|code| eval_code(w, *code, call))
    {
        w.evaluated_visibility(&code, None);
    }
    let sides: &[char] = match text.as_deref() {
        None | Some("self") if string && in_body && singleton => &['#', '.'],
        None | Some("self") if string && in_body => &['#'],
        Some("singleton_class" | "self.singleton_class")
            if in_body && !MIXIN_METHODS.contains(&definer) =>
        {
            &['.']
        }
        // On `self` without a body, `mixin_call` and `declaration` read it.
        None | Some("self") | Some("singleton_class" | "self.singleton_class") => return,
        // The hook's own object, which its replay reads (D14).
        Some(text) if w.hook_base().is_some_and(|base| on_base(text, base)) => return,
        // On an object the rules cannot name (R5-03, R5-04).
        Some(_) if MIXIN_METHODS.contains(&definer) => {
            w.spread(&args);
            return;
        }
        Some(_) => {
            taint_definer(w, definer, &args, call);
            return;
        }
    };
    let names: Vec<Option<String>> = match names {
        Some(names) => names.into_iter().map(Some).collect(),
        None => vec![None],
    };
    for name in names {
        if in_method {
            for side in sides {
                w.macro_defines(*side, name.clone());
            }
        } else {
            w.unknown(sides, name);
        }
    }
}

/// `binding.eval "code"`, `Kernel.eval`, `TOPLEVEL_BINDING.eval`: a string
/// evaluated in a binding the rules do not follow, whose `self` may be
/// `main` (R7-03). What it may define lands on an object the rules cannot
/// name, the modules it names may be mixed in anywhere, and `Object`, whose
/// methods the top-level functions are, may change for any name.
fn eval_elsewhere(w: &mut Walker, call: Node) {
    let args = call
        .child_by_field_name("arguments")
        .map(mixin_arguments)
        .unwrap_or_default();
    let Some(code) = args.last().and_then(|code| eval_code(w, *code, call)) else {
        return;
    };
    if MIXIN_METHODS.iter().any(|verb| code.contains(verb)) {
        mixed_in_text(w, &code);
    }
    taint_definer(w, "class_eval", &args, call);
    let scope = w.context("", "").cref;
    for side in ['#', '.'] {
        w.facts.push(Fact::Evaluated {
            path: "::Object".to_owned(),
            scope: scope.clone(),
            side,
            name: None,
        });
    }
}

/// Taints the names the definer `call` may define on an object the rules
/// cannot name (R5-03, R5-04): none when it names none (`scope module:
/// :v2` in a routes file is no definer), any when one is computed.
fn taint_definer(w: &mut Walker, definer: &str, args: &[Node], call: Node) {
    let names = touched(w, definer, args, call);
    if names.is_none() && positional_of(args).is_empty() {
        return;
    }
    w.taint_all(names);
}

/// The constants a mixin argument names, each with the nesting it is
/// read in (innermost first): its own, or those of a splat of a literal
/// array or of a constant holding one (`include(*[A, B])`, `include
/// *MODULES`); none for any other expression.
pub(super) fn mixed_paths(w: &Walker, arg: Node) -> Vec<(String, Vec<String>)> {
    if let Some(path) = constant_text(w, arg) {
        return vec![(path, w.context("", "").cref)];
    }
    if arg.kind() == "identifier" {
        return iterated(w, arg);
    }
    let mut cursor = arg.walk();
    (arg.kind() == "splat_argument")
        .then(|| arg.named_children(&mut cursor).next())
        .flatten()
        .map(|inner| elements(w, inner))
        .unwrap_or_default()
}

/// The constants `array` holds, each with the nesting it is read in: a
/// literal array's, or those of a constant holding one.
fn elements(w: &Walker, array: Node) -> Vec<(String, Vec<String>)> {
    if array.kind() == "array" {
        let mut cursor = array.walk();
        return array
            .named_children(&mut cursor)
            .filter_map(|element| constant_text(w, element))
            .map(|path| (path, w.context("", "").cref))
            .collect();
    }
    let Some((cref, paths)) = constant_text(w, array).and_then(|c| w.modules.get(&w.refer(&c)))
    else {
        return Vec::new();
    };
    let scope: Vec<String> = cref.iter().rev().cloned().collect();
    paths
        .iter()
        .map(|path| (path.clone(), scope.clone()))
        .collect()
}

/// The constants the local `arg` takes when it is the parameter of a
/// block passed to `each` over an array [`elements`] reads (`[A, B].each
/// { |m| include m }`); none otherwise.
fn iterated(w: &Walker, arg: Node) -> Vec<(String, Vec<String>)> {
    let name = w.text(arg);
    let mut at = arg.parent();
    while let Some(node) = at {
        if matches!(
            node.kind(),
            "method" | "singleton_method" | "class" | "module" | "program"
        ) {
            break;
        }
        let names_it = node
            .child_by_field_name("parameters")
            .is_some_and(|params| {
                let mut cursor = params.walk();
                params
                    .named_children(&mut cursor)
                    .any(|param| param.kind() == "identifier" && w.text(param) == name)
            });
        if matches!(node.kind(), "block" | "do_block") && names_it {
            return node
                .parent()
                .filter(|call| {
                    call.kind() == "call"
                        && w.field_text(*call, "method").as_deref() == Some("each")
                })
                .and_then(|call| call.child_by_field_name("receiver"))
                .map(|receiver| elements(w, receiver))
                .unwrap_or_default();
        }
        at = node.parent();
    }
    Vec::new()
}

/// Records each module `args` names as mixed into an object whose own
/// lookup the rules do not replay ([`Fact::Mixed`], R5-05): any constant
/// an argument that is no module constant writes (`(P1)`, `c ? nil : P1`,
/// `P1.itself`) may be the one (R7-05).
fn mixed(w: &mut Walker, args: &[Node]) {
    for &arg in args {
        let mut paths = mixed_paths(w, arg);
        if paths.is_empty() {
            let scope = w.context("", "").cref;
            paths = written_constants(&w.text(arg))
                .into_iter()
                .map(|path| (path, scope.clone()))
                .collect();
        }
        for (path, scope) in paths {
            w.facts.push(Fact::Mixed { path, scope });
        }
    }
}

/// Records an unknown step on the class or module the constant `path`
/// names, in the nesting `scope`, for the mixin `op` there (on its
/// singleton class, `singleton`) of an argument that is no constant path
/// (`Cfg.mod`, `const_get(name)`, `*mods`): a module the rules cannot
/// name, which may be any of the graph's, on that chain ([`Fact::Opaque`],
/// R8-05). An `extend` of a singleton class acts on its own singleton,
/// which no lookup reads.
fn opaque(
    w: &mut Walker,
    path: &str,
    scope: Vec<String>,
    op: &str,
    singleton: bool,
    args: &[Node],
) {
    let side = match (op, singleton) {
        ("extend", true) => return,
        ("extend", false) | (_, true) => '.',
        _ => '#',
    };
    if args.iter().all(|&arg| constant_text(w, arg).is_some()) {
        return;
    }
    w.facts.push(Fact::Opaque {
        path: path.to_owned(),
        scope,
        side,
        prepend: op == "prepend",
    });
}

/// Records as [`Fact::Mixed`] every constant code the rules do not read
/// may name ([`written_constants`]): it may mix that module in where no
/// lookup replays it (R6-08, R6-14).
fn mixed_in_text(w: &mut Walker, text: &str) {
    let scope = w.context("", "").cref;
    for path in written_constants(text) {
        w.facts.push(Fact::Mixed {
            path,
            scope: scope.clone(),
        });
    }
}

/// Every constant `text` may name, written as a path, a symbol or in a
/// string (`M3`, `self::M3`, `:M3`, `"include M3"`), once each.
pub(super) fn written_constants(text: &str) -> Vec<String> {
    let mut named: Vec<String> = Vec::new();
    for word in text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':')) {
        let word = word.strip_prefix("self::").unwrap_or(word);
        let path = match word.strip_prefix(':') {
            Some(rest) if !rest.starts_with(':') => rest,
            _ => word,
        };
        let constant = path
            .strip_prefix("::")
            .unwrap_or(path)
            .split("::")
            .all(|segment| segment.chars().next().is_some_and(char::is_uppercase));
        if constant && !named.iter().any(|n| n == path) {
            named.push(path.to_owned());
        }
    }
    named
}

/// `K.class_exec(M) { |m| prepend m }` and its family: a block that mixes
/// in what the evaluator passes it, which the rules do not follow, may
/// mix each constant argument into `K` (R6-06).
pub(super) fn exec_mixin(w: &mut Walker, call: Node, method: &str) {
    if !matches!(method, "class_exec" | "module_exec" | "instance_exec") {
        return;
    }
    let (Some(args), Some(block)) = (
        call.child_by_field_name("arguments"),
        call.child_by_field_name("block"),
    ) else {
        return;
    };
    let mut stack = vec![block];
    let mut mixes = false;
    while let Some(at) = stack.pop() {
        if at.kind() == "call"
            && w.field_text(at, "method")
                .and_then(|method| sent(w, at, &method))
                .is_some_and(|(op, _)| MIXIN_METHODS.contains(&op))
        {
            mixes = true;
            break;
        }
        let mut cursor = at.walk();
        stack.extend(at.named_children(&mut cursor));
    }
    if mixes {
        mixed(w, &positional(args));
    }
}

/// Whether the receiver `text` is the hook's parameter `base`, or its
/// singleton class.
fn on_base(text: &str, base: &str) -> bool {
    text == base
        || text
            .strip_prefix(base)
            .is_some_and(|rest| rest == ".singleton_class")
}

/// Whether `call` is a statement of a block passed to `base.class_eval`
/// (or its family), which a hook's replay reads (D14).
fn in_hook_eval(w: &Walker, call: Node, base: &str) -> bool {
    let block = call
        .parent()
        .and_then(|body| body.parent())
        .filter(|block| matches!(block.kind(), "block" | "do_block"));
    block
        .and_then(|block| block.parent())
        .filter(|eval| eval.kind() == "call")
        .is_some_and(|eval| {
            w.field_text(eval, "receiver").as_deref() == Some(base)
                && w.field_text(eval, "method").is_some_and(|m| {
                    matches!(
                        m.as_str(),
                        "class_eval" | "class_exec" | "module_eval" | "module_exec"
                    )
                })
        })
}

/// The arguments of a mixin call, last first, as Ruby inserts them.
fn mixin_arguments(args: Node) -> Vec<Node> {
    let mut cursor = args.walk();
    let mut arguments: Vec<Node> = args
        .named_children(&mut cursor)
        .filter(|arg| arg.kind() != "comment")
        .collect();
    arguments.reverse();
    arguments
}

/// An ancestor declaration of `owner` (a class, a module, or a hook
/// method) naming `arg`: its constant, or an empty name for an expression.
fn push_ancestor(w: &mut Walker, arg: Node, line: u32, owner: usize, kind: &str) {
    push_named_ancestor(w, arg, line, owner, kind, None, false);
}

/// [`push_ancestor`], with the constant `name` resolves to when the
/// argument is no constant path (`self`, the module itself: `::M`), and
/// [`CONDITIONAL`] when it may not run (R5-07).
fn push_named_ancestor(
    w: &mut Walker,
    arg: Node,
    line: u32,
    owner: usize,
    kind: &str,
    name: Option<String>,
    conditional: bool,
) {
    let target = constant_text(w, arg);
    let named = name.clone().or_else(|| target.clone());
    let mut context = w.context(named.as_deref().unwrap_or_default(), kind);
    if conditional {
        context.extra.push(CONDITIONAL.to_owned());
    }
    let written = target.unwrap_or_else(|| one_line(&w.text(arg)));
    w.push_site(EdgeKind::Reference, line, Some(owner), written, &context);
}

/// The hooks Ruby calls on a module included, extended or prepended, and
/// on a class subclassed (D14).
pub(in crate::ruby) const HOOKS: &[&str] = &["included", "extended", "prepended", "inherited"];

/// The methods that replace what `include`, `prepend` or `extend` does.
pub(in crate::ruby) const FEATURES: &[&str] =
    &["append_features", "prepend_features", "extend_object"];

/// The hooks Ruby calls on a method defined, which the rules do not read
/// (R8-06).
pub(in crate::ruby) const ADDED: &[&str] = &["method_added", "singleton_method_added"];

/// `def self.included(base)` and its family in a class or module body
/// (D14), as the hook's ancestor declarations: `base.include(X)`,
/// `base.extend(X)`, `base.prepend(X)`, the same through `send`, `__send__`
/// or `public_send`, and a receiver-less `include`, `extend` or `prepend`
/// in a `base.class_eval do` block (or `class_exec`, `module_eval`,
/// `module_exec`) act on the object the hook runs for, recorded as
/// `hook:include` and its family, owned by the hook method; `super` runs
/// the next definition of the hook ([`Fact::Super`]). Any other statement,
/// a hook whose parameters are not
/// one plain name, and any definition of `append_features`,
/// `prepend_features` or `extend_object` may change that object's
/// ancestors in a way the rules do not read: a [`Fact::Hook`] at its line,
/// and a [`Fact::Mixed`] for each constant such a statement names (R6-08,
/// R6-14).
pub(super) fn hook(w: &mut Walker, node: Node, def: usize) {
    let qualified = w.defs[def].name.clone();
    let Some(at) = qualified.rfind(['#', '.']) else {
        return;
    };
    let name = &qualified[at + 1..];
    if ADDED.contains(&name) {
        return added(w, &qualified);
    }
    // A hook defined on a singleton other than by a `def self.` written
    // in its class or module body (`class << self`, `def M.included`, in
    // a block) is an unknown hook (R8-06).
    let plain = node.kind() == "singleton_method"
        && node
            .child_by_field_name("object")
            .is_some_and(|object| object.kind() == "self")
        && node
            .parent()
            .filter(|body| body.kind() == "body_statement")
            .and_then(|body| body.parent())
            .is_some_and(|owner| matches!(owner.kind(), "class" | "module"));
    if FEATURES.contains(&name) || HOOKS.contains(&name) && &qualified[at..=at] == "." && !plain {
        w.facts.push(Fact::Hook {
            method: qualified,
            line: line(node),
        });
        return;
    }
    if !HOOKS.contains(&name) {
        return;
    }
    let parameter = node.child_by_field_name("parameters").and_then(|params| {
        let mut cursor = params.walk();
        match params.named_children(&mut cursor).collect::<Vec<_>>()[..] {
            [only] if only.kind() == "identifier" => Some(w.text(only)),
            _ => None,
        }
    });
    let Some(base) = parameter else {
        w.facts.push(Fact::Hook {
            method: qualified,
            line: line(node),
        });
        return;
    };
    w.hook_bases.insert(def, base.clone());
    for statement in node
        .child_by_field_name("body")
        .map(statements)
        .unwrap_or_default()
    {
        if !hook_statement(w, statement, &base, def) {
            w.facts.push(Fact::Hook {
                method: qualified.clone(),
                line: line(statement),
            });
            // What it mixes into the object the hook runs for, the rules
            // do not place: any module it names may run its methods there
            // (R6-08, R6-14).
            let text = w.text(statement);
            if MIXIN_METHODS.iter().any(|verb| text.contains(verb)) {
                mixed_in_text(w, &text);
            }
        }
    }
}

/// `method_added` or `singleton_method_added`, defined as `qualified`
/// (`K.method_added`, or `Hooky#method_added` for the objects a module
/// extends): Ruby runs it on each method defined on the objects whose
/// singleton chain holds it, where it may define, remove or reorder any
/// name, as a class macro may (B7, R8-06).
fn added(w: &mut Walker, qualified: &str) {
    let Some((owner, method)) = qualified.rfind(['#', '.']).map(|at| {
        (
            &qualified[..at],
            qualified[at..].chars().next().unwrap_or('.'),
        )
    }) else {
        return;
    };
    for side in ['#', '.'] {
        w.facts.push(Fact::Macro {
            owner: owner.to_owned(),
            method,
            side,
            name: None,
        });
    }
}

/// The statements of a body: a `body_statement`'s, or the one expression
/// of an endless method.
fn statements(body: Node) -> Vec<Node> {
    if !matches!(body.kind(), "body_statement" | "block_body") {
        return vec![body];
    }
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .filter(|n| n.kind() != "comment")
        .collect()
}

/// Records one statement of a hook whose parameter is `base`; false when
/// the rules do not read it.
fn hook_statement(w: &mut Walker, statement: Node, base: &str, def: usize) -> bool {
    let is_super = statement.kind() == "super"
        || statement.kind() == "call"
            && w.field_text(statement, "method").as_deref() == Some("super")
            && statement.child_by_field_name("block").is_none();
    if is_super {
        w.facts.push(Fact::Super {
            method: w.defs[def].name.clone(),
            line: line(statement),
        });
        return true;
    }
    if statement.kind() != "call" {
        return false;
    }
    let method = w.field_text(statement, "method").unwrap_or_default();
    if w.field_text(statement, "receiver").as_deref() != Some(base) {
        return false;
    }
    let args = statement.child_by_field_name("arguments");
    let block = statement.child_by_field_name("block");
    match method.as_str() {
        "include" | "extend" | "prepend" if block.is_none() => {
            let Some(args) = args else { return false };
            let kind = format!("{MIXIN}hook:{method}");
            for arg in mixin_arguments(args) {
                push_ancestor(w, arg, line(statement), def, &kind);
            }
            true
        }
        "send" | "__send__" | "public_send" if block.is_none() => {
            let Some(args) = args else { return false };
            let positional = positional(args);
            let Some((first, rest)) = positional.split_first() else {
                return false;
            };
            let Some(sent) =
                literal_name(w, *first).filter(|m| MIXIN_METHODS.contains(&m.as_str()))
            else {
                return false;
            };
            if rest.is_empty() || positional.len() != mixin_arguments(args).len() {
                return false;
            }
            let kind = format!("{MIXIN}hook:{sent}");
            for arg in rest.iter().rev() {
                push_ancestor(w, *arg, line(statement), def, &kind);
            }
            true
        }
        "class_eval" | "class_exec" | "module_eval" | "module_exec" if args.is_none() => {
            let Some(body) = block.and_then(|b| b.child_by_field_name("body")) else {
                return block.is_some();
            };
            statements(body)
                .into_iter()
                .all(|inner| evaluated_mixin(w, inner, def))
        }
        _ => false,
    }
}

/// A receiver-less (or `self.`) `include`, `extend` or `prepend` in a
/// hook's `base.class_eval do` block, recorded as the hook's; false for
/// any other statement.
fn evaluated_mixin(w: &mut Walker, statement: Node, def: usize) -> bool {
    if statement.kind() != "call"
        || statement.child_by_field_name("block").is_some()
        || w.field_text(statement, "receiver")
            .is_some_and(|r| r != "self")
    {
        return false;
    }
    let method = w.field_text(statement, "method").unwrap_or_default();
    let Some(args) = statement
        .child_by_field_name("arguments")
        .filter(|_| MIXIN_METHODS.contains(&method.as_str()))
    else {
        return false;
    };
    let kind = format!("{MIXIN}hook:{method}");
    for arg in mixin_arguments(args) {
        push_ancestor(w, arg, line(statement), def, &kind);
    }
    true
}

/// The text of a literal constant path (`A`, `A::B`, `::A::B`), or `None`
/// for any other expression (v1 `constant_text`).
pub(super) fn constant_text(w: &Walker, node: Node) -> Option<String> {
    let mut at = node;
    while at.kind() == "scope_resolution" {
        if !at
            .child_by_field_name("name")
            .is_some_and(|name| name.kind() == "constant")
        {
            return None;
        }
        match at.child_by_field_name("scope") {
            Some(scope) => at = scope,
            None => return Some(w.text(node)),
        }
    }
    (at.kind() == "constant").then(|| w.text(node))
}

/// `included do ... end` (`method` `included`) or `prepended do ... end`:
/// the call whose block runs on the class including or prepending the
/// concern.
pub(super) fn is_concern_call(w: &Walker, call: Node, method: &str) -> bool {
    call.kind() == "call"
        && call.child_by_field_name("receiver").is_none()
        && w.field_text(call, "method").as_deref() == Some(method)
}

/// Whether the first argument of `call` is a literal method name: a
/// `define_method(:name)` that defines `name` (D16).
pub(super) fn names_literally(w: &Walker, call: Node) -> bool {
    call.child_by_field_name("arguments")
        .and_then(|args| positional(args).first().copied())
        .is_some_and(|first| literal_name(w, first).is_some())
}

/// An Active Record association macro, whose block Rails turns into the
/// association's extension module (`Module.new(&block)`).
pub(super) fn is_association_macro(method: &str) -> bool {
    MACROS.contains(&method)
}

/// `class_methods do ... end` (or `{ }`) in a module body: its block
/// defines the concern's `ClassMethods` module, which the includer extends.
pub(super) fn is_class_methods_block(w: &Walker, call: Node) -> bool {
    call.child_by_field_name("receiver").is_none()
        && call.child_by_field_name("block").is_some()
        && w.field_text(call, "method").as_deref() == Some("class_methods")
        && w.nearest() == Some(BoundKind::Module)
}

/// The methods a declaration generates, with the method each forwards to
/// on its own owner (v1 `ruby_generated::Generated`).
struct Generated {
    names: Vec<String>,
    /// `#` for an instance method, `.` for a method of the class itself
    /// (`scope`, or an accessor in `class << self`); both for a
    /// `define_method` under `module_function`.
    separators: Vec<char>,
    /// `(qualifier, method)` of the reference each generated name carries.
    forwards_to: Option<(&'static str, String)>,
}

/// A receiver-less declaration in a class, module or `class << self` body
/// (`attr_reader`, `attr_writer`, `attr_accessor`, `alias_method`,
/// `delegate`, `scope`, `define_method`, `define_singleton_method`,
/// `module_function :name`) generates methods of the owner, each spanning
/// the declaration. True iff it generated at least one (v1
/// `walk_declaration`). `remove_method`, `undef_method` and a
/// `define_method` whose name is computed generate nothing, and record
/// that a lookup through the owner cannot know the names they touch.
/// <https://docs.ruby-lang.org/en/master/Module.html#method-i-attr_accessor>
/// <https://api.rubyonrails.org/classes/Module.html#method-i-delegate>
/// <https://api.rubyonrails.org/classes/ActiveRecord/Scoping/Named/ClassMethods.html#method-i-scope>
/// <https://docs.ruby-lang.org/en/master/Module.html#method-i-define_method>
/// <https://docs.ruby-lang.org/en/master/Module.html#method-i-remove_method>
pub(super) fn declaration(w: &mut Walker, call: Node, method: &str, defs: Defs) -> bool {
    // `send(:define_method, :x)` runs the definer it names (B7, B8).
    let Some((definer, sent_args)) = sent(w, call, method) else {
        return false;
    };
    // `class_eval do`: its block is read where its `def`s land.
    let evaluator = matches!(definer, "class_eval" | "module_eval" | "instance_eval");
    if evaluator && positional_of(&sent_args).is_empty() {
        return false;
    }
    // In a block that may run on the class, what it declares may land
    // there (R1, R9).
    if w.foreign() {
        if !MIXIN_METHODS.contains(&definer) {
            let defined = Defined::Sent(sent_side(definer));
            match touched(w, definer, &sent_args, call) {
                Some(names) => {
                    for name in names {
                        w.may_define(defined, Some(name));
                    }
                }
                None => w.may_define(defined, None),
            }
        }
        return false;
    }
    // In a method body, a class macro: it defines on whatever object the
    // method runs on (B7).
    if in_method_body(w) {
        if !MIXIN_METHODS.contains(&definer) {
            let side = sent_side(definer);
            match touched(w, definer, &sent_args, call) {
                Some(names) => {
                    for name in names {
                        w.macro_defines(side, Some(name));
                    }
                }
                None => w.macro_defines(side, None),
            }
        }
        return false;
    }
    // On an object no lookup can name exactly: a new anonymous class, one
    // object's singleton class, `main` (R5-03, R5-04).
    if (w.unplaced() || w.owners.is_empty()) && !MIXIN_METHODS.contains(&definer) {
        taint_definer(w, definer, &sent_args, call);
        return false;
    }
    if w.owners.is_empty() || !in_owner_body(w) {
        return false;
    }
    let args: Vec<Node> = sent_args.into_iter().rev().collect();
    // A `def` or `define_method` defines here; a declaration's methods are
    // instance methods under `module_function` (Ruby 3.3).
    let (Some(declared), defined) = (defs.declared(), defs.defined().to_vec()) else {
        return false;
    };
    let instance = vec![declared];
    let generated = match definer {
        "attr_reader" | "attr" => accessors(w, &args, instance, &[""]),
        "attr_writer" => accessors(w, &args, instance, &["="]),
        "attr_accessor" => accessors(w, &args, instance, &["", "="]),
        "alias_method" => alias_method(w, &args, instance),
        "delegate" => delegate(w, &args, instance),
        // A scope is a class method of the model, whatever the frame.
        "scope" => scope(w, &args),
        // Sent to `self`, the class even in `instance_eval`.
        "define_method" if defs == Defs::InstanceEval => defined_method(w, &args, vec!['#']),
        "define_method" => defined_method(w, &args, defined),
        // On the singleton of `class << self`, a method the rules cannot
        // name.
        "define_singleton_method" if defs != Defs::Singleton => defined_method(w, &args, vec!['.']),
        "module_function" if w.nearest() == Some(BoundKind::Module) => module_functions(w, &args),
        "remove_method" | "undef_method" => {
            removed(w, &args, declared);
            None
        }
        _ => None,
    };
    let Some(generated) = generated else {
        return false;
    };
    // An accessor or a `define_method` after a bare `private` is private
    // (R9-03).
    if matches!(
        definer,
        "attr_reader" | "attr" | "attr_writer" | "attr_accessor" | "define_method"
    ) {
        for (name, separator) in generated
            .names
            .iter()
            .flat_map(|n| generated.separators.iter().map(move |s| (n, *s)))
        {
            w.hide_defined(defs, 0, separator, name);
        }
    }
    push(w, call, generated)
}

/// Whether the walk is in a method body where `self` is the method's
/// receiver: a definer called there on `self` is a class macro (B7).
pub(super) fn in_method_body(w: &Walker) -> bool {
    matches!(
        w.nearest(),
        Some(BoundKind::Method | BoundKind::SingletonMethod)
    ) && w.this() == This::Own
}

/// `define_method(:name) { }`: the method `name` (D16). A computed name
/// (`define_method(name)`) may be any name: a lookup through the owner on
/// that side abstains for every one.
fn defined_method(w: &mut Walker, args: &[Node], separators: Vec<char>) -> Option<Generated> {
    let first = *positional_of(args).first()?;
    match literal_name(w, first) {
        Some(name) => Some(Generated {
            names: vec![name],
            separators,
            forwards_to: None,
        }),
        None => {
            let pattern = name_pattern(w, first);
            w.unknown(&separators, pattern);
            None
        }
    }
}

/// `Struct.new(:a, :b)` and `Data.define(:a)`: the members' readers, and a
/// Struct's writers, methods of the new class itself, which come before
/// any module it includes (Ruby 3.3: `instance_methods(false)`). A leading
/// string names a Struct class, not a member; a computed member may be any
/// name.
pub(super) fn members(w: &mut Walker, call: Node, writers: bool) {
    let Some(args) = call.child_by_field_name("arguments") else {
        return;
    };
    let mut names = Vec::new();
    for (i, arg) in positional(args).into_iter().enumerate() {
        if writers && i == 0 && arg.kind() == "string" {
            continue;
        }
        match literal_name(w, arg) {
            Some(name) => names.push(name),
            None => w.unknown(&['#'], None),
        }
    }
    let suffixes: &[&str] = if writers { &["", "="] } else { &[""] };
    let names = names
        .iter()
        .flat_map(|name| suffixes.iter().map(move |suffix| format!("{name}{suffix}")))
        .collect();
    // Each spans the member list, not the block after it.
    push(
        w,
        args,
        Generated {
            names,
            separators: vec!['#'],
            forwards_to: None,
        },
    );
}

/// `module_function :a, :b` (or `module_function def a`): the module's own
/// copies `M.a`, `M.b` of instance methods it has (D15). A computed name
/// may be any name of the module itself.
fn module_functions(w: &mut Walker, args: &[Node]) -> Option<Generated> {
    let mut names = Vec::new();
    for arg in positional_of(args) {
        let name = match arg.kind() {
            "method" => w.field_text(arg, "name"),
            _ => literal_name(w, arg),
        };
        match name {
            Some(name) => names.push(name),
            None => w.unknown(&['.'], None),
        }
    }
    Some(Generated {
        names,
        separators: vec!['.'],
        forwards_to: None,
    })
}

/// `remove_method :a` and `undef_method :a`: Ruby then finds `a` past the
/// owner, or not at all; a lookup that reaches the owner for `a` abstains
/// (F4). A computed name may be any name.
fn removed(w: &mut Walker, args: &[Node], side: char) {
    for arg in positional_of(args) {
        let name = literal_name(w, arg).or_else(|| name_pattern(w, arg));
        w.unknown(&[side], name);
    }
}

/// `undef a, :b`: as `undef_method` (F4).
pub(super) fn undef_keyword(w: &mut Walker, node: Node) {
    let foreign = w.foreign();
    // In a method body, on the class the method was defined in, once it
    // runs.
    if !foreign && in_method_body(w) {
        let mut cursor = node.walk();
        let names: Vec<Node> = node.named_children(&mut cursor).collect();
        for name in names {
            let name = method_name(w, name);
            w.unknown(&['#', '.'], name);
        }
        return;
    }
    if !foreign && (w.owners.is_empty() || !in_owner_body(w)) {
        return;
    }
    let side = w.locals.current_defs().keyword();
    let mut cursor = node.walk();
    let names: Vec<Node> = node.named_children(&mut cursor).collect();
    for name in names {
        let name = match name.kind() {
            "setter" => w.field_text(name, "name").map(|n| format!("{n}=")),
            _ => method_name(w, name),
        };
        match side {
            Some(side) if !foreign => w.unknown(&[side], name),
            _ => w.may_define(Defined::Def, name),
        }
    }
}

/// `alias new old` (the keyword): `new` is a copy of `old` on the same
/// owner. A global-variable alias (`alias $new $old`) defines no method.
pub(super) fn alias_keyword(w: &mut Walker, node: Node) {
    if w.foreign() {
        let new = node
            .child_by_field_name("name")
            .and_then(|n| method_name(w, n));
        w.may_define(Defined::Def, new);
        return;
    }
    // In a method body, on the class the method was defined in, once it
    // runs.
    if in_method_body(w) {
        let new = node
            .child_by_field_name("name")
            .and_then(|n| method_name(w, n));
        w.unknown(&['#', '.'], new);
        return;
    }
    if w.owners.is_empty() || !in_owner_body(w) {
        return;
    }
    let (Some(new), Some(old)) = (
        node.child_by_field_name("name")
            .and_then(|n| method_name(w, n)),
        node.child_by_field_name("alias")
            .and_then(|n| method_name(w, n)),
    ) else {
        return;
    };
    let Some(separator) = w.locals.current_defs().keyword() else {
        return;
    };
    push(
        w,
        node,
        Generated {
            names: vec![new],
            separators: vec![separator],
            forwards_to: Some((ALIAS, old)),
        },
    );
}

/// In a class, module or `class << self` body, directly or through blocks
/// (`included do`), not in a method body.
fn in_owner_body(w: &Walker) -> bool {
    matches!(
        w.nearest(),
        Some(BoundKind::Class | BoundKind::Module | BoundKind::SingletonClass)
    )
}

/// `attr_reader :a, :b` and its family. A computed name (`*ATTRS`, a
/// variable) may be any name: a lookup through the owner on that side
/// abstains for every one (B7).
fn accessors(
    w: &mut Walker,
    args: &[Node],
    separators: Vec<char>,
    suffixes: &[&str],
) -> Option<Generated> {
    let mut names = Vec::new();
    for arg in positional_of(args) {
        match names_of(w, arg) {
            Some(read) => {
                for name in read {
                    for suffix in suffixes {
                        names.push(format!("{name}{suffix}"));
                    }
                }
            }
            None => match name_pattern(w, arg) {
                Some(pattern) => {
                    for suffix in suffixes {
                        w.unknown(&separators, Some(format!("{pattern}{suffix}")));
                    }
                }
                None => w.unknown(&separators, None),
            },
        }
    }
    Some(Generated {
        names,
        separators,
        forwards_to: None,
    })
}

fn alias_method(w: &Walker, args: &[Node], separators: Vec<char>) -> Option<Generated> {
    let [new, old] = positional_of(args)[..] else {
        return None;
    };
    Some(Generated {
        names: vec![literal_name(w, new)?],
        separators,
        forwards_to: Some((ALIAS, literal_name(w, old)?)),
    })
}

/// `delegate :a, :b, to: :target, prefix: true | :custom`. A dynamic `to:`
/// still names the delegators unless `prefix: true` needs it; a dynamic
/// prefix names nothing. `to:` an instance variable, a constant or
/// `:class` forwards to no method of the owner. A Ruby 3.1 shorthand option
/// (`to:`, `prefix:` with no value) passes the local of that name: dynamic.
fn delegate(w: &Walker, args: &[Node], separators: Vec<char>) -> Option<Generated> {
    let mut target: Option<Option<String>> = None;
    let mut prefix: Option<Prefix> = None;
    let pairs: Vec<Node> = args
        .iter()
        .copied()
        .filter(|a| a.kind() == "pair")
        .collect();
    for pair in pairs {
        let Some(key) = w.field_text(pair, "key") else {
            continue;
        };
        let value = pair.child_by_field_name("value");
        match key.trim_start_matches(':').trim_end_matches(':') {
            "to" => target = Some(value.and_then(|value| literal_name(w, value))),
            "prefix" => {
                let value = value?;
                prefix = Some(match value.kind() {
                    "true" => Prefix::Target,
                    "false" | "nil" => Prefix::None,
                    _ => Prefix::Custom(literal_name(w, value)?),
                });
            }
            _ => {}
        }
    }
    // Rails raises without `to:`; such a call defines nothing.
    let target = target?;
    let prefix = match prefix.unwrap_or(Prefix::None) {
        Prefix::None => String::new(),
        Prefix::Target => format!("{}_", target.clone()?),
        Prefix::Custom(custom) => format!("{custom}_"),
    };
    let names = positional_of(args)
        .into_iter()
        .filter_map(|arg| literal_name(w, arg))
        .map(|name| format!("{prefix}{name}"))
        .collect();
    let forwards_to = target
        .filter(|t| is_method_name(t) && t != "class")
        .map(|t| (DELEGATE, t));
    Some(Generated {
        names,
        separators,
        forwards_to,
    })
}

enum Prefix {
    None,
    Target,
    Custom(String),
}

fn scope(w: &Walker, args: &[Node]) -> Option<Generated> {
    let first = *positional_of(args).first()?;
    Some(Generated {
        names: vec![literal_name(w, first)?],
        separators: vec!['.'],
        forwards_to: None,
    })
}

/// The arguments that are not `key: value` pairs, block arguments or
/// comments; a splat stays and names nothing.
pub(super) fn positional(args: Node) -> Vec<Node> {
    let mut cursor = args.walk();
    let all: Vec<Node> = args.named_children(&mut cursor).collect();
    positional_of(&all)
}

/// [`positional`] among the argument nodes `args`.
fn positional_of<'t>(args: &[Node<'t>]) -> Vec<Node<'t>> {
    args.iter()
        .copied()
        .filter(|a| !matches!(a.kind(), "pair" | "block_argument" | "comment"))
        .collect()
}

/// A literal method name: `:name`, `:"name"`, `"name"` or `'name'`, without
/// interpolation or escape sequence; `None` for anything else.
pub(super) fn literal_name(w: &Walker, node: Node) -> Option<String> {
    let name = match node.kind() {
        "simple_symbol" => w.text(node).get(1..)?.to_owned(),
        "string" | "delimited_symbol" => {
            let mut text = String::new();
            let mut cursor = node.walk();
            for part in node.children(&mut cursor) {
                match part.kind() {
                    "string_content" => text.push_str(&w.text(part)),
                    "interpolation" | "escape_sequence" => return None,
                    _ => {}
                }
            }
            text
        }
        _ => return None,
    };
    (!name.is_empty()).then_some(name)
}

/// The names a string or symbol with interpolation may be, as a pattern:
/// each interpolation is [`unread::INTERPOLATED`], any text
/// (`:"#{name}?"`); `None` for anything else, or one with an escape
/// sequence.
pub(super) fn name_pattern(w: &Walker, node: Node) -> Option<String> {
    if !matches!(node.kind(), "string" | "delimited_symbol") {
        return None;
    }
    let mut text = String::new();
    let mut cursor = node.walk();
    for part in node.children(&mut cursor) {
        match part.kind() {
            "string_content" => text.push_str(&w.text(part)),
            "interpolation" => text.push(unread::INTERPOLATED),
            "escape_sequence" => return None,
            _ => {}
        }
    }
    text.contains(unread::INTERPOLATED).then_some(text)
}

/// The literal names an array holds (`%i[a b]`, `%w[a b]`, `[:a, "b"]`,
/// each maybe `.freeze`d); `None` when one is anything else.
pub(super) fn literal_array(w: &Walker, node: Node) -> Option<Vec<String>> {
    let array = match node.kind() {
        "call" if w.field_text(node, "method").as_deref() == Some("freeze") => {
            node.child_by_field_name("receiver")?
        }
        _ => node,
    };
    if !matches!(array.kind(), "array" | "symbol_array" | "string_array") {
        return None;
    }
    let mut cursor = array.walk();
    let elements: Vec<Node> = array
        .named_children(&mut cursor)
        .filter(|n| n.kind() != "comment")
        .collect();
    elements
        .into_iter()
        .map(|element| match element.kind() {
            "bare_symbol" | "bare_string" => {
                let mut cursor = element.walk();
                let parts: Vec<Node> = element.named_children(&mut cursor).collect();
                match parts[..] {
                    [part] if part.kind() == "string_content" => Some(w.text(part)),
                    _ => None,
                }
            }
            _ => literal_name(w, element),
        })
        .collect()
}

/// The constants a literal array holds (`[A, B::C]`, maybe `.freeze`d);
/// `None` when one is anything else.
pub(super) fn constant_array(w: &Walker, node: Node) -> Option<Vec<String>> {
    let array = match node.kind() {
        "call" if w.field_text(node, "method").as_deref() == Some("freeze") => {
            node.child_by_field_name("receiver")?
        }
        _ => node,
    };
    if array.kind() != "array" {
        return None;
    }
    let mut cursor = array.walk();
    let elements: Vec<Node> = array
        .named_children(&mut cursor)
        .filter(|n| n.kind() != "comment")
        .collect();
    elements
        .into_iter()
        .map(|element| constant_text(w, element))
        .collect()
}

/// The names an argument gives: a literal one, or each of a literal array
/// a splat of a constant reads (`*ATTRS` for `ATTRS = %i[a b]` above in
/// this file, where the lexical lookup finds it); `None` for anything
/// else, which may be any name.
fn names_of(w: &Walker, arg: Node) -> Option<Vec<String>> {
    if let Some(name) = literal_name(w, arg) {
        return Some(vec![name]);
    }
    let mut cursor = arg.walk();
    let inner = (arg.kind() == "splat_argument")
        .then(|| arg.named_children(&mut cursor).next())
        .flatten()?;
    let constant = constant_text(w, inner)?;
    w.arrays.get(&w.refer(&constant)).cloned()
}

/// A method name as the `alias` keyword spells it: an identifier, a
/// constant, an operator, a setter or a symbol; `None` for a global.
fn method_name(w: &Walker, node: Node) -> Option<String> {
    match node.kind() {
        "simple_symbol" | "string" | "delimited_symbol" => literal_name(w, node),
        "global_variable" => None,
        _ => Some(w.text(node)),
    }
}

/// A plain method name of the owner, maybe ending in `?` or `!` (not
/// `@ivar`, `@@cvar`, `$global` or `Constant`).
fn is_method_name(name: &str) -> bool {
    let body = name.trim_end_matches(['?', '!']);
    body.chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && body.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Pushes the generated methods, each with its own reference to the method
/// it forwards to: the methods one `delegate :a, :b` generates share a line,
/// and each forwards on its own. True iff there was at least one.
fn push(w: &mut Walker, node: Node, generated: Generated) -> bool {
    let Some(owner) = w.owners.last().map(|o| o.qualified.clone()) else {
        return false;
    };
    let any = !generated.names.is_empty();
    for name in generated.names {
        for separator in &generated.separators {
            let qualified = format!("{owner}{separator}{name}");
            // A hook defined other than by `def` runs statements the rules
            // do not read (R5-08, R8-06).
            if *separator == '.'
                && (HOOKS.contains(&name.as_str()) || FEATURES.contains(&name.as_str()))
            {
                w.facts.push(Fact::Hook {
                    method: qualified.clone(),
                    line: line(node),
                });
            }
            if ADDED.contains(&name.as_str()) {
                added(w, &qualified);
            }
            let Some(def) = w.push_def(SymbolKind::Method, qualified, node, true) else {
                continue;
            };
            if let Some((qualifier, target)) = &generated.forwards_to {
                let context = w.context(target, qualifier);
                w.push_site(
                    EdgeKind::Reference,
                    line(node),
                    Some(def),
                    target.clone(),
                    &context,
                );
            }
        }
    }
    any
}

/// Drops the definitions a later one of the same method overrides,
/// wherever a generated method is involved: Ruby keeps the last definition
/// in source order, a `def` or a declaration (`def name` then `attr_reader
/// :name` leaves the reader; the reverse leaves the `def`). An overridden
/// declaration forwards nothing, and the sites of an overridden `def` body
/// go with it: that body never runs as the method, and v1's move of its
/// calls to the class would read them with the class as `self`.
/// Definitions no declaration generated are kept as they are (v1
/// `drop_overridden`), and so are those of a name that is `unsettled`: one
/// of them is conditional, so none is known to override another (D06).
/// <https://docs.ruby-lang.org/en/master/Module.html#method-i-attr_reader>
pub(super) fn drop_overridden(w: &mut Walker, unsettled: &[String]) {
    if !w.generated.iter().any(|g| *g) {
        return;
    }
    // Per name and kind: whether a generated method is among them, and the
    // last definition (walk order is source order).
    let mut groups: HashMap<(&str, SymbolKind), (bool, usize)> = HashMap::new();
    for (i, def) in w.defs.iter().enumerate() {
        let entry = groups
            .entry((def.name.as_str(), def.kind))
            .or_insert((false, i));
        entry.0 |= w.generated[i];
        entry.1 = i;
    }
    let drop: Vec<bool> = w
        .defs
        .iter()
        .enumerate()
        .map(|(i, def)| {
            groups
                .get(&(def.name.as_str(), def.kind))
                .is_some_and(|&(generated, last)| generated && last != i)
                && !unsettled.contains(&def.name)
        })
        .collect();
    let mut new_index = Vec::with_capacity(drop.len());
    let mut next = 0;
    for dropped in &drop {
        new_index.push((!dropped).then_some(next));
        next += usize::from(!dropped);
    }
    w.sites
        .retain(|site| site.owner.is_none_or(|i| i == FILE_OWNER || !drop[i]));
    for site in &mut w.sites {
        site.owner = site.owner.and_then(|i| {
            if i == FILE_OWNER {
                Some(i)
            } else {
                new_index[i]
            }
        });
    }
    let mut index = 0;
    w.defs.retain(|_| {
        let keep = !drop[index];
        index += 1;
        keep
    });
    w.generated.clear();
}

#[cfg(test)]
mod tests;
