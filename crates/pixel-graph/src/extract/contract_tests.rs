// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the per-language walkers of `extract.rs`: what each
//! language must record as a symbol, a call (with its receiver), a callback
//! reference or an import, and what it must leave out. Every resolver path
//! downstream (`resolve_calls`, `rename`, the ident tier) trusts these rows,
//! so a walker that drops a qualification or invents a call misleads all of
//! them at once.

use std::time::{Duration, Instant};

use tree_sitter::Parser;

use super::{
    FileExtraction, ImportBinding, PARSE_BUDGET, RawSymbol, extract_file, generic_symbol_kind,
    lang_of, language_for, over_budget, parse_file, parse_within,
};
use crate::store::SymbolKind;

fn extract(path: &str, src: &str) -> FileExtraction {
    extract_file(path, src.as_bytes())
        .unwrap_or_else(|| panic!("{path} must extract: its language is wired"))
}

/// `(qualified, kind)` of every symbol, in walk order.
fn symbols(fx: &FileExtraction) -> Vec<(String, SymbolKind)> {
    fx.symbols
        .iter()
        .map(|s| (s.qualified.clone(), s.kind))
        .collect()
}

fn symbol<'a>(fx: &'a FileExtraction, qualified: &str) -> &'a RawSymbol {
    fx.symbols
        .iter()
        .find(|s| s.qualified == qualified)
        .unwrap_or_else(|| panic!("no symbol {qualified} in {:?}", symbols(fx)))
}

/// `(callee, receiver)` of every call, in walk order.
fn calls(fx: &FileExtraction) -> Vec<(String, Option<String>)> {
    fx.calls
        .iter()
        .map(|c| (c.callee_name.clone(), c.receiver.clone()))
        .collect()
}

fn call_receiver(fx: &FileExtraction, callee: &str) -> Option<String> {
    fx.calls
        .iter()
        .find(|c| c.callee_name == callee)
        .unwrap_or_else(|| panic!("no call to {callee} in {:?}", calls(fx)))
        .receiver
        .clone()
}

fn has_call(fx: &FileExtraction, callee: &str) -> bool {
    fx.calls.iter().any(|c| c.callee_name == callee)
}

/// `(name, arg_of)` of every callback reference.
fn references(fx: &FileExtraction) -> Vec<(String, Option<String>)> {
    fx.references
        .iter()
        .map(|r| (r.name.clone(), r.arg_of.clone()))
        .collect()
}

fn import_paths(fx: &FileExtraction) -> Vec<String> {
    fx.imports.iter().map(|i| i.path.clone()).collect()
}

fn owner_of_call(fx: &FileExtraction, callee: &str) -> Option<String> {
    let call = fx
        .calls
        .iter()
        .find(|c| c.callee_name == callee)
        .unwrap_or_else(|| panic!("no call to {callee}"));
    call.enclosing_index
        .map(|i| fx.symbols[i].qualified.clone())
}

// --- language gate ----------------------------------------------------------

#[test]
fn lang_of_should_map_every_wired_extension_to_its_walker_language() {
    let cases = [
        ("a.ts", "ts"),
        ("a.mts", "ts"),
        ("a.cts", "ts"),
        ("a.tsx", "tsx"),
        ("a.js", "js"),
        ("a.jsx", "js"),
        ("a.mjs", "js"),
        ("a.cjs", "js"),
        ("src/a.rs", "rust"),
        ("a.go", "go"),
        ("A.java", "java"),
        ("a.py", "python"),
        ("A.cs", "csharp"),
        ("a.rb", "ruby"),
        ("Rakefile.rake", "ruby"),
        ("x.gemspec", "ruby"),
        ("config.ru", "ruby"),
        ("a.php", "php"),
        ("a.c", "c"),
        ("a.h", "c"),
        ("a.swift", "swift"),
        ("a.ex", "elixir"),
        ("a.exs", "elixir"),
        ("a.lua", "lua"),
    ];
    for (path, lang) in cases {
        assert_eq!(lang_of(path), Some(lang), "{path}");
    }
}

#[test]
fn lang_of_should_reject_unknown_extensions_and_extensionless_files() {
    for path in ["Makefile", "README.md", "a.toml", "dir.d/noext", "a.json"] {
        assert_eq!(lang_of(path), None, "{path}");
    }
}

#[test]
fn lang_of_should_read_the_extension_of_the_file_name_not_of_a_directory() {
    // `pkg.go/README` is a directory named like a Go file: the file has no
    // extension, so it is not Go.
    assert_eq!(lang_of("pkg.go/README"), None);
    assert_eq!(lang_of("pkg.go/main.py"), Some("python"));
}

#[test]
fn extract_file_should_return_none_for_an_unsupported_language() {
    assert!(extract_file("notes.txt", b"fn main() {}").is_none());
    assert!(parse_file("notes.txt", b"fn main() {}").is_none());
}

#[test]
fn parse_file_should_parse_every_wired_language() {
    for (path, src) in [
        ("a.go", "package a\n"),
        ("a.php", "<?php echo 1;\n"),
        ("a.c", "int x;\n"),
        ("a.swift", "let x = 1\n"),
        ("a.ex", "x = 1\n"),
        ("a.lua", "local x = 1\n"),
        ("A.cs", "class A {}\n"),
    ] {
        let tree = parse_file(path, src.as_bytes()).unwrap_or_else(|| panic!("{path}"));
        assert!(!tree.root_node().has_error(), "{path} parses cleanly");
    }
}

// --- signatures --------------------------------------------------------------

