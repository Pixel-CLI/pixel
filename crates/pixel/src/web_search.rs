// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel web-search` — deterministic web retrieval, no LLM.
//!
//! The plan-refinement gate needs an external answer for terms the index
//! cannot know ("JEV"). This command is the retrieval half of that step:
//! one bounded HTTP call per provider, JSON in, normalized results out —
//! same spirit as the index ops: bounded, marked, never an instruction.
//!
//! Providers, chosen by configuration, most-private first:
//!   - SearXNG — `PIXEL_WEB_SEARCH_URL=<base>` (or the stored
//!     `web_search.searxng_url`) → `GET <base>/search?format=json`. Whoever
//!     runs their own instance keeps their queries off public services, so a
//!     thin or failed answer is returned as it is, never topped up
//!     elsewhere.
//!   - Perplexity — `PERPLEXITY_API_KEY` (or the stored
//!     `remote_keys.perplexity`) → one keyed POST to Perplexity's `/search`
//!     endpoint, normalized from `search_results[]` and tagged `perplexity`.
//!     The same "as configured, as answered" rule keeps a failing key out of
//!     the public chain.
//!   - Otherwise, the free public chain: DuckDuckGo Instant Answer, then
//!     Wikipedia OpenSearch while the hits are fewer than the limit.
//!
//! Results are data: title, url, snippet, engine. The agent resolves the
//! term and re-runs `pixel plan`; nothing here writes a checklist.

use serde_json::{Value, json};
use std::time::Duration;

/// Results returned per invocation unless `--limit` says otherwise.
pub const DEFAULT_LIMIT: usize = 8;
/// Per-provider fetch cap: a slow endpoint must not stall the refine step.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Response body cap: a misconfigured endpoint must not exhaust memory.
/// Search payloads are small; 1 MiB is far past any legitimate page.
const BODY_CAP_BYTES: u64 = 1_048_576;
/// Snippet cap: enough to resolve a term, never a page dump.
const SNIPPET_CAP_CHARS: usize = 280;
/// Environment variable naming a SearXNG base URL (`https://host`, no path).
const SEARXNG_ENV: &str = "PIXEL_WEB_SEARCH_URL";
/// Environment variable naming the Perplexity API key.
const PERPLEXITY_ENV: &str = "PERPLEXITY_API_KEY";
/// The Perplexity `/search` (Sonic) endpoint: one POST, JSON in, JSON out.
const PERPLEXITY_SEARCH_URL: &str = "https://api.perplexity.ai/search";

#[derive(Debug, Clone)]
pub struct WebSearchOptions {
    pub query: String,
    pub limit: usize,
    pub json: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub engine: &'static str,
}

/// The configured SearXNG base URL, if any: the env var wins, then the
/// stored `web_search.searxng_url`.
#[cfg_attr(test, mutants::skip)] // thin config adapter; resolution is tested via `search_with`
fn searxng_base() -> Option<String> {
    normalize_base(std::env::var_os(SEARXNG_ENV)).or_else(crate::config_cmd::web_search_searxng_url)
}

/// The configured Perplexity API key, if any: the env var wins, then the
/// stored `remote_keys.perplexity`. The value leaves this function only in
/// an Authorization header — never in a log, an error, or a URL.
#[cfg_attr(test, mutants::skip)] // thin config adapter; resolution is tested via `search_with`
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

/// The provider a `pixel web-search` run resolves to, for `pixel config`
/// and `pixel doctor`: `searxng`, `perplexity`, or `none` (public chain).
/// The precedence (SearXNG → Perplexity → chain) lives here in one place;
/// the per-provider routing below is what `search_with` tests exercise.
#[cfg_attr(test, mutants::skip)] // thin env/config resolution; routing is tested via `search_with`
pub(crate) fn configured_provider() -> &'static str {
    if searxng_base().is_some() {
        "searxng"
    } else if perplexity_key().is_some() {
        "perplexity"
    } else {
        "none"
    }
}

/// The truth marker for `--json` output: `complete` iff any hit survived.
fn marker(hits: &[Hit]) -> &'static str {
    if hits.is_empty() {
        "unresolved"
    } else {
        "complete"
    }
}

