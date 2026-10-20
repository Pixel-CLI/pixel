// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The store on a real SQLite file in a temporary directory.

use std::path::Path;
use std::time::{Duration, SystemTime};

use mvp_model::hash::Hash;
use mvp_model::{
    Delta, Edge, EdgeKind, FileRow, FileRows, FileStat, Graph, Lang, Provenance, Symbol, SymbolKind,
};

use crate::checked::{self, seam};
use crate::schema::FULL_CHECKS;
use crate::{Indexed, Store, StoreError, Writer, db_path, state};

fn symbol(file: &str, name: &str, line: u32) -> Symbol {
    Symbol {
        uid: format!("{file}#{name}#function"),
        file: file.to_owned(),
        kind: SymbolKind::Function,
        name: name.to_owned(),
        signature: format!("fn {name}()"),
        line,
        end_line: line,
    }
}

fn call(src: &Symbol, dst: Option<&Symbol>, target: &str) -> Edge {
    Edge {
        src: src.uid.clone(),
        dst: dst.map(|s| s.uid.clone()),
        kind: EdgeKind::Call,
        line: src.line,
        target: target.to_owned(),
        context: String::new(),
    }
}

fn file(path: &str, symbols: Vec<Symbol>, edges: Vec<Edge>) -> FileRows {
    FileRows {
        file: FileRow {
            path: path.to_owned(),
            lang: Lang::Rust,
            hash: Hash::of(path.as_bytes()),
        },
        facts: String::new(),
        symbols,
        edges,
    }
}

/// The commit of every test generation: a full object name, as git gives it.
const COMMIT: &str = "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00";

/// A generation id: 64 hex digits, as the indexer makes them.
fn id(name: &str) -> String {
    Hash::of(name.as_bytes()).to_hex()
}

/// A delta to the generation whose files are `files`.
fn delta(generation: &str, add: Vec<FileRows>, drop: &[&str], files: &[&str]) -> Delta {
    Delta {
        provenance: Provenance {
            commit: Some(COMMIT.to_owned()),
            dirty: false,
            generation: id(generation),
            extractor: "ruby.1,rust.1".to_owned(),
            indexed_at: 1,
        },
        drop: drop.iter().map(|p| (*p).to_owned()).collect(),
        files: files
            .iter()
            .map(|path| FileRow {
                path: (*path).to_owned(),
                lang: Lang::Rust,
                hash: Hash::of(path.as_bytes()),
            })
            .collect(),
        add,
        redirect: Vec::new(),
        stale: Vec::new(),
        stats: Vec::new(),
    }
}

/// One published generation: `a.rs` with `a` calling `b` (resolved) and
/// `missing` (unresolved), `b.rs` with `b`.
fn published(root: &Path) {
    let a = symbol("a.rs", "a", 1);
    let b = symbol("b.rs", "b", 1);
    let edges = vec![call(&a, Some(&b), "b"), call(&a, None, "missing")];
    let mut writer = Writer::open(root, None).expect("writer");
    writer
        .publish(&delta(
            "gen-1",
            vec![file("a.rs", vec![a], edges), file("b.rs", vec![b], vec![])],
            &[],
            &["a.rs", "b.rs"],
        ))
        .expect("publish");
}

/// The stat the generation keeps beside a file's hash is what a later scan
/// takes the hash from; a file whose stat was not kept is absent, never a
/// zeroed stat, so the scan re-reads it.
#[test]
fn writer_known_exposes_only_stat_kept_files() {
    let dir = tempfile::tempdir().unwrap();
    let stat = FileStat {
        len: 12,
        mtime_ns: 34,
        ctime_ns: 56,
        ino: 78,
    };
    let mut delta = delta(
        "gen-known",
        vec![
            file("a.rs", vec![symbol("a.rs", "a", 1)], Vec::new()),
            file("b.rs", vec![symbol("b.rs", "b", 1)], Vec::new()),
        ],
        &[],
        &["a.rs", "b.rs"],
    );
    delta.stats = vec![("a.rs".to_owned(), Some(stat)), ("b.rs".to_owned(), None)];
    let mut writer = Writer::open(dir.path(), None).unwrap();
    writer.publish(&delta).unwrap();

    let known = writer.known().unwrap();
    assert_eq!(
        known.get("a.rs"),
        Some(&(Hash::of(b"a.rs"), stat)),
        "the kept stat and hash come back together"
    );
    assert!(
        !known.contains_key("b.rs"),
        "no stat on the file row means no reusable hash"
    );
}

fn generation(root: &Path) -> String {
    Store::open(root)
        .expect("readable")
        .provenance()
        .expect("provenance")
        .generation
}

/// Moves the write-ahead log into `graph.db`, so its pages are the file's.
fn checkpoint(root: &Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(db_path(root)).unwrap();
    conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
    conn
}

/// Changes the published index the way a hand edit would.
fn edit(root: &Path, sql: &str) {
    checkpoint(root).execute_batch(sql).unwrap();
}

/// Overwrites the first `len` bytes of page `page` (1-based) with `byte`.
fn smash(root: &Path, page: u32, len: usize, byte: u8) {
    let mut bytes = std::fs::read(db_path(root)).unwrap();
    let start = (page as usize - 1) * 4096;
    bytes[start..start + len].fill(byte);
    std::fs::write(db_path(root), &bytes).unwrap();
}

/// The root page of the table or index `name`.
fn root_page(root: &Path, name: &str) -> u32 {
    checkpoint(root)
        .query_row(
            "SELECT rootpage FROM sqlite_master WHERE name = ?1",
            [name],
            |r| r.get(0),
        )
        .unwrap()
}

/// Replaces every `from` on page `page` (1-based) with `to`, of the same
/// length, and returns how many there were: the cells change, the page
/// stays sound.
fn rekey(root: &Path, page: u32, from: &str, to: &str) -> usize {
    assert_eq!(from.len(), to.len());
    let mut bytes = std::fs::read(db_path(root)).unwrap();
    let start = (page as usize - 1) * 4096;
    let cells = &mut bytes[start..start + 4096];
    let mut count = 0;
    while let Some(at) = cells.windows(from.len()).position(|w| w == from.as_bytes()) {
        cells[at..at + to.len()].copy_from_slice(to.as_bytes());
        count += 1;
    }
    std::fs::write(db_path(root), &bytes).unwrap();
    count
}

/// Runs `sql` on a second connection, as another program would, under a
/// writer that has already checked the file: the writer's next statement
/// reads the damage.
fn damage_under_the_writer(root: &Path, sql: &str) {
    let conn = rusqlite::Connection::open(db_path(root)).unwrap();
    conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, false)
        .unwrap();
    conn.execute_batch(sql).unwrap();
}

/// Makes `edges_dst` and the uid index of `symbols` trade b-trees (both
/// keyed by a uid), with the schema version bumped so that an open
/// connection reads the new schema: every page stays sound, but the rows a
/// publish deletes are missing from the indexes it deletes them from.
fn swap_uid_indexes(root: &Path) {
    let conn = rusqlite::Connection::open(db_path(root)).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA schema_version", [], |r| r.get(0))
        .unwrap();
    drop(conn);
    damage_under_the_writer(
        root,
        &format!(
            "PRAGMA writable_schema = ON;
             UPDATE sqlite_master SET rootpage = (
               SELECT sum(rootpage) FROM sqlite_master
                WHERE name IN ('edges_dst', 'sqlite_autoindex_symbols_1')
             ) - rootpage WHERE name IN ('edges_dst', 'sqlite_autoindex_symbols_1');
             PRAGMA schema_version = {};",
            version + 1
        ),
    );
}

