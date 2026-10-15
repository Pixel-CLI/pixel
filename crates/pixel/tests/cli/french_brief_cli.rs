// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The brief's French mapping, on by default (`PIXEL_BRIEF_FRENCH=0` turns
//! it off): a French prompt is searched and gated on the
//! English words code uses (`execution_brief/french.rs`). The code of a
//! repository is named in English, so a French prompt about it shares almost
//! no word with it: without the mapping its brief finds nothing to start
//! from. An English prompt must get the same brief either way, and a French
//! prompt about something else must still get none.

use std::path::Path;
use std::process::Output;

use crate::support::{Scratch, git, pixel_command};

/// A small repository whose code names its notions in English, with no
/// French anywhere, and enough unrelated files for the gate's statistics.
fn fixture(tag: &str) -> Scratch {
    let root = Scratch::for_test("french-brief-cli", tag);
    let files: [(&str, &str); 9] = [
        (
            "src/billing/refund_expired.rs",
            "/// Refunds a subscription that expired before its renewal.\n\
             pub fn refund_expired_subscription(subscription: &Subscription) -> Refund {\n    \
             Refund::for_subscription(subscription)\n}\n",
        ),
        (
            "src/billing/subscription.rs",
            "pub struct Subscription { pub expires_at: u64, pub renewed: bool }\n\
             pub struct Refund { pub cents: u64 }\n\
             impl Refund {\n    pub fn for_subscription(_s: &Subscription) -> Self { Refund { cents: 0 } }\n}\n",
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
            "# shop\n\nA small shop backend: search, sessions, mail digests.\n",
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

/// `pixel brief` on `prompt`, with the French mapping `french` (`"1"` on,
/// `"0"` off): the brief forced on, no region pack and no probes, so the
/// block is the chain's alone.
fn brief(root: &Path, prompt: &str, french: &str) -> Output {
    let output = pixel_command()
        .current_dir(root)
        .env("PIXEL_BRIEF", "1")
        .env("PIXEL_BRIEF_PACK", "0")
        .env("PIXEL_BRIEF_PROBES", "0")
        .env("PIXEL_METRICS", "0")
        .env("PIXEL_BRIEF_FRENCH", french)
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

const FILE: &str = "src/billing/refund_expired.rs";

#[test]
fn a_french_prompt_naming_a_concept_should_find_the_file_named_in_english() {
    let root = fixture("concept");
    let prompt = "pourquoi les abonnements expirés ne sont pas remboursés ?";
    let mapped = stdout(&brief(&root, prompt, "1"));
    assert!(
        mapped.contains(FILE),
        "the French prompt's brief should name {FILE}: {mapped}"
    );
    // The mapping is what finds it: read as typed, the prompt's words are
    // nowhere in the repository.
    let typed = stdout(&brief(&root, prompt, "0"));
    assert!(
        !typed.contains(FILE),
        "without the mapping the prompt should not reach {FILE}: {typed}"
    );
}

#[test]
fn an_english_prompt_should_get_the_same_brief_with_the_mapping_on() {
    let root = fixture("english");
    for prompt in [
        "why are expired subscriptions not refunded?",
        "where is the search index built",
    ] {
        let off = brief(&root, prompt, "0");
        let on = brief(&root, prompt, "1");
        assert_eq!(stdout(&on), stdout(&off), "{prompt}");
    }
    let off = stdout(&brief(
        &root,
        "why are expired subscriptions not refunded?",
        "0",
    ));
    assert!(off.contains(FILE), "the English prompt is briefed: {off}");
}

#[test]
fn a_french_prompt_about_something_else_should_still_get_no_brief() {
    let root = fixture("off-topic");
    for prompt in [
        "donne moi une recette de crêpes pour ce soir",
        "c'est quoi la météo demain à paris",
    ] {
        let output = brief(&root, prompt, "1");
        assert_eq!(stdout(&output), "", "{prompt}");
    }
}

// On by default: a host that sets nothing gets the mapping.
#[test]
fn the_mapping_should_be_on_when_nothing_turns_it_off() {
    let root = fixture("default");
    let prompt = "pourquoi les abonnements expirés ne sont pas remboursés ?";
    let output = pixel_command()
        .current_dir(&*root)
        .env("PIXEL_BRIEF", "1")
        .env("PIXEL_BRIEF_PACK", "0")
        .env("PIXEL_BRIEF_PROBES", "0")
        .env("PIXEL_METRICS", "0")
        .env_remove("PIXEL_BRIEF_FRENCH")
        .args(["brief", "--metrics", "off", prompt])
        .arg(&*root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains(FILE), "{text}");
}
