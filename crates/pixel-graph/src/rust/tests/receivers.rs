// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Receiver typing: what a method call records about the value it is
//! called on (v1's Y-1 to Y-7, port spec 15.1 and 15.2). v1 recorded the
//! type's last segment as the receiver text (`.GitRunner`); the MVP records
//! the whole path and where it is written, so resolution can prove the type
//! (decision 1), and a constructor as such, so resolution can check that it
//! returns `Self`.

use super::super::{Form, Layer, Receiver, TypeRef};
use super::*;

/// The receiver of every call to `callee`, by line.
fn receivers(src: &str, callee: &str) -> Vec<(u32, Receiver)> {
    let x = extract("src/lib.rs", src);
    x.sites
        .iter()
        .zip(contexts(&x))
        .filter_map(|(site, context)| match context.form {
            Form::Method { name, receiver } if name == callee => Some((site.line, receiver)),
            _ => None,
        })
        .collect()
}

/// A stated type: each layer a path, its generic arguments after a `|`
/// (`Vec|u8`, `HashMap|K|V`).
fn typed(scope: u32, paths: &[&str]) -> Receiver {
    Receiver::Type(TypeRef {
        scope,
        layers: paths
            .iter()
            .map(|written| {
                let mut parts = written.split('|');
                let path = parts.next().unwrap_or_default();
                Layer {
                    path: path.split("::").map(str::to_owned).collect(),
                    dynamic: false,
                    args: parts.map(str::to_owned).collect(),
                }
            })
            .collect(),
    })
}

fn ctor(scope: u32, path: &str, method: &str) -> Receiver {
    call(scope, path, method, false)
}

/// What `?`, `.unwrap()` or `.expect(…)` takes out of `path::method(…)`.
fn unwrapped(scope: u32, path: &str, method: &str) -> Receiver {
    call(scope, path, method, true)
}

fn call(scope: u32, path: &str, method: &str, unwrapped: bool) -> Receiver {
    Receiver::Ctor {
        scope,
        path: path.split("::").map(str::to_owned).collect(),
        method: method.to_owned(),
        unwrapped,
    }
}

#[test]
fn a_local_records_the_type_its_binding_states() {
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
    // v1 read `Box<GitRunner>` as `Box` and `Self` as the impl's name;
    // decision 1 unwraps the pointer and leaves `Self` to resolution.
    assert_eq!(
        receivers(&source, "current_branch"),
        [
            (1, typed(0, &["pixel_git::GitRunner"])),
            (2, typed(0, &["GitRunner"])),
            (3, ctor(0, "GitRunner", "new")),
            (4, typed(0, &["Box|GitRunner", "GitRunner"])),
            (
                5,
                Receiver::Literal {
                    scope: 0,
                    path: vec!["Runner".to_owned()]
                }
            ),
            (6, ctor(0, "GitRunner", "new")),
            (7, ctor(0, "git::GitRunner", "default")),
            (8, typed(0, &["GitRunner"])),
            (9, ctor(0, "GitRunner", "new")),
            (10, typed(0, &["GitRunner"])),
            (12, ctor(1, "Self", "new")),
            (
                12,
                Receiver::Literal {
                    scope: 1,
                    path: vec!["Self".to_owned()]
                }
            ),
        ]
    );
}