#[test]
fn symbol_sig_should_be_the_first_line_of_the_declaration_only() {
    let fx = extract(
        "a.rs",
        "pub fn first(a: u8,\n             b: u8) -> u8 {\n    a + b\n}\n",
    );
    assert_eq!(symbol(&fx, "first").sig, "pub fn first(a: u8,");
}

#[test]
fn symbol_sig_should_be_capped_at_200_bytes_on_a_char_boundary() {
    // A 300-character name made of 2-byte characters: the cap falls inside
    // one character, so the cut must back off to the previous boundary
    // rather than panic on a split UTF-8 sequence.
    let name: String = std::iter::repeat_n('é', 300).collect();
    let src = format!("fn {name}() {{}}\n");
    let fx = extract("a.rs", &src);
    let sig = &fx.symbols[0].sig;
    // `fn ` is 3 bytes and each `é` 2, so byte 200 falls inside a character:
    // the cut keeps the 98 whole ones before it, 199 bytes.
    assert_eq!(sig.len(), 199, "{sig}");
    assert!(sig.starts_with("fn é"));
}

// --- Go ----------------------------------------------------------------------

const GO_SRC: &str = r#"package store

import (
	"fmt"
	str "strings"
)

import "os"

type Store struct {
	path string
}

type Reader interface {
	Read() string
}

type ID int

func New(path string) *Store {
	return &Store{path: path}
}

func (s *Store) Save(data string) error {
	fmt.Println(str.ToUpper(data))
	Register(onSave, nil)
	return s.flush()
}

func (Store) Name() string { return "store" }
"#;

#[test]
fn go_should_record_functions_methods_and_only_struct_or_interface_types() {
    let fx = extract("store/store.go", GO_SRC);
    let syms = symbols(&fx);
    assert!(
        syms.contains(&("Store".into(), SymbolKind::Struct)),
        "{syms:?}"
    );
    assert!(
        syms.contains(&("Reader".into(), SymbolKind::Interface)),
        "{syms:?}"
    );
    assert!(
        syms.contains(&("New".into(), SymbolKind::Function)),
        "{syms:?}"
    );
    // A named non-struct type (`type ID int`) declares no symbol the graph
    // can call or rename through.
    assert!(!syms.iter().any(|(q, _)| q == "ID"), "{syms:?}");
}

#[test]
fn go_method_should_be_qualified_by_its_receiver_type_pointer_or_not() {
    let fx = extract("store/store.go", GO_SRC);
    let syms = symbols(&fx);
    assert!(
        syms.contains(&("Store.Save".into(), SymbolKind::Method)),
        "{syms:?}"
    );
    // An unnamed value receiver `(Store)` still names its type.
    assert!(
        syms.contains(&("Store.Name".into(), SymbolKind::Method)),
        "{syms:?}"
    );
    assert_eq!(symbol(&fx, "Store.Save").name, "Save");
}

#[test]
fn go_calls_should_keep_the_selector_operand_as_receiver() {
    let fx = extract("store/store.go", GO_SRC);
    assert_eq!(call_receiver(&fx, "Println").as_deref(), Some("fmt"));
    assert_eq!(call_receiver(&fx, "ToUpper").as_deref(), Some("str"));
    assert_eq!(call_receiver(&fx, "flush").as_deref(), Some("s"));
    assert_eq!(call_receiver(&fx, "Register"), None);
    assert_eq!(owner_of_call(&fx, "flush").as_deref(), Some("Store.Save"));
}

#[test]
fn go_should_record_a_callback_argument_but_not_nil() {
    let fx = extract("store/store.go", GO_SRC);
    let refs = references(&fx);
    assert!(
        refs.contains(&("onSave".into(), Some("Register".into()))),
        "{refs:?}"
    );
    assert!(!refs.iter().any(|(n, _)| n == "nil"), "{refs:?}");
}

#[test]
fn go_imports_should_list_each_path_unquoted_grouped_or_single() {
    let fx = extract("store/store.go", GO_SRC);
    assert_eq!(import_paths(&fx), vec!["fmt", "strings", "os"]);
    assert!(fx.imports.iter().all(|i| i.bindings.is_empty()));
}

// --- Java --------------------------------------------------------------------

const JAVA_SRC: &str = r#"package com.example;

import java.util.List;
import java.util.*;
import static org.junit.Assert.assertTrue;

public class Outer {
    interface Listener { void fire(); }
    enum Mode { ON, OFF }

    public Outer() { init(); }

    void run(List<String> items) {
        items.forEach(this::handle);
        helper.process(items, callback);
        Widget w = new com.example.ui.Widget<String>(onClick);
        log("done", null);
    }

    class Inner {
        void deep() { }
    }
}
"#;

#[test]
fn java_should_qualify_nested_types_and_members_by_their_enclosing_classes() {
    let fx = extract("src/com/example/Outer.java", JAVA_SRC);
    let syms = symbols(&fx);
    for expected in [
        ("Outer", SymbolKind::Class),
        ("Outer.Listener", SymbolKind::Interface),
        ("Outer.Listener.fire", SymbolKind::Method),
        ("Outer.Mode", SymbolKind::Enum),
        ("Outer.Outer", SymbolKind::Method),
        ("Outer.run", SymbolKind::Method),
        ("Outer.Inner", SymbolKind::Class),
        ("Outer.Inner.deep", SymbolKind::Method),
    ] {
        assert!(
            syms.contains(&(expected.0.into(), expected.1)),
            "missing {expected:?} in {syms:?}"
        );
    }
}