/// Waits until the file system's clock is past every time `graph.db`
/// carries, so that a stamp of it can be recorded even where file times
/// are coarse (a second on ext4 with 128-byte inodes).
fn settle(root: &Path) {
    use std::os::unix::fs::MetadataExt;
    let probe = crate::dir(root).join("settle");
    let started = std::time::Instant::now();
    loop {
        let db = std::fs::metadata(db_path(root)).unwrap();
        std::fs::write(&probe, b"").unwrap();
        let now = std::fs::metadata(&probe).unwrap();
        std::fs::remove_file(&probe).unwrap();
        let newest = (db.mtime(), db.mtime_nsec()).max((db.ctime(), db.ctime_nsec()));
        if (now.mtime(), now.mtime_nsec()) > newest {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the clock stands"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The full checks run on this thread so far.
fn full_checks() -> usize {
    FULL_CHECKS.with(|n| n.get())
}

/// C-19 end to end in the store: the reader refuses the index, the next
/// writer rebuilds it, and the generation it publishes reads back.
fn refused_then_rebuilt(root: &Path, damage: &str) {
    match Store::open(root) {
        Err(StoreError::Unusable(_)) => {}
        Err(other) => panic!("{damage}: refused as {other:?}, not as unusable"),
        Ok(_) => panic!("{damage}: the damaged index was served"),
    }
    let mut writer = Writer::open(root, None).unwrap();
    let indexed = writer.indexed().unwrap();
    assert!(writer.rebuilt().is_some(), "{damage}: not rebuilt");
    assert_eq!(indexed, Indexed::default(), "{damage}");
    writer
        .publish(&delta(
            "gen-2",
            vec![file("c.rs", vec![], vec![])],
            &[],
            &["c.rs"],
        ))
        .unwrap();
    drop(writer);
    assert_eq!(generation(root), id("gen-2"), "{damage}");
}

#[test]
fn a_published_generation_reads_back_with_its_provenance() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let store = Store::open(dir.path()).unwrap();
    let provenance = store.provenance().unwrap();
    assert_eq!(provenance.generation, id("gen-1"));
    assert_eq!(provenance.commit.as_deref(), Some(COMMIT));
    assert_eq!(provenance.extractor, "ruby.1,rust.1");
    let paths: Vec<String> = store.files().unwrap().into_iter().map(|f| f.path).collect();
    assert_eq!(paths, ["a.rs", "b.rs"]);
}

#[test]
fn c10_unresolved_edges_are_counted_and_never_served() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let store = Store::open(dir.path()).unwrap();
    let callees = store.callees("a.rs#a#function").unwrap();
    assert_eq!(callees.len(), 1);
    assert_eq!(callees[0].dst.as_deref(), Some("b.rs#b#function"));
    assert_eq!(store.stats().unwrap().unresolved, 1);
    assert_eq!(store.all_edges().unwrap().len(), 2);
}

/// A reader that follows the index one file at a time (v1's Ruby merge
/// re-reads only the files an update touched) must see, file by file,
/// exactly the edges a reader of every edge sees, unresolved sites
/// included; and must learn which files resolve a site into a file whose
/// definitions changed, since their answer may change with them.
#[test]
fn a_file_s_edges_and_the_files_reaching_it_are_read_one_file_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let a = symbol("a.rs", "a", 1);
    let b = symbol("b.rs", "b", 1);
    let c = symbol("c.rs", "c", 3);
    let mut writer = Writer::open(dir.path(), None).unwrap();
    writer
        .publish(&delta(
            "gen-2",
            vec![file(
                "c.rs",
                vec![c.clone()],
                vec![
                    call(&c, Some(&b), "b"),
                    call(&c, Some(&c), "c"),
                    call(&c, None, "gone"),
                ],
            )],
            &[],
            &["a.rs", "b.rs", "c.rs"],
        ))
        .unwrap();
    drop(writer);
    let store = Store::open(dir.path()).unwrap();
    let mut by_file: Vec<Edge> = ["a.rs", "b.rs", "c.rs"]
        .iter()
        .flat_map(|path| store.edges_of(path).unwrap())
        .collect();
    let mut all = store.all_edges().unwrap();
    let key = |e: &Edge| (e.src.clone(), e.target.clone(), e.dst.clone());
    by_file.sort_by_key(key);
    all.sort_by_key(key);
    assert_eq!(by_file, all);
    let of_a = store.edges_of("a.rs").unwrap();
    assert_eq!(
        of_a,
        vec![call(&a, Some(&b), "b"), call(&a, None, "missing")]
    );
    assert!(store.edges_of("b.rs").unwrap().is_empty());
    assert!(store.edges_of("nowhere.rs").unwrap().is_empty());
    // An unresolved site reaches no file; a resolved one reaches its
    // target's, its own included.
    assert_eq!(store.files_reaching("b.rs").unwrap(), ["a.rs", "c.rs"]);
    assert_eq!(store.files_reaching("c.rs").unwrap(), ["c.rs"]);
    assert!(store.files_reaching("a.rs").unwrap().is_empty());
}

/// An edge names its site's symbol by that symbol's id in the index: an
/// edge leaving a symbol the index does not hold has nothing to name, and
/// a generation holding one would read back an edge from nowhere. Such a
/// delta is refused, and the published generation stays.
#[test]
fn an_edge_leaving_no_symbol_of_the_index_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let c = symbol("c.rs", "c", 1);
    let ghost = symbol("c.rs", "ghost", 2);
    let mut writer = Writer::open(dir.path(), None).unwrap();
    let refused = writer.publish(&delta(
        "gen-2",
        vec![file("c.rs", vec![c], vec![call(&ghost, None, "x")])],
        &[],
        &["a.rs", "b.rs", "c.rs"],
    ));
    assert!(
        matches!(&refused, Err(StoreError::Refused(reason)) if reason.contains("c.rs#ghost#function")),
        "{refused:?}"
    );
    drop(writer);
    assert_eq!(generation(dir.path()), id("gen-1"));
    // A site of a file may leave a symbol of another file the index holds.
    let c = symbol("c.rs", "c", 1);
    let a = symbol("a.rs", "a", 1);
    let mut writer = Writer::open(dir.path(), None).unwrap();
    writer
        .publish(&delta(
            "gen-3",
            vec![file("c.rs", vec![c], vec![call(&a, None, "y")])],
            &[],
            &["a.rs", "b.rs", "c.rs"],
        ))
        .unwrap();
    drop(writer);
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.edges_of("a.rs").unwrap().len(), 3);
}

#[test]
fn dropping_a_file_takes_its_symbols_and_its_edges() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let mut writer = Writer::open(dir.path(), None).unwrap();
    writer
        .publish(&delta("gen-2", vec![], &["a.rs"], &["b.rs"]))
        .unwrap();
    drop(writer);
    let store = Store::open(dir.path()).unwrap();
    assert!(store.symbols("a.rs").unwrap().is_empty());
    assert_eq!(store.stats().unwrap().edges, 0);
    assert!(store.callers("b.rs#b#function").unwrap().is_empty());
}

#[test]
fn a_failed_publish_leaves_the_last_generation_whole() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let duplicate = file(
        "c.rs",
        vec![symbol("c.rs", "c", 1), symbol("c.rs", "c", 2)],
        vec![],
    );
    let mut writer = Writer::open(dir.path(), None).unwrap();
    let failed = writer.publish(&delta(
        "gen-2",
        vec![duplicate],
        &["a.rs"],
        &["b.rs", "c.rs"],
    ));
    assert!(failed.is_err(), "a duplicate uid aborts the transaction");
    drop(writer);
    assert_eq!(generation(dir.path()), id("gen-1"));
    assert_eq!(Store::open(dir.path()).unwrap().stats().unwrap().edges, 2);
}

#[test]
fn a_second_writer_gives_up_when_its_wait_runs_out() {
    let dir = tempfile::tempdir().unwrap();
    let first = Writer::open(dir.path(), None).unwrap();
    let second = Writer::open(dir.path(), Some(Duration::from_millis(50)));
    assert!(matches!(second, Err(StoreError::Busy)));
    drop(first);
    assert!(Writer::open(dir.path(), Some(Duration::from_millis(50))).is_ok());
}

#[test]
fn a_reader_finds_no_index_before_the_first_generation() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(Store::open(dir.path()), Err(StoreError::NoIndex)));
    drop(Writer::open(dir.path(), None).unwrap());
    assert!(matches!(Store::open(dir.path()), Err(StoreError::NoIndex)));
}

