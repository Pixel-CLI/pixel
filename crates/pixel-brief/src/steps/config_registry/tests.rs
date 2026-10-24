// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use std::time::{Duration, Instant};

use super::*;
use crate::steps::testing::{Repo, texts};
use crate::steps::{Steps, blocks};

fn ask(repo: &Repo, typed: &str) -> Vec<String> {
    ask_as(repo, typed, QuestionKind::Config)
}

fn ask_as(repo: &Repo, typed: &str, kind: QuestionKind) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    texts(&blocks(typed, kind, Steps::default(), &[], repo, deadline).lead)
}

const SERVER: &str = "// Port the dev server listens on.\nexport const DEFAULT_PORT = 3000;\nconst timeout = Number(process.env.REQUEST_TIMEOUT_MS ?? 5000);\nconst token = process.env.API_TOKEN;\n";

#[test]
fn an_environment_read_shows_its_fallback_where_it_is_read_and_masks_a_secret_line() {
    let repo = Repo::new(&[("src/server.ts", SERVER)]);
    assert_eq!(
        ask(&repo, "where do I set the request timeout setting"),
        vec![
            "### config registry — keys matching request, timeout, setting (key | value or fallback | declared | read at)\n\
             - REQUEST_TIMEOUT_MS | code fallback 5000; an env file may override | not declared | read at src/server.ts:3",
            "### src/server.ts:2-4\n\
             2| export const DEFAULT_PORT = 3000;\n\
             3| const timeout = Number(process.env.REQUEST_TIMEOUT_MS ?? 5000);\n\
             4| <masked>",
        ]
    );
}

#[test]
fn an_exported_constant_shows_its_value_and_the_comment_above_it() {
    let repo = Repo::new(&[("src/server.ts", SERVER)]);
    assert_eq!(
        ask(&repo, "where is the default port set"),
        vec![
            "### config registry — keys matching default, port (key | value or fallback | declared | read at)\n\
             - DEFAULT_PORT | constant default 3000 | src/server.ts:2 | read at no code read",
            "### src/server.ts:1-2\n\
             1| // Port the dev server listens on.\n\
             2| export const DEFAULT_PORT = 3000;",
        ]
    );
}

#[test]
fn a_dev_server_port_is_found_in_a_config_file() {
    let repo = Repo::new(&[(
        "vite.config.ts",
        "export default {\n  server: {\n    port: 5173,\n  },\n};\n",
    )]);
    let shown = ask(&repo, "what port does the dev server use");
    assert_eq!(shown.len(), 2, "{shown:?}");
    assert!(
        shown[0].contains(
            "- dev-server port | constant default 5173 | vite.config.ts:3 | read at no code read"
        ),
        "{}",
        shown[0]
    );
    assert_eq!(
        shown[1],
        "### vite.config.ts:2-4\n2|   server: {\n3|     port: 5173,\n4|   },"
    );
}

#[test]
fn a_key_that_looks_like_a_secret_is_never_shown_with_a_value() {
    let repo = Repo::new(&[(
        "src/auth.py",
        "API_TOKEN_TTL = 3600\nAPI_KEY = \"hunter2\"\n",
    )]);
    let shown = ask(&repo, "where is the api key setting");
    assert!(
        shown[0].contains("- API_KEY | constant default <masked> | src/auth.py:2 |"),
        "{}",
        shown[0]
    );
    let all = shown.join("\n");
    assert!(!all.contains("hunter2"), "{all}");
    assert!(all.contains("2| <masked>"), "{all}");
}

#[test]
fn several_declarations_of_one_key_stay_candidates_with_an_omission_count() {
    let files: Vec<(String, String)> = (0..6)
        .map(|n| {
            (
                format!("pkg{n}/limits.ts"),
                format!("export const MAX_RETRIES = {n};\n"),
            )
        })
        .collect();
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    let repo = Repo::new(&borrowed);
    let shown = ask(&repo, "what is the retries setting");
    let head = &shown[0];
    assert!(
        head.contains("- MAX_RETRIES | 6 distinct declaration candidates; lexical binding and override precedence unknown"),
        "{head}"
    );
    assert!(
        head.contains("  - MAX_RETRIES @ pkg0/limits.ts:1 | candidate value 0"),
        "{head}"
    );
    assert!(
        head.contains("  - 2 additional candidates omitted; no unique default established"),
        "{head}"
    );
    assert_eq!(
        shown.len(),
        1 + MAX_CANDIDATES,
        "one excerpt per candidate shown"
    );
}

