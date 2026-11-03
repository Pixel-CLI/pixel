// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Generic arguments and qualified paths (C-10): which impls of a generic
//! type apply to a value of it (D43), and what `<T as Tr>::m` names (D20).
//! Each expectation is what rustc 1.99 runs, or `None` where the rules
//! cannot prove it.

use super::*;

const APP: &str = "[package]\nname = \"app\"\n";

// D43: an impl's self type lost its generic arguments, so `G<u8>`'s
// inherent method was linked for a call on a `G<u16>`.
#[test]
fn c10_rust_an_impl_for_other_generic_arguments_is_no_candidate() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub mod v { pub struct Value; }\npub struct G<T>(pub T);\npub trait TrG { fn go(&self); }\n\
             impl<T: Default> G<T> { pub fn new() -> Self { G(T::default()) } }\n\
             impl G<u8> { pub fn go(&self) {} pub fn only(&self) {} }\nimpl TrG for G<u16> { fn go(&self) {} }\n\
             pub struct E<T>(pub T);\nimpl<T> E<T> { pub fn wrap(&self) {} }\nimpl E<v::Value> { pub fn data(&self) {} }\n\
             impl E<std::string::String> { pub fn text(&self) {} }\n\
             pub fn f(a: &G<u16>, b: &G<u8>, e: &E<v::Value>) {\n    a.go();\n    b.go();\n    let c = G::<u16>::new();\n    c.go();\n    G::<u8>::new().only();\n    e.data();\n    e.wrap();\n}\n\
             pub fn h<T>(e: &E<T>) { e.wrap(); }\npub fn g(e: &E<std::string::String>) { e.text(); e.wrap(); }\n",
        ),
    ];
    // What rustc 1.99 runs, except `c.go()`: the rules do not infer that
    // `G::<u16>::new()` is a `G<u16>`, and `G<u8>` and `G<u16>` both have a
    // `go`.
    let in_f_g_h: Vec<_> = calls(&files)
        .into_iter()
        .filter(|(src, _, _)| ["f", "g", "h"].contains(&src.as_str()))
        .collect();
    assert_eq!(
        in_f_g_h,
        [
            edge("f", "a.go", Some("src/lib.rs#<G as TrG>::go#method")),
            edge("f", "b.go", Some("src/lib.rs#G::go#method")),
            edge("f", "G::<u16>::new", Some("src/lib.rs#G::new#method")),
            edge("f", "c.go", None),
            edge(
                "f",
                "G::<u8>::new().only",
                Some("src/lib.rs#G::only#method")
            ),
            edge("f", "G::<u8>::new", Some("src/lib.rs#G::new#method")),
            edge("f", "e.data", Some("src/lib.rs#E::data#method")),
            edge("f", "e.wrap", Some("src/lib.rs#E::wrap#method")),
            edge("h", "e.wrap", Some("src/lib.rs#E::wrap#method")),
            edge("g", "e.text", Some("src/lib.rs#E::text#method")),
            edge("g", "e.wrap", Some("src/lib.rs#E::wrap#method")),
        ]
    );
}

#[test]
fn c10_rust_qualified_path_reaches_the_impl_or_the_trait() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub trait Tr {\n    fn m(&self);\n    fn d(&self) {}\n}\npub struct A;\nimpl Tr for A {\n    fn m(&self) {}\n}\npub fn f<X: Tr>(a: &A, x: &X) {\n    <A as Tr>::m(a);\n    <A as Tr>::d(a);\n    <X as Tr>::m(x);\n    X::m(x);\n}\n",
        ),
    ];
    // `X::m` is rooted at a generic parameter: opaque.
    assert_eq!(
        calls(&files),
        [
            edge("f", "<A as Tr>::m", Some("src/lib.rs#<A as Tr>::m#method")),
            edge("f", "<A as Tr>::d", Some("src/lib.rs#Tr::d#method")),
            edge("f", "<X as Tr>::m", Some("src/lib.rs#Tr::m#method")),
            edge("f", "X::m", None),
        ]
    );
}

