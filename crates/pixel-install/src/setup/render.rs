// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Rendering the steering block from the selected features.
//!
//! The block is rendered once and written to every selected agent's instruction
//! file, so its text is agent-independent by construction. What differs per
//! agent is *where* the text lands, which [`crate::setup::agent::AgentTarget`]
//! owns.

use std::fmt::Write as _;

use super::feature::{Feature, Writes, steering_title};
use super::files::{BLOCK_END, BLOCK_START};

/// The managed block for `features`: every selected steering feature as one
/// `###` section, in catalog order, between the markers.
///
/// An empty selection still produces a well-formed block: the markers are the
/// contract, and a block that lost them could no longer be replaced later.
pub fn render_block(features: &[Feature]) -> String {
    let mut body = String::new();
    body.push_str(BLOCK_START);
    body.push('\n');
    for feature in Feature::ALL {
        if !features.contains(&feature) {
            continue;
        }
        let Some(text) = steering_text(feature) else {
            continue;
        };
        // The baseline reads as the section's parent: `##` for the workflow
        // itself, `###` for what it refines.
        let heading = if feature == Feature::Prompt {
            "##"
        } else {
            "###"
        };
        let _ = writeln!(body, "{heading} {}\n", steering_title(feature));
        for line in text.lines() {
            let _ = writeln!(body, "{line}");
        }
        body.push('\n');
    }
    body.push_str(BLOCK_END);
    body.push('\n');
    body
}

/// The steering bullets of a feature, or `None` when it writes no steering.
fn steering_text(feature: Feature) -> Option<&'static str> {
    match feature.writes() {
        Writes::Steering(text) => Some(text),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selected(ids: &[&str]) -> Vec<Feature> {
        Feature::ALL
            .iter()
            .copied()
            .filter(|feature| ids.contains(&feature.id()))
            .collect()
    }

    #[test]
    fn the_default_selection_renders_the_baseline_and_nothing_else() {
        let block = render_block(
            &Feature::ALL
                .iter()
                .copied()
                .filter(|f| f.default_selected())
                .collect::<Vec<_>>(),
        );
        assert!(block.starts_with(BLOCK_START) && block.ends_with(&format!("{BLOCK_END}\n")));
        assert!(block.contains("## Pixel\n"), "got {block}");
        assert!(block.contains("pixel scope-task"), "got {block}");
        for feature in Feature::ALL.iter().filter(|f| f.default_selected()) {
            if let Writes::Steering(_) = feature.writes() {
                let heading = steering_title(*feature);
                assert!(block.contains(heading), "{heading} missing from {block}");
            }
        }
    }

    #[test]
    fn the_catalog_order_decides_the_section_order_not_the_selection_order() {
        let forward = render_block(&selected(&["prompt", "semantic"]));
        let backward = render_block(&selected(&["semantic", "prompt"]));
        assert_eq!(forward, backward);
        let prompt = forward.find("## Pixel").unwrap();
        let semantic = forward.find("### Search").unwrap();
        assert!(prompt < semantic, "got {forward}");
    }

    #[test]
    fn a_feature_that_writes_no_steering_adds_no_section() {
        let block = render_block(&selected(&[
            "prompt", "metrics", "daemon", "guard", "rules",
        ]));
        assert!(
            block.contains("## Pixel\n"),
            "the markers and the title: {block}"
        );
        for absent in ["### Search", "### Web search", "### Publishing"] {
            assert!(!block.contains(absent), "{absent} must not be rendered");
        }
    }

    #[test]
    fn an_empty_selection_is_still_a_well_formed_block() {
        let block = render_block(&[]);
        assert_eq!(block, format!("{BLOCK_START}\n{BLOCK_END}\n"));
        assert!(
            crate::setup::files::upsert("", &block).is_ok(),
            "a block without steering must stay replaceable"
        );
    }

    #[test]
    fn every_section_is_a_heading_followed_by_bullets() {
        let block = render_block(&Feature::ALL);
        let body = block
            .strip_prefix(BLOCK_START)
            .and_then(|rest| rest.strip_suffix(&format!("{BLOCK_END}\n")))
            .expect("the block is wrapped in markers");
        let mut sections = 0;
        for line in body.lines() {
            if line.starts_with("## ") {
                sections += 1;
                assert!(
                    line.trim_start().starts_with("## ") && !line.starts_with("###"),
                    "the baseline is a level-2 heading: {line}"
                );
            } else if line.starts_with("### ") {
                sections += 1;
            } else if !line.trim().is_empty() {
                assert!(line.starts_with("- "), "not a bullet: {line}");
            }
        }
        assert_eq!(
            sections, 4,
            "prompt, semantic, web-search and land: {block}"
        );
    }
}