#[test]
fn c19_an_index_of_another_schema_version_is_refused_then_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let conn = rusqlite::Connection::open(db_path(dir.path())).unwrap();
    conn.pragma_update(None, "user_version", crate::SCHEMA_VERSION + 1)
        .unwrap();
    drop(conn);
    assert!(matches!(
        Store::open(dir.path()),
        Err(StoreError::Unusable(_))
    ));
    let mut writer = Writer::open(dir.path(), None).unwrap();
    assert!(writer.rebuilt().unwrap().contains("schema version"));
    assert_eq!(writer.indexed().unwrap(), Indexed::default());
}

// IDX-F1-4: uids came to write `%` and `#` in their path escaped under the
// same schema version, so an index written before, holding such a path,
// was refused as damaged, not as another version.
#[test]
fn c19_an_index_written_before_uids_escaped_their_path_is_of_another_version() {
    let dir = tempfile::tempdir().unwrap();
    let a = Symbol {
        uid: "odd%25/a.rs#a#function".to_owned(),
        ..symbol("odd%/a.rs", "a", 1)
    };
    let mut writer = Writer::open(dir.path(), None).unwrap();
    writer
        .publish(&delta(
            "gen-1",
            vec![file("odd%/a.rs", vec![a], vec![])],
            &[],
            &["odd%/a.rs"],
        ))
        .unwrap();
    drop(writer);
    assert!(Store::open(dir.path()).is_ok());
    // Version 2 wrote the path as it is.
    edit(
        dir.path(),
        "UPDATE symbols SET uid = 'odd%/a.rs#a#function'; PRAGMA user_version = 2;",
    );
    match Store::open(dir.path()) {
        Err(StoreError::Unusable(reason)) => {
            assert!(reason.starts_with("schema version 2,"), "{reason}")
        }
        Err(err) => panic!("{err}"),
        Ok(_) => panic!("an index of version 2 was served"),
    }
}

#[test]
fn c19_a_file_that_is_not_a_database_is_refused_then_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    std::fs::write(db_path(dir.path()), b"not a database, just bytes").unwrap();
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db_path(dir.path()).display()));
    }
    assert!(matches!(
        Store::open(dir.path()),
        Err(StoreError::Unusable(_))
    ));
    let writer = Writer::open(dir.path(), None).unwrap();
    assert!(writer.rebuilt().is_some());
}

#[test]
fn c19_a_sqlite_file_of_another_program_is_refused_then_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(crate::dir(dir.path())).unwrap();
    let conn = rusqlite::Connection::open(db_path(dir.path())).unwrap();
    conn.execute_batch("CREATE TABLE notes (body TEXT); INSERT INTO notes VALUES ('x');")
        .unwrap();
    drop(conn);
    assert!(matches!(
        Store::open(dir.path()),
        Err(StoreError::Unusable(_))
    ));
    let writer = Writer::open(dir.path(), None).unwrap();
    assert!(writer.rebuilt().unwrap().contains("not a pixel-mvp index"));
}

#[test]
fn c19_pages_damaged_after_the_header_are_refused_then_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    // Checkpoint the WAL into the main file, then overwrite every page after
    // the first (the header and schema stay readable).
    let conn = rusqlite::Connection::open(db_path(dir.path())).unwrap();
    conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
    drop(conn);
    let mut bytes = std::fs::read(db_path(dir.path())).unwrap();
    let page = 4096.min(bytes.len());
    for byte in &mut bytes[page..] {
        *byte = 0xA5;
    }
    std::fs::write(db_path(dir.path()), &bytes).unwrap();
    let served = Store::open(dir.path()).and_then(|store| store.stats().map(|_| ()));
    assert!(served.is_err(), "a damaged index is never served");
    let mut writer = Writer::open(dir.path(), None).unwrap();
    let indexed = writer.indexed().unwrap();
    assert!(writer.rebuilt().is_some());
    assert_eq!(indexed, Indexed::default());
}

#[test]
fn the_state_directory_lists_its_files_with_their_sizes_without_plugins() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    std::fs::create_dir_all(crate::dir(dir.path()).join("plugins/x")).unwrap();
    std::fs::write(
        crate::dir(dir.path()).join("plugins/x/pixel-plugin.toml"),
        "",
    )
    .unwrap();
    let entries = state(dir.path()).unwrap();
    let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    assert!(paths.contains(&".pixel-mvp/graph.db"));
    assert!(paths.contains(&".pixel-mvp/.gitignore"));
    assert!(paths.iter().all(|p| !p.starts_with(".pixel-mvp/plugins")));
    let db = entries
        .iter()
        .find(|e| e.path == ".pixel-mvp/graph.db")
        .unwrap();
    assert!(db.bytes > 0);
}

#[test]
fn a_delta_made_against_other_rows_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let c = file("c.rs", vec![symbol("c.rs", "c", 1)], vec![]);
    // Made against an index holding nothing: it does not drop a.rs and b.rs.
    let incomplete = delta("gen-2", vec![c], &[], &["c.rs"]);
    let mut writer = Writer::open(dir.path(), None).unwrap();
    assert!(matches!(
        writer.publish(&incomplete),
        Err(StoreError::Refused(_))
    ));
    drop(writer);
    assert_eq!(generation(dir.path()), id("gen-1"));
}

#[test]
fn a_reader_keeps_the_generation_it_opened() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let reader = Store::open(dir.path()).unwrap();
    assert_eq!(reader.stats().unwrap().files, 2);
    let mut writer = Writer::open(dir.path(), None).unwrap();
    writer
        .publish(&delta("gen-2", vec![], &["a.rs"], &["b.rs"]))
        .unwrap();
    drop(writer);
    assert_eq!(reader.provenance().unwrap().generation, id("gen-1"));
    assert_eq!(reader.stats().unwrap().files, 2);
    assert_eq!(generation(dir.path()), id("gen-2"));
}

// D03, D26, D27
#[test]
fn c19_a_damaged_page_of_any_table_or_index_is_refused_then_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let objects: Vec<(String, u32)> = checkpoint(dir.path())
        .prepare("SELECT name, rootpage FROM sqlite_master WHERE rootpage > 0")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(objects.len() >= 8, "{objects:?}");
    for (name, _) in objects {
        let dir = tempfile::tempdir().unwrap();
        published(dir.path());
        let page: u32 = checkpoint(dir.path())
            .query_row(
                "SELECT rootpage FROM sqlite_master WHERE name = ?1",
                [&name],
                |r| r.get(0),
            )
            .unwrap();
        // The page header: the b-tree page cannot be read at all.
        smash(dir.path(), page, 16, 0xFF);
        refused_then_rebuilt(dir.path(), &name);
    }
}

// ADR-0022 §2: the next scan takes a file's hash from the index while its
// stat holds, so the index must keep each file's latest stat: a file only
// touched since keeps its rows, and its stat must still move, or it is
// hashed on every prompt; one whose stat is not to be trusted keeps none.
#[test]
fn every_publish_keeps_each_files_latest_stat() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    assert!(Store::open(dir.path()).unwrap().known().unwrap().is_empty());
    let stat = |n: i64| FileStat {
        len: 7,
        mtime_ns: n,
        ctime_ns: n + 1,
        ino: 42,
    };
    let mut writer = Writer::open(dir.path(), None).unwrap();
    let mut again = delta("gen-1", vec![], &[], &["a.rs", "b.rs"]);
    again.stats = vec![
        ("a.rs".to_owned(), Some(stat(10))),
        ("b.rs".to_owned(), Some(stat(20))),
    ];
    writer.publish(&again).unwrap();
    again.stats = vec![
        ("a.rs".to_owned(), Some(stat(30))),
        ("b.rs".to_owned(), None),
    ];
    writer.publish(&again).unwrap();
    drop(writer);
    let known = Store::open(dir.path()).unwrap().known().unwrap();
    assert_eq!(known.len(), 1);
    assert_eq!(known["a.rs"], (Hash::of(b"a.rs"), stat(30)));
}

