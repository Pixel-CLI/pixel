// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Two definitions with the same qualified name in one file (#1217): the
//! same spec helper `def` in two sibling `describe` blocks. Each keeps its
//! own `symbols` row, and no edge or `unresolved_calls` row is written
//! against a symbol id that no longer exists.

use std::fs;
use std::path::Path;

use pixel_graph::GraphStore;
use pixel_graph::build::{build_graph, update_file};

const SPEC: &str = "spec/a_spec.rb";

/// The reproduction of #1217: the call on line 7 runs the line-3 `helper`,
/// the call on line 15 the line-11 one.
const SAME_NAMED_HELPERS: [&str; 19] = [
    "RSpec.describe \"a\" do",
    "  describe \"one\" do",
    "    def helper",
    "      1",
    "    end",
    "",
    "    it { helper }",
    "  end",
    "",
    "  describe \"two\" do",
    "    def helper",
    "      2",
    "    end",
    "",
    "    it { helper }",
    "  end",
    "end",
    "",
    "",
];

/// `(uid, start_line)` of every `helper` row, in line order.
fn helper_rows(store: &GraphStore) -> Vec<(String, u32)> {
    let mut rows: Vec<(String, u32)> = store
        .symbols_by_name("helper", None, 10)
        .unwrap()
        .into_iter()
        .map(|s| (s.uid, s.start_line))
        .collect();
    rows.sort_by_key(|(_, line)| *line);
    rows
}

/// One count per way a row can point at a missing symbol: an edge's caller,
/// an edge's callee, an unresolved call's enclosing symbol.
fn dangling(db: &Path) -> [i64; 3] {
    let conn = rusqlite::Connection::open(db).unwrap();
    let count = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    [
        count("SELECT count(*) FROM edges WHERE src_id NOT IN (SELECT id FROM symbols)"),
        count("SELECT count(*) FROM edges WHERE dst_id NOT IN (SELECT id FROM symbols)"),
        count(
            "SELECT count(*) FROM unresolved_calls WHERE enclosing_symbol_id IS NOT NULL \
             AND enclosing_symbol_id NOT IN (SELECT id FROM symbols)",
        ),
    ]
}

/// Where each `helper` call went: `(site line, start line of the callee)`
/// for an edge of any kind, `(site line, None)` for an unresolved row.
fn helper_calls(db: &Path) -> Vec<(u32, Option<u32>)> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT e.site_line, s.start_line FROM edges e JOIN symbols s ON s.id = e.dst_id \
             WHERE s.name = 'helper' \
             UNION ALL SELECT site_line, NULL FROM unresolved_calls WHERE name = 'helper' \
             ORDER BY 1",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn assert_each_definition_kept(db: &Path, route: &str) {
    let store = GraphStore::open(db).unwrap();
    assert_eq!(
        helper_rows(&store),
        vec![
            (format!("{SPEC}#helper#function"), 3),
            (format!("{SPEC}#helper#function~2"), 11),
        ],
        "{route}: each `def helper` keeps its own row, the second one's uid carries `~2`"
    );
    assert_eq!(
        dangling(db),
        [0, 0, 0],
        "{route}: no edge caller, edge callee or unresolved enclosing symbol is missing"
    );
    // Each call is kept once, as an edge or an unresolved row, and neither
    // reaches the other block's `helper`. Both rows now exist, so nothing
    // singles one out for v1: the calls stay unresolved rather than taking
    // the first or last by order (graph-resolver.md, "Tier honesty").
    let calls = helper_calls(db);
    let sites: Vec<u32> = calls.iter().map(|(site, _)| *site).collect();
    assert_eq!(
        sites,
        vec![7, 15],
        "{route}: one row per call site: {calls:?}"
    );
    assert!(
        !calls.contains(&(7, Some(11))) && !calls.contains(&(15, Some(3))),
        "{route}: a call never links to the other block's `helper`: {calls:?}"
    );
}

#[test]
fn same_named_definitions_in_one_file_should_each_keep_their_row_on_build_and_update() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("spec")).unwrap();
    fs::write(root.path().join(SPEC), SAME_NAMED_HELPERS.join("\n")).unwrap();
    let db = root.path().join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();

    build_graph(root.path(), &db).unwrap();
    assert_each_definition_kept(&db, "full build");

    update_file(root.path(), &db, SPEC).unwrap();
    assert_each_definition_kept(&db, "update_file");
}
