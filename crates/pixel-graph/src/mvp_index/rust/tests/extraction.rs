// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Extraction: v1's tests of the file gate, the walker, the symbols and the
//! sites (port spec 15.1 and 15.2), with the MVP's mapping. A comment marks
//! each expectation that differs from v1 and the decision that changed it.

use std::time::Duration;

use mvp_model::{EdgeKind, SymbolKind};
use tree_sitter::Node;

use super::super::extract::{MAX_DEPTH, parse_within};
use super::super::{Argument, Form, ItemKind, ModPath, ScopeKind};
use super::*;

#[test]
fn rust_reads_rs_files_by_the_extension_of_the_file_name_and_every_manifest() {
    let rules = Rust;
    assert!(rules.wants("src/a.rs"));
    assert!(rules.wants("main.rs"));
    assert!(rules.wants("crates/x/Cargo.toml"));
    assert!(rules.wants("Cargo.toml"));
    // The extension of a directory is not the file's (v1's G-1).
    assert!(!rules.wants("pkg.rs/README"));
    assert!(!rules.wants("notes.txt"));
    assert!(!rules.wants("Makefile"));
    assert!(!rules.wants("Cargo.lock"));
    assert!(!rules.wants("src/Cargo.toml.orig"));
}

#[test]
fn the_generated_blob_guard_is_exact_on_both_conditions() {
    use crate::mvp_index::guard::{
        GENERATED_MAX_BYTES_PER_LINE as PER_LINE, GENERATED_MIN_BYTES as MIN,
    };
    let one_line = |len: usize| vec![b'x'; len];
    // At the size floor, one line is a blob; one byte under, it is source.
    assert!(!Rust.accepts("a.rs", &one_line(MIN)));
    assert!(Rust.accepts("a.rs", &one_line(MIN - 1)));
    // Lines of exactly the mean are a blob; one byte shorter, source.
    let lines = |width: usize| {
        let line = format!("{}\n", "x".repeat(width - 1));
        line.repeat(MIN / width + 1).into_bytes()
    };
    assert!(!Rust.accepts("a.rs", &lines(PER_LINE)));
    assert!(Rust.accepts("a.rs", &lines(PER_LINE - 1)));
    // A manifest is never refused.
    assert!(Rust.accepts("Cargo.toml", &one_line(MIN)));
}

#[test]
fn a_parse_past_its_budget_gives_no_tree() {
    let long = "fn f() { g(1); }\n".repeat(20_000);
    assert!(parse_within(long.as_bytes(), Duration::from_secs(3)).is_some());
    assert!(parse_within(long.as_bytes(), Duration::ZERO).is_none());
}

/// Depth of `node` in its tree, the root being 0.
fn depth_of(node: Node) -> usize {
    std::iter::successors(node.parent(), |n| n.parent()).count()
}

#[test]
fn the_walk_visits_the_depth_cap_and_stops_past_it() {
    // `g()` wrapped in `n` parentheses; the call's depth grows by one each.
    let source = |n: usize| format!("fn f() {{ {}g(){}; }}\n", "(".repeat(n), ")".repeat(n));
    let call_depth = |n: usize| {
        let text = source(n);
        let tree = parse_within(text.as_bytes(), Duration::from_secs(3)).expect("parses");
        let offset = text.find("g(").expect("the call");
        let call = tree
            .root_node()
            .descendant_for_byte_range(offset, offset)
            .and_then(|n| n.parent())
            .expect("the call node");
        depth_of(call)
    };
    let base = call_depth(0);
    assert_eq!(call_depth(3), base + 3);
    let at_cap = MAX_DEPTH - base;
    assert_eq!(call_depth(at_cap), MAX_DEPTH);
    let calls = |n: usize| sites(&extract("src/lib.rs", &source(n))).len();
    assert_eq!(calls(at_cap), 1, "depth {MAX_DEPTH} itself is visited");
    assert_eq!(calls(at_cap + 1), 0, "one deeper is skipped");
}

const MANIFEST: &str = "[package]\nname = \"app\"\n";

// D17: `types::segments` recursed once per segment, so this path overflowed
// the stack of the thread extracting it and aborted every `index`.
#[test]
fn c18_rust_a_path_of_twenty_thousand_segments_is_read_without_recursion() {
    let path = format!("a{}", "::\n    a".repeat(20_000));
    let src = format!("pub fn f() {{\n    {path}();\n}}\n");
    let linked = link(&[("Cargo.toml", MANIFEST), ("src/lib.rs", &src)]);
    let call = linked
        .iter()
        .find(|l| l.kind == EdgeKind::Call)
        .expect("the call is a site");
    assert_eq!((call.line, call.dst.as_deref()), (20_002, None));
    assert_eq!(call.target.matches("::").count(), 20_000);
}

