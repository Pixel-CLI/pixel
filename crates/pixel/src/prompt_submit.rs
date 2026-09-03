//! `pixel hook prompt-submit` — task boundary detector.
//!
//! Fires on every `UserPromptSubmit` hook event. Embeds the new prompt and
//! the recent conversation context (last N assistant turns from the recall
//! corpus for this cwd), computes cosine similarity, and checks the action
//! log for recent completion signals (commits/publishes). If both a topic
//! shift (low similarity) and a completion signal are present, emits a
//! `[PIXEL:TASK_BOUNDARY]` advisory into the conversation via
//! `additionalContext` — the always-on rule then guides the agent to
//! summarize the previous task and mentally reset.
//!
//! Never blocks the user's prompt: a hard 500ms deadline means any slow
//! path (model load, store open, embedding) is abandoned and the hook
//! exits 0 with no output.

use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

/// Cosine similarity below this + completion signal → task boundary (strong).
const SIMILARITY_THRESHOLD: f32 = 0.45;
/// Cosine similarity below this even without completion → task boundary (weak).
const WEAK_THRESHOLD: f32 = 0.35;
/// How far back to look for completion signals in actions.jsonl (seconds).
const COMPLETION_LOOKBACK_SECS: i64 = 300;
/// Number of recent assistant turns to use as context.
const CONTEXT_TURNS: usize = 5;
/// Hard deadline for the entire hook — never block the user's prompt.
const HOOK_DEADLINE: Duration = Duration::from_millis(500);

/// Commands in actions.jsonl that signal task completion.
const COMPLETION_COMMANDS: &[&str] = &["publish", "ship", "push", "commit"];

/// The UserPromptSubmit hook payload (Claude Code shape).
/// Other CLIs may send different fields; we only need prompt + cwd.
#[derive(Deserialize)]
struct PromptSubmitPayload {
    prompt: String,
    #[serde(default)]
    cwd: Option<String>,
}

/// Entry point for `pixel hook prompt-submit`. Reads the UserPromptSubmit
/// payload from stdin. Never returns an `Err` as exit 1 — every failure
/// path is a silent exit 0 (prompt proceeds normally).
pub fn run() -> ! {
    // Allow opt-out via env var.
    if let Ok(kill) = std::env::var("PIXEL_TASK_BOUNDARY") {
        if matches!(kill.as_str(), "0" | "false" | "off") {
            std::process::exit(0);
        }
    }

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() || input.trim().is_empty() {
        std::process::exit(0);
    }
    let Ok(payload) = serde_json::from_str::<PromptSubmitPayload>(&input) else {
        std::process::exit(0);
    };

    // Short prompts like "yes", "ok", "continue" are almost certainly
    // continuations — skip embedding entirely.
    if is_trivial_continuation(&payload.prompt) {
        std::process::exit(0);
    }

    let cwd = payload
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let deadline = Instant::now() + HOOK_DEADLINE;

    // Run detection in a thread with a channel so we can enforce the hard
    // deadline. Using join() would block until the thread finishes — which
    // could be seconds if the model is loading — defeating the deadline.
    let (tx, rx) = std::sync::mpsc::channel();
    let prompt = payload.prompt.clone();
    let cwd_clone = cwd.clone();
    std::thread::spawn(move || {
        let result = detect_boundary(&prompt, &cwd_clone);
        let _ = tx.send(result);
    });

    match rx.recv_timeout(HOOK_DEADLINE) {
        Ok(Ok(Some(boundary))) if Instant::now() <= deadline => {
            emit_boundary(&boundary);
        }
        _ => std::process::exit(0),
    }
}

/// The boundary event to emit.
struct BoundaryEvent {
    similarity: f32,
    completion_signal: bool,
    context_summary: String,
}

