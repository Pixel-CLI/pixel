//! `envfile` — additive-only, key-level .env mutations with snapshots.
//!
//! Closes the gap the global rule keeps getting violated: env files must
//! never be overwritten wholesale, and .env files are typically gitignored
//! so git history cannot recover them. Every mutation here:
//!
//! 1. Snapshots the current file to `<root>/.pixel/env-snapshots/` BEFORE
//!    touching it (restore is itself snapshotted, so it is undoable too).
//! 2. Mutates at key level only — untouched lines are preserved
//!    byte-for-byte (comments, blanks, `export` prefixes, quoting, CRLF).
//! 3. Writes via temp → fsync → rename (`durable::write_durably`).
//! 4. NEVER includes a value in any output, error message, or journal
//!    record. Key NAMES only. This is a hard invariant with a sentinel
//!    test in `tests/envfile.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::durable::{ensure_dir, write_durably};

/// Subactions of the `envfile` op.
#[derive(Debug, Clone)]
pub enum EnvAction {
    /// Find `.env` / `.env.*` files under root; report key NAMES only.
    Inventory,
    /// Set (replace or append) a single key. Snapshot-first, key-level only.
    Set {
        file: PathBuf,
        key: String,
        value: String,
        create_file: bool,
    },
    /// Restore a file from a named or the latest snapshot (undoable:
    /// the pre-restore state is snapshotted first).
    Restore {
        file: PathBuf,
        snapshot: Option<String>,
    },
    /// List snapshots for a file.
    Snapshots { file: PathBuf },
    /// Verify required key names are present.
    Check { file: PathBuf, require: Vec<String> },
}

/// Entry point for all envfile subactions.
pub fn envfile(root: &Path, action: &EnvAction) -> Result<Value, String> {
    match action {
        EnvAction::Inventory => inventory(root),
        EnvAction::Set {
            file,
            key,
            value,
            create_file,
        } => set(root, file, key, value, *create_file),
        EnvAction::Restore { file, snapshot } => restore(root, file, snapshot.as_deref()),
        EnvAction::Snapshots { file } => snapshots(root, file),
        EnvAction::Check { file, require } => check(root, file, require),
    }
}

// ---------------------------------------------------------------------------
// Path + snapshot-store plumbing
// ---------------------------------------------------------------------------

/// Resolve a possibly-relative file argument against the repo root.
fn resolve_file(root: &Path, file: &Path) -> PathBuf {
    if file.is_absolute() {
        file.to_path_buf()
    } else {
        root.join(file)
    }
}

/// Sanitize a file path into a single directory-name-safe component.
/// `apps/web/.env.local` → `apps__web__.env.local`.
fn sanitize_rel(root: &Path, file: &Path) -> String {
    let rel = file
        .strip_prefix(root)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| file.to_path_buf());
    rel.to_string_lossy()
        .trim_start_matches(['/', '.'])
        .replace(['/', '\\'], "__")
        .replace("..", "_")
}

fn snapshot_dir_for(root: &Path, file: &Path) -> PathBuf {
    root.join(".pixel")
        .join("env-snapshots")
        .join(sanitize_rel(root, file))
}

fn journal_path(root: &Path) -> PathBuf {
    root.join(".pixel").join("env-snapshots").join("journal.jsonl")
}

/// UTC timestamp `YYYYMMDDTHHMMSS.NNNNNNNNNZ` — lexicographically sortable,
/// filename-safe (no colons), nanosecond-unique in practice.
fn utc_timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let nanos = now.subsec_nanos();
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!(
        "{year:04}{month:02}{d:02}T{h:02}{m:02}{s:02}.{nanos:09}Z"
    )
}

