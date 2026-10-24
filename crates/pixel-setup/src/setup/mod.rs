// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pixel setup`: choose which agent CLIs to write for, then write it.
//!
//! - [`detect`] — the env-marker detection (ported from GitButler's `but
//!   agent setup`, read at `5cbe33d`), so the wizard can name the harness
//!   that is running it;
//! - [`agent`] — one target per harness, and no marker shared between two
//!   agents;
//! - [`wizard`] — the one question per agent a terminal run asks.

mod agent;
mod detect;
pub mod wizard;

pub use agent::{AgentTarget, Scope};
pub use detect::{Agent, ENVIRONMENT_VARIABLES, ParseAgentError, detect, detect_with};