// D20: `<T as Tr>::m` fell back to the trait's own item when `T` was no
// type of the workspace, a reference, an alias two crates read as two
// types, or a type a blanket impl covers: an edge to a body that does not
// run.
#[test]
fn c10_rust_qualified_path_names_the_impl_for_exactly_that_type() {
    let lib = "pub mod util;\npub trait Tr { fn m() {} }\npub struct S;\nimpl Tr for Vec<u8> { fn m() {} }\nimpl Tr for S { fn m() {} }\nimpl Tr for &S { fn m() {} }\nimpl Tr for &mut S {}\n\
               pub fn f() { <Vec<u8> as Tr>::m(); <&S as Tr>::m(); <S as Tr>::m(); <&mut S as Tr>::m(); }\n\
               pub trait Other {}\npub struct B;\nimpl Other for B {}\npub trait Tb { fn m() {} }\nimpl<T: Other> Tb for T { fn m() {} }\npub fn g() { <B as Tb>::m(); }\n\
               pub trait Tn { fn m() {} }\nimpl Tn for u8 { fn m() {} }\nimpl Tn for String {}\nimpl Tn for S {}\n\
               pub fn h<X: Tn>() { <u8 as Tn>::m(); <String as Tn>::m(); <S as Tn>::m(); <X as Tn>::m(); }\n";
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", lib),
        (
            "src/main.rs",
            "mod util;\nuse app::Tr;\npub struct S;\nimpl Tr for S { fn m() {} }\nfn main() {}\n",
        ),
        (
            "src/util.rs",
            "use crate::{S, Tr};\ntype A = S;\npub fn k() { <A as Tr>::m(); }\n",
        ),
    ];
    // rustc 1.99 runs `<Vec<u8> as Tr>::m`, `<&S as Tr>::m`, `<S as Tr>::m`,
    // `Tr::m` (the impl for `&mut S` takes the default), the blanket
    // `<T as Tb>::m`, `<u8 as Tn>::m`, `Tn::m` twice, and in each crate its
    // own `<S as Tr>::m` for `<A as Tr>::m`. A type outside the workspace,
    // a type with no impl the rules see for exactly it, and an alias two
    // crates read as two types are unresolved; a generic parameter names
    // the trait's own item.
    let dst: Vec<(String, Option<String>)> = calls(&files)
        .into_iter()
        .map(|(src, _, dst)| (src, dst))
        .collect();
    let at = |name: &str| Some(format!("src/lib.rs#{name}#method"));
    assert_eq!(
        dst,
        [
            ("f", None),
            ("f", at("<&S as Tr>::m")),
            ("f", at("<S as Tr>::m")),
            ("f", at("Tr::m")),
            ("g", None),
            ("h", None),
            ("h", None),
            ("h", at("Tn::m")),
            ("h", at("Tn::m")),
            ("k", None),
        ]
        .map(|(src, dst)| (src.to_owned(), dst))
    );
}

