// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Cost of the daemon's NDJSON frame: one bounded request line in, one
//! response line out.
//!
//! Read lanes use the production codec; write lanes compare two framing shapes:
//!
//! - `write/split` — payload and newline written separately, for comparison;
//! - `write/whole` — a pre-encoded frame, one write;
//! - `write/response` — serialize into a reused buffer and write the frame;
//! - `parse/{serde_json,simd_json}` — the request envelope through both
//!   parsers: `simd-json` is 25% slower on a 60-byte line and seven times
//!   slower on an 8 KiB one, which is why the daemon keeps `serde_json`;
//! - `read/short` — a 120-byte request line, the common size;
//! - `read/long` — an 8 KiB line, so the newline scan is not a rounding error.
//!   Each has a `bytes` lane (the per-byte `position` scan the frame used
//!   first, kept here to compare) and a `memchr` lane (the production scan);
//!   both reuse the caller's buffer, so the lane pair isolates the scan.
//!
//! Writes go to a `UnixStream::pair` with a thread draining the far end,
//! so the numbers carry the syscall cost and not a `Vec` sink's memcpy.
//! Reads run over a `Cursor` at the same `MAX_REQUEST_LINE` cap the daemon
//! uses, with a deadline far enough out to never trip.
//!
//! Run with: cargo bench -p pixel-bench --bench wire

use std::io::{BufRead, BufReader, Cursor, Read};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use pixel_daemon::wire::{
    ReadResult, encode_line, read_capped_line_with, write_line_split, write_line_whole,
    write_response,
};

/// The daemon's own cap (`MAX_REQUEST_LINE` in `daemon.rs`).
const MAX_REQUEST_LINE: usize = 1 << 20;

/// A response envelope of the size the daemon usually sends.
fn envelope() -> Vec<u8> {
    let value = serde_json::json!({
        "ok": true,
        "op": "locate",
        "protocol": 1,
        "result": {
            "candidates": [
                {"path": "crates/pixel-daemon/src/daemon.rs", "line": 1471, "why": "handler"},
                {"path": "crates/pixel-daemon/src/wire.rs", "line": 60, "why": "frame"},
            ],
            "evidence": ["the accept loop hands each connection to a thread"],
        }
    });
    serde_json::to_vec(&value).expect("fixture serializes")
}

fn request_line(padding: usize) -> Vec<u8> {
    let mut line = br#"{"op":"targets","protocol":1,"query":"daemon request loop"}"#.to_vec();
    line.extend(std::iter::repeat_n(b' ', padding));
    line.push(b'\n');
    line
}

fn bench_write(c: &mut Criterion) {
    let payload = envelope();
    let mut group = c.benchmark_group("write");
    group.throughput(Throughput::Bytes(payload.len() as u64));

    let drain = |stream: UnixStream| {
        std::thread::spawn(move || {
            let mut sink = [0u8; 4096];
            let mut stream = stream;
            while stream.read(&mut sink).is_ok_and(|n| n > 0) {}
        })
    };

    let (mut split, far) = UnixStream::pair().expect("socketpair");
    let _reader = drain(far);
    group.bench_function("split", |b| {
        b.iter(|| write_line_split(&mut split, &payload).expect("write"));
    });
    drop(split);
    drop(_reader);

    let mut framed = payload.clone();
    framed.push(b'\n');
    let (mut whole, far) = UnixStream::pair().expect("socketpair");
    let _reader = drain(far);
    group.bench_function("whole", |b| {
        b.iter(|| write_line_whole(&mut whole, &framed).expect("write"));
    });

    let response = serde_json::from_slice::<serde_json::Value>(&payload).expect("fixture parses");
    let (mut stream, far) = UnixStream::pair().expect("socketpair");
    let _reader = drain(far);
    let mut out = Vec::with_capacity(1024);
    group.bench_function("response", |b| {
        b.iter(|| write_response(&mut stream, &mut out, &response).expect("write"));
    });
    group.finish();
}

fn bench_encode(c: &mut Criterion) {
    let response =
        serde_json::from_slice::<serde_json::Value>(&envelope()).expect("fixture parses");
    let mut out = Vec::with_capacity(1024);
    c.bench_function("encode/response", |b| {
        b.iter(|| encode_line(&mut out, &response).expect("encode"));
    });
}

/// Parse the same request line with both parsers. `simd-json` needs a
/// mutable buffer and rewrites in place; `serde_json` takes a slice.
fn bench_parse(c: &mut Criterion) {
    use pixel_daemon::Request;
    let mut group = c.benchmark_group("parse");
    for (name, line) in [("short", request_line(0)), ("long", request_line(8 * 1024))] {
        group.bench_with_input(BenchmarkId::new("serde_json", name), &line, |b, line| {
            b.iter_batched(
                || line.clone(),
                |buf| serde_json::from_slice::<Request>(&buf),
                criterion::BatchSize::SmallInput,
            );
        });
        group.bench_with_input(BenchmarkId::new("simd_json", name), &line, |b, line| {
            b.iter_batched(
                || line.clone(),
                |mut buf| simd_json::from_slice::<Request>(buf.as_mut_slice()),
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}
/// The per-byte newline scan the frame used before `memchr`, over the same
/// cap and deadline, so a read lane pair isolates the scan cost.
fn read_bytes_scan(reader: &mut impl BufRead, buf: &mut Vec<u8>, max_bytes: usize) -> ReadResult {
    let deadline = Instant::now() + Duration::from_secs(60);
    buf.clear();
    loop {
        if Instant::now() >= deadline {
            return ReadResult::TimedOut;
        }
        let Ok(block) = reader.fill_buf() else {
            return ReadResult::Err;
        };
        if block.is_empty() {
            if buf.is_empty() {
                return ReadResult::Eof;
            }
            break;
        }
        let newline = block.iter().position(|&b| b == b'\n');
        let line_part = newline.unwrap_or(block.len());
        if buf.len() + line_part > max_bytes {
            return ReadResult::TooLong;
        }
        buf.extend_from_slice(&block[..line_part]);
        match newline {
            Some(at) => {
                reader.consume(at + 1);
                break;
            }
            None => reader.consume(line_part),
        }
    }
    ReadResult::Ok
}

fn bench_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("read");
    for (name, line) in [("short", request_line(0)), ("long", request_line(8 * 1024))] {
        group.throughput(Throughput::Bytes(line.len() as u64));
        group.bench_with_input(BenchmarkId::new("bytes", name), &line, |b, line| {
            b.iter_batched(
                || {
                    (
                        BufReader::new(Cursor::new(line.clone())),
                        Vec::with_capacity(1024),
                    )
                },
                |(mut reader, mut buf)| read_bytes_scan(&mut reader, &mut buf, MAX_REQUEST_LINE),
                criterion::BatchSize::SmallInput,
            );
        });
        group.bench_with_input(BenchmarkId::new("memchr", name), &line, |b, line| {
            b.iter_batched(
                || {
                    (
                        BufReader::new(Cursor::new(line.clone())),
                        Vec::with_capacity(1024),
                    )
                },
                |(mut reader, mut buf)| {
                    read_capped_line_with(
                        &mut reader,
                        &mut buf,
                        MAX_REQUEST_LINE,
                        Instant::now() + Duration::from_secs(60),
                        Instant::now,
                    )
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_write, bench_parse, bench_encode, bench_read);
criterion_main!(benches);