// D25
#[test]
fn c19_a_row_the_store_cannot_read_back_is_refused_then_rebuilt() {
    for damage in [
        "UPDATE files SET lang = 'python' WHERE path = 'b.rs'",
        // ADR-0030: a posting the generation does not hold is damage.
        "UPDATE postings SET count = count + 1 WHERE file = 'a.rs'",
        // ADR-0022 §2: a stat is whole or absent; half of one is damage.
        "UPDATE files SET stat_len = 3 WHERE path = 'a.rs'",
        "UPDATE files SET hash = 'not-hex' WHERE path = 'a.rs'",
        "UPDATE files SET hash = replace(hash, substr(hash, 1, 2), '+f') WHERE path = 'a.rs'",
        "UPDATE symbols SET kind = 'lambda' WHERE file = 'b.rs'",
        "UPDATE edges SET kind = 'jump' WHERE dst IS NULL",
        "UPDATE symbols SET line = 'one' WHERE file = 'a.rs'",
        "UPDATE edges SET line = -1 WHERE dst IS NOT NULL",
    ] {
        let dir = tempfile::tempdir().unwrap();
        published(dir.path());
        edit(dir.path(), damage);
        refused_then_rebuilt(dir.path(), damage);
    }
}

// D47
#[test]
fn c19_a_damaged_meta_value_is_refused_then_rebuilt() {
    for damage in [
        "UPDATE meta SET value = 'abcdefg' || char(233) || '123' WHERE key = 'commit'",
        "UPDATE meta SET value = 'c0ffee' WHERE key = 'commit'",
        "UPDATE meta SET value = 'zz' WHERE key = 'indexed_at'",
        "UPDATE meta SET value = 'yes' WHERE key = 'dirty'",
        "UPDATE meta SET value = 'ab' || char(233) || 'cdefg' WHERE key = 'generation'",
        "UPDATE meta SET value = '' WHERE key = 'extractor'",
        "UPDATE meta SET value = 'a.rs' || char(0) || char(0) || 'b.rs' WHERE key = 'stale'",
        "DELETE FROM meta WHERE key = 'indexed_at'",
    ] {
        let dir = tempfile::tempdir().unwrap();
        published(dir.path());
        edit(dir.path(), damage);
        refused_then_rebuilt(dir.path(), damage);
    }
}

// D49
#[test]
fn c19_an_index_of_another_schema_at_the_same_version_is_refused_then_rebuilt() {
    for damage in [
        "DROP TABLE edges; CREATE TABLE edges_v2 (a)",
        "CREATE TABLE notes (body TEXT)",
        "ALTER TABLE meta RENAME COLUMN value TO body",
        "ALTER TABLE files ADD COLUMN size INTEGER",
        "DROP INDEX edges_dst",
    ] {
        let dir = tempfile::tempdir().unwrap();
        published(dir.path());
        edit(dir.path(), damage);
        refused_then_rebuilt(dir.path(), damage);
    }
}

// D45
#[test]
fn c19_a_graph_db_that_is_not_a_file_is_refused_then_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let db = db_path(dir.path());
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
    }
    std::fs::create_dir_all(db.join("inside")).unwrap();
    refused_then_rebuilt(dir.path(), "a directory");
    assert!(db.is_file());

    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let db = db_path(dir.path());
    std::fs::remove_file(&db).unwrap();
    std::os::unix::fs::symlink(dir.path().join("elsewhere"), &db).unwrap();
    refused_then_rebuilt(dir.path(), "a dangling link");
    assert!(db.is_file());
}

// D03, D27: damage done after the writer's open check, by another
// program: two indexes trade their b-trees, so the rows a publish deletes
// are missing from the index it deletes them through. The publish
// rebuilds the index empty and fails; `Writer::retried` runs it again
// (below).
#[test]
fn c19_damage_a_publish_meets_rebuilds_the_index_empty() {
    let dir = tempfile::tempdir().unwrap();
    let a = symbol("a.rs", "a", 1);
    let b = symbol("b.rs", "b", 1);
    let edges = (vec![call(&a, Some(&b), "b")], vec![call(&b, Some(&a), "a")]);
    let mut writer = Writer::open(dir.path(), None).unwrap();
    writer
        .publish(&delta(
            "gen-1",
            vec![
                file("a.rs", vec![a], edges.0),
                file("b.rs", vec![b], edges.1),
            ],
            &[],
            &["a.rs", "b.rs"],
        ))
        .unwrap();
    drop(writer);
    let mut writer = Writer::open(dir.path(), None).unwrap();
    swap_uid_indexes(dir.path());
    writer.indexed().unwrap();
    let failed = writer.publish(&delta("gen-2", vec![], &["a.rs"], &["b.rs"]));
    assert!(
        matches!(&failed, Err(StoreError::Unusable(reason)) if reason.ends_with("rebuilt empty")),
        "{failed:?}"
    );
    drop(writer);
    assert!(matches!(Store::open(dir.path()), Err(StoreError::NoIndex)));
    let mut writer = Writer::open(dir.path(), None).unwrap();
    assert_eq!(writer.indexed().unwrap(), Indexed::default());
}

// P6: the same damage met inside `Writer::retried`, as `index` runs it.
// The run rebuilds the index empty, runs once more against it, and
// publishes.
#[test]
fn c19_damage_a_publish_meets_is_rebuilt_and_published_in_the_same_run() {
    let dir = tempfile::tempdir().unwrap();
    let a = symbol("a.rs", "a", 1);
    let b = symbol("b.rs", "b", 1);
    let edges = (vec![call(&a, Some(&b), "b")], vec![call(&b, Some(&a), "a")]);
    let mut writer = Writer::open(dir.path(), None).unwrap();
    writer
        .publish(&delta(
            "gen-1",
            vec![
                file("a.rs", vec![a], edges.0),
                file("b.rs", vec![b.clone()], edges.1),
            ],
            &[],
            &["a.rs", "b.rs"],
        ))
        .unwrap();
    drop(writer);
    let mut writer = Writer::open(dir.path(), None).unwrap();
    swap_uid_indexes(dir.path());
    let mut read = Vec::new();
    let done = writer.retried(|writer| -> Result<(), StoreError> {
        let files = writer.indexed()?.files;
        read.push(files.len());
        // Made against what it reads: drop a.rs from the stored
        // generation, or add b.rs to an empty one.
        let next = if files.is_empty() {
            let b = file("b.rs", vec![b.clone()], vec![]);
            delta("gen-3", vec![b], &[], &["b.rs"])
        } else {
            delta("gen-2", vec![], &["a.rs"], &["b.rs"])
        };
        writer.publish(&next)
    });
    assert!(done.is_ok(), "{done:?}");
    assert_eq!(
        read,
        [2, 0],
        "once on the damaged index, once on the empty one"
    );
    assert!(writer.rebuilt().is_some_and(|r| r.contains("malformed")));
    drop(writer);
    assert_eq!(generation(dir.path()), id("gen-3"));
}

// P6: a failure that is not damage is not run again.
#[test]
fn a_refused_delta_is_not_run_again() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let mut writer = Writer::open(dir.path(), None).unwrap();
    let mut runs = 0;
    let refused = writer.retried(|writer| {
        runs += 1;
        writer.publish(&delta("gen-2", vec![], &[], &["c.rs"]))
    });
    assert!(
        matches!(refused, Err(StoreError::Refused(_))),
        "{refused:?}"
    );
    assert_eq!(runs, 1);
    assert_eq!(writer.rebuilt(), None);
}

// D46
#[test]
fn a_delta_that_names_other_files_of_the_same_count_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let c = file("c.rs", vec![symbol("c.rs", "c", 1)], vec![]);
    // Leaves a.rs and c.rs, but names a.rs and zzz.rs.
    let other = delta("gen-2", vec![c], &["b.rs"], &["a.rs", "zzz.rs"]);
    let mut writer = Writer::open(dir.path(), None).unwrap();
    assert!(matches!(
        writer.publish(&other),
        Err(StoreError::Refused(_))
    ));
    drop(writer);
    assert_eq!(generation(dir.path()), id("gen-1"));
    let paths: Vec<String> = Store::open(dir.path())
        .unwrap()
        .files()
        .unwrap()
        .into_iter()
        .map(|f| f.path)
        .collect();
    assert_eq!(paths, ["a.rs", "b.rs"]);
}

