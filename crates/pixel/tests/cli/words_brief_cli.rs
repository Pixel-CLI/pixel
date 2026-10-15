// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The brief's plain-word leads (`execution_brief/words.rs`), on with
//! `PIXEL_BRIEF_BM25=auto`: with no daemon there is no meaning search, and a
//! prompt whose words the code names in other forms ("renaming", `rename`)
//! reaches the file through BM25 over its path and symbol names, where the
//! relevance co-files, which match whole words, do not.

use std::path::Path;
use std::process::Output;

use crate::support::{Scratch, git, pixel_command};

/// A small repository whose rename code names its notions in their base
/// forms, beside unrelated files for the gate's statistics.
fn fixture(tag: &str) -> Scratch {
    let root = Scratch::for_test("words-brief-cli", tag);
    let files: [(&str, &str); 9] = [
        (
            "src/refactor/rename.rs",
            "pub fn rename_symbol(uid: &str, to: &str) -> Edit {\n    Edit::new(uid, to)\n}\n\
             pub fn updated_reference(edit: &Edit) -> usize { edit.len() }\n",
        ),
        (
            "src/refactor/edit.rs",
            "/// An edit of the symbols a renaming touches.\n\
             pub struct Edit { pub symbol: String, pub callers: Vec<String> }\n",
        ),
        (
            "src/search/index.rs",
            "pub fn build_search_index(words: &[String]) -> Vec<String> { words.to_vec() }\n",
        ),
        (
            "src/http/router.rs",
            "pub fn route_request(path: &str) -> &'static str { if path == \"/\" { \"home\" } else { \"404\" } }\n",
        ),
        (
            "src/auth/session.rs",
            "pub fn open_session(user: &str) -> String { format!(\"session:{user}\") }\n",
        ),
        (
            "src/cli/args.rs",
            "pub fn parse_args(args: &[String]) -> usize { args.len() }\n",
        ),
        (
            "src/storage/cache.rs",
            "pub fn cache_get(key: &str) -> Option<String> { Some(key.to_string()) }\n",
        ),
        (
            "src/mail/digest.rs",
            "pub fn weekly_digest(lines: &[String]) -> String { lines.join(\"\\n\") }\n",
        ),
        (
            "README.md",
            "# tool\n\nA small refactoring tool: search, sessions, mail digests.\n",
        ),
    ];
    for (path, text) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    std::fs::write(root.join("fixture-marker.txt"), format!("{tag}\n")).unwrap();
    std::fs::write(root.join(".gitignore"), ".pixel/\n").unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "fixture"]);
    let prepare = pixel_command()
        .args(["prepare-repo", "--no-daemon"])
        .arg(&*root)
        .output()
        .unwrap();
    assert!(prepare.status.success(), "{prepare:?}");
    root
}

/// `pixel brief` on `prompt` with no daemon (so no meaning search), the
/// switch at `words` (`None`: unset), no region pack and no probes, so the
/// block is the chain's alone.
fn brief(root: &Path, prompt: &str, words: Option<&str>) -> Output {
    let mut command = pixel_command();
    command
        .current_dir(root)
        .env("PIXEL_BRIEF", "1")
        .env("PIXEL_BRIEF_PACK", "0")
        .env("PIXEL_BRIEF_PROBES", "0")
        .env("PIXEL_DAEMON_AUTO_START", "0")
        .env("PIXEL_METRICS", "0");
    match words {
        Some(value) => command.env("PIXEL_BRIEF_BM25", value),
        None => command.env_remove("PIXEL_BRIEF_BM25"),
    };
    let output = command
        .args(["brief", "--metrics", "off", prompt])
        .arg(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    output
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

const FILE: &str = "src/refactor/rename.rs";
const PROMPT: &str = "how does renaming symbols update the references of callers";

/// The first file of the brief's `files:` line.
fn first_file(text: &str) -> &str {
    text.lines()
        .find_map(|line| line.strip_prefix("files: "))
        .and_then(|files| files.split([';', ' ', ':']).next())
        .unwrap_or_default()
}

#[test]
fn a_prompt_in_other_forms_of_the_codes_names_should_start_at_the_file_its_plain_words_name() {
    let root = fixture("forms");
    // Switched on, the file whose names hold the prompt's words as stems
    // comes first.
    let ranked = stdout(&brief(&root, PROMPT, Some("auto")));
    assert_eq!(first_file(&ranked), FILE, "{ranked}");
    // The leads are what put it there: off, the co-files, which match whole
    // words, start at the doc comment that spells them.
    let off = stdout(&brief(&root, PROMPT, Some("0")));
    assert_eq!(first_file(&off), "src/refactor/edit.rs", "{off}");
    // Off by default: a host that sets nothing gets the brief it had.
    assert_eq!(stdout(&brief(&root, PROMPT, None)), off);
}

#[test]
fn a_prompt_about_something_else_should_still_get_no_brief() {
    // The leads rank files once the gate let a prompt through; they never
    // open the gate.
    let root = fixture("off-topic");
    for prompt in [
        "give me a pancake recipe for tonight",
        "what's the weather going to be like tomorrow",
    ] {
        assert_eq!(stdout(&brief(&root, prompt, Some("auto"))), "", "{prompt}");
    }
}
