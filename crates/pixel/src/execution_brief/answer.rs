//! What lets a confident brief be answered from: the search receipt (which
//! searches ran, over what) and answer-sized excerpts built from the evidence
//! the chain gathered. Every function here is pure: the chain reads the
//! files, these shape the text, so each kind's excerpt is tested on its own.

use std::fmt::Write as _;

use super::chain::RichHit;
use super::relevance::RelevanceInput;

/// Environment switch of the search receipt (`0`, `false` or `off` removes it).
pub(crate) const RECEIPT_ENV: &str = "PIXEL_BRIEF_RECEIPT";
/// Environment switch of the answer-sized excerpts.
pub(crate) const ANSWER_ENV: &str = "PIXEL_BRIEF_ANSWER";

/// Terms and ignored words one receipt line names.
const MAX_TERMS_SHOWN: usize = 6;
const MAX_TERM_CHARS: usize = 24;
/// Lines of source read before the matched line, to reach the signature and
/// its doc comment, and after it.
const WINDOW_BEFORE: u64 = 12;
const WINDOW_AFTER: u64 = 9;
/// Body lines an excerpt shows after the signature and doc line.
const BODY_LINES: usize = 8;
/// Chars of one excerpt line.
const LINE_CHARS: usize = 110;
/// Hops a flow excerpt names.
const MAX_HOPS: usize = 5;
/// Test functions a tests excerpt names.
pub(crate) const MAX_TESTS: usize = 4;
/// Lines of a test file read to find its test functions.
pub(crate) const TEST_FILE_LINES: u64 = 4000;
/// Lines one test function is searched for its first assertion and its
/// mention of the target.
const TEST_SPAN: usize = 40;
/// Lines a config excerpt shows.
const MAX_CONFIG_LINES: usize = 4;

/// Whether an environment value leaves a feature on: only an explicit off
/// word turns it off.
pub(crate) fn toggle_on(value: Option<&str>) -> bool {
    !matches!(value, Some("0" | "false" | "off"))
}

pub(crate) fn receipt_enabled() -> bool {
    toggle_on(std::env::var(RECEIPT_ENV).ok().as_deref())
}

pub(crate) fn answer_enabled() -> bool {
    toggle_on(std::env::var(ANSWER_ENV).ok().as_deref())
}

/// What the search of one brief covered, taken from the probes that answered
/// and from nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Receipt {
    terms: Vec<String>,
    ignored: Vec<String>,
    files_considered: usize,
    /// Chunks the meaning search returned; `None` when it did not answer.
    chunks: Option<usize>,
}

impl Receipt {
    /// The receipt of a relevance probe that answered (`input`) and, when it
    /// answered too, a meaning search that returned `chunks` chunks. `None`
    /// when the probe weighed no term.
    pub(crate) fn new(input: &RelevanceInput, chunks: Option<usize>) -> Option<Self> {
        let mut terms: Vec<String> = Vec::new();
        let mut ignored: Vec<String> = Vec::new();
        for stat in &input.keywords {
            let list = if stat.weight > 0.0 {
                &mut terms
            } else {
                &mut ignored
            };
            if !list.contains(&stat.keyword) {
                list.push(stat.keyword.clone());
            }
        }
        (!terms.is_empty()).then_some(Self {
            terms,
            ignored,
            files_considered: input.files_considered,
            chunks,
        })
    }

    /// The terms the probe weighed, as the receipt lists them.
    pub(crate) fn terms(&self) -> &[String] {
        &self.terms
    }

    /// The receipt lines: facts, then the instruction they support.
    pub(crate) fn lines(&self) -> Vec<String> {
        let mut searched = format!(
            "searched: content+symbols+paths for {} ({} terms, {} files)",
            names(&self.terms),
            self.terms.len(),
            self.files_considered
        );
        if let Some(chunks) = self.chunks {
            let _ = write!(searched, " · meaning search returned {chunks} chunks");
        }
        let mut lines = vec![searched];
        if !self.ignored.is_empty() {
            lines.push(format!("ignored (too common): {}", names(&self.ignored)));
        }
        let scope = if self.chunks.is_some() {
            "both searches"
        } else {
            "the search"
        };
        lines.push(format!(
            "result: the matches below are the best across {scope}; answer from them if they suffice, search further only if they don't"
        ));
        lines
    }
}

