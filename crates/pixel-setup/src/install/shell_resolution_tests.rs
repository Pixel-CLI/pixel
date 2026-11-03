// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use super::{parse_dscl_user_shell, parse_passwd_shell, resolve_shell_from};

/// The order is override, account, `$SHELL`: an agent's tool shell in
/// `$SHELL` must lose to the account's login shell, and an empty value
/// at any level must not shadow the next one.
#[test]
fn override_beats_account_beats_env_and_empty_values_are_skipped() {
    let fish = || Some("/opt/homebrew/bin/fish".to_string());
    let zsh = || Some("/bin/zsh".to_string());
    assert_eq!(resolve_shell_from(Some("bash"), fish(), zsh()), "bash");
    assert_eq!(
        resolve_shell_from(None, fish(), zsh()),
        "/opt/homebrew/bin/fish"
    );
    assert_eq!(resolve_shell_from(None, None, zsh()), "/bin/zsh");
    assert_eq!(
        resolve_shell_from(None, Some(" ".into()), zsh()),
        "/bin/zsh"
    );
    assert_eq!(resolve_shell_from(None, None, Some(String::new())), "");
    assert_eq!(resolve_shell_from(None, None, None), "");
}

#[test]
fn dscl_output_yields_the_user_shell_line_only() {
    assert_eq!(
        parse_dscl_user_shell("UserShell: /opt/homebrew/bin/fish\n"),
        Some("/opt/homebrew/bin/fish".to_string())
    );
    assert_eq!(
        parse_dscl_user_shell(
            "RecordName: navid\nUserShell:\t/bin/zsh\nNFSHomeDirectory: /Users/navid\n"
        ),
        Some("/bin/zsh".to_string())
    );
    assert_eq!(parse_dscl_user_shell("UserShell:\n"), None);
    assert_eq!(parse_dscl_user_shell("No such key: UserShell\n"), None);
    assert_eq!(parse_dscl_user_shell(""), None);
}

#[test]
fn passwd_text_yields_the_seventh_field_of_the_exact_user() {
    let passwd = "root:x:0:0:root:/root:/bin/bash\n\
                  navid:x:501:20:Navid:/home/navid:/usr/bin/fish\n\
                  navidx:x:502:20::/home/navidx:/bin/sh\n";
    assert_eq!(
        parse_passwd_shell(passwd, "navid"),
        Some("/usr/bin/fish".to_string())
    );
    assert_eq!(
        parse_passwd_shell(passwd, "navidx"),
        Some("/bin/sh".to_string()),
        "exact name, not prefix"
    );
    assert_eq!(parse_passwd_shell(passwd, "nobody"), None);
    assert_eq!(parse_passwd_shell("short:x:1:1\n", "short"), None);
    assert_eq!(
        parse_passwd_shell("empty:x:1:1::/home/empty:\n", "empty"),
        None
    );
}
