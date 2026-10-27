// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use serde_json::json;

use super::*;

const FILE: &str = "fn a() {\n    one();\n}\n\nfn b() {\n    two();\n}\n";

fn disk(path: &str) -> Option<String> {
    (path == "src/x.rs").then(|| FILE.to_owned())
}

fn target(path: &str, proposed: Option<&str>) -> EditTarget {
    EditTarget {
        path: path.to_owned(),
        proposed: proposed.map(str::to_owned),
    }
}

#[test]
fn a_write_carries_its_whole_content_under_any_of_its_names() {
    for (tool, key) in [
        ("Write", "content"),
        ("write_file", "content"),
        ("create_file", "file_text"),
        ("write", "text"),
    ] {
        let input = json!({"file_path": "src/n.rs", key: "fn n() {}\n"});
        assert_eq!(
            targets(tool, &input, &disk),
            vec![target("src/n.rs", Some("fn n() {}\n"))],
            "{tool}"
        );
    }
    assert_eq!(
        targets("Write", &json!({"path": "src/n.rs"}), &disk),
        vec![target("src/n.rs", None)]
    );
}

#[test]
fn an_edit_applies_its_one_unambiguous_replacement() {
    let input =
        json!({"file_path": "src/x.rs", "old_string": "fn a() {", "new_string": "fn a(x: u8) {"});
    let found = targets("Edit", &input, &disk);
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].proposed.as_deref(),
        Some(FILE.replacen("fn a() {", "fn a(x: u8) {", 1).as_str())
    );
    for (tool, old, new) in [
        ("replace", "old_string", "new_string"),
        ("edit", "oldText", "newText"),
        ("str_replace_editor", "old_str", "new_str"),
    ] {
        let input = json!({"path": "src/x.rs", old: "two();", new: "three();"});
        let found = targets(tool, &input, &disk);
        assert!(
            found[0].proposed.as_deref().unwrap().contains("three();"),
            "{tool}"
        );
    }
}

#[test]
fn an_ambiguous_missing_or_empty_replacement_leaves_the_content_unknown() {
    let twice = "x();\nx();\n";
    let read = |_: &str| Some(twice.to_owned());
    let ambiguous = json!({"file_path": "f.rs", "old_string": "x();", "new_string": "y();"});
    assert_eq!(
        targets("Edit", &ambiguous, &read),
        vec![target("f.rs", None)]
    );
    let all = json!({"file_path": "f.rs", "old_string": "x();", "new_string": "y();", "replace_all": true});
    assert_eq!(
        targets("Edit", &all, &read),
        vec![target("f.rs", Some("y();\ny();\n"))]
    );
    let missing = json!({"file_path": "src/x.rs", "old_string": "nope", "new_string": "y"});
    assert_eq!(
        targets("Edit", &missing, &disk),
        vec![target("src/x.rs", None)]
    );
    // `replace_all` of text that is absent changes nothing: the content is unknown.
    let missing_all = json!({"file_path": "src/x.rs", "old_string": "nope", "new_string": "y", "replace_all": true});
    assert_eq!(
        targets("Edit", &missing_all, &disk),
        vec![target("src/x.rs", None)]
    );
    let empty = json!({"file_path": "src/x.rs", "old_string": "", "new_string": "y"});
    assert_eq!(
        targets("Edit", &empty, &disk),
        vec![target("src/x.rs", None)]
    );
    let unreadable = json!({"file_path": "src/other.rs", "old_string": "a", "new_string": "b"});
    assert_eq!(
        targets("Edit", &unreadable, &disk),
        vec![target("src/other.rs", None)]
    );
}

#[test]
fn a_multi_edit_applies_its_edits_in_order() {
    let input = json!({"file_path": "src/x.rs", "edits": [
        {"old_string": "one();", "new_string": "uno();"},
        {"old_string": "uno();", "new_string": "eins();"},
    ]});
    let found = targets("MultiEdit", &input, &disk);
    assert!(found[0].proposed.as_deref().unwrap().contains("eins();"));
    let broken = json!({"file_path": "src/x.rs", "edits": [
        {"old_string": "one();", "new_string": "uno();"},
        {"old_string": "absent", "new_string": "x"},
    ]});
    assert_eq!(
        targets("MultiEdit", &broken, &disk),
        vec![target("src/x.rs", None)]
    );
}

#[test]
fn the_view_commands_notebooks_and_shell_tools_name_no_target() {
    assert!(
        targets(
            "str_replace_editor",
            &json!({"command": "view", "path": "src/x.rs"}),
            &disk
        )
        .is_empty()
    );
    assert!(targets("NotebookEdit", &json!({"notebook_path": "n.ipynb"}), &disk).is_empty());
    assert!(targets("Bash", &json!({"command": "sed -i s/a/b/ src/x.rs"}), &disk).is_empty());
    assert!(targets("Read", &json!({"file_path": "src/x.rs"}), &disk).is_empty());
    assert!(targets("Write", &json!({}), &disk).is_empty());
}

#[test]
fn str_replace_editor_create_carries_its_text() {
    let input = json!({"command": "create", "path": "n.rs", "file_text": "fn n() {}\n"});
    assert_eq!(
        targets("str_replace_editor", &input, &disk),
        vec![target("n.rs", Some("fn n() {}\n"))]
    );
}

const PATCH: &str = "*** Begin Patch\n*** Update File: src/x.rs\n@@\n fn b() {\n-    two();\n+    three();\n }\n*** Add File: src/new.rs\n+fn n() {}\n*** Delete File: src/old.rs\n*** End Patch";

#[test]
fn a_patch_names_every_file_it_touches_with_the_content_it_gives_each() {
    for input in [
        json!({"input": PATCH}),
        json!({"command": ["apply_patch", PATCH]}),
        json!({"patch": PATCH}),
    ] {
        let found = targets("apply_patch", &input, &disk);
        assert_eq!(found.len(), 3, "{input}");
        assert_eq!(
            found[0].proposed.as_deref(),
            Some("fn a() {\n    one();\n}\n\nfn b() {\n    three();\n}\n")
        );
        assert_eq!(found[1], target("src/new.rs", Some("fn n() {}\n")));
        assert_eq!(found[2], target("src/old.rs", Some("")));
    }
}

#[test]
fn a_hunk_whose_lines_are_not_found_leaves_the_content_unknown() {
    let patch = "*** Begin Patch\n*** Update File: src/x.rs\n@@\n-    absent();\n+    other();\n*** End Patch";
    assert_eq!(
        targets("apply_patch", &json!({"input": patch}), &disk),
        vec![target("src/x.rs", None)]
    );
    assert!(targets("apply_patch", &json!({"input": "not a patch"}), &disk).is_empty());
}

#[test]
fn a_patch_with_two_hunks_applies_both_in_order() {
    let patch = "*** Begin Patch\n*** Update File: src/x.rs\n@@\n-    one();\n+    uno();\n@@\n-    two();\n+    dos();\n*** End Patch";
    let found = targets("apply_patch", &json!({"input": patch}), &disk);
    assert_eq!(
        found[0].proposed.as_deref(),
        Some("fn a() {\n    uno();\n}\n\nfn b() {\n    dos();\n}\n")
    );
}

#[test]
fn a_string_under_an_earlier_key_without_the_patch_marker_does_not_hide_the_patch() {
    let input = json!({"input": "not a patch", "patch": PATCH});
    assert_eq!(patch_text(&input), Some(PATCH));
    let argv = json!({"command": ["apply_patch", PATCH]});
    assert_eq!(patch_text(&argv), Some(PATCH));
    assert_eq!(patch_text(&json!({"input": "not a patch"})), None);
}
