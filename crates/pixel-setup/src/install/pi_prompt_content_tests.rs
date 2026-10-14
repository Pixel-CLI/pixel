// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use super::{AGENT_PROMPT_ASSET, PI_PROMPT_ASSET, managed_pi_content, write_pi_prompt};
use crate::config::{MANAGED_BEGIN, MANAGED_END};

const PRE_MARKER_PROMPT: &str = "# Pixel Retrieval Layer — Mandatory Agent Protocol\n\n## THE COMPLETE REPLACEMENT MAP\npixel search \"term\"\n## ENVIRONMENT\nAll commands accept `[PATH]` (default: current directory).\n";

#[test]
fn a_known_pre_marker_prompt_is_replaced_in_place_without_consuming_user_sections() {
    let existing = format!("Before.\n{PRE_MARKER_PROMPT}## My notes\nKeep this.\n");
    let expected = format!(
        "Before.\n{MANAGED_BEGIN}\n{PI_PROMPT_ASSET}\n{MANAGED_END}\n## My notes\nKeep this.\n"
    );
    assert_eq!(managed_pi_content(&existing, PI_PROMPT_ASSET), expected);
}

#[test]
fn a_pre_marker_prompt_above_a_managed_block_is_removed_but_user_sections_survive() {
    let existing = format!(
        "My Pi note.\n{PRE_MARKER_PROMPT}## My notes\nKeep this.\n{MANAGED_BEGIN}\n{PI_PROMPT_ASSET}\n{MANAGED_END}\nAfter.\n"
    );
    let expected = format!(
        "My Pi note.\n## My notes\nKeep this.\n{MANAGED_BEGIN}\n{PI_PROMPT_ASSET}\n{MANAGED_END}\nAfter.\n"
    );
    let migrated = managed_pi_content(&existing, PI_PROMPT_ASSET);
    assert_eq!(migrated, expected);
    assert_eq!(managed_pi_content(&migrated, PI_PROMPT_ASSET), migrated);
}

#[test]
fn a_pasted_prompt_inside_a_code_fence_is_user_text() {
    let user_text = format!("~~~markdown\n{PRE_MARKER_PROMPT}~~~\n");
    let existing = format!("{user_text}{MANAGED_BEGIN}\n{PI_PROMPT_ASSET}\n{MANAGED_END}\n");
    assert_eq!(managed_pi_content(&existing, PI_PROMPT_ASSET), existing);
}

#[test]
fn fence_boundaries_decide_whether_a_historical_prompt_is_user_text() {
    let managed = format!("{MANAGED_BEGIN}\n{PI_PROMPT_ASSET}\n{MANAGED_END}\n");
    for (prefix, is_fenced) in [
        ("``\n", false),
        ("   ```markdown\n", true),
        ("    ```markdown\n", false),
        ("```markdown\n", true),
        ("```lang`note\n", false),
        ("~~~lang`note\n", true),
        ("~~~markdown\n", true),
        ("~~~~markdown\n~~~\n", true),
        ("~~~~markdown\n~~~~\n", false),
        ("~~~markdown\n~~~ignored\n", true),
        ("~~~markdown\n```\n", true),
        ("```markdown\n```\n", false),
        ("~~~markdown\n~~~  \n", false),
    ] {
        let existing = format!("{prefix}{PRE_MARKER_PROMPT}{managed}");
        let expected = if is_fenced {
            existing.clone()
        } else {
            format!("{prefix}{managed}")
        };
        assert_eq!(
            managed_pi_content(&existing, PI_PROMPT_ASSET),
            expected,
            "prefix {prefix:?} must preserve user text only inside a fence"
        );
    }
}