#[test]
fn java_should_pop_the_class_qualifier_when_the_class_body_ends() {
    let src = "class A { void a() {} }\nclass B { void b() {} }\n";
    let fx = extract("A.java", src);
    let syms = symbols(&fx);
    assert!(
        syms.contains(&("B.b".into(), SymbolKind::Method)),
        "{syms:?}"
    );
    assert!(!syms.iter().any(|(q, _)| q.starts_with("A.B")), "{syms:?}");
}

#[test]
fn java_calls_should_keep_the_invocation_object_as_receiver() {
    let fx = extract("src/com/example/Outer.java", JAVA_SRC);
    assert_eq!(call_receiver(&fx, "process").as_deref(), Some("helper"));
    assert_eq!(call_receiver(&fx, "forEach").as_deref(), Some("items"));
    assert_eq!(call_receiver(&fx, "init"), None);
    assert_eq!(owner_of_call(&fx, "init").as_deref(), Some("Outer.Outer"));
}

#[test]
fn java_new_should_call_the_constructor_by_its_simple_name_without_generics() {
    let fx = extract("src/com/example/Outer.java", JAVA_SRC);
    assert!(has_call(&fx, "Widget"), "{:?}", calls(&fx));
    assert!(
        !fx.calls.iter().any(|c| c.callee_name.contains(['<', '.'])),
        "{:?}",
        calls(&fx)
    );
}

#[test]
fn java_should_record_callback_arguments_of_calls_and_constructors() {
    let fx = extract("src/com/example/Outer.java", JAVA_SRC);
    let refs = references(&fx);
    assert!(
        refs.contains(&("callback".into(), Some("process".into()))),
        "{refs:?}"
    );
    assert!(
        refs.contains(&("items".into(), Some("process".into()))),
        "{refs:?}"
    );
    // Constructor arguments have no named callee.
    assert!(refs.contains(&("onClick".into(), None)), "{refs:?}");
    assert!(!refs.iter().any(|(n, _)| n == "null"), "{refs:?}");
}

#[test]
fn java_imports_should_keep_the_path_and_mark_a_wildcard_with_star() {
    let fx = extract("src/com/example/Outer.java", JAVA_SRC);
    let paths = import_paths(&fx);
    assert!(paths.contains(&"java.util.List".to_string()), "{paths:?}");
    assert!(paths.contains(&"java.util.*".to_string()), "{paths:?}");
    assert!(
        paths.contains(&"org.junit.Assert.assertTrue".to_string()),
        "{paths:?}"
    );
}

// --- Python ------------------------------------------------------------------

const PY_SRC: &str = r#"import os
import numpy as np, sys
from pkg.util import helper as h
from . import sibling

def top(x):
    return helper(x)

class Service:
    def handle(self, req):
        self.store.save(req)
        register(on_done, self, None, True)
        return os.path.join("a", "b")

    class Nested:
        def inner(self):
            pass

def after():
    run(self.method, user.name)
"#;

#[test]
fn python_should_tell_functions_from_methods_by_their_enclosing_class() {
    let fx = extract("svc.py", PY_SRC);
    let syms = symbols(&fx);
    for expected in [
        ("top", SymbolKind::Function),
        ("Service", SymbolKind::Class),
        ("Service.handle", SymbolKind::Method),
        ("Service.Nested", SymbolKind::Class),
        ("Service.Nested.inner", SymbolKind::Method),
        ("after", SymbolKind::Function),
    ] {
        assert!(
            syms.contains(&(expected.0.into(), expected.1)),
            "missing {expected:?} in {syms:?}"
        );
    }
}

#[test]
fn python_calls_should_keep_the_attribute_object_as_receiver() {
    let fx = extract("svc.py", PY_SRC);
    assert_eq!(call_receiver(&fx, "save").as_deref(), Some("self.store"));
    assert_eq!(call_receiver(&fx, "join").as_deref(), Some("os.path"));
    assert_eq!(call_receiver(&fx, "helper"), None);
    assert_eq!(
        owner_of_call(&fx, "save").as_deref(),
        Some("Service.handle")
    );
}

#[test]
fn python_callback_arguments_should_skip_literals_and_members_of_other_values() {
    let fx = extract("svc.py", PY_SRC);
    let refs = references(&fx);
    assert!(
        refs.contains(&("on_done".into(), Some("register".into()))),
        "{refs:?}"
    );
    // `self.method` names a method of the enclosing type; `user.name` is data.
    assert!(
        refs.contains(&("method".into(), Some("run".into()))),
        "{refs:?}"
    );
    // `self` parses as an identifier in Python but is the receiver, not a
    // function passed along.
    for literal in ["self", "None", "True", "name", "user"] {
        assert!(
            !refs.iter().any(|(n, _)| n == literal),
            "{literal} in {refs:?}"
        );
    }
}

#[test]
fn python_imports_should_name_the_module_not_the_alias_or_the_imported_name() {
    let fx = extract("svc.py", PY_SRC);
    let paths = import_paths(&fx);
    assert!(paths.contains(&"os".to_string()), "{paths:?}");
    assert!(paths.contains(&"numpy".to_string()), "{paths:?}");
    assert!(paths.contains(&"sys".to_string()), "{paths:?}");
    assert!(paths.contains(&"pkg.util".to_string()), "{paths:?}");
    assert!(!paths.iter().any(|p| p == "np" || p == "h"), "{paths:?}");
}

// --- C# ----------------------------------------------------------------------

const CS_SRC: &str = r#"using System;
using Json = Newtonsoft.Json;

namespace Acme.Billing
{
    public delegate void Notify(string message);

