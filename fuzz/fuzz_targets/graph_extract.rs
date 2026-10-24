// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel_graph::extract::rows` on arbitrary bytes.
//!
//! The graph is built from whatever a repository holds: half-written files,
//! a vendored grammar test case, a binary blob with a source extension. The
//! daemon re-extracts every saved file, so input here is untrusted.
//!
//! Input: `<extension>\n<file content>`, e.g. `rs\nfn main() {}`. Only the
//! extensions a language of `pixel_graph::lang` wants are extracted (and only
//! content that language accepts); the seeds under
//! `fuzz/seeds/graph_extract/` hold one per language and a few that no
//! language wants.
//!
//! Invariants:
//! - no panic in the rule files' extraction: the panic hook libFuzzer
//!   installs aborts first, so a panic the production code would turn into a
//!   file missing from the graph is a crash here;
//! - every line a symbol or a site reports is a line of the file (1-based, at
//!   most one past the last newline), with `line <= end_line`;
//! - every edge's `src` is the uid of a symbol of the same file, and the
//!   first symbol is the file's own.
#![no_main]

use std::collections::HashSet;

use libfuzzer_sys::fuzz_target;
use pixel_graph::extract::rows;
use pixel_graph::lang::{accepts, wanted};

fuzz_target!(|data: &[u8]| {
    let Some(newline) = data.iter().position(|&byte| byte == b'\n') else {
        return;
    };
    let Ok(extension) = std::str::from_utf8(&data[..newline]) else {
        return;
    };
    let content = &data[newline + 1..];
    let path = format!("src/fuzz.{extension}");
    let Some(lang) = wanted(&path) else {
        return;
    };
    if !accepts(lang, &path, content) {
        return;
    }
    let extracted = rows(&path, lang, content);
    let newlines = content.iter().filter(|&&byte| byte == b'\n').count();
    let last_line = u32::try_from(newlines + 1).expect("fuzz inputs are far below 4 GiB");
    let in_file = |line: u32| (1..=last_line).contains(&line);

    for symbol in &extracted.symbols {
        assert!(
            in_file(symbol.line) && in_file(symbol.end_line) && symbol.line <= symbol.end_line,
            "symbol {:?} spans lines {}..={} of a {last_line}-line file",
            symbol.name,
            symbol.line,
            symbol.end_line,
        );
    }
    let uids: HashSet<&str> = extracted
        .symbols
        .iter()
        .map(|symbol| symbol.uid.as_str())
        .collect();
    for edge in &extracted.edges {
        assert!(
            in_file(edge.line),
            "site of {:?} on line {} of a {last_line}-line file",
            edge.target,
            edge.line
        );
        assert!(
            uids.contains(edge.src.as_str()),
            "site of {:?} belongs to {:?}, which is no symbol of the file",
            edge.target,
            edge.src
        );
    }
});