/// Core detection logic: embed prompt + context, compute similarity, check
/// completion signals. Returns `Some(BoundaryEvent)` if a task boundary is
/// detected, `None` otherwise.
fn detect_boundary(prompt: &str, cwd: &PathBuf) -> Result<Option<BoundaryEvent>, String> {
    // 1. Open embedder (download=false — fail fast if model not cached).
    let mut embedder = pixel_recall::embed::open_default_embedder(false)?;

    // 2. Get recent assistant turns from the recall corpus for this cwd.
    let context_text = recent_context_text(cwd, CONTEXT_TURNS);
    if context_text.is_empty() {
        // No prior context — can't detect a boundary.
        return Ok(None);
    }

    // 3. Embed prompt and context.
    let prompt_text = embed_text_for_prompt(prompt, cwd);
    let texts = [prompt_text.as_str(), context_text.as_str()];
    let vecs = embedder.embed_batch(&texts, pixel_recall::embed::EmbedKind::Query)?;
    if vecs.len() != 2 {
        return Ok(None);
    }
    let similarity = cosine_similarity(&vecs[0], &vecs[1]);

    // 4. Check actions.jsonl for recent completion signals.
    let completion = recent_completion_signal(cwd);

    // 5. Decision logic.
    let is_boundary = if similarity < SIMILARITY_THRESHOLD && completion {
        true
    } else if similarity < WEAK_THRESHOLD {
        true
    } else {
        false
    };

    if !is_boundary {
        return Ok(None);
    }

    // Build a short context summary from the first 200 chars of context.
    let context_summary = context_text.chars().take(200).collect::<String>();

    Ok(Some(BoundaryEvent {
        similarity,
        completion_signal: completion,
        context_summary,
    }))
}

/// Retrieve the last N assistant turns from the recall corpus for the given
/// cwd. Returns concatenated text suitable for embedding.
fn recent_context_text(cwd: &PathBuf, n: usize) -> String {
    let db_path = pixel_recall::db_path();
    let Ok(store) = pixel_recall::store::RecallStore::open(&db_path) else {
        return String::new();
    };

    let cwd_str = cwd.display().to_string();
    // Find the most recent session matching this cwd.
    let Ok(sessions) = store.sessions(None, Some(&cwd_str), None, None, false, 1) else {
        return String::new();
    };
    let Some(session) = sessions.first() else {
        return String::new();
    };

    // Get turns for that session, take the last N assistant turns.
    let Ok(turns) = store.turns_for_session(session.id, None) else {
        return String::new();
    };

    let assistant_texts: Vec<String> = turns
        .iter()
        .rev()
        .filter(|t| t.role == "assistant")
        .take(n)
        .map(|t| t.text.clone())
        .collect();

    if assistant_texts.is_empty() {
        return String::new();
    }

    // Concatenate in chronological order (we reversed for take, so reverse back).
    assistant_texts.into_iter().rev().collect::<Vec<_>>().join("\n")
}

/// Format the prompt text for embedding, matching the recall corpus's
/// `embed_text` convention so similarity is comparable.
fn embed_text_for_prompt(prompt: &str, cwd: &PathBuf) -> String {
    let repo = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("-");
    format!("[prompt] [{repo}] user: {prompt}")
}

/// Check `~/.pixel/actions.jsonl` for recent completion signals (commits,
/// publishes, pushes) in the given cwd within the lookback window.
fn recent_completion_signal(cwd: &PathBuf) -> bool {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let log_path = PathBuf::from(&home).join(".pixel").join("actions.jsonl");
    let Ok(content) = std::fs::read_to_string(&log_path) else {
        return false;
    };

    let now_ms = pixel_actionlog::now_ms();
    let cutoff = now_ms - (COMPLETION_LOOKBACK_SECS * 1000);
    let cwd_str = cwd.display().to_string();

    for line in content.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(ts) = v.get("ts_ms").and_then(Value::as_i64) else {
            continue;
        };
        if ts < cutoff {
            break; // lines are roughly chronological, older entries past this
        }
        let Some(command) = v.get("command").and_then(Value::as_str) else {
            continue;
        };
        let Some(log_cwd) = v.get("cwd").and_then(Value::as_str) else {
            continue;
        };
        let outcome = v.get("outcome").and_then(Value::as_str).unwrap_or("");
        if outcome != "ok" {
            continue;
        }
        if !cwd_matches(&cwd_str, log_cwd) {
            continue;
        }
        if COMPLETION_COMMANDS.contains(&command) {
            return true;
        }
    }
    false
}