    public record Invoice(int Id);

    public struct Money { }

    public class Service
    {
        public int Count { get; set; }

        public void Run()
        {
            int Local() => 1;
            var list = Factory.Create<List<int>>();
            Parse<int>("1");
            var w = new Acme.Widgets.Widget<int>(OnClick);
            Handle(OnDone, null);
        }
    }
}
"#;

#[test]
fn csharp_should_record_delegates_properties_and_local_functions_as_methods() {
    let fx = extract("Service.cs", CS_SRC);
    let syms = symbols(&fx);
    for expected in [
        ("Acme.Billing", SymbolKind::Module),
        ("Acme.Billing.Notify", SymbolKind::Method),
        ("Acme.Billing.Invoice", SymbolKind::Class),
        ("Acme.Billing.Money", SymbolKind::Class),
        ("Acme.Billing.Service.Count", SymbolKind::Method),
        ("Acme.Billing.Service.Run", SymbolKind::Method),
        ("Acme.Billing.Service.Local", SymbolKind::Method),
    ] {
        assert!(
            syms.contains(&(expected.0.into(), expected.1)),
            "missing {expected:?} in {syms:?}"
        );
    }
}

// --- TypeScript / JavaScript ---------------------------------------------------

const TS_SRC: &str = r#"import def, { named, other as alias } from "./mod";
import * as ns from "./ns";
import "./side-effect";
export { reexported } from "./re";

export interface Shape { area(): number }
export enum Color { Red }
export abstract class Base {
  abstract draw(): void;
  render() { this.draw(); }
}
export function* gen() { yield 1; }
const arrow = () => helper();
const { a, b } = makePair(() => 1);
const plain = 42;
const w = new Widget(onReady);
const x = new ns.Thing();
api.client.send(payload);
register(onAbort, undefined);
"#;

#[test]
fn ts_should_record_interfaces_enums_abstract_classes_and_generators() {
    let fx = extract("src/shapes.ts", TS_SRC);
    let syms = symbols(&fx);
    for expected in [
        ("Shape", SymbolKind::Interface),
        ("Color", SymbolKind::Enum),
        ("Base", SymbolKind::Class),
        ("Base.render", SymbolKind::Method),
        ("gen", SymbolKind::Function),
        ("arrow", SymbolKind::Function),
    ] {
        assert!(
            syms.contains(&(expected.0.into(), expected.1)),
            "missing {expected:?} in {syms:?}"
        );
    }
}

#[test]
fn ts_variable_declarator_should_be_a_symbol_only_for_a_named_function_value() {
    let fx = extract("src/shapes.ts", TS_SRC);
    let syms = symbols(&fx);
    // A destructuring pattern names no function, and a plain value is data.
    assert!(!syms.iter().any(|(q, _)| q.contains('{')), "{syms:?}");
    assert!(
        !syms.iter().any(|(q, _)| q == "plain" || q == "w"),
        "{syms:?}"
    );
}

#[test]
fn ts_new_should_call_only_a_bare_constructor_identifier() {
    let fx = extract("src/shapes.ts", TS_SRC);
    assert!(has_call(&fx, "Widget"), "{:?}", calls(&fx));
    // `new ns.Thing()` has a member constructor: not recorded as a call.
    assert!(!has_call(&fx, "Thing"), "{:?}", calls(&fx));
    assert!(references(&fx).contains(&("onReady".into(), None)));
}

#[test]
fn ts_member_call_should_keep_the_whole_object_expression_as_receiver() {
    let fx = extract("src/shapes.ts", TS_SRC);
    assert_eq!(call_receiver(&fx, "send").as_deref(), Some("api.client"));
    let refs = references(&fx);
    assert!(
        refs.contains(&("onAbort".into(), Some("register".into()))),
        "{refs:?}"
    );
    // `undefined` parses as an identifier in JS/TS but names no function.
    assert!(!refs.iter().any(|(n, _)| n == "undefined"), "{refs:?}");
    assert_eq!(call_receiver(&fx, "draw").as_deref(), Some("this"));
    assert_eq!(owner_of_call(&fx, "draw").as_deref(), Some("Base.render"));
}

#[test]
fn ts_imports_should_carry_their_bindings_except_for_namespace_and_bare_imports() {
    let fx = extract("src/shapes.ts", TS_SRC);
    let by_path = |p: &str| {
        fx.imports
            .iter()
            .find(|i| i.path == p)
            .unwrap_or_else(|| panic!("no import {p} in {:?}", import_paths(&fx)))
    };
    assert_eq!(
        by_path("./mod").bindings,
        vec![
            ImportBinding::named("def"),
            ImportBinding::named("named"),
            ImportBinding::aliased("other", "alias"),
        ]
    );
    assert!(by_path("./ns").bindings.is_empty());
    assert!(by_path("./side-effect").bindings.is_empty());
    assert_eq!(
        by_path("./re").bindings,
        vec![ImportBinding::named("reexported")]
    );
}

#[test]
fn ts_export_without_a_source_should_not_be_an_import() {
    let fx = extract("src/a.ts", "export const x = 1;\nexport { x as y };\n");
    assert!(fx.imports.is_empty(), "{:?}", import_paths(&fx));
}

#[test]
fn js_and_jsx_files_should_use_the_javascript_walker() {
    let fx = extract(
        "web/app.jsx",
        "function App() { return <Panel onClose={close} />; }\n",
    );
    assert_eq!(fx.lang, "js");
    assert!(has_call(&fx, "Panel"), "{:?}", calls(&fx));
    assert!(
        references(&fx).contains(&("close".into(), Some("Panel.onClose".into()))),
        "{:?}",
        references(&fx)
    );
}