// D17 (review of the fix): an impl in blocks nested past the walker's
// depth cap was dropped without a trace, and the impls of a type apply
// wherever they sit: the trait method was linked where rustc runs the
// skipped inherent one.
#[test]
fn c10_rust_an_impl_below_the_depth_cap_leaves_its_names_unresolved() {
    let head = "pub struct S;\nimpl S { pub fn new() -> S { S } }\npub trait Tr { fn go(&self); }\n\
                impl Tr for S { fn go(&self) {} }\npub fn c1() { S::new().go(); }\n";
    let deep = |depth: usize, inner: &str| {
        format!(
            "{head}fn hidden() {{\n{}{inner}\n{}}}\n",
            "{\n".repeat(depth),
            "}\n".repeat(depth)
        )
    };
    let go = |src: &str| {
        let linked = link(&[("Cargo.toml", MANIFEST), ("src/lib.rs", src)]);
        linked
            .into_iter()
            .find(|l| l.target == "S::new().go")
            .expect("the call is a site")
            .dst
    };
    // rustc 1.99 runs the inherent `go(self)` the impl at depth 300 or 100
    // writes, and `<S as Tr>::go` with no impl down there.
    let inherent = "impl crate::S { pub fn go(self) {} }";
    assert_eq!(go(&deep(300, inherent)), None);
    assert_eq!(
        go(&deep(100, inherent)).as_deref(),
        Some("src/lib.rs#S::go#method")
    );
    assert_eq!(
        go(&deep(300, "let _x = 1;")).as_deref(),
        Some("src/lib.rs#<S as Tr>::go#method")
    );
}

// D17: the other helpers over syntax that nests without bound (references,
// pointers, `+` bounds, `use` groups) recursed too. Each is now a loop or
// stops at a cap, past which what it names is unresolved.
#[test]
fn c18_rust_types_bounds_and_use_groups_nested_without_bound_cost_no_stack() {
    let n = 20_000;
    let src = format!(
        "pub struct S;\npub trait Tr {{ fn go(&self); }}\n\
         impl Tr for {refs}S {{ fn go(&self) {{}} }}\n\
         pub fn r(x: {refs}S) {{ x.go(); }}\n\
         pub fn b(x: {boxes}S{closes}) {{ x.go(); }}\n\
         pub fn i(x: impl {bounds}) {{ x.go(); }}\n\
         use crate::{groups}S{braces};\n\
         pub fn u() {{ S::new(); }}\n{blank}",
        refs = "&".repeat(n),
        boxes = "Box<".repeat(n / 4),
        closes = ">".repeat(n / 4),
        bounds = vec!["Tr"; n].join(" + "),
        groups = "{self::".repeat(n / 4),
        braces = "}".repeat(n / 4),
        // Blank lines keep the mean line below the generated-blob guard's,
        // so the file is read, not indexed unread (`tree::unread`).
        blank = "\n".repeat(n / 10),
    );
    assert!(!crate::mvp_index::tree::unread(
        Lang::Rust,
        "src/lib.rs",
        src.as_bytes()
    ));
    let x = extract("src/lib.rs", &src);
    assert_eq!(
        defs(&x).last(),
        Some(&(SymbolKind::Function, "u".to_owned()))
    );
    // The impl for `&…&S` is named by its references, every one of them.
    let qualified = format!("<{}S as Tr>::go", "&".repeat(n));
    assert!(defs(&x).contains(&(SymbolKind::Method, qualified)));
    // The `use` nested past the cap is unread, and so is its scope.
    let opaque_import = contexts(&x)
        .iter()
        .zip(&x.sites)
        .any(|(c, s)| s.kind == EdgeKind::Import && c.form == Form::Opaque);
    assert!(opaque_import);
    let linked = link(&[("Cargo.toml", MANIFEST), ("src/lib.rs", &src)]);
    for caller in ["r", "b", "i"] {
        let call = linked
            .iter()
            .find(|l| l.kind == EdgeKind::Call && l.src == caller)
            .expect("each call is a site");
        assert_eq!(call.dst, None, "{caller}");
    }
}

