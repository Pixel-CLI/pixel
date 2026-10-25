// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The step on small repositories, with the script's own output as the
//! expectation: each `expected` is what `probes/cand-k-code-sites/probe`
//! printed on the same files, prompt and brief.

use super::*;
use crate::execution_brief::steps::testing::{repo, run};

/// One case: the repository, the question and what the script printed.
struct Case {
    prompt: &'static str,
    brief: &'static str,
    files: &'static [(&'static str, &'static str)],
    expected: &'static str,
}

fn ask(case: &Case) -> String {
    let dir = repo(case.files);
    run(&STEP, dir.path(), case.prompt, case.brief)
}

const ENV_BASIC: Case = Case {
    prompt: "where is MYAPP_CONFIG_DIR read and applied?",
    brief: r#"[PIXEL:BRIEF]
### README.md:1-3 — file named config
1| # myapp
### src/config.rs:1-9 — whole file
1| use std::env;
### src/other.rs:1-3 — file named other
"#,
    files: &[
        ("src/config.rs", r#"use std::env;

/// Where the settings live: MYAPP_CONFIG_DIR, else the home directory.
pub fn load_dirs(profile: &str) -> Vec<String> {
    let base = match env::var("MYAPP_CONFIG_DIR") {
        Ok(dir) => expand_home(&dir),
        Err(_) => default_dir(),
    };
    vec![format!("{base}/{profile}")]
}

fn expand_home(dir: &str) -> String {
    dir.replace("~", &home_dir())
}

fn default_dir() -> String {
    format!("{}/.myapp", home_dir())
}

fn home_dir() -> String {
    env::var("HOME").unwrap_or_default()
}

fn unrelated_helper() -> u32 {
    4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_directory() {
        std::env::set_var("MYAPP_CONFIG_DIR", "/x");
        assert_eq!(load_dirs("a"), vec!["/x/a".to_string()]);
    }
}
"#),
        ("src/main.rs", r#"mod config;

fn main() {
    let dirs = config::load_dirs("default");
    for dir in dirs {
        println!("{dir}");
    }
}
"#),
        ("README.md", r#"# myapp

MYAPP_CONFIG_DIR sets the directory.
"#),
        ("doc/myapp.1", r#"MYAPP_CONFIG_DIR
"#),
    ],
    expected: r#"drop: ^\S*\.(md|rst|txt|adoc|[0-9])\b:|^\S*/doc/
drop: ^src/config\.rs:\d+-\d+ — (file named|whole file)
drop: ^src/main\.rs:\d+-\d+ — (file named|whole file)
### src/config.rs:3-10 — reads MYAPP_CONFIG_DIR
3| /// Where the settings live: MYAPP_CONFIG_DIR, else the home directory.
4| pub fn load_dirs(profile: &str) -> Vec<String> {
5|     let base = match env::var("MYAPP_CONFIG_DIR") {
6|         Ok(dir) => expand_home(&dir),
7|         Err(_) => default_dir(),
8|     };
9|     vec![format!("{base}/{profile}")]
10| }
### src/config.rs:12-14 — callee of load_dirs
12| fn expand_home(dir: &str) -> String {
13|     dir.replace("~", &home_dir())
14| }
### src/config.rs:16-18 — callee of load_dirs
16| fn default_dir() -> String {
17|     format!("{}/.myapp", home_dir())
18| }
### src/config.rs:20-22 — callee of load_dirs
20| fn home_dir() -> String {
21|     env::var("HOME").unwrap_or_default()
22| }
### src/main.rs:3-8 — calls load_dirs, where the loaded values take effect
3| fn main() {
4|     let dirs = config::load_dirs("default");
5|     for dir in dirs {
6|         println!("{dir}");
7|     }
8| }
"#,
};

#[test]
fn env_basic_prints_what_the_script_prints() {
    assert_eq!(ask(&ENV_BASIC), ENV_BASIC.expected);
}

const ENV_NO_BRIEF: Case = Case {
    prompt: "where is MYAPP_CONFIG_DIR read?",
    brief: "",
    files: &[
        ("src/config.rs", r#"use std::env;

/// Where the settings live: MYAPP_CONFIG_DIR, else the home directory.
pub fn load_dirs(profile: &str) -> Vec<String> {
    let base = match env::var("MYAPP_CONFIG_DIR") {
        Ok(dir) => expand_home(&dir),
        Err(_) => default_dir(),
    };
    vec![format!("{base}/{profile}")]
}

fn expand_home(dir: &str) -> String {
    dir.replace("~", &home_dir())
}

fn default_dir() -> String {
    format!("{}/.myapp", home_dir())
}

fn home_dir() -> String {
    env::var("HOME").unwrap_or_default()
}

fn unrelated_helper() -> u32 {
    4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_directory() {
        std::env::set_var("MYAPP_CONFIG_DIR", "/x");
        assert_eq!(load_dirs("a"), vec!["/x/a".to_string()]);
    }
}
"#),
        ("src/main.rs", r#"mod config;

fn main() {
    let dirs = config::load_dirs("default");
    for dir in dirs {
        println!("{dir}");
    }
}
"#),
        ("README.md", r#"# myapp

MYAPP_CONFIG_DIR sets the directory.
"#),
        ("doc/myapp.1", r#"MYAPP_CONFIG_DIR
"#),
    ],
    expected: r#"drop: ^\S*\.(md|rst|txt|adoc|[0-9])\b:|^\S*/doc/
drop: ^src/config\.rs:\d+-\d+ — (file named|whole file)
drop: ^src/main\.rs:\d+-\d+ — (file named|whole file)
### src/config.rs:3-10 — reads MYAPP_CONFIG_DIR
3| /// Where the settings live: MYAPP_CONFIG_DIR, else the home directory.
4| pub fn load_dirs(profile: &str) -> Vec<String> {
5|     let base = match env::var("MYAPP_CONFIG_DIR") {
6|         Ok(dir) => expand_home(&dir),
7|         Err(_) => default_dir(),
8|     };
9|     vec![format!("{base}/{profile}")]
10| }
### src/config.rs:12-14 — callee of load_dirs
12| fn expand_home(dir: &str) -> String {
13|     dir.replace("~", &home_dir())
14| }
### src/config.rs:16-18 — callee of load_dirs
16| fn default_dir() -> String {
17|     format!("{}/.myapp", home_dir())
18| }
### src/config.rs:20-22 — callee of load_dirs
20| fn home_dir() -> String {
21|     env::var("HOME").unwrap_or_default()
22| }
### src/main.rs:3-8 — calls load_dirs, where the loaded values take effect
3| fn main() {
4|     let dirs = config::load_dirs("default");
5|     for dir in dirs {
6|         println!("{dir}");
7|     }
8| }
"#,
};

#[test]
fn env_no_brief_prints_what_the_script_prints() {
    assert_eq!(ask(&ENV_NO_BRIEF), ENV_NO_BRIEF.expected);
}

const ENV_BARE_CALLER: Case = Case {
    prompt: "where is MYAPP_CONFIG_DIR checked",
    brief: "",
    files: &[
        ("src/config.rs", r#"use std::env;

/// Where the settings live: MYAPP_CONFIG_DIR, else the home directory.
pub fn load_dirs(profile: &str) -> Vec<String> {
    let base = match env::var("MYAPP_CONFIG_DIR") {
        Ok(dir) => expand_home(&dir),
        Err(_) => default_dir(),
    };
    vec![format!("{base}/{profile}")]
}

fn expand_home(dir: &str) -> String {
    dir.replace("~", &home_dir())
}

fn default_dir() -> String {
    format!("{}/.myapp", home_dir())
}

fn home_dir() -> String {
    env::var("HOME").unwrap_or_default()
}

fn unrelated_helper() -> u32 {
    4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_directory() {
        std::env::set_var("MYAPP_CONFIG_DIR", "/x");
        assert_eq!(load_dirs("a"), vec!["/x/a".to_string()]);
    }
}
"#),
        ("src/main.rs", r#"mod config;
"#),
        ("README.md", r#"# myapp

MYAPP_CONFIG_DIR sets the directory.
"#),
        ("doc/myapp.1", r#"MYAPP_CONFIG_DIR
"#),
        ("src/start.rs", r#"fn start() {
    let dirs = load_dirs("x");
    run(dirs);
}
"#),
    ],
    expected: r#"drop: ^\S*\.(md|rst|txt|adoc|[0-9])\b:|^\S*/doc/
drop: ^src/config\.rs:\d+-\d+ — (file named|whole file)
drop: ^src/start\.rs:\d+-\d+ — (file named|whole file)
### src/config.rs:3-10 — reads MYAPP_CONFIG_DIR
3| /// Where the settings live: MYAPP_CONFIG_DIR, else the home directory.
4| pub fn load_dirs(profile: &str) -> Vec<String> {
5|     let base = match env::var("MYAPP_CONFIG_DIR") {
6|         Ok(dir) => expand_home(&dir),
7|         Err(_) => default_dir(),
8|     };
9|     vec![format!("{base}/{profile}")]
10| }
### src/config.rs:12-14 — callee of load_dirs
12| fn expand_home(dir: &str) -> String {
13|     dir.replace("~", &home_dir())
14| }
### src/config.rs:16-18 — callee of load_dirs
16| fn default_dir() -> String {
17|     format!("{}/.myapp", home_dir())
18| }
### src/config.rs:20-22 — callee of load_dirs
20| fn home_dir() -> String {
21|     env::var("HOME").unwrap_or_default()
22| }
### src/config.rs:32-36 — calls load_dirs, where the loaded values take effect
32|     #[test]
33|     fn reads_the_directory() {
34|         std::env::set_var("MYAPP_CONFIG_DIR", "/x");
35|         assert_eq!(load_dirs("a"), vec!["/x/a".to_string()]);
36|     }
### src/start.rs:1-4 — calls load_dirs, where the loaded values take effect
1| fn start() {
2|     let dirs = load_dirs("x");
3|     run(dirs);
4| }
"#,
};

#[test]
fn env_bare_caller_prints_what_the_script_prints() {
    assert_eq!(ask(&ENV_BARE_CALLER), ENV_BARE_CALLER.expected);
}

const ENV_PYTHON: Case = Case {
    prompt: "where is APP_TOKEN_FILE used",
    brief: "",
    files: &[
        ("settings.py", r#"import os


def load_settings(path=None):
    """The settings: APP_TOKEN_FILE names the file."""
    name = os.environ.get("APP_TOKEN_FILE", "token")
    return read_token(name)


def read_token(name):
    return open(name).read().strip()


class Store:
    def fetch(self):
        return read_token("x")
"#),
        ("client.py", r#"from settings import load_settings


def connect():
    token = load_settings()
    return token
"#),
    ],
    expected: r#"drop: ^\S*\.(md|rst|txt|adoc|[0-9])\b:|^\S*/doc/
drop: ^client\.py:\d+-\d+ — (file named|whole file)
drop: ^settings\.py:\d+-\d+ — (file named|whole file)
### settings.py:4-9 — reads APP_TOKEN_FILE
4| def load_settings(path=None):
5|     """The settings: APP_TOKEN_FILE names the file."""
6|     name = os.environ.get("APP_TOKEN_FILE", "token")
7|     return read_token(name)
8| 
9| 
### settings.py:10-13 — callee of load_settings
10| def read_token(name):
11|     return open(name).read().strip()
12| 
13| 
### client.py:4-6 — calls load_settings, where the loaded values take effect
4| def connect():
5|     token = load_settings()
6|     return token
"#,
};

#[test]
fn env_python_prints_what_the_script_prints() {
    assert_eq!(ask(&ENV_PYTHON), ENV_PYTHON.expected);
}

const ENV_HIDDEN_DIR: Case = Case {
    prompt: "where is MYAPP_CONFIG_DIR read",
    brief: "",
    files: &[
        (".hidden/config.rs", r#"use std::env;

/// Where the settings live: MYAPP_CONFIG_DIR, else the home directory.
pub fn load_dirs(profile: &str) -> Vec<String> {
    let base = match env::var("MYAPP_CONFIG_DIR") {
        Ok(dir) => expand_home(&dir),
        Err(_) => default_dir(),
    };
    vec![format!("{base}/{profile}")]
}

fn expand_home(dir: &str) -> String {
    dir.replace("~", &home_dir())
}

fn default_dir() -> String {
    format!("{}/.myapp", home_dir())
}

fn home_dir() -> String {
    env::var("HOME").unwrap_or_default()
}

fn unrelated_helper() -> u32 {
    4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_directory() {
        std::env::set_var("MYAPP_CONFIG_DIR", "/x");
        assert_eq!(load_dirs("a"), vec!["/x/a".to_string()]);
    }
}
"#),
    ],
    expected: "",
};

#[test]
fn env_hidden_dir_prints_what_the_script_prints() {
    assert_eq!(ask(&ENV_HIDDEN_DIR), ENV_HIDDEN_DIR.expected);
}

const ENV_DOC_ONLY: Case = Case {
    prompt: "where is MYAPP_CONFIG_DIR read",
    brief: "",
    files: &[
        ("README.md", r#"# myapp

MYAPP_CONFIG_DIR sets the directory.
"#),
    ],
    expected: "",
};

#[test]
fn env_doc_only_prints_what_the_script_prints() {
    assert_eq!(ask(&ENV_DOC_ONLY), ENV_DOC_ONLY.expected);
}

const ENV_TEST_ONLY: Case = Case {
    prompt: "where is MYAPP_CONFIG_DIR read",
    brief: "",
    files: &[
        ("src/config.rs", r#"#[cfg(test)]
mod tests {
    fn t() {
        std::env::set_var("MYAPP_CONFIG_DIR", "x");
    }
}
"#),
    ],
    expected: "",
};

#[test]
fn env_test_only_prints_what_the_script_prints() {
    assert_eq!(ask(&ENV_TEST_ONLY), ENV_TEST_ONLY.expected);
}

const ENV_TWO_TOKENS: Case = Case {
    prompt: "where are MYAPP_CONFIG_DIR and APP_TOKEN_FILE read",
    brief: "",
    files: &[
        ("src/config.rs", r#"use std::env;

/// Where the settings live: MYAPP_CONFIG_DIR, else the home directory.
pub fn load_dirs(profile: &str) -> Vec<String> {
    let base = match env::var("MYAPP_CONFIG_DIR") {
        Ok(dir) => expand_home(&dir),
        Err(_) => default_dir(),
    };
    vec![format!("{base}/{profile}")]
}

fn expand_home(dir: &str) -> String {
    dir.replace("~", &home_dir())
}

fn default_dir() -> String {
    format!("{}/.myapp", home_dir())
}

fn home_dir() -> String {
    env::var("HOME").unwrap_or_default()
}

fn unrelated_helper() -> u32 {
    4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_directory() {
        std::env::set_var("MYAPP_CONFIG_DIR", "/x");
        assert_eq!(load_dirs("a"), vec!["/x/a".to_string()]);
    }
}
"#),
        ("src/main.rs", r#"mod config;

fn main() {
    let dirs = config::load_dirs("default");
    for dir in dirs {
        println!("{dir}");
    }
}
"#),
        ("README.md", r#"# myapp

MYAPP_CONFIG_DIR sets the directory.
"#),
        ("doc/myapp.1", r#"MYAPP_CONFIG_DIR
"#),
        ("settings.py", r#"import os


def load_settings(path=None):
    """The settings: APP_TOKEN_FILE names the file."""
    name = os.environ.get("APP_TOKEN_FILE", "token")
    return read_token(name)


def read_token(name):
    return open(name).read().strip()


class Store:
    def fetch(self):
        return read_token("x")
"#),
    ],
    expected: r#"drop: ^\S*\.(md|rst|txt|adoc|[0-9])\b:|^\S*/doc/
drop: ^settings\.py:\d+-\d+ — (file named|whole file)
drop: ^src/config\.rs:\d+-\d+ — (file named|whole file)
drop: ^src/main\.rs:\d+-\d+ — (file named|whole file)
### src/config.rs:3-10 — reads MYAPP_CONFIG_DIR
3| /// Where the settings live: MYAPP_CONFIG_DIR, else the home directory.
4| pub fn load_dirs(profile: &str) -> Vec<String> {
5|     let base = match env::var("MYAPP_CONFIG_DIR") {
6|         Ok(dir) => expand_home(&dir),
7|         Err(_) => default_dir(),
8|     };
9|     vec![format!("{base}/{profile}")]
10| }
### src/config.rs:12-14 — callee of load_dirs
12| fn expand_home(dir: &str) -> String {
13|     dir.replace("~", &home_dir())
14| }
### src/config.rs:16-18 — callee of load_dirs
16| fn default_dir() -> String {
17|     format!("{}/.myapp", home_dir())
18| }
### src/config.rs:20-22 — callee of load_dirs
20| fn home_dir() -> String {
21|     env::var("HOME").unwrap_or_default()
22| }
### src/main.rs:3-8 — calls load_dirs, where the loaded values take effect
3| fn main() {
4|     let dirs = config::load_dirs("default");
5|     for dir in dirs {
6|         println!("{dir}");
7|     }
8| }
### settings.py:4-9 — reads APP_TOKEN_FILE
4| def load_settings(path=None):
5|     """The settings: APP_TOKEN_FILE names the file."""
6|     name = os.environ.get("APP_TOKEN_FILE", "token")
7|     return read_token(name)
8| 
9| 
### settings.py:10-13 — callee of load_settings
10| def read_token(name):
11|     return open(name).read().strip()
12| 
13| 
"#,
};

#[test]
fn env_two_tokens_prints_what_the_script_prints() {
    assert_eq!(ask(&ENV_TWO_TOKENS), ENV_TWO_TOKENS.expected);
}

const ENV_SHORT_TOKEN_IGNORED: Case = Case {
    prompt: "where is A_B read",
    brief: "",
    files: &[
        ("src/config.rs", r#"use std::env;

/// Where the settings live: MYAPP_CONFIG_DIR, else the home directory.
pub fn load_dirs(profile: &str) -> Vec<String> {
    let base = match env::var("MYAPP_CONFIG_DIR") {
        Ok(dir) => expand_home(&dir),
        Err(_) => default_dir(),
    };
    vec![format!("{base}/{profile}")]
}

fn expand_home(dir: &str) -> String {
    dir.replace("~", &home_dir())
}

fn default_dir() -> String {
    format!("{}/.myapp", home_dir())
}

fn home_dir() -> String {
    env::var("HOME").unwrap_or_default()
}

fn unrelated_helper() -> u32 {
    4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_directory() {
        std::env::set_var("MYAPP_CONFIG_DIR", "/x");
        assert_eq!(load_dirs("a"), vec!["/x/a".to_string()]);
    }
}
"#),
        ("src/main.rs", r#"mod config;

fn main() {
    let dirs = config::load_dirs("default");
    for dir in dirs {
        println!("{dir}");
    }
}
"#),
        ("README.md", r#"# myapp

MYAPP_CONFIG_DIR sets the directory.
"#),
        ("doc/myapp.1", r#"MYAPP_CONFIG_DIR
"#),
    ],
    expected: "",
};

#[test]
fn env_short_token_ignored_prints_what_the_script_prints() {
    assert_eq!(ask(&ENV_SHORT_TOKEN_IGNORED), ENV_SHORT_TOKEN_IGNORED.expected);
}

const ENV_PATH_ORDER: Case = Case {
    prompt: "where is MYAPP_CONFIG_DIR read",
    brief: "",
    files: &[
        ("m/n.rs", r#"fn caller_n() {
    config::load_dirs("n");
}
"#),
        ("m.rs", r#"fn caller_m() {
    config::load_dirs("m");
}
"#),
        ("m-x.rs", r#"fn caller_x() {
    config::load_dirs("x");
}
"#),
        ("z.rs", r#"fn caller_z() {
    config::load_dirs("z");
}
"#),
        ("src/config.rs", r#"use std::env;

/// Where the settings live: MYAPP_CONFIG_DIR, else the home directory.
pub fn load_dirs(profile: &str) -> Vec<String> {
    let base = match env::var("MYAPP_CONFIG_DIR") {
        Ok(dir) => expand_home(&dir),
        Err(_) => default_dir(),
    };
    vec![format!("{base}/{profile}")]
}

fn expand_home(dir: &str) -> String {
    dir.replace("~", &home_dir())
}

fn default_dir() -> String {
    format!("{}/.myapp", home_dir())
}

fn home_dir() -> String {
    env::var("HOME").unwrap_or_default()
}

fn unrelated_helper() -> u32 {
    4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_directory() {
        std::env::set_var("MYAPP_CONFIG_DIR", "/x");
        assert_eq!(load_dirs("a"), vec!["/x/a".to_string()]);
    }
}
"#),
    ],
    expected: r#"drop: ^\S*\.(md|rst|txt|adoc|[0-9])\b:|^\S*/doc/
drop: ^m\-x\.rs:\d+-\d+ — (file named|whole file)
drop: ^m/n\.rs:\d+-\d+ — (file named|whole file)
drop: ^src/config\.rs:\d+-\d+ — (file named|whole file)
### src/config.rs:3-10 — reads MYAPP_CONFIG_DIR
3| /// Where the settings live: MYAPP_CONFIG_DIR, else the home directory.
4| pub fn load_dirs(profile: &str) -> Vec<String> {
5|     let base = match env::var("MYAPP_CONFIG_DIR") {
6|         Ok(dir) => expand_home(&dir),
7|         Err(_) => default_dir(),
8|     };
9|     vec![format!("{base}/{profile}")]
10| }
### src/config.rs:12-14 — callee of load_dirs
12| fn expand_home(dir: &str) -> String {
13|     dir.replace("~", &home_dir())
14| }
### src/config.rs:16-18 — callee of load_dirs
16| fn default_dir() -> String {
17|     format!("{}/.myapp", home_dir())
18| }
### src/config.rs:20-22 — callee of load_dirs
20| fn home_dir() -> String {
21|     env::var("HOME").unwrap_or_default()
22| }
### m/n.rs:1-3 — calls load_dirs, where the loaded values take effect
1| fn caller_n() {
2|     config::load_dirs("n");
3| }
### m-x.rs:1-3 — calls load_dirs, where the loaded values take effect
1| fn caller_x() {
2|     config::load_dirs("x");
3| }
"#,
};

#[test]
fn env_path_order_prints_what_the_script_prints() {
    assert_eq!(ask(&ENV_PATH_ORDER), ENV_PATH_ORDER.expected);
}

/// A file over the 300 000 bytes whose lines are read still takes one of the two
/// sites of the variable, as it does for `rg`.
#[test]
fn env_big_file_takes_a_slot_prints_what_the_script_prints() {
    let big = format!(
        "const T: &str = \"APP_BIG_TOKEN\";\n{}",
        (0..12000).map(|i| format!("fn f{i}() {{ let a = {i}; }}\n")).collect::<String>()
    );
    assert!(big.len() > 300_000);
    let dir = repo(&[
        ("a_big.rs", &big),
        ("b.rs", r#"fn b() {
    let t = "APP_BIG_TOKEN";
}
"#),
        ("c.rs", r#"fn c() {
    let t = "APP_BIG_TOKEN";
}
"#),
    ]);
    let out = run(&STEP, dir.path(), "where is APP_BIG_TOKEN read", "");
    assert_eq!(out, r#"drop: ^\S*\.(md|rst|txt|adoc|[0-9])\b:|^\S*/doc/
drop: ^b\.rs:\d+-\d+ — (file named|whole file)
### b.rs:1-3 — reads APP_BIG_TOKEN
1| fn b() {
2|     let t = "APP_BIG_TOKEN";
3| }
"#);
}

const SITES_BASIC: Case = Case {
    prompt: "which flags control following links, and where are they applied?",
    brief: "",
    files: &[
        ("src/flags.rs", r#"/// What the walker is told.
pub struct Flags {
    pub follow_links: bool,
    pub max_depth: Option<usize>,
    pub hidden: bool,
}

impl Flags {
    pub fn new() -> Flags {
        Flags { follow_links: false, max_depth: None, hidden: false }
    }

    pub fn follow(&self) -> bool {
        self.follow_links
    }
}
"#),
        ("src/walker.rs", r#"use crate::flags::Flags;

pub fn build_walker(flags: &Flags) -> Walker {
    let mut builder = WalkBuilder::new();
    builder.set_follow(flags.follow_links);
    if flags.follow_links {
        builder.max_symlinks(40);
    }
    builder.hidden(flags.hidden);
    builder.build()
}

pub fn set_follow(builder: &mut WalkBuilder, follow: bool) {
    builder.follow_links(follow);
    if follow {
        builder.same_file_system(false);
    }
}

fn other() {
    let x = 1;
    let y = 2;
    let z = 3;
}
"#),
        ("src/cli.rs", r#"pub fn parse(args: &[String]) -> Flags {
    let mut flags = Flags::new();
    for arg in args {
        match arg.as_str() {
            "-L" | "--follow" => flags.follow_links = true,
            "--hidden" => flags.hidden = true,
            _ => {}
        }
    }
    flags
}
"#),
    ],
    expected: r#"drop: ^config registry|^(?!.*(?:follow|follow_links|links|set_follow))
drop: ^\S*\.(md|rst|txt|adoc|[0-9])\b:|^\S*/doc/
### src/walker.rs:3-11 — names follow/follow_links/links/set_follow
3| pub fn build_walker(flags: &Flags) -> Walker {
4|     let mut builder = WalkBuilder::new();
5|     builder.set_follow(flags.follow_links);
6|     if flags.follow_links {
7|         builder.max_symlinks(40);
8|     }
9|     builder.hidden(flags.hidden);
10|     builder.build()
11| }
### src/walker.rs:13-18 — names follow/follow_links/links/set_follow
13| pub fn set_follow(builder: &mut WalkBuilder, follow: bool) {
14|     builder.follow_links(follow);
15|     if follow {
16|         builder.same_file_system(false);
17|     }
18| }
### src/cli.rs:1-11 — names follow/follow_links/links
1| pub fn parse(args: &[String]) -> Flags {
2|     let mut flags = Flags::new();
3|     for arg in args {
4|         match arg.as_str() {
5|             "-L" | "--follow" => flags.follow_links = true,
6|             "--hidden" => flags.hidden = true,
7|             _ => {}
8|         }
9|     }
10|     flags
11| }
### src/flags.rs:13-15 — names follow/follow_links/links
13|     pub fn follow(&self) -> bool {
14|         self.follow_links
15|     }
### src/flags.rs:9-11 — names follow/follow_links/links
9|     pub fn new() -> Flags {
10|         Flags { follow_links: false, max_depth: None, hidden: false }
11|     }
### src/flags.rs:1-6 — names follow/follow_links/links
1| /// What the walker is told.
2| pub struct Flags {
3|     pub follow_links: bool,
4|     pub max_depth: Option<usize>,
5|     pub hidden: bool,
6| }
"#,
};

#[test]
fn sites_basic_prints_what_the_script_prints() {
    assert_eq!(ask(&SITES_BASIC), SITES_BASIC.expected);
}

const SITES_NO_WHERE: Case = Case {
    prompt: "which flags control following links",
    brief: "",
    files: &[
        ("src/flags.rs", r#"/// What the walker is told.
pub struct Flags {
    pub follow_links: bool,
    pub max_depth: Option<usize>,
    pub hidden: bool,
}

impl Flags {
    pub fn new() -> Flags {
        Flags { follow_links: false, max_depth: None, hidden: false }
    }

    pub fn follow(&self) -> bool {
        self.follow_links
    }
}
"#),
        ("src/walker.rs", r#"use crate::flags::Flags;

pub fn build_walker(flags: &Flags) -> Walker {
    let mut builder = WalkBuilder::new();
    builder.set_follow(flags.follow_links);
    if flags.follow_links {
        builder.max_symlinks(40);
    }
    builder.hidden(flags.hidden);
    builder.build()
}

pub fn set_follow(builder: &mut WalkBuilder, follow: bool) {
    builder.follow_links(follow);
    if follow {
        builder.same_file_system(false);
    }
}

fn other() {
    let x = 1;
    let y = 2;
    let z = 3;
}
"#),
        ("src/cli.rs", r#"pub fn parse(args: &[String]) -> Flags {
    let mut flags = Flags::new();
    for arg in args {
        match arg.as_str() {
            "-L" | "--follow" => flags.follow_links = true,
            "--hidden" => flags.hidden = true,
            _ => {}
        }
    }
    flags
}
"#),
    ],
    expected: "",
};

#[test]
fn sites_no_where_prints_what_the_script_prints() {
    assert_eq!(ask(&SITES_NO_WHERE), SITES_NO_WHERE.expected);
}

const SITES_IMPL_FOR: Case = Case {
    prompt: "which options control follow policy, where are they used",
    brief: "",
    files: &[
        ("src/policy.rs", r#"/// The strategy for symlinks.
struct FollowPolicy;

impl Default for FollowPolicy {
    fn default() -> Self {
        FollowPolicy
    }
}

impl FollowPolicy {
    fn should_follow(&self, depth: usize) -> bool {
        depth < 4
    }
}
"#),
        ("src/walker.rs", r#"use crate::flags::Flags;

pub fn build_walker(flags: &Flags) -> Walker {
    let mut builder = WalkBuilder::new();
    builder.set_follow(flags.follow_links);
    if flags.follow_links {
        builder.max_symlinks(40);
    }
    builder.hidden(flags.hidden);
    builder.build()
}

pub fn set_follow(builder: &mut WalkBuilder, follow: bool) {
    builder.follow_links(follow);
    if follow {
        builder.same_file_system(false);
    }
}

fn other() {
    let x = 1;
    let y = 2;
    let z = 3;
}
"#),
        ("src/flags.rs", r#"/// What the walker is told.
pub struct Flags {
    pub follow_links: bool,
    pub max_depth: Option<usize>,
    pub hidden: bool,
}

impl Flags {
    pub fn new() -> Flags {
        Flags { follow_links: false, max_depth: None, hidden: false }
    }

    pub fn follow(&self) -> bool {
        self.follow_links
    }
}
"#),
    ],
    expected: r#"drop: ^config registry|^(?!.*(?:follow|follow_links|policy|set_follow))
drop: ^\S*\.(md|rst|txt|adoc|[0-9])\b:|^\S*/doc/
### src/walker.rs:13-18 — names follow/follow_links/set_follow
13| pub fn set_follow(builder: &mut WalkBuilder, follow: bool) {
14|     builder.follow_links(follow);
15|     if follow {
16|         builder.same_file_system(false);
17|     }
18| }
### src/policy.rs:1-8 — names follow/policy
1| /// The strategy for symlinks.
2| struct FollowPolicy;
3| 
4| impl Default for FollowPolicy {
5|     fn default() -> Self {
6|         FollowPolicy
7|     }
8| }
### src/policy.rs:10-14 — names follow/policy
10| impl FollowPolicy {
11|     fn should_follow(&self, depth: usize) -> bool {
12|         depth < 4
13|     }
14| }
### src/walker.rs:3-11 — names follow/follow_links/set_follow
3| pub fn build_walker(flags: &Flags) -> Walker {
4|     let mut builder = WalkBuilder::new();
5|     builder.set_follow(flags.follow_links);
6|     if flags.follow_links {
7|         builder.max_symlinks(40);
8|     }
9|     builder.hidden(flags.hidden);
10|     builder.build()
11| }
### src/flags.rs:13-15 — names follow/follow_links
13|     pub fn follow(&self) -> bool {
14|         self.follow_links
15|     }
### src/flags.rs:9-11 — names follow/follow_links
9|     pub fn new() -> Flags {
10|         Flags { follow_links: false, max_depth: None, hidden: false }
11|     }
### src/flags.rs:1-6 — names follow/follow_links
1| /// What the walker is told.
2| pub struct Flags {
3|     pub follow_links: bool,
4|     pub max_depth: Option<usize>,
5|     pub hidden: bool,
6| }
"#,
};

#[test]
fn sites_impl_for_prints_what_the_script_prints() {
    assert_eq!(ask(&SITES_IMPL_FOR), SITES_IMPL_FOR.expected);
}

const SITES_TS: Case = Case {
    prompt: "which settings control following redirects and where are they applied",
    brief: "",
    files: &[
        ("src/opts.ts", r#"export class Options {
  followRedirects: boolean = true;
  maxRedirects = 5;
}

export function applyFollow(opts: Options, status: number) {
  if (opts.followRedirects && status >= 300) {
    return follow(opts);
  }
  return null;
}

export const follow = (opts: Options) => {
  return opts.maxRedirects > 0;
};
"#),
        ("src/http.ts", r#"import { follow } from './opts';
export function go() {
  if (follow(opts)) {
    return 1;
  }
}
"#),
    ],
    expected: r#"drop: ^config registry|^(?!.*(?:follow|redirect))
drop: ^\S*\.(md|rst|txt|adoc|[0-9])\b:|^\S*/doc/
### src/opts.ts:6-11 — names follow/redirect
6| export function applyFollow(opts: Options, status: number) {
7|   if (opts.followRedirects && status >= 300) {
8|     return follow(opts);
9|   }
10|   return null;
11| }
### src/opts.ts:13-15 — names follow/redirect
13| export const follow = (opts: Options) => {
14|   return opts.maxRedirects > 0;
15| };
### src/opts.ts:1-4 — names follow/redirect
1| export class Options {
2|   followRedirects: boolean = true;
3|   maxRedirects = 5;
4| }
### src/http.ts:2-6 — names follow
2| export function go() {
3|   if (follow(opts)) {
4|     return 1;
5|   }
6| }
"#,
};

#[test]
fn sites_ts_prints_what_the_script_prints() {
    assert_eq!(ask(&SITES_TS), SITES_TS.expected);
}

const SITES_TEST_MASKED: Case = Case {
    prompt: "which flags control following links, where are they applied",
    brief: "",
    files: &[
        ("src/flags.rs", r#"/// What the walker is told.
pub struct Flags {
    pub follow_links: bool,
    pub max_depth: Option<usize>,
    pub hidden: bool,
}

impl Flags {
    pub fn new() -> Flags {
        Flags { follow_links: false, max_depth: None, hidden: false }
    }

    pub fn follow(&self) -> bool {
        self.follow_links
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn follows() {
        let f = Flags::new();
        assert!(!f.follow_links);
        assert!(!f.follow());
    }
}
"#),
        ("src/walker.rs", r#"use crate::flags::Flags;

pub fn build_walker(flags: &Flags) -> Walker {
    let mut builder = WalkBuilder::new();
    builder.set_follow(flags.follow_links);
    if flags.follow_links {
        builder.max_symlinks(40);
    }
    builder.hidden(flags.hidden);
    builder.build()
}

pub fn set_follow(builder: &mut WalkBuilder, follow: bool) {
    builder.follow_links(follow);
    if follow {
        builder.same_file_system(false);
    }
}

fn other() {
    let x = 1;
    let y = 2;
    let z = 3;
}
"#),
    ],
    expected: r#"drop: ^config registry|^(?!.*(?:follow|follow_links|links|set_follow))
drop: ^\S*\.(md|rst|txt|adoc|[0-9])\b:|^\S*/doc/
### src/walker.rs:3-11 — names follow/follow_links/links/set_follow
3| pub fn build_walker(flags: &Flags) -> Walker {
4|     let mut builder = WalkBuilder::new();
5|     builder.set_follow(flags.follow_links);
6|     if flags.follow_links {
7|         builder.max_symlinks(40);
8|     }
9|     builder.hidden(flags.hidden);
10|     builder.build()
11| }
### src/walker.rs:13-18 — names follow/follow_links/links/set_follow
13| pub fn set_follow(builder: &mut WalkBuilder, follow: bool) {
14|     builder.follow_links(follow);
15|     if follow {
16|         builder.same_file_system(false);
17|     }
18| }
### src/flags.rs:13-15 — names follow/follow_links/links
13|     pub fn follow(&self) -> bool {
14|         self.follow_links
15|     }
### src/flags.rs:9-11 — names follow/follow_links/links
9|     pub fn new() -> Flags {
10|         Flags { follow_links: false, max_depth: None, hidden: false }
11|     }
### src/flags.rs:1-6 — names follow/follow_links/links
1| /// What the walker is told.
2| pub struct Flags {
3|     pub follow_links: bool,
4|     pub max_depth: Option<usize>,
5|     pub hidden: bool,
6| }
"#,
};

#[test]
fn sites_test_masked_prints_what_the_script_prints() {
    assert_eq!(ask(&SITES_TEST_MASKED), SITES_TEST_MASKED.expected);
}

const SITES_SINGLE_HIT: Case = Case {
    prompt: "which flags control hidden entries, where are they used",
    brief: "",
    files: &[
        ("src/a.rs", r#"pub struct S {
    pub hidden: bool,
}

fn f() {
    let x = 1;
}
"#),
    ],
    expected: "",
};

#[test]
fn sites_single_hit_prints_what_the_script_prints() {
    assert_eq!(ask(&SITES_SINGLE_HIT), SITES_SINGLE_HIT.expected);
}

const SITES_WITH_BRIEF: Case = Case {
    prompt: "which flags control following links, and where are they applied?",
    brief: r#"[PIXEL:BRIEF]
### src/x.rs:1-2 — x
"#,
    files: &[
        ("src/flags.rs", r#"/// What the walker is told.
pub struct Flags {
    pub follow_links: bool,
    pub max_depth: Option<usize>,
    pub hidden: bool,
}

impl Flags {
    pub fn new() -> Flags {
        Flags { follow_links: false, max_depth: None, hidden: false }
    }

    pub fn follow(&self) -> bool {
        self.follow_links
    }
}
"#),
        ("src/walker.rs", r#"use crate::flags::Flags;

pub fn build_walker(flags: &Flags) -> Walker {
    let mut builder = WalkBuilder::new();
    builder.set_follow(flags.follow_links);
    if flags.follow_links {
        builder.max_symlinks(40);
    }
    builder.hidden(flags.hidden);
    builder.build()
}

pub fn set_follow(builder: &mut WalkBuilder, follow: bool) {
    builder.follow_links(follow);
    if follow {
        builder.same_file_system(false);
    }
}

fn other() {
    let x = 1;
    let y = 2;
    let z = 3;
}
"#),
    ],
    expected: r#"drop: ^config registry|^(?!.*(?:follow|follow_links|links|set_follow))
drop: ^\S*\.(md|rst|txt|adoc|[0-9])\b:|^\S*/doc/
### src/walker.rs:3-11 — names follow/follow_links/links/set_follow
3| pub fn build_walker(flags: &Flags) -> Walker {
4|     let mut builder = WalkBuilder::new();
5|     builder.set_follow(flags.follow_links);
6|     if flags.follow_links {
7|         builder.max_symlinks(40);
8|     }
9|     builder.hidden(flags.hidden);
10|     builder.build()
11| }
### src/walker.rs:13-18 — names follow/follow_links/links/set_follow
13| pub fn set_follow(builder: &mut WalkBuilder, follow: bool) {
14|     builder.follow_links(follow);
15|     if follow {
16|         builder.same_file_system(false);
17|     }
18| }
### src/flags.rs:13-15 — names follow/follow_links/links
13|     pub fn follow(&self) -> bool {
14|         self.follow_links
15|     }
### src/flags.rs:9-11 — names follow/follow_links/links
9|     pub fn new() -> Flags {
10|         Flags { follow_links: false, max_depth: None, hidden: false }
11|     }
### src/flags.rs:1-6 — names follow/follow_links/links
1| /// What the walker is told.
2| pub struct Flags {
3|     pub follow_links: bool,
4|     pub max_depth: Option<usize>,
5|     pub hidden: bool,
6| }
"#,
};

#[test]
fn sites_with_brief_prints_what_the_script_prints() {
    assert_eq!(ask(&SITES_WITH_BRIEF), SITES_WITH_BRIEF.expected);
}

const NEITHER: Case = Case {
    prompt: "what is the weather",
    brief: "",
    files: &[
        ("src/config.rs", r#"use std::env;

/// Where the settings live: MYAPP_CONFIG_DIR, else the home directory.
pub fn load_dirs(profile: &str) -> Vec<String> {
    let base = match env::var("MYAPP_CONFIG_DIR") {
        Ok(dir) => expand_home(&dir),
        Err(_) => default_dir(),
    };
    vec![format!("{base}/{profile}")]
}

fn expand_home(dir: &str) -> String {
    dir.replace("~", &home_dir())
}

fn default_dir() -> String {
    format!("{}/.myapp", home_dir())
}

fn home_dir() -> String {
    env::var("HOME").unwrap_or_default()
}

fn unrelated_helper() -> u32 {
    4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_directory() {
        std::env::set_var("MYAPP_CONFIG_DIR", "/x");
        assert_eq!(load_dirs("a"), vec!["/x/a".to_string()]);
    }
}
"#),
        ("src/main.rs", r#"mod config;

fn main() {
    let dirs = config::load_dirs("default");
    for dir in dirs {
        println!("{dir}");
    }
}
"#),
        ("README.md", r#"# myapp

MYAPP_CONFIG_DIR sets the directory.
"#),
        ("doc/myapp.1", r#"MYAPP_CONFIG_DIR
"#),
    ],
    expected: "",
};

#[test]
fn neither_prints_what_the_script_prints() {
    assert_eq!(ask(&NEITHER), NEITHER.expected);
}

fn rows(text: &[&str]) -> Vec<String> {
    text.iter().map(ToString::to_string).collect()
}

#[test]
fn the_step_applies_to_a_where_question_and_to_a_variable_name() {
    assert!(STEP.applies(None, "where is the timeout applied", false));
    assert!(STEP.applies(Some("flow"), "where are the flags read", true));
    assert!(STEP.applies(None, "what does MYAPP_CONFIG_DIR do", false));
    assert!(!STEP.applies(None, "what is the weather", false));
    assert!(!STEP.applies(None, "explain the thing", false));
}

#[test]
fn only_source_outside_tests_and_vendored_trees_is_code() {
    for path in ["src/a.rs", "lib/x.py", "app/main.go", "a.tsx", "x/y/z.cpp", "src/contest.rs"] {
        assert!(Source::is_code(path), "{path}");
    }
    for path in [
        "README.md",
        "tests/a.rs",
        "src/test/a.rs",
        "a_test.go",
        "src/vendor/x.js",
        "examples/a.py",
        "benches/x.rs",
        "src/test_x.py",
        "lib/x.spec.ts",
        "lib/x.test.js",
        "node_modules/p/i.js",
        "src/a.PY",
        "Makefile",
    ] {
        assert!(!Source::is_code(path), "{path}");
    }
}

#[test]
fn a_variable_is_a_word_only_between_non_word_characters() {
    assert!(has_word("let x = FOO_BAR;", "FOO_BAR"));
    assert!(has_word("FOO_BAR", "FOO_BAR"));
    assert!(has_word("AFOO_BAR FOO_BAR", "FOO_BAR"));
    assert!(!has_word("XFOO_BAR", "FOO_BAR"));
    assert!(!has_word("FOO_BARS", "FOO_BAR"));
    assert!(!has_word("FOO_BAR_", "FOO_BAR"));
    assert!(!has_word("éFOO_BAR", "FOO_BAR"));
    assert!(has_word("É-FOO_BAR", "FOO_BAR"));
}

#[test]
fn indentation_and_comments_are_read_as_the_script_reads_them() {
    assert_eq!(indent("    x"), 4);
    assert_eq!(indent("\t x"), 2);
    assert_eq!(indent("x"), 0);
    assert_eq!(indent(""), 0);
    for line in ["", "   ", "// x", "# x", "* x", "/* x", "-- x", "<!-- x", "\"\"\" doc", "'''"] {
        assert!(is_comment(line), "{line:?}");
    }
    for line in ["x // y", "x", "let a = 1;", "<b>"] {
        assert!(!is_comment(line), "{line:?}");
    }
}

#[test]
fn a_definition_ends_where_its_indentation_or_brace_says() {
    assert_eq!(decl_end(&rows(&["fn a() {", "  x;", "}", "fn b() {}"]), 0), 2);
    assert_eq!(decl_end(&rows(&["def a():", "    x", "", "y = 1"]), 0), 2);
    assert_eq!(decl_end(&rows(&["def a():", "    x"]), 0), 1);
    assert_eq!(decl_end(&rows(&["struct A;", "x"]), 0), 0);
    assert_eq!(decl_end(&rows(&["fn a() {", "x"]), 0), 1);
    assert_eq!(decl_end(&rows(&["fn a() {", "    }", "}"]), 0), 2);
    assert_eq!(decl_end(&rows(&["  fn a() {", "  x", "  }", "}"]), 0), 2);
    let long: Vec<String> = std::iter::once("fn a() {".to_owned())
        .chain((0..100).map(|n| format!("    x{n};")))
        .collect();
    assert_eq!(decl_end(&long, 0), 40, "no closing brace: forty lines on");
}

#[test]
fn a_window_starts_at_the_definition_and_then_follows_the_hits() {
    assert_eq!(window(0, 9, &[], 20), (0, 9));
    assert_eq!(window(0, 99, &[], 10), (0, 9));
    assert_eq!(window(0, 99, &[50], 10), (45, 54));
    assert_eq!(window(0, 99, &[99], 10), (90, 99));
    assert_eq!(window(0, 99, &[40, 60], 10), (45, 54));
    assert_eq!(window(20, 99, &[22], 10), (20, 29));
}

#[test]
fn docs_and_the_type_above_an_impl_come_with_a_definition() {
    let lines = rows(&["x", "/// doc", "#[derive(Debug)]", "fn a() {}"]);
    assert_eq!(with_docs(&lines, 3), 1);
    assert_eq!(with_docs(&lines, 0), 0);
    assert_eq!(with_docs(&rows(&["a", "  // note", "fn b() {}"]), 2), 1);
    assert_eq!(with_docs(&rows(&["a", "@decorator", "def b():"]), 2), 1);
    let lines = rows(&["/// T", "struct T;", "", "impl X for T {}"]);
    assert_eq!(with_type(&lines, 3), 0);
    assert_eq!(with_type(&rows(&["struct T {", "}", "impl X for T {}"]), 2), 2);
    assert_eq!(with_type(&rows(&["fn a() {}", "struct T;", "fn b() {}"]), 2), 2);
    assert_eq!(
        with_type(&rows(&["struct T;", "impl X for T {}"]), 1),
        1,
        "only a type with a blank line before the impl is pulled in"
    );
}

#[test]
fn a_file_stem_drops_the_last_extension_only() {
    assert_eq!(file_stem("src/config.rs"), "config");
    assert_eq!(file_stem("a.b.rs"), "a.b");
    assert_eq!(file_stem(".rs"), ".rs");
    assert_eq!(file_stem("noext"), "noext");
}

#[test]
fn stems_drop_one_ending_and_skip_the_question_words() {
    assert_eq!(
        stems_of("which flags control following links, and where are they applied?"),
        ["control", "follow", "links", "appli"]
    );
    assert_eq!(stems_of("Which settings handle processes? processes"), ["process"]);
    assert_eq!(stems_of("Flag FLAG other OTHER"), ["other"]);
}

#[test]
fn a_stem_is_named_between_non_identifier_characters() {
    assert!(names_stem("follow, x", "follow"));
    assert!(names_stem("(Follow)", "follow"));
    assert!(names_stem("a follow", "follow"));
    assert!(names_stem("x, unfollow, follow", "follow"));
    assert!(!names_stem("follow_links, x", "follow"));
    assert!(!names_stem("unfollow", "follow"));
    assert!(!names_stem("follow1", "follow"));
}

#[test]
fn a_call_or_a_condition_weighs_more_than_a_field() {
    assert!((hit_weight("pub follow: bool,", "follow") - 0.3).abs() < f64::EPSILON);
    assert!((hit_weight("    follow_links: bool,", "follow") - 0.3).abs() < f64::EPSILON);
    assert!((hit_weight("pub(crate) follow: bool,", "follow") - 0.3).abs() < f64::EPSILON);
    assert!((hit_weight("let a = follow(x);", "follow") - 2.0).abs() < f64::EPSILON);
    assert!((hit_weight("if enabled {", "follow") - 2.0).abs() < f64::EPSILON);
    assert!((hit_weight("while go {", "follow") - 2.0).abs() < f64::EPSILON);
    assert!((hit_weight("let a = follow;", "follow") - 1.0).abs() < f64::EPSILON);
}
