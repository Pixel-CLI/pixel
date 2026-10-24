// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Locators: the lines a question about a setting or about tests would be
//! answered from, found by the shape of the line instead of by a symbol the
//! prompt names.
//!
//! A census of vanilla sessions found two questions the region pack could not
//! answer, because their answer is not a symbol: "where do I set that, and
//! what port does it use" (a constant, an environment read, a flag
//! definition, a key of a config file) and "which tests cover redirect
//! handling" (test functions whose name carries the topic). Both are found
//! the same way. The prompt's content words are searched one at a time with a
//! pattern that only matches a line of the shape asked for (a definition, an
//! assignment, an environment read, a flag, a default value; or a test
//! declaration), the rows are ranked by how much of the prompt they carry,
//! and the best are cut into the numbered blocks of the region pack.
//!
//! The chain gathers the rows ([`Evidence::grep`], file reads); what decides
//! what is shown (the triggers, the words, the line shapes, the ranking, the
//! windows) is a pure function in this file. Everything fails open: a search
//! that errors or runs out of time leaves the pack as it would have been.

use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;

use crate::answer::{self, Receipt};
use crate::chain::{Evidence, RichHit};
use crate::region_pack::{self, Block};

/// The window a locator may use, whatever is left of the brief's own. The
/// searches run in parallel; this bounds the slowest.
pub(crate) const LOCATOR_WINDOW: Duration = Duration::from_millis(300);
/// Content words searched at most.
const MAX_TERMS: usize = 4;
/// Content words of a prompt considered.
const CANDIDATE_TERMS: usize = 8;
/// Rows one pattern search pulls.
const SEARCH_ROWS: usize = 200;
/// Lines kept for each term before the best of the rest.
const PER_TERM: usize = 2;
/// Setting lines shown, at most, and per file.
const MAX_SETTING_LINES: usize = 14;
const PER_FILE: usize = 4;
/// Files a locator reads to cut its windows.
const MAX_FILES: usize = 16;
/// Lines of a file a locator reads.
const MAX_FILE_LINES: u64 = 20_000;
/// A configuration file this short is shown whole.
const WHOLE_CONFIG_LINES: usize = 80;
/// Lines kept around a key, a read, a flag or a default.
const AROUND: u64 = 2;
/// Doc-comment lines kept above a setting.
const DOC_LINES: usize = 4;
/// Longest declaration followed to its end.
const DECL_LINES: usize = 10;
/// Environment variables listed at most.
const MAX_ENV_NAMES: usize = 24;
/// Tests shown with their first lines, and listed by name.
const MAX_TEST_BODIES: usize = 8;
const MAX_TEST_NAMES: usize = 24;
/// Lines of a test shown under its name.
const TEST_BODY_LINES: usize = 12;
/// Test functions per file shown with their body.
const BODIES_PER_FILE: usize = 3;

/// Words that name a setting: a configuration, an environment variable, a
/// flag or option, a default, a port.
const SETTING_WORDS: &[&str] = &[
    "config",
    "configs",
    "configure",
    "configured",
    "configures",
    "configuring",
    "configuration",
    "configurations",
    "setting",
    "settings",
    "env",
    "envvar",
    "envvars",
    "environment",
    "flag",
    "flags",
    "option",
    "options",
    "default",
    "defaults",
    "port",
    "ports",
    "timeout",
    "timeouts",
    "threshold",
    "thresholds",
    "preference",
    "preferences",
];
/// Words that name a setting only inside a question about a value.
const VALUE_WORDS: &[&str] = &[
    "limit",
    "limits",
    "max",
    "maximum",
    "minimum",
    "ttl",
    "interval",
    "retries",
    "verbosity",
];
/// Words that open a question about a value.
const VALUE_OPENERS: &[&str] = &[
    "where",
    "what",
    "which",
    "how many",
    "how long",
    "how much",
    "how often",
];
/// Words that only say a setting is asked about, whatever it is: no search is
/// spent on them (a `port` or a `timeout`, which are the setting itself,
/// stay searchable).
const META_WORDS: &[&str] = &[
    "config",
    "configs",
    "configure",
    "configured",
    "configures",
    "configuring",
    "configuration",
    "configurations",
    "setting",
    "settings",
    "env",
    "envvar",
    "envvars",
    "environment",
    "flag",
    "flags",
    "option",
    "options",
    "default",
    "defaults",
    "preference",
    "preferences",
];
/// Words that ask which environment variables there are.
const ENV_WORDS: &[&str] = &["env", "envvar", "envvars", "environment"];
/// Words that ask about tests.
const TEST_WORDS: &[&str] = &[
    "test", "tests", "tested", "testing", "spec", "specs", "coverage",
];
/// Words the content search must not be spent on: the ones that say what is
/// asked, and the verbs and nouns of any question about code.
const GENERIC_WORDS: &[&str] = &[
    "about",
    "after",
    "again",
    "also",
    "anything",
    "behave",
    "behaves",
    "behaviour",
    "behavior",
    "change",
    "changes",
    "changed",
    "code",
    "cover",
    "covers",
    "covered",
    "covering",
    "does",
    "else",
    "file",
    "files",
    "find",
    "give",
    "global",
    "handling",
    "handle",
    "handles",
    "have",
    "keep",
    "keeps",
    "kept",
    "know",
    "like",
    "local",
    "long",
    "look",
    "make",
    "makes",
    "many",
    "much",
    "name",
    "named",
    "need",
    "number",
    "numbers",
    "only",
    "other",
    "program",
    "project",
    "repo",
    "repository",
    "return",
    "returns",
    "right",
    "running",
    "runs",
    "show",
    "some",
    "such",
    "tell",
    "thing",
    "things",
    "those",
    "these",
    "type",
    "types",
    "usage",
    "used",
    "uses",
    "using",
    "value",
    "values",
    "variable",
    "variables",
    "want",
    "wants",
    "were",
    "with",
    "work",
    "works",
    "working",
    "would",
    "which",
    "where",
    "what",
    "when",
    "your",
];

/// What a prompt asks the locators for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ask {
    /// Where a setting is defined, set or defaulted.
    pub(crate) setting: bool,
    /// Which tests cover something.
    pub(crate) tests: bool,
    /// Which environment variables there are.
    pub(crate) env: bool,
}

impl Ask {
    pub(crate) const fn any(self) -> bool {
        self.setting || self.tests
    }
}

/// The lowercase words of `text`.
fn words(text: &str) -> Vec<String> {
    text.split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// What `typed` asks the locators for. A setting is asked about with one of
/// [`SETTING_WORDS`], or with a [`VALUE_WORDS`] one inside a question that
/// opens like one about a value; tests with one of [`TEST_WORDS`]. A word
/// that is a segment of an identifier the prompt quotes (`TEST_MODE`) names
/// the identifier, not the question, so it counts all the same: a locator
/// only adds lines.
pub(crate) fn asks(typed: &str) -> Ask {
    let lower = typed.to_lowercase();
    let held = words(&lower);
    let has = |list: &[&str]| held.iter().any(|word| list.contains(&word.as_str()));
    let value = has(VALUE_WORDS) && VALUE_OPENERS.iter().any(|opener| lower.contains(opener));
    let env = has(ENV_WORDS);
    Ask {
        setting: has(SETTING_WORDS) || value,
        tests: has(TEST_WORDS),
        env,
    }
}

/// `word` without a verb ending or a plural, in the form [`answer::tokens`]
/// gives the words of a text, so that a term and the text it is looked for
/// in compare equal (`redirecting` and `redirects` are both `redirect`).
pub(crate) fn stem(word: &str) -> String {
    let mut word = word.to_string();
    for suffix in ["ing", "ied", "ed"] {
        if let Some(rest) = word.strip_suffix(suffix)
            && rest.len() >= 4
        {
            word = rest.to_string();
            break;
        }
    }
    answer::tokens(&word).into_iter().next().unwrap_or(word)
}

/// Whether `word` says nothing about what is asked (a question word, a
/// verb or noun of any question about code).
pub(crate) fn is_generic(word: &str) -> bool {
    GENERIC_WORDS.contains(&word) || META_WORDS.contains(&word) || TEST_WORDS.contains(&word)
}

/// The content words of `typed` worth a search, stems, in prompt order:
/// no word that only says what is asked ([`META_WORDS`], [`TEST_WORDS`]), no
/// generic one, none shorter than four characters.
pub(crate) fn terms(typed: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in crate::prompt::brief_keywords(crate::prompt::brief_task(typed)) {
        let word = word.to_lowercase();
        if word.len() < 4
            || GENERIC_WORDS.contains(&word.as_str())
            || META_WORDS.contains(&word.as_str())
            || TEST_WORDS.contains(&word.as_str())
            || !word.chars().all(|ch| ch.is_ascii_alphanumeric())
        {
            continue;
        }
        let word = stem(&word);
        if !out.contains(&word) {
            out.push(word);
        }
    }
    out.truncate(CANDIDATE_TERMS);
    out
}

// ---------------------------------------------------------------------------
// The shapes of a setting line
// ---------------------------------------------------------------------------

/// A declaration of a constant or a static: `pub const X: usize`,
/// `export const X`, `static final int X`, `#define X`. A function that is
/// `const` (`const fn`) names no value: the declared name must be followed
/// by `:`, `=` or `;`.
const DECL_CUE: &str = r"^\s*(?:(?:export|pub(?:\([^)]*\))?|public|private|protected|internal|declare|default|extern)\s+)*(?:const|static|final|readonly|#define|define)\s+(?:[\w<>\[\],.&']+\s+)*?[A-Za-z_]\w*\s*[:=;]";
/// The start of a declaration of a constant, up to its keyword: what a
/// search can anchor on when the term is in the name, which [`DECL_CUE`]
/// consumes.
const DECL_HEAD: &str = r"^\s*(?:(?:export|pub(?:\([^)]*\))?|public|private|protected|internal|declare|default|extern)\s+)*(?:const|static|final|readonly|#define|define)\s+";
/// An upper-case name assigned: `MAX_REDIRECTS = 30`, `VITE_APP_PORT=3000`.
const UPPER_CUE: &str = r"^\s*(?:export\s+)?([A-Z][A-Z0-9_]{2,})\s*(?::[^=\n]*)?=\s*\S";
/// A read of the process environment.
const ENV_CUE: &str = r"(?:env::var(?:_os)?|option_env!|env!|getenv|os\.environ|environ\.get|process\.env|import\.meta\.env|os\.Getenv|System\.getenv|\bENV\[)";
/// The definition of a command-line flag.
const FLAG_CUE: &str = r#"(?:\.long\(|\.short\(|add_argument\(|#\[arg\(|#\[clap\(|\.flag\(|\.option\(|\.addOption\(|\.arg\(|['"]--[a-z])"#;
/// A default value.
const DEFAULT_CUE: &str = r"(?:default_value(?:_t)?\b|\bdefault\s*[:=]|\bdefault\()";
/// A name given a literal: `self.max_redirects = 30`, `port: 3000,`,
/// `host = "localhost"`. In a configuration file any value does.
const KEY_CUE: &str = r"^\s*";
/// What follows the name of a key in a source file: a number, a boolean, a
/// string, a null.
const LITERAL_CUE: &str =
    r#"\s*[:=]\s*(?:-?\d|true\b|false\b|["'`]|None\b|null\b|undefined\b|Some\()"#;

/// What a line that matched a setting search is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    /// A constant or an upper-case assignment: the name and its value.
    Constant,
    /// A key of a configuration file, or an assignment to a field.
    Key,
    /// A read of the environment.
    Env,
    /// A flag's definition.
    Flag,
    /// A default value.
    Default,
    /// An upper-case name that carries a term, used on the line.
    Named,
}