/// The text (non-JSON) rendering of a result set.
fn render(query: &str, hits: &[Hit]) -> String {
    if hits.is_empty() {
        return format!("unresolved: no web results for {query:?}\n");
    }
    let mut out = String::new();
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!(
            "{}. {} [{}]\n   {}\n",
            i + 1,
            h.title,
            h.engine,
            h.url
        ));
        if !h.snippet.is_empty() {
            out.push_str(&format!("   {}\n", h.snippet));
        }
    }
    out
}

/// The result document: `hits` plus the epistemics/snapshot envelope every
/// op emits. `confidence` mirrors the marker — `resolved` when the chain
/// answered, `unresolved` when no provider had anything to say.
fn document(query: &str, limit: usize, hits: &[Hit]) -> Value {
    let engines: Vec<&str> = {
        let mut seen = std::collections::BTreeSet::new();
        hits.iter()
            .map(|h| h.engine)
            .filter(|e| seen.insert(*e))
            .collect()
    };
    json!({
        "query": query,
        "marker": marker(hits),
        "hits": hits.iter().map(|h| json!({
            "title": h.title,
            "url": h.url,
            "snippet": h.snippet,
            "engine": h.engine,
        })).collect::<Vec<_>>(),
        "epistemics": {
            "closed_world": false,
            "lower_bound": true,
            "basis": "web search",
            "confidence": marker(hits),
        },
        "snapshot": {
            "providers": engines,
            "limit": limit,
        },
    })
}

#[cfg_attr(test, mutants::skip)] // printing adapter; logic lives in `document`/`render` and is tested
pub fn run(opts: WebSearchOptions) -> Result<(), String> {
    let hits = search_with(
        &opts.query,
        opts.limit,
        searxng_base().as_deref(),
        perplexity_key().as_deref(),
        &fetch,
    );
    if opts.json {
        // `print_data` enforces PIXEL_OUTPUT_CAP_BYTES and broken-pipe
        // handling — the standard bounded output path.
        crate::print_data(&document(&opts.query, opts.limit, &hits), true)
    } else {
        crate::write_stdout(&render(&opts.query, &hits))
    }
}

/// One request over the fetch seam. The SearXNG and public-chain providers
/// are plain GETs; Perplexity POSTs its JSON body and rides the API key in
/// the Authorization header. The value is carried on the request so no key
/// ever appears in a URL, an error format string, or the action log.
#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub url: String,
    /// JSON body when the provider POSTs (`None` = GET).
    pub post_body: Option<String>,
    /// The `Authorization` header value (e.g. `Bearer <key>`), when a
    /// keyed provider needs one.
    pub authorization: Option<String>,
}

/// The providers over a fetch seam: tests inject canned bodies. A configured
/// provider short-circuits precedence — SearXNG, then Perplexity; the
/// public chain runs only with neither configured.
fn search_with(
    query: &str,
    limit: usize,
    searxng_base: Option<&str>,
    perplexity_key: Option<&str>,
    fetch: &dyn Fn(&FetchRequest) -> Result<String, String>,
) -> Vec<Hit> {
    let hits = match searxng_base {
        Some(base) => searxng_hits(query, base, fetch),
        None => match perplexity_key {
            Some(key) => perplexity_hits(query, key, limit, fetch),
            None => public_hits(query, limit, fetch),
        },
    };
    dedupe_and_cap(hits, limit)
}

/// One query to the configured SearXNG; a fetch error yields no hits.
fn searxng_hits(
    query: &str,
    base: &str,
    fetch: &dyn Fn(&FetchRequest) -> Result<String, String>,
) -> Vec<Hit> {
    let url = format!(
        "{}/search?q={}&format=json",
        base.trim_end_matches('/'),
        url_encode(query)
    );
    fetch(&FetchRequest {
        url,
        post_body: None,
        authorization: None,
    })
    .map_or_else(|_| Vec::new(), |body| parse_searxng(&body))
}

/// One bounded POST to the Perplexity `/search` endpoint: the query and the
/// result cap in the JSON body, the key in the Authorization header, results
/// normalized from `search_results[]`. A fetch error yields no hits — a
/// failing key is not topped up by the public chain.
fn perplexity_hits(
    query: &str,
    key: &str,
    limit: usize,
    fetch: &dyn Fn(&FetchRequest) -> Result<String, String>,
) -> Vec<Hit> {
    let request = FetchRequest {
        url: PERPLEXITY_SEARCH_URL.to_string(),
        post_body: Some(json!({ "query": query, "max_results": limit }).to_string()),
        authorization: Some(format!("Bearer {key}")),
    };
    fetch(&request).map_or_else(|_| Vec::new(), |body| parse_perplexity(&body))
}