#[test]
fn a_signature_is_the_first_line_of_the_declaration_only() {
    let x = extract(
        "a.rs",
        "pub fn first(a: u8,\n             b: u8) -> u8 {\n    a + b\n}\n",
    );
    assert_eq!(x.defs[0].signature, "pub fn first(a: u8,");
    assert_eq!((x.defs[0].line, x.defs[0].end_line), (1, 4));
}

#[test]
fn a_signature_is_capped_on_a_character() {
    let name: String = std::iter::repeat_n('é', 300).collect();
    let x = extract("a.rs", &format!("fn {name}() {{}}\n"));
    let signature = &x.defs[0].signature;
    // v1 cut at 200 bytes (199 here); the MVP's shared `text::signature`
    // cuts at 200 characters (`SIGNATURE_CAP`), the same for every language.
    assert_eq!(signature.chars().count(), 200);
    assert!(signature.starts_with("fn é"));
}

#[test]
fn trait_methods_are_qualified_by_the_trait_with_or_without_a_body() {
    let x = extract(
        "a.rs",
        "pub trait Store {\n    fn get(&self) -> u8;\n    fn put(&mut self) {}\n}\n",
    );
    // v1 dropped `get`, a signature without a body; decision 2 keeps it.
    assert_eq!(
        defs(&x),
        [
            (SymbolKind::Trait, "Store".to_owned()),
            (SymbolKind::Method, "Store::get".to_owned()),
            (SymbolKind::Method, "Store::put".to_owned()),
        ]
    );
    assert_eq!(x.defs[1].signature, "fn get(&self) -> u8;");
}

#[test]
fn impl_methods_are_qualified_by_the_bare_type_and_trait_impls_by_both() {
    let x = extract(
        "a.rs",
        "struct Cache<T>(T);\nimpl<T: Clone> Cache<T> {\n    fn fill(&self) {}\n}\n\
         impl<T> std::fmt::Display for crate::a::Cache<T> {\n    fn fmt(&self) {}\n}\n\
         impl dyn Tr {\n    fn q() {}\n}\n",
    );
    // v1 marked trait impls with a flag and qualified `crate::a::Cache` and
    // `dyn Tr` as written; decision 1 (Q7) keeps the last segment and names a
    // trait impl's methods with Rust's fully qualified syntax.
    assert_eq!(
        defs(&x),
        [
            (SymbolKind::Struct, "Cache".to_owned()),
            (SymbolKind::Method, "Cache::fill".to_owned()),
            (SymbolKind::Method, "<Cache as Display>::fmt".to_owned()),
            (SymbolKind::Method, "dyn Tr::q".to_owned()),
        ]
    );
}

#[test]
fn consts_statics_aliases_and_macros_are_symbols_of_their_own_kind() {
    let x = extract(
        "a.rs",
        "const LIMIT: u8 = 1;\nstatic NAME: &str = \"n\";\ntype Id = u64;\nmacro_rules! log { () => {} }\n\
         struct S;\nimpl S { const C: u8 = 2; type Item = u8; }\n",
    );
    // v1 made a static a `Const` and had no alias or macro symbol; decision 2
    // gives each its kind. An associated type is not an alias.
    assert_eq!(
        defs(&x),
        [
            (SymbolKind::Const, "LIMIT".to_owned()),
            (SymbolKind::Static, "NAME".to_owned()),
            (SymbolKind::Type, "Id".to_owned()),
            (SymbolKind::Macro, "log".to_owned()),
            (SymbolKind::Struct, "S".to_owned()),
            (SymbolKind::Const, "C".to_owned()),
        ]
    );
}

#[test]
fn a_turbofish_call_names_the_function_and_keeps_its_path() {
    let x = extract(
        "a.rs",
        "fn f() {\n    parse::<u8>(\"1\");\n    serde_json::from_str::<V>(s);\n}\n",
    );
    assert_eq!(
        sites(&x),
        [
            (EdgeKind::Call, 2, "parse".to_owned()),
            (EdgeKind::Call, 3, "serde_json::from_str".to_owned()),
            (EdgeKind::Reference, 3, "s".to_owned()),
        ]
    );
    let forms: Vec<Form> = contexts(&x).into_iter().map(|c| c.form).collect();
    assert_eq!(forms[0], Form::Path(vec!["parse".to_owned()]));
    assert_eq!(
        forms[1],
        Form::PathCall {
            path: vec!["serde_json".to_owned(), "from_str".to_owned()],
            first: Argument::Unknown,
        }
    );
}

