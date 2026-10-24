// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The name-resolved languages (`specs/graph.md`): TypeScript and JavaScript,
//! Python, Go, Java, C# and the generic table (PHP, C, Swift, Elixir, Lua,
//! Kotlin). Each is a grammar,
//! a syntax walker and a [`resolve::Dialect`] over one shared resolver, which
//! proves a site only through the same file or a named import and labels every
//! guess `Likely`.
//!
//! A site's `context` is `["c", callee, receiver]` for a call,
//! `["r", name, arg_of]` for a reference and `["i", spec]` for an import; a
//! file's facts are its imports (`resolve::encode_facts`).

mod imports;
mod resolve;
mod walk;
mod walk_langs;

use pixel_model::{EdgeKind, Lang};
use tree_sitter::Language;

use crate::codec;
use crate::guard;
use crate::lang::{Decision, Def, Extraction, Rules, Site, World};
use crate::text;
use resolve::{Dialect, FactImport};
use walk::{Found, Walker};

/// The rule file of one name-resolved language.
pub struct Names {
    lang: Lang,
    version: u32,
    dialect: Dialect,
    wants: fn(&str) -> bool,
    grammar: fn(&str) -> Language,
    walk: WalkFn,
}

type WalkFn = fn(&mut Walker, tree_sitter::Node, usize);

/// A table row: the dialect of a language whose imports `resolve` reads.
const fn dialect(
    bare_methods: bool,
    resolve_import: fn(&str, &str, &imports::Files) -> Option<imports::Resolved>,
) -> Dialect {
    Dialect {
        bare_methods,
        resolve_import,
        unique_name: true,
    }
}

/// A language the generic walker reads: no imports resolve, and a name
/// defined elsewhere is no `Likely` target.
const fn generic(bare_methods: bool) -> Dialect {
    Dialect {
        bare_methods,
        resolve_import: imports::resolve_none,
        unique_name: false,
    }
}

macro_rules! names {
    ($name:ident, $lang:expr, $dialect:expr, $wants:expr, $grammar:expr, $walk:expr) => {
        pub static $name: Names = Names {
            lang: $lang,
            version: 1,
            dialect: $dialect,
            wants: $wants,
            grammar: $grammar,
            walk: $walk,
        };
    };
}

// TypeScript and JavaScript, one world.
names!(
    TYPESCRIPT,
    Lang::TypeScript,
    dialect(false, imports::resolve_ts),
    wants_typescript,
    grammar_typescript,
    walk::walk_ts
);
names!(
    PYTHON,
    Lang::Python,
    dialect(false, imports::resolve_python),
    |p| extension(p) == Some("py"),
    |_| tree_sitter_python::LANGUAGE.into(),
    walk_langs::walk_python
);
names!(
    GO,
    Lang::Go,
    dialect(false, imports::resolve_go),
    |p| extension(p) == Some("go"),
    |_| tree_sitter_go::LANGUAGE.into(),
    walk_langs::walk_go
);
names!(
    JAVA,
    Lang::Java,
    dialect(true, imports::resolve_java),
    |p| extension(p) == Some("java"),
    |_| tree_sitter_java::LANGUAGE.into(),
    walk_langs::walk_java
);
names!(
    CSHARP,
    Lang::CSharp,
    dialect(true, imports::resolve_none),
    |p| extension(p) == Some("cs"),
    |_| tree_sitter_c_sharp::LANGUAGE.into(),
    walk_langs::walk_csharp
);
names!(
    PHP,
    Lang::Php,
    generic(false),
    |p| extension(p) == Some("php"),
    |_| tree_sitter_php::LANGUAGE_PHP.into(),
    walk_langs::walk_generic
);
names!(
    C,
    Lang::C,
    generic(false),
    |p| matches!(extension(p), Some("c" | "h")),
    |_| tree_sitter_c::LANGUAGE.into(),
    walk_langs::walk_generic
);
names!(
    SWIFT,
    Lang::Swift,
    generic(true),
    |p| extension(p) == Some("swift"),
    |_| tree_sitter_swift::LANGUAGE.into(),
    walk_langs::walk_generic
);
names!(
    ELIXIR,
    Lang::Elixir,
    generic(false),
    |p| matches!(extension(p), Some("ex" | "exs")),
    |_| tree_sitter_elixir::LANGUAGE.into(),
    walk_langs::walk_generic
);
names!(
    LUA,
    Lang::Lua,
    generic(false),
    |p| extension(p) == Some("lua"),
    |_| tree_sitter_lua::LANGUAGE.into(),
    walk_langs::walk_generic
);
names!(
    KOTLIN,
    Lang::Kotlin,
    generic(true),
    |p| matches!(extension(p), Some("kt" | "kts")),
    |_| tree_sitter_kotlin_ng::LANGUAGE.into(),
    walk_langs::walk_generic
);