// D20 (parser study): `Tr::m(&mut x)`, a path to a trait's method, linked
// the trait's own item whatever type the first argument gave `Self`: a
// default that does not run, or a method with no body, where rustc runs
// the impl's (`Corpus::watch_ready(&mut svc)` in Pixel-CLI/pixel). The
// first argument now gives `Self` as `self` takes it, and the path names
// the item of that type's impl as `<Self as Tr>::m` does; a generic
// parameter, `dyn Tr` or `impl Tr` still names the trait's own item, the
// impl being the caller's to choose; any other argument, or none, leaves
// it unresolved.
#[test]
fn c10_rust_a_trait_s_path_names_the_impl_its_first_argument_gives() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub mod m {\n    pub trait Corpus {\n        fn watch_ready(&mut self) {}\n        fn handle(&self) {}\n        \
             fn take(self) where Self: Sized {}\n        fn make() -> Self where Self: Sized;\n        \
             fn c14(&mut self) { Corpus::watch_ready(self); }\n    }\n}\nuse m::Corpus;\npub struct Service;\n\
             impl Service { pub fn new() -> Service { Service } pub fn open() -> Option<Service> { Some(Service) } }\n\
             impl Corpus for Service { fn watch_ready(&mut self) {} fn handle(&self) {} fn take(self) {} fn make() -> Self { Service } }\n\
             pub struct Stub;\nimpl Stub { pub fn new() -> Stub { Stub } }\nimpl Corpus for Stub { fn make() -> Self { Stub } }\n\
             impl Service { pub fn c13(&mut self) { Corpus::watch_ready(self); } }\n\
             pub fn c1() { let mut svc = Service::new(); Corpus::watch_ready(&mut svc); }\n\
             pub fn c2(svc: &mut Service) { Corpus::watch_ready(svc); }\n\
             pub fn c3() { let mut stub = Stub::new(); Corpus::watch_ready(&mut stub); }\n\
             pub fn c4(s: Service) { Corpus::handle(&s); }\npub fn c5(mut s: Service) { Corpus::handle(&mut s); }\n\
             pub fn c6(s: Service) { Corpus::take(s); }\npub fn c7<X: Corpus>(x: &mut X) { Corpus::watch_ready(x); }\n\
             pub fn c8(x: &mut dyn Corpus) { Corpus::watch_ready(x); }\npub fn c9(x: &mut impl Corpus) { Corpus::watch_ready(x); }\n\
             pub fn c10() { let mut svc = Service::open().unwrap(); Corpus::watch_ready(&mut svc); }\n\
             pub fn c11() { let _s: Service = Corpus::make(); }\n\
             pub fn c12(svc: &mut Service) { crate::m::Corpus::watch_ready(svc); }\n\
             pub fn c15(s: &&Service) { Corpus::handle(*s); }\n\
             pub trait Tr { fn take(self) where Self: Sized {} fn get(&self) {} }\npub struct S;\n\
             impl S { pub fn get(&self) {} }\nimpl Tr for S { fn take(self) {} fn get(&self) {} }\nimpl Tr for &S { fn take(self) {} }\n\
             pub struct G<T>(pub T);\nimpl Tr for G<u8> { fn get(&self) {} }\nimpl Tr for G<u16> { fn get(&self) {} }\n\
             pub fn c16(s: S) { Tr::take(&s); }\npub fn c17(r: &S) { Tr::get(&r); }\npub fn c18(s: &S) { Tr::get(s); }\n\
             pub fn c19(g: &G<u16>) { Tr::get(g); }\npub fn c20(b: &Box<G<u8>>) { Tr::get(&**b); }\n\
             pub fn c21(v: &[S]) { v.iter().for_each(Tr::get); }\n",
        ),
    ];
    // cargo 1.99 runs `<Service as Corpus>::watch_ready` (c1, c2, c10,
    // c12, c13), the default `Corpus::watch_ready` for `Stub` (c3) and in
    // generic code for the types the tests pass (c7 to c9, c14),
    // `<Service as Corpus>::handle` (c4, c5, c15) and `::take` (c6),
    // `Service::make` (c11), `<&S as Tr>::take` (c16), the default
    // `Tr::get` for `&S` (c17), `<S as Tr>::get` (c18), `<G<u16> as
    // Tr>::get` (c19) and `<G<u8> as Tr>::get` (c20). `open().unwrap()`
    // is typed by what `open` returns, an `Option<Service>` (c10). The
    // rules do not type a dereference (c15, c20), or a `Self` nothing
    // gives (c11), nor where a path passed as a value is called (c21 runs
    // `<&S as Tr>::get`'s default for each `&S`).
    let method = |name: &str| Some(format!("src/lib.rs#{name}#method"));
    let got: Vec<(String, Option<String>)> = calls(&files)
        .into_iter()
        .filter(|(src, target, _)| {
            let caller = src.rsplit("::").next().unwrap_or_default();
            caller.starts_with('c')
                && caller != "c21"
                && !target.ends_with("::new")
                && !target.contains("open")
        })
        .map(|(src, _, dst)| (src, dst))
        .collect();
    assert_eq!(
        got,
        [
            ("Corpus::c14", method("Corpus::watch_ready")),
            ("Service::c13", method("<Service as Corpus>::watch_ready")),
            ("c1", method("<Service as Corpus>::watch_ready")),
            ("c2", method("<Service as Corpus>::watch_ready")),
            ("c3", method("Corpus::watch_ready")),
            ("c4", method("<Service as Corpus>::handle")),
            ("c5", method("<Service as Corpus>::handle")),
            ("c6", method("<Service as Corpus>::take")),
            ("c7", method("Corpus::watch_ready")),
            ("c8", method("Corpus::watch_ready")),
            ("c9", method("Corpus::watch_ready")),
            ("c10", method("<Service as Corpus>::watch_ready")),
            ("c11", None),
            ("c12", method("<Service as Corpus>::watch_ready")),
            ("c15", None),
            ("c16", method("<&S as Tr>::take")),
            ("c17", method("Tr::get")),
            ("c18", method("<S as Tr>::get")),
            ("c19", Some("src/lib.rs#<G as Tr>::get#method~2".to_owned())),
            ("c20", None),
        ]
        .map(|(src, dst)| (src.to_owned(), dst))
    );
    assert_eq!(
        references(&files)
            .into_iter()
            .filter(|(src, _, _)| src == "c21")
            .collect::<Vec<_>>(),
        [edge("c21", "Tr::get", None)]
    );
}

// D20 (review of the fix): a path to the method of a trait with a type
// parameter took the trait's defaults, as `<A as D>::d` does, and linked
// `impl D for A`; but the call infers the trait's arguments, from a
// turbofish, another argument or its return type, and rustc ran the
// default `impl D<u16> for A` or a blanket impl takes. Such a trait's path
// is unresolved; one with a lifetime parameter only is not, and `<A as
// D>::d` still names the impl for the defaults.
#[test]
fn c10_rust_a_generic_trait_s_path_infers_its_arguments() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub struct A;
pub struct B;
pub trait D<T = u8> { fn d(&self) {} }
impl D for A { fn d(&self) {} }
             impl D<u16> for A {}
pub trait Combine<Rhs = Self> { fn combine(&self, _o: &Rhs) {} }
             impl Combine for A { fn combine(&self, _o: &A) {} }
impl Combine<B> for A {}
             pub trait Mark {}
impl Mark for B {}
impl D for B { fn d(&self) {} }
impl<X: Mark> D<u16> for X {}
             pub trait L<'a> { fn l(&self) {} }
impl<'a> L<'a> for A { fn l(&self) {} }
             pub fn c1(a: &A) { D::<u16>::d(a); }
pub fn c2(a: A, b: B) { Combine::combine(&a, &b); }
             pub fn c3(b: &B) { D::<u16>::d(b); }
pub fn c4(a: &A) { D::<u8>::d(a); }
             pub fn c5(a: &A) { <A as D>::d(a); }
