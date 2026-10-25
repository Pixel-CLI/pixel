// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `{key: declarations, reads}` built from the scanned lines.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;

use super::super::text::{head, last_segment};
use super::scan::{CODE_EXT, ENV_LINE, Row};

/// Where a key is declared and the value written there.
pub(super) struct Def {
    pub(super) path: String,
    pub(super) line: usize,
    pub(super) text: String,
    pub(super) value: String,
}

/// Where a key is read and the fallback written beside the read.
pub(super) struct Read {
    pub(super) path: String,
    pub(super) line: usize,
    pub(super) text: String,
    pub(super) default: String,
}

#[derive(Default)]
pub(super) struct Entry {
    pub(super) defs: Vec<Def>,
    pub(super) reads: Vec<Read>,
}

pub(super) type Registry = BTreeMap<String, Entry>;

macro_rules! pattern {
    ($name:ident, $text:expr) => {
        static $name: LazyLock<Regex> =
            LazyLock::new(|| Regex::new($text).expect("a registry pattern is a literal"));
    };
}

pattern!(JS_READ, r#"(?:process\.env|import\.meta\.env)(?:\.|\[\s*["'])([A-Za-z_][A-Za-z0-9_]*)"#);
pattern!(
    PREFIXED,
    r"\b((?:VITE|REACT_APP|NEXT_PUBLIC|NUXT|EXPO_PUBLIC|PUBLIC)_[A-Z0-9_]{2,})\b"
);
pattern!(
    PY_READ,
    r#"os\.(?:environ(?:\.get)?|getenv)\s*[\(\[]\s*["']([A-Za-z_][A-Za-z0-9_]*)["']"#
);
pattern!(
    RS_READ,
    r#"env::var(?:_os)?\(\s*"([A-Za-z_][A-Za-z0-9_]*)"|\benv\s*=\s*"([A-Z_][A-Z0-9_]*)"|\benv!\(\s*"([A-Z_][A-Z0-9_]*)""#
);
pattern!(GO_READ, r#"os\.(?:Getenv|LookupEnv)\(\s*"([A-Za-z_][A-Za-z0-9_]*)""#);
pattern!(RB_READ, r#"\bENV(?:\[|\.fetch\()\s*["']([A-Za-z_][A-Za-z0-9_]*)["']"#);
pattern!(ENV_DECL, r"^\s*([A-Z][A-Z0-9_]{2,})\??\s*:\s*[A-Za-z<>\[\]| ]+;");
pattern!(
    CONST,
    r"^\s*(?:pub(?:\([a-z]+\))?\s+)?(?:export\s+)?(?:const|static)\s+([A-Z][A-Z0-9_]{2,})\b[^=]*=\s*(.+?);?\s*$"
);
pattern!(PYCONST, r"^([A-Z][A-Z0-9_]{2,})\s*(?::[^=]+)?=\s*(.+)$");
pattern!(
    PORT_CFG,
    r"(?i)(?:\bport\s*[:=]\s*|--port[= ]|\bPORT=)([^,;}\n]+)"
);
pattern!(DEFAULT_AFTER, r#"(?:\|\||\?\?)\s*([^;,)\n]+)|,\s*(["'0-9][^)]*)\)"#);
pattern!(ENV_D_TS, r"(vite-env|env)\.d\.ts$");
pattern!(VALUE_COMMENT, r"\s*//.*$|\s*#.*$|;$");
pattern!(
    VALUE_START,
    r#"^["'\d\-\[]|^(?:true|false|Duration|Some|None)\b|^\d"#
);
pattern!(PORT_PATH, r"(\.config\.|package\.json|docker-compose|\.ya?ml$|\.toml$)");
pattern!(PORT_SCRIPT, r#""(build|preview|serve|test|docs?)[^"]*"\s*:"#);
pattern!(DIGIT, r"\d");
pattern!(PORT_DOC_PATH, r"(^|/)(dev-docs|examples?|docs?|website|e2e|fixtures?)/");

fn is_env_file(base: &str) -> bool {
    base.starts_with(".env") || base.ends_with(".env")
}


/// Every capture group 1 of `pattern` over `text`.
fn keys_of<'a>(pattern: &Regex, text: &'a str) -> impl Iterator<Item = &'a str> {
    pattern
        .captures_iter(text)
        .filter_map(|caps| caps.get(1))
        .map(|found| found.as_str())
        .collect::<Vec<_>>()
        .into_iter()
}

pub(super) fn registry(rows: &[Row]) -> Registry {
    let mut reg = Registry::new();
    for row in rows {
        let Row { path, line, text } = row;
        let base = last_segment(path);
        let stripped = text.trim();
        if is_env_file(base) {
            if let Some(caps) = ENV_LINE.captures(text) {
                let value = caps[2].trim().trim_matches(['\'', '"']);
                entry(&mut reg, &caps[1]).defs.push(Def {
                    path: path.clone(),
                    line: *line,
                    text: stripped.to_owned(),
                    value: value.to_owned(),
                });
            }
            continue;
        }
        let code = CODE_EXT.iter().any(|ext| path.ends_with(ext));
        if code && ENV_D_TS.is_match(path) {
            if let Some(caps) = ENV_DECL.captures(text) {
                entry(&mut reg, &caps[1]).defs.push(Def {
                    path: path.clone(),
                    line: *line,
                    text: stripped.to_owned(),
                    value: String::new(),
                });
            }
            continue;
        }
        if code {
            let mut keys: BTreeSet<&str> = BTreeSet::new();
            for pattern in [&JS_READ, &PY_READ, &GO_READ, &RB_READ, &PREFIXED] {
                keys.extend(keys_of(pattern, text));
            }
            for caps in RS_READ.captures_iter(text) {
                keys.extend((1..=3).filter_map(|group| caps.get(group)).map(|found| found.as_str()));
            }
            let default = DEFAULT_AFTER.captures(text).map(|caps| {
                caps.get(1)
                    .or_else(|| caps.get(2))
                    .map_or("", |found| found.as_str())
                    .trim()
                    .to_owned()
            });
            for key in &keys {
                entry(&mut reg, key).reads.push(Read {
                    path: path.clone(),
                    line: *line,
                    text: stripped.to_owned(),
                    default: default.clone().unwrap_or_default(),
                });
            }
            let constant = CONST
                .captures(text)
                .or_else(|| path.ends_with(".py").then(|| PYCONST.captures(text)).flatten());
            if let Some(caps) = constant.filter(|_| keys.is_empty()) {
                let value = caps[2].trim();
                let value = VALUE_COMMENT.replace_all(value, "");
                let value = value.trim();
                if VALUE_START.is_match(value) {
                    entry(&mut reg, &caps[1]).defs.push(Def {
                        path: path.clone(),
                        line: *line,
                        text: stripped.to_owned(),
                        value: head(value, 60).to_owned(),
                    });
                }
            }
        }
        if PORT_PATH.is_match(path)
            && let Some(caps) = PORT_CFG.captures(text)
        {
            let value = &caps[1];
            if DIGIT.is_match(value)
                && !PREFIXED.is_match(text)
                && !PORT_SCRIPT.is_match(text)
                && !PORT_DOC_PATH.is_match(path)
            {
                entry(&mut reg, "dev-server port").defs.push(Def {
                    path: path.clone(),
                    line: *line,
                    text: stripped.to_owned(),
                    value: value.trim().trim_matches(['"', ',']).to_owned(),
                });
            }
        }
    }
    reg
}

fn entry<'a>(reg: &'a mut Registry, key: &str) -> &'a mut Entry {
    reg.entry(key.to_owned()).or_default()
}
