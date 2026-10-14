// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Constants as the walker reads them: where Ruby defines a class, module
//! or assigned constant (D09, D11, R2, R8, B10), and the classes a
//! constructor assigned to a constant makes (D01).

use std::collections::HashSet;
use std::rc::Rc;

use mvp_model::{EdgeKind, SymbolKind};
use tree_sitter::Node;

use super::{BoundKind, Defs, Gate, Landing, Owner, This, Walker, line, one_line};
use crate::lang::FILE_OWNER;
use crate::ruby::{Context, Fact, MIXIN, dsl};

/// What a definition keeps of its nesting, in bytes: each scope's name and
/// 32 more for its copy (R10-03).
const KEPT_NESTING: usize = 256;

/// The innermost scopes of the nesting `cref` (outermost first) that fit
/// in [`KEPT_NESTING`], innermost first, at least one: a
/// [`Fact::Definee`] per definition kept the whole nesting, which costs
/// the square of its depth (R10-03). Past the scopes kept, resolution
/// reads the constant as past every scope (the ancestors of the
/// innermost): any class or module of the generation holding the path,
/// which may only add doubt.
fn kept(cref: &[String]) -> Rc<[String]> {
    cref.iter()
        .rev()
        .take(fitting(cref.iter().rev()))
        .cloned()
        .collect()
}

/// How many of the innermost scopes of `scopes` (innermost first) fit in
/// [`KEPT_NESTING`], at least one.
fn fitting<'s>(scopes: impl Iterator<Item = &'s String>) -> usize {
    let mut size = 0;
    let fit = scopes
        .take_while(|name| {
            size += name.len() + 32;
            size <= KEPT_NESTING
        })
        .count();
    fit.max(1)
}

impl<'a> Walker<'a> {
    /// The constant `node` is, for a definition on its class (`def X.m`,
    /// `class << X`, B2): its path, what a definition keeps of the nesting
    /// ([`Self::kept_scope`]), and the class or module this file reads it
    /// as.
    pub(super) fn definee(&self, node: Option<Node>) -> Option<(String, Rc<[String]>, String)> {
        let path = dsl::constant_text(self, node?)?;
        let read = self.refer(&path);
        Some((path, self.kept_scope(), read))
    }

    /// What a definition on a constant keeps of the nesting here
    /// ([`kept`]), built once for every one in it: each `def X.m` or
    /// `class << X` copied the whole nesting, about 119 KB 49 modules deep
    /// with 95-character names (R11-02).
    pub(super) fn kept_scope(&self) -> Rc<[String]> {
        Rc::clone(self.kept.get_or_init(|| kept(&self.cref)))
    }

    /// The cref entry of the body of `class << X` for a constant `X`: the
    /// singleton class of the class or module this file reads `X` as
    /// (B2), with the [`Fact::Definee`] that lets resolution check it.
    #[inline(never)]
    pub(super) fn singleton_body(&mut self) -> Option<String> {
        let Landing::SingletonOf { path, scope, read } = self.locals.landing() else {
            return None;
        };
        let name = crate::ruby::singleton_scope(&read);
        self.facts.push(Fact::Definee {
            name: name.clone(),
            written: path,
            scope,
        });
        Some(name)
    }

    /// The method `name` of the singleton class of the constant `path`,
    /// written in the nesting `scope`: `read.name`, `read` being the class
    /// or module this file reads `path` as, which a [`Fact::Definee`] lets
    /// resolution check (B2).
    pub(super) fn defined_on(
        &mut self,
        path: String,
        scope: Rc<[String]>,
        read: &str,
        name: &str,
        node: Node,
    ) -> Option<usize> {
        let qualified = format!("{read}.{name}");
        self.facts.push(Fact::Definee {
            name: qualified.clone(),
            written: path,
            scope,
        });
        self.push_def(SymbolKind::Method, qualified, node, false)
    }