/// The free public chain, used only when no provider is configured:
/// DuckDuckGo, then Wikipedia while the hits are still under `limit`.
fn public_hits(
    query: &str,
    limit: usize,
    fetch: &dyn Fn(&FetchRequest) -> Result<String, String>,
) -> Vec<Hit> {
    let mut hits = Vec::new();
    if hits.len() < limit {
        let url = format!(
            "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1",
            url_encode(query)
        );
        if let Ok(body) = fetch(&FetchRequest {
            url,
            post_body: None,
            authorization: None,
        }) {
            hits.extend(parse_duckduckgo(&body));
        }
    }
    if hits.len() < limit {
        let url = format!(
            "https://en.wikipedia.org/w/api.php?action=opensearch&search={}&limit={}&namespace=0&format=json",
            url_encode(query),
            limit
        );
        if let Ok(body) = fetch(&FetchRequest {
            url,
            post_body: None,
            authorization: None,
        }) {
            hits.extend(parse_wikipedia(&body));
        }
    }
    hits
}

/// One bounded request, body to string. The only place the network is
/// touched; the timeout and body cap apply to every provider alike.
#[cfg_attr(test, mutants::skip)] // thin adapter over ureq; parsing is tested on bodies
fn fetch(request: &FetchRequest) -> Result<String, String> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .user_agent("pixel-cli web-search")
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut response = match (&request.post_body, &request.authorization) {
        (Some(body), authorization) => {
            let builder = agent
                .post(&request.url)
                .header("Content-Type", "application/json");
            let builder = match authorization {
                Some(token) => builder.header("Authorization", token.as_str()),
                None => builder,
            };
            builder.send(body.clone())
        }
        (None, authorization) => {
            let builder = agent.get(&request.url);
            let builder = match authorization {
                Some(token) => builder.header("Authorization", token.as_str()),
                None => builder,
            };
            builder.call()
        }
    }
    .map_err(|e| format!("web-search fetch {}: {e}", request.url))?;
    response
        .body_mut()
        .with_config()
        .limit(BODY_CAP_BYTES)
        .read_to_string()
        .map_err(|e| format!("web-search read {}: {e}", request.url))
}

/// SearXNG `/search?format=json`: `results[]` with title/url/content/engine.
fn parse_searxng(body: &str) -> Vec<Hit> {
    let Ok(data) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    data.get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let url = r.get("url")?.as_str()?;
            if url.is_empty() {
                return None;
            }
            Some(Hit {
                title: r
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or(url)
                    .to_string(),
                url: url.to_string(),
                snippet: clip(r.get("content").and_then(Value::as_str).unwrap_or("")),
                engine: "searxng",
            })
        })
        .collect()
}

/// Perplexity `/search`: `search_results[]` with title/url/snippet. Some
/// responses carry the body under `content` instead of `snippet`.
fn parse_perplexity(body: &str) -> Vec<Hit> {
    let Ok(data) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    data.get("search_results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let url = r.get("url")?.as_str()?;
            if url.is_empty() {
                return None;
            }
            let snippet = r
                .get("snippet")
                .and_then(Value::as_str)
                .or_else(|| r.get("content").and_then(Value::as_str))
                .unwrap_or("");
            Some(Hit {
                title: r
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or(url)
                    .to_string(),
                url: url.to_string(),
                snippet: clip(snippet),
                engine: "perplexity",
            })
        })
        .collect()
}

/// DuckDuckGo Instant Answer: `Abstract*` plus `RelatedTopics[]` (which may
/// nest one level under `Topics`).
fn parse_duckduckgo(body: &str) -> Vec<Hit> {
    let Ok(data) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    if let (Some(text), Some(url)) = (
        data.get("AbstractText").and_then(Value::as_str),
        data.get("AbstractURL").and_then(Value::as_str),
    ) && !text.is_empty()
        && !url.is_empty()
    {
        hits.push(Hit {
            title: data
                .get("Heading")
                .and_then(Value::as_str)
                .unwrap_or(url)
                .to_string(),
            url: url.to_string(),
            snippet: clip(text),
            engine: "duckduckgo",
        });
    }
    collect_ddg_topics(
        data.get("RelatedTopics").and_then(Value::as_array),
        &mut hits,
    );
    hits
}

