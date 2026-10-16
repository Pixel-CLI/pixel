// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! MVP C-10/C-11/C-19 through Pixel's production refresh adapter: edits
//! must not leave obsolete edges, and damaged indexes must be rebuilt.

use std::fs;
use std::path::Path;

use mvp_model::Graph;
use pixel_graph::mvp_adapter::refresh;

use super::mvp_adapter::{fixture, git};

/// Compare logical rows, including unresolved sites, without timestamps or
/// SQLite insertion order (which legitimately differ after an incremental run).
fn rows(store: &mvp_store::Store) -> Vec<String> {
    let mut out = Vec::new();
    for file in store.files().unwrap() {
        out.push(format!(
            "file {} {:?} {:?}",
            file.path, file.lang, file.hash
        ));
        for symbol in store.symbols(&file.path).unwrap() {
            out.push(format!("symbol {symbol:?}"));
        }
    }
    for edge in store.all_edges().unwrap() {
        out.push(format!("edge {edge:?}"));
    }
    out.sort();
    out
}

fn root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fixture(&dir.path().canonicalize().unwrap());
    dir
}

#[test]
fn refresh_should_match_a_clean_build_after_targets_move_or_disappear() {
    let dir = root();
    let root = dir.path().canonicalize().unwrap();
    fs::create_dir_all(root.join("app/models")).unwrap();
    fs::write(
        root.join("app/models/user.rb"),
        "class User\n  def run\n    name\n  end\n  def name; end\nend\n",
    )
    .unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "Ruby fixture"]);
    let state = root.join(".pixel");
    let before = refresh(&root, &state).unwrap();
    let before_rows = rows(&before);
    let before_generation = before.provenance().unwrap().generation;
    let removed_targets: Vec<_> = [("helper", "src/util.rs"), ("name", "app/models/user.rb")]
        .into_iter()
        .map(|(name, file)| {
            let matches: Vec<_> = before
                .named(name)
                .unwrap()
                .into_iter()
                .filter(|symbol| symbol.file == file)
                .collect();
            assert_eq!(matches.len(), 1, "fixture definition {file}:{name}");
            let uid = matches.into_iter().next().unwrap().uid;
            assert!(
                before
                    .all_edges()
                    .unwrap()
                    .iter()
                    .any(|edge| edge.dst.as_ref() == Some(&uid)),
                "fixture must actually call {uid} before it is removed"
            );
            uid
        })
        .collect();
    drop(before);

    fs::write(root.join("src/util.rs"), "pub fn renamed() {}\n").unwrap();
    fs::remove_file(root.join("src/paths.rs")).unwrap();
    fs::write(root.join("src/added.rs"), "pub fn added() {}\n").unwrap();
    fs::write(
        root.join("app/models/user.rb"),
        "class User\n  def run\n    name\n  end\n  def renamed; end\nend\n",
    )
    .unwrap();
    let incremental = refresh(&root, &state).unwrap();
    let after_rows = rows(&incremental);
    let provenance = incremental.provenance().unwrap();
    assert_ne!(
        after_rows, before_rows,
        "an ignored edit must fail this check"
    );
    assert_ne!(provenance.generation, before_generation);
    assert!(provenance.dirty);
    assert!(incremental.symbols("src/paths.rs").unwrap().is_empty());
    assert!(
        incremental.all_edges().unwrap().iter().all(|edge| {
            edge.dst
                .as_ref()
                .is_none_or(|uid| !removed_targets.contains(uid))
        }),
        "removed Rust and Ruby targets must not survive an update"
    );
    drop(incremental);

    fs::remove_dir_all(mvp_store::dir(&state)).unwrap();
    let clean = refresh(&root, &state).unwrap();
    assert_eq!(after_rows, rows(&clean));
    assert_eq!(
        provenance.generation,
        clean.provenance().unwrap().generation
    );
}

fn damage(state: &Path, sql: Option<&str>) {
    let db = mvp_store::db_path(state);
    if let Some(sql) = sql {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.execute_batch(sql).unwrap();
    } else {
        fs::write(db, b"not a SQLite database").unwrap();
    }
}

#[test]
fn refresh_should_rebuild_corruption_without_serving_old_or_missing_rows() {
    for corruption in [
        None,
        Some("DELETE FROM meta WHERE key = 'generation'"),
        Some("UPDATE files SET hash = 'not-a-hash'"),
        Some("DELETE FROM symbols WHERE name LIKE '%helper%'"),
        Some("CREATE TABLE foreign_table (value TEXT)"),
    ] {
        let dir = root();
        let root = dir.path().canonicalize().unwrap();
        let state = root.join(".pixel");
        let first = refresh(&root, &state).unwrap();
        let expected = rows(&first);
        let generation = first.provenance().unwrap().generation;
        assert!(
            first
                .all_edges()
                .unwrap()
                .iter()
                .any(|edge| edge.dst.is_some())
        );
        drop(first);
        damage(&state, corruption);
        assert!(
            mvp_store::Store::open(&state).is_err(),
            "served corruption: {corruption:?}"
        );
        let rebuilt = refresh(&root, &state).unwrap();
        assert_eq!(rows(&rebuilt), expected, "{corruption:?}");
        assert_eq!(rebuilt.provenance().unwrap().generation, generation);
        drop(rebuilt);
        assert_eq!(rows(&refresh(&root, &state).unwrap()), expected);
    }
}

#[test]
fn refresh_should_preserve_commit_provenance_and_git_index_bytes() {
    let dir = root();
    let root = dir.path().canonicalize().unwrap();
    let index = fs::read(root.join(".git/index")).unwrap();
    let head = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(head.status.success());
    let state = root.join(".pixel");
    let store = refresh(&root, &state).unwrap();
    let provenance = store.provenance().unwrap();
    assert_eq!(
        provenance.commit.as_deref(),
        Some(String::from_utf8(head.stdout).unwrap().trim())
    );
    assert!(!provenance.dirty);
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
    drop(store);
    fs::write(root.join("src/util.rs"), "pub fn changed() {}\n").unwrap();
    let edited = refresh(&root, &state).unwrap().provenance().unwrap();
    assert_eq!(edited.commit, provenance.commit);
    assert!(edited.dirty);
    assert_ne!(edited.generation, provenance.generation);
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
}
