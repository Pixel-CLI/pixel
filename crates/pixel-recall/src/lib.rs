// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! pixel-recall — machine-wide LLM transcript retrieval corpus.
//!
//! Ingests every parseable CLI transcript store (Claude Code, Codex,
//! opencode, Devin, Cursor CLI, zcode, Gemini history, pi) into one SQLite
//! corpus of turn-granular text, then serves lexical (trigram) and semantic
//! (embedding) retrieval over it. Unlike the repo index, this corpus is
//! global: transcripts belong to the machine, not to a repository.

pub mod ask;
pub mod code_chunks;
pub mod code_search;
pub mod code_vectors;
pub mod embed;
pub mod export;
pub mod hybrid;
pub mod ingest;
pub mod intent;
pub mod model;
pub mod search;
pub mod segment;
pub mod sources;
pub mod store;
pub mod vector;

use std::path::{Path, PathBuf};

/// Corpus root: `$PIXEL_RECALL_DIR`, else `~/.local/share/pixel/recall`.
/// Falls back to the legacy `~/.local/share/gitpixel/recall` path if it
/// already exists (migration compat). Created on demand with owner-only
/// permissions — this directory concentrates every transcript on the machine.
pub fn recall_dir() -> PathBuf {
    recall_dir_from(
        std::env::var("PIXEL_RECALL_DIR").ok(),
        std::env::var("HOME").ok(),
    )
}

/// [`recall_dir`] over explicit `PIXEL_RECALL_DIR` and `HOME` values.
fn recall_dir_from(override_dir: Option<String>, home: Option<String>) -> PathBuf {
    if let Some(dir) = override_dir
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    let home = home.unwrap_or_else(|| ".".to_string());
    let new_path = PathBuf::from(&home).join(".local/share/pixel/recall");
    // Migration: if the new path doesn't exist but the legacy gitpixel path
    // does, keep using the legacy path so existing users don't lose their
    // corpus. New users get the new path.
    let legacy_path = PathBuf::from(&home).join(".local/share/gitpixel/recall");
    if !new_path.exists() && legacy_path.exists() {
        return legacy_path;
    }
    new_path
}

/// Ensure the corpus root exists with mode 0700.
pub fn ensure_recall_dir() -> std::io::Result<PathBuf> {
    let dir = recall_dir();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

pub fn db_path() -> PathBuf {
    recall_dir().join("recall.db")
}

pub fn segments_dir() -> PathBuf {
    recall_dir().join("segments")
}

pub fn vectors_dir() -> PathBuf {
    recall_dir().join("vectors")
}

/// Embedding model cache — shared across rebuilds, never inside a repo.
pub fn models_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let new_path = PathBuf::from(&home).join(".local/share/pixel/models");
    // Migration: use legacy path if it exists and the new one doesn't.
    let legacy_path = PathBuf::from(&home).join(".local/share/gitpixel/models");
    if !new_path.exists() && legacy_path.exists() {
        return legacy_path;
    }
    new_path
}

/// The marker releases up to 0.4.0 left in [`models_dir`] after any potion
/// download. It names the last repository downloaded, so the models sharing
/// the directory overwrote each other's.
pub const LEGACY_POTION_MARKER: &str = "potion.ok";

/// The marker a finished download of the potion model `repo` leaves under
/// `models`: one per repository, since several share the directory (the
/// daemon's `potion-code-64M-v2`, `search-meaning`'s `potion-code-16M-v2`,
/// transcript recall's multilingual model).
pub fn potion_marker(models: &Path, repo: &str) -> PathBuf {
    models.join(format!("potion-{}.ok", repo.replace('/', "--")))
}

/// Whether the potion model `repo` finished a download under `models`: its
/// own marker, or the legacy shared one while it still names `repo`, so an
/// upgrade does not download it again.
pub fn potion_cached(models: &Path, repo: &str) -> bool {
    potion_marker(models, repo).is_file()
        || std::fs::read_to_string(models.join(LEGACY_POTION_MARKER))
            .is_ok_and(|content| content.trim() == repo)
}

#[cfg(test)]
mod potion_marker_tests {
    use super::*;

    const CODE_64M: &str = "minishlab/potion-code-64M-v2";
    const CODE_16M: &str = "minishlab/potion-code-16M-v2";

    /// Serialises the tests that write `PIXEL_RECALL_DIR`.
    static RECALL_DIR_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn potion_marker_should_be_one_file_per_repository() {
        let models = Path::new("/m");
        assert_eq!(
            potion_marker(models, CODE_64M),
            Path::new("/m/potion-minishlab--potion-code-64M-v2.ok")
        );
        assert_ne!(
            potion_marker(models, CODE_64M),
            potion_marker(models, CODE_16M)
        );
    }

