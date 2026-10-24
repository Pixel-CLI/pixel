// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pixel search-meaning` (`specs/semantic.md`): a natural-language question
//! ranked over the code chunks the daemon holds embedded.
//!
//! The daemon builds its pool on a background thread and answers `warming`
//! until it is ready; this command waits for it, a bounded time, instead of
//! handing the caller an empty answer. `--download` fetches the embedding
//! model once, printing where from and where to before any byte moves; no
//! other path ever touches the network.

use std::path::Path;
use std::time::{Duration, Instant};

use pixel_daemon::{Request, Service};
use serde_json::Value;

use crate::client;
use crate::commands::Done;

/// How long a question waits for the pool to be built.
pub const WAIT: Duration = Duration::from_secs(60);

/// The pause between two asks while the pool is being built.
const POLL: Duration = Duration::from_millis(200);

/// What the daemon says while it is still building.
fn building(status: &str, reason: Option<&str>) -> bool {
    status == "unavailable" && matches!(reason, Some("warming" | "refreshing" | "cold"))
}

/// Asks `ask` until it answers something other than "still building" or
/// `wait` passes. Returns the last answer.
fn poll(wait: Duration, mut ask: impl FnMut() -> Result<Value, String>) -> Result<Value, String> {
    let deadline = Instant::now() + wait;
    loop {
        let value = ask()?;
        let status = value.get("status").and_then(Value::as_str).unwrap_or("");
        let reason = value.get("reason").and_then(Value::as_str);
        if !building(status, reason) || Instant::now() >= deadline {
            return Ok(value);
        }
        std::thread::sleep(POLL);
    }
}

pub fn run(
    query: &str,
    path: &Path,
    limit: Option<usize>,
    download: bool,
    json: bool,
    no_daemon: bool,
) -> Result<Done, String> {
    if download {
        return fetch_model(json);
    }
    let root = client::discover_root(path)?;
    let request = || Request::Meaning {
        stale_ok: false,
        query: query.to_owned(),
        limit,
    };
    let value = if no_daemon || client::probe(&root) == client::Probe::Absent {
        // Nothing keeps a pool between two in-process asks, so one service
        // answers all of them.
        let mut service = Service::open(&root).map_err(|e| e.to_string())?;
        poll(WAIT, || client::unwrap_response(service.handle(request())))?
    } else {
        poll(WAIT, || {
            client::unwrap_response(client::execute(&root, request(), false)?)
        })?
    };
    let text = render(&value);
    let mut done = Done::new(value.clone(), text, json);
    if value.get("status").and_then(Value::as_str) != Some("ready") {
        done.exit = 1;
    }
    Ok(done)
}

/// What a person reads.
pub fn render(value: &Value) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if value.get("status").and_then(Value::as_str) != Some("ready") {
        let reason = value
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let _ = write!(out, "semantic search unavailable: {reason}");
        if let Some(detail) = value.get("detail").and_then(Value::as_str) {
            let _ = write!(out, " ({detail})");
        }
        out.push('\n');
        if matches!(reason, "warming" | "refreshing" | "cold") {
            out.push_str("the pool is still being built; ask again in a moment\n");
        }
        return out;
    }
    let hits = value
        .get("hits")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    if hits.is_empty() {
        out.push_str("no leads\n");
    }
    for hit in hits {
        let path = hit.get("path").and_then(Value::as_str).unwrap_or("");
        let start = hit.get("start_line").and_then(Value::as_u64).unwrap_or(0);
        let end = hit.get("end_line").and_then(Value::as_u64).unwrap_or(0);
        let _ = write!(out, "{path}:{start}-{end}");
        if let Some(symbol) = hit.get("symbol").and_then(Value::as_str) {
            let _ = write!(out, "  {symbol}");
        }
        out.push('\n');
        if let Some(snippet) = hit
            .get("snippet")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            let _ = writeln!(out, "    {snippet}");
        }
    }
    for cap in value
        .get("caps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let _ = writeln!(out, "note: {cap}");
    }
    out
}

#[cfg(not(feature = "semantic"))]
fn fetch_model(_json: bool) -> Result<Done, String> {
    Err("this build has no semantic search (built without the `semantic` feature), so there is no model to download".to_owned())
}

/// The Hugging Face revision fetched: the repository's default branch, since
/// the model is not pinned by digest. Printed before the download.
#[cfg(feature = "semantic")]
const REVISION: &str = "main";

/// No single model file is larger than this.
#[cfg(feature = "semantic")]
const FILE_CAP: u64 = 256 * 1024 * 1024;