fn extension(path: &str) -> Option<&str> {
    path.rsplit('/')
        .next()?
        .rsplit_once('.')
        .map(|(_, ext)| ext)
}

fn wants_typescript(path: &str) -> bool {
    matches!(
        extension(path),
        Some("ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs")
    )
}

fn grammar_typescript(path: &str) -> Language {
    match extension(path) {
        Some("tsx") => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Some("ts" | "mts" | "cts") => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        _ => tree_sitter_javascript::LANGUAGE.into(),
    }
}

impl Rules for Names {
    fn lang(&self) -> Lang {
        self.lang
    }

    fn version(&self) -> u32 {
        self.version
    }

    fn wants(&self, path: &str) -> bool {
        (self.wants)(path)
    }

    /// A generated or minified bundle is refused, and indexed unread.
    fn accepts(&self, _path: &str, content: &[u8]) -> bool {
        !guard::is_generated_blob(content)
    }

    fn parse(&self, path: &str, content: &[u8]) -> Option<tree_sitter::Tree> {
        guard::parse(&(self.grammar)(path), content, guard::PARSE_BUDGET)
    }

    fn extract(&self, path: &str, content: &[u8]) -> Extraction {
        let language = (self.grammar)(path);
        let Some(tree) = guard::parse(&language, content, guard::PARSE_BUDGET) else {
            return Extraction::default();
        };
        let mut walker = Walker::new(content);
        (self.walk)(&mut walker, tree.root_node(), 0);
        extraction(content, walker.found)
    }

    fn resolve(&self, world: &World<'_>) -> Vec<Option<usize>> {
        self.decide(world)
            .into_iter()
            .map(|decision| decision.target().map(|(at, _)| at))
            .collect()
    }

    fn decide(&self, world: &World<'_>) -> Vec<Decision> {
        resolve::decide(world, &self.dialect)
    }
}

