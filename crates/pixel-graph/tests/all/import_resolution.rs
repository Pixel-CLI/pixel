// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use pixel_graph::imports::resolve_import;

fn paths(files: &[&str]) -> Vec<String> {
    files.iter().map(|path| (*path).to_owned()).collect()
}

#[test]
fn ambiguous_suffix_imports_stay_unresolved_in_every_file_order() {
    let cases = [
        (
            "crate::util",
            "crates/caller/src/main.rs",
            ["crates/a/src/util.rs", "crates/b/src/util.rs"],
        ),
        (
            "pkg.util",
            "apps/caller/main.py",
            ["apps/a/pkg/util.py", "apps/b/pkg/util.py"],
        ),
        (
            "com.example.Util",
            "apps/caller/Main.java",
            [
                "apps/a/com/example/Util.java",
                "apps/b/com/example/Util.java",
            ],
        ),
        (
            "example.com/acme/util",
            "svc/caller/main.go",
            ["svc/a/util/a.go", "svc/b/util/b.go"],
        ),
        (
            "com.example.*",
            "apps/caller/Main.java",
            [
                "apps/a/com/example/Util.java",
                "apps/b/com/example/Util.java",
            ],
        ),
    ];
    for (specifier, importer, candidates) in cases {
        let mut files = paths(&candidates);
        assert_eq!(resolve_import(specifier, importer, &files), None);
        files.reverse();
        assert_eq!(resolve_import(specifier, importer, &files), None);
    }
}

#[test]
fn unique_suffix_imports_and_missing_imports_keep_their_behavior() {
    let cases = [
        (
            "crate::util",
            "crates/caller/src/main.rs",
            "crates/a/src/util.rs",
        ),
        ("pkg.util", "apps/caller/main.py", "apps/a/pkg/util.py"),
        (
            "com.example.Util",
            "apps/caller/Main.java",
            "apps/a/com/example/Util.java",
        ),
        (
            "example.com/acme/util",
            "svc/caller/main.go",
            "svc/a/util/a.go",
        ),
        (
            "com.example.*",
            "apps/caller/Main.java",
            "apps/a/com/example/Util.java",
        ),
    ];
    for (specifier, importer, candidate) in cases {
        assert_eq!(
            resolve_import(specifier, importer, &paths(&[candidate])),
            Some(candidate.to_owned())
        );
        assert_eq!(resolve_import(specifier, importer, &[]), None);
    }
}

#[test]
fn several_files_in_one_package_directory_are_one_candidate() {
    // A Go package and a Java package are directories: a second file in the
    // same directory is the same package, not a competing answer. The
    // package's smallest path stands for it, whatever order the walk or the
    // store lists the files in: a full build and an incremental update list
    // them differently and must still store the same import row.
    let cases = [
        (
            "example.com/acme/util",
            "svc/caller/main.go",
            // A smaller path that is not a `.go` file, and a smaller `.go`
            // file outside the package, must both be passed over.
            &[
                "svc/a/util/util.go",
                "svc/a/util/README.md",
                "svc/a/aaa.go",
                "svc/a/util/parse.go",
            ][..],
            "svc/a/util/parse.go",
        ),
        (
            "com.example.*",
            "apps/caller/Main.java",
            &[
                "apps/a/com/example/Util.java",
                "apps/a/com/example/Helper.java",
            ][..],
            "apps/a/com/example/Helper.java",
        ),
    ];
    for (specifier, importer, candidates, expected) in cases {
        let mut files = paths(candidates);
        assert_eq!(
            resolve_import(specifier, importer, &files),
            Some(expected.to_owned()),
            "{specifier} in listed order"
        );
        files.reverse();
        assert_eq!(
            resolve_import(specifier, importer, &files),
            Some(expected.to_owned()),
            "{specifier} in reverse order"
        );
    }
}