#[test]
fn test_functions_and_test_modules_are_indexed_like_any_code() {
    let x = extract(
        "src/lib.rs",
        r#"fn production_entry() { production_helper(); }
fn production_helper() {}
#[test]
fn top_level_test() { production_entry(); }
#[tokio::test]
async fn async_test() { production_entry(); }
#[cfg(test)]
mod tests {
    #[test]
    fn nested_test() { super::production_entry(); }
}
"#,
    );
    // v1 hid every test container (T-1 to T-3); decision 3 indexes them, so
    // the minimap can say a test exercises a match.
    assert_eq!(
        defs(&x),
        [
            (SymbolKind::Function, "production_entry".to_owned()),
            (SymbolKind::Function, "production_helper".to_owned()),
            (SymbolKind::Function, "top_level_test".to_owned()),
            (SymbolKind::Function, "async_test".to_owned()),
            (SymbolKind::Module, "tests".to_owned()),
            (SymbolKind::Function, "nested_test".to_owned()),
        ]
    );
    assert_eq!(sites(&x).len(), 4);
}

#[test]
fn a_function_gated_on_cfg_test_is_a_definition() {
    let x = extract("a.rs", "#[cfg(test)]\nfn fixture() {}\n");
    assert_eq!(defs(&x), [(SymbolKind::Function, "fixture".to_owned())]);
}

#[test]
fn a_site_belongs_to_the_innermost_definition_around_it() {
    let x = extract(
        "src/lib.rs",
        "mod inline { fn inner() { f(); } }\nconst X: u8 = g();\nfn outer() {\n    fn nested() { h(); }\n    k();\n}\n",
    );
    let owners: Vec<(String, Option<String>)> = x
        .sites
        .iter()
        .map(|s| (s.target.clone(), s.owner.map(|o| x.defs[o].name.clone())))
        .collect();
    // v1 picked the smallest span holding the line (W-7), so the one-line
    // module owned `f()`; the walk knows the innermost definition exactly.
    assert_eq!(
        owners,
        [
            ("f".to_owned(), Some("inner".to_owned())),
            ("g".to_owned(), Some("X".to_owned())),
            ("h".to_owned(), Some("nested".to_owned())),
            ("k".to_owned(), Some("outer".to_owned())),
        ]
    );
}

#[test]
fn mod_declarations_are_facts_and_inline_modules_are_symbols() {
    let x = extract(
        "src/lib.rs",
        "pub mod external;\n#[path = \"other.rs\"]\nmod moved;\nmod inline {\n    pub fn body() {}\n}\n",
    );
    // v1 made `mod external;` a module symbol flagged `module_decl`; the MVP
    // has no flag, so the declaration is a fact and the module is its file.
    assert_eq!(
        defs(&x),
        [
            (SymbolKind::Module, "inline".to_owned()),
            (SymbolKind::Function, "body".to_owned()),
        ]
    );
    let modules: Vec<(String, ItemKind)> = facts(&x)
        .items
        .into_iter()
        .filter(|i| matches!(i.kind, ItemKind::Module { .. }))
        .map(|i| (i.name, i.kind))
        .collect();
    assert_eq!(
        modules,
        [
            (
                "external".to_owned(),
                ItemKind::Module {
                    body: None,
                    path: ModPath::Layout
                }
            ),
            (
                "moved".to_owned(),
                ItemKind::Module {
                    body: None,
                    path: ModPath::Literal("other.rs".to_owned())
                }
            ),
            (
                "inline".to_owned(),
                ItemKind::Module {
                    body: Some(1),
                    path: ModPath::Layout
                }
            ),
        ]
    );
}