/// Copy the current file content into the snapshot store. Returns the
/// snapshot id (the timestamp filename). Never journals or returns values.
fn take_snapshot(root: &Path, file: &Path, content: &[u8]) -> Result<String, String> {
    let dir = snapshot_dir_for(root, file);
    ensure_dir(&dir).map_err(|e| format!("cannot create snapshot dir {}: {e}", dir.display()))?;
    let mut id = utc_timestamp();
    let mut path = dir.join(&id);
    let mut bump = 0u32;
    while path.exists() {
        bump += 1;
        id = format!("{}-{bump}", utc_timestamp());
        path = dir.join(&id);
    }
    write_durably(&path, content)
        .map_err(|e| format!("cannot write snapshot {}: {e}", path.display()))?;
    Ok(id)
}

/// Append a journal record. HARD INVARIANT: the record carries the key NAME
/// at most — never a value.
fn journal_append(root: &Path, file: &Path, action: &str, key: Option<&str>) -> Result<(), String> {
    let path = journal_path(root);
    if let Some(parent) = path.parent() {
        ensure_dir(parent)
            .map_err(|e| format!("cannot create journal dir {}: {e}", parent.display()))?;
    }
    let mut record = json!({
        "ts": utc_timestamp(),
        "file": resolve_file(root, file).display().to_string(),
        "action": action,
    });
    if let Some(k) = key {
        record["key"] = json!(k);
    }
    let mut existing = fs::read(&path).unwrap_or_default();
    existing.extend_from_slice(record.to_string().as_bytes());
    existing.push(b'\n');
    write_durably(&path, &existing)
        .map_err(|e| format!("cannot append journal {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Line-level .env parsing — byte-preserving
// ---------------------------------------------------------------------------

/// Parse one logical line (terminator already stripped). Returns
/// `(key, value_start)` where `value_start` is the byte index just past `=`
/// — everything from there on is the value portion. `None` for comments,
/// blanks, and non-assignment lines.
fn parse_env_line(line: &str) -> Option<(String, usize)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] == b'#' {
        return None;
    }
    // Optional `export ` prefix.
    if line[i..].starts_with("export")
        && matches!(bytes.get(i + 6), Some(b' ') | Some(b'\t'))
    {
        i += 6;
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
    }
    let start = i;
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric()
            || bytes[i] == b'_'
            || bytes[i] == b'.'
            || bytes[i] == b'-')
    {
        i += 1;
    }
    if i == start {
        return None;
    }
    let key = line[start..i].to_string();
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'=' {
        return None;
    }
    Some((key, i + 1))
}

/// Split content into (line_without_terminator, terminator) pairs so every
/// untouched line can be reassembled byte-for-byte (LF, CRLF, or none).
fn split_lines(content: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut rest = content;
    while !rest.is_empty() {
        match rest.find('\n') {
            Some(idx) => {
                let (line_with_nl, tail) = rest.split_at(idx + 1);
                let body = &line_with_nl[..idx];
                if let Some(stripped) = body.strip_suffix('\r') {
                    out.push((stripped, "\r\n"));
                } else {
                    out.push((body, "\n"));
                }
                rest = tail;
            }
            None => {
                out.push((rest, ""));
                rest = "";
            }
        }
    }
    out
}

/// Key names in file order (first occurrence wins, duplicates skipped).
fn key_names(content: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut keys = Vec::new();
    for (line, _) in split_lines(content) {
        if let Some((key, _)) = parse_env_line(line) {
            if seen.insert(key.clone()) {
                keys.push(key);
            }
        }
    }
    keys
}

fn read_env_text(path: &Path) -> Result<String, String> {
    let bytes =
        fs::read(path).map_err(|e| format!("cannot read env file {}: {e}", path.display()))?;
    String::from_utf8(bytes)
        .map_err(|_| format!("env file {} is not valid UTF-8", path.display()))
}

// ---------------------------------------------------------------------------
// Subactions
// ---------------------------------------------------------------------------