    /// Where the `def`s of `class << x` land, for an `x` that is not
    /// `self`: on the class or module a constant names (B2); maybe on the
    /// owner's own singleton class for another expression in its body,
    /// which may be the class itself; else on one object's singleton
    /// class, which no lookup of a named owner reads. An empty body lands
    /// nothing, and is not read (R11-02).
    pub(super) fn singleton_landing(&self, node: Node) -> Landing {
        let body = node.child_by_field_name("body");
        let mut cursor = node.walk();
        if body.is_none_or(|body| {
            body.named_children(&mut cursor)
                .all(|child| child.kind() == "comment")
        }) {
            return Landing::Nowhere;
        }
        if let Some((path, scope, read)) = self.definee(node.child_by_field_name("value")) {
            return Landing::SingletonOf { read, scope, path };
        }
        match self.owners.last() {
            Some(owner) if self.locals.gate() == Gate::Owner => {
                Landing::SingletonOfOwner(owner.qualified.clone())
            }
            _ => Landing::Nowhere,
        }
    }

    /// The constant path `name` (a class, a module, an assigned constant)
    /// qualified where Ruby defines it: in the innermost lexical scope,
    /// which a block does not change (`class_methods do`, `Class.new do`);
    /// from the top level after a leading `::`. A compact `A::B` defines
    /// `B` in whatever `A` Ruby's lexical lookup finds (D09): `Scope::A`
    /// for the nearest scope this file has already given an `A`, else the
    /// top-level `A`. When another file could give a nearer scope an `A`,
    /// a [`Fact::Compact`] says so, and resolution abstains where that
    /// reading matters.
    pub(in crate::ruby) fn qualify(&mut self, name: &str) -> String {
        if let Some(rooted) = name.strip_prefix("::") {
            return rooted.to_owned();
        }
        let Some(innermost) = self.cref.last() else {
            return name.to_owned();
        };
        let Some((first, _)) = name.split_once("::") else {
            return format!("{innermost}::{name}");
        };
        let scopes: Vec<String> = self.cref.iter().rev().cloned().collect();
        let found = scopes
            .iter()
            .position(|scope| self.defines_constant(&format!("{scope}::{first}")));
        let (qualified, nearer) = match found {
            Some(at) => (format!("{}::{name}", scopes[at]), scopes[..at].to_vec()),
            // Else through the ancestors of the innermost scope, as this
            // file declares them so far (B10); another file's may come
            // first, which the fact's `top` lets resolution check.
            None => match self
                .ancestors_in_file(innermost)
                .into_iter()
                .find(|ancestor| self.defines_constant(&format!("{ancestor}::{first}")))
            {
                Some(ancestor) => (format!("{ancestor}::{name}"), scopes),
                None => (name.to_owned(), scopes),
            },
        };
        if !nearer.is_empty() {
            // Past what a definition keeps of its nesting, any scope may
            // be a nearer one (R10-03).
            let deep = fitting(nearer.iter()) < nearer.len();
            self.facts.push(Fact::Compact {
                name: qualified.clone(),
                written: name.to_owned(),
                top: found.is_none() || deep,
                nearer: if deep { Vec::new() } else { nearer },
            });
        }
        qualified
    }

    /// `X = Struct.new(..)`, `Class.new(..)`, `Data.define(..)` (a class)
    /// or `Module.new` (a module): the kind of the constant `call` defines
    /// and the assignment, whose left side names it.
    pub(super) fn constructor(&self, call: Node<'a>) -> Option<(SymbolKind, Node<'a>)> {
        // In `class << x` the constant is the singleton class's of an
        // object the rules cannot name; under a condition it may never be
        // assigned (R6-05).
        if self.nearest() == Some(BoundKind::SingletonClass)
            && self.locals.current_defs() != Defs::Singleton
            || self.branches > 0
        {
            return None;
        }
        let assignment = call.parent().filter(|p| p.kind() == "assignment")?;
        if assignment.child_by_field_name("right")?.id() != call.id() {
            return None;
        }
        dsl::constant_text(self, assignment.child_by_field_name("left")?)?;
        let receiver = dsl::constant_text(self, call.child_by_field_name("receiver")?)?;
        let method = self.field_text(call, "method")?;
        let symbol = match (receiver.trim_start_matches("::"), method.as_str()) {
            ("Struct" | "Class", "new") | ("Data", "define") => SymbolKind::Class,
            ("Module", "new") => SymbolKind::Module,
            _ => return None,
        };
        Some((symbol, assignment))
    }