impl Shape {
    const fn label(self) -> &'static str {
        match self {
            Self::Constant => "constant",
            Self::Key => "key",
            Self::Env => "environment",
            Self::Flag => "flag",
            Self::Default => "default",
            Self::Named => "named",
        }
    }
}

static UPPER: LazyLock<Regex> = LazyLock::new(|| Regex::new(UPPER_CUE).expect("a literal pattern"));
static ENV: LazyLock<Regex> = LazyLock::new(|| Regex::new(ENV_CUE).expect("a literal pattern"));
static FLAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(FLAG_CUE).expect("a literal pattern"));
static DEFAULT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(DEFAULT_CUE).expect("a literal pattern"));
/// The name before the `=` or `:` of a `name = value` line.
static ASSIGNED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*([\w."'\-]+)\s*[:=]\s*\S"#).expect("a literal pattern"));
/// A name given a literal, in a source file.
static LITERAL_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"{KEY_CUE}[A-Za-z_][\w.]*{LITERAL_CUE}")).expect("a literal pattern")
});
/// An upper-case identifier.
static UPPER_IDENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[A-Z][A-Z0-9_]{2,}\b").expect("a literal pattern"));
/// The identifier a constant declaration names.
static DECLARED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&DECL_CUE.replace(r"[A-Za-z_]\w*\s*[:=;]", r"([A-Za-z_]\w*)\s*[:=;]"))
        .expect("a literal pattern")
});
/// The variable an environment read names, quoted or as a constant.
static ENV_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"(?:env::var(?:_os)?|option_env!|env!|getenv|os\.environ(?:\.get)?|environ\.get|os\.Getenv|System\.getenv)\s*\(\s*["']?([A-Za-z_][A-Za-z0-9_]*)"#,
        r#"|(?:process\.env|import\.meta\.env)\.([A-Za-z_][A-Za-z0-9_]*)"#,
        r#"|\bENV\[\s*["']([A-Za-z_][A-Za-z0-9_]*)"#,
        r#"|os\.environ\[\s*["']([A-Za-z_][A-Za-z0-9_]*)"#,
    ))
    .expect("a literal pattern")
});

/// Whether `path` is a configuration file read as `key = value` lines.
fn is_config_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(
        name.rsplit_once('.').map(|(_, extension)| extension),
        Some("toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "properties")
    )
}