// D43: what an impl requires of each parameter of its self type, and the
// bounds a type declares on its own.
#[test]
fn an_impl_parameter_keeps_its_bounds_and_a_type_its_declared_ones() {
    let x = extract(
        "src/lib.rs",
        "pub struct S<T: ?Sized + Tr, U, const N: usize>(Box<T>, U);\npub struct P<T> where T: Copy + Tr<u8> { t: T }\n\
         pub struct Q<T: Into<U>, U>(T, U);\n\
         impl<'a, T: Tr + 'a, U: ?Sized, const N: usize> S<T, U, N> {}\nimpl<T> P<T> where T: Copy, T: Tr<u8> {}\n\
         impl<T, U> Q<T, U> where Vec<T>: Clone {}\nimpl<T> Q<T, T> {}\nimpl<T: Into<U>, U> Q<T, U> {}\nimpl<T> W<T> where Self: Tr {}\n",
    );
    let facts = facts(&x);
    let args: Vec<Vec<String>> = facts
        .scopes
        .iter()
        .filter_map(|scope| match &scope.kind {
            ScopeKind::Impl {
                self_type: Some(layer),
                ..
            } => Some(layer.args.clone()),
            _ => None,
        })
        .collect();
    let bounded = |bounds: &[&str]| format!("_:{}", crate::mvp_index::codec::record(bounds));
    assert_eq!(
        args,
        [
            vec![bounded(&["Tr"]), "_".to_owned(), "_".to_owned()],
            vec![bounded(&["Copy", "Tr<u8>"])],
            vec!["_+".to_owned(), "_".to_owned()],
            vec!["_+".to_owned(), "_+".to_owned()],
            vec!["_+".to_owned(), "_".to_owned()],
            vec!["_+".to_owned()],
        ]
    );
    let declared: Vec<(u32, Vec<Vec<String>>)> = facts
        .bounds
        .into_iter()
        .map(|d| (d.item, d.bounds))
        .collect();
    let list = |b: &[&str]| b.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    // `T: Into<U>` names a parameter: it is left out.
    assert_eq!(
        declared,
        [
            (0, vec![list(&["?Sized", "Tr"]), list(&[]), list(&[])]),
            (1, vec![list(&["Copy", "Tr<u8>"])]),
        ]
    );
}

#[test]
fn enum_variants_are_symbols_qualified_by_their_enum() {
    let x = extract(
        "src/main.rs",
        "\nenum Command {\n    SelfUpdate { force: bool },\n    RepoState,\n    DryRun,\n    ListErrors(usize),\n}\n",
    );
    let variants: Vec<(String, String)> = x
        .defs
        .iter()
        .filter(|d| d.kind == SymbolKind::Variant)
        .map(|d| (d.name.clone(), d.signature.clone()))
        .collect();
    assert_eq!(
        variants,
        [
            (
                "Command::SelfUpdate".to_owned(),
                "SelfUpdate { force: bool }".to_owned()
            ),
            ("Command::RepoState".to_owned(), "RepoState".to_owned()),
            ("Command::DryRun".to_owned(), "DryRun".to_owned()),
            (
                "Command::ListErrors".to_owned(),
                "ListErrors(usize)".to_owned()
            ),
        ]
    );
    assert_eq!(x.defs[0].name, "Command");
    assert_eq!(x.defs[0].signature, "enum Command {");
}

#[test]
fn an_argument_is_a_reference_only_when_it_names_an_item() {
    let x = extract(
        "src/a.rs",
        "impl C { fn go(&self, cfg: Cfg) { run(cfg.field, self.handler, Self::helper, self, None, helper, cfg); } }\n",
    );
    let references: Vec<String> = sites(&x)
        .into_iter()
        .filter(|s| s.0 == EdgeKind::Reference)
        .map(|s| s.2)
        .collect();
    // v1 also recorded `handler` from `self.handler` and the parameter
    // `cfg`, dropping plain values at resolution (Q-1). In Rust `self.x` is a
    // field (a method cannot be named without a call), and a local or a
    // parameter is a value: neither can be an item, so neither is a site.
    assert_eq!(references, ["Self::helper", "helper"]);
}

#[test]
fn a_nested_impl_and_a_nested_function_are_named_by_themselves() {
    let x = extract(
        "src/lib.rs",
        r#"
struct X;
impl std::fmt::Display for X {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { Ok(()) }
}
impl X {
    fn own(&self) {
        struct Y;
        impl Default for Y { fn default() -> Self { Y } }
        fn helper() {}
    }
    fn after(&self) {}
}
trait Greet { fn hello(&self) {} }
fn free() {}
"#,
    );
    // v1 qualified by a stack, giving `X::Y::default` and the method
    // `X::helper` (S-11); a function in a block is a free function and an
    // impl is named by its own type (decision 1, Q7).
    assert_eq!(
        defs(&x),
        [
            (SymbolKind::Struct, "X".to_owned()),
            (SymbolKind::Method, "<X as Display>::fmt".to_owned()),
            (SymbolKind::Method, "X::own".to_owned()),
            (SymbolKind::Struct, "Y".to_owned()),
            (SymbolKind::Method, "<Y as Default>::default".to_owned()),
            (SymbolKind::Function, "helper".to_owned()),
            (SymbolKind::Method, "X::after".to_owned()),
            (SymbolKind::Trait, "Greet".to_owned()),
            (SymbolKind::Method, "Greet::hello".to_owned()),
            (SymbolKind::Function, "free".to_owned()),
        ]
    );
}