// D53
#[test]
fn c11_the_stale_files_read_back_whole_whatever_their_paths_hold() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let mut next = delta("gen-2", vec![], &[], &["a.rs", "b.rs"]);
    next.stale = vec!["lib/x\ny.rb".to_owned(), "lib/z, w.rb".to_owned()];
    let mut writer = Writer::open(dir.path(), None).unwrap();
    writer.publish(&next).unwrap();
    drop(writer);
    let stats = Store::open(dir.path()).unwrap().stats().unwrap();
    assert_eq!(stats.stale, next.stale);
    // IDX-FINAL2-1: the next run plans against it too.
    let mut writer = Writer::open(dir.path(), None).unwrap();
    assert_eq!(writer.indexed().unwrap().stale, next.stale);
}

// P1: a value SQLite holds as TEXT but whose bytes are not UTF-8.
#[test]
fn c19_a_value_that_is_not_utf8_is_refused_then_rebuilt() {
    for damage in [
        "UPDATE meta SET value = CAST(x'61ff' AS TEXT) WHERE key = 'commit'",
        "UPDATE meta SET value = CAST(x'ff' AS TEXT) WHERE key = 'stale'",
        "UPDATE files SET facts = CAST(x'ff' AS TEXT) WHERE path = 'a.rs'",
        "UPDATE symbols SET name = CAST(x'ff' AS TEXT) WHERE file = 'b.rs'",
        "UPDATE symbols SET signature = CAST(x'c3' AS TEXT) WHERE file = 'a.rs'",
        "UPDATE edges SET target = CAST(x'ff' AS TEXT) WHERE dst IS NULL",
        "UPDATE edges SET context = CAST(x'80' AS TEXT) WHERE dst IS NOT NULL",
    ] {
        let dir = tempfile::tempdir().unwrap();
        published(dir.path());
        edit(dir.path(), damage);
        refused_then_rebuilt(dir.path(), damage);
    }
}

// P2: rows with no generation to name them.
#[test]
fn c19_a_meta_without_a_generation_is_refused_then_rebuilt() {
    for damage in [
        "DELETE FROM meta WHERE key = 'generation'",
        "DELETE FROM meta",
    ] {
        let dir = tempfile::tempdir().unwrap();
        published(dir.path());
        edit(dir.path(), damage);
        refused_then_rebuilt(dir.path(), damage);
    }
}

// P3: a row the next publish would neither delete nor replace, so every
// publish after it fails on a unique key.
#[test]
fn c19_a_row_that_belongs_to_no_file_is_refused_then_rebuilt() {
    for damage in [
        "DELETE FROM files WHERE path = 'b.rs'",
        "UPDATE symbols SET file = 'b.rs' WHERE file = 'a.rs'",
        "DELETE FROM symbols WHERE file = 'a.rs'",
        "UPDATE edges SET resolved = 0 WHERE dst IS NOT NULL",
        "UPDATE edges SET resolved = 1 WHERE dst IS NULL",
    ] {
        let dir = tempfile::tempdir().unwrap();
        published(dir.path());
        edit(dir.path(), damage);
        refused_then_rebuilt(dir.path(), damage);
    }
}

// P5: a value of another storage class than the one the store writes.
#[test]
fn c19_a_value_of_another_type_is_refused_then_rebuilt() {
    for damage in [
        "UPDATE files SET hash = CAST(hash AS BLOB) WHERE path = 'a.rs'",
        "UPDATE files SET path = CAST(path AS BLOB) WHERE path = 'b.rs'",
        "UPDATE files SET indexed_at = 'yesterday'",
        "UPDATE symbols SET line = 1.5 WHERE file = 'a.rs'",
        "UPDATE symbols SET end_line = 'ten' WHERE file = 'b.rs'",
        "UPDATE edges SET target = CAST(target AS BLOB)",
        "UPDATE edges SET resolved = 2 WHERE dst IS NOT NULL",
    ] {
        let dir = tempfile::tempdir().unwrap();
        published(dir.path());
        edit(dir.path(), damage);
        refused_then_rebuilt(dir.path(), damage);
    }
}

// P4: SQLite falls back to a read-only shared memory when `-shm` cannot be
// opened, so every write after it would fail.
#[test]
fn c19_a_journal_file_that_is_not_a_file_is_refused_then_rebuilt() {
    for suffix in ["-shm", "-wal", "-journal"] {
        let dir = tempfile::tempdir().unwrap();
        published(dir.path());
        let journal = format!("{}{suffix}", db_path(dir.path()).display());
        let _ = std::fs::remove_file(&journal);
        std::fs::create_dir_all(Path::new(&journal).join("inside")).unwrap();
        refused_then_rebuilt(dir.path(), suffix);
        assert!(!Path::new(&journal).is_dir(), "{suffix}");
    }
}