#[test]
fn a_local_without_a_stated_type_is_unknown() {
    let source = [
        "fn untyped(runner: &GitRunner) { let runner = open(); runner.current_branch(); }",
        "fn method_value(runner: &GitRunner) { let runner = runner.open()?; runner.current_branch(); }",
        "fn looped(runner: &GitRunner) { for runner in all() { runner.current_branch(); } }",
        "fn matched(runner: &GitRunner) { match x { Some(runner) => runner.current_branch(), _ => {} } }",
        "fn if_let(runner: &GitRunner) { if let Some(runner) = x { runner.current_branch(); } }",
        "fn closure_param(runner: &GitRunner) { let f = |runner| runner.current_branch(); }",
        "fn destructured(runner: &GitRunner) { let (runner, _) = pair(); runner.current_branch(); }",
        "fn closed_block() { { let runner = GitRunner::new(r); } runner.current_branch(); }",
        "fn later() { runner.current_branch(); let runner = GitRunner::new(r); }",
        "fn unwrap_or(r: &Path) { let runner = GitRunner::open(r).unwrap_or(d); runner.current_branch(); }",
        "fn wrapped(r: &Path) { let Wrapper(runner) = Wrapper::new(r); runner.current_branch(); }",
        "fn wrapped_param(Wrapper(runner): Wrapper) { runner.current_branch(); }",
        "fn shorthand(runner: &GitRunner) { let Pair { runner, .. } = p; runner.current_branch(); }",
        "fn generic<R: Branch>(runner: R) { runner.current_branch(); }",
        "fn field(&self) { self.runner.current_branch(); }",
    ]
    .join("\n");
    // v1 kept the expression (`.runner`); the MVP records nothing known. Two
    // of v1's lines changed (Y-8): a struct pattern's shorthand field binds
    // `runner` (v1 let the parameter's type leak through), and a generic
    // parameter names no type.
    let unknown: Vec<u32> = receivers(&source, "current_branch")
        .into_iter()
        .map(|(line, receiver)| {
            assert_eq!(receiver, Receiver::Unknown, "line {line}");
            line
        })
        .collect();
    assert_eq!(unknown, (1..=15).collect::<Vec<u32>>());
}

#[test]
fn a_binding_applies_only_where_rust_puts_it_in_scope() {
    let source = [
        "fn f(item: Item, maybe: Option<Item>) {",
        "    if let Some(item) = item.next() {",
        "        item.visit();",
        "    } else {",
        "        item.fallback();",
        "    }",
        "    for item in item.children() {}",
        "    match maybe { Some(i) if i.ok() => {}, _ => {} }",
        "}",
    ]
    .join("\n");
    // `item.next()` and `item.children()` read the parameter: a pattern binds
    // in the body, not in its own value or iterator, nor in an `else` (v1
    // treated all three as rebound). A guard sees its arm's pattern (v1 did
    // not, Y-8).
    assert_eq!(receivers(&source, "next"), [(2, typed(0, &["Item"]))]);
    assert_eq!(receivers(&source, "visit"), [(3, Receiver::Unknown)]);
    assert_eq!(receivers(&source, "fallback"), [(5, typed(0, &["Item"]))]);
    assert_eq!(receivers(&source, "children"), [(7, typed(0, &["Item"]))]);
    assert_eq!(receivers(&source, "ok"), [(8, Receiver::Unknown)]);
}

#[test]
fn a_type_is_read_through_references_and_pointers_and_trait_objects() {
    let source = [
        "fn f(r: &mut crate::git::Runner<'_>, list: Vec<u8>, t: impl Tool + Send, d: &dyn Tr, s: Arc<Mutex<S>>, b: Box<dyn Tr>) {",
        "    r.exec(); list.len(); t.call(); d.call(); s.lock(); b.call();",
        "}",
    ]
    .join("\n");
    let dynamic = |path: &str| Layer {
        path: vec![path.to_owned()],
        dynamic: true,
        args: Vec::new(),
    };
    assert_eq!(
        receivers(&source, "exec"),
        [(2, typed(0, &["crate::git::Runner"]))]
    );
    assert_eq!(receivers(&source, "len"), [(2, typed(0, &["Vec|u8"]))]);
    assert_eq!(
        receivers(&source, "lock"),
        [(2, typed(0, &["Arc|Mutex<S>", "Mutex|S"]))]
    );
    // v1 kept `.t` for `impl Tool` and had no trait objects (Y-6).
    let calls = receivers(&source, "call");
    assert_eq!(
        calls,
        [
            (
                2,
                Receiver::Type(TypeRef {
                    scope: 0,
                    layers: vec![dynamic("Tool")]
                })
            ),
            (
                2,
                Receiver::Type(TypeRef {
                    scope: 0,
                    layers: vec![dynamic("Tr")]
                })
            ),
            (
                2,
                Receiver::Type(TypeRef {
                    scope: 0,
                    layers: vec![
                        Layer {
                            path: vec!["Box".to_owned()],
                            dynamic: false,
                            args: vec!["dyn Tr".to_owned()],
                        },
                        dynamic("Tr")
                    ]
                })
            ),
        ]
    );
}

