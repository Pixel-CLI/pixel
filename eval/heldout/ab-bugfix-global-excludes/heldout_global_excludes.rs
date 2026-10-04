//! Held-out contract for ab-bugfix-global-excludes (copied in after the agent
//! exits). Mirrors the test of the historical fix 2732fa2 through the public
//! API; its own test binary, so setting GIT_CONFIG_GLOBAL races nothing.

use std::path::Path;

use pixel_index::index::policy_walk;

fn walked(root: &Path) -> Vec<String> {
    let mut files: Vec<String> = policy_walk(root)
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            entry
                .path()
                .strip_prefix(root)
                .ok()
                .map(|path| path.to_string_lossy().into_owned())
        })
        .collect();
    files.sort();
    files
}

#[test]
fn the_global_excludes_file_does_not_hide_project_files_but_project_gitignore_applies() {
    let base = std::env::temp_dir().join(format!("heldout-global-excludes-{}", std::process::id()));
    std::fs::remove_dir_all(&base).ok();
    let root = base.join("tree");
    std::fs::create_dir_all(root.join(".claude/hooks")).unwrap();
    std::fs::write(root.join(".claude/hooks/guard.py"), "def guard(): pass\n").unwrap();
    std::fs::write(root.join(".gitignore"), "project-ignored.txt\n").unwrap();
    std::fs::write(root.join("project-ignored.txt"), "ignored\n").unwrap();
    std::fs::write(root.join("keep.rs"), "fn keep() {}\n").unwrap();
    let excludes = base.join("global-ignore");
    std::fs::write(&excludes, ".claude/\n").unwrap();
    let config = base.join("global.gitconfig");
    std::fs::write(&config, format!("[core]\n\texcludesFile = {}\n", excludes.display())).unwrap();
    // SAFETY: the only test in this binary; nothing else reads or writes the
    // environment concurrently.
    unsafe {
        std::env::set_var("GIT_CONFIG_GLOBAL", &config);
    }

    let files = walked(&root);

    std::fs::remove_dir_all(&base).ok();
    assert_eq!(
        files,
        [".claude/hooks/guard.py", ".gitignore", "keep.rs"],
        "the global excludes file must not hide .claude/, and the project .gitignore must stay in force"
    );
}