pub fn c6(a: &A) { L::l(a); }
             pub fn c7<X: D<u16>>(x: &X) { D::d(x); }
",
        ),
    ];
    // cargo 1.99 runs the default `D::d` (c1, c3), `Combine::combine`
    // (c2), `<A as D>::d` (c4, c5), `<A as L>::l` (c6), and in generic code
    // the impl the caller's type gives (c7).
    let method = |name: &str| Some(format!("src/lib.rs#{name}#method"));
    let got: Vec<(String, Option<String>)> = calls(&files)
        .into_iter()
        .map(|(src, _, dst)| (src, dst))
        .collect();
    assert_eq!(
        got,
        [
            ("c1", None),
            ("c2", None),
            ("c3", None),
            ("c4", None),
            ("c5", method("<A as D>::d")),
            ("c6", method("<A as L>::l")),
            ("c7", method("D::d")),
        ]
        .map(|(src, dst)| (src.to_owned(), dst))
    );
}

// D43 (review of the fix): an impl whose parameter has a bound, a `where`
// predicate or a second use in the self type was taken for every argument
// of its type, and so was one whose `Sized` parameter cannot be the `str`
// of a type that says `?Sized`: its method was linked for a value it does
// not apply to.
#[test]
fn c10_rust_an_impl_whose_parameters_are_constrained_may_not_apply() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub trait Tr { fn get(&self); }\npub struct L;\n\
             pub mod a {\n    use super::{L, Tr};\n    pub struct W<T>(pub T);\n    \
             impl<T: Copy> W<T> { pub fn get(&self) {} pub fn run(&self) { self.get(); } }\n    \
             impl Tr for W<L> { fn get(&self) {} }\n    pub fn a1() { let w: W<L> = W(L); w.get(); }\n    \
             pub fn a2() { let w: W<u8> = W(1); w.get(); }\n    pub fn a3() { let w: W<u8> = W(1); w.run(); }\n}\n\
             pub mod b {\n    use super::{L, Tr};\n    pub struct W<T>(pub T);\n    \
             impl<T> W<T> where T: Copy { pub fn get(&self) {} }\n    impl Tr for W<L> { fn get(&self) {} }\n    \
             pub fn b1() { let w: W<L> = W(L); w.get(); }\n}\n\
             pub mod c {\n    use super::Tr;\n    pub struct W<A, B>(pub A, pub B);\n    \
             impl<T> W<T, T> { pub fn get(&self) {} }\n    impl Tr for W<u8, u16> { fn get(&self) {} }\n    \
             pub fn c1() { let w: W<u8, u16> = W(1, 2); w.get(); }\n    pub fn c2() { let w: W<u8, u8> = W(1, 2); w.get(); }\n}\n\
             pub mod d {\n    use super::L;\n    pub struct W<T>(pub T);\n    pub trait Tq { fn m(&self) {} }\n    \
             impl<T: Copy> Tq for W<T> { fn m(&self) {} }\n    impl Tq for W<L> {}\n    \
             pub fn d1() { <W<L> as Tq>::m(&W(L)); }\n    pub fn d2() { <W<u8> as Tq>::m(&W(1u8)); }\n}\n\
             pub mod e {\n    use super::{L, Tr};\n    pub struct V<T>(pub T);\n    \
             impl<'a, T: 'a> V<T> { pub fn get(&self) {} }\n    impl Tr for V<L> { fn get(&self) {} }\n    \
             pub fn e1() { let v: V<L> = V(L); v.get(); }\n}\n\
             pub mod f {\n    use super::Tr;\n    pub struct U<T: ?Sized>(pub Box<T>);\n    \
             impl<T> U<T> { pub fn get(&self) {} }\n    impl Tr for U<str> { fn get(&self) {} }\n    \
             pub fn f1(u: U<str>) { u.get(); }\n    pub fn f2(u: U<u8>) { u.get(); }\n}\n",
        ),
        ("crates/g/Cargo.toml", "[package]\nname = \"g\"\n"),
        (
            "crates/g/src/lib.rs",
            "pub struct L;\npub struct W<T>(pub T);\nimpl<T: Copy> W<T> { pub fn get(&self) {} }\n\
             macro_rules! mk { ($t:ty) => { impl W<$t> { pub fn get(&self) {} } }; }\nmk!(L);\n\
             pub fn g1() { let w: W<L> = W(L); w.get(); }\npub trait Weigher {}\n#[derive(Clone)]\npub struct Foo;\n\
             impl Weigher for Foo {}\npub struct X<V: Weigher>(pub V);\nimpl<V: Weigher> X<V> { pub fn new(v: V) -> Self { X(v) } }\n\
             pub struct Y<V: Weigher>(pub V);\nimpl<V: Weigher + Clone> Y<V> { pub fn new(v: V) -> Self { Y(v) } }\n\
             pub struct Z;\nmacro_rules! mk2 { ($t:ident) => { impl $t { pub fn new() -> Self { $t } } }; }\nmk2!(Z);\n\
             pub fn g2() { X::new(Foo); }\npub fn g3() { Y::new(Foo); }\n",
        ),
        ("crates/h/Cargo.toml", "[package]\nname = \"h\"\n"),
        (
            "crates/h/src/lib.rs",
            "pub trait Weigher {}\npub struct Foo;\nimpl Weigher for Foo {}\npub struct X<V: Weigher>(pub V);\n\
             impl<V: Weigher> X<V> { pub fn new(v: V) -> Self { X(v) } }\npub fn h1() { X::new(Foo); }\n",
        ),
    ];
    // What cargo 1.99 runs: `<W<L> as Tr>::get` (a1, b1), `W::get` (a2,
    // and `self.get()` in `run`, `Self` being a `W<T>` with `T: Copy`),
    // `W::run` (a3), `<W<u8, u16> as Tr>::get` (c1), `W::get` (c2), `Tq`'s
    // default `m` (d1, through `impl Tq for W<L>`), `<W as Tq>::m` (d2),
    // `V::get` (e1: a lifetime bound applies to every argument), `<U<str>
    // as Tr>::get` (f1), `U::get` (f2), the macro's `get` (g1), `X::new`
    // (g2, h1) and `Y::new` (g3). An impl that may apply stands only when
    // nothing else provides the name (a2, a3, c2), or when an impl of the
    // same trait is proven for the type (d1, coherence); the rules do not
    // evaluate `T: Copy` (d2), and the macros of `g` leave its probes
    // unresolved (g1 to g3, D23). An impl requiring no more than its type
    // declares is for every argument (h1): rustc rejects an impl for any
    // other as ill-formed (E0277).
    let method = |name: &str| Some(format!("src/lib.rs#{name}#method"));
    let got: Vec<(String, Option<String>)> = calls(&files)
        .into_iter()
        .filter(|(_, target, _)| !matches!(target.as_str(), "W" | "V" | "U" | "X" | "Y"))
        .map(|(src, _, dst)| (src, dst))
        .collect();
    assert_eq!(
        got,
        [
            ("W::run", method("W::get")),
            ("a1", None),
            ("a2", method("W::get")),
            ("a3", method("W::run")),
            ("b1", None),
            ("c1", None),
            ("c2", Some("src/lib.rs#W::get#method~3".to_owned())),
            ("d1", method("Tq::m")),
            ("d2", None),
            ("e1", method("V::get")),
            ("f1", None),
            ("f2", method("U::get")),
            ("g1", None),
            ("g2", None),
            ("g3", None),
            ("h1", Some("crates/h/src/lib.rs#X::new#method".to_owned())),
        ]
        .map(|(src, dst)| (src.to_owned(), dst))
    );
}

