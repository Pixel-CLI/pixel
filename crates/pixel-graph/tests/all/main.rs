// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Single integration-test binary for `pixel-graph`: each former `tests/<name>.rs`
//! is a module here, so cargo links one executable instead of one per file.

mod changes_consumers;
mod changes_suggested_tests;
mod changes_uncovered;
mod concept_engine1_audit;
mod concept_tests;
mod crux_lines;
mod env_read_concepts;
mod import_resolution;
mod resolve_receiver_shadowing;
mod ruby_bare_calls;
mod ruby_callbacks;
mod ruby_constant_receivers;
mod ruby_generated_methods;
mod scoped_symbol_lookup;
mod unresolved_diagnostic;
