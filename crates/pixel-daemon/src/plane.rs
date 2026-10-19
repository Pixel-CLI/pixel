// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Which side of the daemon may answer a request (#1087).
//!
//! The writer owns the only writable `Service`: it applies watcher batches,
//! builds and updates `graph.v2.db`, ingests `history.db` and runs git. A
//! read replica ([`crate::api::Service::read_replica`]) shares the published
//! text index and opens the graph read-only, so it may answer only what a
//! published generation already holds. [`plane`] is the one place that
//! says which is which; the evidence bridge's allowlist and the git-state
//! invalidation after a mutation both derive from it.

use crate::api::Request;

/// The plane a request is served on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plane {
    /// A read replica may answer it from the published index, graph and
    /// vectors: no write, no build, no history ingest, no git subprocess
    /// beyond what a replica allows. The writer may answer it too.
    Read,
    /// Only the writer: the op may build or update the graph, ingest
    /// history, download a model, write under `.pixel/` or run git.
    Write,
    /// Only the writer, and the op moves HEAD, a branch, the working tree or
    /// the whole index: the cached HEAD/branch/dirty triple is stale the
    /// moment it returns.
    RepoWrite,
}

impl Plane {
    /// The cached git state must be dropped after this op succeeds.
    pub(crate) fn mutates_repo(self) -> bool {
        self == Plane::RepoWrite
    }
}

/// The plane `req` is served on. No wildcard arm: a new `Request` variant
/// does not compile until it is classified here.
pub(crate) fn plane(req: &Request) -> Plane {
    match req {
        // `targets` is the execution brief; `impact` and `targets` read the
        // graph a replica opens read-only (the writer builds it first).
        Request::Ping
        | Request::Search { .. }
        | Request::Targets { .. }
        | Request::Resolve { .. }
        | Request::Impact { .. }
        | Request::Meaning { .. } => Plane::Read,
        Request::Publish { .. }
        | Request::Push { .. }
        | Request::Ship { .. }
        | Request::BranchOp { .. }
        | Request::Update { .. }
        | Request::Sync { .. }
        | Request::Rename { .. }
        | Request::Reindex {} => Plane::RepoWrite,
        // Graph ops build the graph when it is missing or drifted; history,
        // lifecycle and excavate run a lazy ingest unless `read_only`; the
        // git ops spawn git; `status` probes the stores; `shutdown` must
        // stay ordered behind every write.
        Request::Recall { .. }
        | Request::TargetsFacts { .. }
        | Request::Symbol { .. }
        | Request::Skeleton { .. }
        | Request::Context { .. }
        | Request::Uses { .. }
        | Request::Trace { .. }
        | Request::Processes { .. }
        | Request::Clusters { .. }
        | Request::Changes { .. }
        | Request::ReviewGate { .. }
        | Request::Graph { .. }
        | Request::Status {}
        | Request::History { .. }
        | Request::Lifecycle { .. }
        | Request::Excavate { .. }
        | Request::Reconcile { .. }
        | Request::Journal { .. }
        | Request::Inspect { .. }
        | Request::Review { .. }
        | Request::Diff { .. }
        | Request::HistoryOp { .. }
        | Request::Note { .. }
        | Request::Map { .. }
        | Request::Plan { .. }
        | Request::Evaluate { .. }
        | Request::Cycles { .. }
        | Request::Shutdown => Plane::Write,
    }
}

/// A read replica may answer `req`: a read-plane op, or a `targets_facts`
/// or `symbol` that sets `stale_ok`. Those stay [`Plane::Write`] for every
/// other purpose (the evidence bridge, the git-state invalidation): only the
/// caller that accepts the published generation lets a replica serve them.
pub(crate) fn replica_may_answer(req: &Request) -> bool {
    plane(req) == Plane::Read || req.stale_ok()
}

