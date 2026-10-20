// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Cosine similarity over embedding-sized vectors: the single-accumulator
//! shape the scorer started with, against the production lane version.
//!
//! One accumulator serializes every `f32` add behind the previous one; eight
//! independent lanes do not. The bodies are otherwise identical, so the
//! pair isolates the dependency chain.
//!
//! Run with: cargo bench -p pixel-bench --bench cosine

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use pixel_recall::code_search::cosine;

/// A single-accumulator cosine, as the scorer computed it first.
fn cosine_serial(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = (na * nb).sqrt();
    if denom > 0.0 { dot / denom } else { 0.0 }
}

fn vector(dim: usize, seed: f32) -> Vec<f32> {
    (0..dim)
        .map(|i| (i as f32 * 0.37 + seed).sin() * 0.1)
        .collect()
}

fn bench_cosine(c: &mut Criterion) {
    let mut group = c.benchmark_group("cosine");
    for dim in [64_usize, 384, 768] {
        let a = vector(dim, 0.0);
        let b = vector(dim, 1.0);
        group.bench_with_input(BenchmarkId::new("serial", dim), &dim, |bench, _| {
            bench.iter(|| cosine_serial(&a, &b));
        });
        group.bench_with_input(BenchmarkId::new("lanes", dim), &dim, |bench, _| {
            bench.iter(|| cosine(&a, &b));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_cosine);
criterion_main!(benches);