/// The quoted strings and `--flags` of a line: where a flag's name is.
static FLAG_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"["'`]([A-Za-z][\w-]{0,39})["'`]|(--[A-Za-z][\w-]*)"#).expect("a literal pattern")
});

/// A setting line: its shape and the name it gives the setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Setting {
    pub(crate) shape: Shape,
    /// The constant, key, variable or flag the line names; for a default,
    /// the line itself.
    pub(crate) name: String,
}

/// The setting a line that matched a search for `terms` is, or `None` for a
/// line that only mentions them: the name the line gives the setting must
/// carry a term (`SEARCH_DEFAULT_ROWS`, `VITE_APP_PORT`, `--redirects`), or
/// the line is a mention of a word in a value. In a source file a key's name
/// is not quoted (a line inside a string literal is a fixture, not a
/// setting).
pub(crate) fn shape_of(path: &str, text: &str, terms: &[String]) -> Option<Setting> {
    let setting = |shape: Shape, name: &str| {
        carries(name, terms).then(|| Setting {
            shape,
            name: name.to_string(),
        })
    };
    if ENV.is_match(text) {
        return env_names_of(text)
            .into_iter()
            .find_map(|name| setting(Shape::Env, &name));
    }
    if FLAG.is_match(text)
        && let Some(flag) = FLAG_NAME
            .captures_iter(text)
            .filter_map(|found| found.get(1).or_else(|| found.get(2)))
            .find_map(|name| setting(Shape::Flag, name.as_str()))
    {
        return Some(flag);
    }
    if let Some(name) = declared_name(text) {
        return setting(Shape::Constant, &name);
    }
    if DEFAULT.is_match(text) {
        return carries(text, terms).then(|| Setting {
            shape: Shape::Default,
            name: text.trim().to_string(),
        });
    }
    if let Some(found) = ASSIGNED.captures(text).and_then(|found| found.get(1)) {
        let name = found.as_str();
        let quoted = name.starts_with(['"', '\'']);
        let literal = is_config_file(path) || LITERAL_KEY.is_match(text);
        if literal
            && (!quoted || is_config_file(path))
            && let Some(key) = setting(Shape::Key, name.trim_matches(['"', '\'']))
        {
            return Some(key);
        }
    }
    UPPER_IDENT
        .find_iter(text)
        .map(|found| found.as_str())
        .find_map(|name| setting(Shape::Named, name))
}

/// The constant a declaration or an upper-case assignment names.
fn declared_name(text: &str) -> Option<String> {
    DECLARED
        .captures(text)
        .or_else(|| UPPER.captures(text))
        .and_then(|found| found.get(1))
        .map(|name| name.as_str().to_string())
}

/// The variables an environment read names.
fn env_names_of(text: &str) -> Vec<String> {
    ENV_NAME
        .captures_iter(text)
        .filter_map(|found| {
            found
                .iter()
                .skip(1)
                .flatten()
                .next()
                .map(|m| m.as_str().to_string())
        })
        .collect()
}

/// Whether `name` is written in upper case, as a constant is.
fn is_upper_name(name: &str) -> bool {
    name.chars().any(|ch| ch.is_ascii_uppercase())
        && name
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

/// Whether `text` carries `term` as a word of its own: a segment of an
/// identifier (`server_port`, `serverPort`, `PORT`) or a plain word, in the
/// plural or with a verb ending when the term is long enough to stay itself
/// there (`redirect` is in `redirects` and `redirecting`, `port` is not in
/// `portal`).
pub(crate) fn carries_term(text: &str, term: &str) -> bool {
    answer::tokens(text)
        .iter()
        .any(|token| token == term || (term.len() >= answer::LONG_STEM && token.starts_with(term)))
}

/// Whether `text` carries any of `terms`.
fn carries(text: &str, terms: &[String]) -> bool {
    terms.iter().any(|term| carries_term(text, term))
}

/// The regular expression that finds `term` as [`carries_term`] reads it:
/// at the start of a word (after a character that is not a letter, or as the
/// capitalised segment of a camel-case name) and, for a short term, at its
/// end (an `s` allowed, then a non-letter, the end of the line, or the start
/// of the next camel-case segment).
fn term_pattern(term: &str) -> String {
    let lower = regex::escape(term);
    let mut capital = term.chars();
    let capitalised = capital
        .next()
        .map(|first| format!("{}{}", first.to_ascii_uppercase(), capital.as_str()))
        .unwrap_or_default();
    let start = format!(
        "(?:(?:^|[^A-Za-z])(?i:{lower})|[a-z]{})",
        regex::escape(&capitalised)
    );
    if term.len() >= answer::LONG_STEM {
        start
    } else {
        format!("{start}(?i:s)?(?:[^A-Za-z]|[A-Z][a-z]|$)")
    }
}

/// `terms` as one alternation of [`term_pattern`]s.
fn any_of(terms: &[String]) -> String {
    let patterns: Vec<String> = terms.iter().map(|term| term_pattern(term)).collect();
    format!("(?:{})", patterns.join("|"))
}

/// An upper-case name that carries one of `terms` as a whole segment
/// (`VITE_APP_PORT`, `MAX_REDIRECTS`, `PORTS`).
fn upper_name(terms: &[String]) -> String {
    let upper: Vec<String> = terms
        .iter()
        .map(|term| regex::escape(&term.to_ascii_uppercase()))
        .collect();
    format!(
        "(?:[A-Z][A-Z0-9]*_)*(?:{})S?(?:_[A-Z0-9]+)*",
        upper.join("|")
    )
}

/// The pattern that finds the lines of a setting that carry any of `terms`:
/// a declaration, an upper-case assignment, a key given a literal, an
/// environment read, a flag, a default; the terms are matched in any case,
/// the shape is not.
pub(crate) fn setting_pattern(terms: &[String]) -> String {
    let term = any_of(terms);
    let upper = upper_name(terms);
    format!(
        "(?:{DECL_HEAD}[^\\n]*{term}|^\\s*(?:export\\s+)?{upper}\\s*(?::[^=\\n]*)?=\\s*\\S|{KEY_CUE}(?:[A-Za-z_][\\w.]*)?{term}[\\w.]*{LITERAL_CUE}|{ENV_CUE}[^\\n]*{term}|{FLAG_CUE}[^\\n]*{term}|{DEFAULT_CUE}[^\\n]*{term})"
    )
}

/// The pattern that finds the lines that use an upper-case name carrying one
/// of `terms` as a whole segment (`VITE_APP_PORT`, `MAX_REDIRECTS`): a
/// constant or an environment variable, wherever it is used.
pub(crate) fn named_pattern(terms: &[String]) -> String {
    format!(
        "(?:^|[^A-Za-z0-9_]){}(?:[^A-Za-z0-9_]|$)",
        upper_name(terms)
    )
}

/// Matches every read of the environment.
fn env_pattern() -> String {
    ENV_CUE.to_string()
}

/// The variable names `rows` read from the environment, once each, with the
/// first place that reads them.
pub(crate) fn env_names(rows: &[RichHit]) -> Vec<(String, String, u64)> {
    let mut seen: Vec<(String, String, u64)> = Vec::new();
    for row in rows {
        let Some(text) = row.text.as_deref() else {
            continue;
        };
        let Some(found) = ENV_NAME.captures(text) else {
            continue;
        };
        let Some(name) = found.iter().skip(1).flatten().next() else {
            continue;
        };
        let name = name.as_str().to_string();
        if !seen.iter().any(|(held, _, _)| *held == name) {
            seen.push((name, row.path.clone(), row.line));
        }
    }
    seen
}

// ---------------------------------------------------------------------------
// Ranking
// ---------------------------------------------------------------------------

/// A row of a setting search that has a shape, and why it ranks where it
/// does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) path: String,
    pub(crate) line: u64,
    pub(crate) setting: Setting,
    pub(crate) score: i64,
}

/// How many of `terms` the `text` carries.
fn carried(text: &str, terms: &[String]) -> i64 {
    terms.iter().filter(|term| carries_term(text, term)).count() as i64
}

/// What a setting is worth before the terms count: a constant named in
/// upper case is the setting itself; a read, a flag or a default says where
/// one is used; a key counts for more in a configuration file; a script, an
/// example or a hidden directory counts for less than the code.
fn base_score(setting: &Setting, path: &str) -> i64 {
    let shape = match setting.shape {
        Shape::Constant if is_upper_name(&setting.name) => 6,
        Shape::Constant | Shape::Env | Shape::Flag => 4,
        Shape::Default => 3,
        Shape::Key if is_config_file(path) => 5,
        Shape::Key => 3,
        Shape::Named => 2,
    };
    shape - if is_primary_path(path) { 0 } else { 5 }
}

/// The ranked setting lines among `rows`: only rows with a [`Shape`], not in
/// generated, credential-shaped, test or documentation files; a line scores
/// its [`base_score`], three for each term it carries and one for each its
/// path carries. At most [`PER_FILE`] per file and `limit` in all, best first,
/// ties in path order.
pub(crate) fn rank_settings(
    rows: &[RichHit],
    terms: &[String],
    wants_tests: bool,
    limit: usize,
) -> Vec<Candidate> {
    let mut found: BTreeMap<(String, u64), Candidate> = BTreeMap::new();
    for row in rows {
        let Some(text) = row.text.as_deref() else {
            continue;
        };
        if region_pack::is_noise_path(&row.path)
            || (!wants_tests && answer::is_test_path(&row.path))
            || answer::is_docs_path(&row.path)
        {
            continue;
        }
        let Some(setting) = shape_of(&row.path, text, terms) else {
            continue;
        };
        let score = base_score(&setting, &row.path)
            + 3 * carried(&setting.name, terms)
            + carried(&row.path, terms);
        found
            .entry((row.path.clone(), row.line))
            .or_insert(Candidate {
                path: row.path.clone(),
                line: row.line,
                setting,
                score,
            });
    }
    let mut ranked: Vec<Candidate> = found.into_values().collect();
    ranked.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
    let mut per_file: BTreeMap<String, usize> = BTreeMap::new();
    ranked.retain(|candidate| {
        let count = per_file.entry(candidate.path.clone()).or_insert(0);
        *count += 1;
        *count <= PER_FILE
    });
    ranked.truncate(limit);
    ranked
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

/// The lines `first..=last` (1-based) to show around the setting at `line`
/// of `lines`: its doc comment above and the declaration to its end for a
/// constant, one line around a key or a read.
pub(crate) fn setting_window(lines: &[String], line: u64, shape: Shape) -> Option<(u64, u64)> {
    let at = usize::try_from(line.checked_sub(1)?).ok()?;
    if at >= lines.len() {
        return None;
    }
    if shape == Shape::Constant {
        let (end, _) = region_pack::decl_end(lines, at, DECL_LINES);
        let top = answer::doc_above(lines, at)
            .unwrap_or_else(|| comment_above(lines, at))
            .max(at.saturating_sub(DOC_LINES));
        return Some((top as u64 + 1, end as u64 + 1));
    }
    let first = line.saturating_sub(AROUND).max(1);
    let last = (line + AROUND).min(lines.len() as u64);
    Some((first, last))
}

/// Whether the line a candidate points at, read in `lines`, is a setting and
/// not a fixture (the test module of a source file, unless tests were asked
/// about) or a local variable (an indented constant in lower case).
pub(crate) fn is_setting_line(lines: &[String], candidate: &Candidate, wants_tests: bool) -> bool {
    if !wants_tests && answer::in_test_module(lines, candidate.line) {
        return false;
    }
    let local = candidate.setting.shape == Shape::Constant
        && !is_upper_name(&candidate.setting.name)
        && usize::try_from(candidate.line.saturating_sub(1))
            .ok()
            .and_then(|at| lines.get(at))
            .is_some_and(|text| text.starts_with(char::is_whitespace));
    !local
}

/// Whether `line` is a line comment or part of a block comment.
fn is_comment(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("//")
        || line.starts_with("/*")
        || line.starts_with("* ")
        || line.starts_with("*/")
        || (line.starts_with('#') && !line.starts_with("#[") && !line.starts_with("#!"))
}

/// The first line (0-based) of the comment block right above line `at`, or
/// `at` when none sits there: Python and shell write the note on a setting
/// as `#` lines, which `answer::doc_above` does not know.
fn comment_above(lines: &[String], at: usize) -> usize {
    let mut top = at;
    while top > 0 && at - top < DOC_LINES && is_comment(&lines[top - 1]) {
        top -= 1;
    }
    top
}

/// `limit` of the `ranked` candidates: the two best for each term first (a
/// question about a collaboration server and a port needs both), then the
/// best of the rest.
pub(crate) fn diversify<'a>(
    ranked: &[&'a Candidate],
    terms: &[String],
    limit: usize,
) -> Vec<&'a Candidate> {
    let mut chosen: Vec<&Candidate> = Vec::new();
    // One line for each term, then a second for each, then the rest.
    for round in 0..PER_TERM {
        for term in terms {
            let next = ranked
                .iter()
                .filter(|candidate| carries_term(&candidate.setting.name, term))
                .nth(round);
            if let Some(candidate) = next
                && chosen.len() < limit
                && !chosen.contains(candidate)
            {
                chosen.push(candidate);
            }
        }
    }
    for candidate in ranked {
        if chosen.len() < limit && !chosen.contains(candidate) {
            chosen.push(candidate);
        }
    }
    chosen
}