// generic-trait-impl-bound (final review of the D43 fix): an impl that
// may apply was set aside beside one proven for the value, as an impl of
// another trait would be (E0034). Two impls of one generic trait may both
// apply (`Extend<T>` and, bounded, `Extend<&'a T>`), and the trait's
// arguments, which the call's infer, pick one: `s.extend(v.iter())` and
// hashlink's `self.extend(iter.into_iter().cloned())` were linked to the
// other impl's `extend`.
#[test]
fn c10_rust_two_impls_of_a_generic_trait_may_both_apply() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub struct S<T>(pub Vec<T>);
pub struct R<T>(pub Vec<T>);
pub struct H<T>(pub Vec<T>);
             impl<T> Extend<T> for S<T> { fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {} }
             impl<'a, T: 'a + Copy> Extend<&'a T> for S<T> { fn extend<I: IntoIterator<Item = &'a T>>(&mut self, iter: I) {} }
             impl<T: Eq> Extend<T> for H<T> { fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {} }
             impl<'a, T: 'a + Eq + Copy> Extend<&'a T> for H<T> {
                 fn extend<I: IntoIterator<Item = &'a T>>(&mut self, iter: I) { self.extend(iter.into_iter().cloned()); }
}
             pub trait Put<A> { fn put(&mut self, a: A); }
             impl<T> Put<T> for R<T> { fn put(&mut self, a: T) {} }
             impl<'a, T: Copy> Put<&'a T> for R<T> { fn put(&mut self, a: &'a T) {} }
             pub trait One { fn one(&self); }
impl<T: Copy> One for R<T> { fn one(&self) {} }
             pub fn c1(s: &mut S<u8>, v: &[u8]) { s.extend(v.iter()); }
             pub fn c2(s: &mut R<u8>, x: u8) { s.put(x); }
pub fn c3(s: &mut R<u8>, x: &u8) { s.put(x); }
             pub fn c4(s: &R<u8>) { s.one(); }
",
        ),
    ];
    // cargo 1.99 runs `Extend<&'a T>`'s `extend` (c1), `Extend<T>`'s for
    // `H` (in `Extend<&'a T>`'s), `Put<T>`'s `put` (c2), `Put<&'a T>`'s
    // (c3) and `One`'s `one` (c4): an impl of a trait without parameters
    // that may apply still stands when nothing else provides the name.
    let got: Vec<_> = calls(&files)
        .into_iter()
        .filter(|(_, target, _)| !target.starts_with("v.") && !target.starts_with("iter"))
        .collect();
    assert_eq!(
        got,
        [
            edge("<H as Extend>::extend", "self.extend", None),
            edge("c1", "s.extend", None),
            edge("c2", "s.put", None),
            edge("c3", "s.put", None),
            edge("c4", "s.one", Some("src/lib.rs#<R as One>::one#method")),
        ]
    );
}