fn collect_ddg_topics(topics: Option<&Vec<Value>>, hits: &mut Vec<Hit>) {
    let Some(topics) = topics else { return };
    for topic in topics {
        if let Some(nested) = topic.get("Topics").and_then(Value::as_array) {
            for t in nested {
                push_ddg_topic(t, hits);
            }
        } else {
            push_ddg_topic(topic, hits);
        }
    }
}

fn push_ddg_topic(topic: &Value, hits: &mut Vec<Hit>) {
    let (Some(text), Some(url)) = (
        topic.get("Text").and_then(Value::as_str),
        topic.get("FirstURL").and_then(Value::as_str),
    ) else {
        return;
    };
    if text.is_empty() || url.is_empty() {
        return;
    }
    hits.push(Hit {
        title: clip_len(text.split(" - ").next().unwrap_or(text), 120),
        url: url.to_string(),
        snippet: clip(text),
        engine: "duckduckgo",
    });
}

/// Wikipedia OpenSearch: `[term, [titles], [descriptions], [urls]]`.
fn parse_wikipedia(body: &str) -> Vec<Hit> {
    let Ok(data) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let Some(rows) = data.as_array() else {
        return Vec::new();
    };
    let titles = rows.get(1).and_then(Value::as_array);
    let descs = rows.get(2).and_then(Value::as_array);
    let urls = rows.get(3).and_then(Value::as_array);
    let (Some(titles), Some(urls)) = (titles, urls) else {
        return Vec::new();
    };
    titles
        .iter()
        .zip(urls.iter())
        .enumerate()
        .filter_map(|(i, (t, u))| {
            let url = u.as_str()?;
            if url.is_empty() {
                return None;
            }
            Some(Hit {
                title: t.as_str().unwrap_or(url).to_string(),
                url: url.to_string(),
                snippet: clip(
                    descs
                        .and_then(|d| d.get(i))
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                ),
                engine: "wikipedia",
            })
        })
        .collect()
}

/// First occurrence wins across providers; the cap applies after dedupe so
/// `limit` always means distinct results.
fn dedupe_and_cap(hits: Vec<Hit>, limit: usize) -> Vec<Hit> {
    let mut seen = std::collections::HashSet::new();
    hits.into_iter()
        .filter(|h| seen.insert(h.url.clone()))
        .take(limit)
        .collect()
}

fn clip(text: &str) -> String {
    clip_len(text, SNIPPET_CAP_CHARS)
}

fn clip_len(text: &str, cap: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= cap {
        return text;
    }
    if cap == 0 {
        return String::new();
    }
    let mut s: String = text.chars().take(cap - 1).collect();
    s.push('…');
    s
}

