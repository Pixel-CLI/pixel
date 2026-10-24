// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use std::time::{Duration, Instant};

use super::*;
use crate::routes::QuestionKind;
use crate::steps::testing::{Repo, texts};
use crate::steps::{Steps, blocks};

const CONFIG: &str = "use std::env;\n\npub struct Settings {\n    pub level: String,\n}\n\npub fn load() -> Settings {\n    let level = env::var(\"APP_LOG_LEVEL\").unwrap_or_default();\n    Settings { level: normalize(level) }\n}\n\nfn normalize(level: String) -> String {\n    level.to_lowercase()\n}\n";
const MAIN: &str =
    "mod config;\n\nfn main() {\n    let settings = config::load();\n    run(settings);\n}\n";

fn outcome(repo: &Repo, typed: &str, kind: QuestionKind) -> crate::steps::Outcome {
    let deadline = Instant::now() + Duration::from_secs(5);
    blocks(typed, kind, Steps::default(), &[], repo, deadline)
}

fn env_repo() -> Repo {
    Repo::new(&[
        ("src/config.rs", CONFIG),
        ("src/main.rs", MAIN),
        ("docs/env.md", "APP_LOG_LEVEL sets the level.\n"),
        (
            "tests/env.rs",
            "fn t() { std::env::set_var(\"APP_LOG_LEVEL\", \"x\"); }\n",
        ),
    ])
}

#[test]
fn a_variable_shows_its_reader_what_it_calls_and_who_applies_the_values() {
    let out = outcome(
        &env_repo(),
        "where is APP_LOG_LEVEL read and applied?",
        QuestionKind::Config,
    );
    assert!(out.rescuing);
    assert_eq!(
        texts(&out.lead),
        vec![
            "### src/config.rs:7-10 — reads APP_LOG_LEVEL\n\
             7| pub fn load() -> Settings {\n\
             8|     let level = env::var(\"APP_LOG_LEVEL\").unwrap_or_default();\n\
             9|     Settings { level: normalize(level) }\n\
             10| }",
            "### src/config.rs:12-14 — callee of load\n\
             12| fn normalize(level: String) -> String {\n\
             13|     level.to_lowercase()\n\
             14| }",
            "### src/main.rs:3-6 — calls load, where the loaded values take effect\n\
             3| fn main() {\n\
             4|     let settings = config::load();\n\
             5|     run(settings);\n\
             6| }",
        ]
    );
    // The manual and the whole files of the code found are dropped from the
    // rest of the pack; the test that sets the variable was never a site.
    assert!(
        out.drops
            .drops_head("docs/env.md:1-1 — documentation section")
    );
    assert!(
        out.drops
            .drops_head("src/config.rs:1-14 — whole file (best match)")
    );
    assert!(
        out.drops
            .drops_head("src/main.rs:1-6 — file named 'main' (a word of the prompt)")
    );
    assert!(!out.drops.drops_head("src/lib.rs:1-3 — lib.rs region"));
    assert!(
        !out.drops
            .drops_head("src/config.rs:7-10 — load definition (function)")
    );
}

#[test]
fn a_test_that_sets_the_variable_is_not_a_site() {
    let repo = Repo::new(&[(
        "tests/env.rs",
        "fn t() {\n    std::env::set_var(\"APP_LOG_LEVEL\", \"x\");\n}\n",
    )]);
    assert!(
        outcome(&repo, "where is APP_LOG_LEVEL read", QuestionKind::Config)
            .lead
            .is_empty()
    );
    let inline = Repo::new(&[(
        "src/a.rs",
        "pub fn a() {}\n\n#[cfg(test)]\nmod tests {\n    fn t() {\n        std::env::var(\"APP_LOG_LEVEL\");\n    }\n}\n",
    )]);
    assert!(
        outcome(&inline, "where is APP_LOG_LEVEL read", QuestionKind::Config)
            .lead
            .is_empty()
    );
}

#[test]
fn a_variable_nobody_reads_falls_through_and_a_short_name_is_no_variable() {
    let repo = env_repo();
    assert!(
        outcome(
            &repo,
            "where is NOT_A_VARIABLE_HERE read",
            QuestionKind::Config
        )
        .lead
        .is_empty()
    );
    assert!(
        outcome(&repo, "where is A_B used", QuestionKind::Config)
            .lead
            .is_empty()
    );
}

const CLIENT: &str = "pub struct Options {\n    pub retry: u32,\n    pub verbose: bool,\n}\n\nimpl Options {\n    pub fn should_retry(&self, attempt: u32) -> bool {\n        attempt < self.retry\n    }\n}\n\npub fn send(options: &Options) {\n    let mut attempt = 0;\n    while options.should_retry(attempt) {\n        attempt += 1;\n    }\n}\n";

