// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel setup`: choose which agent CLIs to write for and which pixel
//! features to switch on, see exactly what that changes, then write it.
//!
//! Ported from GitButler's `but agent setup` ([`but-agent-setup`]), read at
//! `5cbe33d`. The parts kept are the ones that make an installer honest:
//!
//! - [`detect`] — the same env-marker detection `but` uses, so the wizard can
//!   name the harness that is running it;
//! - [`agent`] — one target per harness, each naming the instruction file it
//!   writes, and no marker shared between two agents;
//! - [`feature`] — what each feature actually writes, stated as one of four
//!   kinds rather than left to the review screen to imply;
//! - [`plan`] — every path resolved before anything is written;
//! - [`render`] / [`files`] — one managed block, rendered once, upserted
//!   idempotently, and refused rather than guessed at when malformed.
//!
//! The parts deliberately left out: `but setup` (pixel prepares nothing here —
//! no index, no workspace mode), the skill-bundle install (pixel ships its
//! prompt through `pixel install`), and the runtime retired-syntax sweep (the
//! answers are rendered fresh on every run).
//!
//! [`but-agent-setup`]: https://docs.gitbutler.com/ai-agents/getting-started

mod agent;
mod detect;
mod feature;
mod files;
#[cfg(test)]
mod goldens;
mod plan;
mod render;

pub use agent::{AgentTarget, Scope};
pub use detect::{Agent, ENVIRONMENT_VARIABLES, ParseAgentError, detect, detect_with};
pub use feature::{ConfigValue, Feature, Writes};
pub use files::{BLOCK_END, BLOCK_START, strip};
pub use plan::{ConfigWrite, InstructionWrite, RuleWrite, SetupPlan, rule_files};

/// Render the block the wizard would write for `features`, without writing
/// anything. This is what `pixel setup --print` prints and what the golden
/// files under `tests/setup/.agents/` are compared against.
pub fn preview(features: &[Feature]) -> String {
    render::render_block(features)
}

/// Remove the block `pixel setup` owns from `text`, keeping the user's own
/// lines. Used by `pixel uninstall`.
pub fn strip_block(text: &str) -> String {
    files::strip(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_module_exposes_the_whole_answer_surface() {
        assert_eq!(AgentTarget::ALL.len(), 9);
        assert_eq!(Feature::ALL.len(), 12);
        assert!(BLOCK_START.starts_with("<!-- pixel:setup:"));
        assert!(BLOCK_END.ends_with("-->"));
    }

    #[test]
    fn stripping_removes_a_block_and_leaves_the_users_own_lines() {
        let block = preview(&[Feature::Prompt]);
        let file = format!("# House rules\n\n{block}\nkeep this\n");
        assert_eq!(strip_block(&file), "# House rules\n\nkeep this\n");
    }

    #[test]
    fn a_preview_can_be_upserted_and_stripped_back_out() {
        let block = preview(&Feature::ALL.iter().copied().take(3).collect::<Vec<_>>());
        let with = files::upsert("keep\n", &block).unwrap();
        assert!(with.starts_with("keep\n\n"), "got {with:?}");
        assert_eq!(
            files::strip(&with),
            "keep\n",
            "the blank line the block was separated by goes with it"
        );
    }
}
