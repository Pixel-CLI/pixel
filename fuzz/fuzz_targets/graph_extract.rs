// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel_graph::extract::extract_file` on arbitrary bytes.
//!
//! The graph is built from whatever a repository holds: half-written files,
//! a vendored grammar test case, a binary blob with a source extension. The
//! daemon re-extracts every saved file, so input here is untrusted.
//!
//! Input: `<extension>\n<file content>`, e.g. `rs\nfn main() {}`; inputs
//! whose extension `lang_of` does not map are skipped. The seeds under
//! `fuzz/seeds/graph_extract/` hold one per language.
//!
//! Invariants:
//! - no panic. `extract_file` wraps the walk in `catch_unwind`, but the
//!   panic hook libFuzzer installs aborts first, so a panic the production
//!   code would swallow (and turn into a file missing from the graph) is a
//!   crash here;
//! - every line a symbol, call site, reference or JSX element reports is a
//!   line of the file (1-based, at most one past the last newline), with
//!   `start_line <= end_line`;
//! - every `enclosing_index` points into `symbols` (`build.rs` indexes the
//!   stored symbol ids with it) at a symbol whose range holds the site.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pixel_graph::extract::{FileExtraction, extract_file, lang_of};

fuzz_target!(|data: &[u8]| {
    let Some(newline) = data.iter().position(|&byte| byte == b'\n') else {
        return;
    };
    let Ok(extension) = std::str::from_utf8(&data[..newline]) else {
        return;
    };
    let content = &data[newline + 1..];
    let path = format!("src/fuzz.{extension}");
    if lang_of(&path).is_none() {
        return;
    }
    if let Some(extraction) = extract_file(&path, content) {
        check(&extraction, content);
    }
});

fn check(extraction: &FileExtraction, content: &[u8]) {
    let newlines = content.iter().filter(|&&byte| byte == b'\n').count();
    let last_line = u32::try_from(newlines + 1).expect("fuzz inputs are far below 4 GiB");
    let in_file = |line: u32| (1..=last_line).contains(&line);

    for symbol in &extraction.symbols {
        assert!(
            in_file(symbol.start_line)
                && in_file(symbol.end_line)
                && symbol.start_line <= symbol.end_line,
            "symbol {:?} spans lines {}..={} of a {last_line}-line file",
            symbol.name,
            symbol.start_line,
            symbol.end_line,
        );
    }
    for element in &extraction.jsx_elements {
        assert!(
            in_file(element.start_line)
                && in_file(element.end_line)
                && element.start_line <= element.end_line,
            "JSX element <{}> spans lines {}..={} of a {last_line}-line file",
            element.tag,
            element.start_line,
            element.end_line,
        );
    }

    let sites = extraction
        .calls
        .iter()
        .map(|call| (&call.callee_name, call.site_line, call.enclosing_index))
        .chain(extraction.references.iter().map(|reference| {
            (
                &reference.name,
                reference.site_line,
                reference.enclosing_index,
            )
        }));
    for (name, line, enclosing) in sites {
        assert!(
            in_file(line),
            "site of {name:?} on line {line} of a {last_line}-line file"
        );
        if let Some(index) = enclosing {
            let symbol = extraction.symbols.get(index).unwrap_or_else(|| {
                panic!(
                    "site of {name:?} encloses symbol #{index} of {}",
                    extraction.symbols.len()
                )
            });
            assert!(
                (symbol.start_line..=symbol.end_line).contains(&line),
                "site of {name:?} on line {line} is enclosed by {:?}, lines {}..={}",
                symbol.name,
                symbol.start_line,
                symbol.end_line,
            );
        }
    }
}