#[test]
fn the_definitions_that_apply_a_setting_are_shown_with_the_stems_that_name_them() {
    let repo = Repo::new(&[
        ("src/client.rs", CLIENT),
        ("src/other.rs", "pub fn unrelated() {}\n"),
        ("docs/retry.md", "The retry option.\n"),
    ]);
    let out = outcome(
        &repo,
        "which options control retry and where are they used?",
        QuestionKind::Config,
    );
    assert!(out.rescuing);
    let heads: Vec<&str> = out.lead.iter().map(Block::head).collect();
    assert_eq!(
        heads,
        [
            "src/client.rs:7-9 — names retry",
            "src/client.rs:12-17 — names retry",
            "src/client.rs:1-4 — names retry",
        ]
    );
    assert!(texts(&out.lead)[0].contains("8|         attempt < self.retry"));
    // Everything else of the pack must name the stem, and the registry and the
    // manual go.
    assert!(out.drops.drops_head("config registry"));
    assert!(out.drops.drops_head("docs/retry.md:1-1 — retry"));
    assert!(out.drops.drops_head("src/other.rs:1-1 — unrelated"));
    assert!(
        !out.drops
            .drops_head("src/other.rs:1-1 — retry usage in unrelated")
    );
}

#[test]
fn a_stem_every_file_uses_or_no_file_declares_names_no_site() {
    // `verbose` is declared once but one use is no site: two hits are needed.
    let repo = Repo::new(&[("src/client.rs", CLIENT)]);
    assert!(
        outcome(
            &repo,
            "which options control verbose and where are they used?",
            QuestionKind::Config
        )
        .lead
        .is_empty()
    );
    // A stem nothing declares (only used) is not a setting.
    let used_only = Repo::new(&[(
        "src/a.rs",
        "pub fn a(x: u32) {\n    helper(x);\n    helper(x + 1);\n}\n",
    )]);
    assert!(
        outcome(
            &used_only,
            "which options control helper and where are they used?",
            QuestionKind::Config
        )
        .lead
        .is_empty()
    );
}

#[test]
fn only_a_question_about_flags_and_where_they_apply_runs() {
    let repo = Repo::new(&[("src/client.rs", CLIENT)]);
    for prompt in [
        "which options control retry",
        "where is retry handled",
        "what is the retry delay",
    ] {
        assert!(
            outcome(&repo, prompt, QuestionKind::Config).lead.is_empty(),
            "{prompt}"
        );
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let off = Steps::default()
        .without(&["cand-k-code-sites".to_string()])
        .unwrap();
    assert!(
        blocks(
            "which options control retry and where are they used?",
            QuestionKind::Config,
            off,
            &[],
            &repo,
            deadline
        )
        .lead
        .is_empty()
    );
}

#[test]
fn definitions_end_where_their_language_says() {
    let lines = |text: &str| -> Vec<String> { text.lines().map(ToString::to_string).collect() };
    let rust = lines("fn a() {\n    x();\n}\nfn b();\n");
    assert_eq!(decl_end(&rust, 0), 2);
    assert_eq!(decl_end(&rust, 3), 3);
    let python = lines("def a():\n    x()\n\n    y()\n\ndef b():\n    pass\n");
    assert_eq!(decl_end(&python, 0), 4);
    assert_eq!(decl_end(&python, 5), 6);
    let open = lines("fn a() {\n    x();\n");
    assert_eq!(decl_end(&open, 0), 1);
}

#[test]
fn a_window_starts_at_the_definition_and_then_centres_on_the_hits() {
    assert_eq!(window(10, 20, &[], 40), (10, 20));
    assert_eq!(window(0, 100, &[], 20), (0, 19));
    assert_eq!(window(0, 100, &[80, 90], 20), (75, 94));
    assert_eq!(window(0, 100, &[99], 20), (81, 100));
    assert_eq!(window(50, 100, &[51], 20), (50, 69));
}

#[test]
fn stems_drop_question_words_and_cut_one_inflection() {
    assert_eq!(
        stems_of("which options control retries and where are they used"),
        ["control", "retri"]
    );
    assert_eq!(
        stems_of("which flags govern following redirects"),
        ["govern", "follow", "redirect"]
    );
    assert!(parts_of("followRedirects_max2").contains("redirects"));
    assert!(has_ident("let X = Retry_count;", "retry_count"));
    assert!(!has_ident("let X = retry_counter;", "retry_count"));
    let weigher = Weigher::new("retry").unwrap();
    assert!((weigher.weight("pub retry: u32,") - 0.3).abs() < f64::EPSILON);
    assert!((weigher.weight("self.retry(3)") - 2.0).abs() < f64::EPSILON);
    assert!((weigher.weight("let n = self.retry;") - 1.0).abs() < f64::EPSILON);
    assert!((weigher.weight("if self.retry > 1 {") - 2.0).abs() < f64::EPSILON);
}
