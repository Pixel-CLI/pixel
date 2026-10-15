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
//! - [`agent`] — one target per harness, and no marker shared between two
//!   agents;
//! - [`feature`] — what each feature actually writes, stated as one of four
//!   kinds rather than left to the review screen to imply;
//! - [`plan`] — every write resolved before anything is written.
//!
//! The parts deliberately left out: `but setup` (pixel prepares nothing here —
//! no index, no workspace mode) and the skill-bundle install (the `pixel`
//! skill deploy lives in [`crate::skill`], not here).
//!
//! [`but-agent-setup`]: https://docs.gitbutler.com/ai-agents/getting-started

mod agent;
mod detect;
mod feature;
mod plan;

pub use agent::{AgentTarget, Scope};
pub use detect::{Agent, ENVIRONMENT_VARIABLES, ParseAgentError, detect, detect_with};
pub use feature::{ConfigValue, Feature, HookGroup, Writes};
pub use pixel_plugin::Available;
pub use plan::{ConfigWrite, SetupPlan};

// Test modules come last in their file (`test-hygiene.md`; the workspace
// boundary test in `crates/pixel-git/tests/boundary.rs` enforces it).
// `goldens` holds tests only, so it is declared here with the others rather
// than beside the modules it reads.
#[cfg(test)]
mod goldens;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_module_exposes_the_whole_answer_surface() {
        assert_eq!(AgentTarget::ALL.len(), 9);
        assert_eq!(Feature::ALL.len(), 4);
    }
}
