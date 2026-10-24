// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! pixel-brief: the prompt brief (`specs/brief.md`).
//!
//! `prompt` reads the text the user typed, `relevance` and `gate_model` decide
//! whether the repository talks about it, `routes` picks the question kind,
//! `chain` runs the kind's evidence steps over an [`chain::Evidence`] and
//! renders the `[PIXEL:BRIEF]` block, `region_pack` packs the source it names,
//! and `sent` keeps a session from being sent the same range twice. Nothing
//! here reads a store: the daemon implements `Evidence`.

mod answer;
pub mod chain;
mod continuation;
pub mod decision;
mod french;
mod gate_model;
pub mod git_changes;
pub mod kinds;
mod locator;
pub mod parts;
mod passes;
pub mod probes;
pub mod prompt;
pub mod region_pack;
pub mod relevance;
pub mod routes;
pub mod sent;
mod session_paths;
pub mod steps;
pub mod tokens;
pub mod words;
