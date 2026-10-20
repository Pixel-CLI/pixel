// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Allocation ceilings for the daemon's request frame.
//!
//! A daemon that allocates per request pays the allocator on every call and
//! grows the heap for a fixed-size line. `dhat` counts blocks and bytes, so
//! "this path must not allocate" is an assertion instead of a review note.
//!
//! Each ceiling is the *current* cost, not an aspiration: a change that
//! adds an allocation lowers nothing and fails here, which is the point.
//! When a change removes one, tighten the number in the same pull request.
//!
//! One test, one profiler: dhat allows only one running profiler per
//! process, and the fixtures are built before it starts so they stay out of
//! the count.
//!
//! The socket is not measured (it allocates nothing on our side); the read
//! is.

use std::io::BufReader;
use std::time::{Duration, Instant};

use pixel_daemon::wire::{ReadResult, read_capped_line_with, write_response};

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

const MAX_REQUEST_LINE: usize = 1 << 20;

fn read_one_line(line: &[u8]) -> ReadResult {
    let mut reader = BufReader::new(line);
    let mut buf = String::new();
    read_capped_line_with(
        &mut reader,
        &mut buf,
        MAX_REQUEST_LINE,
        Instant::now() + Duration::from_secs(60),
        Instant::now,
    )
}

#[test]
fn reading_a_request_line_should_stay_within_its_allocation_ceiling() {
    let short = br#"{"op":"targets","protocol":1,"query":"daemon request loop"}"#.to_vec();
    let mut long = br#"{"op":"targets","protocol":1,"query":"#.to_vec();
    long.extend(std::iter::repeat_n(b' ', 8 * 1024));
    long.extend(b"\"}\n");

    let first = serde_json::json!({"ok": true, "result": {"value": "first"}});
    let second = serde_json::json!({"ok": false});
    let mut frame = Vec::with_capacity(1024);
    let mut writer = Vec::with_capacity(2048);
    let _profiler = dhat::Profiler::builder().testing().build();

    assert_eq!(read_one_line(&short), ReadResult::Ok);
    let stats = dhat::HeapStats::get();
    // The line buffer the reader fills, and nothing else. `String::from_utf8`
    // moves that same allocation into the caller, so it is one block.
    assert!(
        stats.total_blocks <= 3,
        "one request line allocated {} blocks",
        stats.total_blocks
    );
    assert!(
        stats.total_bytes <= 12288,
        "one request line allocated {} bytes",
        stats.total_bytes
    );

    assert_eq!(read_one_line(&long), ReadResult::Ok);
    let grown = dhat::HeapStats::get().total_blocks;
    assert!(
        grown <= 6,
        "a line over the 4 KiB start capacity grew the buffer to {grown} blocks in total"
    );

    // Reusing capacity must remove per-response allocations without retaining
    // bytes from the larger preceding response or emitting extra newlines.
    let before = dhat::HeapStats::get().total_blocks;
    write_response(&mut writer, &mut frame, &first).unwrap();
    write_response(&mut writer, &mut frame, &second).unwrap();
    assert_eq!(dhat::HeapStats::get().total_blocks, before);
    assert_eq!(
        writer,
        b"{\"ok\":true,\"result\":{\"value\":\"first\"}}\n{\"ok\":false}\n"
    );
}
