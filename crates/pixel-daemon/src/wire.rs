// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The daemon's NDJSON frame codec: one bounded request line in, one
//! response line out, over a socket.
//!
//! Both directions are here rather than inline in `daemon` so that the
//! transport loop and the wire benches (`pixel-bench/benches/wire.rs`)
//! exercise the same code, instead of a bench-shaped copy that drifts.
//!
//! Production serializes into a reused buffer and writes the complete frame.
//! [`write_line_split`] remains a benchmark comparison; it is not the normal
//! response baseline, which already included the newline in one write.

use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::time::Instant;

/// The envelope sent when a response itself cannot be serialized.
pub const SERIALIZE_FAILURE: &[u8] =
    b"{\"ok\":false,\"op\":\"error\",\"protocol\":1,\"error\":{\"code\":\"INVARIANT_VIOLATION\",\"message\":\"serialize failure\"}}\n";

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

/// Serialize `value` as one NDJSON frame into `out`: the buffer is
/// cleared first, the newline padded last, so the caller can hand the whole
/// slice to one [`write_line_whole`]. Reusing `out` across requests keeps
/// the frame allocation out of the request path.
pub fn encode_line(out: &mut Vec<u8>, value: &impl serde::Serialize) -> serde_json::Result<()> {
    out.clear();
    serde_json::to_writer(&mut *out, value)?;
    out.push(b'\n');
    Ok(())
}

/// Serialize `response` into `out` and write it as one frame, one write.
/// This is the production path; `out` is the connection's reused buffer.
pub fn write_response(
    writer: &mut impl Write,
    out: &mut Vec<u8>,
    response: &impl serde::Serialize,
) -> std::io::Result<()> {
    encode_line(out, response).map_err(std::io::Error::other)?;
    write_line_whole(writer, out)
}

/// Write `payload` and then the newline, as two writes.
pub fn write_line_split(writer: &mut impl Write, payload: &[u8]) -> std::io::Result<()> {
    writer.write_all(payload)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// Write an already-encoded frame (newline included) in one write.
pub fn write_line_whole(writer: &mut impl Write, frame: &[u8]) -> std::io::Result<()> {
    writer.write_all(frame)?;
    writer.flush()
}