// P7
#[test]
fn a_delta_refused_for_a_hash_says_which_file_and_what_differs() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let mut other = delta("gen-2", vec![], &[], &["a.rs", "b.rs"]);
    other.files[1].hash = Hash::of(b"other bytes");
    let mut writer = Writer::open(dir.path(), None).unwrap();
    match writer.publish(&other) {
        Err(StoreError::Refused(reason)) => {
            assert!(reason.contains("b.rs"), "{reason}");
            assert!(reason.contains("hash"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    drop(writer);
    assert_eq!(generation(dir.path()), id("gen-1"));
}

// R1: index entries changed in place, every page sound. `quick_check`
// does not compare an index with its table, so the reader served these
// as fresh and a publish through them deleted too little.
#[test]
fn c19_an_index_that_disagrees_with_its_table_is_refused_then_rebuilt() {
    for (index, from, to) in [
        (
            "sqlite_autoindex_symbols_1",
            "b.rs#b#function",
            "b.rs#b#functioo",
        ),
        ("symbols_file", "b.rs", "c.rs"),
        ("edges_dst", "b.rs#b#function", "b.rs#b#functioo"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        published(dir.path());
        let page = root_page(dir.path(), index);
        assert!(rekey(dir.path(), page, from, to) > 0, "{index}");
        let quick: String = checkpoint(dir.path())
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(quick, "ok", "{index}: damage of a kind quick_check finds");
        refused_then_rebuilt(dir.path(), index);
    }
}

// R2: a value of another type met by a query after the open check is
// damage, whichever error rusqlite gives it: the writer rebuilds.
#[test]
fn c19_a_value_damaged_after_the_open_check_is_rebuilt() {
    for damage in [
        // InvalidColumnType, met reading the files.
        "UPDATE files SET hash = CAST(hash AS BLOB) WHERE path = 'a.rs'",
        // Utf8Error, met reading the files.
        "UPDATE files SET lang = CAST(x'ff' AS TEXT) WHERE path = 'b.rs'",
        // IntegralValueOutOfRange, met reading the kept symbols.
        "UPDATE symbols SET line = -1 WHERE file = 'a.rs'",
    ] {
        let dir = tempfile::tempdir().unwrap();
        published(dir.path());
        let mut writer = Writer::open(dir.path(), None).unwrap();
        damage_under_the_writer(dir.path(), damage);
        let read = writer.indexed().and_then(|_| writer.kept(&[]));
        assert!(
            matches!(read, Ok(_) | Err(StoreError::Unusable(_))),
            "{damage}: {read:?}"
        );
        assert!(writer.rebuilt().is_some(), "{damage}: not rebuilt");
        drop(writer);
        assert!(
            matches!(Store::open(dir.path()), Err(StoreError::NoIndex)),
            "{damage}"
        );
    }
}

// R3: values `meta` and `files` would parse but the writer never writes:
// a time with a sign or a leading zero, a name or a hash in upper case.
#[test]
fn c19_a_value_in_a_form_the_writer_never_writes_is_refused_then_rebuilt() {
    for damage in [
        "UPDATE meta SET value = '-5' WHERE key = 'indexed_at'",
        "UPDATE meta SET value = '+5' WHERE key = 'indexed_at'",
        "UPDATE meta SET value = '05' WHERE key = 'indexed_at'",
        "UPDATE meta SET value = upper(value) WHERE key = 'commit'",
        "UPDATE meta SET value = upper(value) WHERE key = 'generation'",
        "UPDATE files SET hash = upper(hash) WHERE path = 'a.rs'",
    ] {
        let dir = tempfile::tempdir().unwrap();
        published(dir.path());
        edit(dir.path(), damage);
        refused_then_rebuilt(dir.path(), damage);
    }
}

// ADR-0022: the open check runs once per version of the file. Damage done
// through the file system after a check, even keeping the file's size and
// mtime, changes its ctime, so the next open checks it whole again.
#[test]
fn c19_a_file_checked_once_is_checked_again_once_written() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let page = root_page(dir.path(), "edges_dst");
    // `root_page` wrote the file: a reader checks it whole and stamps it.
    settle(dir.path());
    assert_eq!(generation(dir.path()), id("gen-1"));
    assert!(checked::trusted(dir.path()), "stamped");
    let db = db_path(dir.path());
    let before = std::fs::metadata(&db).unwrap();
    assert!(rekey(dir.path(), page, "b.rs#b#function", "b.rs#b#functioo") > 0);
    let file = std::fs::File::options().write(true).open(&db).unwrap();
    file.set_modified(before.modified().unwrap()).unwrap();
    drop(file);
    let after = std::fs::metadata(&db).unwrap();
    assert_eq!(
        (after.len(), after.modified().unwrap()),
        (before.len(), before.modified().unwrap())
    );
    refused_then_rebuilt(dir.path(), "edges_dst rekeyed under its old mtime");
}

// ADR-0022: a writer stamps the file only when it holds what a check passed
// and the writer's own commits. Damage another program does while the
// writer runs, on pages its publish never reads, is not stamped as
// checked: the next reader checks the file whole and refuses it.
#[test]
fn c19_damage_done_under_a_writer_is_never_stamped_as_checked() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let page = root_page(dir.path(), "symbols_file");
    let mut writer = Writer::open(dir.path(), None).unwrap();
    assert!(rekey(dir.path(), page, "b.rs", "c.rs") > 0);
    writer
        .publish(&delta(
            "gen-2",
            vec![file("d.rs", vec![], vec![])],
            &[],
            &["a.rs", "b.rs", "d.rs"],
        ))
        .expect("the publish never reads the damaged index");
    drop(writer);
    match Store::open(dir.path()) {
        Err(StoreError::Unusable(_)) => {}
        Err(other) => panic!("refused as {other:?}, not as unusable"),
        Ok(_) => panic!("damage done under the writer was served as checked"),
    }
}

// ADR-0022, review of #52: on a file system whose times are coarse, a
// write in the same tick as the stamp's stat leaves device, inode, size,
// mtime and ctime as they were. A stamp is recorded only when the times it
// holds are older than the file system's clock, so such a write cannot
// make a damaged file read as checked.
#[test]
fn c19_a_write_in_the_tick_of_the_stamp_is_checked_again() {
    seam::freeze();
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let page = root_page(dir.path(), "edges_dst");
    drop(Writer::open(dir.path(), None).unwrap());
    assert!(rekey(dir.path(), page, "b.rs#b#function", "b.rs#b#functioo") > 0);
    refused_then_rebuilt(dir.path(), "edges_dst rekeyed in the tick of the stamp");
}

// Review of #52: damage another process does after the writer's open
// check read the file, before the stamp, is never stamped as checked.
#[test]
fn c19_damage_done_during_the_open_check_is_never_stamped_as_checked() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let page = root_page(dir.path(), "symbols_file");
    checked::forget(dir.path());
    settle(dir.path());
    let root = dir.path().to_path_buf();
    seam::during(move || assert!(rekey(&root, page, "b.rs", "c.rs") > 0));
    let writer = Writer::open(dir.path(), None).unwrap();
    assert_eq!(writer.rebuilt(), None, "the check read the file sound");
    drop(writer);
    match Store::open(dir.path()) {
        Err(StoreError::Unusable(_)) => {}
        Err(other) => panic!("refused as {other:?}, not as unusable"),
        Ok(_) => panic!("damage done during the open check was served as checked"),
    }
}

// Review of #52: damage another process does while a publish commits, on
// pages the publish never writes, is never stamped as checked.
#[test]
fn c19_damage_done_during_a_publish_is_never_stamped_as_checked() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let page = root_page(dir.path(), "symbols_file");
    // Back to WAL, then a writer that finds the file as it left it.
    drop(Writer::open(dir.path(), None).unwrap());
    settle(dir.path());
    let mut writer = Writer::open(dir.path(), None).unwrap();
    assert!(checked::trusted(dir.path()), "the writer stamped the file");
    let root = dir.path().to_path_buf();
    seam::during(move || assert!(rekey(&root, page, "b.rs", "c.rs") > 0));
    writer
        .publish(&delta(
            "gen-2",
            vec![file("d.rs", vec![], vec![])],
            &[],
            &["a.rs", "b.rs", "d.rs"],
        ))
        .expect("the publish never reads the damaged index");
    drop(writer);
    match Store::open(dir.path()) {
        Err(StoreError::Unusable(_)) => {}
        Err(other) => panic!("refused as {other:?}, not as unusable"),
        Ok(_) => panic!("damage done during the publish was served as checked"),
    }
}

// #1204: the writer trusts a stamp while the full check behind it is less
// than a day old, as the readers do: damage stamped as checked, by
// whatever way, in pages no read of the run reports, is served until then.
// The first writer after that checks the file whole and rebuilds it.
#[test]
fn c19_damage_stamped_as_checked_is_caught_by_the_writer_once_its_check_is_a_day_old() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let page = root_page(dir.path(), "symbols_file");
    assert!(rekey(dir.path(), page, "b.rs", "c.rs") > 0);
    let now = SystemTime::now();
    let stamp = |at| checked::record(dir.path(), &checked::Stamp::of(dir.path()).unwrap(), at);
    stamp(now - checked::TRUSTED_FOR + Duration::from_secs(60));
    let writer = Writer::open(dir.path(), None).unwrap();
    assert_eq!(
        writer.rebuilt(),
        None,
        "a stamp checked a day ago less a minute was not trusted"
    );
    drop(writer);
    stamp(now - checked::TRUSTED_FOR);
    let writer = Writer::open(dir.path(), None).unwrap();
    assert!(
        writer.rebuilt().is_some(),
        "stamped damage was trusted after a day"
    );
}

// #1204: a stamp is recent while its check is less than `TRUSTED_FOR` old,
// and never when the check reads as later than now (a clock set back).
#[test]
fn c19_a_check_is_recent_for_less_than_a_day_and_never_from_the_future() {
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let day = checked::TRUSTED_FOR;
    assert_eq!(day, Duration::from_secs(24 * 60 * 60));
    assert!(checked::recent(at, at));
    assert!(checked::recent(at, at + day - Duration::from_nanos(1)));
    assert!(!checked::recent(at, at + day));
    assert!(!checked::recent(at, at - Duration::from_secs(1)));
}

// #1204: a writer that finds the files as a recent check stamped them
// skips the full check, and keeps the time of that check in the stamp it
// records; a file written since is checked whole.
#[test]
fn c19_a_writer_checks_whole_only_a_file_changed_since_its_stamp() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    settle(dir.path());
    drop(Writer::open(dir.path(), None).unwrap());
    settle(dir.path());
    let hour_ago = SystemTime::now() - Duration::from_secs(3600);
    checked::record(
        dir.path(),
        &checked::Stamp::of(dir.path()).unwrap(),
        hour_ago,
    );
    let before = full_checks();
    let writer = Writer::open(dir.path(), None).unwrap();
    assert_eq!(full_checks(), before, "a stamped file was checked whole");
    assert_eq!(writer.rebuilt(), None);
    drop(writer);
    assert_eq!(
        checked::checked_at(dir.path()).map(seconds),
        Some(seconds(hour_ago)),
        "the writer's stamp moved the time of the check it trusted"
    );
    let db = db_path(dir.path());
    let file = std::fs::File::options().write(true).open(&db).unwrap();
    file.set_modified(std::fs::metadata(&db).unwrap().modified().unwrap())
        .unwrap();
    drop(file);
    let started = SystemTime::now();
    let writer = Writer::open(dir.path(), None).unwrap();
    assert_eq!(
        full_checks(),
        before + 1,
        "a changed file was not checked whole"
    );
    assert_eq!(writer.rebuilt(), None);
    drop(writer);
    let at = checked::checked_at(dir.path()).expect("stamped after its check");
    assert!(seconds(at) >= seconds(started), "{at:?} before {started:?}");
}

