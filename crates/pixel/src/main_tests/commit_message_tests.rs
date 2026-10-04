// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use super::{commit_message, normalize_commit_message};
use std::path::Path;

#[test]
fn normalize_should_keep_paragraphs_and_drop_trailing_whitespace() {
    let raw = "subject\n\nbody line one\n\n- bullet\n\n";
    assert_eq!(
        normalize_commit_message(raw).unwrap(),
        "subject\n\nbody line one\n\n- bullet"
    );
}

#[test]
fn normalize_should_refuse_a_blank_message() {
    assert_eq!(
        normalize_commit_message(" \n\t\n").unwrap_err(),
        "commit message is empty"
    );
}

#[test]
fn commit_message_should_prefer_inline_text_and_name_a_missing_file() {
    assert_eq!(
        commit_message(Some("fix: x".into()), None).unwrap(),
        "fix: x"
    );
    let missing = Path::new("/nonexistent/pixel-msg.txt");
    let err = commit_message(None, Some(missing)).unwrap_err();
    assert!(
        err.starts_with("cannot read commit message file /nonexistent/pixel-msg.txt:"),
        "{err}"
    );
    assert_eq!(
        commit_message(None, None).unwrap_err(),
        "a commit message is required (-m or --message-file)"
    );
}

#[test]
fn commit_message_should_read_the_file_verbatim() {
    let dir = std::env::temp_dir().join(format!("pixel-msg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("msg.txt");
    std::fs::write(&file, "feat: a\n\nSecond paragraph.\n").unwrap();
    assert_eq!(
        commit_message(None, Some(&file)).unwrap(),
        "feat: a\n\nSecond paragraph."
    );
    let _ = std::fs::remove_dir_all(dir);
}
