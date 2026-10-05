// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the vector store: a store never mixes models, a
//! damaged segment is refused instead of read, and KNN returns the best
//! scores in order within the allowed set.

use super::*;
use std::collections::HashSet;

fn store() -> (tempfile::TempDir, VectorStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = VectorStore::open(dir.path()).unwrap();
    (dir, store)
}

fn ids(hits: &[(i64, f32)]) -> Vec<i64> {
    hits.iter().map(|(id, _)| *id).collect()
}

/// An empty store accepts any model; a bound store refuses another model
/// or dimension, naming both and the rebuild command.
#[test]
fn check_model_should_refuse_another_model_or_dimension_once_bound() {
    let (_dir, mut store) = store();
    assert_eq!(store.check_model("anything", 7), Ok(()));
    store
        .append_segment("m1", 2, &[(1, vec![1.0, 0.0])])
        .unwrap();
    assert_eq!(store.check_model("m1", 2), Ok(()));
    assert_eq!(
        store.check_model("m1", 3),
        Err("vector store was built with model 'm1' (2d) but the active model is 'm1' (3d) — run `pixel recall embed --rebuild`".to_string())
    );
    assert!(store.check_model("m2", 2).is_err());
    assert_eq!(
        store.append_segment("m2", 2, &[(2, vec![0.0, 1.0])]),
        Err("vector store was built with model 'm1' (2d) but the active model is 'm2' (2d) — run `pixel recall embed --rebuild`".to_string()),
        "appending another model's vectors is refused"
    );
    assert_eq!(store.meta.segments.len(), 1);
}

/// Appending nothing writes no segment; a row of the wrong dimension is an
/// error and leaves nothing behind: no segment, no `.tmp` file, and no
/// chunk id raised by the valid rows before it (#786).
#[test]
fn append_segment_should_write_nothing_for_empty_or_mismatched_rows() {
    let (dir, mut store) = store();
    store.append_segment("m", 2, &[]).unwrap();
    assert!(store.meta.segments.is_empty());
    assert_eq!(
        store.append_segment("m", 2, &[(7, vec![1.0, 0.0]), (8, vec![1.0, 0.0, 0.0])]),
        Err("vector dim mismatch".to_string())
    );
    assert!(store.meta.segments.is_empty());
    assert_eq!(store.meta.last_chunk_id, 0);
    let left: Vec<String> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(left.is_empty(), "no segment and no .tmp remain: {left:?}");
}

/// The store reopens with its model, segments and highest chunk id.
#[test]
fn open_should_restore_the_store_written_before() {
    let (dir, mut store) = store();
    store
        .append_segment("m", 2, &[(5, vec![1.0, 0.0]), (9, vec![0.0, 1.0])])
        .unwrap();
    store
        .append_segment("m", 2, &[(7, vec![1.0, 1.0])])
        .unwrap();
    let reopened = VectorStore::open(dir.path()).unwrap();
    assert_eq!(reopened.meta.model_id, "m");
    assert_eq!(reopened.meta.dim, 2);
    assert_eq!(
        reopened.meta.segments,
        vec!["seg-000001.vec", "seg-000002.vec"]
    );
    assert_eq!(reopened.meta.last_chunk_id, 9);
    assert_eq!(ids(&reopened.knn(&[1.0, 0.0], 3, None)), vec![5, 7, 9]);
}

/// A corrupt meta file is an error, not an empty store to overwrite.
#[test]
fn open_should_fail_on_a_corrupt_meta_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("meta.json"), b"{ nope").unwrap();
    assert!(
        VectorStore::open(dir.path())
            .err()
            .unwrap()
            .starts_with("corrupt vector meta:")
    );
}

/// Clearing removes every segment and unbinds the model.
#[test]
fn clear_should_remove_segments_and_unbind_the_model() {
    let (dir, mut store) = store();
    store
        .append_segment("m", 2, &[(1, vec![1.0, 0.0])])
        .unwrap();
    store.clear().unwrap();
    assert!(!dir.path().join("seg-000001.vec").exists());
    assert_eq!(store.meta.model_id, "");
    assert_eq!(store.check_model("other", 5), Ok(()));
    assert!(store.knn(&[1.0, 0.0], 3, None).is_empty());
}