// D20 (review of the fix): `<S as Tr<u16>>::m` dropped the trait's
// arguments and took the `m` that `impl Tr<u8> for S` defines, where rustc
// runs the default `impl Tr<u16> for S` takes: `Tr<u8>` and `Tr<u16>` are
// two traits.
#[test]
fn c10_rust_qualified_path_names_the_impl_of_the_traits_arguments() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub struct S;\npub trait Tr<T> { fn m(&self) {} }\nimpl Tr<u8> for S { fn m(&self) {} }\nimpl Tr<u16> for S {}\n\
             pub trait Tw<T> { fn m(&self) {} }\nimpl<T> Tw<T> for S { fn m(&self) {} }\n\
             pub trait Td<T = u8> { fn m(&self) {} }\nimpl Td for S { fn m(&self) {} }\nimpl Td<u16> for S {}\n\
             pub trait Tq<T> { fn m(&self) {} }\nimpl Tq<u8> for S { fn m(&self) {} }\npub struct W<T>(pub T);\n\
             pub trait Tv<T> { fn m(&self) {} }\nimpl<T> Tv<T> for W<T> { fn m(&self) {} }\nimpl Tv<u16> for W<u8> {}\n\
             pub fn c1() { <S as Tr<u16>>::m(&S); }\npub fn c2() { <S as Tr<u8>>::m(&S); }\npub fn c3() { <S as Tw<u32>>::m(&S); }\n\
             pub fn c4() { <S as Td>::m(&S); }\npub fn c5() { <S as Td<u16>>::m(&S); }\npub fn c6() { <S as Tq<_>>::m(&S); }\n\
             pub fn c7() { <W<u8> as Tv<u8>>::m(&W(1u8)); }\npub fn c8() { <W<u8> as Tv<u16>>::m(&W(1u8)); }\n",
        ),
    ];
    // What cargo 1.99 runs: `Tr::m` (c1, the default of `impl Tr<u16>`),
    // `<S as Tr>::m` (c2), the generic `<S as Tw>::m` (c3), `<S as Td>::m`
    // (c4: `Td` alone is `Td<u8>`, its default), `Td::m` (c5), `<S as
    // Tq>::m` (c6, inferred), `<W as Tv>::m` (c7, `impl<T> Tv<T> for W<T>`)
    // and `Tv::m` (c8). The rules do not infer a trait's arguments (c6),
    // nor tell `T` of `Tv<T>` once the self type fixes it (c7); an impl
    // proven for the path's arguments rules out the others (coherence).
    let method = |name: &str| Some(format!("src/lib.rs#{name}#method"));
    let got: Vec<(String, Option<String>)> = calls(&files)
        .into_iter()
        .filter(|(_, target, _)| target != "W")
        .map(|(src, _, dst)| (src, dst))
        .collect();
    assert_eq!(
        got,
        [
            ("c1", method("Tr::m")),
            ("c2", method("<S as Tr>::m")),
            ("c3", method("<S as Tw>::m")),
            ("c4", method("<S as Td>::m")),
            ("c5", method("Td::m")),
            ("c6", None),
            ("c7", None),
            ("c8", method("Tv::m")),
        ]
        .map(|(src, dst)| (src.to_owned(), dst))
    );
}

