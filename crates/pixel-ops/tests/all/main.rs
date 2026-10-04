// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Single integration-test binary for `pixel-ops`: each former `tests/<name>.rs`
//! is a module here, so cargo links one executable instead of one per file.

mod branches;
mod crash_matrix;
mod envfile;
mod fail_closed;
mod provenance;
mod publish_property;
mod reconcile_matrix;
mod repo_identity;
mod rewrite_matrix;

/// One lock for every module that points `XDG_STATE_HOME` at a per-test
/// directory (`crash_matrix`, `reconcile_matrix`). The variable is
/// process-wide: when each file was its own binary a module-local mutex
/// was enough, but in one binary two locals let one module remove the
/// variable while the other's test is mid-flight ("read journal: No such
/// file or directory").
pub(crate) static XDG_STATE_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
