// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

// Portions derived from marjoballabani/hypergrep (MIT) — see NOTICE.
//! Posting-list intersection/union and boolean gram-query resolution.
//!
//! A posting list is a sorted array of file IDs (u32) associated with a gram
//! hash. `resolve_query` evaluates the boolean plan produced by the query
//! planner against a lookup function.

/// Boolean candidate-selection plan over gram hashes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GramQuery {
    Literal(u64),
    And(Vec<GramQuery>),
    Or(Vec<GramQuery>),
    /// No narrowing possible — every document is a candidate.
    All,
}

/// Intersect two sorted slices using galloping (exponential search).
pub fn intersect_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };

    let mut result = Vec::with_capacity(short.len());
    let mut long_idx = 0;
    for &val in short {
        long_idx = gallop(long, long_idx, val);
        if long_idx >= long.len() {
            break;
        }
        if long[long_idx] == val {
            result.push(val);
        }
    }
    result
}

/// Union two sorted slices into a sorted, deduplicated vec.
pub fn union_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut result = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                result.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                result.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                result.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    result.extend_from_slice(&a[i..]);
    result.extend_from_slice(&b[j..]);
    result
}

/// First index in `list[start..]` with `list[idx] >= target`.
fn gallop(list: &[u32], start: usize, target: u32) -> usize {
    if start >= list.len() {
        return list.len();
    }
    let mut bound = 1;
    while start + bound < list.len() && list[start + bound] < target {
        bound *= 2;
    }
    let lo = start + bound / 2;
    let hi = (start + bound).min(list.len() - 1);
    match list[lo..=hi].binary_search(&target) {
        Ok(pos) | Err(pos) => lo + pos,
    }
}

/// Resolve a `GramQuery` to sorted candidate file IDs. The lookup returns an
/// owned list because shard postings are varint-decoded on demand.
pub fn resolve_query<F>(query: &GramQuery, total_docs: u32, lookup: &F) -> Vec<u32>
where
    F: Fn(u64) -> Vec<u32>,
{
    match query {
        GramQuery::Literal(h) => lookup(*h),
        GramQuery::And(children) => {
            if children.is_empty() {
                return all_docs(total_docs);
            }
            let mut lists: Vec<Vec<u32>> = children
                .iter()
                .map(|c| resolve_query(c, total_docs, lookup))
                .collect();
            lists.sort_by_key(Vec::len);
            let mut result = lists.swap_remove(0);
            for list in &lists {
                if result.is_empty() {
                    break;
                }
                result = intersect_sorted(&result, list);
            }
            result
        }
        GramQuery::Or(children) => {
            let mut result: Vec<u32> = Vec::new();
            for child in children {
                result = union_sorted(&result, &resolve_query(child, total_docs, lookup));
            }
            result
        }
        GramQuery::All => all_docs(total_docs),
    }
}

fn all_docs(total: u32) -> Vec<u32> {
    (0..total).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    #[test]
    fn intersect_basic() {
        assert_eq!(intersect_sorted(&[1, 3, 5, 7], &[2, 3, 5, 8]), vec![3, 5]);
        assert!(intersect_sorted(&[], &[1]).is_empty());
        assert!(intersect_sorted(&[1, 3, 5], &[2, 4, 6]).is_empty());
        assert_eq!(intersect_sorted(&[1, 2, 3], &[1, 2, 3]), vec![1, 2, 3]);
    }

    #[test]
    fn union_basic() {
        assert_eq!(union_sorted(&[1, 3, 5], &[2, 3, 6]), vec![1, 2, 3, 5, 6]);
    }

    #[test]
    fn resolve_and_or() {
        let lists: Vec<(u64, Vec<u32>)> =
            vec![(1, vec![0, 1, 2]), (2, vec![1, 2, 3]), (3, vec![3, 4])];
        let lookup = |h: u64| -> Vec<u32> {
            lists
                .iter()
                .find(|(k, _)| *k == h)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        let q = GramQuery::And(vec![GramQuery::Literal(1), GramQuery::Literal(2)]);
        assert_eq!(resolve_query(&q, 5, &lookup), vec![1, 2]);
        let q = GramQuery::Or(vec![GramQuery::Literal(2), GramQuery::Literal(3)]);
        assert_eq!(resolve_query(&q, 5, &lookup), vec![1, 2, 3, 4]);
        assert_eq!(resolve_query(&GramQuery::All, 3, &lookup), vec![0, 1, 2]);
    }

    proptest! {
        #[test]
        fn prop_intersect_union_match_sets(
            a in proptest::collection::btree_set(any::<u32>(), 0..100),
            b in proptest::collection::btree_set(any::<u32>(), 0..100),
        ) {
            let av: Vec<u32> = a.iter().copied().collect();
            let bv: Vec<u32> = b.iter().copied().collect();
            let expect_and: Vec<u32> = a.intersection(&b).copied().collect();
            let expect_or: Vec<u32> = a.union(&b).copied().collect::<BTreeSet<_>>().into_iter().collect();
            prop_assert_eq!(intersect_sorted(&av, &bv), expect_and);
            prop_assert_eq!(union_sorted(&av, &bv), expect_or);
        }
    }
}
