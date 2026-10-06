// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the agent-config rewrite helpers: what `pixel install`
//! may write into a user's CLAUDE.md / AGENTS.md / settings.json, what it must
//! leave alone, and what a dry run must never touch.

use super::*;
use serde_json::json;

const MANAGED: &str = "use pixel";

fn home() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn backups_in(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .map(|it| {
            it.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.to_string_lossy().contains(".pixel-bak."))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

// --- rewrite_agent_config -------------------------------------------------

/// A dry run reports the exact outcome a real run would have, but must not
/// create the file, its directory, or a backup.
#[test]
fn rewrite_agent_config_should_report_without_writing_when_dry_run_on_a_missing_file() {
    let home = home();
    let path = home.path().join(".claude/CLAUDE.md");
    let outcome = rewrite_agent_config(&path, MANAGED, true).unwrap();
    assert!(!outcome.rewritten);
    assert!(!outcome.already_managed);
    assert!(
        outcome.would_change,
        "an empty file gains the managed block"
    );
    assert_eq!(outcome.stale_blocks_removed, 0);
    assert_eq!(outcome.backup_path, None);
    assert!(!path.exists(), "dry run created the file");
    assert!(
        !path.parent().unwrap().exists(),
        "dry run created the directory"
    );
}

/// A dry run over a file carrying a stale GitNexus section counts it and
/// leaves the bytes on disk untouched, with no backup beside them.
#[test]
fn rewrite_agent_config_should_count_stale_blocks_and_keep_the_file_when_dry_run() {
    let home = home();
    let path = home.path().join("AGENTS.md");
    let original = "intro\n# GitNexus — Code Intelligence\nold\n## sub\nmore\n# Mine\nkeep\n";
    fs::write(&path, original).unwrap();
    let outcome = rewrite_agent_config(&path, MANAGED, true).unwrap();
    assert_eq!(outcome.stale_blocks_removed, 1);
    assert!(outcome.would_change);
    assert!(!outcome.rewritten);
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
    assert!(backups_in(home.path()).is_empty(), "dry run wrote a backup");
}

/// A real run on a missing file creates its parent directory and the file,
/// and takes no backup because there was nothing to lose.
#[test]
fn rewrite_agent_config_should_create_file_and_parent_without_backup_when_missing() {
    let home = home();
    let path = home.path().join("nested/dir/CLAUDE.md");
    let outcome = rewrite_agent_config(&path, MANAGED, false).unwrap();
    assert!(outcome.rewritten);
    assert_eq!(outcome.backup_path, None);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("{MANAGED_BEGIN}\n{MANAGED}\n{MANAGED_END}\n")
    );
}

/// Rewriting a file that changes keeps the user's text, drops the stale
/// section, and saves the previous bytes in a backup beside it.
#[test]
fn rewrite_agent_config_should_back_up_and_keep_user_text_when_content_changes() {
    let home = home();
    let path = home.path().join("CLAUDE.md");
    let original = "# Mine\nkeep me\n# GitNexus — Code Intelligence\nstale\n";
    fs::write(&path, original).unwrap();
    let outcome = rewrite_agent_config(&path, MANAGED, false).unwrap();
    assert!(outcome.rewritten);
    assert!(!outcome.already_managed);
    assert_eq!(outcome.stale_blocks_removed, 1);
    let written = fs::read_to_string(&path).unwrap();
    assert_eq!(
        written,
        format!("# Mine\nkeep me\n{MANAGED_BEGIN}\n{MANAGED}\n{MANAGED_END}\n")
    );
    let backup = outcome.backup_path.expect("changed file is backed up");
    assert_eq!(fs::read_to_string(backup).unwrap(), original);
}

/// A re-install with the same managed text is a no-op: the outcome says the
/// file was already managed and unchanged, and no backup accumulates.
#[test]
fn rewrite_agent_config_should_not_back_up_when_reinstalling_the_same_block() {
    let home = home();
    let path = home.path().join("CLAUDE.md");
    rewrite_agent_config(&path, MANAGED, false).unwrap();
    let second = rewrite_agent_config(&path, MANAGED, false).unwrap();
    assert!(second.already_managed);
    assert!(!second.would_change);
    assert_eq!(second.backup_path, None);
    assert!(backups_in(home.path()).is_empty());
}

/// A path that cannot be read for a reason other than absence (here, a
/// directory) is an error, never treated as an empty file to overwrite.
#[test]
fn rewrite_agent_config_should_fail_when_the_path_is_a_directory() {
    let home = home();
    let dir = home.path().join("CLAUDE.md");
    fs::create_dir_all(&dir).unwrap();
    assert!(matches!(
        rewrite_agent_config(&dir, MANAGED, false),
        Err(ConfigError::Io(_))
    ));
    assert!(dir.is_dir(), "the directory must survive");
    assert!(
        rewrite_agent_config(&dir, MANAGED, true).is_err(),
        "a dry run must report the same failure"
    );
}

/// A file that is not valid UTF-8 cannot be edited as text: the rewrite
/// fails and the user's bytes stay, rather than being replaced by a fresh
/// managed block as if the file were empty.
#[test]
fn rewrite_agent_config_should_keep_bytes_when_the_file_is_not_utf8() {
    let home = home();
    let path = home.path().join("CLAUDE.md");
    let bytes = b"keep \xff\xfe me\n";
    fs::write(&path, bytes).unwrap();
    assert!(matches!(
        rewrite_agent_config(&path, MANAGED, false),
        Err(ConfigError::Io(_))
    ));
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

/// `backup_if_changing` surfaces an unreadable source instead of skipping
/// the backup and letting the caller overwrite it.
#[test]
fn backup_if_changing_should_fail_when_the_source_is_a_directory() {
    let home = home();
    assert!(backup_if_changing(home.path(), b"x").is_err());
}

// --- managed markers and stale blocks ----------------------------------------

/// An unterminated managed block is owned by pixel through end of file: the
/// rewrite replaces it with one complete block instead of nesting a second.
#[test]
fn apply_managed_markers_should_replace_through_eof_when_the_end_marker_is_missing() {
    let original = format!("head\n{MANAGED_BEGIN}\nold body without end\n");
    let out = apply_managed_markers(&original, MANAGED);
    assert_eq!(
        out,
        format!("head\n{MANAGED_BEGIN}\n{MANAGED}\n{MANAGED_END}\n")
    );
    assert_eq!(out.matches(MANAGED_BEGIN).count(), 1);
}

/// Text without a trailing newline gets one before the appended block, so
/// the user's last line is never glued to the begin marker.
#[test]
fn apply_managed_markers_should_add_a_newline_before_appending_to_unterminated_text() {
    let out = apply_managed_markers("last line", MANAGED);
    assert_eq!(
        out,
        format!("last line\n{MANAGED_BEGIN}\n{MANAGED}\n{MANAGED_END}\n")
    );
}

/// A stale section runs until the next header at the same or a shallower
/// depth: its deeper sub-headers go with it, the next sibling section stays.
#[test]
fn strip_stale_blocks_should_remove_subsections_and_stop_at_a_sibling_header() {
    let text = "## codebase-memory tools\na\n### deeper\nb\n## Kept\nc\n";
    let (out, removed) = strip_stale_blocks(text);
    assert_eq!(removed, 1);
    assert_eq!(out, "## Kept\nc\n");
}

/// A deeper stale header ends at a shallower one, which survives.
#[test]
fn strip_stale_blocks_should_stop_at_a_shallower_header() {
    let (out, removed) = strip_stale_blocks("# Top\n### GitNexus notes\nx\n# Next\ny\n");
    assert_eq!(removed, 1);
    assert_eq!(out, "# Top\n# Next\ny\n");
}

/// Only Markdown headers can open a stale section: a `#tag` without a
/// space, a seven-hash line or prose mentioning gitnexus is user text.
#[test]
fn strip_stale_blocks_should_keep_lines_that_are_not_markdown_headers() {
    let text = "#gitnexus tag\n####### gitnexus seven\nwe skip gitnexus here\n";
    let (out, removed) = strip_stale_blocks(text);
    assert_eq!(removed, 0);
    assert_eq!(out, text);
}

/// The header closing a stale section may open the next one: both go.
#[test]
fn strip_stale_blocks_should_remove_back_to_back_stale_sections() {
    let (out, removed) =
        strip_stale_blocks("intro\n## gitnexus\na\n## codebase-memory\nb\n## Kept\nc\n");
    assert_eq!(removed, 2);
    assert_eq!(out, "intro\n## Kept\nc\n");
}

/// A bare `#` line is a header (depth 1) and closes a stale section.
#[test]
fn strip_stale_blocks_should_treat_a_bare_hash_line_as_a_closing_header() {
    let (out, removed) = strip_stale_blocks("# gitnexus\nx\n#\nafter\n");
    assert_eq!(removed, 1);
    assert_eq!(out, "#\nafter\n");
}

/// Uninstall leaves text alone when a begin marker has no end marker: it
/// cannot tell where pixel's block stops, so it removes nothing.
#[test]
fn strip_managed_block_should_keep_text_when_the_end_marker_is_missing() {
    let text = format!("a\n{MANAGED_BEGIN}\nb\n");
    assert_eq!(strip_managed_block(&text), text);
}

// --- retired rule files -----------------------------------------------------

/// Retired-tool rule files are found in every scanned directory, including
/// the `.agent-config` source the per-tool directories are rebuilt from;
/// other files there are not touched.
#[test]
fn scrub_deprecated_rule_files_should_list_but_keep_them_when_dry_run() {
    let home = home();
    let devin = home.path().join(".devin/rules");
    let source = home.path().join(".agent-config/rules");
    fs::create_dir_all(&devin).unwrap();
    fs::create_dir_all(&source).unwrap();
    fs::write(devin.join("gitpixel.md"), "old").unwrap();
    fs::write(source.join("sniper.md"), "fence").unwrap();
    fs::write(devin.join("mine.md"), "keep").unwrap();

    let found = scrub_deprecated_rule_files(home.path(), true).unwrap();
    assert_eq!(
        found,
        vec![devin.join("gitpixel.md"), source.join("sniper.md")]
    );
    assert!(
        devin.join("gitpixel.md").is_file(),
        "dry run removed a file"
    );
    assert!(backups_in(&devin).is_empty());
}

/// A real scrub removes each retired file and keeps its content in a
/// backup, while the user's own rule files stay.
#[test]
fn scrub_deprecated_rule_files_should_remove_and_back_up_each_retired_file() {
    let home = home();
    let rules = home.path().join(".claude/rules");
    fs::create_dir_all(&rules).unwrap();
    fs::write(rules.join("usable-git.md"), "retired text").unwrap();
    fs::write(rules.join("mine.md"), "keep").unwrap();

    let removed = scrub_deprecated_rule_files(home.path(), false).unwrap();
    assert_eq!(removed, vec![rules.join("usable-git.md")]);
    assert!(!rules.join("usable-git.md").exists());
    assert_eq!(fs::read_to_string(rules.join("mine.md")).unwrap(), "keep");
    let backups = backups_in(&rules);
    assert_eq!(backups.len(), 1);
    assert_eq!(fs::read_to_string(&backups[0]).unwrap(), "retired text");
}

// --- settings.json ------------------------------------------------------------

#[test]
fn scrub_settings_json_should_report_absent_without_creating_when_missing() {
    let home = home();
    let path = home.path().join("settings.json");
    let outcome = scrub_settings_json(&path, false).unwrap();
    assert!(!outcome.existed);
    assert_eq!(outcome.mcp_servers_removed, 0);
    assert!(!path.exists());
}

/// A settings.json that does not parse is reported, never overwritten with
/// a reconstructed one that would lose the user's settings.
#[test]
fn scrub_settings_json_should_refuse_and_keep_the_file_when_json_is_invalid() {
    let home = home();
    let path = home.path().join("settings.json");
    fs::write(&path, "{ not json").unwrap();
    let err = scrub_settings_json(&path, false).unwrap_err();
    assert!(
        matches!(&err, ConfigError::InvalidSettings { path: p, .. } if p == &path),
        "{err}"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");
}

/// Only the retired MCP servers go; `pixel` and the user's own servers and
/// keys stay, and the previous file is backed up.
#[test]
fn scrub_settings_json_should_remove_only_retired_servers_and_back_up() {
    let home = home();
    let path = home.path().join("settings.json");
    let original = json!({
        "theme": "dark",
        "mcpServers": {"usable-git": {}, "sniper": {}, "pixel": {"cmd": "p"}, "mine": {}}
    });
    fs::write(&path, original.to_string()).unwrap();
    let outcome = scrub_settings_json(&path, false).unwrap();
    assert_eq!(outcome.mcp_servers_removed, 2);
    let now: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        now,
        json!({"theme": "dark", "mcpServers": {"pixel": {"cmd": "p"}, "mine": {}}})
    );
    let backup = outcome.backup_path.expect("backup of the changed file");
    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(backup).unwrap()).unwrap();
    assert_eq!(saved, original);
}

/// A dry run counts what would go and leaves the file and directory as-is.
#[test]
fn scrub_settings_json_should_count_without_writing_when_dry_run() {
    let home = home();
    let path = home.path().join("settings.json");
    let raw = r#"{"mcpServers":{"gitpixel":{}}}"#;
    fs::write(&path, raw).unwrap();
    let outcome = scrub_settings_json(&path, true).unwrap();
    assert_eq!(outcome.mcp_servers_removed, 1);
    assert_eq!(outcome.backup_path, None);
    assert_eq!(fs::read_to_string(&path).unwrap(), raw);
    assert!(backups_in(home.path()).is_empty());
}

/// Nothing to remove means no rewrite at all: the user's formatting stays
/// byte for byte.
#[test]
fn scrub_settings_json_should_leave_bytes_alone_when_nothing_is_retired() {
    let home = home();
    let path = home.path().join("settings.json");
    let raw = "{\"mcpServers\": {\"pixel\": {}},   \"x\": 1}";
    fs::write(&path, raw).unwrap();
    let outcome = scrub_settings_json(&path, false).unwrap();
    assert!(outcome.existed);
    assert_eq!(outcome.mcp_servers_removed, 0);
    assert_eq!(fs::read_to_string(&path).unwrap(), raw);
}

// --- hook merging ---------------------------------------------------------

fn nested(command: &str) -> serde_json::Value {
    json!({"matcher": "Bash", "hooks": [{"type": "command", "command": command}]})
}

/// A malformed, non-array hook value is someone's configuration: it is kept
/// beside pixel's entry, not dropped.
#[test]
fn merge_hook_entry_should_wrap_and_keep_a_non_array_value() {
    let other = json!({"matcher": "x", "hooks": []});
    let merged = merge_hook_entry(
        Some(&other),
        "run-hook guard",
        nested("pixel run-hook guard"),
    );
    assert_eq!(merged, json!([other, nested("pixel run-hook guard")]));
}

/// Re-installing replaces pixel's previous entry and keeps every other one.
#[test]
fn merge_hook_entry_should_replace_only_the_marked_entry_when_reinstalling() {
    let existing = json!([nested("other tool"), nested("pixel run-hook guard --old")]);
    let merged = merge_hook_entry(
        Some(&existing),
        "run-hook guard",
        nested("pixel run-hook guard"),
    );
    assert_eq!(
        merged,
        json!([nested("other tool"), nested("pixel run-hook guard")])
    );
    assert_eq!(
        merge_hook_entry(None, "m", nested("pixel m")),
        json!([nested("pixel m")])
    );
}

/// The flat (Cursor) merge also wraps a non-array value and drops only the
/// previous pixel command.
#[test]
fn merge_flat_hook_entry_should_wrap_a_non_array_and_start_from_nothing() {
    let other = json!({"command": "theirs"});
    let pixel = json!({"command": "pixel run-hook guard"});
    assert_eq!(
        merge_flat_hook_entry(Some(&other), "run-hook guard", pixel.clone()),
        json!([other, pixel.clone()])
    );
    assert_eq!(
        merge_flat_hook_entry(None, "run-hook guard", pixel.clone()),
        json!([pixel])
    );
}

/// Removing pixel's hooks keeps entries that carry no nested `hooks` list,
/// and leaves a non-array value untouched.
#[test]
fn remove_hook_entries_should_keep_entries_without_hooks_and_non_arrays() {
    let plain = json!({"matcher": "Bash"});
    let arr = json!([plain.clone(), nested("pixel-targets-guard")]);
    assert_eq!(remove_hook_entries(&arr, GUARD_HOOK), json!([plain]));
    let scalar = json!("not an array");
    assert_eq!(remove_hook_entries(&scalar, GUARD_HOOK), scalar);
    assert_eq!(remove_flat_hook_entries(&scalar, GUARD_HOOK), scalar);
}

/// When the guard was the only hook of an event, the event key goes too;
/// when user hooks remain, the event keeps them.
#[test]
fn remove_guard_hook_entries_should_drop_emptied_events_and_keep_the_rest() {
    let mut hooks = serde_json::Map::new();
    hooks.insert("Only".into(), json!([nested("/x/pixel-targets-guard")]));
    hooks.insert(
        "Mixed".into(),
        json!([nested("/x/pixel-targets-guard"), nested("mine")]),
    );
    hooks.insert("Untouched".into(), json!([nested("mine")]));
    assert_eq!(remove_guard_hook_entries(&mut hooks), 2);
    assert!(!hooks.contains_key("Only"));
    assert_eq!(hooks["Mixed"], json!([nested("mine")]));
    assert_eq!(hooks["Untouched"], json!([nested("mine")]));
}
