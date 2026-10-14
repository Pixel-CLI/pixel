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
//! - [`plan`] — every write resolved before anything is written;
//! - [`files`] — the managed-block format, kept so `pixel setup --remove` can
//!   still strip a block an earlier release upserted (pixel itself writes
//!   none: the `pixel` skill carries the content now).
//!
//! The parts deliberately left out: `but setup` (pixel prepares nothing here —
//! no index, no workspace mode), the skill-bundle install (the `pixel` skill
//! deploy lives in [`crate::skill`], not here), and the runtime
//! retired-syntax sweep (the answers are resolved fresh on every run).
//!
//! [`but-agent-setup`]: https://docs.gitbutler.com/ai-agents/getting-started

mod agent;
mod detect;
mod feature;
mod files;
mod plan;

pub use agent::{AgentTarget, Scope};
pub use detect::{Agent, ENVIRONMENT_VARIABLES, ParseAgentError, detect, detect_with};
pub use feature::{ConfigValue, Feature, HookGroup, Writes};
pub use files::{BLOCK_END, BLOCK_START, strip};
pub use pixel_plugin::Available;
pub use plan::{ConfigWrite, SetupPlan, rule_files};

/// Every file a setup ever deployed its block into under `base`, whatever
/// the answers: the instruction file of each agent in `scope` and the rule
/// files. Nothing writes the block any more, but the doctor still reads the
/// list to check text an earlier release deployed, and `--remove` strips it.
pub fn deployed_files(base: &std::path::Path, scope: Scope) -> Vec<std::path::PathBuf> {
    let mut paths: Vec<std::path::PathBuf> = AgentTarget::ALL
        .iter()
        .filter_map(|agent| agent.instruction_path(base, scope))
        .chain(rule_files(base))
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// The text of the blocks `pixel setup` owns in `text`, and nothing the user
/// wrote around them; empty when there is none or the markers are malformed.
pub fn managed_text(text: &str) -> String {
    files::block_spans(text)
        .map(|spans| {
            spans
                .iter()
                .map(|span| &text[span.clone()])
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Remove the block `pixel setup` owns from `text`, keeping the user's own
/// lines. Used by `pixel setup --remove`.
pub fn strip_block(text: &str) -> String {
    files::strip(text)
}

// Test modules come last in their file (`test-hygiene.md`; the workspace
// boundary test in `crates/pixel-git/tests/boundary.rs` enforces it, and
// caught this one in CI). `goldens` holds tests only, so it is declared here
// with the others rather than beside the modules it reads.
#[cfg(test)]
mod goldens;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_module_exposes_the_whole_answer_surface() {
        assert_eq!(AgentTarget::ALL.len(), 9);
        assert_eq!(Feature::ALL.len(), 4);
        assert!(BLOCK_START.starts_with("<!-- pixel:setup:"));
        assert!(BLOCK_END.ends_with("-->"));
    }

    /// A block in the retired managed format, for the strip tests: nothing
    /// renders one for real any more.
    fn retired_block() -> String {
        format!("{BLOCK_START}\nold guidance\n{BLOCK_END}\n")
    }

    #[test]
    fn stripping_removes_a_block_and_leaves_the_users_own_lines() {
        let block = retired_block();
        let file = format!("# House rules\n\n{block}\nkeep this\n");
        assert_eq!(strip_block(&file), "# House rules\n\nkeep this\n");
    }

    #[test]
    fn a_retired_block_can_be_stripped_back_out() {
        let block = retired_block();
        let with = files::upsert("keep\n", &block).unwrap();
        assert!(with.starts_with("keep\n\n"), "got {with:?}");
        assert_eq!(
            files::strip(&with),
            "keep\n",
            "the blank line the block was separated by goes with it"
        );
    }
}