    /// The daemon's model stays cached when another model is downloaded
    /// after it: the flip-flop that sent the daemon back to a background
    /// download on every `search-meaning`.
    #[test]
    fn a_second_model_should_not_uncache_the_first() {
        let dir = tempfile::tempdir().unwrap();
        for repo in [CODE_64M, CODE_16M] {
            std::fs::write(potion_marker(dir.path(), repo), repo).unwrap();
        }
        assert!(potion_cached(dir.path(), CODE_64M));
        assert!(potion_cached(dir.path(), CODE_16M));
        assert!(!potion_cached(
            dir.path(),
            "minishlab/potion-multilingual-128M"
        ));
    }

    #[test]
    fn the_legacy_marker_should_count_only_for_the_repository_it_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(LEGACY_POTION_MARKER),
            format!("{CODE_64M}\n"),
        )
        .unwrap();
        assert!(potion_cached(dir.path(), CODE_64M));
        assert!(!potion_cached(dir.path(), CODE_16M));
    }

    #[test]
    fn recall_dir_should_read_the_override_then_home() {
        {
            let _guard = RECALL_DIR_ENV
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let saved = std::env::var_os("PIXEL_RECALL_DIR");
            let explicit = tempfile::tempdir().unwrap();
            // SAFETY: `PIXEL_RECALL_DIR` is only written under `RECALL_DIR_ENV`.
            unsafe { std::env::set_var("PIXEL_RECALL_DIR", explicit.path()) };
            let got = recall_dir();
            // SAFETY: as above, restoring the value read under the lock.
            unsafe {
                match saved {
                    Some(v) => std::env::set_var("PIXEL_RECALL_DIR", v),
                    None => std::env::remove_var("PIXEL_RECALL_DIR"),
                }
            }
            assert_eq!(got, explicit.path());
        }
        assert_eq!(
            recall_dir(),
            recall_dir_from(
                std::env::var("PIXEL_RECALL_DIR").ok(),
                std::env::var("HOME").ok()
            )
        );
        assert_eq!(
            recall_dir_from(Some("/x/r".to_string()), Some("/h".to_string())),
            Path::new("/x/r")
        );
        let home = tempfile::tempdir().unwrap();
        let h = home.path().to_string_lossy().into_owned();
        let fresh = home.path().join(".local/share/pixel/recall");
        assert_eq!(recall_dir_from(Some(String::new()), Some(h.clone())), fresh);
        let legacy = home.path().join(".local/share/gitpixel/recall");
        std::fs::create_dir_all(&legacy).unwrap();
        assert_eq!(recall_dir_from(None, Some(h.clone())), legacy);
        std::fs::create_dir_all(&fresh).unwrap();
        assert_eq!(recall_dir_from(None, Some(h)), fresh);
    }
}

/// Store fixtures shared by the unit tests: sessions inserted straight
/// through `RecallStore::replace_session`, no source adapter involved.
#[cfg(test)]
pub(crate) mod testutil {
    use crate::model::{IntentSource, Role, TsSource, UnifiedSession, UnifiedTurn};
    use crate::store::{IngestState, RecallStore};

    pub(crate) const TS: i64 = 1_760_000_000_000; // 2025-10-09

    pub(crate) fn state() -> IngestState {
        IngestState {
            file_size: 1,
            mtime_ms: 1,
            bytes_ingested: 1,
            cursor: None,
        }
    }

    /// One session of `agent` with the given turns (role, text); every turn
    /// gets `TS` plus one minute per position, user turns count as human
    /// intent. Returns the session id.
    pub(crate) fn add_session(
        store: &mut RecallStore,
        agent: &'static str,
        source_session_id: &str,
        turns: &[(Role, &str)],
    ) -> i64 {
        let turns: Vec<(Role, Option<IntentSource>, &str)> = turns
            .iter()
            .map(|(role, text)| {
                let intent = (*role == Role::User).then_some(IntentSource::Human);
                (*role, intent, *text)
            })
            .collect();
        add_session_with_intents(store, agent, source_session_id, &turns)
    }

    /// `add_session` with each turn's intent source under the caller's
    /// control, so a fixture can hold a harness-injected user turn.
    pub(crate) fn add_session_with_intents(
        store: &mut RecallStore,
        agent: &'static str,
        source_session_id: &str,
        turns: &[(Role, Option<IntentSource>, &str)],
    ) -> i64 {
        let session = UnifiedSession {
            agent,
            source_session_id: source_session_id.to_string(),
            source_path: format!("/fake/{agent}/{source_session_id}.jsonl"),
            cwd: Some("/work/pixel".to_string()),
            git_branch: None,
            title: None,
            ts_source: TsSource::Iso,
            is_subagent: false,
            parent_source_session_id: None,
        };
        let turns: Vec<UnifiedTurn> = turns
            .iter()
            .enumerate()
            .map(|(i, (role, intent_source, text))| UnifiedTurn {
                role: *role,
                intent_source: *intent_source,
                ts: Some(TS + i as i64 * 60_000),
                text: (*text).to_string(),
                truncated: false,
                source_byte_start: None,
                source_byte_len: None,
            })
            .collect();
        store
            .replace_session(&session, &turns, source_session_id, &state())
            .expect("replace_session")
    }
}
