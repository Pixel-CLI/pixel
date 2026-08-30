//! Property-based test over `publish`, in the spirit of usable-git's
//! `publish-property.test.ts`: instead of the crash matrix's 6 hand-picked
//! cells, vary the fixture (which candidate files are pre-staged, which
//! subset gets published) and check the same commit-scope invariant holds
//! broadly, not just for one hand-picked scenario.
//!
//! This does not attempt to port every property from the TS suite — it is
//! a modest, honest slice: for any non-empty subset of a small candidate
//! file set, `publish` must commit EXACTLY that subset (regardless of what
//! else happens to already be staged), and leave every non-selected
//! candidate's on-disk content untouched.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use proptest::prelude::*;
use serde_json::json;
use tempfile::TempDir;

use pixel_ops::publish::{publish_with_state, PublishOptions};

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {:?}: {e}", args));
    if !output.status.success() {
        panic!(
            "git -C {} {:?} failed: {}",
            root.display(),
            args,
            String::from_utf8_lossy(&output.stderr),
        );
    }
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn init_repo(root: &Path) {
    Command::new("git")
        .args(["init", "-q", root.to_str().unwrap()])
        .status()
        .unwrap();
    git(root, &["config", "user.email", "prop@pixel"]);
    git(root, &["config", "user.name", "Prop"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    git(root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    std::fs::write(root.join("base.txt"), b"base").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);
}

const CANDIDATES: &[&str] = &["alpha.txt", "beta.txt", "gamma.txt"];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// For any non-empty subset of `CANDIDATES` selected for publishing,
    /// and any independent subset already staged beforehand, the resulting
    /// commit contains exactly the selected subset — never more (sweeping
    /// in a pre-staged-but-unselected file), never less.
    #[test]
    fn publish_commits_exactly_the_selected_files(
        selection_mask in 1u8..8u8,
        pre_stage_mask in 0u8..8u8,
    ) {
        let repo_dir = TempDir::new().unwrap();
        let state_dir = TempDir::new().unwrap();
        let root = repo_dir.path();
        init_repo(root);

        let mut selected: Vec<String> = Vec::new();
        for (i, name) in CANDIDATES.iter().enumerate() {
            std::fs::write(root.join(name), format!("content-{name}")).unwrap();
            if pre_stage_mask & (1 << i) != 0 {
                git(root, &["add", "--", name]);
            }
            if selection_mask & (1 << i) != 0 {
                selected.push(name.to_string());
            }
        }
        prop_assume!(!selected.is_empty());

        let opts = PublishOptions {
            message: "property publish".to_string(),
            files: selected.clone(),
            expected_head: None,
            expected_fingerprints: Default::default(),
            push: false,
            amend: false,
            request_id: format!("prop-{}", uuid::Uuid::new_v4()),
        };

        let result = publish_with_state(root, &opts, None, state_dir.path())
            .unwrap_or_else(|e| panic!("publish failed for selection {selected:?}: {e}"));
        prop_assert_eq!(result["published"].clone(), json!(true));

        let committed_raw = git(root, &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"]);
        let committed: BTreeSet<String> = committed_raw
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let expected: BTreeSet<String> = selected.iter().cloned().collect();
        prop_assert_eq!(
            committed.clone(),
            expected.clone(),
            "commit must contain exactly the selected files",
        );

        for (i, name) in CANDIDATES.iter().enumerate() {
            if selection_mask & (1 << i) == 0 {
                let content = std::fs::read_to_string(root.join(name)).unwrap();
                prop_assert_eq!(content, format!("content-{name}"));
                prop_assert!(!committed.contains(*name));
            }
        }
    }
}