    /// `X = Struct.new(..) do ... end` and its family: the class or module
    /// `X`, owner of the block's `def`s and of a Struct's or Data's
    /// members, with its superclass: the argument of `Class.new(Base)`,
    /// `Object` for a bare `Class.new` (none recorded), and `Struct` or
    /// `Data`, which the graph never holds. Returns whether it pushed the
    /// owner.
    pub(in crate::ruby) fn constructed(&mut self, call: Node<'a>) -> bool {
        let Some((symbol, assignment)) = self.constructor(call) else {
            return false;
        };
        let (Some(name), Some(receiver)) = (
            assignment
                .child_by_field_name("left")
                .and_then(|left| dsl::constant_text(self, left)),
            call.child_by_field_name("receiver"),
        ) else {
            return false;
        };
        let class_new = self.text(receiver).trim_start_matches("::") == "Class";
        let parent = match symbol {
            SymbolKind::Module => None,
            _ if class_new => call
                .child_by_field_name("arguments")
                .and_then(|args| dsl::positional(args).first().copied()),
            _ => Some(receiver),
        };
        let qualified = self.qualify(&name);
        let Some(def) = self.push_def(symbol, qualified.clone(), assignment, false) else {
            return false;
        };
        self.facts.push(Fact::Constructed {
            name: qualified.clone(),
        });
        self.owners.push(Owner { qualified, def });
        match self.text(receiver).trim_start_matches("::") {
            "Struct" => dsl::members(self, call, true),
            "Data" => dsl::members(self, call, false),
            _ => {}
        }
        if let Some(parent) = parent {
            let known = class_new
                .then(|| dsl::constant_text(self, parent))
                .flatten();
            let qualifier = format!("{}superclass", super::MIXIN);
            // Read where the assignment is, as `class X < Base` reads its
            // superclass outside the class.
            let context = self.context(known.as_deref().unwrap_or_default(), &qualifier);
            let written = known.unwrap_or_else(|| one_line(&self.text(parent)));
            self.push_site(
                EdgeKind::Reference,
                line(parent),
                Some(def),
                written,
                &context,
            );
        }
        true
    }