#[test]
fn closure_parameters_bind_and_shadow() {
    let source = "fn f(x: Outer) {\n    let g = |x: Inner| x.go();\n    let h = |x| x.stop();\n    x.outer();\n}\n";
    assert_eq!(receivers(source, "go"), [(2, typed(0, &["Inner"]))]);
    // An unannotated closure parameter shadows the function's `x: Outer`.
    assert_eq!(receivers(source, "stop"), [(3, Receiver::Unknown)]);
    assert_eq!(receivers(source, "outer"), [(4, typed(0, &["Outer"]))]);
}

#[test]
fn the_last_let_before_the_call_wins_and_an_outer_block_is_seen() {
    let source = "fn f() {\n    let s = First::new();\n    let s = Second::new();\n    s.go();\n    let s = Third::new();\n    {\n        let other = 1;\n        s.query();\n    }\n}\n";
    assert_eq!(receivers(source, "go"), [(4, ctor(0, "Second", "new"))]);
    assert_eq!(receivers(source, "query"), [(8, ctor(0, "Third", "new"))]);
}

#[test]
fn self_is_the_impl_type_and_a_type_is_read_where_it_is_written() {
    let source = "impl Store {\n    fn a(&self) { self.b(); }\n    fn c(self: Box<Self>) { let f = || self.b(); }\n}\nfn f() {\n    struct Local;\n    let x: Local = make();\n    x.go();\n}\n";
    // A typed `self` is what its type says: `Box<Self>` is a `Box`.
    assert_eq!(
        receivers(source, "b"),
        [
            (2, Receiver::SelfValue),
            (3, typed(1, &["Box|Self", "Self"]))
        ]
    );
    // The block declares an item, so it is a scope (2) and `Local` is read in it.
    assert_eq!(receivers(source, "go"), [(8, typed(2, &["Local"]))]);
}

#[test]
fn a_call_to_any_associated_function_and_what_unwrapping_it_gives_are_recorded() {
    let source = [
        "fn opener(r: &Path) { GitRunner::open(r).current_branch(); }",
        "fn fallible(r: &Path) { let runner = GitRunner::open(r)?; runner.current_branch(); }",
        "fn chained(r: &Path) { vcs::GitRunner::open(r)?.current_branch(); }",
        "fn unwrapped(r: &Path) { let runner = GitRunner::open(r).unwrap(); runner.current_branch(); }",
        "fn expected(r: &Path) { let runner = GitRunner::open(r).expect(\"vcs\"); runner.current_branch(); }",
        "fn twice(r: &Path) { let runner = GitRunner::open(r).unwrap().unwrap(); runner.current_branch(); }",
        "fn generic<R: Open>(r: &Path) { let runner = R::open(r)?; runner.current_branch(); }",
    ]
    .join("\n");
    // v1 typed `T::new` and `T::default` only; resolution proves what any
    // associated function returns (and what is inside a `Result` or an
    // `Option` of it), so the call is recorded whatever its name. Nothing
    // is unwrapped twice, and a generic parameter names no type.
    assert_eq!(
        receivers(&source, "current_branch"),
        [
            (1, ctor(0, "GitRunner", "open")),
            (2, unwrapped(0, "GitRunner", "open")),
            (3, unwrapped(0, "vcs::GitRunner", "open")),
            (4, unwrapped(0, "GitRunner", "open")),
            (5, unwrapped(0, "GitRunner", "open")),
            (6, Receiver::Unknown),
            (7, Receiver::Unknown),
        ]
    );
}