/// RFC 3986 unreserved set — the only characters passed through verbatim.
fn url_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searxng_results_normalize_and_skip_empty_urls() {
        let body = r#"{"results": [
            {"title": "JEV", "url": "https://example.test/jev", "content": "Joint Embedded Validator"},
            {"url": ""},
            {"title": "No URL"}
        ]}"#;
        let hits = parse_searxng(body);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "JEV");
        assert_eq!(hits[0].snippet, "Joint Embedded Validator");
        assert_eq!(hits[0].engine, "searxng");
    }

    #[test]
    fn duckduckgo_abstract_and_nested_topics() {
        let body = r#"{
            "Heading": "JEV",
            "AbstractText": "A thing.",
            "AbstractURL": "https://example.test/a",
            "RelatedTopics": [
                {"Text": "One - first", "FirstURL": "https://example.test/1"},
                {"Topics": [{"Text": "Two - second", "FirstURL": "https://example.test/2"}]},
                {"Text": "", "FirstURL": "https://example.test/skip"}
            ]
        }"#;
        let hits = parse_duckduckgo(body);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].title, "JEV");
        assert_eq!(hits[0].snippet, "A thing.");
        assert_eq!(hits[2].url, "https://example.test/2");
        assert_eq!(hits[2].title, "Two");
    }

    #[test]
    fn wikipedia_rows_zip_titles_descriptions_urls() {
        let body = r#"["jev", ["JEV", "JEV (band)"], ["Joint Embedded Validator", ""],
            ["https://en.wikipedia.org/wiki/JEV", "https://en.wikipedia.org/wiki/JEV_(band)"]]"#;
        let hits = parse_wikipedia(body);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "JEV");
        assert_eq!(hits[0].snippet, "Joint Embedded Validator");
        assert_eq!(hits[1].snippet, "");
    }

    #[test]
    fn malformed_bodies_yield_no_hits() {
        assert!(parse_searxng("not json").is_empty());
        assert!(parse_duckduckgo("{}").is_empty());
        assert!(parse_wikipedia("[\"jev\"]").is_empty());
        assert!(parse_perplexity("not json").is_empty());
        assert!(parse_perplexity("{\"missing\": []}").is_empty());
    }

    #[test]
    fn dedupe_keeps_first_and_cap_applies_after() {
        let hit = |url: &str| Hit {
            title: url.into(),
            url: url.into(),
            snippet: String::new(),
            engine: "duckduckgo",
        };
        let hits = vec![hit("a"), hit("a"), hit("b"), hit("c")];
        let out = dedupe_and_cap(hits, 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].url, "a");
        assert_eq!(out[1].url, "b");
    }

    #[test]
    fn clip_collapses_whitespace_and_marks_truncation() {
        assert_eq!(clip("a  b\n c"), "a b c");
        let long = "x".repeat(SNIPPET_CAP_CHARS + 10);
        let clipped = clip(&long);
        assert!(clipped.ends_with('…'));
        assert_eq!(
            clipped.chars().count(),
            SNIPPET_CAP_CHARS,
            "the ellipsis lives inside the cap"
        );
        let exact = "y".repeat(SNIPPET_CAP_CHARS);
        assert_eq!(clip(&exact).chars().count(), SNIPPET_CAP_CHARS);
        assert_eq!(clip_len("anything", 0), "");
    }

    #[test]
    fn url_encode_percent_encodes_only_non_unreserved() {
        assert_eq!(url_encode("a b&c"), "a%20b%26c");
        assert_eq!(url_encode("JEV-2.0_~"), "JEV-2.0_~");
        assert_eq!(url_encode("été"), "%C3%A9t%C3%A9");
    }

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

    #[test]
    fn marker_mirrors_whether_any_hit_survived() {
        assert_eq!(marker(&[]), "unresolved");
        assert_eq!(
            marker(&[Hit {
                title: "t".into(),
                url: "u".into(),
                snippet: String::new(),
                engine: "wikipedia",
            }]),
            "complete"
        );
    }

    #[test]
    fn render_lists_hits_and_omits_empty_snippets() {
        let hits = vec![
            Hit {
                title: "One".into(),
                url: "https://a.test".into(),
                snippet: "snippet one".into(),
                engine: "searxng",
            },
            Hit {
                title: "Two".into(),
                url: "https://b.test".into(),
                snippet: String::new(),
                engine: "wikipedia",
            },
        ];
        assert_eq!(
            render("jev", &hits),
            "1. One [searxng]\n   https://a.test\n   snippet one\n\
             2. Two [wikipedia]\n   https://b.test\n"
        );
        assert_eq!(
            render("jev", &[]),
            "unresolved: no web results for \"jev\"\n"
        );
    }

    #[test]
    fn document_carries_marker_epistemics_and_provider_snapshot() {
        let hits = vec![
            Hit {
                title: "a".into(),
                url: "https://a.test".into(),
                snippet: String::new(),
                engine: "searxng",
            },
            Hit {
                title: "b".into(),
                url: "https://b.test".into(),
                snippet: String::new(),
                engine: "searxng",
            },
            Hit {
                title: "c".into(),
                url: "https://c.test".into(),
                snippet: String::new(),
                engine: "wikipedia",
            },
        ];
        let doc = document("jev", 8, &hits);
        assert_eq!(doc["marker"], "complete");
        assert_eq!(doc["hits"].as_array().unwrap().len(), 3);
        assert_eq!(doc["epistemics"]["closed_world"], false);
        assert_eq!(doc["epistemics"]["lower_bound"], true);
        assert_eq!(doc["epistemics"]["basis"], "web search");
        assert_eq!(doc["epistemics"]["confidence"], "complete");
        // Providers are deduplicated and sorted: searxng before wikipedia.
        assert_eq!(
            doc["snapshot"]["providers"],
            json!(["searxng", "wikipedia"])
        );
        assert_eq!(doc["snapshot"]["limit"], 8);

        let empty = document("jev", 8, &[]);
        assert_eq!(empty["marker"], "unresolved");
        assert_eq!(empty["epistemics"]["confidence"], "unresolved");
        assert_eq!(empty["snapshot"]["providers"], json!([]));
    }

    /// A fetch seam that records every URL and answers by provider:
    /// `searxng`, `ddg`, `wiki` and `perplexity` are the bodies (or errors)
    /// to return.
    fn recording_fetch<'a>(
        calls: &'a std::cell::RefCell<Vec<String>>,
        searxng: Result<&'a str, &'a str>,
        ddg: &'a str,
        wiki: &'a str,
        perplexity: Result<&'a str, &'a str>,
    ) -> impl Fn(&FetchRequest) -> Result<String, String> + 'a {
        move |request: &FetchRequest| {
            calls.borrow_mut().push(request.url.clone());
            if request.url.contains("/search?q=") {
                searxng.map(str::to_string).map_err(str::to_string)
            } else if request.url.contains("duckduckgo") {
                Ok(ddg.to_string())
            } else if request.url.contains("wikipedia") {
                Ok(wiki.to_string())
            } else {
                perplexity.map(str::to_string).map_err(str::to_string)
            }
        }
    }

    const ONE_SEARXNG_HIT: &str =
        r#"{"results":[{"title":"t","url":"https://sx.test/u1","content":"c"}]}"#;
    const ONE_DDG_HIT: &str =
        r#"{"Heading":"D","AbstractText":"d","AbstractURL":"https://ddg.test/a"}"#;
    const ONE_WIKI_HIT: &str = r#"["q",["W"],["w"],["https://wiki.test/w"]]"#;
    const ONE_PERPLEXITY_HIT: &str =
        r#"{"search_results":[{"title":"p","url":"https://pplx.test/u1","snippet":"s"}]}"#;
    const PERPLEXITY_URL: &str = "https://api.perplexity.ai/search";
    const DDG_URL: &str = "https://api.duckduckgo.com/?q=q&format=json&no_html=1&skip_disambig=1";

    fn urls(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|h| h.url.as_str()).collect()
    }

    #[test]
    fn a_configured_searxng_is_the_only_provider_queried() {
        // Under the limit, with public providers and a Perplexity key that
        // would answer: none of them may be asked, the query stays with the
        // user's own instance.
        let calls = std::cell::RefCell::new(Vec::new());
        let fetch = recording_fetch(
            &calls,
            Ok(ONE_SEARXNG_HIT),
            ONE_DDG_HIT,
            ONE_WIKI_HIT,
            Ok(ONE_PERPLEXITY_HIT),
        );
        let hits = search_with("q", 8, Some("https://sx.test/"), Some("sk-pplx"), &fetch);
        assert_eq!(urls(&hits), ["https://sx.test/u1"]);
        assert_eq!(*calls.borrow(), ["https://sx.test/search?q=q&format=json"]);
    }

    #[test]
    fn a_failing_searxng_does_not_fall_back_to_public_providers() {
        // Unreachable or empty instance: the answer is unresolved, not
        // topped up from DuckDuckGo, Wikipedia or Perplexity.
        for searxng in [Err("connection refused"), Ok(r#"{"results":[]}"#)] {
            let calls = std::cell::RefCell::new(Vec::new());
            let fetch = recording_fetch(
                &calls,
                searxng,
                ONE_DDG_HIT,
                ONE_WIKI_HIT,
                Ok(ONE_PERPLEXITY_HIT),
            );
            let hits = search_with("q", 8, Some("https://sx.test"), Some("sk-pplx"), &fetch);
            assert_eq!(hits, Vec::<Hit>::new(), "{searxng:?}");
            assert_eq!(
                *calls.borrow(),
                ["https://sx.test/search?q=q&format=json"],
                "{searxng:?}"
            );
        }
    }

    #[test]
    fn without_searxng_the_public_chain_stops_once_full() {
        let wiki_url = |limit: usize| {
            format!(
                "https://en.wikipedia.org/w/api.php?action=opensearch&search=q&limit={limit}&namespace=0&format=json"
            )
        };
        // One DuckDuckGo hit fills a limit of 1: Wikipedia is not asked.
        let calls = std::cell::RefCell::new(Vec::new());
        let fetch = recording_fetch(
            &calls,
            Err("unused"),
            ONE_DDG_HIT,
            ONE_WIKI_HIT,
            Ok(ONE_PERPLEXITY_HIT),
        );
        assert_eq!(
            urls(&search_with("q", 1, None, None, &fetch)),
            ["https://ddg.test/a"]
        );
        assert_eq!(*calls.borrow(), [DDG_URL]);

        // Under the limit, Wikipedia tops it up, after DuckDuckGo.
        let calls = std::cell::RefCell::new(Vec::new());
        let fetch = recording_fetch(
            &calls,
            Err("unused"),
            ONE_DDG_HIT,
            ONE_WIKI_HIT,
            Ok(ONE_PERPLEXITY_HIT),
        );
        assert_eq!(
            urls(&search_with("q", 8, None, None, &fetch)),
            ["https://ddg.test/a", "https://wiki.test/w"]
        );
        assert_eq!(*calls.borrow(), [DDG_URL.to_string(), wiki_url(8)]);
    }

    #[test]
    fn a_zero_limit_without_searxng_fetches_nothing() {
        let calls = std::cell::RefCell::new(Vec::new());
        let fetch = recording_fetch(
            &calls,
            Err("unused"),
            ONE_DDG_HIT,
            ONE_WIKI_HIT,
            Ok(ONE_PERPLEXITY_HIT),
        );
        assert_eq!(search_with("q", 0, None, None, &fetch), Vec::<Hit>::new());
        assert_eq!(*calls.borrow(), Vec::<String>::new());
    }

    #[test]
    fn a_configured_perplexity_is_queried_once_with_its_key_and_json_body() {
        // No SearXNG, key present: exactly one keyed POST to Perplexity, and
        // the public chain is not asked.
        let calls = std::cell::RefCell::new(Vec::new());
        let requests = std::cell::RefCell::new(Vec::new());
        let fetch = |request: &FetchRequest| {
            calls.borrow_mut().push(request.url.clone());
            requests.borrow_mut().push(request.clone());
            Ok(ONE_PERPLEXITY_HIT.to_string())
        };
        let hits = search_with("q", 3, None, Some("sk-pplx-secret"), &fetch);
        assert_eq!(urls(&hits), ["https://pplx.test/u1"]);
        assert_eq!(hits[0].engine, "perplexity");
        assert_eq!(*calls.borrow(), [PERPLEXITY_URL]);
        let recorded = requests.borrow();
        let request = &recorded[0];
        assert_eq!(request.url, PERPLEXITY_URL);
        assert_eq!(
            request.authorization,
            Some("Bearer sk-pplx-secret".to_string())
        );
        let body: Value = serde_json::from_str(request.post_body.as_deref().unwrap()).unwrap();
        assert_eq!(body["query"], "q");
        assert_eq!(body["max_results"], 3);
        // The key must never ride in the URL or leak into a body echo.
        assert!(!request.url.contains("sk-pplx-secret"));
    }

    #[test]
    fn a_failing_perplexity_does_not_fall_back_to_public_providers() {
        // A dead key or an empty page: the answer is unresolved, not topped
        // up from DuckDuckGo or Wikipedia.
        for perplexity in [Err("401 unauthorized"), Ok(r#"{"search_results":[]}"#)] {
            let calls = std::cell::RefCell::new(Vec::new());
            let fetch =
                recording_fetch(&calls, Err("unused"), ONE_DDG_HIT, ONE_WIKI_HIT, perplexity);
            let hits = search_with("q", 8, None, Some("sk-pplx"), &fetch);
            assert_eq!(hits, Vec::<Hit>::new(), "{perplexity:?}");
            assert_eq!(*calls.borrow(), [PERPLEXITY_URL], "{perplexity:?}");
        }
    }

    #[test]
    fn perplexity_results_normalize_and_skip_empty_urls() {
        let body = r#"{"search_results": [
            {"title": "JEV", "url": "https://pplx.test/jev", "snippet": "Joint Embedded Validator"},
            {"url": ""},
            {"title": "No URL"},
            {"title": "C", "url": "https://pplx.test/c", "content": "body under content"}
        ]}"#;
        let hits = parse_perplexity(body);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "JEV");
        assert_eq!(hits[0].snippet, "Joint Embedded Validator");
        assert_eq!(hits[0].engine, "perplexity");
        assert_eq!(hits[1].snippet, "body under content");
        assert_eq!(hits[1].title, "C");
    }
}