// #1204: a run that fails forgets the stamp, so the next writer checks the
// file whole; one that succeeds keeps it.
#[test]
fn c19_a_failed_run_forgets_the_stamp() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    settle(dir.path());
    let mut writer = Writer::open(dir.path(), None).unwrap();
    assert!(checked::trusted(dir.path()), "the writer stamped the file");
    assert_eq!(writer.retried(|_| Ok::<_, ()>(1)), Ok(1));
    assert!(
        checked::trusted(dir.path()),
        "a run that succeeded forgot the stamp"
    );
    assert_eq!(writer.retried(|_| Err::<(), _>("failed")), Err("failed"));
    assert!(!checked::trusted(dir.path()), "a failed run kept the stamp");
    drop(writer);
    settle(dir.path());
    let before = full_checks();
    drop(Writer::open(dir.path(), None).unwrap());
    assert_eq!(
        full_checks(),
        before + 1,
        "checked whole after the failed run"
    );
}

// #1204: a reader whose full check passed stamps the time that check
// started, to the second.
#[test]
fn c19_a_readers_stamp_names_when_its_check_ran() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    checked::forget(dir.path());
    settle(dir.path());
    let started = SystemTime::now();
    assert_eq!(generation(dir.path()), id("gen-1"));
    let ended = SystemTime::now();
    let at = checked::checked_at(dir.path()).expect("the reader stamped the file");
    assert!(
        (seconds(started)..=seconds(ended)).contains(&seconds(at)),
        "{at:?} outside {started:?}..{ended:?}"
    );
}

/// Whole seconds since the epoch: what a stamp keeps of a time.
fn seconds(at: SystemTime) -> u64 {
    at.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs()
}

// Review of #52: the stamp never waits for a reader to end its read: a
// publish under an open reader returns at once, and what it stamps reads
// back once the reader is gone.
#[test]
fn a_publish_never_waits_for_an_open_reader() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let reader = Store::open(dir.path()).unwrap();
    let started = std::time::Instant::now();
    let mut writer = Writer::open(dir.path(), None).unwrap();
    writer
        .publish(&delta("gen-2", vec![], &["a.rs"], &["b.rs"]))
        .unwrap();
    let took = started.elapsed();
    drop(writer);
    assert!(took < Duration::from_millis(500), "{took:?}");
    assert_eq!(reader.provenance().unwrap().generation, id("gen-1"));
    drop(reader);
    assert_eq!(generation(dir.path()), id("gen-2"));
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.stats().unwrap().files, 1);
}

// ADR-0022: a file whose stamp matches is not checked again.
#[test]
fn c19_a_stamped_file_is_not_checked_again() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    settle(dir.path());
    drop(Store::open(dir.path()).unwrap());
    let before = full_checks();
    assert_eq!(generation(dir.path()), id("gen-1"));
    assert_eq!(full_checks(), before, "a stamped file was checked again");
}

// Review of #52: a reader whose full check passed stamps the file too, so
// the readers after it skip the check.
#[test]
fn c19_a_reader_whose_check_passed_stamps_the_file() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    checked::forget(dir.path());
    settle(dir.path());
    let before = full_checks();
    assert_eq!(generation(dir.path()), id("gen-1"));
    assert_eq!(full_checks(), before + 1, "checked whole");
    assert_eq!(generation(dir.path()), id("gen-1"));
    assert_eq!(full_checks(), before + 1, "checked again");
}

// Review of #52: a stamp names the version of the check that passed, so a
// build whose check is stricter checks the file again.
#[test]
fn c19_a_stamp_of_an_older_check_is_not_trusted() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    settle(dir.path());
    drop(Store::open(dir.path()).unwrap());
    assert!(checked::trusted(dir.path()));
    let path = crate::dir(dir.path()).join(checked::CHECKED);
    let text = std::fs::read_to_string(&path).unwrap();
    let (head, rest) = text.split_once('\n').unwrap();
    let words: Vec<&str> = head.split(' ').collect();
    let at = words
        .iter()
        .position(|w| *w == "check")
        .expect("the stamp names the version of the check");
    let older = words[at + 1].parse::<u32>().unwrap() - 1;
    let mut head: Vec<String> = words.iter().map(|w| (*w).to_owned()).collect();
    head[at + 1] = older.to_string();
    std::fs::write(&path, format!("{}\n{rest}", head.join(" "))).unwrap();
    let before = full_checks();
    assert_eq!(generation(dir.path()), id("gen-1"));
    assert_eq!(
        full_checks(),
        before + 1,
        "an older check's stamp was trusted"
    );
}

/// Links at every name of `.pixel-mvp/` the store writes, each to its own
/// file outside, as a clone commits them; this process's temporaries
/// (`checked.<pid>.<n>.probe`, `.tmp`, `.owner.<pid>.probe`) included,
/// whose pid a clone cannot know but can cover. The victims, by name.
#[cfg(unix)]
fn links_everywhere(root: &Path, outside: &Path) -> Vec<(std::path::PathBuf, String)> {
    let state = crate::dir(root);
    let pid = std::process::id();
    let mut names: Vec<String> = ["checked", "index.lock", ".gitignore", "log.jsonl"]
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    names.push(format!(".owner.{pid}.probe"));
    for n in 0..64 {
        names.push(format!("checked.{pid}.{n}.probe"));
        names.push(format!("checked.{pid}.{n}.tmp"));
    }
    let mut victims = Vec::new();
    for name in names {
        let at = state.join(&name);
        if let Ok(meta) = std::fs::symlink_metadata(&at) {
            assert!(!meta.is_dir());
            std::fs::remove_file(&at).unwrap();
        }
        let victim = outside.join(&name);
        let content = format!("the user's {name}\n");
        std::fs::write(&victim, &content).unwrap();
        std::os::unix::fs::symlink(&victim, &at).unwrap();
        victims.push((victim, content));
    }
    victims
}

// Round 4 of #60: the stamp, its temporary and the clock probe were
// written with `std::fs::write`, which follows a link a clone committed
// at their names and truncates the file it names (C-18).
#[cfg(unix)]
#[test]
fn c18_r4_the_stamp_and_its_temporaries_are_never_written_through_a_link() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    published(dir.path());
    checked::forget(dir.path());
    settle(dir.path());
    let victims = links_everywhere(dir.path(), outside.path());
    // A reader checks the file whole and stamps it: the probe and the
    // stamp's temporary are made where the links were.
    assert_eq!(generation(dir.path()), id("gen-1"));
    assert!(checked::trusted(dir.path()), "stamped despite the links");
    // A writer takes the lock, checks the file and stamps it too.
    let victims_after_reader = links_everywhere(dir.path(), outside.path());
    drop(Writer::open(dir.path(), None).expect("writer"));
    for (victim, content) in victims.iter().chain(&victims_after_reader) {
        assert_eq!(
            std::fs::read_to_string(victim).unwrap(),
            *content,
            "{} was written through its link",
            victim.display()
        );
    }
    // The link at the lock is no writer's: it went, and the lock is a file.
    let lock = crate::dir(dir.path()).join(crate::LOCK);
    assert!(std::fs::symlink_metadata(&lock).unwrap().is_file());
}

// Round 4 of #60: a FIFO at `checked` held every reader, which read it
// with `std::fs::read_to_string`; at `index.lock`, every writer (C-18,
// C-8).
#[cfg(unix)]
#[test]
fn c18_r4_a_fifo_at_the_stamp_or_the_lock_never_holds_a_reader_or_a_writer() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    let state = crate::dir(dir.path());
    for name in [checked::CHECKED, crate::LOCK] {
        let at = state.join(name);
        let _ = std::fs::remove_file(&at);
        let made = std::process::Command::new("mkfifo")
            .arg(&at)
            .status()
            .unwrap();
        assert!(made.success());
    }
    let root = dir.path().to_path_buf();
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let read = Store::open(&root)
            .map(|_| ())
            .map_err(|err| err.to_string());
        let written = Writer::open(&root, None)
            .map(|_| ())
            .map_err(|err| err.to_string());
        let _ = send.send((read, written));
    });
    let (read, written) = receive
        .recv_timeout(Duration::from_secs(10))
        .expect("held by a FIFO");
    assert_eq!(read, Ok(()));
    // The FIFO at the lock is no writer's lock: it went.
    assert_eq!(written, Ok(()));
    assert!(state.join(crate::LOCK).is_file());
}

