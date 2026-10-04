//! GitHub Copilot CLI hook installation.
//!
//! Copilot loads hook definitions from `~/.copilot/hooks/*.json` (user level)
//! and `<repo>/.github/hooks/*.json` (repo level). Each file is a
//! `{version: 1, hooks: {<event>: [entries]}}` document; entries are
//! `{type: "command", bash: "...", timeoutSec: N}` and stdin carries a
//! camelCase payload (`toolName`, `toolArgs`, `toolResult`).
//!
//! Pixel writes a dedicated `pixel.json` file — never merged into foreign
//! hook files — containing a `preToolUse` guard entry and a `postToolUse`
//! metrics relay.

use std::fs;
use std::path::{Path, PathBuf};

use crate::install::{CheckStatus, InstallStep, Result, dry_run_summary};

const HOOKS_DIR: &str = ".copilot/hooks";
const HOOKS_FILE: &str = "pixel.json";

/// The user-level hooks directory. `None` when Copilot has never run on
/// this machine — `pixel install` must not fabricate config for a tool
/// that is not installed.
pub(crate) fn copilot_hooks_dir(home: &Path) -> Option<PathBuf> {
    home.join(".copilot").is_dir().then(|| home.join(HOOKS_DIR))
}

/// The `pixel.json` document with both entries pointing at `exe`.
fn hooks_document(exe: &Path) -> serde_json::Value {
    let entry = |args: &str| {
        serde_json::json!({
            "type": "command",
            "bash": format!("'{}' {args}", exe.display()),
            "timeoutSec": 10,
        })
    };
    serde_json::json!({
        "version": 1,
        "hooks": {
            "preToolUse": [entry("run-hook guard --provider copilot")],
            "postToolUse": [entry("run-hook metrics --provider copilot")],
        }
    })
}

/// `pixel install` step: deploy `~/.copilot/hooks/pixel.json`. Idempotent —
/// rewritten only when the content differs (e.g. the binary moved).
pub(crate) fn install_copilot_hooks(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let Some(dir) = copilot_hooks_dir(home) else {
        return Ok(InstallStep {
            id: "copilot-hooks".into(),
            status: CheckStatus::Green,
            summary: "skipped — ~/.copilot not present".into(),
            detail: None,
        });
    };
    let path = dir.join(HOOKS_FILE);
    let detail = Some(format!("path={}", path.display()));
    let step = |status, summary: String| InstallStep {
        id: "copilot-hooks".into(),
        status,
        summary,
        detail: detail.clone(),
    };
    let wanted = serde_json::to_string_pretty(&hooks_document(exe))? + "\n";
    match fs::read_to_string(&path) {
        Ok(existing) if existing == wanted => {
            return Ok(step(
                CheckStatus::Green,
                format!("verified pixel hooks in {}", path.display()),
            ));
        }
        Ok(_) | Err(_) => {}
    }
    let verb = if path.exists() {
        "updated"
    } else {
        "installed"
    };
    let summary = format!("{verb} pixel hooks in {}", path.display());
    if dry_run {
        return Ok(step(CheckStatus::Green, dry_run_summary(true, &summary)));
    }
    fs::create_dir_all(&dir)?;
    let tmp = path.with_extension("json.pixel-tmp");
    fs::write(&tmp, &wanted)?;
    fs::rename(&tmp, &path)?;
    Ok(step(CheckStatus::Green, summary))
}

/// `pixel uninstall` step: remove `~/.copilot/hooks/pixel.json` entirely —
/// the file is exclusively pixel's, so there is nothing to merge back.
pub(crate) fn remove_copilot_hooks(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let path = home.join(HOOKS_DIR).join(HOOKS_FILE);
    let detail = Some(format!("path={}", path.display()));
    let step = |status, summary: String| InstallStep {
        id: "copilot-hooks".into(),
        status,
        summary,
        detail: detail.clone(),
    };
    if !path.exists() {
        return Ok(step(
            CheckStatus::Green,
            "no pixel copilot hooks to remove".into(),
        ));
    }
    let summary = format!("removed {}", path.display());
    if dry_run {
        return Ok(step(CheckStatus::Green, dry_run_summary(true, &summary)));
    }
    fs::remove_file(&path)?;
    Ok(step(CheckStatus::Green, summary))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_has_guard_and_metrics_entries() {
        let doc = hooks_document(Path::new("/usr/local/bin/pixel"));
        assert_eq!(doc["version"], 1);
        let pre = &doc["hooks"]["preToolUse"][0];
        assert_eq!(pre["type"], "command");
        assert!(
            pre["bash"]
                .as_str()
                .unwrap()
                .contains("run-hook guard --provider copilot")
        );
        let post = &doc["hooks"]["postToolUse"][0];
        assert!(
            post["bash"]
                .as_str()
                .unwrap()
                .contains("run-hook metrics --provider copilot")
        );
    }

    #[test]
    fn install_writes_dedicated_file() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".copilot")).unwrap();
        let exe = Path::new("/tmp/pixel-test");
        let step = install_copilot_hooks(home.path(), exe, false).unwrap();
        assert_eq!(step.status, CheckStatus::Green);
        let written = fs::read_to_string(home.path().join(HOOKS_DIR).join(HOOKS_FILE)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(doc["version"], 1);
        // Re-run is idempotent.
        let step = install_copilot_hooks(home.path(), exe, false).unwrap();
        assert!(step.summary.contains("verified"));
        // A moved binary must rewrite the file, not verify stale content.
        let moved = Path::new("/opt/pixel-bin/pixel");
        let step = install_copilot_hooks(home.path(), moved, false).unwrap();
        assert!(step.summary.contains("updated"), "{}", step.summary);
        let written = fs::read_to_string(home.path().join(HOOKS_DIR).join(HOOKS_FILE)).unwrap();
        assert!(written.contains("/opt/pixel-bin/pixel"), "{written}");
        let step = install_copilot_hooks(home.path(), exe, false).unwrap();
        assert!(step.summary.contains("updated"), "{}", step.summary);
        // Uninstall removes the file.
        remove_copilot_hooks(home.path(), false).unwrap();
        assert!(!home.path().join(HOOKS_DIR).join(HOOKS_FILE).exists());
    }

    #[test]
    fn remove_reports_whether_a_file_was_there() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(HOOKS_DIR).join(HOOKS_FILE);
        // Nothing installed: reported, no write.
        let step = remove_copilot_hooks(home.path(), false).unwrap();
        assert!(step.summary.contains("no pixel copilot hooks"));
        // A deployed file is actually deleted, not just reported.
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{}").unwrap();
        let step = remove_copilot_hooks(home.path(), false).unwrap();
        assert!(step.summary.contains("removed"));
        assert!(!path.exists());
    }
}