/// The request an evidence-bridge query kind stands for, `None` for a kind
/// the bridge does not know. The bridge serves it only when its [`plane`] is
/// [`Plane::Read`].
pub(crate) fn evidence_request(kind: &str, query: &str, limit: usize) -> Option<Request> {
    let text = query.to_string();
    Some(match kind {
        "execution_brief" => Request::Targets {
            task: text,
            limit: Some(limit),
            max_tier: Some("P1".into()),
            precision: false,
            regions: false,
        },
        "search" => Request::Search {
            pattern: text,
            json: false,
            limit: Some(limit),
            offset: None,
            paths: None,
            scope: None,
            globs: Vec::new(),
            types: Vec::new(),
        },
        "resolve" => Request::Resolve {
            phrase: text,
            limit: Some(limit),
        },
        "impact" => Request::Impact {
            uid_or_name: text,
            direction: "upstream".into(),
            depth: Some(2),
        },
        "meaning" => Request::Meaning {
            query: text,
            limit: Some(limit),
        },
        "reindex" => Request::Reindex {},
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every required field of every variant, so one object deserializes
    /// into any of them (an internally tagged enum ignores the rest).
    fn any_request(op: &str) -> Request {
        let mut value = json!({
            "action": "a", "params": null, "pattern": "p", "task": "t",
            "query": "q", "name": "n", "file": "f", "uid": "u",
            "uid_or_name": "u", "direction": "upstream", "role": "r",
            "from": "a", "to": "b", "phrase": "p", "kind": "k",
            "message": "m", "files": [], "request_id": "r", "remote": "o",
            "refspec": "s", "expected_head": "h", "target_oid": "t",
            "new_name": "n2",
        });
        value["op"] = json!(op);
        serde_json::from_value(value).unwrap_or_else(|e| panic!("{op}: {e}"))
    }

    /// The expected plane of every op, by wire name.
    const EXPECTED: &[(&str, Plane)] = &[
        ("ping", Plane::Read),
        ("search", Plane::Read),
        ("targets", Plane::Read),
        ("resolve", Plane::Read),
        ("impact", Plane::Read),
        ("meaning", Plane::Read),
        ("publish", Plane::RepoWrite),
        ("push", Plane::RepoWrite),
        ("ship", Plane::RepoWrite),
        ("branch_op", Plane::RepoWrite),
        ("update", Plane::RepoWrite),
        ("sync", Plane::RepoWrite),
        ("rename", Plane::RepoWrite),
        ("reindex", Plane::RepoWrite),
        ("recall", Plane::Write),
        ("targets_facts", Plane::Write),
        ("symbol", Plane::Write),
        ("skeleton", Plane::Write),
        ("context", Plane::Write),
        ("uses", Plane::Write),
        ("trace", Plane::Write),
        ("processes", Plane::Write),
        ("clusters", Plane::Write),
        ("changes", Plane::Write),
        ("review_gate", Plane::Write),
        ("graph", Plane::Write),
        ("status", Plane::Write),
        ("history", Plane::Write),
        ("lifecycle", Plane::Write),
        ("excavate", Plane::Write),
        ("reconcile", Plane::Write),
        ("journal", Plane::Write),
        ("inspect", Plane::Write),
        ("review", Plane::Write),
        ("diff", Plane::Write),
        ("history_op", Plane::Write),
        ("note", Plane::Write),
        ("map", Plane::Write),
        ("plan", Plane::Write),
        ("evaluate", Plane::Write),
        ("cycles", Plane::Write),
        ("shutdown", Plane::Write),
    ];

    #[test]
    fn every_op_has_its_plane() {
        for (op, expected) in EXPECTED {
            let req = any_request(op);
            assert_eq!(req.op_name(), *op);
            assert_eq!(plane(&req), *expected, "{op}");
        }
    }

    /// `plane` has no wildcard arm, so a new variant does not compile
    /// unclassified; this pins `EXPECTED` to the op list clients see, so
    /// the table cannot silently miss the new op's expected plane.
    /// `SESSION_CAPABILITIES` lists `flow` (a CLI command, no `Op` variant)
    /// and leaves out the internal `shutdown` and `reindex`.
    #[test]
    fn the_table_covers_every_op_a_client_may_send() {
        let mut classified: Vec<&str> = EXPECTED.iter().map(|(op, _)| *op).collect();
        let mut advertised: Vec<&str> = pixel_proto::op::SESSION_CAPABILITIES
            .iter()
            .copied()
            .filter(|op| *op != "flow")
            .chain(["shutdown", "reindex"])
            .collect();
        classified.sort_unstable();
        advertised.sort_unstable();
        assert_eq!(classified, advertised);
    }

    /// Only the ops that move HEAD, a branch or the tree drop the cached git
    /// state: a read clearing it would cost three git spawns per request.
    #[test]
    fn only_repo_writes_mutate_the_repo() {
        assert!(Plane::RepoWrite.mutates_repo());
        assert!(!Plane::Write.mutates_repo());
        assert!(!Plane::Read.mutates_repo());
    }

    #[test]
    fn evidence_kinds_are_the_read_plane_ops_they_name() {
        let cases = [
            ("execution_brief", "targets"),
            ("search", "search"),
            ("resolve", "resolve"),
            ("impact", "impact"),
            ("meaning", "meaning"),
        ];
        for (kind, op) in cases {
            let req = evidence_request(kind, "login", 4).expect(kind);
            assert_eq!(req.op_name(), op);
            assert_eq!(plane(&req), Plane::Read, "{kind}");
        }
        assert_eq!(
            evidence_request("execution_brief", "login", 4),
            Some(Request::Targets {
                task: "login".into(),
                limit: Some(4),
                max_tier: Some("P1".into()),
                precision: false,
                regions: false,
            })
        );
        assert_eq!(
            evidence_request("impact", "login", 4),
            Some(Request::Impact {
                uid_or_name: "login".into(),
                direction: "upstream".into(),
                depth: Some(2),
            })
        );
        // Known to the wire but refused by the plane; unknown altogether.
        let reindex = evidence_request("reindex", "login", 4).expect("reindex");
        assert_eq!(plane(&reindex), Plane::RepoWrite);
        assert_eq!(evidence_request("telepathy", "login", 4), None);
    }
}