fn names(list: &[String]) -> String {
    let shown: Vec<String> = list
        .iter()
        .take(MAX_TERMS_SHOWN)
        .map(|name| name.chars().take(MAX_TERM_CHARS).collect())
        .collect();
    let hidden = list.len().saturating_sub(MAX_TERMS_SHOWN);
    if hidden == 0 {
        shown.join(", ")
    } else {
        format!("{} (+{hidden})", shown.join(", "))
    }
}

/// One block of answer evidence: a label and its source lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Excerpt {
    pub(crate) label: String,
    pub(crate) lines: Vec<String>,
}

/// The line range to read around `matched`: enough above it for the
/// signature and the doc comment, enough below for the body.
pub(crate) fn window_range(matched: u64) -> (u64, u64) {
    let matched = matched.max(1);
    (
        matched.saturating_sub(WINDOW_BEFORE).max(1),
        matched + WINDOW_AFTER,
    )
}

fn cut(text: &str, chars: usize) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(chars)
        .collect()
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Number the chosen `(line number, text)` rows and take the common
/// indentation off, so a nested method reads as compact as a free function.
fn numbered(rows: &[(u64, &str)]) -> Vec<String> {
    let indent = rows
        .iter()
        .map(|(_, text)| indent_of(text))
        .min()
        .unwrap_or(0);
    rows.iter()
        .map(|(number, text)| {
            let body = text.get(indent..).unwrap_or(text).trim_end();
            format!("{number}| {}", cut(body, LINE_CHARS))
        })
        .collect()
}

const SIGNATURE_STARTS: &[&str] = &[
    "fn ",
    "pub fn ",
    "pub(crate) fn ",
    "pub(super) fn ",
    "async fn ",
    "pub async fn ",
    "pub(crate) async fn ",
    "const fn ",
    "unsafe fn ",
    "struct ",
    "pub struct ",
    "pub(crate) struct ",
    "enum ",
    "pub enum ",
    "pub(crate) enum ",
    "trait ",
    "pub trait ",
    "pub(crate) trait ",
    "impl ",
    "impl<",
    "pub const ",
    "pub(crate) const ",
    "const ",
    "pub static ",
    "pub(crate) static ",
    "static ",
    "type ",
    "pub type ",
    "def ",
    "async def ",
    "class ",
    "function ",
    "async function ",
    "export ",
    "func ",
    "interface ",
];

fn is_signature(line: &str) -> bool {
    SIGNATURE_STARTS
        .iter()
        .any(|start| line.trim_start().starts_with(start))
}

fn is_doc(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("///")
        || line.starts_with("//!")
        || line.starts_with("/**")
        || line.starts_with("* ")
        || line.starts_with("//")
}

fn is_attribute(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("#[") || line.starts_with('@')
}

/// Index of the first line of the doc comment right above `sig`, skipping
/// attributes between them.
fn doc_above(window: &[String], sig: usize) -> Option<usize> {
    let mut at = sig;
    while at > 0 && is_attribute(&window[at - 1]) {
        at -= 1;
    }
    let mut top = None;
    while at > 0 && is_doc(&window[at - 1]) {
        at -= 1;
        top = Some(at);
    }
    top
}

/// The generic excerpt, and the lookup one: the signature of the symbol the
/// matched line belongs to, the first line of its doc comment, and up to
/// [`BODY_LINES`] lines of the matched region. `window[0]` is line `start`.
pub(crate) fn lookup_excerpt(start: u64, window: &[String], matched: u64) -> Vec<String> {
    if window.is_empty() {
        return Vec::new();
    }
    let at = usize::try_from(matched.saturating_sub(start))
        .unwrap_or(0)
        .min(window.len() - 1);
    let sig = (0..=at).rev().find(|&index| is_signature(&window[index]));
    let mut chosen: Vec<usize> = Vec::new();
    if let Some(sig) = sig {
        chosen.extend(doc_above(window, sig));
        chosen.push(sig);
    }
    let begin = match sig {
        Some(sig) if at <= sig + BODY_LINES => sig + 1,
        _ => at.saturating_sub(1),
    };
    chosen.extend(
        (begin..window.len())
            .filter(|index| !window[*index].trim().is_empty() && !chosen.contains(index))
            .take(BODY_LINES)
            .collect::<Vec<_>>(),
    );
    let rows: Vec<(u64, &str)> = chosen
        .iter()
        .map(|&index| (start + index as u64, window[index].as_str()))
        .collect();
    numbered(&rows)
}