#[test]
fn jsx_text_content_should_join_text_and_fall_back_to_aria_label_on_interactive_tags() {
    let src = r#"export const V = () => (
  <div>
    <button aria-label="Close dialog"><Icon /></button>
    <a title="Docs">  Read   </a>
    <span aria-label="ignored"></span>
  </div>
);
"#;
    let fx = extract("v.tsx", src);
    let el = |tag: &str| {
        fx.jsx_elements
            .iter()
            .find(|e| e.tag == tag)
            .unwrap_or_else(|| panic!("no <{tag}>"))
    };
    assert_eq!(el("button").text_content, "Close dialog");
    // Text wins over the attribute fallback.
    assert_eq!(el("a").text_content, "Read");
    // A non-interactive tag gets no fallback text.
    assert_eq!(el("span").text_content, "");
}

#[test]
fn jsx_member_handler_should_reference_its_property_name() {
    let src = "export const F = () => <form onSubmit={this.save} onReset={() => reset()} />;\n";
    let fx = extract("f.tsx", src);
    let refs = references(&fx);
    assert!(
        refs.contains(&("save".into(), Some("form.onSubmit".into()))),
        "{refs:?}"
    );
    // An inline arrow is walked as code: its inner call is a call, not a
    // reference of the handler prop.
    assert!(has_call(&fx, "reset"));
    assert!(!refs.iter().any(|(n, _)| n == "reset"), "{refs:?}");
}

#[test]
fn jsx_namespaced_and_lowercase_tags_should_render_no_component() {
    let src =
        "export const S = () => <svg:rect /> ;\nexport const D = () => <div><Menu.Item /></div>;\n";
    let fx = extract("s.tsx", src);
    assert!(!has_call(&fx, "svg:rect") && !has_call(&fx, "rect"));
    assert!(!has_call(&fx, "div"));
    assert_eq!(call_receiver(&fx, "Item").as_deref(), Some("Menu"));
}

// --- Rust ----------------------------------------------------------------------

#[test]
fn rust_trait_methods_should_be_qualified_by_the_trait() {
    let src = "pub trait Store {\n    fn get(&self) -> u8;\n    fn put(&mut self) {}\n}\n";
    let fx = extract("a.rs", src);
    let syms = symbols(&fx);
    assert!(
        syms.contains(&("Store".into(), SymbolKind::Trait)),
        "{syms:?}"
    );
    assert!(
        syms.contains(&("Store::put".into(), SymbolKind::Method)),
        "{syms:?}"
    );
}

#[test]
fn rust_impl_of_a_generic_type_should_qualify_by_the_bare_type_name() {
    let src = "struct Cache<T>(T);\nimpl<T: Clone> Cache<T> {\n    fn fill(&self) {}\n}\n";
    let fx = extract("a.rs", src);
    let syms = symbols(&fx);
    assert!(
        syms.contains(&("Cache::fill".into(), SymbolKind::Method)),
        "{syms:?}"
    );
    assert!(!symbol(&fx, "Cache::fill").trait_impl);
}

#[test]
fn rust_consts_and_statics_should_be_const_symbols() {
    let fx = extract("a.rs", "const LIMIT: u8 = 1;\nstatic NAME: &str = \"n\";\n");
    let syms = symbols(&fx);
    assert!(
        syms.contains(&("LIMIT".into(), SymbolKind::Const)),
        "{syms:?}"
    );
    assert!(
        syms.contains(&("NAME".into(), SymbolKind::Const)),
        "{syms:?}"
    );
}

#[test]
fn rust_turbofish_call_should_name_the_function_and_keep_its_path() {
    let src = "fn f() {\n    parse::<u8>(\"1\");\n    serde_json::from_str::<V>(s);\n}\n";
    let fx = extract("a.rs", src);
    assert_eq!(call_receiver(&fx, "parse"), None);
    assert_eq!(
        call_receiver(&fx, "from_str").as_deref(),
        Some("serde_json")
    );
}

#[test]
fn rust_test_attributes_from_other_crates_should_also_hide_test_functions() {
    let src = r#"fn runtime() {}
#[tokio::test]
async fn async_test() { runtime(); }
#[rstest::rstest]
#[test_case::test(1)]
fn param_test() { runtime(); }
"#;
    let fx = extract("a.rs", src);
    let syms = symbols(&fx);
    assert_eq!(syms, vec![("runtime".into(), SymbolKind::Function)]);
    assert!(fx.calls.is_empty(), "{:?}", calls(&fx));
}

#[test]
fn rust_cfg_test_on_a_function_should_not_hide_it() {
    // `#[cfg(test)]` hides a test *module*; a function gated on it is a
    // helper some test calls, and is still a definition.
    let src = "#[cfg(test)]\nfn fixture() {}\n";
    let fx = extract("a.rs", src);
    assert_eq!(symbols(&fx), vec![("fixture".into(), SymbolKind::Function)]);
}

fn rust_receiver_of(src: &str, callee: &str) -> Option<String> {
    let fx = extract("src/lib.rs", src);
    call_receiver(&fx, callee)
}

#[test]
fn rust_receiver_should_be_the_impl_type_for_self_new() {
    let src = "struct Pool;\nimpl Pool {\n    fn make() {\n        Self::new().warm();\n    }\n}\n";
    assert_eq!(rust_receiver_of(src, "warm").as_deref(), Some(".Pool"));
}