#[test]
fn historical_prompt_recognition_needs_the_full_release_signature() {
    for fragment in [
        PRE_MARKER_PROMPT.replace("## ENVIRONMENT\n", ""),
        PRE_MARKER_PROMPT.replace("## THE COMPLETE REPLACEMENT MAP\n", ""),
        PRE_MARKER_PROMPT.replace(
            "All commands accept `[PATH]` (default: current directory).\n",
            "",
        ),
    ] {
        let existing = format!("{fragment}{MANAGED_BEGIN}\n{PI_PROMPT_ASSET}\n{MANAGED_END}\n");
        assert_eq!(
            managed_pi_content(&existing, PI_PROMPT_ASSET),
            existing,
            "partial historical text may be the user's own note"
        );
    }
}

#[test]
fn an_inline_historical_title_is_user_text_not_a_deployment() {
    let note = format!("Quoted: {PRE_MARKER_PROMPT}");
    let existing = format!("{note}{MANAGED_BEGIN}\n{PI_PROMPT_ASSET}\n{MANAGED_END}\n");
    assert_eq!(managed_pi_content(&existing, PI_PROMPT_ASSET), existing);
}

#[test]
fn every_historical_copy_outside_the_managed_block_is_removed() {
    let existing = format!(
        "Before.\n{PRE_MARKER_PROMPT}{PRE_MARKER_PROMPT}{MANAGED_BEGIN}\n{PI_PROMPT_ASSET}\n{MANAGED_END}\n{PRE_MARKER_PROMPT}After.\n"
    );
    let expected = format!("Before.\n{MANAGED_BEGIN}\n{PI_PROMPT_ASSET}\n{MANAGED_END}\nAfter.\n");
    assert_eq!(managed_pi_content(&existing, PI_PROMPT_ASSET), expected);
}

#[test]
fn first_install_should_remove_every_historical_copy_and_preserve_user_sections() {
    let existing = format!("Before.\n{PRE_MARKER_PROMPT}Between.\n{PRE_MARKER_PROMPT}After.\n");
    let expected =
        format!("Before.\nBetween.\n{MANAGED_BEGIN}\n{PI_PROMPT_ASSET}\n{MANAGED_END}\nAfter.\n");
    let migrated = managed_pi_content(&existing, PI_PROMPT_ASSET);
    assert_eq!(migrated, expected);
    assert_eq!(managed_pi_content(&migrated, PI_PROMPT_ASSET), migrated);
}

#[test]
fn cleanup_should_ignore_orphan_end_markers_before_a_managed_block() {
    let prefix = format!("{MANAGED_END}\nBefore.\n");
    let managed = format!("{MANAGED_BEGIN}\n{PI_PROMPT_ASSET}\n{MANAGED_END}\n");
    let existing = format!("{prefix}{managed}{PRE_MARKER_PROMPT}After.\n");
    let expected = format!("{prefix}{managed}After.\n");
    assert_eq!(super::strip_unmarked_pi_prompts(&existing), expected);
    assert_eq!(managed_pi_content(&existing, PI_PROMPT_ASSET), expected);

    let unterminated = format!("{prefix}{MANAGED_BEGIN}\n{PRE_MARKER_PROMPT}");
    assert_eq!(
        super::strip_unmarked_pi_prompts(&unterminated),
        unterminated
    );
    assert_eq!(
        managed_pi_content(&unterminated, PI_PROMPT_ASSET),
        format!("{prefix}{managed}")
    );
}

#[test]
fn cleanup_should_preserve_a_user_fence_spanning_the_managed_block() {
    let managed = format!("{MANAGED_BEGIN}\n{PI_PROMPT_ASSET}\n{MANAGED_END}\n");
    for (opener, closer) in [("```markdown", "```"), ("~~~~markdown", "~~~~")] {
        let protected = format!("{opener}\n{managed}{PRE_MARKER_PROMPT}{closer}\n");
        let existing = format!("{protected}{PRE_MARKER_PROMPT}After.\n");
        let expected = format!("{protected}After.\n");
        assert_eq!(super::strip_unmarked_pi_prompts(&existing), expected);
        assert_eq!(managed_pi_content(&existing, PI_PROMPT_ASSET), expected);
    }
}

