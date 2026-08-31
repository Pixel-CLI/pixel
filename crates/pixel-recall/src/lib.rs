//! pixel-recall — machine-wide LLM transcript retrieval corpus.
//!
//! Ingests every parseable CLI transcript store (Claude Code, Codex,
//! opencode, Devin, Cursor CLI, zcode, Gemini history) into one SQLite
//! corpus of turn-granular text, then serves lexical (trigram) and semantic
//! (embedding) retrieval over it. Unlike the repo index, this corpus is
//! global: transcripts belong to the machine, not to a repository.

pub mod ask;
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

use std::path::PathBuf;

/// Corpus root: `$PIXEL_RECALL_DIR`, else `~/.local/share/gitpixel/recall`.
/// Created on demand with owner-only permissions — this directory
/// concentrates every transcript on the machine.
pub fn recall_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PIXEL_RECALL_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".local/share/gitpixel/recall")
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
    PathBuf::from(home).join(".local/share/gitpixel/models")
}
