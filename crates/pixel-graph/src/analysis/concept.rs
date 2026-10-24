// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `find-code`: from words a person (or a log) used to the lines that carry
//! them, ranked by what kind of thing the line is.
//!
//! An agent is asked about "the Add to cart button", "a 503", "the retry limit
//! setting": text that lives in a string literal, a route, an environment
//! variable name or a config key, not in a symbol name. The search is the text
//! index's (the caller runs [`pattern`] through it); this module reads each
//! hit line, says what it is ([`ConceptKind`]), ranks the hits and names the
//! symbol that owns the line. There is no stored concept table: a rebuilt
//! index cannot disagree with it, and the answer is as fresh as the text.

use std::sync::LazyLock;

use regex::Regex;

use crate::store::{Store, StoreError};

/// What a matching line is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConceptKind {
    /// A route definition: `get '/orders'`, `app.post("/pay")`.
    Route,
    /// The name of an environment variable the code reads.
    EnvRead,
    /// Text a user sees: markup text or an attribute string in a UI file.
    UiText,
    /// A string literal.
    Text,
    /// A key or value of a JSON, YAML or TOML file.
    ConfigKey,
    /// Code that is none of the above.
    Code,
    /// A comment.
    Comment,
}

impl ConceptKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Route => "route",
            Self::EnvRead => "env_read",
            Self::UiText => "ui_text",
            Self::Text => "string",
            Self::ConfigKey => "config_key",
            Self::Code => "code",
            Self::Comment => "comment",
        }
    }

    /// How much a hit of this kind says about where a concept lives.
    fn weight(self) -> f64 {
        match self {
            Self::Route | Self::EnvRead => 1.2,
            Self::UiText => 1.1,
            Self::Text | Self::ConfigKey => 1.0,
            Self::Code => 0.5,
            Self::Comment => 0.3,
        }
    }
}

/// One ranked line.
#[derive(Clone, Debug, PartialEq)]
pub struct Lead {
    pub path: String,
    pub line: u32,
    pub kind: ConceptKind,
    /// The line, trimmed and cut.
    pub text: String,
    pub score: f64,
    /// The smallest symbol that holds the line, when the graph has one.
    pub owner: Option<String>,
}

/// The words of `phrase`: alphanumeric runs of two characters or more,
/// lower case, in order.
pub fn words(phrase: &str) -> Vec<String> {
    phrase
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| word.chars().count() >= 2)
        .map(str::to_lowercase)
        .collect()
}

/// A regular expression matching the words in order, case-insensitively, with
/// anything that is not a letter or a digit between them (`add to cart`
/// matches `Add-to-cart`, `add_to_cart` and `"Add to cart"`).
pub fn pattern(words: &[String]) -> Option<String> {
    if words.is_empty() {
        return None;
    }
    let parts: Vec<String> = words.iter().map(|word| regex::escape(word)).collect();
    Some(format!("(?i){}", parts.join("[^[:alnum:]]+")))
}

static ROUTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b(?:get|post|put|patch|delete|head|options|route|routes|resources?|mount|match|redirect)\b[^\n]{0,60}['"`]/|\b(?:app|router|api|server)\.(?:get|post|put|patch|delete|use|route)\s*\(|@(?:get|post|put|patch|delete)(?:mapping)?\s*\("#,
    )
    .expect("the route pattern is a literal")
});
static ENV_READ: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"env::var(?:_os)?\s*\(|env!\s*\(|getenv\s*\(|process\.env\b|\bENV\s*(?:\[|\.fetch)|os\.environ|std::env\b|Deno\.env\.get|System\.getenv"#,
    )
    .expect("the env pattern is a literal")
});

const MARKUP: &[&str] = &[
    "jsx", "tsx", "html", "htm", "vue", "svelte", "erb", "hbs", "ejs", "haml", "slim",
];
/// Markup files that hold nothing but a template.
const TEMPLATE: &[&str] = &["html", "htm", "erb", "hbs", "ejs", "haml", "slim"];
const CONFIG: &[&str] = &[
    "json",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "env",
    "properties",
];
const DOCS: &[&str] = &["md", "mdx", "rst", "txt", "adoc"];

