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

/// Environment variable naming a SearXNG base URL (`https://host`, no path).
const SEARXNG_ENV: &str = "PIXEL_WEB_SEARCH_URL";
/// Environment variable naming the Perplexity API key.
const PERPLEXITY_ENV: &str = "PERPLEXITY_API_KEY";

/// The configured SearXNG base URL, if any: the env var wins, then the
/// stored `web_search.searxng_url`.
#[cfg_attr(test, mutants::skip)] // thin env/config adapter; `normalize_base` is tested directly
fn searxng_base() -> Option<String> {
    normalize_base(std::env::var_os(SEARXNG_ENV)).or_else(crate::config_cmd::web_search_searxng_url)
}

/// The configured Perplexity API key, if any: the env var wins, then the
/// stored `remote_keys.perplexity`.
#[cfg_attr(test, mutants::skip)] // thin env/config adapter; `normalize_base` is tested directly
fn perplexity_key() -> Option<String> {
    normalize_base(std::env::var_os(PERPLEXITY_ENV))
        .or_else(crate::config_cmd::web_search_perplexity_key)
}

/// A configured value is present, UTF-8, and non-empty.
fn normalize_base(value: Option<std::ffi::OsString>) -> Option<String> {
    value
        .and_then(|v| v.into_string().ok())
        .filter(|v| !v.is_empty())
}

/// The provider `pixel web-search` resolves to, for `pixel config` and
/// `pixel doctor`: `searxng`, `perplexity`, or `none` (public chain).
#[cfg_attr(test, mutants::skip)] // thin env/config resolution; the print path is not a contract
pub(crate) fn configured_provider() -> &'static str {
    if searxng_base().is_some() {
        "searxng"
    } else if perplexity_key().is_some() {
        "perplexity"
    } else {
        "none"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_base_keeps_only_nonempty_utf8_values() {
        use std::ffi::OsString;
        assert_eq!(normalize_base(None), None);
        assert_eq!(normalize_base(Some(OsString::new())), None);
        assert_eq!(
            normalize_base(Some(OsString::from("https://sx.test"))),
            Some("https://sx.test".to_string())
        );
        // Non-UTF-8 values cannot name a URL and are dropped.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            assert_eq!(normalize_base(Some(OsString::from_vec(vec![0xFF]))), None);
        }
    }
}
