// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The daemon's NDJSON frame codec: one bounded request line in, one
//! response line out, over a socket.
//!
//! Both directions are here rather than inline in `daemon` so that the
//! transport loop and the wire benches (`pixel-bench/benches/wire.rs`)
//! exercise the same code, instead of a bench-shaped copy that drifts.
//!
//! The write side carries two shapes on purpose while it is being
//! measured: [`write_line_split`] writes the payload and the newline
//! separately (the error-arm shape), [`write_line_whole`] appends the
//! newline before the single write. Normal responses already append their
//! newline before writing; the split lane is not their production baseline.

use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::time::Instant;

/// How a read of one request line ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadResult {
    /// A line was read into the caller's buffer, without its newline.
    Ok,
    /// The peer closed before the line started.
    Eof,
    /// The line passed the cap before a newline arrived.
    TooLong,
    /// The bytes were not UTF-8.
    InvalidUtf8,
    /// The deadline passed.
    TimedOut,
    /// The underlying read failed.
    Err,
}

/// Read one line into `buf`, returning `TooLong` if it exceeds `max_bytes`
/// before a newline is found. The trailing newline is consumed but not
/// included in `buf` (same semantics as `read_line` minus the newline).
pub fn read_capped_line(
    reader: &mut std::io::BufReader<UnixStream>,
    buf: &mut String,
    max_bytes: usize,
    deadline: Instant,
) -> ReadResult {
    read_capped_line_with(reader, buf, max_bytes, deadline, Instant::now)
}

/// [`read_capped_line`] over any buffered reader, with the clock passed in.
/// The line is taken a buffered block at a time, and the deadline is
/// checked once per block, not once per byte.
pub fn read_capped_line_with(
    reader: &mut impl BufRead,
    buf: &mut String,
    max_bytes: usize,
    deadline: Instant,
    mut now: impl FnMut() -> Instant,
) -> ReadResult {
    let mut bytes = Vec::with_capacity(max_bytes.min(4096));
    loop {
        if now() >= deadline {
            return ReadResult::TimedOut;
        }
        let Ok(block) = reader.fill_buf() else {
            return ReadResult::Err;
        };
        if block.is_empty() {
            if bytes.is_empty() {
                return ReadResult::Eof;
            }
            break;
        }
        let newline = block.iter().position(|&b| b == b'\n');
        let line_part = newline.unwrap_or(block.len());
        if bytes.len() + line_part > max_bytes {
            return ReadResult::TooLong;
        }
        bytes.extend_from_slice(&block[..line_part]);
        match newline {
            Some(at) => {
                reader.consume(at + 1);
                break;
            }
            None => reader.consume(line_part),
        }
    }
    match String::from_utf8(bytes) {
        Ok(line) => {
            *buf = line;
            ReadResult::Ok
        }
        Err(_) => ReadResult::InvalidUtf8,
    }
}

/// Write `payload` as one NDJSON frame: payload, newline, flush. This is
/// the payload write and the newline write kept apart.
pub fn write_line_split(writer: &mut impl Write, payload: &[u8]) -> std::io::Result<()> {
    writer.write_all(payload)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// [`write_line_split`] with the newline appended first, so the frame
/// costs one payload buffer and one write.
pub fn write_line_whole(writer: &mut impl Write, payload: &[u8]) -> std::io::Result<()> {
    let mut frame = Vec::with_capacity(payload.len() + 1);
    frame.extend_from_slice(payload);
    frame.push(b'\n');
    writer.write_all(&frame)?;
    writer.flush()
}
