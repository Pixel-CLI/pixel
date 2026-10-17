// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Single integration-test binary for `pixel-daemon` (see CONTRIBUTING.md:
//! one binary per crate). Each module builds its own git fixture in a temp
//! dir through `support::git`, which isolates git from the machine's config; all but `watcher_freshness` drive `Service::handle` directly, that
//! one runs the daemon's socket loop to test transport ordering.

mod graph_incremental;
mod regions;
mod relevance;
mod support;
mod targets;
mod watcher_freshness;
