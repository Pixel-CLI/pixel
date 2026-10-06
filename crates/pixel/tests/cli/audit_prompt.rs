// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The language-alternatives audit prompt (issue #526) is a committed
//! document under `docs/audit/`. This test verifies it exists, is not
//! swallowed by the `docs/audit/.gitignore` artifact exclusion, and covers
//! every language and every evaluation criterion the issue names.

use std::path::Path;

/// The audit prompt's path relative to the repository root.
const PROMPT_REL: &str = "docs/audit/language-alternatives.md";

/// The `docs/audit/.gitignore` that keeps audit artifacts local while
/// allowing the prompt itself to be committed.
const GITIGNORE_REL: &str = "docs/audit/.gitignore";

fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn prompt_path() -> std::path::PathBuf {
    repo_root().join(PROMPT_REL)
}

#[test]
fn audit_prompt_exists_and_is_committed() {
    let path = prompt_path();
    assert!(
        path.exists(),
        "audit prompt must exist at {}",
        path.display()
    );
    // The .gitignore must not ignore the prompt file itself.
    let gitignore = repo_root().join(GITIGNORE_REL);
    let ignore = std::fs::read_to_string(&gitignore).unwrap_or_default();
    assert!(
        ignore.contains("!language-alternatives.md"),
        "docs/audit/.gitignore must un-ignore the prompt file"
    );
}

#[test]
fn audit_prompt_covers_all_seven_languages() {
    let content = std::fs::read_to_string(prompt_path())
        .expect("audit prompt must be readable");
    for language in [
        "Rust",
        "TypeScript / Bun",
        "Zig",
        "Go",
        "C++",
        "C# Native AOT",
        "Ruby / Spinel",
    ] {
        assert!(
            content.contains(language),
            "audit prompt must cover {language}"
        );
    }
}

#[test]
fn audit_prompt_covers_all_five_criteria() {
    let content = std::fs::read_to_string(prompt_path())
        .expect("audit prompt must be readable");
    for criterion in [
        "Correctness",
        "Runtime performance",
        "Representative prototypes",
        "agent iteration time",
        "Migration economics",
    ] {
        assert!(
            content.contains(criterion),
            "audit prompt must cover criterion: {criterion}"
        );
    }
}

#[test]
fn audit_prompt_defines_audit_procedure() {
    let content = std::fs::read_to_string(prompt_path())
        .expect("audit prompt must be readable");
    assert!(
        content.contains("Audit procedure"),
        "audit prompt must define the audit procedure"
    );
    assert!(
        content.contains("Baseline"),
        "audit prompt must define a baseline step"
    );
    assert!(
        content.contains("Decision matrix"),
        "audit prompt must define a decision matrix output"
    );
}

#[test]
fn audit_prompt_is_reusable() {
    let content = std::fs::read_to_string(prompt_path())
        .expect("audit prompt must be readable");
    assert!(
        content.contains("Reusability"),
        "audit prompt must define how to re-run the audit"
    );
    assert!(
        content.contains("local"),
        "audit prompt must specify that the audit is local"
    );
}