/// One step of a flow: a function, where it is defined, and its first line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Hop {
    pub(crate) name: String,
    pub(crate) site: Option<(String, u64)>,
    pub(crate) head: Option<String>,
}

/// The hops a flow excerpt names: the first [`MAX_HOPS`], or the first
/// `MAX_HOPS - 1` and the last when the chain is longer.
pub(crate) fn pick_hops(names: &[String]) -> Vec<String> {
    if names.len() <= MAX_HOPS {
        names.to_vec()
    } else {
        let mut picked = names[..MAX_HOPS - 1].to_vec();
        picked.extend(names.last().cloned());
        picked
    }
}

/// The ordered call chain, one hop per line: `path:line name — first line`.
/// Empty when no hop has a known site, so the caller falls back.
pub(crate) fn flow_excerpt(hops: &[Hop]) -> Vec<String> {
    if hops.iter().all(|hop| hop.site.is_none()) {
        return Vec::new();
    }
    hops.iter()
        .take(MAX_HOPS)
        .map(|hop| {
            let place = hop
                .site
                .as_ref()
                .map_or_else(String::new, |(path, line)| format!("{path}:{line} "));
            let head = hop
                .head
                .as_ref()
                .map_or_else(String::new, |head| format!(" — {}", head.trim()));
            cut(&format!("{place}{}{head}", hop.name), LINE_CHARS + 30)
        })
        .collect()
}

fn word_in(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let word = |ch: char| ch.is_alphanumeric() || ch == '_';
    haystack.match_indices(needle).any(|(at, found)| {
        let before = haystack[..at].chars().next_back();
        let after = haystack[at + found.len()..].chars().next();
        !before.is_some_and(word) && !after.is_some_and(word)
    })
}

/// The name of the test function that starts at `lines[index]`, when one does.
fn test_name(lines: &[String], index: usize) -> Option<String> {
    let line = lines[index].trim();
    let attributed = || {
        lines[index.saturating_sub(3)..index].iter().any(|before| {
            let before = before.trim();
            before.starts_with("#[test")
                || before.starts_with("#[tokio::test")
                || before.starts_with("#[rstest")
        })
    };
    let after = |prefixes: &[&str]| {
        prefixes.iter().find_map(|prefix| {
            let rest = line.strip_prefix(prefix)?;
            let end = rest.find(['(', '<'])?;
            Some(rest[..end].trim().to_string())
        })
    };
    if let Some(name) = after(&["fn ", "async fn "]).filter(|_| attributed()) {
        return Some(name);
    }
    if let Some(name) = after(&["def test_", "async def test_"]) {
        return Some(format!("test_{name}"));
    }
    if let Some(name) = after(&["func Test"]) {
        return Some(format!("Test{name}"));
    }
    ["it(", "test(", "it.each", "test.each"]
        .iter()
        .find(|prefix| line.starts_with(**prefix))
        .and_then(|_| {
            let quote = line.find(['"', '\'', '`'])?;
            let mark = line[quote..].chars().next()?;
            let rest = &line[quote + 1..];
            Some(rest[..rest.find(mark)?].to_string())
        })
}

fn is_assert(line: &str) -> bool {
    line.contains("assert") || line.contains("expect(") || line.contains(".should")
}

