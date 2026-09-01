//! `ship` — publish + push in one op (convenience wrapper).

use std::path::Path;

use serde_json::{json, Value};

use crate::publish::{publish, PublishOptions};
use crate::push::{push, PushOptions};

/// Ship = publish (commit) then push. Uses separate request IDs for
/// each phase so they're independently recoverable.
pub fn ship(
    root: &Path,
    message: &str,
    files: &[String],
    remote: &str,
    refspec: &str,
    request_id: &str,
) -> Result<Value, String> {
    ship_with_lease(root, message, files, remote, refspec, request_id, false)
}

/// Ship with an optional leased push (`--force-with-lease`) for the push
/// phase — the publish phase is unaffected.
pub fn ship_with_lease(
    root: &Path,
    message: &str,
    files: &[String],
    remote: &str,
    refspec: &str,
    request_id: &str,
    force_with_lease: bool,
) -> Result<Value, String> {
    // Publish first.
    let pub_opts = PublishOptions {
        message: message.to_string(),
        files: files.to_vec(),
        expected_head: None,
        expected_fingerprints: std::collections::BTreeMap::new(),
        push: false,
        amend: false,
        request_id: format!("{request_id}-pub"),
    };
    let pub_result = publish(root, &pub_opts, None)?;

    // Then push.
    let push_opts = PushOptions {
        remote: remote.to_string(),
        refspec: refspec.to_string(),
        request_id: format!("{request_id}-push"),
        force_with_lease,
    };
    let push_result = push(root, &push_opts, None)?;

    Ok(json!({
        "published": pub_result,
        "pushed": push_result,
        "shipped": true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn init_repo_with_remote(root: &Path, remote: &Path) {
        // `-b main`: never rely on the machine's init.defaultBranch — the
        // ship below pushes the literal refspec "main".
        std::process::Command::new("git").arg("init").arg("-q").arg("-b").arg("main").arg(root).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["config", "user.email", "t@t"]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["config", "user.name", "t"]).status().unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["add", "."]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["commit", "-qm", "init"]).status().unwrap();
        std::process::Command::new("git").arg("init").arg("--bare").arg("-q").arg(remote).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["remote", "add", "origin", remote.to_str().unwrap()]).status().unwrap();
    }

    #[test]
    fn ship_commits_and_pushes() {
        let dir = tempdir().unwrap();
        let remote = tempdir().unwrap();
        init_repo_with_remote(dir.path(), remote.path());
        std::fs::write(dir.path().join("b.txt"), b"b").unwrap();

        let result = ship(
            dir.path(),
            "ship test",
            &["b.txt".to_string()],
            "origin",
            "main",
            &format!("ship-{}", uuid::Uuid::new_v4()),
        )
        .unwrap();
        assert_eq!(result["shipped"], json!(true));
    }
}