/// `windows` merged: sorted, and joined when they overlap or touch.
pub(crate) fn merge(mut windows: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    windows.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (first, last) in windows {
        match out.last_mut() {
            Some((_, end)) if first <= *end + 1 => *end = (*end).max(last),
            _ => out.push((first, last)),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The gathering
// ---------------------------------------------------------------------------

/// File contents read once, a bounded number of them.
struct Files<'a> {
    evidence: &'a dyn Evidence,
    deadline: Instant,
    held: BTreeMap<String, Option<Vec<String>>>,
}

impl<'a> Files<'a> {
    fn get(&mut self, path: &str) -> Option<&Vec<String>> {
        if !self.held.contains_key(path) {
            if self.held.len() >= MAX_FILES || Instant::now() >= self.deadline {
                return None;
            }
            let lines = self
                .evidence
                .lines_at(path, 1, MAX_FILE_LINES, self.deadline)
                .ok();
            self.held.insert(path.to_string(), lines);
        }
        self.held.get(path).and_then(Option::as_ref)
    }
}

/// The terms worth one search among `candidates`: a word the probe found in
/// too many files says nothing, so it is dropped; the rest go rarest first
/// (the probe's weight), words the probe did not weigh after the weighed
/// ones, and at most [`MAX_TERMS`] stay.
pub(crate) fn pick_terms(candidates: &[String], receipt: Option<&Receipt>) -> Vec<String> {
    let mut weighed: Vec<(String, f64)> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    for term in candidates {
        match receipt.and_then(|receipt| receipt.weight_of(term)) {
            Some(weight) if weight <= 0.0 => {}
            Some(weight) => weighed.push((term.clone(), weight)),
            None => unknown.push(term.clone()),
        }
    }
    weighed.sort_by(|a, b| b.1.total_cmp(&a.1));
    weighed
        .into_iter()
        .map(|(term, _)| term)
        .chain(unknown)
        .take(MAX_TERMS)
        .collect()
}

/// One pattern search for all of `terms`; a failed search is an empty one.
fn search(evidence: &dyn Evidence, pattern: &str, deadline: Instant) -> Vec<RichHit> {
    evidence
        .grep(pattern, SEARCH_ROWS, deadline)
        .map(|found| found.hits)
        .unwrap_or_default()
}

/// The terms among `terms` that some row carries (in its text or its path).
fn carried_by(terms: &[String], rows: &[RichHit]) -> Vec<String> {
    terms
        .iter()
        .filter(|term| {
            rows.iter().any(|row| {
                row.text
                    .as_deref()
                    .is_some_and(|text| carries_term(text, term))
                    || carries_term(&row.path, term)
            })
        })
        .cloned()
        .collect()
}

/// The label of a setting block: the names it is about, and what they are.
fn setting_label(group: &[&Candidate]) -> String {
    let mut names: Vec<String> = Vec::new();
    for candidate in group {
        let name = candidate.setting.name.chars().take(48).collect::<String>();
        if !names.contains(&name) && names.len() < 3 {
            names.push(name);
        }
    }
    let mut kinds: Vec<&str> = Vec::new();
    for candidate in group {
        if !kinds.contains(&candidate.setting.shape.label()) {
            kinds.push(candidate.setting.shape.label());
        }
    }
    format!("{} ({})", names.join(", "), kinds.join(", "))
}

/// The setting blocks of a prompt: the best lines cut into windows, a
/// configuration file this short shown whole, and, for a question about the
/// environment, the variables read and where.
fn locate_settings(
    typed: &str,
    ask: Ask,
    evidence: &dyn Evidence,
    receipt: Option<&Receipt>,
    deadline: Instant,
) -> Vec<Block> {
    let picked = pick_terms(&terms(typed), receipt);
    let rows = if picked.is_empty() {
        Vec::new()
    } else {
        // The definitions, and the places that use a name which carries the
        // words (`envVars.VITE_APP_PORT`), asked together.
        let (defined, named) = std::thread::scope(|scope| {
            let defined = scope.spawn(|| search(evidence, &setting_pattern(&picked), deadline));
            let named = search(evidence, &named_pattern(&picked), deadline);
            (defined.join().unwrap_or_default(), named)
        });
        let mut rows = defined;
        rows.extend(named);
        rows
    };
    let wants_tests = answer::asks_tests(&words(typed));
    let ranked = rank_settings(&rows, &picked, wants_tests, MAX_SETTING_LINES * 3);
    let used = carried_by(&picked, &rows);
    let mut files = Files {
        evidence,
        deadline,
        held: BTreeMap::new(),
    };
    // The files are read here: a line in the test module of a source file is
    // a fixture, and an indented lower-case constant a local variable.
    let ranked_refs: Vec<&Candidate> = ranked.iter().collect();
    let kept: Vec<&Candidate> = diversify(&ranked_refs, &used, MAX_SETTING_LINES * 2)
        .into_iter()
        .filter(|candidate| {
            files
                .get(&candidate.path)
                .is_some_and(|lines| is_setting_line(lines, candidate, wants_tests))
        })
        .take(MAX_SETTING_LINES)
        .collect();
    let mut by_file: BTreeMap<String, Vec<&Candidate>> = BTreeMap::new();
    for candidate in kept {
        by_file
            .entry(candidate.path.clone())
            .or_default()
            .push(candidate);
    }
    // Files in the order of their best line.
    let mut order: Vec<(&String, i64)> = by_file
        .iter()
        .map(|(path, lines)| (path, lines.iter().map(|c| c.score).max().unwrap_or(0)))
        .collect();
    order.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let mut blocks: Vec<Block> = Vec::new();
    for (path, _) in order {
        let Some(lines) = files.get(path).cloned() else {
            continue;
        };
        let group = &by_file[path];
        let label = setting_label(group);
        if is_config_file(path) && lines.len() <= WHOLE_CONFIG_LINES {
            if let Some(block) =
                region_pack::range_block(label, path, &lines, 1, lines.len() as u64)
            {
                blocks.push(masked(block));
            }
            continue;
        }
        let windows = merge(
            group
                .iter()
                .filter_map(|candidate| {
                    setting_window(&lines, candidate.line, candidate.setting.shape)
                })
                .collect(),
        );
        for (first, last) in windows {
            if let Some(block) = region_pack::range_block(label.clone(), path, &lines, first, last)
            {
                blocks.push(masked(block));
            }
        }
    }
    if ask.env {
        blocks.extend(env_block(evidence, &rows, &used, deadline));
    }
    blocks
}

/// A quoted run of key characters, long enough to be a token.
static QUOTED_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(["'`])([A-Za-z0-9+=\-]{20,})(["'`])"#).expect("a literal pattern")
});

/// `line` with every quoted token that reads as a credential (long, letters
/// and digits, no word separators) replaced: a default that is a credential
/// is the one value a brief must not print. Identifiers and paths are not
/// quoted runs of that shape, so code stays readable.
pub(crate) fn mask_line(line: &str) -> String {
    QUOTED_TOKEN
        .replace_all(line, |found: &regex::Captures| {
            let token = &found[2];
            let secret = token.chars().any(|ch| ch.is_ascii_digit())
                && token.chars().any(|ch| ch.is_ascii_alphabetic());
            if secret {
                format!("{0}<redacted>{0}", &found[1])
            } else {
                found[0].to_string()
            }
        })
        .into_owned()
}

/// `block` with every credential-shaped literal of its lines masked.
fn masked(mut block: Block) -> Block {
    block.map_body(mask_line);
    block
}

/// Rows one environment search pulls: every read of the repository, not
/// only the ones that carry a term.
const ENV_ROWS: usize = 600;
/// Names that every program reads and no repository configures.
const COMMON_ENV: &[&str] = &[
    "HOME",
    "PATH",
    "USER",
    "PWD",
    "SHELL",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LANG",
    "TERM",
    "CI",
    "EDITOR",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "CARGO_MANIFEST_DIR",
    "OUT_DIR",
    "NODE_ENV",
    "PWD",
    "LOGNAME",
    "USERPROFILE",
    "APPDATA",
];
/// Directories whose environment reads configure nothing the question is about.
const AUXILIARY_DIRS: &[&str] = &[
    "scripts", "script", "examples", "example", "benches", "bench", "fixtures", "tools", "ci",
];

/// Whether `path` is where a repository configures itself from the
/// environment: not a hidden directory, a script, an example or a bench.
fn is_primary_path(path: &str) -> bool {
    !path
        .split('/')
        .any(|part| part.starts_with('.') || AUXILIARY_DIRS.contains(&part))
}

/// `names` in the order a reader wants them: those the question's `terms`
/// name first, then those in the repository's own namespace (the prefix
/// most names share: `PIXEL_`, `VITE_APP_`), the rest last; each group in
/// the order found. A name every program reads is dropped.
pub(crate) fn rank_env_names(
    mut names: Vec<(String, String, u64)>,
    terms: &[String],
) -> Vec<(String, String, u64)> {
    names.retain(|(name, _, _)| !COMMON_ENV.contains(&name.as_str()));
    let prefix = |name: &str| name.split('_').next().unwrap_or(name).to_string();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for (name, _, _) in &names {
        *counts.entry(prefix(name)).or_default() += 1;
    }
    let mut keyed: Vec<(usize, (String, String, u64))> = names
        .into_iter()
        .map(|entry| {
            let group = if carries(&entry.0, terms) {
                0
            } else if counts.get(&prefix(&entry.0)).copied().unwrap_or(0) >= 3 {
                1
            } else {
                2
            };
            (group, entry)
        })
        .collect();
    keyed.sort_by_key(|(group, _)| *group);
    keyed.into_iter().map(|(_, entry)| entry).collect()
}

/// The environment variables the repository reads: those the setting rows
/// already read first, then the first reads the whole repository has.
fn env_block(
    evidence: &dyn Evidence,
    setting_rows: &[RichHit],
    terms: &[String],
    deadline: Instant,
) -> Option<Block> {
    let mut rows: Vec<RichHit> = setting_rows
        .iter()
        .filter(|row| row.text.as_deref().is_some_and(|text| ENV.is_match(text)))
        .cloned()
        .collect();
    if let Ok(found) = evidence.grep(&env_pattern(), ENV_ROWS, deadline) {
        rows.extend(found.hits);
    }
    rows.retain(|row| {
        !region_pack::is_noise_path(&row.path)
            && !answer::is_test_path(&row.path)
            && !answer::is_docs_path(&row.path)
            && is_primary_path(&row.path)
    });
    let mut names = rank_env_names(env_names(&rows), terms);
    names.truncate(MAX_ENV_NAMES);
    if names.is_empty() {
        return None;
    }
    let body = names
        .into_iter()
        .map(|(name, path, line)| format!("{name} — read at {path}:{line}"))
        .collect();
    Some(masked(Block::new(
        "environment variables read (name — first read)".to_string(),
        body,
    )))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The end of a short term in a name or a title: an `s` allowed, then a
/// non-letter, the end of the line, or the start of the next camel-case word.
const SHORT_END: &str = "(?i:s)?(?:[^A-Za-z]|[A-Z][a-z]|$)";

/// `terms` as snake-case or upper-case segments of a name that follow an
/// underscore or start it, and as capitalised camel-case ones.
fn name_segments(terms: &[String]) -> (String, String) {
    let mut snake: Vec<String> = Vec::new();
    let mut camel: Vec<String> = Vec::new();
    for term in terms {
        let lower = regex::escape(term);
        let mut chars = term.chars();
        let capital = chars
            .next()
            .map(|first| format!("{}{}", first.to_ascii_uppercase(), chars.as_str()))
            .unwrap_or_default();
        let end = if term.len() >= answer::LONG_STEM {
            ""
        } else {
            SHORT_END
        };
        snake.push(format!("(?i:{lower}){end}"));
        camel.push(format!("{}{end}", regex::escape(&capital)));
    }
    (
        format!("(?:\\w*_)?(?:{})", snake.join("|")),
        format!("\\w*(?:{})", camel.join("|")),
    )
}

/// The pattern that finds the declaration of a test whose name (or, for a
/// JavaScript test, whose title) carries any of `terms`: a Python `def` whose
/// name starts with `test`, a Rust function, a Go `Test` function, an
/// `it(...)` or `test(...)` call.
pub(crate) fn test_pattern(terms: &[String]) -> String {
    let (snake, camel) = name_segments(terms);
    let words: Vec<String> = terms
        .iter()
        .map(|term| {
            let end = if term.len() >= answer::LONG_STEM {
                ""
            } else {
                "(?i:s)?(?:[^A-Za-z]|$)"
            };
            format!("(?i:{}){end}", regex::escape(term))
        })
        .collect();
    // The title is prose: the word starts it, or follows a non-letter.
    let title = format!("(?:[^'\"`\\n]*[^A-Za-z'\"`\\n])?(?:{})", words.join("|"));
    format!(
        "(?:\\bdef\\s+test(?:{snake}|{camel})|\\b(?:async\\s+)?fn\\s+(?:{snake}|{camel})\\w*\\s*[(<]|\\bfunc\\s+Test{camel}|\\b(?:it|test)(?:\\.\\w+)?\\s*\\(\\s*['\"`]{title})"
    )
}

/// A test function found by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TestHit {
    pub(crate) path: String,
    pub(crate) line: u64,
    pub(crate) name: String,
    /// How many of the terms the name (or the path) carries.
    pub(crate) score: usize,
}

/// The tests among the `rows` of a test search: a declaration `lines` of its
/// file confirm is a test (an attribute above a Rust `fn`, the `test` of a
/// Python `def`), ranked by the terms its name carries.
pub(crate) fn confirm_test(
    path: &str,
    row: u64,
    lines: &[String],
    terms: &[String],
) -> Option<TestHit> {
    let at = usize::try_from(row.checked_sub(1)?).ok()?;
    if at >= lines.len() {
        return None;
    }
    let name = answer::test_name(lines, at)?;
    let score = terms
        .iter()
        .filter(|term| carries_term(&name, term) || carries_term(path, term))
        .count();
    Some(TestHit {
        path: path.to_string(),
        line: row,
        name,
        score,
    })
}

/// The command that runs the tests whose name carries `term`, from the
/// language of the test file and what the manifests say; `None` when nothing
/// vouches for a runner.
pub(crate) fn run_hint(path: &str, term: &str, manifests: &[String]) -> Option<String> {
    let mentions = |needle: &str| manifests.iter().any(|text| text.contains(needle));
    let extension = path.rsplit_once('.').map(|(_, extension)| extension)?;
    match extension {
        "rs" => Some(format!("cargo test {term}")),
        "py" if mentions("pytest") => Some(format!("pytest -k {term}")),
        "go" => Some(format!("go test ./... -run {term}")),
        "ts" | "tsx" | "js" | "jsx" | "mjs" if mentions("vitest") => {
            Some(format!("vitest run -t {term}"))
        }
        "ts" | "tsx" | "js" | "jsx" | "mjs" if mentions("jest") => Some(format!("jest -t {term}")),
        _ => None,
    }
}

/// Manifests read for [`run_hint`].
const MANIFESTS: &[&str] = &[
    "pyproject.toml",
    "tox.ini",
    "setup.cfg",
    "pytest.ini",
    "package.json",
];
/// Lines of a manifest read.
const MANIFEST_LINES: u64 = 600;

/// The test blocks of a prompt: the tests whose name carries its words, each
/// with its first lines, the rest by name, and the command that runs them.
fn locate_tests(
    typed: &str,
    evidence: &dyn Evidence,
    receipt: Option<&Receipt>,
    deadline: Instant,
) -> Vec<Block> {
    let picked = pick_terms(&terms(typed), receipt);
    if picked.is_empty() {
        return Vec::new();
    }
    let rows: Vec<RichHit> = search(evidence, &test_pattern(&picked), deadline)
        .into_iter()
        .filter(|row| !region_pack::is_noise_path(&row.path))
        .collect();
    let used = carried_by(&picked, &rows);
    let mut files = Files {
        evidence,
        deadline,
        held: BTreeMap::new(),
    };
    let mut hits: Vec<TestHit> = Vec::new();
    // Test files first, then the files that hold the most rows: a test file
    // about the topic has several, and a source file with an inline test
    // module has the rows of every function that carries the word.
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for row in &rows {
        *counts.entry(row.path.clone()).or_default() += 1;
    }
    let mut paths: Vec<(String, usize)> = counts.into_iter().collect();
    paths.sort_by(|a, b| {
        answer::is_test_path(&b.0)
            .cmp(&answer::is_test_path(&a.0))
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.0.cmp(&b.0))
    });
    for (path, _) in paths {
        let Some(lines) = files.get(&path) else {
            continue;
        };
        for row in rows.iter().filter(|row| row.path == path) {
            if let Some(hit) = confirm_test(&path, row.line, lines, &used) {
                hits.push(hit);
            }
        }
    }
    if hits.is_empty() {
        return Vec::new();
    }
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
    let total = hits.len();
    let mut blocks: Vec<Block> = Vec::new();
    let mut list: Vec<String> = hits
        .iter()
        .take(MAX_TEST_NAMES)
        .map(|hit| format!("{}:{} {}", hit.path, hit.line, hit.name))
        .collect();
    if let Some(term) = used.first()
        && let Some(first) = hits.first()
    {
        let manifests: Vec<String> = MANIFESTS
            .iter()
            .filter_map(|name| {
                evidence
                    .lines_at(name, 1, MANIFEST_LINES, deadline)
                    .ok()
                    .map(|lines| lines.join("\n"))
            })
            .collect();
        if let Some(command) = run_hint(&first.path, term, &manifests) {
            list.push(format!("run only these: {command}"));
        }
    }
    if total > MAX_TEST_NAMES {
        list.push(format!("… {} more tests match", total - MAX_TEST_NAMES));
    }
    blocks.push(Block::new(
        format!("tests matching {} ({total} found)", used.join(", ")),
        list,
    ));
    let mut shown: BTreeMap<String, usize> = BTreeMap::new();
    for hit in &hits {
        if blocks.len() > MAX_TEST_BODIES {
            break;
        }
        let count = shown.entry(hit.path.clone()).or_default();
        if *count >= BODIES_PER_FILE {
            continue;
        }
        let Some(lines) = files.get(&hit.path) else {
            continue;
        };
        let Ok(start) = usize::try_from(hit.line.saturating_sub(1)) else {
            continue;
        };
        let (end, _) = region_pack::decl_end(lines, start, TEST_BODY_LINES);
        if let Some(block) = region_pack::range_block(
            format!("{} (test)", hit.name),
            &hit.path,
            lines,
            hit.line,
            end as u64 + 1,
        ) {
            *count += 1;
            blocks.push(masked(block));
        }
    }
    blocks
}

/// What the locators find for `typed`, within [`LOCATOR_WINDOW`] of now and
/// the brief's `deadline`: blocks of the pack, best first. Empty when the
/// prompt asks for neither a setting nor tests.
pub(crate) fn locate(
    typed: &str,
    evidence: &dyn Evidence,
    receipt: Option<&Receipt>,
    deadline: Instant,
) -> Vec<Block> {
    let ask = asks(typed);
    if !ask.any() {
        return Vec::new();
    }
    let deadline = deadline.min(Instant::now() + LOCATOR_WINDOW);
    let mut blocks = Vec::new();
    if ask.setting {
        blocks.extend(locate_settings(typed, ask, evidence, receipt, deadline));
    }
    if ask.tests {
        blocks.extend(locate_tests(typed, evidence, receipt, deadline));
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::{CallerHit, Found, RichFound, SymbolHit};
    use crate::relevance::{KeywordStat, RelevanceInput};

    fn row(path: &str, line: u64, text: &str) -> RichHit {
        RichHit {
            path: path.to_string(),
            line,
            text: Some(text.to_string()),
        }
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(ToString::to_string).collect()
    }

    // ---- what a prompt asks ------------------------------------------------

    #[test]
    fn asks_should_read_a_setting_from_its_nouns_and_a_value_from_its_opener() {
        let ask = |text: &str| asks(text);
        assert!(ask("where do I set the config for the remote key").setting);
        assert!(ask("which environment variables change how it behaves").setting);
        assert!(ask("what port does the dev server use").setting);
        assert!(ask("how do I turn that flag off").setting);
        assert!(ask("requests follows up to thirty redirects by default").setting);
        // A limit alone is a word of code; inside a question about a value it
        // is a setting.
        assert!(ask("what is the maximum number of redirects").setting);
        assert!(!ask("the limit of the loop is wrong").setting);
        assert!(!ask("rename the helper and update its callers").setting);
        // The environment is asked about by its own words only.
        assert!(ask("which env vars does it read").env);
        assert!(!ask("what port does the dev server use").env);
    }

    #[test]
    fn asks_should_read_tests_from_the_words_of_testing() {
        assert!(asks("which tests cover redirect handling").tests);
        assert!(asks("how is the parser tested").tests);
        assert!(asks("where is the coverage for the cache").tests);
        assert!(!asks("how does the parser work").tests);
        // `specific` is not `spec`: words match whole.
        assert!(!asks("be more specific about the cache").tests);
        assert!(!asks("rename the helper").any());
        assert!(asks("which tests cover the default port").any());
    }

    #[test]
    fn terms_should_keep_the_subject_and_drop_what_only_says_what_is_asked() {
        assert_eq!(
            terms("which tests cover redirect handling and how would I run only those"),
            ["redirect"]
        );
        assert_eq!(
            terms("where does the daemon keep its global config and how do I set the remote key"),
            ["daemon", "remote"]
        );
        // A plural and a verb ending both reach the stem of the text's words.
        assert_eq!(terms("rows"), ["row"]);
        assert_eq!(terms("redirecting"), ["redirect"]);
        // A port is the setting itself, not a word that says one is asked.
        assert!(terms("what port does the dev server use").contains(&"port".to_string()));
        // At most CANDIDATE_TERMS, none twice.
        let many =
            terms("alpha1 bravo2 charlie3 delta4 echo55 foxtrot6 golf77 hotel88 india99 juliet0");
        assert_eq!(many.len(), CANDIDATE_TERMS);
        assert_eq!(terms("redirect redirects redirected").len(), 1);
    }

    // ---- the words a line carries ------------------------------------------

    #[test]
    fn carries_term_should_find_a_segment_and_not_a_substring() {
        assert!(carries_term("pub const SERVER_PORT: u16 = 80;", "port"));
        assert!(carries_term("let serverPort = 3000", "port"));
        assert!(carries_term("ports = [1, 2]", "port"));
        assert!(!carries_term("import { x } from './portal'", "port"));
        assert!(!carries_term("let report = 1", "port"));
        assert!(!carries_term("export default x", "port"));
        // A long term is also the start of a longer word.
        assert!(carries_term("max_redirects = 30", "redirect"));
        assert!(carries_term("redirecting = true", "redirect"));
        assert!(!carries_term("redirect_to_login", "login_redirect"));
    }

    #[test]
    fn the_search_pattern_should_agree_with_carries_term_on_what_a_line_carries() {
        let pattern = Regex::new(&setting_pattern(&strings(&["port"]))).unwrap();
        for (line, found) in [
            ("pub const SERVER_PORT: u16 = 80;", true),
            ("export const serverPort = 3000;", true),
            ("PORT = 8080", true),
            ("const PORTAL_URL = \"x\";", false),
            ("import { x } from './portal';", false),
            ("let report = 1;", false),
            ("const IMPORT_MAP: usize = 3;", false),
        ] {
            assert_eq!(pattern.is_match(line), found, "{line}");
        }
        let pattern = Regex::new(&setting_pattern(&strings(&["redirect"]))).unwrap();
        for (line, found) in [
            ("DEFAULT_REDIRECT_LIMIT = 30", true),
            ("self.max_redirects = 30", true),
            ("    max_redirects: 30,", true),
            ("let redirects = vec![];", false),
            ("fn follow_redirects(&self) {", false),
            ("os.environ.get(\"REDIRECT_MAX\")", true),
            (".long(\"redirects\")", true),
            ("default_value = \"30 redirects\"", true),
        ] {
            assert_eq!(pattern.is_match(line), found, "{line}");
        }
    }

    #[test]
    fn the_named_pattern_should_find_an_upper_case_name_by_its_segment() {
        let pattern = Regex::new(&named_pattern(&strings(&["port"]))).unwrap();
        for (line, found) in [
            ("port: Number(envVars.VITE_APP_PORT),", true),
            ("  VITE_APP_PORT: string;", true),
            ("PORT=3000", true),
            ("VITE_APP_PORTAL_URL: string;", false),
            ("const x = IMPORT_MAP;", false),
            ("let port = 3;", false),
        ] {
            assert_eq!(pattern.is_match(line), found, "{line}");
        }
    }

    // ---- shapes ------------------------------------------------------------

    fn shape(path: &str, text: &str, terms: &[&str]) -> Option<(Shape, String)> {
        shape_of(path, text, &strings(terms)).map(|setting| (setting.shape, setting.name))
    }

    #[test]
    fn shape_of_should_name_the_setting_a_line_defines() {
        let constant = |name: &str| Some((Shape::Constant, name.to_string()));
        assert_eq!(
            shape(
                "src/lib.rs",
                "pub const SEARCH_DEFAULT_ROWS: usize = 100;",
                &["row"]
            ),
            constant("SEARCH_DEFAULT_ROWS")
        );
        assert_eq!(
            shape("a.ts", "export const maxRedirects = 30;", &["redirect"]),
            constant("maxRedirects")
        );
        assert_eq!(
            shape("a.py", "DEFAULT_REDIRECT_LIMIT = 30", &["redirect"]),
            constant("DEFAULT_REDIRECT_LIMIT")
        );
        assert_eq!(
            shape(
                "a.java",
                "private static final int MAX_RETRIES = 3;",
                &["retrie"]
            ),
            constant("MAX_RETRIES"),
            "`retries` reads as `retrie` in a term and in the text alike"
        );
        assert_eq!(
            shape(
                "a.java",
                "private static final int MAX_RETRY = 3;",
                &["retry"]
            ),
            constant("MAX_RETRY")
        );
        // A `const fn` names no value.
        assert_eq!(
            shape("a.rs", "pub const fn rows(&self) -> usize {", &["row"]),
            None
        );
        // The name must carry the term, not the value.
        assert_eq!(
            shape(
                "a.rs",
                "const NO_CALLER_NOTE: &str = \"search the callers\";",
                &["search"]
            ),
            None
        );
    }

    #[test]
    fn shape_of_should_read_an_environment_read_a_flag_a_default_and_a_key() {
        assert_eq!(
            shape(
                "a.rs",
                "let x = std::env::var(\"PIXEL_REMOTE_KEY\").ok();",
                &["remote"]
            ),
            Some((Shape::Env, "PIXEL_REMOTE_KEY".to_string()))
        );
        assert_eq!(
            shape("a.js", "const p = process.env.VITE_APP_PORT;", &["port"]),
            Some((Shape::Env, "VITE_APP_PORT".to_string()))
        );
        assert_eq!(
            shape("a.rs", "std::env::var(DAEMON_IDLE_ENV)", &["idle"]),
            Some((Shape::Env, "DAEMON_IDLE_ENV".to_string()))
        );
        assert_eq!(
            shape("a.rs", "#[arg(long = \"redirects\")]", &["redirect"]),
            Some((Shape::Flag, "redirects".to_string()))
        );
        assert_eq!(
            shape(
                "a.py",
                "parser.add_argument(\"--max-redirects\", type=int)",
                &["redirect"]
            ),
            Some((Shape::Flag, "--max-redirects".to_string()))
        );
        let default = shape(
            "a.rs",
            "#[arg(default_value = \"30 redirects\")]",
            &["redirect"],
        );
        assert_eq!(default.map(|(shape, _)| shape), Some(Shape::Default));
        assert_eq!(
            shape("a.py", "    self.max_redirects = 30", &["redirect"]),
            Some((Shape::Key, "self.max_redirects".to_string()))
        );
        assert_eq!(
            shape("config.toml", "redirect_limit = \"high\"", &["redirect"]),
            Some((Shape::Key, "redirect_limit".to_string())),
            "a configuration file takes any value"
        );
        // A key in code needs a literal; a type annotation is not one.
        assert_eq!(
            shape("a.rs", "    redirects: Vec<Url>,", &["redirect"]),
            None
        );
        // A line inside a string literal is a fixture.
        assert_eq!(
            shape("a.rs", "\"redirect: {limit: 3}\"", &["redirect"]),
            None
        );
        assert_eq!(
            shape("a.ts", "port: Number(envVars.VITE_APP_PORT),", &["port"]),
            Some((Shape::Named, "VITE_APP_PORT".to_string()))
        );
        assert_eq!(shape("a.ts", "let report = 3", &["port"]), None);
    }

    #[test]
    fn mask_line_should_hide_a_credential_shaped_literal_and_keep_code() {
        assert_eq!(
            mask_line("const KEY = \"sk-abcdef0123456789abcdef\";"),
            "const KEY = \"<redacted>\";"
        );
        assert_eq!(
            mask_line("nonce=\"6bf5d6e4da1ce66918800195d6b9130d\""),
            "nonce=\"<redacted>\""
        );
        for kept in [
            "def test_digestauth_401_count_reset_on_redirect(httpbin_secure):",
            "const NAME = \"a-very-long-name-with-only-letters\";",
            "url = 'https://example.org/a/b/c/d/e/f/g/h'",
        ] {
            assert_eq!(mask_line(kept), kept);
        }
    }

    // ---- ranking -----------------------------------------------------------

    #[test]
    fn rank_settings_should_prefer_the_constant_that_names_the_words_and_drop_noise() {
        let rows = vec![
            row("src/a.py", 3, "    self.max_redirects = 30"),
            row("src/a.py", 9, "DEFAULT_REDIRECT_LIMIT = 30"),
            row("tests/test_a.py", 4, "MAX_REDIRECTS = 5"),
            row("docs/guide.md", 2, "REDIRECT_LIMIT = 3"),
            row("dist/gen.rs", 1, "const REDIRECT: u8 = 1;"),
            row("src/b.py", 1, "REDIRECT_NOTE = \"no\""),
            row("src/c.py", 7, "x = redirect(url)"),
        ];
        let terms = strings(&["redirect"]);
        let ranked = rank_settings(&rows, &terms, false, 10);
        let places: Vec<(&str, u64)> = ranked
            .iter()
            .map(|candidate| (candidate.path.as_str(), candidate.line))
            .collect();
        // Test and documentation files are out unless asked for; a call is not
        // a setting; an upper-case constant outranks a key.
        assert_eq!(places, [("src/a.py", 9), ("src/b.py", 1), ("src/a.py", 3)]);
        let with_tests = rank_settings(&rows, &terms, true, 10);
        assert!(with_tests.iter().any(|c| c.path == "tests/test_a.py"));
        assert!(with_tests.iter().all(|c| c.path != "docs/guide.md"));
    }

    #[test]
    fn rank_settings_should_keep_a_few_lines_per_file_and_cut_at_the_limit() {
        let rows: Vec<RichHit> = (1..=9)
            .map(|line| row("src/big.py", line, &format!("REDIRECT_{line} = {line}")))
            .chain(std::iter::once(row("src/z.py", 1, "REDIRECT_ONE = 1")))
            .collect();
        let ranked = rank_settings(&rows, &strings(&["redirect"]), false, 10);
        assert_eq!(
            ranked.iter().filter(|c| c.path == "src/big.py").count(),
            PER_FILE
        );
        assert!(ranked.iter().any(|c| c.path == "src/z.py"));
        assert_eq!(
            rank_settings(&rows, &strings(&["redirect"]), false, 2).len(),
            2
        );
    }

    #[test]
    fn diversify_should_give_each_term_its_best_lines_before_the_rest() {
        let rows = vec![
            row("a.ts", 1, "export const collabAAA = 1;"),
            row("a.ts", 2, "export const collabBBB = 1;"),
            row("a.ts", 3, "export const collabCCC = 1;"),
            row("b.ts", 1, "port: Number(envVars.VITE_APP_PORT),"),
        ];
        let terms = strings(&["collab", "port"]);
        let ranked = rank_settings(&rows, &terms, false, 10);
        let refs: Vec<&Candidate> = ranked.iter().collect();
        let names = |chosen: Vec<&Candidate>| -> Vec<String> {
            chosen.iter().map(|c| c.setting.name.clone()).collect()
        };
        let two = names(diversify(&refs, &terms, 2));
        assert!(two.contains(&"VITE_APP_PORT".to_string()), "{two:?}");
        assert_eq!(names(diversify(&refs, &terms, 10)).len(), 4);
        assert_eq!(diversify(&refs, &terms, 0).len(), 0);
    }

    #[test]
    fn is_setting_line_should_refuse_a_test_module_and_a_local_variable() {
        let source = strings(&[
            "pub const MAX_ROWS: usize = 10;",
            "fn f() {",
            "    let rows = 3;",
            "    const rows_local: usize = 2;",
            "}",
            "#[cfg(test)]",
            "mod tests {",
            "    const FIXTURE_ROWS: usize = 3;",
            "}",
        ]);
        let candidate = |line: u64, name: &str| Candidate {
            path: "a.rs".into(),
            line,
            setting: Setting {
                shape: Shape::Constant,
                name: name.into(),
            },
            score: 1,
        };
        assert!(is_setting_line(&source, &candidate(1, "MAX_ROWS"), false));
        assert!(!is_setting_line(
            &source,
            &candidate(4, "rows_local"),
            false
        ));
        assert!(!is_setting_line(
            &source,
            &candidate(8, "FIXTURE_ROWS"),
            false
        ));
        assert!(is_setting_line(
            &source,
            &candidate(8, "FIXTURE_ROWS"),
            true
        ));
    }

    // ---- windows -----------------------------------------------------------

    #[test]
    fn setting_window_should_take_the_doc_comment_and_the_whole_constant() {
        let source = strings(&[
            "use x;",
            "",
            "/// Rows a search returns.",
            "/// Raised by the caller.",
            "pub const SEARCH_DEFAULT_ROWS: usize = 100;",
            "",
            "pub const NAMES: &[&str] = &[",
            "    \"a\",",
            "    \"b\",",
            "];",
        ]);
        assert_eq!(setting_window(&source, 5, Shape::Constant), Some((3, 5)));
        assert_eq!(setting_window(&source, 7, Shape::Constant), Some((7, 10)));
        // A read or a key keeps the lines around it.
        assert_eq!(setting_window(&source, 5, Shape::Env), Some((3, 7)));
        assert_eq!(setting_window(&source, 1, Shape::Key), Some((1, 3)));
        assert_eq!(setting_window(&source, 99, Shape::Key), None);
        assert_eq!(setting_window(&source, 0, Shape::Key), None);
    }

    #[test]
    fn merge_should_join_windows_that_overlap_or_touch_and_sort_them() {
        assert_eq!(
            merge(vec![(10, 12), (1, 3), (4, 6), (11, 15), (20, 21)]),
            [(1, 6), (10, 15), (20, 21)]
        );
        assert!(merge(Vec::new()).is_empty());
    }

    // ---- terms and the receipt ---------------------------------------------

    fn receipt(keywords: &[(&str, f64)]) -> Receipt {
        Receipt::new(
            &RelevanceInput {
                graph: true,
                files_considered: 100,
                structural_files: 1,
                keywords: keywords
                    .iter()
                    .map(|(keyword, weight)| KeywordStat {
                        keyword: (*keyword).to_string(),
                        weight: *weight,
                        french_only: false,
                    })
                    .collect(),
                cofiles: Vec::new(),
            },
            None,
        )
        .expect("a keyword weighs")
    }

    #[test]
    fn pick_terms_should_drop_the_common_and_put_the_rare_first() {
        let candidates = strings(&["server", "port", "collab", "unseen", "other", "fifth"]);
        let receipt = receipt(&[("server", 0.0), ("port", 1.0), ("collab", 4.0)]);
        // `server` is in too many files; `collab` outweighs `port`; the words
        // the probe did not weigh follow; four stay.
        assert_eq!(
            pick_terms(&candidates, Some(&receipt)),
            ["collab", "port", "unseen", "other"]
        );
        // Without a probe all stay, in the order of the prompt.
        assert_eq!(
            pick_terms(&candidates, None),
            ["server", "port", "collab", "unseen"]
        );
        assert!(pick_terms(&[], Some(&receipt)).is_empty());
    }

    // ---- environment variables ----------------------------------------------

    #[test]
    fn env_names_should_list_each_variable_once_with_its_first_read() {
        let rows = vec![
            row("a.rs", 3, "let a = std::env::var(\"PIXEL_A\");"),
            row("b.rs", 9, "env::var_os(\"PIXEL_A\")"),
            row("b.rs", 12, "env::var(KEY_ENV)"),
            row("c.js", 1, "process.env.VITE_PORT"),
            row("d.py", 2, "os.environ.get('HOME')"),
            row("e.py", 2, "os.environ['PIXEL_DIR']"),
            row("f.rs", 1, "no environment here"),
        ];
        let names: Vec<(String, String, u64)> = env_names(&rows);
        assert_eq!(
            names,
            [
                ("PIXEL_A".to_string(), "a.rs".to_string(), 3),
                ("KEY_ENV".to_string(), "b.rs".to_string(), 12),
                ("VITE_PORT".to_string(), "c.js".to_string(), 1),
                ("HOME".to_string(), "d.py".to_string(), 2),
                ("PIXEL_DIR".to_string(), "e.py".to_string(), 2),
            ]
        );
    }

    #[test]
    fn rank_env_names_should_put_the_questions_words_then_the_repositorys_namespace_first() {
        let name = |name: &str| (name.to_string(), "x.rs".to_string(), 1);
        let ranked = rank_env_names(
            vec![
                name("HOME"),
                name("OTHER_THING"),
                name("PIXEL_ONE"),
                name("PIXEL_TWO"),
                name("PIXEL_REMOTE_KEY"),
                name("PIXEL_THREE"),
                name("LONELY"),
            ],
            &strings(&["remote"]),
        );
        let order: Vec<&str> = ranked.iter().map(|(name, _, _)| name.as_str()).collect();
        assert_eq!(
            order,
            [
                "PIXEL_REMOTE_KEY",
                "PIXEL_ONE",
                "PIXEL_TWO",
                "PIXEL_THREE",
                "OTHER_THING",
                "LONELY"
            ]
        );
        assert!(is_primary_path("crates/a/src/lib.rs"));
        assert!(!is_primary_path(".github/workflows/ci.yml"));
        assert!(!is_primary_path("scripts/build.py"));
        assert!(!is_primary_path("crates/x/benches/b.rs"));
    }

    // ---- tests -------------------------------------------------------------

    #[test]
    fn the_test_pattern_should_find_a_test_declaration_by_a_word_of_its_name() {
        let pattern = Regex::new(&test_pattern(&strings(&["redirect"]))).unwrap();
        for (line, found) in [
            ("def test_redirect_history_no_self_reference(self):", true),
            (
                "    def test_HTTP_302_ALLOW_REDIRECT_GET(self, httpbin):",
                true,
            ),
            ("fn redirects_follow_the_limit() {", true),
            ("async fn it_redirects_once() {", true),
            ("func TestRedirectLimit(t *testing.T) {", true),
            (
                "  it(\"follows a redirect to the same host\", () => {",
                true,
            ),
            ("test('redirect loops', async () => {", true),
            ("def helper_for_redirect(self):", false),
            ("def testRedirectFollows(self):", true),
            ("def test_cookies(self):", false),
            ("it(\"adds numbers\", () => {", false),
        ] {
            assert_eq!(pattern.is_match(line), found, "{line}");
        }
    }

    #[test]
    fn confirm_test_should_accept_only_a_declaration_the_language_says_is_a_test() {
        let source = strings(&[
            "fn redirects_in_production() {}",
            "",
            "#[test]",
            "fn redirects_follow_the_limit() {",
            "    assert!(true);",
            "}",
            "def test_redirect_history(self):",
        ]);
        let terms = strings(&["redirect", "history"]);
        assert_eq!(confirm_test("src/lib.rs", 1, &source, &terms), None);
        let hit = confirm_test("src/lib.rs", 4, &source, &terms).unwrap();
        assert_eq!(
            (hit.name.as_str(), hit.line, hit.score),
            ("redirects_follow_the_limit", 4, 1)
        );
        let python = confirm_test("tests/test_x.py", 7, &source, &terms).unwrap();
        assert_eq!(
            (python.name.as_str(), python.score),
            ("test_redirect_history", 2)
        );
        assert_eq!(confirm_test("a.rs", 0, &source, &terms), None);
        assert_eq!(confirm_test("a.rs", 99, &source, &terms), None);
    }

    #[test]
    fn run_hint_should_name_a_runner_only_when_something_vouches_for_it() {
        let pytest = strings(&["[tool.pytest.ini_options]\naddopts = \"-q\""]);
        assert_eq!(
            run_hint("tests/test_a.py", "redirect", &pytest).as_deref(),
            Some("pytest -k redirect")
        );
        assert_eq!(run_hint("tests/test_a.py", "redirect", &[]), None);
        assert_eq!(
            run_hint("src/lib.rs", "redirect", &[]).as_deref(),
            Some("cargo test redirect")
        );
        assert_eq!(
            run_hint("a_test.go", "Redirect", &[]).as_deref(),
            Some("go test ./... -run Redirect")
        );
        let vitest = strings(&["{\"devDependencies\": {\"vitest\": \"^2\"}}"]);
        assert_eq!(
            run_hint("src/a.test.ts", "zoom", &vitest).as_deref(),
            Some("vitest run -t zoom")
        );
        let jest = strings(&["{\"devDependencies\": {\"jest\": \"^29\"}}"]);
        assert_eq!(
            run_hint("src/a.test.js", "zoom", &jest).as_deref(),
            Some("jest -t zoom")
        );
        assert_eq!(run_hint("src/a.test.js", "zoom", &[]), None);
        assert_eq!(run_hint("Makefile", "zoom", &vitest), None);
    }

    // ---- the whole locator ---------------------------------------------------

    /// A repository of files, searched with the real pattern like the daemon
    /// does.
    #[derive(Default)]
    struct Repo {
        files: Vec<(String, Vec<String>)>,
    }

    impl Repo {
        fn with(mut self, path: &str, source: &[&str]) -> Self {
            self.files.push((path.to_string(), strings(source)));
            self
        }
    }

    impl Evidence for Repo {
        fn files_with(&self, _: &str, _: Instant) -> Result<Found, String> {
            Err("unsupported".into())
        }
        fn grep(&self, pattern: &str, limit: usize, _: Instant) -> Result<RichFound, String> {
            let regex = Regex::new(pattern).map_err(|error| error.to_string())?;
            let mut hits = Vec::new();
            for (path, lines) in &self.files {
                for (index, text) in lines.iter().enumerate() {
                    if regex.is_match(text) && hits.len() < limit {
                        hits.push(RichHit {
                            path: path.clone(),
                            line: index as u64 + 1,
                            text: Some(text.clone()),
                        });
                    }
                }
            }
            Ok(RichFound {
                hits,
                capped: false,
            })
        }
        fn concept(&self, _: &str, _: Instant) -> Result<Found, String> {
            Err("unsupported".into())
        }
        fn symbols(&self, _: &str, _: Instant) -> Result<Vec<SymbolHit>, String> {
            Ok(Vec::new())
        }
        fn callers(&self, _: &str, _: Instant) -> Result<Vec<CallerHit>, String> {
            Ok(Vec::new())
        }
        fn lines_at(
            &self,
            path: &str,
            start: u64,
            end: u64,
            _: Instant,
        ) -> Result<Vec<String>, String> {
            let (_, lines) = self
                .files
                .iter()
                .find(|(held, _)| held == path)
                .ok_or_else(|| "no such file".to_string())?;
            Ok(lines
                .iter()
                .skip(usize::try_from(start.saturating_sub(1)).unwrap_or(0))
                .take(usize::try_from(end + 1 - start).unwrap_or(0))
                .cloned()
                .collect())
        }
    }

    /// A source that cannot search.
    struct Down;
    impl Evidence for Down {
        fn files_with(&self, _: &str, _: Instant) -> Result<Found, String> {
            Err("down".into())
        }
        fn concept(&self, _: &str, _: Instant) -> Result<Found, String> {
            Err("down".into())
        }
        fn symbols(&self, _: &str, _: Instant) -> Result<Vec<SymbolHit>, String> {
            Err("down".into())
        }
        fn callers(&self, _: &str, _: Instant) -> Result<Vec<CallerHit>, String> {
            Err("down".into())
        }
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    fn heads(blocks: &[Block]) -> Vec<String> {
        blocks
            .iter()
            .map(|block| block.text().lines().next().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn locate_should_show_the_constant_that_sets_a_value_with_its_doc_comment() {
        let repo = Repo::default()
            .with(
                "src/sessions.py",
                &[
                    "# Redirects followed before giving up.",
                    "DEFAULT_REDIRECT_LIMIT = 30",
                    "",
                    "class Session:",
                    "    def __init__(self):",
                    "        self.max_redirects = DEFAULT_REDIRECT_LIMIT",
                ],
            )
            .with("tests/test_sessions.py", &["DEFAULT_REDIRECT_LIMIT = 5"])
            .with("README.md", &["DEFAULT_REDIRECT_LIMIT = 3"]);
        let blocks = locate(
            "requests follows up to thirty redirects by default. can you lower that to twenty",
            &repo,
            None,
            soon(),
        );
        let text: String = blocks
            .iter()
            .map(Block::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("src/sessions.py:1-2 — DEFAULT_REDIRECT_LIMIT (constant"),
            "{text}"
        );
        assert!(
            text.contains("1| # Redirects followed before giving up."),
            "{text}"
        );
        assert!(text.contains("2| DEFAULT_REDIRECT_LIMIT = 30"), "{text}");
        assert!(
            text.contains("self.max_redirects = DEFAULT_REDIRECT_LIMIT"),
            "{text}"
        );
        assert!(!text.contains("test_sessions"), "{text}");
        assert!(!text.contains("README"), "{text}");
    }

    #[test]
    fn locate_should_list_the_tests_that_carry_the_topic_with_a_run_command() {
        let repo = Repo::default()
            .with(
                "tests/test_requests.py",
                &[
                    "class TestRequests:",
                    "    def test_redirect_history_no_self_reference(self, httpbin):",
                    "        r = requests.get(httpbin('redirect', '1'))",
                    "        assert r.history",
                    "",
                    "    def test_cookies(self):",
                    "        assert True",
                    "",
                    "    def test_too_many_redirects(self, httpbin):",
                    "        with pytest.raises(TooManyRedirects):",
                    "            requests.get(httpbin('redirect', '50'))",
                ],
            )
            .with(
                "pyproject.toml",
                &["[tool.pytest.ini_options]", "addopts = \"-q\""],
            );
        let blocks = locate(
            "which tests cover redirect handling and how would I run only those",
            &repo,
            None,
            soon(),
        );
        let heads = heads(&blocks);
        assert_eq!(heads[0], "### tests matching redirect (2 found)");
        let list = blocks[0].text();
        assert!(
            list.contains("tests/test_requests.py:2 test_redirect_history_no_self_reference"),
            "{list}"
        );
        assert!(
            list.contains("tests/test_requests.py:9 test_too_many_redirects"),
            "{list}"
        );
        assert!(
            list.contains("run only these: pytest -k redirect"),
            "{list}"
        );
        assert!(!list.contains("test_cookies"), "{list}");
        assert!(
            heads
                .iter()
                .any(|head| head.contains("test_redirect_history_no_self_reference (test)")),
            "{heads:?}"
        );
        // The body is the test's lines, numbered.
        let body = blocks[1].text();
        assert!(
            body.contains("3|     r = requests.get(httpbin('redirect', '1'))"),
            "{body}"
        );
    }

    #[test]
    fn locate_should_list_the_environment_variables_of_the_repository_when_asked() {
        let repo = Repo::default()
            .with(
                "src/main.rs",
                &[
                    "let a = std::env::var(\"PIXEL_ONE\");",
                    "let b = std::env::var(\"PIXEL_TWO\");",
                    "let c = std::env::var(\"PIXEL_THREE\");",
                    "let h = std::env::var(\"HOME\");",
                ],
            )
            .with(".github/workflows/x.rs", &["env::var(\"CI_ONLY\")"]);
        let blocks = locate(
            "which environment variables change how it behaves",
            &repo,
            None,
            soon(),
        );
        let heads = heads(&blocks);
        assert_eq!(
            heads,
            ["### environment variables read (name — first read)"]
        );
        let text = blocks[0].text();
        assert!(text.contains("PIXEL_ONE — read at src/main.rs:1"), "{text}");
        assert!(!text.contains("HOME"), "{text}");
        assert!(!text.contains("CI_ONLY"), "{text}");
    }

    #[test]
    fn locate_should_find_nothing_for_a_question_about_neither_and_survive_a_failing_search() {
        let repo = Repo::default().with("a.py", &["MAX = 1"]);
        assert!(
            locate(
                "rename the helper and update its callers",
                &repo,
                None,
                soon()
            )
            .is_empty()
        );
        // A repository that cannot search leaves the pack as it was.
        assert!(locate("which tests cover redirect handling", &Down, None, soon()).is_empty());
        assert!(
            locate(
                "where is the redirect limit set by default",
                &Down,
                None,
                soon()
            )
            .is_empty()
        );
        // An expired window searches nothing it cannot finish: the fake still
        // answers, the real source refuses; the contract is only "no panic".
        let _ = locate(
            "which tests cover redirect handling",
            &repo,
            None,
            Instant::now(),
        );
    }
}