#[test]
fn rust_receiver_should_be_the_struct_literal_type_and_default_constructor_type() {
    let src = r#"fn f() {
    let a = Config { x: 1 };
    a.apply();
    let b = pixel_git::Runner::default();
    b.run();
    let c = Vec::<u8>::new();
    c.push(1);
}
"#;
    assert_eq!(rust_receiver_of(src, "apply").as_deref(), Some(".Config"));
    assert_eq!(rust_receiver_of(src, "run").as_deref(), Some(".Runner"));
    assert_eq!(rust_receiver_of(src, "push").as_deref(), Some(".Vec"));
}

#[test]
fn rust_receiver_should_keep_the_expression_for_a_non_constructor_call() {
    // `open()` may return anything: only `new`/`default` count as
    // constructors, and a lowercase path is a module function, not a type.
    let src = r#"fn f() {
    let a = Store::open();
    a.read();
    let b = store::new();
    b.write();
}
"#;
    assert_eq!(rust_receiver_of(src, "read").as_deref(), Some(".a"));
    assert_eq!(rust_receiver_of(src, "write").as_deref(), Some(".b"));
}

#[test]
fn rust_receiver_should_follow_a_reference_or_generic_annotation_to_its_path() {
    let src = r#"fn f(r: &mut crate::git::Runner<'_>, list: Vec<u8>, t: impl Tool) {
    r.exec();
    list.len();
    t.call();
}
"#;
    assert_eq!(rust_receiver_of(src, "exec").as_deref(), Some(".Runner"));
    assert_eq!(rust_receiver_of(src, "len").as_deref(), Some(".Vec"));
    // `impl Trait` names no concrete type.
    assert_eq!(rust_receiver_of(src, "call").as_deref(), Some(".t"));
}

#[test]
fn rust_receiver_should_be_the_closure_parameter_type_when_it_is_annotated() {
    let src = r#"fn f(x: Outer) {
    let g = |x: Inner| x.go();
    let h = |x| x.stop();
}
"#;
    assert_eq!(rust_receiver_of(src, "go").as_deref(), Some(".Inner"));
    // An unannotated closure parameter shadows the function's `x: Outer`.
    assert_eq!(rust_receiver_of(src, "stop").as_deref(), Some(".x"));
}

#[test]
fn rust_receiver_should_not_cross_a_pattern_that_rebinds_the_name() {
    let src = r#"fn f(item: Item, maybe: Option<Item>, items: Vec<Item>) {
    for item in items.iter() {
        item.visit();
    }
    if let Some(maybe) = maybe {
        maybe.check();
    }
    match maybe {
        Some(item) => item.matched(),
        None => {}
    }
    while let Some(item) = next() {
        item.looped();
    }
}
"#;
    for callee in ["visit", "matched", "looped"] {
        assert_eq!(
            rust_receiver_of(src, callee).as_deref(),
            Some(".item"),
            "{callee} reads a pattern binding, whose type is not stated"
        );
    }
    assert_eq!(rust_receiver_of(src, "check").as_deref(), Some(".maybe"));
}

#[test]
fn rust_receiver_should_use_the_last_let_before_the_call_and_ignore_later_ones() {
    let src = r#"fn f() {
    let s = First::new();
    let s = Second::new();
    s.go();
    let s = Third::new();
}
"#;
    assert_eq!(rust_receiver_of(src, "go").as_deref(), Some(".Second"));
}

#[test]
fn rust_receiver_should_be_unknown_for_a_destructured_binding() {
    let src = r#"fn f((a, b): (Left, Right)) {
    let (c, d) = pair();
    a.left();
    c.first();
}
"#;
    assert_eq!(rust_receiver_of(src, "left").as_deref(), Some(".a"));
    assert_eq!(rust_receiver_of(src, "first").as_deref(), Some(".c"));
}

#[test]
fn rust_receiver_should_find_an_outer_block_let_from_a_nested_block() {
    let src = r#"fn f() {
    let db = Db::new();
    {
        let other = 1;
        db.query();
    }
}
"#;
    assert_eq!(rust_receiver_of(src, "query").as_deref(), Some(".Db"));
}

// --- generic walker (php, c, swift, elixir, lua) -------------------------------

#[test]
fn generic_symbol_kind_should_classify_declaration_kinds_by_their_words() {
    let cases = [
        ("function_definition", Some((SymbolKind::Function, false))),
        ("method_declaration", Some((SymbolKind::Function, false))),
        ("lambda_definition", Some((SymbolKind::Function, false))),
        ("namespace_definition", Some((SymbolKind::Module, true))),
        ("module_declaration", Some((SymbolKind::Module, true))),
        ("package_declaration", Some((SymbolKind::Module, true))),
        ("class_declaration", Some((SymbolKind::Class, true))),
        ("struct_declaration", Some((SymbolKind::Class, true))),
        ("actor_declaration", Some((SymbolKind::Class, true))),
        ("interface_declaration", Some((SymbolKind::Interface, true))),
        ("trait_declaration", Some((SymbolKind::Trait, true))),
        ("protocol_declaration", Some((SymbolKind::Trait, true))),
        ("enum_declaration", Some((SymbolKind::Enum, false))),
    ];
    for (kind, expected) in cases {
        assert_eq!(generic_symbol_kind(kind), expected, "{kind}");
    }
}