fn inventory(root: &Path) -> Result<Value, String> {
    let mut found: Vec<PathBuf> = Vec::new();
    walk_env_files(root, root, 0, &mut found);
    found.sort();
    let mut files = Vec::new();
    for path in found {
        let content = read_env_text(&path).unwrap_or_default();
        let keys = key_names(&content);
        let line_count = split_lines(&content).len();
        let snap_dir = snapshot_dir_for(root, &path);
        let snapshot_count = fs::read_dir(&snap_dir)
            .map(|it| it.filter_map(|e| e.ok()).count())
            .unwrap_or(0);
        let rel = path
            .strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string());
        files.push(json!({
            "path": rel,
            "keys": keys,
            "line_count": line_count,
            "snapshot_count": snapshot_count,
        }));
    }
    Ok(json!({
        "root": root.display().to_string(),
        "files": files,
        "file_count": files.len(),
    }))
}

fn walk_env_files(root: &Path, dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            if matches!(name.as_ref(), "node_modules" | ".git" | "target" | ".pixel") {
                continue;
            }
            walk_env_files(root, &path, depth + 1, out);
        } else if name == ".env" || name.starts_with(".env.") {
            // Skip editor backups / examples? No — `.env.example` is still an
            // env file; inventory reports names only, so listing it is safe.
            out.push(path);
        }
    }
}

fn set(
    root: &Path,
    file: &Path,
    key: &str,
    value: &str,
    create_file: bool,
) -> Result<Value, String> {
    if key.is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
    {
        return Err(format!("invalid env key name: {key:?}"));
    }
    let path = resolve_file(root, file);
    let exists = path.exists();
    if !exists && !create_file {
        return Err(format!(
            "env file {} does not exist (pass create_file to create it)",
            path.display()
        ));
    }

    let content = if exists {
        read_env_text(&path)?
    } else {
        String::new()
    };
    let keys_before = key_names(&content);

    // Snapshot BEFORE mutating — the whole point of this op.
    let snapshot = if exists {
        Some(take_snapshot(root, &path, content.as_bytes())?)
    } else {
        None
    };

    // Key-level mutation: replace only the value portion of the FIRST
    // occurrence; every other byte of the file is preserved exactly.
    let mut new_content = String::with_capacity(content.len() + key.len() + value.len() + 2);
    let mut action = "appended";
    let mut replaced = false;
    for (line, term) in split_lines(&content) {
        if !replaced {
            if let Some((k, value_start)) = parse_env_line(line) {
                if k == key {
                    new_content.push_str(&line[..value_start]);
                    new_content.push_str(value);
                    new_content.push_str(term);
                    replaced = true;
                    action = "replaced";
                    continue;
                }
            }
        }
        new_content.push_str(line);
        new_content.push_str(term);
    }
    if !replaced {
        // Trailing-newline hygiene: never glue onto the last line.
        if !new_content.is_empty() && !new_content.ends_with('\n') {
            new_content.push('\n');
        }
        new_content.push_str(key);
        new_content.push('=');
        new_content.push_str(value);
        new_content.push('\n');
    }

    if let Some(parent) = path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create dir {}: {e}", parent.display()))?;
        }
    }
    write_durably(&path, new_content.as_bytes())
        .map_err(|e| format!("cannot write env file {}: {e}", path.display()))?;
    journal_append(root, &path, "set", Some(key))?;

    let keys_after = key_names(&new_content);
    Ok(json!({
        "file": path.display().to_string(),
        "key": key,
        "action": action,
        "snapshot": snapshot,
        "keys_before": keys_before.len(),
        "keys_after": keys_after.len(),
    }))
}

