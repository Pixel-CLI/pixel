// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Web-search provider *configuration*.
//!
//! The `pixel web-search` command moved to the `pixel-utilities` plugin, but
//! the provider resolution stays in the core: `pixel config` and
//! `pixel doctor` report which provider a run would use, and the
//! `pixel config setup` step stores it. Providers, most-private first:
//!   - SearXNG — `PIXEL_WEB_SEARCH_URL=<base>` (or the stored
//!     `web_search.searxng_url`).
//!   - Perplexity — `PERPLEXITY_API_KEY` (or the stored
//!     `remote_keys.perplexity`).
//!   - Otherwise the free public chain (`none`).

/// The provider `pixel web-search` resolves to, for `pixel config`:
/// `searxng`, `perplexity`, or `none` (public chain), by the rule doctor
/// uses too (`pixel_setup::config::web_search_provider`).
#[cfg_attr(test, mutants::skip)] // thin adapter over the process environment and the global file; the rule is tested in pixel-setup
pub(crate) fn configured_provider() -> &'static str {
    pixel_setup::config::web_search_provider(
        pixel_setup::config::WebSearchEnv::from_process(),
        &crate::config_cmd::global_config_doc(),
    )
}
