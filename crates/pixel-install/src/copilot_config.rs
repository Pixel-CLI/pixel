//! GitHub Copilot CLI hook installation.
//!
//! Copilot loads hook definitions from `~/.copilot/hooks/*.json` (user level)
//! and `<repo>/.github/hooks/*.json` (repo level). Each file is a
//! `{version: 1, hooks: {<event>: [entries]}}` document; entries are
//! `{type: "exec", exec: "...", args: [...], timeoutSec: N}` and stdin carries a
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

/// Top-level key Pixel writes into its hook document so it can tell its own
/// `pixel.json` from a user's file. Copilot CLI ignores keys it does not read.
const MANAGED_KEY: &str = "_pixel_managed";
const MANAGED_MARKER: &str = "pixel-managed-copilot-hooks-v1";

/// The user-level hooks directory. `None` when Copilot has never run on
/// this machine — `pixel install` must not fabricate config for a tool
/// that is not installed.
pub(crate) fn copilot_hooks_dir(home: &Path) -> Option<PathBuf> {
    home.join(".copilot").is_dir().then(|| home.join(HOOKS_DIR))
}

/// Whether the document Pixel is about to replace or delete is one it wrote —
/// recognized by the `MANAGED_KEY` marker it always embeds. A file that is
/// foreign, or edited so the marker is gone, is not Pixel's to touch.
fn is_pixel_owned(content: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(content)
        .ok()
        .as_ref()
        .and_then(|doc| doc.get(MANAGED_KEY))
        .and_then(serde_json::Value::as_str)
        == Some(MANAGED_MARKER)
}

/// The `pixel.json` document with both entries executing `exe` directly.
///
/// Copilot's `exec`/`args` entry fields run the binary by its own path, so a
/// path containing an apostrophe works (a `bash` string interpolation would
/// not) and no `powershell` command is needed on Windows.
fn hooks_document(exe: &Path) -> serde_json::Value {
    let entry = |args: &[&str]| {
        serde_json::json!({
            "type": "exec",
            "exec": exe.display().to_string(),
            "args": args,
            "timeoutSec": 10,
        })
    };
    serde_json::json!({
        "version": 1,
        MANAGED_KEY: MANAGED_MARKER,
        "hooks": {
            "preToolUse": [entry(&["run-hook", "guard", "--provider", "copilot"])],
            "postToolUse": [entry(&["run-hook", "metrics", "--provider", "copilot"])],
        }
    })
}

/// `pixel install` step: deploy `~/.copilot/hooks/pixel.json`. Idempotent —
/// rewritten only when the content differs (e.g. the binary moved) and the
/// existing document is one Pixel wrote.
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
        // A document Pixel did not write, or one edited since: never replace
        // it. Report the conflict and leave the file untouched.
        Ok(existing) if !is_pixel_owned(&existing) => {
            return Ok(step(
                CheckStatus::Red,
                format!(
                    "conflict — {} is not pixel-managed; preserved unchanged",
                    path.display()
                ),
            ));
        }
        // A pixel-managed file whose content differs (e.g. the binary moved):
        // rewrite it below.
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

/// `pixel uninstall` step: remove `~/.copilot/hooks/pixel.json` — but only
/// when Pixel wrote it. A foreign document is preserved and reported.
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
    // A file Pixel did not write is not Pixel's to delete: preserve it and
    // report the conflict.
    let existing = fs::read_to_string(&path)?;
    if !is_pixel_owned(&existing) {
        return Ok(step(
            CheckStatus::Red,
            format!(
                "conflict — {} is not pixel-managed; preserved unchanged",
                path.display()
            ),
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

    /// The entry uses Copilot's `exec`/`args` fields, so a path with an
    /// apostrophe reaches the executable directly instead of being quoted
    /// into a shell string.
    #[test]
    fn document_uses_an_exec_entry_that_survives_apostrophes() {
        let path = "/usr/local/o'reilly/bin/pixel";
        let doc = hooks_document(Path::new(path));
        assert_eq!(doc["version"], 1);
        assert_eq!(doc[MANAGED_KEY], MANAGED_MARKER);
        let pre = &doc["hooks"]["preToolUse"][0];
        assert_eq!(pre["type"], "exec");
        assert_eq!(pre["exec"], path);
        assert_eq!(pre["timeoutSec"], 10);
        assert_eq!(
            pre["args"],
            serde_json::json!(["run-hook", "guard", "--provider", "copilot"])
        );
        let post = &doc["hooks"]["postToolUse"][0];
        assert_eq!(post["exec"], path);
        assert_eq!(post["timeoutSec"], 10);
        assert_eq!(
            post["args"],
            serde_json::json!(["run-hook", "metrics", "--provider", "copilot"])
        );
    }

    #[test]
    fn install_skips_when_copilot_has_never_run() {
        let home = tempfile::tempdir().unwrap();
        // No ~/.copilot directory: pixel install must not fabricate config
        // for a tool that is not installed, and must not create it either.
        let step = install_copilot_hooks(home.path(), Path::new("/tmp/pixel"), false).unwrap();
        assert!(step.summary.contains("skipped"), "{}", step.summary);
        assert_eq!(step.status, CheckStatus::Green);
        assert!(!home.path().join(HOOKS_DIR).exists());
    }

    #[test]
    fn dry_run_previews_changes_without_writing() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".copilot")).unwrap();
        let installed = home.path().join(HOOKS_DIR).join(HOOKS_FILE);
        // Install dry-run reports the action but writes nothing.
        let step = install_copilot_hooks(home.path(), Path::new("/tmp/pixel"), true).unwrap();
        assert!(step.summary.contains("would"), "{}", step.summary);
        assert!(!installed.exists());
        // The real install lands, then a remove dry-run leaves it in place.
        install_copilot_hooks(home.path(), Path::new("/tmp/pixel"), false).unwrap();
        assert!(installed.exists());
        let step = remove_copilot_hooks(home.path(), true).unwrap();
        assert!(step.summary.contains("would"), "{}", step.summary);
        assert!(installed.exists());
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
    fn install_preserves_a_foreign_file() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(HOOKS_DIR).join(HOOKS_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\"version\":1,\"hooks\":{}}").unwrap();
        let step = install_copilot_hooks(home.path(), Path::new("/tmp/pixel"), false).unwrap();
        assert!(
            step.summary.contains("is not pixel-managed"),
            "{}",
            step.summary
        );
        assert_eq!(step.status, CheckStatus::Red);
        let still = fs::read_to_string(&path).unwrap();
        assert_eq!(still, "{\"version\":1,\"hooks\":{}}");
    }

    #[test]
    fn remove_reports_whether_a_file_was_there() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(HOOKS_DIR).join(HOOKS_FILE);
        // Nothing installed: reported, no write.
        let step = remove_copilot_hooks(home.path(), false).unwrap();
        assert!(step.summary.contains("no pixel copilot hooks"));
        // A pixel-managed file is actually deleted, not just reported.
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        install_copilot_hooks(home.path(), Path::new("/tmp/pixel-test"), false).unwrap();
        let step = remove_copilot_hooks(home.path(), false).unwrap();
        assert!(step.summary.contains("removed"));
        assert!(!path.exists());
    }

    #[test]
    fn remove_preserves_a_foreign_file() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(HOOKS_DIR).join(HOOKS_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{}").unwrap();
        let step = remove_copilot_hooks(home.path(), false).unwrap();
        assert!(
            step.summary.contains("is not pixel-managed"),
            "{}",
            step.summary
        );
        assert_eq!(step.status, CheckStatus::Red);
        assert!(path.exists());
    }
}
