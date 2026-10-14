// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel graph-shadow` — this graph's Rust edges against pixel-mvp's
//! index ([`pixel_graph::shadow`]), or, with `--lang ruby`, what merging
//! its Ruby answer site by site does to this graph's
//! ([`pixel_graph::shadow::ruby`]). Both graphs are built in a scratch
//! directory, in-process: nothing under the repository is written and no
//! daemon is asked.

use std::path::Path;

use pixel_graph::shadow::ruby::RubyShadow;
use pixel_graph::shadow::{LINE_TOLERANCE, Shadow};

/// The language `graph-shadow` compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Lang {
    /// pixel-mvp's edges replace this graph's (`PIXEL_GRAPH_SOURCE_RUST`).
    Rust,
    /// pixel-mvp's edges are merged with this graph's, site by site.
    Ruby,
}

pub fn run(path: &Path, lang: Lang, examples: usize, json: bool) -> Result<(), String> {
    let root = path
        .canonicalize()
        .map_err(|e| format!("graph-shadow: {}: {e}", path.display()))?;
    if lang == Lang::Ruby {
        let report = pixel_graph::shadow::ruby::shadow(&root, examples)
            .map_err(|e| format!("graph-shadow: {e}"))?;
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
            );
        } else {
            print!("{}", render_ruby(&report));
        }
        return Ok(());
    }
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

/// The plain-text report of the Ruby merge.
fn render_ruby(r: &RubyShadow) -> String {
    let s = &r.sites;
    let mut out = format!(
        "Ruby edges: this graph (v1) merged with pixel-mvp {} site by site (pixel-mvp's target where it resolves, v1's edge where it does not)\n",
        r.mvp_commit
    );
    out.push_str(&format!(
        "sites         pixel-mvp resolves {}, abstains {}, mixed {}, has no such site {}\n",
        s.resolved, s.abstained, s.mixed, s.no_site
    ));
    for (title, e) in [
        ("calls", &r.calls),
        ("routes", &r.routes),
        ("associations", &r.associations),
        ("references", &r.references),
    ] {
        out.push_str(&format!(
            "{title:<13} v1 {}, hybrid {}: both {} ({} raised to exact), v1 kept {}, v1 replaced {}; pixel-mvp added at v1-unresolved sites {}, at mixed sites {}, at its own sites {}\n",
            e.v1,
            e.hybrid,
            e.both,
            e.raised_to_exact,
            e.v1_kept,
            e.v1_replaced,
            e.mvp_at_unresolved,
            e.mvp_at_mixed,
            e.mvp_only_site
        ));
    }
    out.push_str(&format!(
        "unresolved    v1 {}, hybrid {}\n",
        r.unresolved.v1, r.unresolved.hybrid
    ));
    out.push_str(&format!(
        "dropped       {} pixel-mvp targets with no symbol here, {} sites with no caller here\n",
        r.dropped_no_target, r.dropped_no_caller
    ));
    for (title, list) in [
        ("v1 replaced", &r.examples.v1_replaced),
        ("pixel-mvp added", &r.examples.mvp_added),
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

    #[test]
    fn the_ruby_text_report_names_each_kind_and_the_examples() {
        let mut report = RubyShadow {
            mvp_commit: "abc1234",
            ..RubyShadow::default()
        };
        report.sites.resolved = 3;
        report.calls.v1 = 10;
        report.calls.hybrid = 9;
        report.calls.v1_replaced = 1;
        report.routes.mvp_only_site = 2;
        report.examples.v1_replaced =
            vec!["Card#label -> Book#title (app/models/card.rb:8)".to_string()];
        let text = render_ruby(&report);
        assert!(
            text.starts_with(
                "Ruby edges: this graph (v1) merged with pixel-mvp abc1234 site by site"
            ),
            "{text}"
        );
        assert!(
            text.contains("sites         pixel-mvp resolves 3, abstains 0,"),
            "{text}"
        );
        assert!(text.contains("calls         v1 10, hybrid 9: both 0 (0 raised to exact), v1 kept 0, v1 replaced 1;"), "{text}");
        assert!(text.contains("at its own sites 2\n"), "{text}");
        assert!(
            text.ends_with(
                "v1 replaced, for example:\n  Card#label -> Book#title (app/models/card.rb:8)\n"
            ),
            "{text}"
        );
        assert!(!text.contains("pixel-mvp added, for example"), "{text}");
    }
}