#[test]
fn a_repeated_definition_gets_its_own_uid() {
    let x = extract(
        "src/lib.rs",
        "struct X;\nimpl std::fmt::Display for X { fn fmt(&self) { helper_a(); } }\n\
         impl std::fmt::Debug for X { fn fmt(&self) { helper_b(); } }\n\
         #[cfg(unix)]\nfn sys() {}\n#[cfg(windows)]\nfn sys() {}\n",
    );
    let uids: Vec<String> = facts(&x).items.iter().map(|i| i.uid.clone()).collect();
    let methods: Vec<String> = facts(&x).assocs.iter().map(|a| a.uid.clone()).collect();
    // v1's uid `path#qualified#kind` collapsed the two `fmt`s (S-10); the
    // trait in the name and the driver's `~n` keep every definition apart.
    assert_eq!(uids, ["#X#struct", "#sys#function", "#sys#function~2"]);
    assert_eq!(
        methods,
        ["#<X as Display>::fmt#method", "#<X as Debug>::fmt#method"]
    );
    assert_eq!(sites(&x).len(), 2);
}

// D23: every macro invoked where items go is a call resolution reads, in
// a module, an impl or a trait, with its path as written and its
// `#[cfg]`s; one in a function's body is not. The same call repeated in
// a scope is read once (macro-call-records-memory: a file of a thousand
// `mk! { … }` kept a thousand records). A union keeps its derives, as a
// struct does.
#[test]
fn every_macro_invoked_where_items_go_is_a_call() {
    let x = extract(
        "src/lib.rs",
        "gen!(A);\ngen!(B);\nmod m {\n    #[cfg(test)]\n    a::b! { x }\n    a::b! { y }\n    a::b! { z }\n}\nimpl S { inner!(); }\ntrait T { self::inner!(); }\n\
         fn f() { stmt!(); }\nstd::thread_local!(static N: u8 = 0);\n#[derive(Copy, Clone, pm::Get)]\nunion U { a: u8 }\n",
    );
    let facts = facts(&x);
    let calls: Vec<(u32, String, Vec<String>, bool)> = facts
        .macro_calls
        .iter()
        .map(|c| (c.scope, c.path.join("::"), c.cfg.clone(), c.statics))
        .collect();
    assert_eq!(
        calls,
        [
            (0, "gen".to_owned(), Vec::new(), false),
            (1, "a::b".to_owned(), vec!["test".to_owned()], false),
            (1, "a::b".to_owned(), Vec::new(), false),
            (2, "inner".to_owned(), Vec::new(), false),
            (3, "self::inner".to_owned(), Vec::new(), false),
            (0, "std::thread_local".to_owned(), Vec::new(), true),
        ]
    );
    let union = facts.items.iter().find(|i| i.name == "U").map(|i| &i.kind);
    assert_eq!(
        union,
        Some(&ItemKind::Union {
            derives: vec!["Copy".to_owned(), "Clone".to_owned(), "Get".to_owned()]
        })
    );
}

// D43: an alias keeps how many parameters it declares and, on its
// target, which of them each argument is.
#[test]
fn an_alias_keeps_its_parameters_on_its_target() {
    let x = extract(
        "src/lib.rs",
        "pub type A<'a, T, const N: usize> = X<'a, u8, T, N, Box<T>>;\npub type B = Y;\n",
    );
    let kinds: Vec<ItemKind> = facts(&x).items.into_iter().map(|i| i.kind).collect();
    let layer = |path: &str, args: &[&str]| super::super::Layer {
        path: vec![path.to_owned()],
        dynamic: false,
        args: args.iter().map(|a| (*a).to_owned()).collect(),
    };
    assert_eq!(
        kinds,
        [
            ItemKind::Alias {
                target: Some(layer("X", &["u8", "$0", "$1", "?"])),
                params: 2,
            },
            ItemKind::Alias {
                target: Some(layer("Y", &[])),
                params: 0,
            },
        ]
    );
}
