// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! A pick a plugin added: validated against the current generation by the
//! plugin host, merged after the chain's picks by the minimap
//! (`design/plugins.md`, "The protocol"; C-31).

use crate::rows::Symbol;

/// One pick of a `chain-step` plugin, as the generation knows it: the
/// symbol (and so its path and line) is read from the graph, never taken
/// from the plugin.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginPick {
    /// The plugin's name, shown in the reason (`plugin <name>: <reason>`).
    pub plugin: String,
    pub symbol: Symbol,
    /// One line of at most 80 characters, control characters removed.
    pub reason: String,
    /// At most 0.8, so a prompt's named symbols stay first.
    pub score: f64,
}