#[test]
fn first_install_should_remove_a_historical_copy_after_an_exact_asset_match() {
    let existing = format!("Before.\n{AGENT_PROMPT_ASSET}Between.\n{PRE_MARKER_PROMPT}After.\n");
    let expected =
        format!("Before.\n{MANAGED_BEGIN}\n{PI_PROMPT_ASSET}\n{MANAGED_END}\nBetween.\nAfter.\n");
    assert_eq!(managed_pi_content(&existing, PI_PROMPT_ASSET), expected);
}

#[test]
fn a_fence_inside_the_managed_block_does_not_protect_an_outside_legacy_prompt() {
    let managed = format!("{MANAGED_BEGIN}\n```\n{MANAGED_END}");
    let existing = format!("{managed}\n{PRE_MARKER_PROMPT}After.\n");
    assert_eq!(
        super::strip_unmarked_pi_prompts(&existing),
        format!("{managed}\nAfter.\n"),
        "cleanup must preserve the entire managed block and scan outside it independently"
    );
}

#[test]
fn a_prompt_file_written_by_an_older_install_is_wrapped_in_place_not_duplicated() {
    let wrapped = managed_pi_content(AGENT_PROMPT_ASSET, PI_PROMPT_ASSET);
    assert!(wrapped.starts_with(MANAGED_BEGIN), "{wrapped}");
    assert!(wrapped.trim_end().ends_with(MANAGED_END), "{wrapped}");
    assert_eq!(
        wrapped.matches(PI_PROMPT_ASSET).count(),
        1,
        "the short Pi rule must appear once"
    );
    assert!(!wrapped.contains(AGENT_PROMPT_ASSET));
}

#[test]
fn user_text_around_a_stale_copy_is_kept() {
    let existing = format!("My own pi note.\n{AGENT_PROMPT_ASSET}");
    let wrapped = managed_pi_content(&existing, PI_PROMPT_ASSET);
    assert!(wrapped.starts_with("My own pi note.\n"), "{wrapped}");
    assert_eq!(wrapped.matches(PI_PROMPT_ASSET).count(), 1);
    assert!(!wrapped.contains(AGENT_PROMPT_ASSET));
}

#[test]
fn migration_without_stale_sections_preserves_an_unterminated_user_tail() {
    let existing = format!("{AGENT_PROMPT_ASSET}Keep this exact tail.");
    let wrapped = managed_pi_content(&existing, PI_PROMPT_ASSET);
    assert!(wrapped.ends_with("Keep this exact tail."), "{wrapped}");
}

#[test]
fn an_unmarked_current_pi_rule_is_wrapped_without_losing_following_user_text() {
    let existing = format!("Before.\n{PI_PROMPT_ASSET}After.\n");
    let wrapped = managed_pi_content(&existing, PI_PROMPT_ASSET);
    let expected = format!("Before.\n{MANAGED_BEGIN}\n{PI_PROMPT_ASSET}\n{MANAGED_END}\nAfter.\n");
    assert_eq!(wrapped, expected, "install owns only the existing Pi rule");
}

#[test]
fn a_user_heading_alone_does_not_identify_a_legacy_prompt() {
    let existing = "# Pixel Retrieval Layer\nMy own note.\n";
    let wrapped = managed_pi_content(existing, PI_PROMPT_ASSET);
    assert!(wrapped.starts_with(existing), "{wrapped}");
    assert_eq!(wrapped.matches(PI_PROMPT_ASSET).count(), 1);
}