/// The rows a walk found, as the driver takes them.
fn extraction(content: &[u8], found: Found) -> Extraction {
    let source = String::from_utf8_lossy(content);
    let defs = found
        .symbols
        .into_iter()
        .map(|symbol| Def {
            kind: symbol.kind,
            signature: text::line(&source, symbol.start_line)
                .map(text::signature)
                .unwrap_or_default(),
            name: symbol.qualified,
            line: symbol.start_line,
            end_line: symbol.end_line,
        })
        .collect();
    let mut sites = Vec::new();
    for call in found.calls {
        let target = match &call.receiver {
            Some(receiver) => format!("{receiver}.{}", call.callee),
            None => call.callee.clone(),
        };
        sites.push(Site {
            kind: EdgeKind::Call,
            line: call.line,
            owner: None,
            target,
            context: codec::record(&[
                "c",
                &call.callee,
                call.receiver.as_deref().unwrap_or_default(),
            ]),
        });
    }
    for reference in found.references {
        sites.push(Site {
            kind: EdgeKind::Reference,
            line: reference.line,
            owner: None,
            target: reference.name.clone(),
            context: codec::record(&[
                "r",
                &reference.name,
                reference.arg_of.as_deref().unwrap_or_default(),
            ]),
        });
    }
    let mut facts = Vec::new();
    for import in found.imports {
        sites.push(Site {
            kind: EdgeKind::Import,
            line: import.line,
            owner: None,
            target: import.spec.clone(),
            context: codec::record(&["i", &import.spec]),
        });
        facts.push(FactImport {
            spec: import.spec,
            scope: import.scope,
            bindings: import
                .bindings
                .into_iter()
                .map(|b| (b.local, b.source))
                .collect(),
        });
    }
    Extraction {
        defs,
        sites,
        facts: resolve::encode_facts(&facts),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixel_model::SymbolKind;
    use pixel_model::Tier::{Likely, Proved};

    /// Whether a symbol kind is one the name-resolved walkers emit.
    fn emitted(kind: SymbolKind) -> bool {
        matches!(
            kind,
            SymbolKind::Function
                | SymbolKind::Method
                | SymbolKind::Class
                | SymbolKind::Trait
                | SymbolKind::Enum
        )
    }

    fn extract(path: &str, source: &str) -> Extraction {
        TYPESCRIPT.extract(path, source.as_bytes())
    }

    fn defs(x: &Extraction) -> Vec<(String, SymbolKind, u32)> {
        x.defs
            .iter()
            .map(|d| (d.name.clone(), d.kind, d.line))
            .collect()
    }

    /// The calls by line: a walk meets the outer call of a chain first.
    fn calls(x: &Extraction) -> Vec<(String, u32)> {
        let mut calls: Vec<(String, u32)> = x
            .sites
            .iter()
            .filter(|s| s.kind == EdgeKind::Call)
            .map(|s| (s.target.clone(), s.line))
            .collect();
        calls.sort_by_key(|(_, line)| *line);
        calls
    }

    #[test]
    fn the_extensions_of_both_languages_are_wanted() {
        for path in [
            "a.ts", "a.tsx", "a.mts", "a.cts", "a.js", "a.jsx", "a.mjs", "a.cjs", "x/y.d.ts",
        ] {
            assert!(TYPESCRIPT.wants(path), "{path}");
        }
        for path in ["a.rs", "a.rb", "a", "a.json", "ts", "dir.ts/readme"] {
            assert!(!TYPESCRIPT.wants(path), "{path}");
        }
    }

    #[test]
    fn definitions_are_qualified_by_their_classes_and_typed_by_kind() {
        let x = extract(
            "a.ts",
            "export function top() {}\n\
             class Store {\n\
               open() {}\n\
               handle = async (e) => { e; };\n\
             }\n\
             interface Shape { area(): number }\n\
             enum Color { Red }\n\
             const arrow = () => 1;\n\
             const plain = 3;\n",
        );
        assert_eq!(
            defs(&x),
            [
                ("top".to_owned(), SymbolKind::Function, 1),
                ("Store".to_owned(), SymbolKind::Class, 2),
                ("Store.open".to_owned(), SymbolKind::Method, 3),
                ("Store.handle".to_owned(), SymbolKind::Method, 4),
                ("Shape".to_owned(), SymbolKind::Trait, 6),
                ("Color".to_owned(), SymbolKind::Enum, 7),
                ("arrow".to_owned(), SymbolKind::Function, 8),
            ]
        );
        assert!(x.defs.iter().all(|d| emitted(d.kind)));
    }

    #[test]
    fn a_signature_is_the_source_line_even_after_an_export_keyword() {
        let x = extract("a.ts", "export function top(a: number) {\n  return a;\n}\n");
        assert_eq!(x.defs[0].signature, "export function top(a: number) {");
    }

    #[test]
    fn a_call_sits_on_the_line_of_its_callee_even_in_a_chain() {
        let x = extract(
            "a.ts",
            "function f() {\n  source\n    .map(g)\n    .filter(h);\n  plain();\n}\n",
        );
        assert_eq!(
            calls(&x),
            [
                ("source.map".to_owned(), 3),
                ("source.map(g).filter".to_owned(), 4),
                ("plain".to_owned(), 5),
            ]
        );
        let mut references: Vec<_> = x
            .sites
            .iter()
            .filter(|s| s.kind == EdgeKind::Reference)
            .map(|s| (s.target.clone(), s.line))
            .collect();
        references.sort_by_key(|(_, line)| *line);
        assert_eq!(references, [("g".to_owned(), 3), ("h".to_owned(), 4)]);
    }

    #[test]
    fn a_constructor_a_callback_argument_and_a_self_member_are_sites() {
        let x = extract(
            "a.ts",
            "class A {\n  m() {\n    new Widget(onDone);\n    plugin(this.save, user.name, null);\n  }\n}\n",
        );
        let kinds: Vec<_> = x.sites.iter().map(|s| (s.kind, s.target.clone())).collect();
        assert!(kinds.contains(&(EdgeKind::Call, "Widget".to_owned())));
        assert!(kinds.contains(&(EdgeKind::Reference, "onDone".to_owned())));
        assert!(kinds.contains(&(EdgeKind::Reference, "save".to_owned())));
        assert!(!kinds.iter().any(|(_, t)| t == "name" || t == "null"));
    }

    #[test]
    fn imports_are_sites_and_their_bindings_are_facts() {
        let x = extract(
            "a.ts",
            "import greet, { helper as h, plain } from './lib';\nimport * as ns from './ns';\nexport { re } from './re';\n",
        );
        let specs: Vec<_> = x
            .sites
            .iter()
            .filter(|s| s.kind == EdgeKind::Import)
            .map(|s| (s.target.clone(), s.line))
            .collect();
        assert_eq!(
            specs,
            [
                ("./lib".to_owned(), 1),
                ("./ns".to_owned(), 2),
                ("./re".to_owned(), 3)
            ]
        );
        let facts = resolve::decode_facts(&x.facts);
        assert_eq!(
            facts[0].bindings,
            [
                ("greet".to_owned(), "greet".to_owned()),
                ("h".to_owned(), "helper".to_owned()),
                ("plain".to_owned(), "plain".to_owned()),
            ]
        );
        assert!(facts[1].bindings.is_empty(), "a wildcard binds nothing");
        assert_eq!(facts[2].bindings, [("re".to_owned(), "re".to_owned())]);
    }

    #[test]
    fn jsx_components_are_calls_and_handler_props_are_references() {
        let x = extract(
            "a.jsx",
            "function App() {\n  return <div><Button onClick={save} /><Menu.Item onHover={this.hover}>x</Menu.Item></div>;\n}\n",
        );
        let sites: Vec<_> = x.sites.iter().map(|s| (s.kind, s.target.clone())).collect();
        assert!(sites.contains(&(EdgeKind::Call, "Button".to_owned())));
        assert!(sites.contains(&(EdgeKind::Call, "Menu.Item".to_owned())));
        assert!(sites.contains(&(EdgeKind::Reference, "save".to_owned())));
        assert!(sites.contains(&(EdgeKind::Reference, "hover".to_owned())));
        assert!(
            !sites.iter().any(|(_, t)| t == "div"),
            "an intrinsic element renders no symbol"
        );
    }

    #[test]
    fn a_file_that_does_not_parse_still_yields_what_could_be_read() {
        let x = extract("a.ts", "function ok() {}\nfunction broken( {\n");
        assert!(x.defs.iter().any(|d| d.name == "ok"));
    }

    /// The whole pipeline over a small project: the oracle finds every row
    /// true of the tree, and the proved and likely edges are the ones the
    /// tiers promise.
    #[test]
    fn a_project_indexes_true_to_its_tree_with_the_promised_tiers() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<(String, String)> = [
            (
                "lib/greet.ts",
                "export function greet(name: string) {\n  return shout(name);\n}\nfunction shout(s: string) {\n  return s;\n}\n",
            ),
            (
                "lib/other.ts",
                "export function greet(name: string) {\n  return name;\n}\nexport function unique() {}\n",
            ),
            (
                "main.tsx",
                "import { greet } from './lib/greet';\nimport React from 'react';\nexport function App() {\n  unique();\n  return <Page onLoad={greet} />;\n}\nfunction Page() {\n  return greet('x');\n}\n",
            ),
        ]
        .iter()
        .map(|(path, content)| ((*path).to_owned(), (*content).to_owned()))
        .collect();
        let rows = crate::truth::index(dir.path(), &files);
        assert_eq!(
            crate::truth::violations(dir.path(), &rows),
            Vec::<String>::new()
        );
        let mut found: Vec<(String, u32, String, pixel_model::Tier)> = rows
            .iter()
            .flat_map(|r| &r.edges)
            .filter_map(|e| Some((e.src.clone(), e.line, e.dst.clone()?, e.tier)))
            .collect();
        found.sort();
        let expected = [
            // `shout` is the one function of its file with that name.
            (
                "lib/greet.ts#greet#function",
                2,
                "lib/greet.ts#shout#function",
                pixel_model::Tier::Proved,
            ),
            // `import { greet } from './lib/greet'` resolves to that file.
            (
                "main.tsx#main.tsx#file",
                1,
                "lib/greet.ts#lib/greet.ts#file",
                pixel_model::Tier::Proved,
            ),
            // `unique` is defined once in the repository, in another file.
            (
                "main.tsx#App#function",
                4,
                "lib/other.ts#unique#function",
                pixel_model::Tier::Likely,
            ),
            // The component `Page` is the one function of the file.
            (
                "main.tsx#App#function",
                5,
                "main.tsx#Page#function",
                pixel_model::Tier::Proved,
            ),
            // `greet` is bound by the import, so `lib/other.ts` is no target.
            (
                "main.tsx#App#function",
                5,
                "lib/greet.ts#greet#function",
                pixel_model::Tier::Proved,
            ),
            (
                "main.tsx#Page#function",
                8,
                "lib/greet.ts#greet#function",
                pixel_model::Tier::Proved,
            ),
        ];
        let mut expected: Vec<_> = expected
            .iter()
            .map(|(s, l, d, t)| ((*s).to_owned(), *l, (*d).to_owned(), *t))
            .collect();
        expected.sort();
        assert_eq!(found, expected);
    }

    /// Python, Go, Java and C# projects through the oracle and the tiers: a
    /// relative Python import is proved, a suffix-resolved Go or Java import
    /// is a guess, and a same-file call is proved in all of them.
    #[test]
    fn imports_resolved_by_suffix_are_likely_and_relative_ones_proved() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<(String, String)> = [
            ("pkg/util.py", "def helper():\n    pass\n"),
            (
                "pkg/main.py",
                "from .util import helper\n\ndef run():\n    helper()\n    local()\n\ndef local():\n    pass\n",
            ),
            ("internal/util/u.go", "package util\n\nfunc Shout() {}\n"),
            (
                "cmd/main.go",
                "package main\n\nimport \"example.com/m/internal/util\"\n\nfunc main() {\n\tutil.Shout()\n}\n",
            ),
            (
                "src/com/acme/Foo.java",
                "package com.acme;\npublic class Foo {\n  public void go() {}\n}\n",
            ),
            (
                "src/Main.java",
                "import com.acme.Foo;\nclass Main {\n  void run() {\n    stop();\n  }\n  void stop() {}\n}\n",
            ),
        ]
        .iter()
        .map(|(path, content)| ((*path).to_owned(), (*content).to_owned()))
        .collect();
        let rows = crate::truth::index(dir.path(), &files);
        assert_eq!(
            crate::truth::violations(dir.path(), &rows),
            Vec::<String>::new()
        );
        let mut found: Vec<(String, u32, String, pixel_model::Tier)> = rows
            .iter()
            .flat_map(|r| &r.edges)
            .filter_map(|e| Some((e.src.clone(), e.line, e.dst.clone()?, e.tier)))
            .collect();
        found.sort();
        let tier = |src: &str, line: u32, dst: &str| {
            found
                .iter()
                .find(|(s, l, d, _)| s == src && *l == line && d == dst)
                .map(|(.., t)| *t)
        };
        assert_eq!(
            tier(
                "pkg/main.py#pkg/main.py#file",
                1,
                "pkg/util.py#pkg/util.py#file"
            ),
            Some(Proved)
        );
        assert_eq!(
            tier("pkg/main.py#run#function", 4, "pkg/util.py#helper#function"),
            Some(Proved)
        );
        assert_eq!(
            tier("pkg/main.py#run#function", 5, "pkg/main.py#local#function"),
            Some(Proved)
        );
        assert_eq!(
            tier(
                "cmd/main.go#cmd/main.go#file",
                3,
                "internal/util/u.go#internal/util/u.go#file"
            ),
            Some(Likely)
        );
        assert_eq!(
            tier(
                "src/Main.java#src/Main.java#file",
                1,
                "src/com/acme/Foo.java#src/com/acme/Foo.java#file"
            ),
            Some(Likely)
        );
        assert_eq!(
            tier(
                "src/Main.java#Main.run#method",
                4,
                "src/Main.java#Main.stop#method"
            ),
            Some(Proved)
        );
    }

    #[test]
    fn a_source_file_is_accepted_and_a_minified_bundle_is_refused() {
        let source = b"export function f() {}\n";
        let bundle = format!("var a=1;{}\n", "x".repeat(70_000));
        for rules in [&TYPESCRIPT, &PYTHON, &GO, &KOTLIN] {
            assert!(rules.accepts("a.src", source), "{}", rules.lang());
            assert!(
                !rules.accepts("a.src", bundle.as_bytes()),
                "{}",
                rules.lang()
            );
        }
    }

    #[test]
    fn a_site_context_names_its_form() {
        let x = extract("a.ts", "function f() { a.b(); c(); }\n");
        let contexts: Vec<Vec<String>> =
            x.sites.iter().map(|s| codec::fields(&s.context)).collect();
        assert_eq!(contexts[0], ["c", "b", "a"]);
        assert_eq!(contexts[1], ["c", "c", ""]);
    }
}