/// KNN keeps the k best scores, best first, and only within the allowed
/// set; a query of the wrong dimension matches nothing.
#[test]
fn knn_should_return_the_best_k_in_order_within_the_allowed_set() {
    let (_dir, mut store) = store();
    store
        .append_segment(
            "m",
            2,
            &[
                (1, vec![0.1, 1.0]),
                (2, vec![1.0, 0.0]),
                (3, vec![0.5, 0.5]),
                (4, vec![0.9, 0.1]),
                (5, vec![0.0, 1.0]),
            ],
        )
        .unwrap();
    assert_eq!(ids(&store.knn(&[1.0, 0.0], 3, None)), vec![2, 4, 3]);
    assert_eq!(ids(&store.knn(&[1.0, 0.0], 10, None)), vec![2, 4, 3, 1, 5]);
    let allowed: HashSet<i64> = [1, 3, 5].into_iter().collect();
    assert_eq!(ids(&store.knn(&[1.0, 0.0], 2, Some(&allowed))), vec![3, 1]);
    assert!(store.knn(&[1.0, 0.0, 0.0], 3, None).is_empty());
}

/// Rows arriving in ascending score order keep evicting the weakest kept
/// hit, so the k best survive whatever order they were written in.
#[test]
fn knn_should_keep_the_best_k_when_rows_arrive_in_ascending_order() {
    let (_dir, mut store) = store();
    store
        .append_segment(
            "m",
            2,
            &[
                (1, vec![0.1, 1.0]),
                (2, vec![0.5, 0.5]),
                (3, vec![0.9, 0.1]),
                (4, vec![1.0, 0.0]),
            ],
        )
        .unwrap();
    assert_eq!(ids(&store.knn(&[1.0, 0.0], 2, None)), vec![4, 3]);
}

/// A zero vector is stored without dividing by zero and scores zero.
#[test]
fn append_segment_should_store_a_zero_vector_with_a_zero_score() {
    let (_dir, mut store) = store();
    store
        .append_segment("m", 2, &[(1, vec![0.0, 0.0])])
        .unwrap();
    assert_eq!(store.knn(&[1.0, 0.0], 1, None), vec![(1, 0.0)]);
}

fn segment_bytes(store: &VectorStore, dir: &Path) -> (PathBuf, Vec<u8>) {
    let path = dir.join(&store.meta.segments[0]);
    let bytes = std::fs::read(&path).unwrap();
    (path, bytes)
}

/// A segment with a wrong magic, an unknown version or fewer rows than its
/// header claims is refused, and KNN skips it instead of reading it.
#[test]
fn open_should_refuse_damaged_segments_and_knn_should_skip_them() {
    let (dir, mut store) = store();
    store
        .append_segment("m", 2, &[(1, vec![1.0, 0.0])])
        .unwrap();
    let (path, good) = segment_bytes(&store, dir.path());
    assert!(OpenSegment::open(&path).is_ok());

    let mut bad_magic = good.clone();
    bad_magic[0] = b'X';
    std::fs::write(&path, &bad_magic).unwrap();
    assert_eq!(
        OpenSegment::open(&path).err(),
        Some("bad vector segment header".to_string())
    );
    assert!(store.knn(&[1.0, 0.0], 1, None).is_empty());

    let mut v2 = good.clone();
    v2[8..12].copy_from_slice(&2u32.to_le_bytes());
    std::fs::write(&path, &v2).unwrap();
    assert_eq!(
        OpenSegment::open(&path).err(),
        Some("vector segment version 2".to_string())
    );
    let mut v0 = good.clone();
    v0[8..12].copy_from_slice(&0u32.to_le_bytes());
    std::fs::write(&path, &v0).unwrap();
    assert_eq!(
        OpenSegment::open(&path).err(),
        Some("vector segment version 0".to_string()),
        "an older version is refused too"
    );

    std::fs::write(&path, &good[..good.len() - 1]).unwrap();
    assert_eq!(
        OpenSegment::open(&path).err(),
        Some("truncated vector segment".to_string())
    );

    std::fs::write(&path, &good[..10]).unwrap();
    assert_eq!(
        OpenSegment::open(&path).err(),
        Some("bad vector segment header".to_string())
    );
}
