// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use std::os::unix::fs::{PermissionsExt, symlink};

use super::*;

fn root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".pixel")).unwrap();
    dir
}

fn sample() -> Session {
    let mut session = Session::default();
    session.baselines.insert(
        "src/a.rs".to_owned(),
        vec![Base {
            uid: "src/a.rs#run#function".to_owned(),
            name: "run".to_owned(),
            kind: "function".to_owned(),
            shape: "pub fn run(x: u32) -> u32".to_owned(),
        }],
    );
    session
        .carded
        .insert(card_key("src/a.rs#run#function", "reshaped:x"));
    session.remember_candidates(
        "src/a.rs#run#function",
        vec![Candidate {
            file: "src/b.rs".to_owned(),
            line: 4,
            holder: "go".to_owned(),
        }],
    );
    session.pending.push(Pending {
        uid: "src/a.rs#run#function".to_owned(),
        name: "run".to_owned(),
        file: "src/a.rs".to_owned(),
        old: "pub fn run(x: u32) -> u32".to_owned(),
        new: None,
        sites: vec![PendingSite {
            file: "src/b.rs".to_owned(),
            line: 4,
            holder: "go".to_owned(),
            hash: content_hash(b"b"),
        }],
    });
    session.blocked.insert("abc".to_owned());
    session.fixed.insert(Session::fixed_key("u", "f.rs"));
    session
}

#[test]
fn a_saved_session_should_load_back_equal() {
    let dir = root();
    let session = sample();
    save(dir.path(), "host-session", &session).unwrap();
    assert_eq!(load(dir.path(), "host-session"), session);
    assert_eq!(load(dir.path(), "another"), Session::default());
}

#[test]
fn the_file_should_be_named_by_the_hashed_session_and_owner_only() {
    let dir = root();
    save(dir.path(), "host-session", &sample()).unwrap();
    let path = file_for(dir.path(), "host-session");
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(name.len(), 16 + ".json".len(), "{name}");
    assert!(!name.contains("host-session"));
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(dir.path().join(".pixel/sessions"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[test]
fn save_should_not_create_the_pixel_directory() {
    let dir = tempfile::tempdir().unwrap();
    assert!(save(dir.path(), "s", &sample()).is_err());
    assert!(!dir.path().join(".pixel").exists());
}

#[test]
fn a_linked_sessions_directory_should_be_refused_for_reads_and_writes() {
    let dir = root();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), dir.path().join(".pixel/sessions")).unwrap();
    assert!(save(dir.path(), "s", &sample()).is_err());
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    assert_eq!(load(dir.path(), "s"), Session::default());
}

#[test]
fn a_file_that_is_not_json_should_be_an_empty_session() {
    let dir = root();
    save(dir.path(), "s", &sample()).unwrap();
    fs::write(file_for(dir.path(), "s"), "not json").unwrap();
    assert_eq!(load(dir.path(), "s"), Session::default());
    fs::write(
        file_for(dir.path(), "s"),
        r#"{"pending": [{"uid": 3}], "carded": [1, "k"]}"#,
    )
    .unwrap();
    let loaded = load(dir.path(), "s");
    assert!(loaded.pending.is_empty());
    assert_eq!(loaded.carded.len(), 1);
}

#[test]
fn reset_should_forget_the_session() {
    let dir = root();
    save(dir.path(), "s", &sample()).unwrap();
    reset(dir.path(), "s");
    assert_eq!(load(dir.path(), "s"), Session::default());
    reset(dir.path(), "never-saved");
}

#[test]
fn prune_should_drop_only_the_files_older_than_the_ttl() {
    let dir = root();
    save(dir.path(), "old", &sample()).unwrap();
    save(dir.path(), "new", &sample()).unwrap();
    let sessions = dir.path().join(".pixel/sessions");
    let past = SystemTime::now() - TTL - Duration::from_secs(60);
    fs::File::options()
        .write(true)
        .open(file_for(dir.path(), "old"))
        .unwrap()
        .set_modified(past)
        .unwrap();
    prune(&sessions, SystemTime::now(), TTL);
    assert!(!file_for(dir.path(), "old").exists());
    assert!(file_for(dir.path(), "new").exists());
}

#[test]
fn the_pending_hash_should_ignore_file_hashes_and_order_and_move_with_the_sites() {
    let mut a = sample();
    let mut b = sample();
    b.pending[0].sites[0].hash = content_hash(b"other");
    assert_eq!(a.pending_hash(), b.pending_hash());
    a.pending[0].sites.push(PendingSite {
        file: "src/c.rs".to_owned(),
        line: 9,
        holder: "x".to_owned(),
        hash: String::new(),
    });
    assert_ne!(a.pending_hash(), b.pending_hash());
    assert_eq!(a.pending_hash().len(), 16);
}

#[test]
fn the_baselines_and_candidates_should_stay_bounded() {
    let mut session = Session::default();
    for n in 0..MAX_BASELINES {
        session.baselines.insert(format!("f{n}.rs"), Vec::new());
    }
    assert!(!session.has_room_for("new.rs"));
    assert!(session.has_room_for("f0.rs"));
    for n in 0..200 {
        session.remember_candidates(&format!("u{n}"), Vec::new());
    }
    assert_eq!(session.candidates.len(), 64);
    session.remember_candidates(
        "u0",
        vec![Candidate {
            file: "f".into(),
            line: 1,
            holder: "h".into(),
        }],
    );
    assert_eq!(session.candidates["u0"].len(), 1);
}

#[test]
fn a_card_key_should_differ_by_symbol_and_by_change() {
    assert_ne!(card_key("u", "a"), card_key("u", "b"));
    assert_ne!(card_key("u", "a"), card_key("v", "a"));
    assert_eq!(card_key("u", "a"), card_key("u", "a"));
}
