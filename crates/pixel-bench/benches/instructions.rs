// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Executed instructions for production kernels, with setup outside collection.
//! The repetitions input doubles real work for the lane's negative control.

use std::hint::black_box;
use std::time::{Duration, Instant};

use gungraun::{LibraryBenchmarkConfig, library_benchmark, library_benchmark_group, main};
use pixel_daemon::wire::{ReadResult, read_capped_line_with, write_response};
use pixel_index::{Crc32Weigher, GramExtractor, GramHit, SparseGramExtractor};
use pixel_recall::code_search::cosine as score_cosine;

fn repetitions() -> usize {
    match std::env::var("PIXEL_BENCH_REPETITIONS").as_deref() {
        Ok("2") => 2,
        Ok("1") | Err(_) => 1,
        _ => panic!("benchmark repetitions must be 1 or 2"),
    }
}

fn read_fixture(size: usize) -> (Vec<u8>, Vec<u8>, Instant, usize) {
    let mut line = vec![b'x'; size];
    line.push(b'\n');
    (
        line,
        Vec::with_capacity(size),
        Instant::now(),
        repetitions(),
    )
}

#[library_benchmark]
#[bench::short(read_fixture(64))]
#[bench::long(read_fixture(8192))]
fn wire_read((line, mut output, now, count): (Vec<u8>, Vec<u8>, Instant, usize)) {
    for _ in 0..count {
        let mut reader = black_box(line.as_slice());
        assert_eq!(
            read_capped_line_with(
                &mut reader,
                &mut output,
                line.len(),
                now + Duration::from_secs(1),
                || now,
            ),
            ReadResult::Ok
        );
        black_box(&output);
    }
}

fn write_fixture(size: usize) -> (serde_json::Value, Vec<u8>, Vec<u8>, usize) {
    let response = serde_json::json!({"ok": true, "result": "x".repeat(size)});
    let capacity = size + 128;
    (
        response,
        Vec::with_capacity(capacity),
        Vec::with_capacity(capacity),
        repetitions(),
    )
}

#[library_benchmark]
#[bench::short(write_fixture(64))]
#[bench::long(write_fixture(8192))]
fn wire_write(
    (response, mut buffer, mut writer, count): (serde_json::Value, Vec<u8>, Vec<u8>, usize),
) {
    for _ in 0..count {
        writer.clear();
        write_response(black_box(&mut writer), &mut buffer, black_box(&response)).unwrap();
        black_box(&writer);
    }
}

fn vector_fixture(dim: usize) -> (Vec<f32>, Vec<f32>, usize) {
    // Arithmetic fixtures avoid platform libm differences in the input vectors.
    let a = (0..dim).map(|i| (i % 31) as f32 / 31.0).collect();
    let b = (0..dim).map(|i| (i % 17) as f32 / 17.0).collect();
    (a, b, repetitions())
}

#[library_benchmark]
#[bench::embedding(vector_fixture(384))]
#[bench::tail(vector_fixture(769))]
fn cosine((a, b, count): (Vec<f32>, Vec<f32>, usize)) {
    for _ in 0..count {
        black_box(score_cosine(black_box(&a), black_box(&b)));
    }
}

fn extraction_fixture() -> (Vec<u8>, Vec<GramHit>, usize) {
    // Fixed source: editing the extractor must not also change its input.
    let source = b"pub fn find(items: &[Item], name: &str) -> Option<usize> {\n\
        items.iter().position(|item| item.name == name)\n}\n";
    let corpus = source.repeat(256);
    let output = Vec::with_capacity(corpus.len() * 4);
    (corpus, output, repetitions())
}

#[library_benchmark]
#[bench::sparse(extraction_fixture())]
fn extraction((corpus, mut output, count): (Vec<u8>, Vec<GramHit>, usize)) {
    let extractor = SparseGramExtractor::new(Crc32Weigher);
    for _ in 0..count {
        output.clear();
        extractor.grams(black_box(&corpus), &mut output);
        black_box(&output);
    }
}

library_benchmark_group!(
    name = kernels;
    benchmarks = wire_read, wire_write, cosine, extraction
);
main!(
    config = LibraryBenchmarkConfig::default()
        .pass_through_env("PIXEL_BENCH_REPETITIONS");
    library_benchmark_groups = kernels
);