#[cfg(feature = "semantic")]
fn fetch_model(json: bool) -> Result<Done, String> {
    use pixel_index::semantic::embed::{CODE_MODEL_REPO, COMPLETE_MARKER, MODEL_FILES, model_dir};
    let cache = pixel_index::cache::cache_root().ok_or("no cache directory (HOME is not set)")?;
    let dir = model_dir(&cache, CODE_MODEL_REPO);
    eprintln!(
        "pixel: downloading embedding model {CODE_MODEL_REPO} (revision {REVISION}, unpinned) from https://huggingface.co into {}",
        dir.display()
    );
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut sizes = serde_json::Map::new();
    for name in MODEL_FILES {
        let url = format!("https://huggingface.co/{CODE_MODEL_REPO}/resolve/{REVISION}/{name}");
        let bytes = download(&url, &dir.join(name))?;
        sizes.insert(name.to_owned(), serde_json::json!(bytes));
    }
    // The marker comes last: a directory without it is never read as a model.
    std::fs::write(
        dir.join(COMPLETE_MARKER),
        serde_json::to_vec(&sizes).unwrap_or_default(),
    )
    .map_err(|e| e.to_string())?;
    let value = serde_json::json!({"model": CODE_MODEL_REPO, "revision": REVISION, "dir": dir.display().to_string(), "files": sizes});
    let text = format!("downloaded {CODE_MODEL_REPO} into {}\n", dir.display());
    Ok(Done::new(value, text, json))
}

/// Streams `url` into `dest` through a `.part` file renamed when whole.
#[cfg(feature = "semantic")]
fn download(url: &str, dest: &Path) -> Result<u64, String> {
    use std::io::Write as _;
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(600)))
            .build(),
    );
    let mut response = agent.get(url).call().map_err(|e| format!("{url}: {e}"))?;
    let mut reader = response.body_mut().with_config().limit(FILE_CAP).reader();
    let part = dest.with_extension("part");
    let mut file = std::fs::File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?;
    let bytes = std::io::copy(&mut reader, &mut file).map_err(|e| format!("{url}: {e}"))?;
    file.flush().map_err(|e| e.to_string())?;
    drop(file);
    std::fs::rename(&part, dest).map_err(|e| e.to_string())?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn poll_should_stop_at_the_first_answer_that_is_not_still_building() {
        let mut calls = 0;
        let value = poll(Duration::from_secs(5), || {
            calls += 1;
            Ok(if calls < 3 {
                json!({"status": "unavailable", "reason": "warming"})
            } else {
                json!({"status": "ready", "hits": []})
            })
        })
        .unwrap();
        assert_eq!((calls, value["status"].as_str()), (3, Some("ready")));
    }

    #[test]
    fn poll_should_give_up_when_the_wait_is_over_and_return_the_last_answer() {
        let mut calls = 0;
        let value = poll(Duration::ZERO, || {
            calls += 1;
            Ok(json!({"status": "unavailable", "reason": "refreshing"}))
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(value["reason"], "refreshing");
    }

    #[test]
    fn poll_should_not_wait_for_a_failure_or_a_missing_model() {
        for reason in ["failed", "model_missing", "stale"] {
            let mut calls = 0;
            let _ = poll(Duration::from_secs(5), || {
                calls += 1;
                Ok(json!({"status": "unavailable", "reason": reason}))
            });
            assert_eq!(calls, 1, "{reason}");
        }
        assert!(poll(Duration::from_secs(1), || Err("boom".to_owned())).is_err());
    }

    #[test]
    fn render_should_list_leads_with_their_lines_symbol_and_snippet() {
        let value = json!({
            "status": "ready",
            "hits": [
                {"path": "src/retry.rs", "start_line": 2, "end_line": 4, "symbol": "retry_limit", "score": 0.03, "snippet": "/// Reads the retry limit"},
                {"path": "docs/guide.md", "start_line": 1, "end_line": 9, "score": 0.01, "snippet": ""}
            ],
            "pool": {},
            "caps": ["leads are unverified"]
        });
        assert_eq!(
            render(&value),
            "src/retry.rs:2-4  retry_limit\n    /// Reads the retry limit\ndocs/guide.md:1-9\nnote: leads are unverified\n"
        );
        assert_eq!(
            render(&json!({"status": "ready", "hits": [], "caps": []})),
            "no leads\n"
        );
    }

    #[test]
    fn render_should_say_why_it_is_unavailable() {
        let value =
            json!({"status": "unavailable", "reason": "model_missing", "detail": "run --download"});
        assert_eq!(
            render(&value),
            "semantic search unavailable: model_missing (run --download)\n"
        );
        let warming = render(&json!({"status": "unavailable", "reason": "warming"}));
        assert!(warming.starts_with("semantic search unavailable: warming\n"));
        assert!(warming.contains("ask again in a moment"));
    }

    #[cfg(not(feature = "semantic"))]
    #[test]
    fn download_should_say_the_build_cannot_embed() {
        let error = fetch_model(false).err().unwrap();
        assert!(error.contains("no semantic search"), "{error}");
    }
}
