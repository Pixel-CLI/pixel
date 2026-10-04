// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use super::*;

/// The `--account` shortcut picks the account variable the flow itself
/// declares, and a malformed `--var` is refused before anything opens.
#[test]
fn flow_vars_parses_the_pairs_and_uses_the_flows_account_var() {
    let _guard = crate::ENV_LOCK.lock().unwrap();
    let dir = std::env::temp_dir().join(format!("pixel-flow-vars-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // SAFETY: ENV_LOCK serialises every test that touches process-wide
    // variables, and PIXEL_FLOW_DIR is one of them.
    unsafe {
        std::env::set_var("PIXEL_FLOW_DIR", &dir);
    }
    let store = |name: &str, vars: &[(&str, &str)]| {
        let vars: Vec<String> = vars
            .iter()
            .map(|(n, d)| format!(r#"{{"name": "{n}", "description": "{d}", "required": false}}"#))
            .collect();
        std::fs::write(
            dir.join(format!("{name}.json")),
            format!(
                r#"{{"name": "{name}", "title": "t", "description": "",
                    "vars": [{}], "steps": [{{"action": "snapshot"}}],
                    "created_unix": 1, "revised_unix": 1, "revision": 1, "proven": false }}"#,
                vars.join(", ")
            ),
        )
        .unwrap();
    };
    store(
        "codex",
        &[
            ("openai_account", "the Codex account"),
            ("env_name", "which env"),
        ],
    );
    // A trailing variable after google_account must not win the lookup.
    store(
        "claude",
        &[
            ("google_account", "which account"),
            ("env_name", "which env"),
        ],
    );

    // `--account` lands on the variable the flow itself declares.
    let vars = flow_vars(
        "codex",
        &["env=prod".to_string()],
        &Some("bob@example.com".to_string()),
    )
    .unwrap();
    assert_eq!(
        vars.get("openai_account").map(String::as_str),
        Some("bob@example.com")
    );
    assert_eq!(vars.get("env").map(String::as_str), Some("prod"));
    // A flow without an openai_account falls back to google_account.
    let vars = flow_vars("claude", &[], &Some("carol@example.com".to_string())).unwrap();
    assert_eq!(
        vars.get("google_account").map(String::as_str),
        Some("carol@example.com")
    );

    // The pairs are read in order and a malformed one is refused.
    let vars = flow_vars("codex", &["a=1".to_string(), "b=2".to_string()], &None).unwrap();
    assert_eq!(vars.get("a").map(String::as_str), Some("1"));
    assert_eq!(vars.get("b").map(String::as_str), Some("2"));
    assert_eq!(
        flow_vars("codex", &["broken".to_string()], &None).unwrap_err(),
        "--var expects key=value, got 'broken'"
    );
    // SAFETY: as above.
    unsafe {
        std::env::remove_var("PIXEL_FLOW_DIR");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
