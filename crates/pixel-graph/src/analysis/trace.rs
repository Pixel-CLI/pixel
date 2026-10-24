// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The shortest call path from one symbol to another, over the resolved
//! `Call` edges of the generation: how `main` reaches `parse`.

use std::collections::{HashMap, VecDeque};

use pixel_model::{EdgeKind, Symbol, Tier};

use crate::store::{Store, StoreError};

/// The longest path searched, in calls.
pub const MAX_TRACE_DEPTH: usize = 8;

/// One call on a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hop {
    /// The callee this call reaches.
    pub symbol: Symbol,
    /// The line of the call, in the caller's file.
    pub line: u32,
    pub tier: Tier,
}

/// The path from `from` to `to` (`from` is not in it), or `None` when no
/// chain of resolved calls up to `max_depth` long joins them. A path is only
/// as sure as its weakest hop: read the tiers.
pub fn trace(
    store: &Store,
    from: &Symbol,
    to: &Symbol,
    max_depth: usize,
) -> Result<Option<Vec<Hop>>, StoreError> {
    let max_depth = max_depth.min(MAX_TRACE_DEPTH);
    // `reached[uid]` is the hop that first reached it and the uid before it.
    let mut reached: HashMap<String, (Hop, String)> = HashMap::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::from([(from.uid.clone(), 0)]);
    while let Some((uid, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        for edge in store.callees(&uid)? {
            let Some(dst) = edge.dst.as_deref() else {
                continue;
            };
            if edge.kind != EdgeKind::Call
                || edge.tier == Tier::Open
                || dst == from.uid
                || reached.contains_key(dst)
            {
                continue;
            }
            let Some(symbol) = store.symbol(dst)? else {
                continue;
            };
            let hop = Hop {
                symbol,
                line: edge.line,
                tier: edge.tier,
            };
            reached.insert(dst.to_owned(), (hop, uid.clone()));
            if dst == to.uid {
                return Ok(Some(path(&reached, &to.uid, &from.uid)));
            }
            queue.push_back((dst.to_owned(), depth + 1));
        }
    }
    Ok(None)
}

/// The hops from `start` to `end`, read back through `reached`.
fn path(reached: &HashMap<String, (Hop, String)>, end: &str, start: &str) -> Vec<Hop> {
    let mut hops = Vec::new();
    let mut at = end.to_owned();
    while at != start {
        let Some((hop, before)) = reached.get(&at) else {
            break;
        };
        hops.push(hop.clone());
        at = before.clone();
    }
    hops.reverse();
    hops
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::fixture::project;
    use crate::analysis::impact::resolve_target;

    fn symbol(store: &Store, name: &str) -> Symbol {
        resolve_target(store, name).unwrap().remove(0)
    }

    #[test]
    fn a_path_names_each_call_and_its_line() {
        let (_repo, store) = project();
        let hops = trace(&store, &symbol(&store, "t"), &symbol(&store, "run"), 4)
            .unwrap()
            .unwrap();
        let shown: Vec<(&str, u32)> = hops
            .iter()
            .map(|h| (h.symbol.name.as_str(), h.line))
            .collect();
        assert_eq!(shown, [("go", 4), ("run", 5)]);
        assert!(hops.iter().all(|h| h.tier == Tier::Proved));
    }

    #[test]
    fn no_path_the_other_way_and_none_past_the_depth() {
        let (_repo, store) = project();
        let (run, t) = (symbol(&store, "run"), symbol(&store, "t"));
        assert_eq!(trace(&store, &run, &t, 4).unwrap(), None);
        assert_eq!(trace(&store, &t, &run, 1).unwrap(), None);
        assert!(trace(&store, &t, &run, 2).unwrap().is_some());
        assert_eq!(
            trace(&store, &t, &t, 4).unwrap(),
            None,
            "a symbol is not a hop from itself"
        );
    }
}