#[test]
fn generic_symbol_kind_should_refuse_declarations_that_define_no_symbol() {
    for kind in [
        "import_declaration",
        "namespace_use_declaration",
        "attribute_declaration",
        "parameter_declaration",
        "preproc_function_definition",
        "deinit_declaration",
        "typealias_declaration",
        "associatedtype_declaration",
        "operator_declaration",
        "property_declaration",
        "class_body",
        "function_call",
    ] {
        assert_eq!(generic_symbol_kind(kind), None, "{kind}");
    }
}

#[test]
fn php_should_record_classes_methods_calls_and_requires() {
    let src = r#"<?php
require_once 'lib/helpers.php';
class Mailer {
    public function send($to) {
        $this->transport->deliver($to);
        format_body($to);
        Logger::info("sent");
    }
}
function standalone() {}
"#;
    let fx = extract("src/Mailer.php", src);
    let syms = symbols(&fx);
    assert!(
        syms.contains(&("Mailer".into(), SymbolKind::Class)),
        "{syms:?}"
    );
    assert!(
        syms.contains(&("Mailer.send".into(), SymbolKind::Function)),
        "{syms:?}"
    );
    assert!(
        syms.contains(&("standalone".into(), SymbolKind::Function)),
        "{syms:?}"
    );
    assert!(has_call(&fx, "deliver"), "{:?}", calls(&fx));
    assert!(has_call(&fx, "format_body"), "{:?}", calls(&fx));
    assert!(
        import_paths(&fx).contains(&"lib/helpers.php".to_string()),
        "{:?}",
        import_paths(&fx)
    );
}

#[test]
fn swift_should_record_classes_protocols_their_functions_and_imports() {
    let src = r#"import Foundation
protocol Drawable { func draw() }
class View {
    func render() { layout() }
}
"#;
    let fx = extract("View.swift", src);
    let syms = symbols(&fx);
    assert!(
        syms.contains(&("View".into(), SymbolKind::Class)),
        "{syms:?}"
    );
    assert!(
        syms.contains(&("View.render".into(), SymbolKind::Function)),
        "{syms:?}"
    );
    assert!(
        syms.contains(&("Drawable".into(), SymbolKind::Trait)),
        "{syms:?}"
    );
    assert!(import_paths(&fx).contains(&"Foundation".to_string()));
}

#[test]
fn lua_function_calls_should_be_recorded_with_their_callee() {
    let src = "local function greet(name)\n  print(name)\nend\ngreet(\"x\")\n";
    let fx = extract("init.lua", src);
    assert!(has_call(&fx, "print"), "{:?}", calls(&fx));
    assert!(has_call(&fx, "greet"), "{:?}", calls(&fx));
}

#[test]
fn csharp_new_should_call_the_simple_type_name_without_namespace_or_generics() {
    let fx = extract("Service.cs", CS_SRC);
    assert!(has_call(&fx, "Widget"), "{:?}", calls(&fx));
    assert!(
        !fx.calls.iter().any(|c| c.callee_name.starts_with("Acme")),
        "{:?}",
        calls(&fx)
    );
}

#[test]
fn csharp_using_directive_should_import_the_plain_namespace() {
    let fx = extract("Service.cs", CS_SRC);
    let paths = import_paths(&fx);
    assert!(paths.contains(&"System".to_string()), "{paths:?}");
    assert!(paths.contains(&"Newtonsoft.Json".to_string()), "{paths:?}");
}

#[test]
fn c_calls_should_name_the_called_function_or_field() {
    let src = "int add(int a) { return helper(a) + o->start(a); }\n";
    let fx = extract("src/add.c", src);
    assert!(has_call(&fx, "helper"), "{:?}", calls(&fx));
    // A call through a member names the field, never the whole `o->start`.
    assert!(has_call(&fx, "start"), "{:?}", calls(&fx));
    assert!(
        !fx.calls.iter().any(|c| c.callee_name.contains("->")),
        "{:?}",
        calls(&fx)
    );
    let refs = references(&fx);
    assert!(
        refs.contains(&("a".into(), Some("helper".into()))),
        "{refs:?}"
    );
}

#[test]
fn elixir_remote_call_should_be_recorded_by_its_dotted_target() {
    let src = "Enum.sum(items)\nLogger.info(msg)\n";
    let fx = extract("lib/cart.ex", src);
    assert!(has_call(&fx, "Enum.sum"), "{:?}", calls(&fx));
    assert!(has_call(&fx, "Logger.info"), "{:?}", calls(&fx));
}

// --- parse budget (#800) ----------------------------------------------------------

/// 262 bytes of `.tsx` (the `Fuzz` job's `timeout-287dc264…` reproducer) on
/// which TSX error recovery ran for minutes without a budget.
fn parse_hang_reproducer() -> Vec<u8> {
    [
        &b"import fu\x00\x00\x00\xfbon Button({ onClick(}# Pconst Page = (=> save()} /><span>te=t</span></di<Button onClick={() => save()} /><span>te=0</s`an></dn onClick={() => save()} /><span>t"[..],
        &[0xa9; 72][..],
        &b"e=t</span></div>;\n"[..],
    ]
    .concat()
}

/// Run `work` on its own thread and give it `cap`: a regression that hangs
/// fails this test instead of holding the whole suite until its timeout.
fn within<T: Send + 'static>(cap: Duration, work: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    rx.recv_timeout(cap)
        .unwrap_or_else(|_| panic!("still running after {cap:?}"))
}

fn tsx_parser() -> Parser {
    let mut parser = Parser::new();
    parser.set_language(&language_for("tsx").unwrap()).unwrap();
    parser
}