    /// `X = v`, `X += v`, `A::X = v`, `self::X = v`, `X, Y = v`: each
    /// constant Ruby defines in the scope around it, a `Const` whose value
    /// the rules do not read (D11, R8); `X ||= v`, which may assign nothing,
    /// a [`Fact::MaybeConstant`] (R5-12), and so is an assignment that may
    /// not run (`X = v unless defined?(X)`, `defined?(X) || X = v`, R6-05),
    /// whatever its value. A constant assigned in a method
    /// body is a syntax error, and one in `class << x` the singleton
    /// class's of an object the rules cannot name: neither is recorded; one
    /// in the owner's `class << self` is its singleton class's
    /// ([`super::singleton_scope`], R2). A constructor's constant is its
    /// class ([`Self::constructed`]).
    pub(in crate::ruby) fn constants(&mut self, node: Node<'a>) {
        let own_singleton = self.nearest() == Some(BoundKind::SingletonClass)
            && (self.locals.current_defs() == Defs::Singleton
                || matches!(self.locals.landing(), Landing::SingletonOf { .. }));
        if !own_singleton
            && !matches!(
                self.nearest(),
                None | Some(BoundKind::Class | BoundKind::Module | BoundKind::Foreign)
            )
        {
            return;
        }
        if node
            .child_by_field_name("right")
            .is_some_and(|right| right.kind() == "call" && self.constructor(right).is_some())
        {
            return;
        }
        let Some(left) = node.child_by_field_name("left") else {
            return;
        };
        let targets = if left.kind() == "left_assignment_list" {
            let mut cursor = left.walk();
            left.named_children(&mut cursor).collect()
        } else {
            vec![left]
        };
        let single = targets.len() == 1 && node.kind() == "assignment";
        // `X ||= v` assigns only when no `X` is found, `X &&= v` only when
        // one is (R5-12); under a condition, a loop, a `rescue` or a block,
        // it may not run at all (R6-05).
        let maybe = node.kind() == "operator_assignment"
            && node
                .child_by_field_name("operator")
                .is_some_and(|op| matches!(self.text(op).as_str(), "||=" | "&&="))
            || self.branches > 0;
        for target in targets {
            // `X::Y = v`, `::Y = v`, `self::Y = v` (R8-09).
            if target.kind() == "scope_resolution" {
                let value = node.child_by_field_name("right").filter(|_| single);
                let name = target
                    .child_by_field_name("name")
                    .map(|name| self.text(name));
                if let Some(name) = name {
                    self.alias(&name, value);
                }
            }
            if let Some(name) = self.assigned_constant(target) {
                if maybe {
                    let segment = name.rsplit("::").next().unwrap_or(&name).to_owned();
                    self.facts.push(Fact::MaybeConstant { segment });
                    continue;
                }
                let qualified = self.qualify(&name);
                // A literal array of names, which a splat may read, or of
                // constants, which a loop may include; a new value
                // replaces it.
                self.arrays.remove(&qualified);
                self.modules.remove(&qualified);
                let right = node.child_by_field_name("right").filter(|_| single);
                if let Some(names) = right.and_then(|right| dsl::literal_array(self, right)) {
                    self.arrays.insert(qualified.clone(), names);
                } else if let Some(paths) = right.and_then(|right| dsl::constant_array(self, right))
                {
                    self.modules
                        .insert(qualified.clone(), (self.cref.clone(), paths));
                }
                self.push_def(SymbolKind::Const, qualified, node, false);
            }
        }
    }

    /// A read of a constant holding a literal array: past any read but a
    /// splat (`*ATTRS` copies it) or a call that leaves it as it is
    /// (`MODULES.each`), the array may have changed (`ATTRS << :c`,
    /// `helper(ATTRS)`), so the walk no longer reads it.
    #[inline(never)]
    pub(super) fn read_array(&mut self, node: Node, parent: Option<Node>, field: Option<&str>) {
        if self.arrays.is_empty() && self.modules.is_empty() {
            return;
        }
        let kept = parent.is_some_and(|parent| match parent.kind() {
            "splat_argument" => true,
            "assignment" => field == Some("left"),
            "call" => {
                field == Some("receiver")
                    && self
                        .field_text(parent, "method")
                        .is_some_and(|m| KEEPS_ARRAY.contains(&m.as_str()))
            }
            _ => false,
        });
        if kept {
            return;
        }
        let Some(path) = dsl::constant_text(self, node) else {
            return;
        };
        let qualified = self.refer(&path);
        self.arrays.remove(&qualified);
        self.modules.remove(&qualified);
    }

    /// The constant an assignment target names: its path, or `X` for
    /// `self::X` in a class or module body, where `self` is the innermost
    /// lexical scope (R8, [`Self::self_is_scope`]).
    pub(in crate::ruby) fn assigned_constant(&self, target: Node) -> Option<String> {
        if let Some(path) = dsl::constant_text(self, target) {
            return Some(path);
        }
        let on_self = target.kind() == "scope_resolution"
            && target
                .child_by_field_name("scope")
                .is_some_and(|scope| scope.kind() == "self");
        let name = target
            .child_by_field_name("name")
            .filter(|name| on_self && name.kind() == "constant")?;
        let body = matches!(
            self.nearest(),
            Some(BoundKind::Class | BoundKind::Module | BoundKind::SingletonClass)
        ) && self.self_is_scope();
        body.then(|| self.text(name))
    }

