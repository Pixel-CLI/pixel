//! M1 latency gates per PLAN.md:
//! - Daemon retrieve operations: <1ms service time
//! - CLI end-to-end: <5ms
//!
//! ## What actually gates here
//!
//! `criterion::Criterion::bench_function` only measures and prints
//! statistics -- it has no notion of pass/fail, so a `cargo bench` run
//! that regresses `targets_service_time` to 10ms still exits 0. To make
//! the <1ms daemon service-time budget an actual gate, every
//! `bench_*_service_time` function below also runs a plain
//! `Instant`-timed loop and `assert!`s the median against the budget
//! (see `assert_service_time_budget`), so a violation panics and fails
//! the `cargo bench` run. The same assertions are exposed as ordinary
//! `#[test]` functions (`gate_*`) so they can be run the fast way, without
//! criterion's statistical sampling overhead, via
//! `cargo test -p pixel-bench --bench m1_latency`.
//!
//! These benchmarks/tests only exercise the in-process service path (the
//! daemon's `Service::handle` call) -- no socket, no process startup. That
//! is deliberate: it is the deterministic core PLAN.md's <1ms budget is
//! about, and it is the part of the pipeline a synthetic in-process
//! fixture can actually measure.
//!
//! ## The <5ms CLI end-to-end gate is NOT currently measured anywhere
//!
//! An earlier version of this comment claimed the CLI end-to-end gate was
//! "measured separately by the parity harness's timing wrapper"
//! (`tests/parity/harness.sh`, since removed). That was false: the parity
//! harness had no timing or latency-measurement code at all -- it only
//! checked output parity between implementations. There is currently no
//! automated check anywhere in this repo for the <5ms CLI end-to-end
//! budget.
//!
//! That gap is also not a small oversight to casually fix: an ad hoc
//! measurement (fork the real `pixel` binary via `std::process::Command`,
//! time the full process lifecycle) showed a bare process-spawn floor of
//! roughly 17ms for the ~45MB CLI binary on this machine, before the CLI
//! does any work at all. That is already ~3.4x over the stated <5ms
//! target on process startup alone, independent of how fast the daemon
//! side is. If that floor holds up under more rigorous measurement, the
//! <5ms CLI end-to-end target as stated is not reachable by a
//! fork-per-invocation CLI architecture, and PLAN.md's gate needs to
//! either move to a persistent/daemon-resident client, or be restated as
//! a service-time-only budget with process startup called out separately.
//! This is flagged here as an open issue rather than silently glossed
//! over; it is not fixed by this file alone since it also constrains the
//! CLI's process model, which lives outside `pixel-bench`.
//!
//! Run with: cargo bench -p pixel-bench --bench m1_latency
//! Fast gate-only run: cargo test -p pixel-bench --bench m1_latency

use std::path::PathBuf;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use pixel_daemon::api::{Request, Response, Service};
use pixel_proto::Op;
use tempfile::tempdir;

/// The M1 daemon service-time budget from PLAN.md.
const SERVICE_TIME_BUDGET: Duration = Duration::from_millis(1);

/// Iteration count for the fast (`#[test]`) gate path. Large enough for a
/// stable median without criterion's sampling overhead.
const FAST_GATE_ITERS: usize = 200;

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

    // Leak the tempdir so it persists for the benchmark/test. Each
    // invocation of this helper leaks exactly one tempdir (not one per
    // timing iteration -- callers reuse the same `Service` across their
    // N timed iterations), so this does not accumulate unreasonably; the
    // OS reaps /tmp on its own schedule.
    std::mem::forget(dir);

    let svc = Service::open(&root).unwrap();
    (root, svc)
}

/// Run `op` `iters` times, timing each call with `Instant`, and assert the
/// median duration is under `budget`. Panics (failing the `cargo bench` or
/// `cargo test` run) if the budget is violated -- this is the actual gate;
/// criterion's own `bench_function` only measures and never fails.
///
/// Also asserts every response is `ok`, so a gate can't "pass" by silently
/// timing a failed/error response.
fn assert_service_time_budget(
    name: &str,
    budget: Duration,
    iters: usize,
    mut op: impl FnMut() -> Response,
) {
    assert!(iters > 0, "{name}: iters must be > 0");
    let mut samples: Vec<Duration> = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = std::time::Instant::now();
        let resp = op();
        samples.push(start.elapsed());
        assert!(
            resp.ok,
            "{name}: service call returned ok=false: {:?}",
            resp.error
        );
    }
    samples.sort();
    let median = samples[samples.len() / 2];
    let p95_idx = ((samples.len() * 95) / 100).min(samples.len() - 1);
    let p95 = samples[p95_idx];
    assert!(
        median < budget,
        "{name}: median service time {median:?} exceeds the M1 <{budget:?} gate \
         (p95 {p95:?}, n={iters}) -- this is a real regression, not measurement noise"
    );
}

/// M1 gate: daemon search service time must be <1ms for a typical repo.
/// This is the deterministic core (no socket, no process startup).
fn bench_search_service_time(c: &mut Criterion) {
    let (_root, mut svc) = fixture_with_needle(50, "uniqueNeedle123");

    let make_req = || {
        Request::from(Op::Search {
            pattern: "uniqueNeedle123".into(),
            json: false,
            limit: Some(10),
            offset: None,
            paths: None,
            scope: None,
        })
    };

    // Warm up the index (first search builds it).
    let _ = svc.handle(make_req());

    // Real gate: fails this `cargo bench` run if the budget is violated.
    assert_service_time_budget("search_service_time", SERVICE_TIME_BUDGET, FAST_GATE_ITERS, || {
        svc.handle(make_req())
    });

    c.bench_function("search_service_time", |b| b.iter(|| svc.handle(make_req())));
}

