// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The methods a body may make private or protected, which a call on a
//! constant receiver never reaches: those defined while a bare `private`
//! or `protected` is in force, the instance copies a bare
//! `module_function` makes (R10-04), and those `private`, `protected`,
//! `private_class_method` or `module_function` name (R9-03, R10-04). A
//! visibility change the rules do not read (`send(:private, :m)`, a
//! `class_eval` from outside, `K.private_class_method :s`) leaves the
//! methods it names unproven for such a call on the class or module it
//! acts on, every method for a name the rules cannot read (R11-03).

use tree_sitter::Node;

use super::{BoundKind, Defs, Landing, Walker};
use crate::ruby::{Fact, dsl};

/// The methods that change the visibility of methods (or constants) of the
/// class or module they act on.
const VISIBILITY: &[&str] = &[
    "private",
    "protected",
    "public",
    "private_class_method",
    "public_class_method",
    "private_constant",
    "module_function",
];

impl Walker<'_> {
    /// The method `name` the open owner gets on the side `separator`, from
    /// a `def` or a declaration in the frame `at` places from the innermost
    /// one, whose `defs` they are: hidden when a bare `private` or
    /// `protected` is in force there. It hides the methods of the class
    /// itself only in `class << self`, never a `def self.m`.
    pub(in crate::ruby) fn hide_defined(
        &mut self,
        defs: Defs,
        at: usize,
        separator: char,
        name: &str,
    ) {
        let side = if defs == Defs::Singleton { '.' } else { '#' };
        if separator == side && self.locals.hidden(at) {
            self.hide(side, Some(name.to_owned()));
        }
    }

    /// `private :a, "b"`, `protected def c`, `private attr_reader :d`,
    /// `private_class_method :e` in a class or module body: each method
    /// named, its writer too for a declaration; any method of that side for
    /// a name the rules cannot read (`private(*NAMES)`). `module_function
    /// :f` makes the instance method `f` private too (R10-04).
    pub(super) fn hide_named(&mut self, call: Node, method: &str) {
        let side = match method {
            "private" | "protected" if self.locals.current_defs() == Defs::Singleton => '.',
            "private" | "protected" | "module_function" => '#',
            "private_class_method" => '.',
            _ => return,
        };
        if call.child_by_field_name("receiver").is_some()
            || !matches!(
                self.nearest(),
                Some(BoundKind::Class | BoundKind::Module | BoundKind::SingletonClass)
            )
        {
            return;
        }
        let Some(args) = call.child_by_field_name("arguments") else {
            return;
        };
        let mut cursor = args.walk();
        let args: Vec<Node> = args
            .named_children(&mut cursor)
            .filter(|arg| arg.kind() != "comment")
            .collect();
        for arg in args {
            match self.named_by(arg) {
                Some(names) => {
                    for name in names {
                        self.hide(side, Some(name));
                    }
                }
                None => self.hide(side, None),
            }
        }
    }

    /// The methods an argument of a visibility method names: a `def`'s,
    /// a declaration's (its writer too), a literal name; `None` when the
    /// rules cannot read it.
    fn named_by(&self, arg: Node) -> Option<Vec<String>> {
        match arg.kind() {
            "method" | "singleton_method" => self.field_text(arg, "name").map(|n| vec![n]),
            // `attr_reader :a`, `define_method(:b) { }`: the names it
            // defines, every one literal.
            "call" => arg.child_by_field_name("arguments").and_then(|inner| {
                let mut cursor = inner.walk();
                inner
                    .named_children(&mut cursor)
                    .filter(|name| name.kind() != "comment")
                    .map(|name| dsl::literal_name(self, name))
                    .collect::<Option<Vec<String>>>()
                    .map(|names| {
                        names
                            .into_iter()
                            .flat_map(|n| [format!("{n}="), n])
                            .collect()
                    })
            }),
            _ => dsl::literal_name(self, arg).map(|n| vec![n]),
        }
    }

    /// A visibility change the rules do not read (R11-03): a visibility
    /// method sent (`send`, `public_send`, `__send__`), called on a
    /// receiver (`self.private :a`, `K.private_class_method :s`), or given
    /// arguments where definitions do not land on the open owner
    /// (`K.class_eval { private :a }`, `class << K`, a concern's `included
    /// do`, a method body). What it names, any method for a name the rules
    /// cannot read, is hidden on both sides of the class or module it acts
    /// on: the open owner, the constant's, or every one when the rules
    /// cannot name it.
    pub(in crate::ruby) fn hide_unread(&mut self, call: Node, method: &str) {
        let sent = matches!(method, "send" | "public_send" | "__send__");
        if !sent && !VISIBILITY.contains(&method) {
            return;
        }
        let mut cursor = call.walk();
        let mut args: Vec<Node> = call
            .child_by_field_name("arguments")
            .map(|args| {
                args.named_children(&mut cursor)
                    .filter(|arg| !matches!(arg.kind(), "comment" | "pair" | "block_argument"))
                    .collect()
            })
            .unwrap_or_default();
        if sent {
            let verb = args
                .first()
                .and_then(|first| dsl::literal_name(self, *first));
            if !verb.is_some_and(|verb| VISIBILITY.contains(&verb.as_str())) {
                return;
            }
            args.remove(0);
        }
        let receiver = call.child_by_field_name("receiver");
        let in_body = self.locals.current_defs() != Defs::Foreign
            && matches!(
                self.nearest(),
                Some(BoundKind::Class | BoundKind::Module | BoundKind::SingletonClass)
            );
        // A bare word hides what follows it, which lands where the rules
        // place it ([`Self::visibility_word`]); with a receiver and no
        // argument it is no visibility change (`room.private`). With
        // arguments in a body, [`Self::hide_named`] reads it.
        if !sent && (args.is_empty() || receiver.is_none() && in_body) {
            return;
        }
        let names: Option<Vec<String>> = (!args.is_empty())
            .then(|| {
                args.iter()
                    .map(|&arg| self.named_by(arg))
                    .collect::<Option<Vec<_>>>()
            })
            .flatten()
            .map(|names| names.into_iter().flatten().collect());
        let names: Vec<Option<String>> = match names {
            Some(names) => names.into_iter().map(Some).collect(),
            None => vec![None],
        };
        let on_self = receiver.is_none_or(|r| r.kind() == "self");
        let owner = if !on_self {
            None
        } else if in_body {
            self.owners.last().map(|o| o.qualified.clone())
        } else {
            match self.locals.landing() {
                Landing::Owner(owner) | Landing::SingletonOfOwner(owner) => Some(owner),
                Landing::Constant { path, scope, .. } => {
                    return self.hide_on(path, scope, names);
                }
                Landing::SingletonOf { path, scope, .. } => {
                    return self.hide_on(path, scope.to_vec(), names);
                }
                _ => None,
            }
        };
        if let Some(owner) = owner {
            for name in names {
                for side in ['#', '.'] {
                    self.facts.push(Fact::Hidden {
                        owner: owner.clone(),
                        side,
                        name: name.clone(),
                    });
                }
            }
            return;
        }
        let constant = receiver.filter(|_| !on_self).and_then(|r| {
            dsl::constant_text(self, r).or_else(|| {
                let object = r.child_by_field_name("receiver")?;
                (r.kind() == "call"
                    && self.field_text(r, "method").as_deref() == Some("singleton_class"))
                .then(|| dsl::constant_text(self, object))
                .flatten()
            })
        });
        match constant {
            Some(path) => {
                let scope = self.context("", "").cref;
                self.hide_on(path, scope, names);
            }
            None => self.hide_on(String::new(), Vec::new(), names),
        }
    }

    /// [`Fact::Visibility`] for each of `names` on the class or module the
    /// constant `path` names in the nesting `scope`, or on every one.
    fn hide_on(&mut self, path: String, scope: Vec<String>, names: Vec<Option<String>>) {
        for name in names {
            self.facts.push(Fact::Visibility {
                path: path.clone(),
                scope: scope.clone(),
                name,
            });
        }
    }

    /// Code a string evaluates on the class or module the constant `path`
    /// names in the nesting `scope`, or on the open owner for `None`
    /// (`K.class_eval "private :a"`): a visibility method written there
    /// may hide any of its methods (R11-03).
    pub(in crate::ruby) fn evaluated_visibility(
        &mut self,
        code: &str,
        on: Option<(&str, &[String])>,
    ) {
        let mut words = code.split(|c: char| !(c.is_alphanumeric() || c == '_'));
        if !words.any(|word| VISIBILITY.contains(&word)) {
            return;
        }
        match on {
            Some((path, scope)) => self.hide_on(path.to_owned(), scope.to_vec(), vec![None]),
            None => {
                for side in ['#', '.'] {
                    self.hide(side, None);
                }
            }
        }
    }

    /// A bare visibility word ([`Locals::visibility`]); in `class << X`,
    /// where what follows defines methods of `X` itself, `private` and
    /// `protected` hide every method of `X` (R11-03).
    ///
    /// [`Locals::visibility`]: super::locals::Locals::visibility
    pub(in crate::ruby) fn visibility_word(&mut self, word: &str) {
        self.locals.visibility(word);
        if matches!(word, "private" | "protected")
            && let Landing::SingletonOf { path, scope, .. } = self.locals.landing()
        {
            self.hide_on(path, scope.to_vec(), vec![None]);
        }
    }

    /// Records that the open owner's method `name` on `side` (`None`: any)
    /// may be private or protected ([`Fact::Hidden`]).
    fn hide(&mut self, side: char, name: Option<String>) {
        if let Some(owner) = self.owners.last().map(|o| o.qualified.clone()) {
            self.facts.push(Fact::Hidden { owner, side, name });
        }
    }
}