/// Check if two cwd paths refer to the same project (exact match or one
/// is a parent of the other).
fn cwd_matches(a: &str, b: &str) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}

/// Trivial continuations that are almost certainly not new tasks.
fn is_trivial_continuation(prompt: &str) -> bool {
    let trimmed = prompt.trim().to_lowercase();
    let words = trimmed.split_whitespace().count();
    if words == 0 {
        return true;
    }
    // Single-word or very short responses.
    matches!(
        trimmed.as_str(),
        "yes" | "y" | "no" | "n" | "ok" | "okay" | "continue" | "go" | "proceed"
            | "thanks" | "done" | "next" | "sure" | "correct" | "right" | "exactly"
            | "yep" | "yeah" | "nope" | "fine" | "good" | "great" | "perfect"
    ) && words <= 2
}

/// Cosine similarity between two vectors.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum::<f32>();
    let norm_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

/// Emit the boundary advisory JSON. The `additionalContext` field is
/// injected into the conversation by Claude Code's hook system.
fn emit_boundary(event: &BoundaryEvent) -> ! {
    let signal = if event.completion_signal {
        "completion detected"
    } else {
        "topic shift"
    };
    let note = format!(
        "[PIXEL:TASK_BOUNDARY] Task boundary detected ({signal}, similarity {sim:.2}). \
         Previous task context: {summary}…\n\
         Mentally reset: summarize what was accomplished, then treat the new prompt as a fresh task.",
        sim = event.similarity,
        summary = event.context_summary.chars().take(150).collect::<String>(),
    );
    let json = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": note
        }
    });
    print!("{}", serde_json::to_string(&json).unwrap_or_default());
    std::process::exit(0);
}

/// Write the boundary event to `~/.pixel/inbox/task-boundary.json` for
/// downstream consumers (daemons, other tools).
#[allow(dead_code)]
fn write_boundary_file(event: &BoundaryEvent) {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let inbox = PathBuf::from(&home).join(".pixel").join("inbox");
    let _ = std::fs::create_dir_all(&inbox);
    let path = inbox.join("task-boundary.json");
    let ts = pixel_actionlog::now_ms();
    let json = serde_json::json!({
        "ts_ms": ts,
        "similarity": event.similarity,
        "completion_signal": event.completion_signal,
        "context_summary": event.context_summary,
    });
    let _ = std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_identity() {
        let v = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 0.001);
    }

    #[test]
    fn cosine_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 0.001);
    }

    #[test]
    fn cosine_opposite() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim + 1.0).abs() < 0.001);
    }

    #[test]
    fn cosine_empty() {
        let sim = cosine_similarity(&[], &[]);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn cosine_different_lengths() {
        let sim = cosine_similarity(&[1.0], &[1.0, 2.0]);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn trivial_continuations_detected() {
        assert!(is_trivial_continuation("yes"));
        assert!(is_trivial_continuation("OK"));
        assert!(is_trivial_continuation("  continue  "));
        assert!(is_trivial_continuation("thanks"));
        assert!(is_trivial_continuation(""));
    }

    #[test]
    fn non_trivial_prompts_not_flagged() {
        assert!(!is_trivial_continuation("now let's set up docker"));
        assert!(!is_trivial_continuation("fix the login bug"));
        assert!(!is_trivial_continuation("can you also add tests for the auth module"));
    }

    #[test]
    fn cwd_exact_match() {
        assert!(cwd_matches("/tmp/foo", "/tmp/foo"));
    }

    #[test]
    fn cwd_parent_child() {
        assert!(cwd_matches("/tmp/foo", "/tmp/foo/bar"));
        assert!(cwd_matches("/tmp/foo/bar", "/tmp/foo"));
    }

    #[test]
    fn cwd_no_match() {
        assert!(!cwd_matches("/tmp/foo", "/tmp/baz"));
    }
}