/// Test functions of `lines` (line 1 first) that mention `target`, each with
/// its first assertion, as `path:line name — assertion`; at most `max`.
pub(crate) fn tests_excerpt(path: &str, lines: &[String], target: &str, max: usize) -> Vec<String> {
    let starts: Vec<(usize, String)> = (0..lines.len())
        .filter_map(|index| test_name(lines, index).map(|name| (index, name)))
        .collect();
    let mut found = Vec::new();
    for (position, (index, name)) in starts.iter().enumerate() {
        if found.len() >= max {
            break;
        }
        let end = starts
            .get(position + 1)
            .map_or(lines.len(), |(next, _)| *next)
            .min(index + TEST_SPAN)
            .min(lines.len());
        let span = &lines[*index..end];
        if !span.iter().any(|line| word_in(line, target)) {
            continue;
        }
        let first = span
            .iter()
            .find(|line| is_assert(line))
            .map_or_else(String::new, |line| format!(" — {}", line.trim()));
        found.push(cut(
            &format!("{path}:{} {name}{first}", index + 1),
            LINE_CHARS + 30,
        ));
    }
    found
}

fn has_upper_snake(text: &str) -> bool {
    text.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .any(|token| {
            token.len() >= 4
                && token.chars().any(|ch| ch.is_ascii_uppercase())
                && token
                    .chars()
                    .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
        })
}