// D43, D20 (review of the fix): an impl on an alias was compared by the
// arguments written on the alias, none for `impl Grid` (`type Grid =
// M<u8>`), so it fit any value written without arguments, and `impl<T>
// A<T>` (`type A<T> = X<u8, T>`) fit every `X`. The alias's definition
// now stands in its place, on impls and on values, each argument read
// where it is written; an impl on an alias the rules cannot expand (an
// alias of an alias, a parameter left to its default) leaves every probe
// on its type unresolved. Each module is a fixture the review ran with
// cargo 1.99.
#[test]
fn c10_rust_an_impl_on_an_alias_is_for_the_alias_s_definition() {
    let files = [
        ("Cargo.toml", APP),
        (
            "src/lib.rs",
            "pub mod a;\npub mod b;\npub mod c;\npub mod d;\npub mod e;\npub mod f;\npub mod g;\npub mod h;\n",
        ),
        (
            "src/a.rs",
            "pub struct M<T>(pub T);\npub type Grid = M<u8>;\npub type Mask = M<bool>;\n\
             impl Grid { pub fn cell(&self) {} }\npub trait Tr { fn cell(&self); }\n\
             impl Tr for Mask { fn cell(&self) {} }\n\
             pub fn a1(m: &Mask) { m.cell(); }\npub fn a2(g: &Grid) { g.cell(); }\n",
        ),
        (
            "src/b.rs",
            "pub struct X<U, T>(pub U, pub T);\npub type A<T> = X<u8, T>;\n\
             impl<T> A<T> { pub fn get(&self) {} }\npub trait Tr { fn get(&self); }\n\
             impl Tr for X<u16, u32> { fn get(&self) {} }\npub struct G<T>(pub T);\n\
             pub type GB<T> = G<Box<T>>;\nimpl<T> GB<T> { pub fn go(&self) {} }\n\
             pub trait Tg { fn go(&self); }\nimpl Tg for G<u8> { fn go(&self) {} }\n\
             pub fn b1(a: &X<u16, u32>) { a.get(); }\npub fn b2(g: &G<u8>) { g.go(); }\n",
        ),
        (
            "src/c.rs",
            "pub struct X<T = u8>(pub T);\npub type X16 = X<u16>;\npub type X8 = X<u8>;\n\
             impl X16 { pub fn get(&self) {} }\npub trait Tr { fn get(&self); fn put(&self); }\n\
             impl Tr for X { fn get(&self) {} fn put(&self) {} }\nimpl X8 { pub fn put(&self) {} }\n\
             impl Tr for X16 { fn get(&self) {} fn put(&self) {} }\n\
             pub fn c1(x: &X) { x.get(); }\npub fn c2(x: &X8) { x.get(); }\n\
             pub fn c3(x: &X<u8>) { x.get(); }\npub fn c4(x: &X16) { x.put(); }\n\
             pub fn c5(x: &X<u16>) { x.put(); }\n",
        ),
        (
            "src/d.rs",
            "pub struct X<U: Copy, T>(pub U, pub T);\npub type A<T> = X<u8, T>;\n\
             impl<T: Copy> A<T> { pub fn get(&self) {} }\npub trait Tr { fn get(&self); fn put(&self); }\n\
             impl Tr for X<u8, String> { fn get(&self) {} fn put(&self) {} }\n\
             impl A<u16> { pub fn put(&self) {} }\n\
             pub fn d1(a: &X<u8, String>) { a.get(); }\npub fn d2(a: &A<u16>) { a.get(); a.put(); }\n\
             impl A<String> { pub fn own(&self) { self.get(); self.put(); } }\n",
        ),
        (
            "src/e.rs",
            "pub struct M<T>(pub T);\npub type Grid = M<u8>;\npub type Mask = M<bool>;\n\
             pub trait Tr { fn m() {} }\nimpl Tr for Grid { fn m() {} }\nimpl Tr for Mask {}\n\
             pub fn e1() { <Mask as Tr>::m(); }\npub fn e2() { <Grid as Tr>::m(); }\n\
             pub fn e3() { Mask::m(); }\n",
        ),
        (
            "src/f.rs",
            "pub struct X<U, T>(pub U, pub T);\npub type A<T> = X<u8, T>;\n\
             pub trait Tr { fn m(&self) {} }\nimpl<T> Tr for A<T> { fn m(&self) {} }\n\
             impl Tr for X<u16, u32> {}\n\
             pub fn f1(a: &X<u16, u32>) { <X<u16, u32> as Tr>::m(a); }\n\
             pub fn f2(a: &X<u16, u32>) { a.m(); }\n\
             pub fn f3(a: &X<u8, u32>) { <X<u8, u32> as Tr>::m(a); }\n",
        ),
        (
            "src/g.rs",
            "pub struct X<T = u8>(pub T);\npub type A<T = u16> = X<T>;\npub type B = A;\n\
             impl B { pub fn get(&self) {} }\nimpl A { pub fn put(&self) {} }\n\
             pub trait Tr { fn get(&self); fn put(&self); }\n\
             impl Tr for X { fn get(&self) {} fn put(&self) {} }\n\
             pub fn g1(x: &X<u8>) { x.get(); x.put(); }\npub fn g2(x: &X<u16>) { x.get(); x.put(); }\n",
        ),
        (
            "src/h.rs",
            "pub struct P<A, B>(pub A, pub B);\npub type D<T> = P<T, T>;\n\
             impl<T> D<T> { pub fn same(&self) {} }\npub trait Ts { fn same(&self); }\n\
             impl Ts for P<u8, u16> { fn same(&self) {} }\n\
             pub fn h1(p: &P<u8, u16>) { p.same(); }\npub fn h2(p: &P<u8, u8>) { p.same(); }\n",
        ),
    ];
    // cargo 1.99 runs the trait's method in a1, b1, c1, c2, c3, d1, the
    // impl's in b2; the inherent `get` in d2 (`u16: Copy`), the trait's
    // `get` and `put` in `own` (`String` is not `Copy`); g1 the trait's,
    // g2 the inherent ones; h1 the trait's, h2 the inherent one (`D<T>`
    // is `P<T, T>`, which `P<u8, u16>` is not). Unresolved: an impl that
    // may apply beside another candidate (b2: `Box<T>`; c1: `X` is `X<u8>`
    // by a default the rules do not read; d1, `own`: a bound; h1: `T`
    // twice), and every probe on `X` beside impls on aliases the rules
    // cannot expand (g).
    let method = |file: &str, name: &str| Some(format!("src/{file}.rs#{name}#method"));
    let got: Vec<(String, String, Option<String>)> = calls(&files);
    assert_eq!(
        got,
        [
            ("a1", "m.cell", method("a", "<Mask as Tr>::cell")),
            ("a2", "g.cell", method("a", "Grid::cell")),
            ("b1", "a.get", method("b", "<X as Tr>::get")),
            ("b2", "g.go", None),
            ("c1", "x.get", None),
            ("c2", "x.get", method("c", "<X as Tr>::get")),
            ("c3", "x.get", method("c", "<X as Tr>::get")),
            ("c4", "x.put", method("c", "<X16 as Tr>::put")),
            ("c5", "x.put", method("c", "<X16 as Tr>::put")),
            ("d1", "a.get", None),
            ("d2", "a.get", None),
            ("d2", "a.put", method("d", "A::put")),
            ("A::own", "self.get", None),
            ("A::own", "self.put", None),
            ("e1", "<Mask as Tr>::m", method("e", "Tr::m")),
            ("e2", "<Grid as Tr>::m", method("e", "<Grid as Tr>::m")),
            ("e3", "Mask::m", None),
            ("f1", "<X<u16,u32>as Tr>::m", method("f", "Tr::m")),
            ("f2", "a.m", method("f", "Tr::m")),
            ("f3", "<X<u8,u32>as Tr>::m", method("f", "<A as Tr>::m")),
            ("g1", "x.get", None),
            ("g1", "x.put", None),
            ("g2", "x.get", None),
            ("g2", "x.put", None),
            ("h1", "p.same", None),
            ("h2", "p.same", method("h", "D::same")),
        ]
        .map(|(src, target, dst)| (src.to_owned(), target.to_owned(), dst))
    );
}

