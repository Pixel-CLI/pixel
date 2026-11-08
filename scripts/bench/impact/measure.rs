// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Paired production impact queries; fixture creation is outside measurement.
use pixel_graph::store;
use std::{
    hint::black_box,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};
use store::{EdgeKind, EdgeRow, GraphStore, SymbolKind, Tier};

static STATEMENTS: AtomicU64 = AtomicU64::new(0);
fn trace(_: rusqlite::trace::TraceEvent<'_>) {
    STATEMENTS.fetch_add(1, Ordering::Relaxed);
}

fn cpu_time_ns() -> u128 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: time is writable and initialized; the clock reads this thread's CPU time.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) },
        0
    );
    time.tv_sec as u128 * 1_000_000_000 + time.tv_nsec as u128
}

fn query(store: &GraphStore, uid: &str, down: bool, depth: u32, old: bool) -> Vec<u8> {
    // Neither arm inherits prepared statements left by the other arm.
    store.conn().flush_prepared_statement_cache();
    if old {
        let direction = if down {
            before::Direction::Downstream
        } else {
            before::Direction::Upstream
        };
        serde_json::to_vec(&before::impact(store, uid, direction, depth, 20).unwrap()).unwrap()
    } else {
        let direction = if down {
            after::Direction::Downstream
        } else {
            after::Direction::Upstream
        };
        serde_json::to_vec(&after::impact(store, uid, direction, depth, 20).unwrap()).unwrap()
    }
}
fn main() {
    for (name, nodes, fan, copies, flows, depth, down) in [
        ("empty", 1, 1, 1, false, 3, false),
        ("single-caller", 2, 1, 1, false, 3, false),
        ("chain", 4, 1, 1, false, 3, false),
        ("wide", 1001, 1000, 1, false, 3, false),
        ("branching", 585, 8, 1, false, 3, false),
        ("duplicates", 129, 128, 8, false, 3, false),
        ("diamond", 65, 32, 1, false, 3, false),
        ("cached-flows", 585, 8, 1, true, 3, false),
        ("depth-one", 585, 8, 1, false, 1, false),
        ("downstream", 585, 8, 1, false, 3, true),
    ] {
        let mut graph = GraphStore::open_in_memory().unwrap();
        let file = graph.replace_file("bench.ts", "fixture", "ts").unwrap();
        let ids: Vec<_> = (0..nodes)
            .map(|i| {
                graph
                    .insert_symbol(
                        file,
                        &format!("bench.ts#node{i}#function"),
                        &format!("node{i}"),
                        &format!("node{i}"),
                        SymbolKind::Function,
                        i + 1,
                        i + 1,
                        "",
                    )
                    .unwrap()
            })
            .collect();
        for i in 1..nodes {
            let parents: Vec<_> = if name == "diamond" && i > 32 {
                (1..=32).collect()
            } else {
                vec![(i - 1) / fan]
            };
            for parent in parents {
                for copy in 0..copies {
                    graph
                        .insert_edge(&EdgeRow {
                            src_id: ids[i as usize],
                            dst_id: ids[parent as usize],
                            kind: EdgeKind::Calls,
                            tier: if (i + copy) % 2 == 0 {
                                Tier::Exact
                            } else {
                                Tier::Probable
                            },
                            site_line: copy + 1,
                            receiver: None,
                            callee: None,
                        })
                        .unwrap();
                }
            }
        }
        if flows {
            pixel_graph::process::discover(&mut graph, 6, 3, 3, 100).unwrap();
        }
        let uid = format!("bench.ts#node{}#function", if down { nodes - 1 } else { 0 });
        let mut sql = [0; 2];
        let mut outputs = Vec::new();
        for (index, old) in [true, false].into_iter().enumerate() {
            graph
                .conn_mut()
                .trace_v2(
                    rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
                    Some(trace),
                )
                .unwrap();
            STATEMENTS.store(0, Ordering::Relaxed);
            outputs.push(
                serde_json::from_slice::<serde_json::Value>(&query(&graph, &uid, down, depth, old))
                    .unwrap(),
            );
            sql[index] = STATEMENTS.load(Ordering::Relaxed);
            graph
                .conn_mut()
                .trace_v2(
                    rusqlite::trace::TraceEventCodes::empty(),
                    None::<fn(rusqlite::trace::TraceEvent<'_>)>,
                )
                .unwrap();
        }
        for key in ["counts_by_depth", "affected_files"] {
            assert_eq!(outputs[0][key], outputs[1][key], "{name}: {key}");
        }
        let expected = match name {
            "empty" => [0, 0, 0],
            "single-caller" => [1, 0, 0],
            "chain" | "downstream" => [1, 1, 1],
            "wide" => [1000, 0, 0],
            "branching" | "cached-flows" => [8, 64, 512],
            "duplicates" => [128, 0, 0],
            "diamond" => [32, 32, 0],
            "depth-one" => [8, 0, 0],
            _ => unreachable!(),
        };
        assert_eq!(outputs[0]["counts_by_depth"], serde_json::json!(expected));
        if flows {
            assert!(
                !outputs[0]["affected_processes"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(sql[0], sql[1], "{name}: SQL work changed");
        for _ in 0..10 {
            for old in [true, false] {
                black_box(query(&graph, &uid, down, depth, old));
            }
        }
        let start = Instant::now();
        black_box(query(&graph, &uid, down, depth, true));
        let repetitions = (20_000_000 / start.elapsed().as_nanos().max(1)).clamp(1, 2000) as u32;
        let mut samples = Vec::new();
        let mut cpu_samples = Vec::new();
        for round in 0..41 {
            let mut pair = [0.0; 2];
            let mut cpu_pair = [0.0; 2];
            for index in if round % 2 == 0 { [0, 1] } else { [1, 0] } {
                let cpu_start = cpu_time_ns();
                let start = Instant::now();
                for _ in 0..repetitions {
                    black_box(query(&graph, &uid, down, depth, index == 0));
                }
                pair[index] = start.elapsed().as_nanos() as f64 / f64::from(repetitions);
                cpu_pair[index] = (cpu_time_ns() - cpu_start) as f64 / f64::from(repetitions);
            }
            samples.push(pair);
            cpu_samples.push(cpu_pair);
        }
        println!(
            "{}",
            serde_json::json!({"case": name, "nodes": nodes, "repetitions": repetitions,
                "sql_statements": sql, "samples_ns": samples, "cpu_samples_ns": cpu_samples})
        );
    }
}