fn reads_env(text: &str) -> bool {
    [
        "env::var",
        "process.env",
        "getenv",
        "os.environ",
        "std::env",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn defines_setting(text: &str) -> bool {
    !reads_env(text)
        && (text.contains("const ") || text.contains("static ") || text.contains(" = "))
        && has_upper_snake(text)
}

/// The setting a config question asks about, from the matched lines the
/// search kept: the line that defines the constant (its default value is on
/// it) and the lines that read it from the environment. A line counts only
/// when its path or text carries one of the probed `terms` (any, when there
/// are none): a constant that merely matched the question's shape is noise.
pub(crate) fn config_excerpt(hits: &[RichHit], terms: &[String]) -> Vec<String> {
    let about = |hit: &RichHit, text: &str| {
        let haystack = format!("{} {text}", hit.path).to_lowercase();
        terms.is_empty()
            || terms
                .iter()
                .any(|term| haystack.contains(&term.to_lowercase()))
    };
    let site = |hit: &RichHit, text: &str| {
        cut(
            &format!("{}:{} — {}", hit.path, hit.line, text.trim()),
            LINE_CHARS + 30,
        )
    };
    let mut lines: Vec<String> = Vec::new();
    for (wanted, cap) in [(true, 2), (false, 2)] {
        let rows = hits
            .iter()
            .filter_map(|hit| hit.text.as_deref().map(|text| (hit, text)))
            .filter(|(hit, text)| about(hit, text))
            .filter(|(_, text)| {
                if wanted {
                    defines_setting(text)
                } else {
                    reads_env(text)
                }
            })
            .map(|(hit, text)| site(hit, text))
            .filter(|row| !lines.contains(row))
            .take(cap)
            .collect::<Vec<_>>();
        lines.extend(rows);
    }
    lines.truncate(MAX_CONFIG_LINES);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_brief::relevance::KeywordStat;

    fn lines(source: &[&str]) -> Vec<String> {
        source.iter().map(ToString::to_string).collect()
    }

    fn input(keywords: &[(&str, f64)]) -> RelevanceInput {
        RelevanceInput {
            graph: true,
            files_considered: 412,
            structural_files: 3,
            keywords: keywords
                .iter()
                .map(|(keyword, weight)| KeywordStat {
                    keyword: (*keyword).to_string(),
                    weight: *weight,
                    french_only: false,
                })
                .collect(),
            cofiles: Vec::new(),
        }
    }

    #[test]
    fn toggle_should_stay_on_unless_an_off_word_is_given() {
        assert!(toggle_on(None));
        assert!(toggle_on(Some("1")));
        assert!(toggle_on(Some("")));
        for off in ["0", "false", "off"] {
            assert!(!toggle_on(Some(off)), "{off}");
        }
    }

    #[test]
    fn receipt_should_count_the_probed_terms_and_list_the_common_ones_as_ignored() {
        let receipt = Receipt::new(
            &input(&[("watchdog", 2.0), ("does", 0.0), ("daemon", 1.0)]),
            None,
        )
        .unwrap();
        let text = receipt.lines().join("\n");
        assert!(
            text.contains(
                "searched: content+symbols+paths for watchdog, daemon (2 terms, 412 files)"
            ),
            "{text}"
        );
        assert!(text.contains("ignored (too common): does"), "{text}");
        assert!(text.contains("across the search;"), "{text}");
        assert!(!text.contains("meaning"), "{text}");
    }

    #[test]
    fn receipt_should_name_the_meaning_search_only_when_it_answered() {
        let weights = input(&[("watchdog", 2.0)]);
        let with = Receipt::new(&weights, Some(8)).unwrap().lines().join("\n");
        assert!(
            with.contains("· meaning search returned 8 chunks"),
            "{with}"
        );
        assert!(with.contains("across both searches;"), "{with}");
        let without = Receipt::new(&weights, None).unwrap().lines().join("\n");
        assert!(!without.contains("meaning"), "{without}");
    }

    #[test]
    fn receipt_should_not_exist_when_no_term_was_weighed() {
        assert_eq!(Receipt::new(&input(&[("the", 0.0)]), Some(3)), None);
        assert_eq!(Receipt::new(&input(&[]), None), None);
    }

    #[test]
    fn receipt_should_bound_the_terms_it_lists() {
        let many: Vec<(String, f64)> = (0..9).map(|n| (format!("term{n}"), 1.0)).collect();
        let pairs: Vec<(&str, f64)> = many.iter().map(|(k, w)| (k.as_str(), *w)).collect();
        let text = Receipt::new(&input(&pairs), None)
            .unwrap()
            .lines()
            .join("\n");
        assert!(text.contains("term5 (+3) (9 terms"), "{text}");
        assert!(!text.contains("term6"), "{text}");
    }

    #[test]
    fn window_range_should_start_above_the_match_and_never_at_zero() {
        assert_eq!(window_range(100), (88, 109));
        assert_eq!(window_range(3), (1, 12));
        assert_eq!(window_range(0), (1, 10));
    }

    const SOURCE: &[&str] = &[
        "use std::fmt;",
        "",
        "/// Keep the daemon alive.",
        "/// Second doc line, not shown.",
        "#[must_use]",
        "pub fn watchdog(period: u64) -> bool {",
        "    let mut tries = 0;",
        "    loop {",
        "        tries += 1;",
        "        if tries > period {",
        "            return false;",
        "        }",
        "    }",
        "}",
    ];

    #[test]
    fn lookup_excerpt_should_give_the_signature_the_first_doc_line_and_the_body() {
        let window = lines(SOURCE);
        let excerpt = lookup_excerpt(10, &window, 17);
        assert_eq!(excerpt[0], "12| /// Keep the daemon alive.");
        assert_eq!(excerpt[1], "15| pub fn watchdog(period: u64) -> bool {");
        assert_eq!(excerpt[2], "16|     let mut tries = 0;");
        assert!(!excerpt.iter().any(|line| line.contains("Second doc")));
        assert!(excerpt.len() <= 2 + BODY_LINES, "{excerpt:?}");
    }

    #[test]
    fn lookup_excerpt_should_bound_the_body_and_each_line() {
        let mut source: Vec<String> = vec!["pub fn big() {".into()];
        source.extend((0..30).map(|n| format!("    step_{n}();{}", "x".repeat(200))));
        let excerpt = lookup_excerpt(1, &source, 1);
        assert_eq!(excerpt.len(), 1 + BODY_LINES);
        assert!(
            excerpt
                .iter()
                .all(|line| line.chars().count() <= LINE_CHARS + 8)
        );
    }

    #[test]
    fn lookup_excerpt_should_dedent_a_nested_method() {
        let window = lines(&["impl A {", "    fn run(&self) {", "        go();", "    }"]);
        let excerpt = lookup_excerpt(1, &window, 3);
        assert_eq!(excerpt, ["2| fn run(&self) {", "3|     go();", "4| }"]);
    }

    #[test]
    fn lookup_excerpt_should_show_the_region_around_a_match_without_a_signature() {
        let window = lines(&["    a();", "    b();", "    c();", "    d();"]);
        let excerpt = lookup_excerpt(50, &window, 52);
        assert_eq!(excerpt[0], "51| b();");
        assert!(lookup_excerpt(1, &[], 1).is_empty());
    }

    #[test]
    fn flow_excerpt_should_print_each_hop_with_its_site_and_first_line() {
        let hops = vec![
            Hop {
                name: "start".into(),
                site: Some(("src/a.rs".into(), 10)),
                head: Some("pub fn start() {".into()),
            },
            Hop {
                name: "open".into(),
                site: None,
                head: None,
            },
        ];
        assert_eq!(
            flow_excerpt(&hops),
            ["src/a.rs:10 start — pub fn start() {", "open"]
        );
        let unknown = vec![Hop {
            name: "x".into(),
            site: None,
            head: None,
        }];
        assert!(flow_excerpt(&unknown).is_empty());
    }

    #[test]
    fn pick_hops_should_keep_five_and_the_destination_of_a_longer_chain() {
        let names: Vec<String> = (0..8).map(|n| format!("h{n}")).collect();
        assert_eq!(pick_hops(&names[..5]), names[..5]);
        assert_eq!(pick_hops(&names), ["h0", "h1", "h2", "h3", "h7"]);
    }

    #[test]
    fn tests_excerpt_should_name_each_test_that_mentions_the_target_with_its_first_assert() {
        let file = lines(&[
            "#[test]",
            "fn other() {",
            "    assert_eq!(1, 1);",
            "}",
            "#[test]",
            "fn watchdog_should_stop() {",
            "    let ok = watchdog(3);",
            "    assert!(!ok);",
            "}",
        ]);
        assert_eq!(
            tests_excerpt("t.rs", &file, "watchdog", 4),
            ["t.rs:6 watchdog_should_stop — assert!(!ok);"]
        );
        assert!(tests_excerpt("t.rs", &file, "missing", 4).is_empty());
        assert!(tests_excerpt("t.rs", &file, "watchdog", 0).is_empty());
    }

    #[test]
    fn tests_excerpt_should_read_javascript_and_python_tests_and_cap_the_count() {
        let js = lines(&[
            "it('stops the watchdog', () => {",
            "  expect(watchdog(1)).toBe(false)",
            "})",
        ]);
        assert_eq!(
            tests_excerpt("a.test.ts", &js, "watchdog", 4),
            ["a.test.ts:1 stops the watchdog — expect(watchdog(1)).toBe(false)"]
        );
        let py = lines(&[
            "def test_one():",
            "    assert watchdog(1)",
            "def test_two():",
            "    assert watchdog(2)",
        ]);
        let found = tests_excerpt("t.py", &py, "watchdog", 1);
        assert_eq!(found, ["t.py:1 test_one — assert watchdog(1)"]);
    }

    #[test]
    fn tests_excerpt_should_not_match_a_target_inside_a_longer_word() {
        let file = lines(&["#[test]", "fn t() {", "    assert!(rewatchdog());", "}"]);
        assert!(tests_excerpt("t.rs", &file, "watchdog", 4).is_empty());
    }

    fn rich(path: &str, line: u64, text: &str) -> RichHit {
        RichHit {
            path: path.into(),
            line,
            text: Some(text.into()),
        }
    }

    #[test]
    fn config_excerpt_should_give_the_definition_with_its_default_and_where_it_is_read() {
        let hits = vec![
            rich("a.rs", 9, "let v = std::env::var(RECEIPT_ENV).ok();"),
            rich(
                "a.rs",
                3,
                "pub(crate) const RECEIPT_ENV: &str = \"PIXEL_BRIEF_RECEIPT\";",
            ),
            rich("b.rs", 1, "fn unrelated() {}"),
        ];
        assert_eq!(
            config_excerpt(&hits, &[]),
            [
                "a.rs:3 — pub(crate) const RECEIPT_ENV: &str = \"PIXEL_BRIEF_RECEIPT\";",
                "a.rs:9 — let v = std::env::var(RECEIPT_ENV).ok();",
            ]
        );
        assert!(config_excerpt(&hits[2..], &[]).is_empty());
        assert!(config_excerpt(&[], &[]).is_empty());
        // Only lines that carry a probed term count.
        let terms = vec!["receipt".to_string()];
        assert_eq!(config_excerpt(&hits, &terms).len(), 2);
        assert!(config_excerpt(&hits, &["unrelated".to_string()]).is_empty());
    }
}