    /// Whether `self` is the class or module of the innermost lexical
    /// scope, where a constant set on `self` lands: not in the block of
    /// `Class.new`, `Module.new`, `Struct.new`, `Data.define`, `concerning`
    /// or `class_methods`, whose `self` is the class or module it makes
    /// while the scope stays the one around (R9-02).
    fn self_is_scope(&self) -> bool {
        self.this() == This::Own
            && (self.nearest() == Some(BoundKind::SingletonClass)
                || self.owners.last().map(|owner| owner.qualified.as_str())
                    == self.cref.last().map(String::as_str))
    }

    /// `const_set(:X, v)` in a class or module body: the constant `X` of
    /// the class, a `Const` whose value the rules do not read (R8), or one
    /// that may not be assigned under a condition (R6-05); `Object.const_set(:X,
    /// v)` anywhere, the top-level `X`, which a `class X` may then reopen
    /// (R7-11). In a block whose `self` is a class or module it makes
    /// (`Class.new do`), nothing ([`Self::self_is_scope`], R9-02).
    pub(in crate::ruby) fn const_set(&mut self, call: Node) {
        let mut args = call
            .child_by_field_name("arguments")
            .map(|args| dsl::positional(args))
            .unwrap_or_default();
        // `send(:const_set, :X, v)`: the arguments past the name sent.
        let sent = matches!(
            self.field_text(call, "method").as_deref(),
            Some("send" | "__send__" | "public_send")
        );
        if sent && !args.is_empty() {
            args.remove(0);
        }
        // On any receiver, sent or not, it may alias a class (R8-09).
        if let Some(name) = args
            .first()
            .and_then(|&first| dsl::literal_name(self, first))
        {
            self.alias(&name, args.get(1).copied());
        }
        let receiver = self.field_text(call, "receiver");
        let on_object = receiver
            .as_deref()
            .is_some_and(|r| r.trim_start_matches("::") == "Object");
        if !on_object
            && (!matches!(self.nearest(), Some(BoundKind::Class | BoundKind::Module))
                || !self.self_is_scope()
                || receiver.is_some_and(|r| r != "self"))
        {
            return;
        }
        let Some(name) = args
            .first()
            .and_then(|&first| dsl::literal_name(self, first))
            .filter(|name| name.chars().next().is_some_and(char::is_uppercase))
        else {
            return;
        };
        if self.branches > 0 || self.locals.in_method() {
            self.facts.push(Fact::MaybeConstant { segment: name });
            return;
        }
        let qualified = if on_object { name } else { self.qualify(&name) };
        self.push_def(SymbolKind::Const, qualified, call, false);
    }

    /// Records that the constant `name` (a path, of which the last segment
    /// counts) is assigned `value` in a namespace the rules do not follow,
    /// when that value may be a class or module: anything but a literal
    /// ([`Fact::Alias`], R8-09).
    pub(in crate::ruby) fn alias(&mut self, name: &str, value: Option<Node>) {
        let literal = value.is_some_and(|value| {
            matches!(
                value.kind(),
                "string"
                    | "simple_symbol"
                    | "delimited_symbol"
                    | "integer"
                    | "float"
                    | "rational"
                    | "complex"
                    | "nil"
                    | "true"
                    | "false"
                    | "array"
                    | "string_array"
                    | "symbol_array"
                    | "hash"
                    | "regex"
                    | "heredoc_beginning"
                    | "lambda"
            )
        });
        let segment = name.rsplit("::").next().unwrap_or(name);
        if !literal && segment.chars().next().is_some_and(char::is_uppercase) {
            self.facts.push(Fact::Alias {
                segment: segment.to_owned(),
            });
        }
    }

