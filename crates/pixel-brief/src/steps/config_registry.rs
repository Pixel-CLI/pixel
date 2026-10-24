// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `config-registry` (`specs/brief.md`, "Recipes"): a setting question ("where
//! do I set X", "which environment variables", "what port") is answered by a
//! registry built from the text index: key, value or fallback, where it is
//! declared, where it is read.
//!
//! It reads environment reads in JS/TS, Python, Rust, Go and Ruby, typed
//! environment declarations (`env.d.ts`), exported default constants,
//! dev-server port settings and literal `set("key", value)` calls. A key
//! ranks by the prompt's words in its name and in the comment and line that
//! declare it; the best four are shown with a source excerpt. Keys whose name
//! looks like a secret (`key`, `secret`, `token`, `password`, `config`, …) are
//! never shown with a value, and neither is a source line that mentions one.
//!
//! PIXEL reads no `.env*` file, templates included (the same credential-path
//! rule as every other read of the brief), so a value set in an environment
//! file is not quoted; Pixel's script read the committed templates. Read-only
//! and deterministic.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::region_pack::Block;
use crate::routes::QuestionKind;

use super::{Ctx, Outcome, Step};

const CODE_EXTENSIONS: &[&str] = &[
    ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".py", ".rs", ".go", ".rb",
    ".vue", ".svelte",
];
const SCANNED_EXTENSIONS: &[&str] = &[".json", ".yml", ".yaml", ".toml", ".mdx", ".md", ".d.ts"];
const EXCLUDED: &[&str] = &[
    "node_modules/",
    "target/",
    "dist/",
    "build/",
    "vendor/",
    ".lock",
    "yarn.lock",
    "package-lock.json",
    ".min.js",
    ".map",
    ".snap",
    ".svg",
    ".json5",
];
/// Rows longer than this many bytes are not read (ripgrep's `--max-columns`).
const MAX_COLUMNS: usize = 300;
const SCAN_ROWS: usize = 6_000;
const MAX_KEYS: usize = 4;
const OUT_CAP: usize = 9_000;
const LINE_CHARS: usize = 150;
const MAX_CANDIDATES: usize = 4;
const SETTER_BLOCKS: usize = 4;

const STOP: &[&str] = &[
    "about", "above", "after", "again", "against", "also", "another", "because", "been", "before",
    "being", "below", "between", "both", "could", "does", "doing", "down", "during", "each",
    "from", "further", "have", "having", "here", "into", "just", "more", "most", "much", "once",
    "only", "other", "over", "same", "should", "some", "such", "than", "that", "their", "them",
    "then", "there", "these", "they", "this", "those", "through", "under", "until", "very", "want",
    "what", "when", "where", "which", "while", "will", "with", "would", "your", "locally", "local",
    "set", "run", "use", "app", "own", "the", "and", "for", "how", "can", "any", "not", "are",
    "you", "has", "get", "was", "its", "out",
];
const WEAK_WORDS: &[&str] = &[
    "dev", "server", "app", "config", "setting", "file", "default", "value", "url", "locall",
    "local", "where",
];

static GATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(config\w*|settings?|env(ironment)?|variables?|flags?|options?|defaults?|ports?|overrid\w+|max(?:imum)?|min(?:imum)?|bounds?)\b",
    )
    .expect("the config-question pattern is a literal")
});
static SECRETISH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)key|secret|token|password|passwd|credential|private|signature|config")
        .expect("the secret pattern is a literal")
});
static JS_READ: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:process\.env|import\.meta\.env)(?:\.|\[\s*["'])([A-Za-z_][A-Za-z0-9_]*)"#)
        .expect("the js read pattern is a literal")
});
static PREFIXED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b((?:VITE|REACT_APP|NEXT_PUBLIC|NUXT|EXPO_PUBLIC|PUBLIC)_[A-Z0-9_]{2,})\b")
        .expect("the prefixed pattern is a literal")
});
static PY_READ: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"os\.(?:environ(?:\.get)?|getenv)\s*[\(\[]\s*["']([A-Za-z_][A-Za-z0-9_]*)["']"#)
        .expect("the python read pattern is a literal")
});
static RS_READ: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"env::var(?:_os)?\(\s*"([A-Za-z_][A-Za-z0-9_]*)"|\benv\s*=\s*"([A-Z_][A-Z0-9_]*)"|\benv!\(\s*"([A-Z_][A-Z0-9_]*)""#,
    )
    .expect("the rust read pattern is a literal")
});
static GO_READ: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"os\.(?:Getenv|LookupEnv)\(\s*"([A-Za-z_][A-Za-z0-9_]*)""#)
        .expect("the go read pattern is a literal")
});
static RB_READ: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\bENV(?:\[|\.fetch\()\s*["']([A-Za-z_][A-Za-z0-9_]*)["']"#)
        .expect("the ruby read pattern is a literal")
});
static ENV_DECL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*([A-Z][A-Z0-9_]{2,})\??\s*:\s*[A-Za-z<>\[\]| ]+;")
        .expect("the env declaration pattern is a literal")
});
static CONST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:pub(?:\([a-z]+\))?\s+)?(?:export\s+)?(?:const|static)\s+([A-Z][A-Z0-9_]{2,})\b[^=]*=\s*(.+?);?\s*$",
    )
    .expect("the constant pattern is a literal")
});
static PYCONST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([A-Z][A-Z0-9_]{2,})\s*(?::[^=]+)?=\s*(.+)$")
        .expect("the python constant pattern is a literal")
});
static PORT_CFG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:\bport\s*[:=]\s*|--port[= ]|\bPORT=)([^,;}\n]+)")
        .expect("the port pattern is a literal")
});
static DEFAULT_AFTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:\|\||\?\?)\s*([^;,)\n]+)|,\s*(["'0-9][^)]*)\)"#)
        .expect("the default pattern is a literal")
});
static SETTER_CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"\b[\w.]+\.(?:set|setDefault|set_default)\(\s*(?:'([\w .-]+)'|"([\w .-]+)")\s*,\s*((?:'[^'\n]*'|"[^"\n]*")|true|false|null|-?\d+(?:\.\d+)?)\s*\)"#,
    )
    .expect("the setter pattern is a literal")
});
static LITERAL_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^["'\d\-\[]|^(true|false|Duration|Some|None)\b|^\d"#)
        .expect("the literal pattern is a literal")
});
static TRAILING_COMMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*//.*$|\s*#.*$|;$").expect("the comment pattern is a literal"));
static TEST_DIR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(^|/)(test|tests|__tests__|fixtures?|node_modules)/|\.(test|spec)\.")
        .expect("the test directory pattern is a literal")
});
static PORT_FILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\.config\.|package\.json|docker-compose|\.ya?ml$|\.toml$)")
        .expect("the port file pattern is a literal")
});
static PORT_EXCLUDED_DIR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(^|/)(dev-docs|examples?|docs?|website|e2e|fixtures?)/")
        .expect("the excluded directory pattern is a literal")
});
static SCRIPT_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""(build|preview|serve|test|docs?)[^"]*"\s*:"#)
        .expect("the script key pattern is a literal")
});
static COMMENT_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(#|//|\*|/\*)").expect("the comment pattern is a literal"));
static WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[a-z]{2,}").expect("the word pattern is a literal"));
static LOWER_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[a-z]{3,}").expect("the word pattern is a literal"));
static BOUND_QUERY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(max(?:imum)?|min(?:imum)?|bounds?)\b")
        .expect("the bound pattern is a literal")
});
static EXISTING_CONFIG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(config\w*|settings?|env(ironment)?(\s+var\w*)?|variables?|flags?|options?|defaults?|ports?|overrid\w+|where do i set|set (that|this|it))\b",
    )
    .expect("the existing-config pattern is a literal")
});
static MAX_QUERY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bmax(?:imum)?\b").expect("the max pattern is a literal"));
static MIN_QUERY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bmin(?:imum)?\b").expect("the min pattern is a literal"));
static CAMEL_PART: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Z]+(?:[^a-z]|$)|[A-Z]?[a-z]+|\d+").expect("the camel pattern is a literal")
});