/// M1 gate: daemon targets service time must be <1ms for a typical repo.
fn bench_targets_service_time(c: &mut Criterion) {
    let (_root, mut svc) = fixture_with_needle(50, "uniqueNeedle123");

    let make_req = || {
        Request::from(Op::Targets {
            task: "fix uniqueNeedle123".into(),
            limit: Some(20),
            max_tier: None,
            precision: false,
        })
    };

    // Warm up the graph (first targets builds it).
    let _ = svc.handle(make_req());

    // Real gate: fails this `cargo bench` run if the budget is violated.
    // If `targets` is still over budget, this MUST fail loudly -- do not
    // raise the threshold to paper over a real regression.
    assert_service_time_budget("targets_service_time", SERVICE_TIME_BUDGET, FAST_GATE_ITERS, || {
        svc.handle(make_req())
    });

    c.bench_function("targets_service_time", |b| b.iter(|| svc.handle(make_req())));
}

/// M1 gate: ranked search service time must be <1ms (the ranking adds
/// file-grouping + RRF fusion on top of the base search).
fn bench_ranked_search_service_time(c: &mut Criterion) {
    let (_root, mut svc) = fixture_with_needle(50, "uniqueNeedle123");

    let make_req = || {
        Request::from(Op::Search {
            pattern: "uniqueNeedle123".into(),
            json: false,
            limit: Some(10),
            offset: None,
            paths: None,
            scope: Some("code".into()),
        })
    };

    // Warm up.
    let _ = svc.handle(make_req());

    // Real gate: fails this `cargo bench` run if the budget is violated.
    assert_service_time_budget(
        "ranked_search_service_time",
        SERVICE_TIME_BUDGET,
        FAST_GATE_ITERS,
        || svc.handle(make_req()),
    );

    c.bench_function("ranked_search_service_time", |b| {
        b.iter(|| svc.handle(make_req()))
    });
}

/// M1 gate: ping (protocol handshake) service time must be <1ms.
fn bench_ping_service_time(c: &mut Criterion) {
    let (_root, mut svc) = fixture_with_needle(10, "needle");

    // Real gate: fails this `cargo bench` run if the budget is violated.
    assert_service_time_budget("ping_service_time", SERVICE_TIME_BUDGET, FAST_GATE_ITERS, || {
        svc.handle(Request::from(Op::Ping))
    });

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

// ---------------------------------------------------------------------
// Fast gate-only tests (no criterion sampling overhead).
//
// NOTE ON HOW THESE RUN: this file is a `[[bench]]` target compiled with
// `harness = false` (required because `criterion_main!` above defines its
// own `fn main`), and Cargo's default for `[[bench]]` targets is
// `test = false`. That means plain `cargo test -p pixel-bench` will not
// pick these up implicitly; they run when the target is explicitly
// selected with `cargo test -p pixel-bench --bench m1_latency`. Either
// way, `cargo bench -p pixel-bench --bench m1_latency` always runs the
// same `assert_service_time_budget` checks inline (see the `bench_*`
// functions above), so the gate is enforced by the criterion entry point
// regardless of how the test harness is wired.
// ---------------------------------------------------------------------

#[test]
fn gate_ping_service_time() {
    let (_root, mut svc) = fixture_with_needle(10, "needle");
    assert_service_time_budget("ping_service_time", SERVICE_TIME_BUDGET, FAST_GATE_ITERS, || {
        svc.handle(Request::from(Op::Ping))
    });
}

#[test]
fn gate_search_service_time() {
    let (_root, mut svc) = fixture_with_needle(50, "uniqueNeedle123");
    let make_req = || {
        Request::from(Op::Search {
            pattern: "uniqueNeedle123".into(),
            json: false,
            limit: Some(10),
            offset: None,
            paths: None,
            scope: None,
        })
    };
    let _ = svc.handle(make_req());
    assert_service_time_budget("search_service_time", SERVICE_TIME_BUDGET, FAST_GATE_ITERS, || {
        svc.handle(make_req())
    });
}

#[test]
fn gate_ranked_search_service_time() {
    let (_root, mut svc) = fixture_with_needle(50, "uniqueNeedle123");
    let make_req = || {
        Request::from(Op::Search {
            pattern: "uniqueNeedle123".into(),
            json: false,
            limit: Some(10),
            offset: None,
            paths: None,
            scope: Some("code".into()),
        })
    };
    let _ = svc.handle(make_req());
    assert_service_time_budget(
        "ranked_search_service_time",
        SERVICE_TIME_BUDGET,
        FAST_GATE_ITERS,
        || svc.handle(make_req()),
    );
}

/// Deliberately NOT asserting the <1ms budget for `targets` unconditionally
/// passing -- this is the real gate. If `targets_service_time` is still
/// over budget, this test MUST fail and say so; it must never be loosened
/// to hide a real regression.
#[test]
fn gate_targets_service_time() {
    let (_root, mut svc) = fixture_with_needle(50, "uniqueNeedle123");
    let make_req = || {
        Request::from(Op::Targets {
            task: "fix uniqueNeedle123".into(),
            limit: Some(20),
            max_tier: None,
            precision: false,
        })
    };
    let _ = svc.handle(make_req());
    assert_service_time_budget("targets_service_time", SERVICE_TIME_BUDGET, FAST_GATE_ITERS, || {
        svc.handle(make_req())
    });
}