// D43, D20 (this round): an impl on an alias the rules could not expand
// gave candidates that stood alone, as if no other impl could be the one,
// and an impl on an alias of a type outside the workspace was dropped,
// though the type may be a `Box` the probe looks through, whose methods
// come first: `b.get()` linked `N::get` where rustc runs the `Box`'s. Such
// an impl now leaves every probe on its type unresolved, and its items,
// on an alias of an outside type, are any type's.
#[test]
fn c10_rust_an_impl_on_an_alias_the_rules_cannot_expand_leaves_its_type_unresolved() {
    let files = [
        ("Cargo.toml", APP),
        ("src/lib.rs", "pub mod p;\npub mod q;\npub mod r;\n"),
        (
            "src/p.rs",
            "pub struct N<T>(pub T);\nimpl<T> N<T> { pub fn get(&self) {} }\npub type NB = Box<N<u16>>;\n\
             pub trait Tb { fn get(&self); fn put(self); }\nimpl Tb for NB { fn get(&self) {} fn put(self) {} }\n\
             impl N<u16> { pub fn put(&self) {} }\n\
             pub fn p1(b: &Box<N<u16>>) { b.get(); }\npub fn p2(b: Box<N<u16>>) { b.put(); }\n",
        ),
        (
            "src/q.rs",
            "pub struct X<T = u8, U = u16>(pub T, pub U);\npub type Y<T = u32> = X<T>;\npub type Z = Y;\n\
             pub trait Tr { fn m() {} }\nimpl Tr for Z { fn m() {} }\nimpl Tr for X {}\n\
             pub type S<A, B> = X<B, A>;\npub trait Ts { fn m() {} }\nimpl Ts for S<u8, u16> { fn m() {} }\n\
             impl Ts for X<u8, u16> {}\n\
             pub fn q1() { <X as Tr>::m(); }\npub fn q2() { <Z as Tr>::m(); }\npub fn q3() { <X<u16, u8> as Ts>::m(); }\n",
        ),
        (
            "src/r.rs",
            "pub struct X<T = u8>(pub T);\npub type A<T = u16> = X<T>;\npub type B = A;\n\
             impl B { pub fn only(&self) {} }\npub fn r1(x: &X<u16>) { x.only(); }\n",
        ),
    ];
    // cargo 1.99 runs `<Box<N<u16>> as Tb>::get` and `::put` (p1, p2),
    // `Tr::m`'s default (q1), `<Z as Tr>::m` (q2), `<S as Ts>::m` (q3) and
    // `B::only` (r1).
    assert_eq!(
        calls(&files),
        [
            edge("p1", "b.get", None),
            edge("p2", "b.put", None),
            edge("q1", "<X as Tr>::m", None),
            edge("q2", "<Z as Tr>::m", None),
            edge("q3", "<X<u16,u8>as Ts>::m", None),
            edge("r1", "x.only", None),
        ]
    );
}