/// The budget is a strict bound: a parse that took exactly the budget is
/// still within it, one nanosecond more is over.
#[test]
fn over_budget_should_be_strictly_past_the_budget() {
    let budget = Duration::from_millis(200);
    assert!(!over_budget(Duration::from_millis(199), budget));
    assert!(!over_budget(budget, budget));
    assert!(over_budget(budget + Duration::from_nanos(1), budget));
}

/// A well-formed file parses whole within the budget.
#[test]
fn parse_within_should_return_the_tree_of_a_file_inside_its_budget() {
    let tree = parse_within(&mut tsx_parser(), b"const a = <b>hi</b>;\n", PARSE_BUDGET).unwrap();
    assert_eq!(tree.root_node().kind(), "program");
    assert!(!tree.root_node().has_error());
}

/// A parse past its budget is cancelled and yields no tree, shortly after
/// the budget rather than minutes later (#800).
#[test]
fn parse_within_should_give_up_once_past_its_budget() {
    let started = Instant::now();
    let tree = within(Duration::from_secs(15), || {
        parse_within(
            &mut tsx_parser(),
            &parse_hang_reproducer(),
            Duration::from_millis(100),
        )
        .is_some()
    });
    assert!(!tree, "a cancelled parse yields no tree");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

/// The production paths, concept and symbol extraction alike, return on the
/// reproducer: the file just contributes no rows.
#[test]
fn extraction_should_return_on_a_file_that_stalls_the_parser() {
    let (concepts, extraction) = within(Duration::from_secs(19), || {
        let content = parse_hang_reproducer();
        (
            crate::concept::extract_concepts("src/fuzz.tsx", &content),
            extract_file("src/fuzz.tsx", &content).is_some(),
        )
    });
    assert!(concepts.is_empty(), "{concepts:?}");
    assert!(!extraction);
}

// --- generic-walker and C# regressions (#773–#778) ------------------------------

/// A C function definition names its function: the identifier sits inside
/// the `function_declarator`, through a pointer declarator too (#773).
#[test]
fn c_function_definitions_should_be_symbols() {
    let fx = extract(
        "src/add.c",
        "int add(int a) { return a; }\nchar *name(void) { return 0; }\n",
    );
    assert_eq!(
        symbols(&fx),
        vec![
            ("add".to_string(), SymbolKind::Function),
            ("name".to_string(), SymbolKind::Function)
        ]
    );
}

/// `o->start()` keeps `o` as its receiver: C's `field_expression` holds
/// its operand in the `argument` field (#774).
#[test]
fn c_member_call_should_keep_its_receiver() {
    let fx = extract("src/add.c", "void f(void) { o->start(); helper(); }\n");
    assert_eq!(call_receiver(&fx, "start").as_deref(), Some("o"));
    assert_eq!(call_receiver(&fx, "helper"), None);
}

/// `def`, `defmodule … do` and the head a `def` defines are definitions:
/// only the calls in the body are call sites (#775).
#[test]
fn elixir_definitions_should_not_be_calls() {
    let src = "defmodule Cart do\n  def total(x) do\n    Enum.sum(x)\n  end\n  defp tax(x), do: round(x)\n  def pay(x) when is_integer(x) do\n    charge(x)\n  end\nend\n";
    let fx = extract("lib/cart.ex", src);
    // A guarded head (`pay(x) when …`) is a definition too; the guard's
    // own call (`is_integer`) is a call site.
    assert_eq!(
        calls(&fx),
        vec![
            ("Enum.sum".to_string(), None),
            ("round".to_string(), None),
            ("is_integer".to_string(), None),
            ("charge".to_string(), None)
        ]
    );
}

/// Swift's `call_expression` has no `function` field: the callee is its
/// first named child, a plain name or a navigation (#776).
#[test]
fn swift_calls_should_be_recorded_with_their_receiver() {
    let fx = extract("View.swift", "func f() { layout(); v.draw(x) }\n");
    assert_eq!(
        calls(&fx),
        vec![
            ("layout".to_string(), None),
            ("draw".to_string(), Some("v".to_string()))
        ]
    );
}

/// A double-quoted path parses as `encapsed_string`; it is an import like
/// the single-quoted one (#777).
#[test]
fn php_require_once_with_double_quotes_should_be_an_import() {
    let fx = extract(
        "src/a.php",
        "<?php\nrequire_once \"lib/double.php\";\nrequire_once 'lib/single.php';\nrequire_once \"lib/$name.php\";\n",
    );
    // The interpolated path is decided at run time: no import for it.
    assert_eq!(import_paths(&fx), vec!["lib/double.php", "lib/single.php"]);
}

/// C# generic calls are named without their type arguments, an alias
/// `using` imports the namespace and not the alias, and an identifier
/// passed as an argument is a callback reference (#778).
#[test]
fn csharp_generic_calls_alias_usings_and_callback_arguments() {
    let fx = extract("Service.cs", CS_SRC);
    assert!(has_call(&fx, "Parse"), "{:?}", calls(&fx));
    assert_eq!(call_receiver(&fx, "Create").as_deref(), Some("Factory"));
    assert!(
        !fx.calls.iter().any(|c| c.callee_name.contains('<')),
        "{:?}",
        calls(&fx)
    );
    assert_eq!(import_paths(&fx), vec!["System", "Newtonsoft.Json"]);
    let refs = references(&fx);
    assert!(
        refs.contains(&("OnDone".to_string(), Some("Handle".to_string()))),
        "{refs:?}"
    );
    assert!(refs.contains(&("OnClick".to_string(), None)), "{refs:?}");
    assert!(!refs.iter().any(|(n, _)| n == "null"), "{refs:?}");
}