#[test]
fn an_earlier_similar_heading_stays_outside_the_edited_legacy_prompt() {
    let edited = "# Pixel Retrieval Layer\n\
                  Pixel provides deterministic code retrieval, edited by hand.\n\
                  ## MANDATORY WORKFLOW\nDo the workflow.\n\
                  ## REPLACEMENT MAP\nMap.\n\
                  All commands accept `[PATH]`, default current directory.\n";
    let existing = format!("# Pixel Retrieval Layer\nMy own note.\n{edited}After.\n");
    let wrapped = managed_pi_content(&existing, PI_PROMPT_ASSET);
    assert!(
        wrapped.starts_with("# Pixel Retrieval Layer\nMy own note.\n"),
        "{wrapped}"
    );
    assert!(wrapped.ends_with("After.\n"), "{wrapped}");
    assert!(!wrapped.contains("edited by hand"), "{wrapped}");
    assert!(
        !wrapped.contains(super::LEGACY_PI_PROMPT_END),
        "the final legacy rule must be removed with the old section: {wrapped}"
    );
}

#[test]
fn a_partial_legacy_signature_does_not_replace_user_text() {
    let existing = "# Pixel Retrieval Layer\nMy note.\n## MANDATORY WORKFLOW\nMy workflow.\nAll commands accept `[PATH]`, default current directory.\n";
    let wrapped = managed_pi_content(existing, PI_PROMPT_ASSET);
    assert!(wrapped.starts_with(existing), "{wrapped}");
}

#[test]
fn an_inline_legacy_heading_does_not_replace_quoted_user_text() {
    let edited = "# Pixel Retrieval Layer\n\
                  Pixel provides deterministic code retrieval, quoted inline.\n\
                  ## MANDATORY WORKFLOW\nDo the workflow.\n\
                  ## REPLACEMENT MAP\nMap.\n\
                  All commands accept `[PATH]`, default current directory.\n";
    let existing = format!("Quoted: {edited}After.");
    let wrapped = managed_pi_content(&existing, PI_PROMPT_ASSET);
    assert!(wrapped.starts_with(&existing), "{wrapped}");
}

#[test]
fn a_file_pixel_never_wrote_keeps_its_text_and_gets_one_block() {
    let existing = "answer in French.\n";
    let wanted = managed_pi_content(existing, AGENT_PROMPT_ASSET);
    assert!(wanted.starts_with(existing), "{wanted}");
    assert!(wanted.contains(MANAGED_BEGIN), "{wanted}");
    assert_eq!(
        wanted.matches(MANAGED_BEGIN).count(),
        1,
        "one block, not one per install"
    );
    assert_eq!(
        managed_pi_content(&wanted, AGENT_PROMPT_ASSET),
        wanted,
        "a managed file is left alone on the next install"
    );
}

#[cfg(unix)]
#[test]
fn write_pi_prompt_fails_when_path_is_unreadable() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("pixel-write-pi-{:x}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("pi.md");
    fs::write(&path, b"user text").unwrap();
    // Remove read permission but keep write permission.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o200)).unwrap();

    // An unreadable file must be an error, never "no file".
    let result = write_pi_prompt(&path);
    assert!(
        result.is_err(),
        "expected error for unreadable path, got {result:?}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_file_whose_bytes_are_not_utf8_is_not_a_missing_file() {
    use std::fs;

    let dir =
        std::env::temp_dir().join(format!("pixel-write-pi-non-utf8-{:x}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("pi.md");
    // The file is there but cannot be read as text: `read_to_string` fails
    // with InvalidData while the byte-wise backup and the atomic write
    // would both succeed, so "I could not read it" must not pass for "there
    // is no file" — that would replace bytes the user owns.
    let user_bytes = b"\xff\xfeanswer in French\n";
    fs::write(&path, user_bytes).unwrap();

    let result = write_pi_prompt(&path);

    assert!(
        result.is_err(),
        "a prompt pixel cannot read as text must be an error, got {result:?}"
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        user_bytes.as_slice(),
        "the user's bytes must survive a failed read"
    );
    let leftovers: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("pixel-bak") || name.contains("pixel-tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "a failed read must leave no backup and no temp file: {leftovers:?}"
    );

    let _ = fs::remove_dir_all(&dir);
}
