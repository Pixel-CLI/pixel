// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel cycles`: the daemon-side wrapper around the bounded SCC
//! enumeration.
//!
//! The core algorithm lives in [`pixel_graph::cycles`]. This module
//! handles the daemon-specific concerns: opening the graph store, running
//! the enumeration with a wall clock, and converting the result to the
//! wire format.

use pixel_graph::cycles::{self, Enumeration};
use pixel_graph::store::GraphStore;

pub use pixel_graph::cycles::TierSelection;

/// What the caller asked for, already parsed and validated.
#[derive(Debug, Clone)]
pub struct Args {
    pub tiers: TierSelection,
    pub max_nodes: u32,
    pub max_edges: u32,
    pub time_budget_ms: u64,
    pub max_components: u32,
}

/// Run the cycle enumeration against `store`.
///
/// # Errors
///
/// Any store read failure.
pub fn run(store: &GraphStore, args: &Args) -> Result<Enumeration, String> {
    let mut clock = cycles::WallClock::start();
    cycles::enumerate(
        store,
        cycles::Request {
            tiers: args.tiers,
            budget: cycles::Budget {
                max_nodes: args.max_nodes,
                max_edges: args.max_edges,
                time_budget: std::time::Duration::from_millis(args.time_budget_ms),
                max_components: args.max_components,
            },
        },
        &mut clock,
    )
    .map_err(|e| format!("cycles: {e}"))
}
