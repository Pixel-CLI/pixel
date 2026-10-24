// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The settings in force for one repository: the real environment and the real
//! home over `pixel_setup::settings`. Every switch the binary reads goes through
//! here, so `pixel config` shows what the commands and the hooks act on.

use std::path::{Path, PathBuf};

use pixel_setup::settings::{Setting, Settings};

/// `$HOME`, where the global layer lives.
pub(crate) fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// The settings of `repo` (the global ones only when it is `None`).
pub(crate) fn load(repo: Option<&Path>) -> Settings {
    Settings::load(&home(), repo, &|name| std::env::var(name).ok())
}

/// Whether the 🟩 lines are on for `repo`.
pub(crate) fn metrics_on(repo: Option<&Path>) -> bool {
    load(repo).on(Setting::Metrics)
}

/// Whether a command may start the daemon of `repo`.
pub(crate) fn daemon_auto_start(repo: Option<&Path>) -> bool {
    load(repo).on(Setting::DaemonAutoStart)
}

/// Whether the edit-time and stop-time checks run for `repo`.
pub(crate) fn edit_hooks_on(repo: Option<&Path>) -> bool {
    load(repo).on(Setting::EditHooks)
}

/// Whether the prompt brief is switched off for `repo`.
pub(crate) fn brief_off(repo: Option<&Path>) -> bool {
    !load(repo).on(Setting::Brief)
}