#[test]
fn exact_and_relative_paths_do_not_lose_resolution_to_unrelated_suffixes() {
    let cases = [
        (
            "crate::util",
            "src/main.rs",
            "src/util.rs",
            "other/src/util.rs",
        ),
        ("pkg.util", "main.py", "pkg/util.py", "other/pkg/util.py"),
        (".util", "pkg/main.py", "pkg/util.py", "other/pkg/util.py"),
        ("./util", "src/main.ts", "src/util.ts", "other/src/util.ts"),
        (
            "example.com/acme/util",
            "svc/caller/main.go",
            "svc/a/util/a.go",
            "svc/b/other/b.go",
        ),
        (
            "com.example.*",
            "apps/caller/Main.java",
            "apps/a/com/example/Util.java",
            "apps/b/other/Other.java",
        ),
    ];
    for (specifier, importer, expected, other) in cases {
        assert_eq!(
            resolve_import(specifier, importer, &paths(&[other, expected])),
            Some(expected.to_owned())
        );
    }
}

#[test]
fn graph_build_and_incremental_refresh_do_not_invent_ambiguous_import_edges() {
    use pixel_graph::build::{build_graph, update_file};
    use pixel_graph::{EdgeKind, GraphStore};
    use std::fs;

    let root = tempfile::tempdir().unwrap();
    let files = [
        ("crates/a/src/util.rs", "pub fn parse_rust() {}\n"),
        ("crates/b/src/util.rs", "pub fn parse_rust() {}\n"),
        (
            "crates/caller/src/main.rs",
            "use crate::util::parse_rust;\nfn rust_caller() { parse_rust(); }\n",
        ),
        ("apps/a/pkg/util.py", "def parse_python():\n    pass\n"),
        ("apps/b/pkg/util.py", "def parse_python():\n    pass\n"),
        (
            "apps/caller/main.py",
            "from pkg.util import parse_python\ndef python_caller():\n    parse_python()\n",
        ),
        ("svc/a/util/util.go", "package util\n\nfunc ParseGo() {}\n"),
        ("svc/b/util/util.go", "package util\n\nfunc ParseGo() {}\n"),
        (
            "svc/caller/main.go",
            "package main\n\nimport \"example.com/acme/util\"\n\nfunc go_caller() {\n\tutil.ParseGo()\n}\n",
        ),
        (
            "apps/a/com/example/Util.java",
            "package com.example;\n\npublic class Util {\n    public static void parse_java() {}\n}\n",
        ),
        (
            "apps/b/com/example/Util.java",
            "package com.example;\n\npublic class Util {\n    public static void parse_java() {}\n}\n",
        ),
        (
            "apps/caller/Main.java",
            "import com.example.*;\n\npublic class Main {\n    void java_caller() {\n        Util.parse_java();\n    }\n}\n",
        ),
    ];
    for (path, body) in files {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    let db = root.path().join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph(root.path(), &db).unwrap();

    for incremental in [false, true] {
        if incremental {
            for file in [
                "crates/caller/src/main.rs",
                "apps/caller/main.py",
                "svc/caller/main.go",
                "apps/caller/Main.java",
            ] {
                update_file(root.path(), &db, file).unwrap();
            }
        }
        let store = GraphStore::open(&db).unwrap();
        for (caller, callee, file) in [
            ("rust_caller", "parse_rust", "crates/caller/src/main.rs"),
            ("python_caller", "parse_python", "apps/caller/main.py"),
            ("go_caller", "ParseGo", "svc/caller/main.go"),
            ("java_caller", "parse_java", "apps/caller/Main.java"),
        ] {
            let caller = store.symbols_by_name(caller, None, 10).unwrap();
            assert_eq!(caller.len(), 1);
            assert!(
                store
                    .edges_from(caller[0].id, Some(EdgeKind::Calls))
                    .unwrap()
                    .is_empty()
            );
            let file = store.file_by_path(file).unwrap().unwrap();
            let imports: (i64, i64) = store
                .conn()
                .query_row(
                    "SELECT count(*), count(resolved_file_id) FROM imports WHERE file_id = ?1",
                    [file.id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert!(
                imports.0 > 0,
                "fixture must exercise real extracted imports"
            );
            assert_eq!(
                imports.1, 0,
                "ambiguous import must not target any one file"
            );
            assert!(store.envelope_for_name(callee).unwrap().lower_bound);
        }
    }
}
