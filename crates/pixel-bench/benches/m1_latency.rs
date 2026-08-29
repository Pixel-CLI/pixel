//! M1 latency gates per PLAN.md:
//! - Daemon retrieve operations: <1ms service time
//! - CLI end-to-end: <5ms
//!
//! These benchmarks measure the in-process service path (the daemon's
//! `Service::handle` call), which is the service-time component. The CLI
//! end-to-end gate includes process startup + socket round-trip and is
//! measured separately by the parity harness's timing wrapper; here we
//! gate the service-time half (<1ms) which is the deterministic core.
//!
//! Run with: cargo bench -p pixel-bench --bench m1_latency

use std::path::PathBuf;

use criterion::{Criterion, criterion_group, criterion_main};
use pixel_daemon::api::{Request, Service};
use pixel_proto::Op;
use tempfile::tempdir;

/// Build a fixture repo with N files containing a known needle, then open
/// the daemon Service against it (index auto-built on first search).
fn fixture_with_needle(n_files: usize, needle: &str) -> (PathBuf, Service) {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(root.join(".gitignore"), ".pixel/\n").unwrap();

    // Init a git repo so the index layer can discover the root.
    std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .arg(&root)
        .status()
        .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["config", "user.email", "b@b"])
        .status()
        .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["config", "user.name", "b"])
        .status()
        .unwrap();

    for i in 0..n_files {
        let path = root.join(format!("file_{i}.rs"));
        let content = format!("// file {i}\npub fn item_{i}() {{ \"{needle}\" }}\n");
        std::fs::write(&path, content).unwrap();
    }
    std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["add", "."])
        .status()
        .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["commit", "-qm", "bench fixture"])
        .status()
        .unwrap();

    // Leak the tempdir so it persists for the benchmark. We don't need to
    // clean up -- the OS handles /tmp reaping.
    std::mem::forget(dir);

    let svc = Service::open(&root).unwrap();
    (root, svc)
}

/// M1 gate: daemon search service time must be <1ms for a typical repo.
/// This is the deterministic core (no socket, no process startup).
fn bench_search_service_time(c: &mut Criterion) {
    let (_root, mut svc) = fixture_with_needle(50, "uniqueNeedle123");

    // Warm up the index (first search builds it).
    let _ = svc.handle(Request::from(Op::Search {
        pattern: "uniqueNeedle123".into(),
        json: false,
        limit: Some(10),
        offset: None,
        paths: None,
        scope: None,
    }));

    c.bench_function("search_service_time", |b| {
        b.iter(|| {
            svc.handle(Request::from(Op::Search {
                pattern: "uniqueNeedle123".into(),
                json: false,
                limit: Some(10),
                offset: None,
                paths: None,
                scope: None,
            }))
        })
    });
}

/// M1 gate: daemon targets service time must be <1ms for a typical repo.
fn bench_targets_service_time(c: &mut Criterion) {
    let (_root, mut svc) = fixture_with_needle(50, "uniqueNeedle123");

    // Warm up the graph (first targets builds it).
    let _ = svc.handle(Request::from(Op::Targets {
        task: "fix uniqueNeedle123".into(),
        limit: Some(20),
    }));

    c.bench_function("targets_service_time", |b| {
        b.iter(|| {
            svc.handle(Request::from(Op::Targets {
                task: "fix uniqueNeedle123".into(),
                limit: Some(20),
            }))
        })
    });
}

/// M1 gate: ranked search service time must be <1ms (the ranking adds
/// file-grouping + RRF fusion on top of the base search).
fn bench_ranked_search_service_time(c: &mut Criterion) {
    let (_root, mut svc) = fixture_with_needle(50, "uniqueNeedle123");

    // Warm up.
    let _ = svc.handle(Request::from(Op::Search {
        pattern: "uniqueNeedle123".into(),
        json: false,
        limit: Some(10),
        offset: None,
        paths: None,
        scope: Some("code".into()),
    }));

    c.bench_function("ranked_search_service_time", |b| {
        b.iter(|| {
            svc.handle(Request::from(Op::Search {
                pattern: "uniqueNeedle123".into(),
                json: false,
                limit: Some(10),
                offset: None,
                paths: None,
                scope: Some("code".into()),
            }))
        })
    });
}

/// M1 gate: ping (protocol handshake) service time must be <1ms.
fn bench_ping_service_time(c: &mut Criterion) {
    let (_root, mut svc) = fixture_with_needle(10, "needle");

    c.bench_function("ping_service_time", |b| {
        b.iter(|| svc.handle(Request::from(Op::Ping)))
    });
}

criterion_group! {
    name = m1_gates;
    config = Criterion::default()
        .sample_size(100)
        .measurement_time(std::time::Duration::from_secs(5));
    targets =
        bench_ping_service_time,
        bench_search_service_time,
        bench_targets_service_time,
        bench_ranked_search_service_time,
}
criterion_main!(m1_gates);
