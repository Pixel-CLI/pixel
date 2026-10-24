// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use std::io::Cursor;

use super::*;

fn run(input: &str, home: &Path, running: Option<AgentTarget>) -> (Vec<AgentTarget>, String) {
    let mut output = Vec::new();
    let chosen = choose(
        &mut Cursor::new(input.to_owned()),
        &mut output,
        home,
        running,
    )
    .unwrap();
    (chosen, String::from_utf8(output).unwrap())
}

#[test]
fn replies_should_read_as_yes_no_or_neither() {
    for yes in ["y", "Y", "yes", " YES \n"] {
        assert_eq!(answer(yes), Some(true), "{yes:?}");
    }
    for no in ["n", "N", "no", " No\n"] {
        assert_eq!(answer(no), Some(false), "{no:?}");
    }
    for other in ["", "maybe", "yy", "1"] {
        assert_eq!(answer(other), None, "{other:?}");
    }
}

#[test]
fn enter_should_keep_the_default_for_each_agent() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    let enters = "\n".repeat(AgentTarget::ALL.len());
    let (chosen, shown) = run(&enters, home.path(), None);
    assert_eq!(chosen, vec![AgentTarget::ClaudeCode]);
    assert!(shown.contains("Claude Code:"), "{shown}");
    assert!(shown.contains("[Y/n]"), "{shown}");
    assert!(shown.contains("[y/N]"), "{shown}");
}

#[test]
fn the_running_agent_should_default_to_yes_even_without_a_config_directory() {
    let home = tempfile::tempdir().unwrap();
    let enters = "\n".repeat(AgentTarget::ALL.len());
    let (chosen, _) = run(&enters, home.path(), Some(AgentTarget::Pi));
    assert_eq!(chosen, vec![AgentTarget::Pi]);
}

#[test]
fn an_explicit_answer_should_beat_the_default_in_both_directions() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    // Claude Code (in use) declined, Codex (not in use) accepted, the rest Enter.
    let input = format!("n\ny\n{}", "\n".repeat(AgentTarget::ALL.len() - 2));
    let (chosen, _) = run(&input, home.path(), None);
    assert_eq!(chosen, vec![AgentTarget::Codex]);
}

#[test]
fn an_unreadable_reply_should_be_asked_again_and_a_third_one_keeps_the_default() {
    let mut output = Vec::new();
    let mut input = Cursor::new("what\nn\n".to_owned());
    assert!(!ask(&mut input, &mut output, "Q?", true).unwrap());
    let shown = String::from_utf8(output).unwrap();
    assert_eq!(shown.matches("answer y or n").count(), 1, "{shown}");

    let mut output = Vec::new();
    let mut input = Cursor::new("a\nb\nc\ny\n".to_owned());
    assert!(ask(&mut input, &mut output, "Q?", true).unwrap());
    assert_eq!(String::from_utf8(output).unwrap().matches("Q?").count(), 3);
}

#[test]
fn the_end_of_input_should_answer_every_remaining_question_with_its_default() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".codex")).unwrap();
    let (chosen, _) = run("", home.path(), None);
    assert_eq!(chosen, vec![AgentTarget::Codex]);
}
