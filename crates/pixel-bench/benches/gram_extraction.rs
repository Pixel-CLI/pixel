// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use pixel_bench::source_corpus;
use pixel_index::{Crc32Weigher, GramExtractor, SparseGramExtractor, TrigramExtractor};

fn bench_extraction(c: &mut Criterion) {
    let corpus = source_corpus(2 * 1024 * 1024);
    let sparse = SparseGramExtractor::new(Crc32Weigher);
    let trigram = TrigramExtractor;

    let mut group = c.benchmark_group("gram_extraction");
    group.throughput(Throughput::Bytes(corpus.len() as u64));

    group.bench_function("sparse_crc32", |b| {
        let mut out = Vec::with_capacity(4 * 1024 * 1024);
        b.iter(|| {
            out.clear();
            sparse.grams(&corpus, &mut out);
            out.len()
        })
    });

    group.bench_function("trigram", |b| {
        let mut out = Vec::with_capacity(4 * 1024 * 1024);
        b.iter(|| {
            out.clear();
            trigram.grams(&corpus, &mut out);
            out.len()
        })
    });

    group.finish();

    // One-off stats printed for the phase-exit record.
    let mut out = Vec::new();
    sparse.grams(&corpus, &mut out);
    let n = corpus.len();
    println!(
        "corpus={}B sparse_grams={} ({:.2} grams/byte) trigram_grams={}",
        n,
        out.len(),
        out.len() as f64 / n as f64,
        n - 2
    );
}

criterion_group!(benches, bench_extraction);
criterion_main!(benches);
