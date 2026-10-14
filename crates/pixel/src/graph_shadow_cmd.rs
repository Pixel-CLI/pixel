// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel graph-shadow` — this graph's Rust edges against pixel-mvp's
//! index ([`pixel_graph::shadow`]). Both graphs are built in a scratch
//! directory, in-process: nothing under the repository is written and no
//! daemon is asked.

use std::path::Path;

use pixel_graph::shadow::{LINE_TOLERANCE, Shadow};

pub fn run(path: &Path, examples: usize, json: bool) -> Result<(), String> {
    let root = path
        .canonicalize()
        .map_err(|e| format!("graph-shadow: {}: {e}", path.display()))?;
    let report =
        pixel_graph::shadow::shadow(&root, examples).map_err(|e| format!("graph-shadow: {e}"))?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
    } else {
        print!("{}", render_human(&report));
    }
    Ok(())
}

/// The plain-text report, as a pure string so its lines are assertable.
fn render_human(r: &Shadow) -> String {
    let c = &r.calls;
    let by = &c.v1_only_by;
    let refs = &r.references;
    let i = &r.imports;
    let mut out = format!(
        "Rust edges: this graph (v1) against pixel-mvp {} (one caller and callee within {LINE_TOLERANCE} lines are one edge)\n",
        r.mvp_commit
    );
    out.push_str(&format!(
        "calls       v1 {} ({} exact, {} probable), mvp {}, both {}, v1 only {}, mvp only {}\n",
        c.overlap.v1,
        c.v1_exact,
        c.v1_probable,
        c.overlap.mvp,
        c.overlap.both,
        c.overlap.v1_only,
        c.overlap.mvp_only
    ));
    out.push_str(&format!(
        "  v1 only:  {} exact, {} probable, {} into another language; receiver: {} method, {} path, {} none; pixel-mvp at the site: {} unresolved, {} another callee, no site {}\n",
        by.exact,
        by.probable,
        by.into_other_language,
        by.method_receiver,
        by.path_receiver,
        by.no_receiver,
        by.mvp_abstains,
        by.mvp_resolves_elsewhere,
        by.mvp_has_no_site
    ));
    out.push_str(&format!(
        "references  v1 {}, mvp {}, both {}, v1 only {}, mvp only {}\n",
        refs.v1, refs.mvp, refs.both, refs.v1_only, refs.mvp_only
    ));
    out.push_str(&format!(
        "imports     {} rows; resolved: v1 {}, mvp {}; same file {}, v1 only {}, mvp only {}, another file {}\n",
        i.rows, i.v1_resolved, i.mvp_resolved, i.same, i.v1_only, i.mvp_only, i.different_file
    ));
    out.push_str(&format!(
        "unresolved  v1 {}, mvp {}\n",
        r.unresolved.v1, r.unresolved.mvp
    ));
    out.push_str(&format!(
        "dropped     {} pixel-mvp edges with no caller here (test code), {} with no target here\n",
        r.dropped_no_caller, r.dropped_no_target
    ));
    out.push_str(&format!(
        "other languages into Rust, by name: {}\n",
        r.into_rust_from_other_languages
    ));
    for (title, list) in [
        ("v1 only", &r.examples.v1_only),
        ("mvp only", &r.examples.mvp_only),
    ] {
        if !list.is_empty() {
            out.push_str(&format!("{title}, for example:\n"));
            for line in list {
                out.push_str(&format!("  {line}\n"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_report_names_every_count_and_the_examples() {
        let mut report = Shadow {
            mvp_commit: "abc1234",
            ..Shadow::default()
        };
        report.calls.overlap.v1 = 10;
        report.calls.v1_exact = 4;
        report.calls.v1_probable = 6;
        report.calls.v1_only_by.into_other_language = 2;
        report.into_rust_from_other_languages = 3;
        report.examples.v1_only = vec!["run -> join (src/lib.rs:9)".to_string()];
        let text = render_human(&report);
        assert!(text.starts_with("Rust edges: this graph (v1) against pixel-mvp abc1234 (one caller and callee within 3 lines"), "{text}");
        assert!(
            text.contains("calls       v1 10 (4 exact, 6 probable), mvp 0,"),
            "{text}"
        );
        assert!(text.contains(" 2 into another language;"), "{text}");
        assert!(
            text.contains("other languages into Rust, by name: 3\n"),
            "{text}"
        );
        assert!(
            text.ends_with("v1 only, for example:\n  run -> join (src/lib.rs:9)\n"),
            "{text}"
        );
        assert!(!text.contains("mvp only, for example"), "{text}");
    }
}