/// The patterns whose lines may define or read a config key; one alternation.
const SCAN_PATTERN: &str = concat!(
    r#"process\.env|import\.meta\.env"#,
    r#"|(?:VITE|REACT_APP|NEXT_PUBLIC|NUXT|EXPO_PUBLIC)_[A-Z0-9_]{2,}"#,
    r#"|os\.(?:environ|getenv)"#,
    r#"|env::var|\benv\s*=\s*"|env!\("#,
    r#"|os\.(?:Getenv|LookupEnv)"#,
    r#"|\bENV(?:\[|\.fetch)"#,
    r#"|^\s*[A-Z][A-Z0-9_]{2,}\??\s*:\s*[A-Za-z<>\[\]| ]+;"#,
    r#"|^\s*(?:pub(?:\([a-z]+\))?\s+)?(?:export\s+)?(?:const|static)\s+[A-Z][A-Z0-9_]{2,}\b"#,
    r#"|^[A-Z][A-Z0-9_]{2,}\s*(?::[^=]+)?="#,
    r#"|\bport\s*[:=]|--port|\bPORT="#,
    r#"|\b[\w.]+\.(?:set|setDefault|set_default)\(\s*['"][\w .-]+['"]\s*,"#,
);

/// One row of the scan: `(path, line, text)`.
type Row = (String, usize, String);

#[derive(Default)]
struct Entry {
    /// `(path, line, text, default)`.
    defs: Vec<(String, usize, String, String)>,
    reads: Vec<(String, usize, String, String)>,
}

/// `words` truncated to six characters with a plural `s` cut, so `byte`
/// matches `bytes` and `file` matches `files`.
fn stems<'a>(words: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
    words
        .into_iter()
        .map(|word| {
            let mut stem: String = word.chars().take(6).collect();
            if stem.chars().count() >= 4 && stem.ends_with('s') && !stem.ends_with("ss") {
                stem.pop();
            }
            stem
        })
        .collect()
}

fn weak() -> BTreeSet<String> {
    let mut weak = stems(WEAK_WORDS.iter().copied());
    weak.extend(WEAK_WORDS.iter().map(ToString::to_string));
    weak.extend(
        WEAK_WORDS
            .iter()
            .map(|word| word.chars().take(6).collect::<String>()),
    );
    weak
}

fn bound_words() -> BTreeSet<String> {
    stems([
        "max", "maximum", "min", "minimum", "bound", "bounds", "limit", "limited", "cap", "caps",
    ])
}

/// The prompt's content words, in order, once each.
fn words_of(prompt: &str) -> Vec<String> {
    let lower = prompt.to_lowercase();
    let mut seen = BTreeSet::new();
    WORD.find_iter(&lower)
        .map(|found| found.as_str().to_string())
        .filter(|word| seen.insert(word.clone()))
        .filter(|word| (!STOP.contains(&word.as_str()) && word.len() >= 3) || word == "ws")
        .collect()
}

/// The lowercase tokens of a key such as `MAX_BODY_SIZE` or `maxBodySize`.
fn key_tokens(key: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in key
        .split(['_', '-', ' ', '.'])
        .filter(|part| !part.is_empty())
    {
        let upper = part.chars().all(|ch| !ch.is_lowercase());
        let lower = part.chars().all(|ch| !ch.is_uppercase());
        if upper || lower {
            out.push(part.to_lowercase());
        } else {
            out.extend(CAMEL_PART.find_iter(part).map(|found| {
                found
                    .as_str()
                    .trim_end_matches(|ch: char| !ch.is_alphanumeric())
                    .to_lowercase()
            }));
        }
    }
    out.retain(|token| !token.is_empty());
    out
}

/// `mask(key, value)`: a value of a key whose name looks like a secret is
/// never printed.
fn mask(key: &str, value: &str) -> String {
    if !value.is_empty() && SECRETISH.is_match(key) {
        "<masked>".to_string()
    } else {
        value.to_string()
    }
}

fn is_config_path(path: &str) -> bool {
    if EXCLUDED.iter().any(|part| {
        if part.ends_with('/') {
            path.starts_with(part) || path.contains(&format!("/{part}"))
        } else {
            path.ends_with(part)
        }
    }) {
        return false;
    }
    (CODE_EXTENSIONS
        .iter()
        .chain(SCANNED_EXTENSIONS)
        .any(|ext| path.ends_with(ext)))
        && !TEST_DIR.is_match(path)
}