    /// The class or module the constant `path`, read here, names as far as
    /// this file tells: `Scope::path` for the nearest lexical scope this
    /// file has already given the path's first segment, else `path` itself,
    /// the top-level reading (B2). Ruby also looks through the ancestors of
    /// the innermost scope, and another file may define a nearer one: a
    /// [`Fact::Definee`] lets resolution check.
    pub(in crate::ruby) fn refer(&self, path: &str) -> String {
        self.read_through(path, self.cref.iter().rev())
    }

    /// [`Self::refer`] in the nesting `cref`, innermost first.
    pub(in crate::ruby) fn read_in(&self, path: &str, cref: &[String]) -> String {
        self.read_through(path, cref.iter())
    }

    /// [`Self::refer`] through `scopes`, innermost first.
    fn read_through<'s>(&self, path: &str, scopes: impl Iterator<Item = &'s String>) -> String {
        if let Some(rooted) = path.strip_prefix("::") {
            return rooted.to_owned();
        }
        let first = path.split("::").next().unwrap_or(path);
        for scope in scopes {
            if self.defines_constant(&format!("{scope}::{first}")) {
                return format!("{scope}::{path}");
            }
        }
        path.to_owned()
    }

    /// Whether this file has defined the class, module or constant `name`
    /// so far.
    pub(in crate::ruby) fn defines_constant(&self, name: &str) -> bool {
        self.constants.contains(name)
    }

    /// The ancestors of the class or module `scope` that this file has
    /// declared so far, in the order Ruby's constant lookup reads them: its
    /// prepended, then its included modules, the latest first, each with
    /// the modules it includes, then its superclass and that one's; each
    /// constant read where it is written (B10).
    pub(in crate::ruby) fn ancestors_in_file(&self, scope: &str) -> Vec<String> {
        self.ancestors_of(scope, 0, &mut HashSet::from([scope.to_owned()]))
    }

    /// [`Self::ancestors_in_file`] from `depth` levels down, each ancestor
    /// once: one `seen` already is where Ruby has it, and a cycle ends
    /// there (R5-01).
    fn ancestors_of(&self, scope: &str, depth: usize, seen: &mut HashSet<String>) -> Vec<String> {
        if depth > 8 {
            return Vec::new();
        }
        let (mut prepends, mut includes, mut superclass) = (Vec::new(), Vec::new(), None);
        for site in &self.sites {
            let Some(owner) = site.owner.filter(|&owner| owner != FILE_OWNER) else {
                continue;
            };
            if self.defs.get(owner).is_none_or(|def| def.name != scope) {
                continue;
            }
            let context = Context::decode(&site.context);
            let Some(kind) = context.qualifier.strip_prefix(MIXIN) else {
                continue;
            };
            if context.name.is_empty() {
                continue;
            }
            let read = self.read_in(&context.name, &context.cref);
            match kind {
                "prepend" => prepends.push(read),
                "include" => includes.push(read),
                "superclass" => superclass = Some(read),
                _ => {}
            }
        }
        let mut ancestors = Vec::new();
        let modules = prepends.into_iter().rev().chain(includes.into_iter().rev());
        for ancestor in modules.chain(superclass) {
            if !seen.insert(ancestor.clone()) {
                continue;
            }
            let theirs = self.ancestors_of(&ancestor, depth + 1, seen);
            ancestors.push(ancestor);
            ancestors.extend(theirs);
        }
        ancestors
    }
}

/// The calls on an array that leave it as it is.
const KEEPS_ARRAY: &[&str] = &[
    "each",
    "each_with_index",
    "map",
    "include?",
    "freeze",
    "frozen?",
    "dup",
    "to_a",
    "size",
    "length",
    "empty?",
    "any?",
    "first",
    "last",
    "join",
];
