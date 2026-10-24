// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Which side of the daemon may answer an op (`specs/ops.md`).
//!
//! The writer owns the only writable [`Service`](crate::Service): it applies
//! watcher batches, refreshes the graph, ingests history and runs git. A read
//! replica shares the published text index and opens the graph read-only, so
//! it may answer only what a published generation already holds. [`plane`] is
//! the one place that says which is which.

use pixel_proto::Op;

/// The plane an op is served on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plane {
    /// A replica may answer from the published index and graph: no write, no
    /// build, no history ingest. The writer may answer it too.
    Read,
    /// Only the writer: the op may refresh the graph, ingest history, write
    /// session state under `.pixel/` or run git.
    Write,
    /// Only the writer, and the op rebuilds the whole index: the cached
    /// HEAD, branch and dirty state are stale the moment it returns.
    RepoWrite,
}

impl Plane {
    /// The cached git state must be dropped after this op succeeds.
    pub(crate) fn mutates_repo(self) -> bool {
        self == Plane::RepoWrite
    }
}

/// The plane `op` is served on. No wildcard arm: a new [`Op`] variant does
/// not compile until it is classified here.
pub(crate) fn plane(op: &Op) -> Plane {
    match op {
        Op::Ping
        | Op::Search { .. }
        | Op::Symbol { .. }
        | Op::Callers { .. }
        | Op::Impact { .. }
        | Op::Changes { .. }
        | Op::Meaning { .. }
        | Op::Map { .. }
        // A brief reads the stores like `stale_ok` and writes only the
        // session's sent ranges, a file of its own: it never holds the writer.
        | Op::Brief { .. } => Plane::Read,
        // `history` answers a read-only query from the db as it stands;
        // otherwise it runs a bounded lazy ingest first.
        Op::History {
            read_only: true, ..
        } => Plane::Read,
        // The hooks write the session's state files; `status` probes the
        // stores; `shutdown` must stay ordered behind every write.
        Op::History { .. }
        | Op::Status {}
        | Op::EditCheck { .. }
        | Op::StopCheck { .. }
        | Op::Shutdown => Plane::Write,
        Op::Reindex {} => Plane::RepoWrite,
    }
}

/// Whether a replica implements `op` (a replica may still give the request
/// back when it has nothing published to read).
pub(crate) fn replica_may_answer(op: &Op) -> bool {
    matches!(
        op,
        Op::Search { .. }
            | Op::Symbol { .. }
            | Op::Callers { .. }
            | Op::Impact { .. }
            | Op::Changes { .. }
            | Op::Map { .. }
            | Op::Brief { .. }
            | Op::History {
                read_only: true,
                ..
            }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One sample of every op, from the wire form: a variant added to
    /// [`Op`] without a sample here fails `every_op_is_sampled`.
    fn samples() -> Vec<Op> {
        Op::NAMES
            .iter()
            .map(|name| {
                let json = match *name {
                    "brief" => {
                        r#"{"op":"brief","prompt":"p","session":"s","host":"claude"}"#.to_owned()
                    }
                    "edit_check" => r#"{"op":"edit_check","path":"a.rs","session":"s"}"#.to_owned(),
                    "stop_check" => r#"{"op":"stop_check","session":"s"}"#.to_owned(),
                    "search" => r#"{"op":"search","pattern":"x"}"#.to_owned(),
                    "symbol" => r#"{"op":"symbol","name":"n"}"#.to_owned(),
                    "callers" => {
                        r#"{"op":"callers","uid_or_name":"n","role":"callers"}"#.to_owned()
                    }
                    "impact" => {
                        r#"{"op":"impact","uid_or_name":"n","direction":"upstream"}"#.to_owned()
                    }
                    "history" => r#"{"op":"history","query":"q"}"#.to_owned(),
                    "meaning" => r#"{"op":"meaning","query":"q"}"#.to_owned(),
                    other => format!(r#"{{"op":"{other}"}}"#),
                };
                serde_json::from_str(&json).unwrap_or_else(|err| panic!("{name}: {err}"))
            })
            .collect()
    }

    #[test]
    fn every_op_is_sampled() {
        assert_eq!(samples().len(), Op::NAMES.len());
    }

    #[test]
    fn reads_are_what_a_replica_may_answer_or_hand_back() {
        let planes: Vec<(&str, Plane)> = samples()
            .iter()
            .map(|op| (op.op_name(), plane(op)))
            .collect();
        for (name, plane) in &planes {
            let expected = match *name {
                "ping" | "search" | "symbol" | "callers" | "impact" | "changes" | "meaning"
                | "map" | "brief" => Plane::Read,
                "reindex" => Plane::RepoWrite,
                _ => Plane::Write,
            };
            assert_eq!(*plane, expected, "{name}");
        }
        for op in samples() {
            if replica_may_answer(&op) {
                assert_eq!(plane(&op), Plane::Read, "{}", op.op_name());
            }
        }
    }

    #[test]
    fn a_history_query_is_a_read_only_when_it_asks_not_to_ingest() {
        let ask = |read_only: bool| Op::History {
            query: "q".to_owned(),
            facet: None,
            limit: None,
            read_only,
        };
        assert_eq!(plane(&ask(true)), Plane::Read);
        assert_eq!(plane(&ask(false)), Plane::Write);
        assert!(replica_may_answer(&ask(true)));
        assert!(!replica_may_answer(&ask(false)));
    }

    #[test]
    fn only_a_reindex_mutates_the_repo_state() {
        assert!(Plane::RepoWrite.mutates_repo());
        assert!(!Plane::Write.mutates_repo());
        assert!(!Plane::Read.mutates_repo());
    }
}