#[test]
fn a_bound_only_question_needs_a_bound_declaration_and_a_subject() {
    let repo = Repo::new(&[
        (
            "src/http.ts",
            "// Largest body accepted, in bytes.\nexport const MAX_BODY_BYTES = 1048576;\nexport const BODY_ENCODING = \"utf-8\";\n",
        ),
        ("src/log.ts", "export const LOG_LEVEL = \"info\";\n"),
    ]);
    let shown = ask(&repo, "what is the maximum body size");
    assert!(
        shown[0].contains("- MAX_BODY_BYTES | constant default 1048576 |"),
        "{}",
        shown[0]
    );
    assert!(!shown[0].contains("BODY_ENCODING"), "{}", shown[0]);
    assert!(
        ask(&repo, "what is the maximum log").is_empty(),
        "no bound declared for log"
    );
    assert!(ask(&repo, "what is the maximum").is_empty(), "no subject");
}

#[test]
fn a_literal_setting_call_is_quoted_without_inferring_its_effect() {
    let repo = Repo::new(&[(
        "src/init.ts",
        "import { settings } from './s';\nsettings.set('max-retries', 3);\nsettings.set('api-secret', 'x');\n",
    )]);
    let shown = ask(&repo, "where is the retries setting configured");
    assert_eq!(
        shown,
        vec![
            "### src/init.ts:1-3 — literal setting call candidate; inspect surrounding control flow\n\
             1| import { settings } from './s';\n\
             2| settings.set('max-retries', 3);\n\
             3| <masked>"
        ]
    );
    let secret = ask(&repo, "where is the secret setting configured");
    assert!(
        secret[0].contains("2| <masked>") && secret[0].contains("3| <masked>"),
        "{}",
        secret[0]
    );
}

#[test]
fn tests_environment_files_and_non_config_questions_are_left_out() {
    let repo = Repo::new(&[
        ("src/a.ts", "const a = process.env.PUBLIC_TIMEOUT_MS;\n"),
        (
            "tests/a.test.ts",
            "const b = process.env.TEST_ONLY_TIMEOUT;\n",
        ),
        (".env.example", "EXAMPLE_TIMEOUT=30\n"),
        (
            "node_modules/x/index.js",
            "const c = process.env.VENDORED_TIMEOUT;\n",
        ),
    ]);
    let shown = ask(&repo, "which environment variable sets the timeout").join("\n");
    assert!(shown.contains("PUBLIC_TIMEOUT_MS"), "{shown}");
    for left_out in ["TEST_ONLY_TIMEOUT", "EXAMPLE_TIMEOUT", "VENDORED_TIMEOUT"] {
        assert!(!shown.contains(left_out), "{left_out}: {shown}");
    }
    assert!(ask(&repo, "fix the build").is_empty());
    for kind in [
        QuestionKind::Lookup,
        QuestionKind::Flow,
        QuestionKind::Tests,
        QuestionKind::Bugfix,
    ] {
        assert!(
            ask_as(&repo, "which environment variable sets the timeout", kind).is_empty(),
            "{kind:?}"
        );
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let off = Steps::default()
        .without(&["config-registry".to_string()])
        .unwrap();
    assert!(
        blocks(
            "which environment variable sets the timeout",
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
fn keys_stems_and_masks_follow_the_prototype() {
    assert_eq!(key_tokens("MAX_BODY_SIZE"), ["max", "body", "size"]);
    assert_eq!(key_tokens("maxBodySize"), ["max", "body", "size"]);
    assert_eq!(key_tokens("dev-server"), ["dev", "server"]);
    let wanted: BTreeSet<String> = ["byte", "file"].map(String::from).into();
    assert_eq!(
        stems(["bytes", "files", "process"]),
        BTreeSet::from(["byte".into(), "file".into(), "proce".into()])
    );
    assert!(
        stems(["bytes"]).is_superset(&BTreeSet::from(["byte".to_string()])) && wanted.len() == 2
    );
    assert_eq!(
        words_of("Where do I set the API timeout locally?"),
        ["api", "timeout"]
    );
    assert_eq!(mask("API_KEY", "abc"), "<masked>");
    assert_eq!(mask("API_KEY", ""), "");
    assert_eq!(mask("PORT", "80"), "80");
    assert_eq!(
        mask("APP_CONFIG_DIR", "/etc"),
        "<masked>",
        "the prototype masks `config` too"
    );
}