// Round 4 of #60, n1: a checkout the writer cannot write in (read-only,
// or not the user's to write) is refused with what to do, as the other
// refusals of the state directory are, not a bare "Read-only file system
// (os error 30)" (C-18).
#[test]
fn c18_r4_a_state_directory_that_cannot_be_written_is_refused_with_what_to_do() {
    let root = Path::new("/checkout");
    for kind in [
        std::io::ErrorKind::ReadOnlyFilesystem,
        std::io::ErrorKind::PermissionDenied,
    ] {
        let err = crate::unwritable(root, std::io::Error::from(kind)).to_string();
        assert!(
            err.starts_with("/checkout/.pixel-mvp cannot be written (")
                && err.ends_with("make the checkout writable, then run `pixel-mvp index`"),
            "{err}"
        );
    }
    // Any other error is said as it is.
    let other = crate::unwritable(root, std::io::Error::other("disk on fire")).to_string();
    assert_eq!(other, "disk on fire");
}

#[cfg(unix)]
#[test]
fn c18_r4_a_checkout_the_user_cannot_write_in_gets_the_guidance_from_the_writer() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let dir = tempfile::tempdir().unwrap();
    let probe = dir.path().join("probe");
    std::fs::write(&probe, "").unwrap();
    if std::fs::metadata(&probe).unwrap().uid() == 0 {
        // Root writes in a directory of mode 0555: nothing to refuse.
        return;
    }
    std::fs::remove_file(&probe).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let err = Writer::open(dir.path(), None)
        .map(|_| ())
        .unwrap_err()
        .to_string();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(err.contains("make the checkout writable"), "{err}");
}

// The cached counts affect selection: a plausible wrong count must be
// rejected just like an unreadable graph row, not trusted until rebuild.
#[test]
fn c19_word_frequencies_are_checked_against_the_entire_generation() {
    for sql in [
        "UPDATE words SET files = 2 WHERE word = 'a'",
        "DELETE FROM words WHERE word = 'rs'",
        "INSERT INTO words VALUES ('phantom', 1)",
        "UPDATE words SET word = 'A' WHERE word = 'a'",
        "UPDATE words SET files = -1 WHERE word = 'a'",
        "UPDATE words SET files = 'wrong' WHERE word = 'a'",
        "UPDATE words SET word = CAST(x'ff' AS TEXT) WHERE word = 'a'",
    ] {
        let dir = tempfile::tempdir().unwrap();
        published(dir.path());
        damage_under_the_writer(dir.path(), sql);
        refused_then_rebuilt(dir.path(), sql);
    }
}

#[test]
fn c19_schema_five_is_rebuilt_before_reading_word_frequencies() {
    let dir = tempfile::tempdir().unwrap();
    published(dir.path());
    damage_under_the_writer(dir.path(), "DROP TABLE words; PRAGMA user_version = 5");
    refused_then_rebuilt(dir.path(), "schema 5 without words");
}

#[test]
fn c10_word_frequencies_follow_atomic_incremental_publication() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let old = file("old.rs", vec![symbol("old.rs", "SharedOld", 1)], vec![]);
    let kept = file(
        "shared.rs",
        vec![symbol("shared.rs", "SharedShared", 1)],
        vec![],
    );
    let added = file("new.rs", vec![symbol("new.rs", "SharedNew", 1)], vec![]);
    let mut writer = Writer::open(root, None).unwrap();
    writer
        .publish(&delta(
            "one",
            vec![old, kept.clone()],
            &[],
            &["old.rs", "shared.rs"],
        ))
        .unwrap();
    let reader = Store::open(root).unwrap();
    assert_eq!(reader.files_with_word("shared").unwrap(), 2);
    let mut next = delta(
        "two",
        vec![added.clone()],
        &["old.rs"],
        &["new.rs", "shared.rs"],
    );
    next.stale = vec!["shared.rs".to_owned()];
    writer.publish(&next).unwrap();
    // The already open reader stays on the old generation, even if its
    // first lookup of a word happens after publication.
    assert_eq!(reader.files_with_word("old").unwrap(), 1);
    assert_eq!(reader.files_with_word("new").unwrap(), 0);
    let current = Store::open(root).unwrap();
    assert_eq!(current.files_with_word("old").unwrap(), 0);
    assert_eq!(current.files_with_word("shared").unwrap(), 2);
    assert_eq!(current.stale(), &["shared.rs"]);
    let full = tempfile::tempdir().unwrap();
    let mut rebuilt = Writer::open(full.path(), None).unwrap();
    rebuilt
        .publish(&delta(
            "full",
            vec![added, kept],
            &[],
            &["new.rs", "shared.rs"],
        ))
        .unwrap();
    let full = Store::open(full.path()).unwrap();
    for word in ["old", "new", "shared", "rs", "absent"] {
        assert_eq!(
            current.files_with_word(word).unwrap(),
            full.files_with_word(word).unwrap(),
            "{word}"
        );
    }
    // A rejected delta cannot leave counts from a partially applied graph.
    assert!(
        writer
            .publish(&delta("bad", vec![], &["new.rs"], &["new.rs", "shared.rs"]))
            .is_err()
    );
    let after = Store::open(root).unwrap();
    assert_eq!(after.files_with_word("new").unwrap(), 1);
    assert_eq!(after.provenance().unwrap().generation, id("two"));
    writer
        .publish(&delta("empty", vec![], &["new.rs", "shared.rs"], &[]))
        .unwrap();
    let empty = Store::open(root).unwrap();
    assert_eq!(empty.files_with_word("shared").unwrap(), 0);
    assert_eq!(empty.files_with_word("rs").unwrap(), 0);
}

#[test]
fn audited_git_index_should_publish_only_a_refreshed_still_current_copy() {
    for (refresh, holds, expected) in [
        (true, true, true),
        (false, true, false),
        (true, false, false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let base = crate::dir(root.path());
        std::fs::create_dir(&base).unwrap();
        std::fs::write(base.join("current"), b"before").unwrap();
        std::fs::write(base.join("obsolete"), b"old").unwrap();
        std::fs::write(base.join("user"), b"mine").unwrap();
        let placed = crate::place_git_index(
            root.path(),
            "current",
            b"new",
            |temp| {
                assert_eq!(std::fs::read(temp).unwrap(), b"new");
                refresh
            },
            || holds,
            |name| name == "current" || name == "obsolete",
        )
        .unwrap();
        assert_eq!(placed, expected);
        assert_eq!(
            std::fs::read(base.join("current")).unwrap(),
            if expected {
                &b"new"[..]
            } else {
                &b"before"[..]
            }
        );
        assert_eq!(std::fs::read(base.join("user")).unwrap(), b"mine");
        assert!(!base.join("obsolete").exists());
        let temp = base.join(format!("current.{}.tmp", std::process::id()));
        assert!(!temp.exists());
        assert!(
            !base
                .join(format!("current.{}.tmp.lock", std::process::id()))
                .exists()
        );
    }
}

#[test]
fn audited_git_index_should_not_create_a_missing_state_directory() {
    let root = tempfile::tempdir().unwrap();
    assert!(
        !crate::place_git_index(
            root.path(),
            "index",
            b"bytes",
            |_| panic!("no owned state"),
            || panic!("not refreshed"),
            |_| false
        )
        .unwrap()
    );
    assert!(!crate::dir(root.path()).exists());
}

#[test]
fn audited_callers_should_return_the_resolved_site_and_exclude_unresolved_sites() {
    let root = tempfile::tempdir().unwrap();
    published(root.path());
    let store = Store::open(root.path()).unwrap();
    let a = symbol("a.rs", "a", 1);
    let b = symbol("b.rs", "b", 1);
    assert_eq!(
        store.callers(&b.uid).unwrap(),
        vec![call(&a, Some(&b), "b")]
    );
    assert_eq!(store.callers("missing").unwrap(), Vec::<Edge>::new());
}
