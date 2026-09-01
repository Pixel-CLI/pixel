//! `sync` — explicit-refspec fetch (idempotent, outside journal transitions).

use std::path::Path;

use serde_json::{json, Value};

use pixel_git::GitRunner;

pub fn sync(root: &Path, remote: &str, refspec: Option<&str>) -> Result<Value, String> {
    let runner = GitRunner::new(root);

    // Validate remote name.
    pixel_git::validate_ref(remote).map_err(|e| e.to_string())?;

    let mut args: Vec<String> = vec!["fetch".into(), "--end-of-options".into(), remote.to_string()];
    if let Some(rs) = refspec {
        pixel_git::validate_ref(rs).map_err(|e| e.to_string())?;
        args.push(rs.to_string());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    runner.run(&arg_refs).map_err(|e| format!("git fetch: {e}"))?;

    // Report fetched refs.
    let refs_out = runner.run_opt(&["for-each-ref", "--format=%(refname:short) %(objectname:short)", "refs/remotes"]).unwrap_or_default();
    let refs: Vec<Value> = String::from_utf8_lossy(&refs_out)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let parts: Vec<&str> = l.splitn(2, ' ').collect();
            json!({"ref": parts.first().copied().unwrap_or(""), "oid": parts.get(1).copied().unwrap_or("")})
        })
        .collect();

    Ok(json!({
        "synced": true,
        "remote": remote,
        "refspec": refspec,
        "refs": refs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn init_repo_with_remote(root: &Path, remote: &Path) {
        // `-b main`: never rely on the machine's init.defaultBranch — the
        // push below names the literal branch "main".
        std::process::Command::new("git").arg("init").arg("-q").arg("-b").arg("main").arg(root).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["config", "user.email", "t@t"]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["config", "user.name", "t"]).status().unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["add", "."]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["commit", "-qm", "init"]).status().unwrap();
        std::process::Command::new("git").arg("init").arg("--bare").arg("-q").arg(remote).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["remote", "add", "origin", remote.to_str().unwrap()]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(root).args(["push", "origin", "main"]).status().unwrap();
    }

    #[test]
    fn sync_fetches_from_remote() {
        let dir = tempdir().unwrap();
        let remote = tempdir().unwrap();
        init_repo_with_remote(dir.path(), remote.path());

        let result = sync(dir.path(), "origin", None).unwrap();
        assert_eq!(result["synced"], json!(true));
    }
}
