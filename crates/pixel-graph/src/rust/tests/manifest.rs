// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `Cargo.toml`: the `Package` symbol and the facts the module model reads.

use super::super::manifest::read;
use super::super::{DepKind, DepSource, Dependency, Manifest};
use super::*;

#[test]
fn a_manifest_gives_its_package_at_the_line_of_its_name() {
    let x = extract(
        "crates/mvp-index/Cargo.toml",
        "[package]\n# the indexer\nname = \"mvp-index\" # kebab\nversion = \"0.1.0\"\n",
    );
    assert_eq!(defs(&x), [(SymbolKind::Package, "mvp-index".to_owned())]);
    assert_eq!(x.defs[0].line, 3);
    assert_eq!(x.defs[0].signature, "name = \"mvp-index\" # kebab");
    assert!(x.sites.is_empty());
    assert_eq!(
        Manifest::decode(&x.facts).package.as_deref(),
        Some("mvp-index")
    );
}

#[test]
fn a_workspace_manifest_declares_no_package() {
    let x = extract(
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.package]\nname = \"not-a-package\"\n",
    );
    assert!(x.defs.is_empty());
    assert_eq!(
        Manifest::decode(&x.facts),
        Manifest {
            workspace: true,
            ..Manifest::default()
        }
    );
}

#[test]
fn lib_and_bin_tables_are_read_with_either_quote() {
    let (manifest, line) = read(
        "[package]\nname = 'demo-app'\n\n[lib]\nname = \"demo\"\npath = \"lib/demo.rs\"\n\n\
         [[bin]]\nname = \"tool\"\npath = \"tools/tool.rs\"\n\n[[bin]]\nname = \"other\"\n\n\
         [dependencies]\nname = \"ignored\"\n",
    );
    assert_eq!(
        manifest,
        Manifest {
            package: Some("demo-app".to_owned()),
            lib_name: Some("demo".to_owned()),
            lib_path: Some("lib/demo.rs".to_owned()),
            bins: vec![
                ("tool".to_owned(), "tools/tool.rs".to_owned()),
                ("other".to_owned(), String::new()),
            ],
            // A key of `[dependencies]` is a dependency, never the package's
            // name.
            dependencies: vec![Dependency {
                kind: DepKind::Normal,
                key: "name".to_owned(),
                package: None,
                source: DepSource::Other,
            }],
            ..Manifest::default()
        }
    );
    assert_eq!(line, Some(2));
}

#[test]
fn what_the_line_reader_does_not_read_is_left_out() {
    // Dotted keys, inline tables, inherited and unterminated values are not
    // read: a path they would name stays unresolved.
    let (manifest, line) = read(
        "package.name = \"dotted\"\n[package]\nname.workspace = true\n\
         [lib]\npath = \"src/unterminated.rs\nname = { workspace = true }\n",
    );
    assert_eq!(manifest, Manifest::default());
    assert_eq!(line, None);
}

#[test]
fn an_escaped_quote_is_part_of_the_value() {
    let (manifest, _) = read("[package]\nname = \"we\\\"ird\\\\\"\n");
    assert_eq!(manifest.package.as_deref(), Some("we\"ird\\"));
}

// Review of the M1 fixes: a basic string's escapes other than `\"` and
// `\\` were kept as written, so `name = "re\u0061l"` gave a package
// `re\u0061l` where cargo 1.99 reads `real` (`cargo metadata`), and paths
// through the crate stayed unresolved. Every TOML escape is read now; one
// TOML does not have, or a code that is no character, makes cargo reject
// the manifest, and the line is not read.
#[test]
fn c10_rust_a_manifest_s_escapes_are_read_as_cargo_reads_them() {
    let name = |text: &str| read(&format!("[package]\nname = {text}\n")).0.package;
    for (text, expected) in [
        ("\"re\\u0061l\"", "real"),
        ("\"re\\U00000061l\"", "real"),
        ("\"\\u00e9t\\u00E9\"", "\u{e9}t\u{e9}"),
        (
            "\"a\\tb\\nc\\rd\\be\\ff\\eg\\x41\"",
            "a\tb\nc\rd\u{8}e\u{c}f\u{1b}gA",
        ),
    ] {
        assert_eq!(name(text).as_deref(), Some(expected), "{text}");
    }
    for text in [
        "\"bad\\q\"",
        "\"short\\u006\"",
        "\"sign\\u+061\"",
        "\"surrogate\\ud800\"",
        "\"past\\U00110000\"",
    ] {
        assert_eq!(name(text), None, "{text}");
    }
    let files = [
        ("Cargo.toml", "[workspace]\nmembers = [\"app\", \"real\"]\n"),
        (
            "app/Cargo.toml",
            "[package]\nname = \"app\"\n[dependencies]\nreal = { path = \"../real\" }\n",
        ),
        ("app/src/main.rs", "fn main() { real::go(); }\n"),
        ("real/Cargo.toml", "[package]\nname = \"re\\u0061l\"\n"),
        ("real/src/lib.rs", "pub fn go() {}\n"),
    ];
    assert_eq!(
        calls(&files),
        [edge(
            "main",
            "real::go",
            Some("real/src/lib.rs#go#function")
        )]
    );
}

// D41: the line reader read the lines inside a multi-line string as keys,
// giving a false `Package` symbol `fake` and a crate name that captured the
// paths of another crate.
#[test]
fn c10_rust_no_key_is_read_inside_a_multi_line_string_or_array() {
    let text = "[package]\nname = \"real\"\nkeywords = [\n  \"]\",\n  \"[lib]\",\n]\n\
                description = \"\"\"\nname = \"fake\"\n\"\"\"\nreadme = '''\n[lib]\nname = \"fake\"\n'''\n\
                authors = [\"\"\"\n[[bin]]\nname = \"fake\"\n\"\"\"]\n\n[lib]\npath = \"src/real.rs\"\n";
    let (manifest, line) = read(text);
    assert_eq!(
        manifest,
        Manifest {
            package: Some("real".to_owned()),
            lib_path: Some("src/real.rs".to_owned()),
            ..Manifest::default()
        }
    );
    assert_eq!(line, Some(2));
    let files = [
        ("Cargo.toml", "[workspace]\nmembers = [\"real\", \"app\"]\n"),
        (
            "real/Cargo.toml",
            "[package]\nname = \"real\"\ndescription = \"\"\"\nname = \"fake\"\n\"\"\"\n",
        ),
        ("real/src/lib.rs", "pub fn go() {}\n"),
        (
            "app/Cargo.toml",
            "[package]\nname = \"app\"\n\n[dependencies]\nreal = { path = \"../real\" }\nfake = { path = \"../../fake\" }\n",
        ),
        (
            "app/src/main.rs",
            "fn main() {\n    fake::go();\n    real::go();\n}\n",
        ),
    ];
    let packages: Vec<(SymbolKind, String)> = files
        .iter()
        .flat_map(|(path, src)| defs(&extract(path, src)))
        .filter(|(kind, _)| *kind == SymbolKind::Package)
        .collect();
    assert_eq!(
        packages,
        [
            (SymbolKind::Package, "real".to_owned()),
            (SymbolKind::Package, "app".to_owned())
        ]
    );
    // `cargo run` (rustc 1.99) prints the crate outside the repository's
    // `go`, then the workspace's.
    assert_eq!(
        calls(&files),
        [
            edge("main", "fake::go", None),
            edge("main", "real::go", Some("real/src/lib.rs#go#function")),
        ]
    );
}