pub(super) fn run(ctx: &Ctx, out: &mut Outcome) {
    if !ctx.wants(
        Step::ConfigRegistry,
        &[
            QuestionKind::Lookup,
            QuestionKind::Flow,
            QuestionKind::Tests,
            QuestionKind::Bugfix,
            QuestionKind::Rationale,
            QuestionKind::Overview,
            QuestionKind::History,
            QuestionKind::Review,
        ],
    ) || !GATE.is_match(ctx.typed)
    {
        return;
    }
    let _timer = ctx.timer(Step::ConfigRegistry);
    let Ok(found) = ctx.evidence.grep(SCAN_PATTERN, SCAN_ROWS, ctx.deadline) else {
        return;
    };
    let mut rows: Vec<Row> = found
        .hits
        .into_iter()
        .filter_map(|hit| Some((hit.path, usize::try_from(hit.line).ok()?, hit.text?)))
        .filter(|(path, _, text)| is_config_path(path) && text.len() <= MAX_COLUMNS)
        .collect();
    rows.sort();
    let registry = registry(&rows);
    let words = words_of(ctx.typed);
    let keys = if BOUND_QUERY.is_match(ctx.typed) && !EXISTING_CONFIG.is_match(ctx.typed) {
        rank_bound(ctx, &registry)
    } else {
        rank(
            ctx,
            &registry,
            ctx.typed,
            &registry.keys().map(String::as_str).collect::<Vec<_>>(),
        )
    };
    let setters = setter_blocks(ctx, &rows);
    if keys.is_empty() && setters.is_empty() {
        return;
    }
    let mut blocks: Vec<Block> = setters;
    if !keys.is_empty() {
        blocks.extend(registry_blocks(ctx, &registry, &keys, &words));
    }
    let mut used = 0;
    for block in blocks {
        let cost = block.text().len() + 1;
        if used + cost > OUT_CAP {
            break;
        }
        used += cost;
        out.lead.push(block);
    }
}

fn registry(rows: &[Row]) -> BTreeMap<String, Entry> {
    let mut registry: BTreeMap<String, Entry> = BTreeMap::new();
    for (path, n, text) in rows {
        let code = CODE_EXTENSIONS.iter().any(|ext| path.ends_with(ext));
        if code && (path.ends_with("vite-env.d.ts") || path.ends_with("env.d.ts")) {
            if let Some(caps) = ENV_DECL.captures(text) {
                registry.entry(caps[1].to_string()).or_default().defs.push((
                    path.clone(),
                    *n,
                    text.trim().to_string(),
                    String::new(),
                ));
            }
            continue;
        }
        if code {
            let mut keys: BTreeSet<String> = BTreeSet::new();
            for regex in [&JS_READ, &PY_READ, &GO_READ, &RB_READ, &PREFIXED] {
                keys.extend(regex.captures_iter(text).map(|caps| caps[1].to_string()));
            }
            for caps in RS_READ.captures_iter(text) {
                keys.extend(
                    (1..=3)
                        .filter_map(|group| caps.get(group))
                        .map(|m| m.as_str().to_string()),
                );
            }
            let default = DEFAULT_AFTER.captures(text).and_then(|caps| {
                caps.get(1)
                    .or_else(|| caps.get(2))
                    .map(|m| m.as_str().trim().to_string())
            });
            for key in &keys {
                registry.entry(key.clone()).or_default().reads.push((
                    path.clone(),
                    *n,
                    text.trim().to_string(),
                    default.clone().unwrap_or_default(),
                ));
            }
            let constant = CONST.captures(text).or_else(|| {
                path.ends_with(".py")
                    .then(|| PYCONST.captures(text))
                    .flatten()
            });
            if let Some(caps) = constant
                && keys.is_empty()
            {
                let value = caps.get(2).map_or("", |m| m.as_str()).trim().to_string();
                let value = TRAILING_COMMENT.replace(&value, "").trim().to_string();
                if LITERAL_VALUE.is_match(&value) {
                    registry.entry(caps[1].to_string()).or_default().defs.push((
                        path.clone(),
                        *n,
                        text.trim().to_string(),
                        value.chars().take(60).collect(),
                    ));
                }
            }
        }
        if PORT_FILE.is_match(path)
            && let Some(caps) = PORT_CFG.captures(text)
            && caps[1].chars().any(|ch| ch.is_ascii_digit())
            && !PREFIXED.is_match(text)
            && !SCRIPT_KEY.is_match(text)
            && !PORT_EXCLUDED_DIR.is_match(path)
        {
            let value = caps[1].trim().trim_matches(['"', ',']).to_string();
            registry
                .entry("dev-server port".to_string())
                .or_default()
                .defs
                .push((path.clone(), *n, text.trim().to_string(), value));
        }
    }
    registry
}

