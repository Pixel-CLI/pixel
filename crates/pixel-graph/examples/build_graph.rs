// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Builds the graph of a tree into a given db and prints its counts and
//! phase timings as JSON: `cargo run --release -p pixel-graph --example
//! build_graph -- <root> <db>`. A benchmark harness for the full build's
//! wall time and peak RSS.

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut args = std::env::args_os().skip(1);
    let (Some(root), Some(db)) = (args.next(), args.next()) else {
        return Err("usage: build_graph <root> <db>".into());
    };
    let stats = pixel_graph::build::build_graph(root.as_ref(), db.as_ref())?;
    println!("{stats:#?}");
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines().filter(|l| l.starts_with("VmHWM")) {
        println!("{line}");
    }
    Ok(())
}