fn restore(root: &Path, file: &Path, snapshot: Option<&str>) -> Result<Value, String> {
    let path = resolve_file(root, file);
    let dir = snapshot_dir_for(root, &path);

    // Resolve the restore TARGET first — before taking the pre-restore
    // snapshot — so "latest" means "latest at call time", and a chain of
    // restores ping-pongs between states instead of no-oping.
    let target_id = match snapshot {
        Some(id) => {
            let candidate = dir.join(id);
            if !candidate.exists() {
                return Err(format!(
                    "snapshot {id:?} not found for {} (looked in {})",
                    path.display(),
                    dir.display()
                ));
            }
            id.to_string()
        }
        None => {
            let mut ids: Vec<String> = fs::read_dir(&dir)
                .map_err(|_| {
                    format!(
                        "no snapshots for {} (looked in {})",
                        path.display(),
                        dir.display()
                    )
                })?
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            ids.sort();
            ids.pop().ok_or_else(|| {
                format!(
                    "no snapshots for {} (looked in {})",
                    path.display(),
                    dir.display()
                )
            })?
        }
    };
    let target_bytes = fs::read(dir.join(&target_id))
        .map_err(|e| format!("cannot read snapshot {target_id}: {e}"))?;

    // Make restore itself undoable: snapshot current state first.
    if path.exists() {
        let current = fs::read(&path)
            .map_err(|e| format!("cannot read env file {}: {e}", path.display()))?;
        take_snapshot(root, &path, &current)?;
    }

    write_durably(&path, &target_bytes)
        .map_err(|e| format!("cannot write env file {}: {e}", path.display()))?;
    journal_append(root, &path, "restore", None)?;

    let restored_text = String::from_utf8_lossy(&target_bytes).to_string();
    Ok(json!({
        "file": path.display().to_string(),
        "restored_from": target_id,
        "keys_now": key_names(&restored_text),
    }))
}

fn snapshots(root: &Path, file: &Path) -> Result<Value, String> {
    let path = resolve_file(root, file);
    let dir = snapshot_dir_for(root, &path);
    let mut ids: Vec<String> = match fs::read_dir(&dir) {
        Ok(it) => it
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect(),
        Err(_) => Vec::new(),
    };
    ids.sort();
    let mut list = Vec::new();
    for id in ids {
        let snap_path = dir.join(&id);
        let bytes = fs::metadata(&snap_path).map(|m| m.len()).unwrap_or(0);
        let keys = fs::read(&snap_path)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .map(|s| key_names(&s).len())
            .unwrap_or(0);
        list.push(json!({ "id": id, "keys": keys, "bytes": bytes }));
    }
    Ok(json!({
        "file": path.display().to_string(),
        "snapshots": list,
        "snapshot_count": list.len(),
    }))
}

fn check(root: &Path, file: &Path, require: &[String]) -> Result<Value, String> {
    let path = resolve_file(root, file);
    let (file_exists, keys) = if path.exists() {
        (true, key_names(&read_env_text(&path)?))
    } else {
        (false, Vec::new())
    };
    let key_set: std::collections::HashSet<&str> = keys.iter().map(|s| s.as_str()).collect();
    let mut present = Vec::new();
    let mut missing = Vec::new();
    for req in require {
        if key_set.contains(req.as_str()) {
            present.push(req.clone());
        } else {
            missing.push(req.clone());
        }
    }
    let ok = file_exists && missing.is_empty();
    Ok(json!({
        "file": path.display().to_string(),
        "file_exists": file_exists,
        "present": present,
        "missing": missing,
        "ok": ok,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain() {
        assert_eq!(parse_env_line("FOO=bar"), Some(("FOO".into(), 4)));
    }

    #[test]
    fn parse_export_and_spacing() {
        let (k, vs) = parse_env_line("export FOO = bar").unwrap();
        assert_eq!(k, "FOO");
        assert_eq!(&"export FOO = bar"[vs..], " bar");
    }

    #[test]
    fn parse_rejects_comment_blank_nonassign() {
        assert_eq!(parse_env_line("# FOO=bar"), None);
        assert_eq!(parse_env_line("   "), None);
        assert_eq!(parse_env_line("not an assignment"), None);
    }

    #[test]
    fn split_lines_round_trips_bytes() {
        for content in [
            "A=1\nB=2\n",
            "A=1\r\nB=2",
            "",
            "no newline at end",
            "\n\n\n",
        ] {
            let rebuilt: String = split_lines(content)
                .into_iter()
                .map(|(l, t)| format!("{l}{t}"))
                .collect();
            assert_eq!(rebuilt, content);
        }
    }

    #[test]
    fn utc_timestamp_shape() {
        let ts = utc_timestamp();
        assert!(ts.ends_with('Z'));
        assert_eq!(ts.len(), "20260831T120000.123456789Z".len());
        assert!(ts.starts_with("20"));
    }
}