fn extension(path: &str) -> &str {
    path.rsplit_once('.').map_or("", |(_, ext)| ext)
}

/// Whether byte `at` of `line` sits inside a quoted string, by the quotes
/// before it on the line (`'`, `"` or a backtick, an odd count).
fn in_quotes(line: &str, at: usize) -> bool {
    let before = &line[..at.min(line.len())];
    ['\'', '"', '`']
        .into_iter()
        .any(|quote| before.chars().filter(|ch| *ch == quote).count() % 2 == 1)
}

fn is_comment(line: &str) -> bool {
    let trimmed = line.trim_start();
    ["//", "#", "--", "/*", "*", "<!--", ";"]
        .iter()
        .any(|lead| trimmed.starts_with(lead))
}

/// What `line` of `path` is, for a match starting at byte `at`.
pub fn classify(path: &str, line: &str, at: usize) -> ConceptKind {
    let ext = extension(path).to_ascii_lowercase();
    if CONFIG.contains(&ext.as_str()) {
        return ConceptKind::ConfigKey;
    }
    if is_comment(line) && !DOCS.contains(&ext.as_str()) && !MARKUP.contains(&ext.as_str()) {
        return ConceptKind::Comment;
    }
    if ENV_READ.is_match(line) && in_quotes(line, at) {
        return ConceptKind::EnvRead;
    }
    if ROUTE.is_match(line) || path.contains("routes") && in_quotes(line, at) {
        return ConceptKind::Route;
    }
    if MARKUP.contains(&ext.as_str()) {
        let template_only = TEMPLATE.contains(&ext.as_str());
        let before = &line[..at.min(line.len())];
        let inside_tag = before.rfind('<') > before.rfind('>') && before.contains('<');
        let text_node =
            line.contains('<') && before.contains('>') && !inside_tag && !in_code_tag(line, at);
        let attribute = inside_tag && in_quotes(line, at);
        // A page template has no code outside its tags and expressions; a
        // component file has markup only where a tag says so.
        if attribute || text_node || (template_only && !in_code_tag(line, at)) {
            return ConceptKind::UiText;
        }
    }
    if in_quotes(line, at) {
        return ConceptKind::Text;
    }
    ConceptKind::Code
}

/// Whether `at` is inside a `<...>` tag or a `{...}` expression of a markup
/// line: markup text sits between tags, so it is not.
fn in_code_tag(line: &str, at: usize) -> bool {
    let before = &line[..at.min(line.len())];
    let after_last = |open: char, close: char| match (before.rfind(open), before.rfind(close)) {
        (Some(o), Some(c)) => o > c,
        (Some(_), None) => true,
        _ => false,
    };
    after_last('<', '>') || after_last('{', '}')
}

/// Ranks the matches of `phrase`. Each hit is `(path, line number, line)`;
/// the line must hold the pattern ([`pattern`]), or it is dropped. At most
/// `limit` leads, best first, then by path and line.
pub fn rank(
    phrase: &str,
    hits: impl IntoIterator<Item = (String, u32, String)>,
    limit: usize,
) -> Vec<Lead> {
    let words = words(phrase);
    let Some(source) = pattern(&words) else {
        return Vec::new();
    };
    let Ok(matcher) = Regex::new(&source) else {
        return Vec::new();
    };
    let exact = Regex::new(&format!(
        "(?i){}",
        words
            .iter()
            .map(|w| regex::escape(w))
            .collect::<Vec<_>>()
            .join(r"\s+")
    ))
    .ok();
    let mut leads: Vec<Lead> = hits
        .into_iter()
        .filter_map(|(path, line_no, text)| {
            let found = matcher.find(&text)?;
            let kind = classify(&path, &text, found.start());
            let mut score = kind.weight();
            if exact.as_ref().is_some_and(|re| re.is_match(&text)) {
                score *= 1.5;
            }
            if pixel_model::is_test_path(&path) {
                score *= 0.7;
            }
            if DOCS.contains(&extension(&path).to_ascii_lowercase().as_str()) {
                score *= 0.5;
            }
            // A short line is the thing itself; a long one only mentions it.
            score *= 1.0 / (1.0 + text.len() as f64 / 400.0);
            Some(Lead {
                path,
                line: line_no,
                kind,
                text: trim(&text),
                score,
                owner: None,
            })
        })
        .collect();
    leads.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
    leads.truncate(limit);
    leads
}

