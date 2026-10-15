// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT
//! Research only: chunk-level evidence per prompt, never committed.
use std::path::Path;
use std::time::Instant;

use pixel_recall::code_resident::Resident;
use pixel_recall::code_search::VectorCache;
use pixel_recall::embed::open_default_embedder;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (root, input, output) = (Path::new(&args[1]), &args[2], &args[3]);
    let mut embedder = open_default_embedder(false).expect("embedder on disk");
    let started = Instant::now();
    let resident = Resident::build(root, None, embedder.as_mut(), VectorCache::Disabled, 5_000)
        .expect("resident build");
    eprintln!("built {:?} in {:?}", resident.stats().chunks, started.elapsed());
    let rows: Vec<serde_json::Value> = std::fs::read_to_string(input)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let mut out = Vec::new();
    for row in rows {
        let text = row["text"].as_str().unwrap();
        let noncommon: Vec<String> = row["noncommon"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        let started = Instant::now();
        let trace = resident.trace(embedder.as_mut(), text, &noncommon).expect("trace");
        let millis = started.elapsed().as_secs_f64() * 1000.0;
        out.push(serde_json::json!({"id": row["id"], "trace": trace, "ms": millis}));
    }
    std::fs::write(output, serde_json::to_vec(&out).unwrap()).unwrap();
}
