//! File-based flow store — one JSON file per flow in a global directory.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::types::Flow;

/// Flow directory: `$PIXEL_FLOW_DIR`, else `~/.local/share/pixel/flows/`.
/// Created on demand with owner-only permissions.
pub fn flow_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PIXEL_FLOW_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".local/share/pixel/flows")
}

/// Ensure the flow directory exists (mode 0700).
pub fn ensure_flow_dir() -> Result<PathBuf, String> {
    let dir = flow_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create flow dir {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Slugify a flow name into a filename-safe component.
/// `github-auth-device-flow` → `github-auth-device-flow.json`
/// `GitHub Auth!` → `github-auth.json`
pub fn slugify(name: &str) -> String {
    let slug: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else if c == ' ' {
                '-'
            } else {
                '\0'
            }
        })
        .filter(|c| *c != '\0')
        .collect();
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "unnamed".to_string()
    } else {
        slug.to_string()
    }
}

/// Full path for a flow file: `<flow_dir>/<slug>.json`.
fn flow_path(name: &str) -> PathBuf {
    flow_dir().join(format!("{}.json", slugify(name)))
}

/// Check whether a flow exists by name.
pub fn exists(name: &str) -> bool {
    flow_path(name).exists()
}

/// Load a flow by name.
pub fn load(name: &str) -> Result<Flow, String> {
    let path = flow_path(name);
    let data = fs::read_to_string(&path)
        .map_err(|e| format!("cannot read flow '{}': {e}", slugify(name)))?;
    let flow: Flow = serde_json::from_str(&data)
        .map_err(|e| format!("cannot parse flow '{}': {e}", slugify(name)))?;
    flow.validate()?;
    Ok(flow)
}

/// Save a flow to disk (atomic: write to tmp, rename).
pub fn save(flow: &Flow) -> Result<PathBuf, String> {
    flow.validate()?;
    ensure_flow_dir()?;
    let path = flow_path(&flow.name);
    let json = serde_json::to_string_pretty(flow)
        .map_err(|e| format!("cannot serialize flow: {e}"))?;
    write_atomic(&path, json.as_bytes())
        .map_err(|e| format!("cannot write flow '{}': {e}", path.display()))?;
    Ok(path)
}

/// Delete a flow by name. Returns true if a file was removed.
pub fn delete(name: &str) -> Result<bool, String> {
    let path = flow_path(name);
    if !path.exists() {
        return Ok(false);
    }
    fs::remove_file(&path)
        .map_err(|e| format!("cannot delete flow '{}': {e}", path.display()))?;
    Ok(true)
}

/// List all saved flows. Returns a JSON array of `{name, title, tags, proven, revision}`.
pub fn list() -> Result<Value, String> {
    let dir = flow_dir();
    if !dir.exists() {
        return Ok(Value::Array(vec![]));
    }
    let mut entries: Vec<Value> = Vec::new();
    let read = fs::read_dir(&dir)
        .map_err(|e| format!("cannot read flow dir {}: {e}", dir.display()))?;
    for entry in read {
        let entry = entry.map_err(|e| format!("dir entry error: {e}"))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // Load just enough metadata — skip corrupt files.
        if let Ok(data) = fs::read_to_string(&path) {
            if let Ok(flow) = serde_json::from_str::<Flow>(&data) {
                entries.push(serde_json::json!({
                    "name": flow.name,
                    "title": flow.title,
                    "tags": flow.tags,
                    "proven": flow.proven,
                    "revision": flow.revision,
                    "revised_unix": flow.revised_unix,
                }));
            }
        }
    }
    // Sort by name for deterministic output.
    entries.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });
    Ok(Value::Array(entries))
}

/// Atomic write: temp file in same dir, then rename.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|f| f.to_str()).unwrap_or("flow")
    ));
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FlowStep;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now_unix() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn make_flow(name: &str) -> Flow {
        Flow {
            name: name.into(),
            title: "Test".into(),
            description: "Test".into(),
            tags: vec!["test".into()],
            url: Some("https://example.com".into()),
            tab: None,
            success_url_contains: vec![],
            success_url_excludes: vec![],
            mfa_keywords: vec![],
            stale_tab_cleanup: vec![],
            preconditions: vec![],
            vars: vec![],
            steps: vec![FlowStep {
                action: "open".into(),
                url: Some("https://example.com".into()),
                ..Default::default()
            }],
            success_signal: None,
            created_unix: now_unix(),
            revised_unix: now_unix(),
            revision: 1,
            proven: false,
        }
    }

    /// Serialize tests that mutate PIXEL_FLOW_DIR — env vars are process-global.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("github-auth"), "github-auth");
        assert_eq!(slugify("GitHub Auth!"), "github-auth");
        assert_eq!(slugify("  spaced  "), "spaced");
        assert_eq!(slugify(""), "unnamed");
    }

    #[test]
    fn save_load_round_trip() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("PIXEL_FLOW_DIR", tmp.path()); }
        let flow = make_flow("round-trip-test");
        let saved = save(&flow).unwrap();
        assert!(saved.exists());
        let loaded = load("round-trip-test").unwrap();
        assert_eq!(loaded.name, "round-trip-test");
        assert_eq!(loaded.title, "Test");
        unsafe { std::env::remove_var("PIXEL_FLOW_DIR"); }
    }

    #[test]
    fn list_returns_saved_flows() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("PIXEL_FLOW_DIR", tmp.path()); }
        save(&make_flow("alpha")).unwrap();
        save(&make_flow("beta")).unwrap();
        let list = list().unwrap();
        let arr = list.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"].as_str(), Some("alpha"));
        assert_eq!(arr[1]["name"].as_str(), Some("beta"));
        unsafe { std::env::remove_var("PIXEL_FLOW_DIR"); }
    }

    #[test]
    fn delete_removes_flow() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("PIXEL_FLOW_DIR", tmp.path()); }
        save(&make_flow("to-delete")).unwrap();
        assert!(exists("to-delete"));
        assert!(delete("to-delete").unwrap());
        assert!(!exists("to-delete"));
        assert!(!delete("to-delete").unwrap()); // already gone
        unsafe { std::env::remove_var("PIXEL_FLOW_DIR"); }
    }

    #[test]
    fn load_missing_errors() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("PIXEL_FLOW_DIR", tmp.path()); }
        assert!(load("nonexistent").is_err());
        unsafe { std::env::remove_var("PIXEL_FLOW_DIR"); }
    }
}