fn trim(line: &str) -> String {
    let line = line.trim();
    if line.chars().count() <= 160 {
        return line.to_owned();
    }
    let cut: String = line.chars().take(159).collect();
    format!("{cut}…")
}

/// Names the smallest symbol that holds each lead's line.
pub fn with_owners(store: &Store, leads: &mut [Lead]) -> Result<(), StoreError> {
    let mut files: std::collections::HashMap<String, Vec<pixel_model::Symbol>> =
        std::collections::HashMap::new();
    for lead in leads.iter_mut() {
        if !files.contains_key(&lead.path) {
            files.insert(lead.path.clone(), store.symbols_in(&lead.path)?);
        }
        lead.owner = files[&lead.path]
            .iter()
            .filter(|s| {
                !matches!(
                    s.kind,
                    pixel_model::SymbolKind::File | pixel_model::SymbolKind::Package
                ) && s.line <= lead.line
                    && lead.line <= s.end_line
            })
            .min_by_key(|s| s.end_line - s.line)
            .map(|s| s.name.clone());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: &str, line: u32, text: &str) -> (String, u32, String) {
        (path.to_owned(), line, text.to_owned())
    }

    #[test]
    fn words_should_keep_alphanumeric_runs_of_two_or_more_in_order() {
        assert_eq!(words("Add to cart!"), ["add", "to", "cart"]);
        assert_eq!(words("503 (a) error_code"), ["503", "error", "code"]);
        assert!(words("? ! a").is_empty());
    }

    #[test]
    fn pattern_should_match_the_words_across_any_separator_and_case() {
        let re = Regex::new(&pattern(&words("add to cart")).unwrap()).unwrap();
        for text in [
            "\"Add to cart\"",
            "add_to_cart()",
            "ADD-TO-CART",
            "Add   to\tcart",
        ] {
            assert!(re.is_match(text), "{text}");
        }
        assert!(!re.is_match("addtocart"), "words never run together");
        assert!(!re.is_match("add to the cart"));
        assert_eq!(pattern(&[]), None);
        let version = Regex::new(&pattern(&words("v1.beta")).unwrap()).unwrap();
        assert!(version.is_match("V1_beta") && !version.is_match("v1beta"));
    }

    #[test]
    fn classify_should_tell_a_string_a_route_an_env_read_a_config_key_and_a_comment() {
        assert_eq!(
            classify("src/a.rs", r#"    println!("retry limit reached");"#, 14),
            ConceptKind::Text
        );
        assert_eq!(
            classify("src/a.rs", "    // the retry limit is read here", 7),
            ConceptKind::Comment
        );
        assert_eq!(
            classify("src/a.rs", r#"let n = std::env::var("RETRY_LIMIT");"#, 26),
            ConceptKind::EnvRead
        );
        assert_eq!(
            classify(
                "config/routes.rb",
                "  post '/retry/limit', to: 'retries#update'",
                9
            ),
            ConceptKind::Route
        );
        assert_eq!(
            classify("api/app.ts", r#"app.get("/retry/limit", handler)"#, 10),
            ConceptKind::Route
        );
        assert_eq!(
            classify("config/app.yml", "retry_limit: 3", 0),
            ConceptKind::ConfigKey
        );
        assert_eq!(
            classify("src/retry.rs", "fn retry_limit() -> u32 {", 3),
            ConceptKind::Code
        );
    }

    #[test]
    fn classify_should_read_markup_text_as_ui_text_and_markup_code_as_code() {
        assert_eq!(
            classify("src/Cart.tsx", "      <button>Add to cart</button>", 14),
            ConceptKind::UiText
        );
        assert_eq!(
            classify(
                "src/Cart.tsx",
                r#"      <input placeholder="Add to cart" />"#,
                26
            ),
            ConceptKind::UiText
        );
        assert_eq!(
            classify("src/Cart.tsx", "  const addToCart = useCart();", 8),
            ConceptKind::Code
        );
        assert_eq!(
            classify("src/Cart.tsx", "      <button onClick={add_to_cart}>", 22),
            ConceptKind::Code
        );
    }

    #[test]
    fn rank_should_put_a_string_an_exact_phrase_and_a_short_line_first() {
        let hits = vec![
            hit("docs/guide.md", 3, "To add to cart, click the button."),
            hit("src/cart.rs", 10, "// add to cart helper, see the spec"),
            hit("src/cart.rs", 20, r#"    flash("Add to cart failed");"#),
            hit("src/cart.rs", 30, "fn add_to_cart(item: Item) {"),
            hit(
                "tests/cart.rs",
                5,
                r#"    assert_eq!(msg, "Add to cart failed");"#,
            ),
            hit("src/other.rs", 1, "nothing here"),
        ];
        let leads = rank("add to cart", hits, 10);
        let order: Vec<(&str, u32)> = leads.iter().map(|l| (l.path.as_str(), l.line)).collect();
        assert_eq!(
            order,
            [
                ("src/cart.rs", 20),
                ("tests/cart.rs", 5),
                ("src/cart.rs", 30),
                ("src/cart.rs", 10),
                ("docs/guide.md", 3)
            ]
        );
        assert_eq!(leads[0].kind, ConceptKind::Text);
        assert_eq!(leads[0].text, r#"flash("Add to cart failed");"#);
        assert_eq!(
            rank("add to cart", vec![hit("a.rs", 1, "add to cart")], 0).len(),
            0
        );
        assert!(rank("? !", vec![hit("a.rs", 1, "x")], 5).is_empty());
    }

    #[test]
    fn rank_should_break_ties_by_path_and_line_and_cut_long_lines() {
        let long = format!("\"add to cart\" {}", "x".repeat(300));
        let leads = rank(
            "add to cart",
            vec![
                hit("b.rs", 2, "\"add to cart\""),
                hit("a.rs", 9, "\"add to cart\""),
                hit("a.rs", 3, "\"add to cart\""),
                hit("c.rs", 1, &long),
            ],
            10,
        );
        let order: Vec<(&str, u32)> = leads.iter().map(|l| (l.path.as_str(), l.line)).collect();
        assert_eq!(order, [("a.rs", 3), ("a.rs", 9), ("b.rs", 2), ("c.rs", 1)]);
        assert_eq!(leads[3].text.chars().count(), 160);
        assert!(leads[3].text.ends_with('…'));
    }

    #[test]
    fn with_owners_should_name_the_smallest_enclosing_symbol() {
        let (_repo, store) = crate::analysis::fixture::project();
        // `src/b.rs` holds `go`, which spans its own lines.
        let mut leads = vec![Lead {
            path: "src/b.rs".into(),
            line: 4,
            kind: ConceptKind::Code,
            text: "run(1)".into(),
            score: 1.0,
            owner: None,
        }];
        with_owners(&store, &mut leads).unwrap();
        assert_eq!(leads[0].owner.as_deref(), Some("go"));
        let mut outside = vec![Lead {
            line: 1,
            ..leads[0].clone()
        }];
        with_owners(&store, &mut outside).unwrap();
        assert_eq!(outside[0].owner, None, "line 1 is the import, in no symbol");
    }
}