/// The comment lines (up to three) just above line `n` of `path`.
fn comment_above(ctx: &Ctx, path: &str, n: usize) -> String {
    let Some(text) = ctx.read(path) else {
        return String::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for i in (n.saturating_sub(4)..n.saturating_sub(1)).rev() {
        match lines.get(i) {
            Some(line) if COMMENT_LINE.is_match(line) => out.push(*line),
            _ => break,
        }
    }
    out.join(" ")
}

/// The best keys for the prompt: a strong word of the prompt in the key's name
/// or in its declaration's comment and line.
fn rank(
    ctx: &Ctx,
    registry: &BTreeMap<String, Entry>,
    prompt: &str,
    candidates: &[&str],
) -> Vec<String> {
    let st = stems(words_of(prompt).iter().map(String::as_str));
    let weak = weak();
    let strong: BTreeSet<String> = st.difference(&weak).cloned().collect();
    let mut scored: Vec<(i64, usize, String)> = Vec::new();
    for key in candidates {
        let entry = &registry[*key];
        let tokens = if *key == "dev-server port" {
            vec!["dev".to_string(), "server".to_string(), "port".to_string()]
        } else {
            key_tokens(key)
        };
        let ktoks = stems(tokens.iter().map(String::as_str));
        let mut score = 3 * strong.intersection(&ktoks).count() as i64
            + st.intersection(&ktoks)
                .filter(|stem| !strong.contains(*stem))
                .count() as i64;
        let mut hay = String::new();
        for def in entry.defs.iter().take(2) {
            hay.push(' ');
            hay.push_str(&comment_above(ctx, &def.0, def.1));
            hay.push(' ');
            hay.push_str(&def.2);
        }
        let hay_lower = hay.to_lowercase();
        let hs = stems(LOWER_WORD.find_iter(&hay_lower).map(|found| found.as_str()));
        score += 2 * strong
            .intersection(&hs)
            .filter(|stem| !ktoks.contains(*stem))
            .count() as i64;
        if *key == "dev-server port" && st.contains("port") {
            score += 3;
        }
        if !entry.reads.is_empty()
            && entry
                .reads
                .iter()
                .any(|read| !read.0.ends_with(".rs") || read.2.contains("env"))
        {
            score += 2;
        }
        if strong.intersection(&ktoks).next().is_some() || strong.intersection(&hs).next().is_some()
        {
            scored.push((
                score,
                entry.defs.len() + entry.reads.len(),
                (*key).to_string(),
            ));
        }
    }
    scored.sort_by(|a, b| b.cmp(a));
    scored
        .into_iter()
        .take(MAX_KEYS)
        .map(|(_, _, key)| key)
        .collect()
}

/// For a bound-only question ("what is the max X"): only keys with a bound
/// declaration, and a subject the prompt names.
fn rank_bound(ctx: &Ctx, registry: &BTreeMap<String, Entry>) -> Vec<String> {
    let prompt_words = stems(words_of(ctx.typed).iter().map(String::as_str));
    let mut drop = weak();
    drop.extend(bound_words());
    drop.extend(stems([
        "defined",
        "definition",
        "level",
        "size",
        "value",
        "number",
        "amount",
    ]));
    let subject: BTreeSet<String> = prompt_words.difference(&drop).cloned().collect();
    if subject.is_empty() {
        return Vec::new();
    }
    let (want_max, want_min) = (MAX_QUERY.is_match(ctx.typed), MIN_QUERY.is_match(ctx.typed));
    let max_words = stems(["max", "maximum", "cap", "caps"]);
    let min_words = stems(["min", "minimum"]);
    let bound = bound_words();
    let mut eligible: Vec<&str> = Vec::new();
    for (key, entry) in registry {
        let key_words = stems(key_tokens(key).iter().map(String::as_str));
        let declarations: Vec<&str> = entry
            .defs
            .iter()
            .take(2)
            .map(|def| def.2.as_str())
            .collect();
        let comments: Vec<String> = entry
            .defs
            .iter()
            .take(2)
            .map(|def| comment_above(ctx, &def.0, def.1))
            .collect();
        let text = format!("{} {}", declarations.join(" "), comments.join(" ")).to_lowercase();
        let declared = stems(regex_words(&text).iter().map(String::as_str));
        let all: BTreeSet<String> = key_words.union(&declared).cloned().collect();
        let has_bound = all.intersection(&bound).next().is_some();
        let has_max = all.intersection(&max_words).next().is_some();
        let has_min = all.intersection(&min_words).next().is_some();
        let direction = !(want_max || want_min) || (want_max && has_max) || (want_min && has_min);
        if has_bound && direction && subject.intersection(&all).next().is_some() {
            eligible.push(key);
        }
    }
    rank(ctx, registry, ctx.typed, &eligible)
}

fn regex_words(text: &str) -> Vec<String> {
    static ALPHA: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[a-z]+").expect("the alpha pattern is a literal"));
    ALPHA
        .find_iter(text)
        .map(|found| found.as_str().to_string())
        .collect()
}

/// `path:lo-hi` and its lines, `before` and `after` the line `n`, each cut to
/// [`LINE_CHARS`]; a line that names a secret, or any line of a secret key,
/// is masked.
fn evidence_block(
    ctx: &Ctx,
    key: &str,
    path: &str,
    n: usize,
    before: usize,
    after: usize,
) -> Option<Block> {
    let text = ctx.read(path)?;
    let lines: Vec<&str> = text.lines().collect();
    let (lo, hi) = (
        n.saturating_sub(before).max(1),
        (n + after).min(lines.len()),
    );
    if lo > hi {
        return None;
    }
    let body = (lo..=hi)
        .map(|i| {
            let line: String = lines[i - 1].chars().take(LINE_CHARS).collect();
            let numbered = format!("{i}| {line}");
            if SECRETISH.is_match(key) || SECRETISH.is_match(&numbered) {
                format!("{i}| <masked>")
            } else {
                numbered
            }
        })
        .collect();
    Some(Block::new(format!("{path}:{lo}-{hi}"), body))
}

fn registry_blocks(
    ctx: &Ctx,
    registry: &BTreeMap<String, Entry>,
    keys: &[String],
    words: &[String],
) -> Vec<Block> {
    let mut rows = Vec::new();
    let mut blocks: Vec<Block> = Vec::new();
    let mut seen: BTreeSet<(String, usize)> = BTreeSet::new();
    for key in keys {
        let entry = &registry[key];
        let mut defs = entry.defs.clone();
        defs.sort_by(|a, b| a.0.cmp(&b.0));
        if defs.len() > 1 {
            rows.push(format!(
                "- {key} | {} distinct declaration candidates; lexical binding and override precedence unknown",
                defs.len()
            ));
            for (path, n, _, value) in defs.iter().take(MAX_CANDIDATES) {
                let value = mask(key, value);
                rows.push(format!(
                    "  - {key} @ {path}:{n} | candidate value {}",
                    if value.is_empty() { "unknown" } else { &value }
                ));
                blocks.extend(evidence_block(ctx, key, path, *n, 2, 2));
            }
            if defs.len() > MAX_CANDIDATES {
                rows.push(format!(
                    "  - {} additional candidates omitted; no unique default established",
                    defs.len() - MAX_CANDIDATES
                ));
            }
            for read in entry.reads.iter().take(2) {
                blocks.extend(evidence_block(ctx, key, &read.0, read.1, 1, 2));
            }
            continue;
        }
        let fallback = mask(
            key,
            entry
                .reads
                .iter()
                .find(|read| !read.3.is_empty())
                .map(|read| read.3.as_str())
                .or_else(|| {
                    defs.iter()
                        .find(|def| !def.3.is_empty())
                        .map(|def| def.3.as_str())
                })
                .unwrap_or(""),
        );
        let is_env = !entry.reads.is_empty() || defs.iter().any(|def| def.0.ends_with("env.d.ts"));
        let value = if fallback.is_empty() {
            "no value in indexed files".to_string()
        } else if is_env {
            format!("code fallback {fallback}; an env file may override")
        } else {
            format!("constant default {fallback}")
        };
        let reads: Vec<String> = entry
            .reads
            .iter()
            .take(3)
            .map(|read| format!("{}:{}", read.0, read.1))
            .collect();
        let declared = defs.first().map_or("not declared".to_string(), |def| {
            format!("{}:{}", def.0, def.1)
        });
        let more_reads = if entry.reads.len() > 3 {
            format!(" (+{} more)", entry.reads.len() - 3)
        } else {
            String::new()
        };
        rows.push(format!(
            "- {key} | {value} | {declared} | read at {}{more_reads}",
            if reads.is_empty() {
                "no code read".to_string()
            } else {
                reads.join(", ")
            }
        ));
        if let Some(def) = defs.first()
            && seen.insert((def.0.clone(), def.1))
        {
            let above = comment_above(ctx, &def.0, def.1);
            let span = above.matches("//").count() + above.matches('#').count();
            let (before, after) = if key == "dev-server port" {
                (1, 1)
            } else {
                (span.min(3), 0)
            };
            blocks.extend(evidence_block(ctx, key, &def.0, def.1, before, after));
        }
        for read in entry.reads.iter().take(2) {
            if seen.insert((read.0.clone(), read.1)) {
                blocks.extend(evidence_block(ctx, key, &read.0, read.1, 1, 2));
            }
        }
    }
    let head = format!(
        "config registry — keys matching {} (key | value or fallback | declared | read at)",
        words.iter().take(6).cloned().collect::<Vec<_>>().join(", ")
    );
    let mut all = vec![Block::new(head, rows)];
    all.extend(blocks);
    all
}

/// Prompt-matched literal setting calls (`settings.set("limit", 10)`), quoted
/// as source; no precedence or effect is inferred.
fn setter_blocks(ctx: &Ctx, rows: &[Row]) -> Vec<Block> {
    let prompt_words = stems(words_of(ctx.typed).iter().map(String::as_str));
    if prompt_words.is_empty() {
        return Vec::new();
    }
    let mut candidates: Vec<(std::cmp::Reverse<usize>, &str, usize, String)> = Vec::new();
    for (path, n, text) in rows {
        if ![".js", ".jsx", ".ts", ".tsx", ".mjs", ".cjs", ".mts", ".cts"]
            .iter()
            .any(|ext| path.ends_with(ext))
            || ["//", "/*", "*"]
                .iter()
                .any(|prefix| text.trim_start().starts_with(prefix))
        {
            continue;
        }
        let Some(caps) = SETTER_CALL.captures(text) else {
            continue;
        };
        let key = caps
            .get(1)
            .or_else(|| caps.get(2))
            .map_or("", |m| m.as_str())
            .trim()
            .to_string();
        let lower = key.to_lowercase();
        let key_stems = stems(LOWER_WORD_ALL.find_iter(&lower).map(|found| found.as_str()));
        let overlap = prompt_words.intersection(&key_stems).count();
        if overlap > 0 {
            candidates.push((std::cmp::Reverse(overlap), path, *n, key));
        }
    }
    candidates.sort();
    let mut blocks = Vec::new();
    for (_, path, n, key) in candidates.into_iter().take(SETTER_BLOCKS) {
        let Some(text) = ctx.read(path) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        let (lo, hi) = (n.saturating_sub(3).max(1), (n + 3).min(lines.len()));
        if lo > hi {
            continue;
        }
        let body = (lo..=hi)
            .map(|i| {
                let line: String = lines[i - 1].chars().take(LINE_CHARS).collect();
                if SECRETISH.is_match(&key) || SECRETISH.is_match(&line) {
                    format!("{i}| <masked>")
                } else {
                    format!("{i}| {line}")
                }
            })
            .collect();
        blocks.push(Block::new(
            format!("{path}:{lo}-{hi} — literal setting call candidate; inspect surrounding control flow"),
            body,
        ));
    }
    blocks
}

static LOWER_WORD_ALL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[a-z]+").expect("the word pattern is a literal"));

#[cfg(test)]
mod tests;
